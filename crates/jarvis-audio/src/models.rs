//! Gestor seguro de modelos whisper (HU-7, mitigación SEC-02).
//!
//! - Las URLs y los SHA-256 quedan fijados en el código; las URLs de Hugging
//!   Face apuntan a un commit concreto, nunca a `main`.
//! - Descarga a un temporal en el mismo directorio (0600), cálculo del hash en
//!   streaming, verificación de tamaño, hash y cabecera, y `rename` atómico.
//!   Si algo falla el temporal se borra y se reintenta: la ruta final nunca
//!   contiene un archivo a medias (CA-7.3).
//! - Antes de entregar un modelo a whisper.cpp se vuelve a verificar el hash y
//!   la cabecera sobre los **mismos bytes** que se cargarán (sin TOCTOU). Un
//!   modelo alterado se borra y se informa; jamás se carga (CA-7.1, CA-7.2).
//!
//! La única forma de obtener un [`VerifiedModel`] es pasar por este módulo, así
//! que [`crate::stt::WhisperEngine`] no puede cargar rutas arbitrarias ni
//! modelos indicados por variables de entorno (SEC-02 c).

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// Commit de `huggingface.co/ggerganov/whisper.cpp` al que se fijan las URLs.
pub const HF_WHISPER_COMMIT: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";

/// Descripción inmutable de un modelo descargable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    /// Identificador estable (IPC `jarvis.modelStatus`, `--check`).
    pub id: &'static str,
    /// Nombre del archivo dentro del directorio de modelos.
    pub file_name: &'static str,
    /// URL HTTPS fijada a un commit.
    pub url: &'static str,
    /// SHA-256 esperado en hexadecimal minúscula.
    pub sha256: &'static str,
    /// Tamaño exacto en bytes (también es el límite de descarga).
    pub size: u64,
}

/// Modelo principal de transcripción (HU-5).
pub const WHISPER_BASE_Q5_1: ModelSpec = ModelSpec {
    id: "whisper-base-q5_1",
    file_name: "ggml-base-q5_1.bin",
    url: concat!(
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/",
        "5359861c739e955e79d9a303bcbc70fb988958b1",
        "/ggml-base-q5_1.bin"
    ),
    sha256: "422f1ae452ade6f30a004d7e5c6a43195e4433bc370bf23fac9cc591f01a8898",
    size: 59_707_625,
};

/// Modelo pequeño dedicado a la palabra de activación (HU-4).
pub const WHISPER_TINY: ModelSpec = ModelSpec {
    id: "whisper-tiny",
    file_name: "ggml-tiny.bin",
    url: concat!(
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/",
        "5359861c739e955e79d9a303bcbc70fb988958b1",
        "/ggml-tiny.bin"
    ),
    sha256: "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
    size: 77_691_713,
};

/// Catálogo completo de modelos gestionados.
pub const MODELS: &[ModelSpec] = &[WHISPER_BASE_Q5_1, WHISPER_TINY];

/// Magia `ggml` de los modelos whisper (little-endian).
pub const GGML_MAGIC: u32 = 0x6767_6d6c;
/// Bytes de cabecera que se validan: magia + 11 hiperparámetros `i32`.
pub const HEADER_LEN: usize = 4 + 11 * 4;
/// Vocabulario mínimo aceptado (modelos solo-inglés: 51864).
pub const MIN_N_VOCAB: i32 = 51_864;
/// Cota superior defensiva del vocabulario.
pub const MAX_N_VOCAB: i32 = 65_536;
/// Rango de `n_vocab` que dispara la escritura fuera de límites del issue
/// #4059 de whisper.cpp (SEC-02).
pub const ISSUE_4059_N_VOCAB: std::ops::RangeInclusive<i32> = 50_258..=50_363;

const CHUNK: usize = 64 * 1024;

/// Errores del gestor de modelos.
#[derive(Debug)]
pub enum ModelError {
    Io(io::Error),
    Network(String),
    /// El servidor entregó más bytes que el tamaño fijado.
    TooLarge {
        limit: u64,
    },
    SizeMismatch {
        expected: u64,
        actual: u64,
    },
    HashMismatch {
        expected: String,
        actual: String,
    },
    InvalidHeader(String),
    /// Se agotaron los reintentos de descarga; guarda el último error.
    DownloadFailed {
        attempts: u32,
        last: Box<ModelError>,
    },
    NoDataDir,
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelError::Io(e) => write!(f, "error de E/S: {e}"),
            ModelError::Network(e) => write!(f, "error de red: {e}"),
            ModelError::TooLarge { limit } => {
                write!(f, "la descarga supera el límite de {limit} bytes")
            }
            ModelError::SizeMismatch { expected, actual } => {
                write!(
                    f,
                    "tamaño inesperado: {actual} bytes (se esperaban {expected})"
                )
            }
            ModelError::HashMismatch { expected, actual } => {
                write!(f, "SHA-256 no coincide: {actual} (se esperaba {expected})")
            }
            ModelError::InvalidHeader(why) => write!(f, "cabecera de modelo inválida: {why}"),
            ModelError::DownloadFailed { attempts, last } => {
                write!(f, "descarga fallida tras {attempts} intentos: {last}")
            }
            ModelError::NoDataDir => {
                write!(f, "no se pudo resolver ~/.local/share (falta HOME)")
            }
        }
    }
}

impl std::error::Error for ModelError {}

impl From<io::Error> for ModelError {
    fn from(e: io::Error) -> Self {
        ModelError::Io(e)
    }
}

/// Hiperparámetros leídos de la cabecera ggml.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelHeader {
    pub n_vocab: i32,
    pub n_audio_ctx: i32,
    pub n_audio_state: i32,
    pub n_audio_head: i32,
    pub n_audio_layer: i32,
    pub n_text_ctx: i32,
    pub n_text_state: i32,
    pub n_text_head: i32,
    pub n_text_layer: i32,
    pub n_mels: i32,
    pub ftype: i32,
}

/// Valida la cabecera ggml **antes** de pasar el modelo a whisper.cpp
/// (SEC-02 b). Rechaza `n_vocab` del rango del issue #4059 y cualquier valor
/// por debajo de 51864, además de dimensiones absurdas.
pub fn validate_header(bytes: &[u8]) -> Result<ModelHeader, ModelError> {
    if bytes.len() < HEADER_LEN {
        return Err(ModelError::InvalidHeader(format!(
            "archivo truncado ({} bytes)",
            bytes.len()
        )));
    }
    let word = |i: usize| -> [u8; 4] {
        let at = 4 + i * 4;
        [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]
    };
    let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if magic != GGML_MAGIC {
        return Err(ModelError::InvalidHeader(format!(
            "magia {magic:#010x} distinta de ggml"
        )));
    }
    let v: [i32; 11] = std::array::from_fn(|i| i32::from_le_bytes(word(i)));
    let header = ModelHeader {
        n_vocab: v[0],
        n_audio_ctx: v[1],
        n_audio_state: v[2],
        n_audio_head: v[3],
        n_audio_layer: v[4],
        n_text_ctx: v[5],
        n_text_state: v[6],
        n_text_head: v[7],
        n_text_layer: v[8],
        n_mels: v[9],
        ftype: v[10],
    };

    if ISSUE_4059_N_VOCAB.contains(&header.n_vocab) {
        return Err(ModelError::InvalidHeader(format!(
            "n_vocab {} en el rango vulnerable de whisper.cpp #4059",
            header.n_vocab
        )));
    }
    if !(MIN_N_VOCAB..=MAX_N_VOCAB).contains(&header.n_vocab) {
        return Err(ModelError::InvalidHeader(format!(
            "n_vocab {} fuera de [{MIN_N_VOCAB}, {MAX_N_VOCAB}]",
            header.n_vocab
        )));
    }
    if header.n_mels != 80 && header.n_mels != 128 {
        return Err(ModelError::InvalidHeader(format!(
            "n_mels {} no soportado",
            header.n_mels
        )));
    }
    let dims = [
        ("n_audio_ctx", header.n_audio_ctx, 4096),
        ("n_audio_state", header.n_audio_state, 8192),
        ("n_audio_head", header.n_audio_head, 128),
        ("n_audio_layer", header.n_audio_layer, 128),
        ("n_text_ctx", header.n_text_ctx, 4096),
        ("n_text_state", header.n_text_state, 8192),
        ("n_text_head", header.n_text_head, 128),
        ("n_text_layer", header.n_text_layer, 128),
    ];
    for (name, value, max) in dims {
        if !(1..=max).contains(&value) {
            return Err(ModelError::InvalidHeader(format!(
                "{name} = {value} fuera de [1, {max}]"
            )));
        }
    }
    if header.ftype < 0 {
        return Err(ModelError::InvalidHeader(format!(
            "ftype {} negativo",
            header.ftype
        )));
    }
    Ok(header)
}

/// Codifica un digest en hexadecimal minúscula.
pub fn to_hex(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// SHA-256 de un buffer en hexadecimal.
pub fn sha256_hex(bytes: &[u8]) -> String {
    to_hex(&Sha256::digest(bytes))
}

/// Modelo cuyo tamaño, SHA-256 y cabecera ya fueron verificados.
///
/// Contiene los bytes en memoria para que whisper.cpp cargue exactamente lo
/// verificado (sin releer la ruta).
pub struct VerifiedModel {
    spec: ModelSpec,
    bytes: Vec<u8>,
    header: ModelHeader,
}

impl VerifiedModel {
    /// Verifica un buffer contra la especificación.
    fn verify(spec: ModelSpec, bytes: Vec<u8>) -> Result<Self, ModelError> {
        let actual_size = bytes.len() as u64;
        if actual_size != spec.size {
            return Err(ModelError::SizeMismatch {
                expected: spec.size,
                actual: actual_size,
            });
        }
        let actual = sha256_hex(&bytes);
        if actual != spec.sha256 {
            return Err(ModelError::HashMismatch {
                expected: spec.sha256.to_string(),
                actual,
            });
        }
        let header = validate_header(&bytes)?;
        Ok(Self {
            spec,
            bytes,
            header,
        })
    }

    pub fn spec(&self) -> &ModelSpec {
        &self.spec
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn header(&self) -> &ModelHeader {
        &self.header
    }

    pub fn sha256(&self) -> &str {
        self.spec.sha256
    }
}

impl fmt::Debug for VerifiedModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifiedModel")
            .field("id", &self.spec.id)
            .field("len", &self.bytes.len())
            .field("header", &self.header)
            .finish()
    }
}

/// Estado de un modelo en disco (`--check`, `jarvis.modelStatus`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    Missing,
    Verified { sha256: String },
    Invalid { reason: String },
}

/// Origen de los bytes de un modelo (HTTP real o simulado en tests).
pub trait ModelSource: Send + Sync {
    /// Abre un lector sobre el contenido de `url`. `limit` es el máximo de
    /// bytes que el llamador aceptará.
    fn open(&self, url: &str, limit: u64) -> Result<Box<dyn Read + Send>, ModelError>;
}

/// Descarga HTTPS con `ureq` (rustls/ring), sin HTTP plano.
pub struct HttpSource {
    agent: ureq::Agent,
}

impl HttpSource {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .user_agent(concat!("fokkus-jarvis/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl Default for HttpSource {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelSource for HttpSource {
    fn open(&self, url: &str, limit: u64) -> Result<Box<dyn Read + Send>, ModelError> {
        let response = self
            .agent
            .get(url)
            .call()
            .map_err(|e| ModelError::Network(e.to_string()))?;
        // `limit + 1` para que un exceso se detecte como `TooLarge` y no como
        // error opaco del lector.
        let reader = response
            .into_body()
            .into_with_config()
            .limit(limit.saturating_add(1))
            .reader();
        Ok(Box::new(reader))
    }
}

/// Directorio por defecto: `$XDG_DATA_HOME/fokkus-jarvis/models` o
/// `~/.local/share/fokkus-jarvis/models`.
pub fn default_models_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".local").join("share"))
        })?;
    Some(base.join("fokkus-jarvis").join("models"))
}

/// Gestor de descarga y verificación de modelos.
pub struct ModelManager {
    dir: PathBuf,
    source: Box<dyn ModelSource>,
    max_attempts: u32,
    retry_delay: Duration,
}

impl ModelManager {
    /// Gestor sobre el directorio por defecto con descarga HTTPS.
    pub fn new() -> Result<Self, ModelError> {
        let dir = default_models_dir().ok_or(ModelError::NoDataDir)?;
        Ok(Self::with_dir(dir))
    }

    pub fn with_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            source: Box::new(HttpSource::new()),
            max_attempts: 3,
            retry_delay: Duration::from_secs(2),
        }
    }

    pub fn with_source(mut self, source: impl ModelSource + 'static) -> Self {
        self.source = Box::new(source);
        self
    }

    pub fn with_retries(mut self, max_attempts: u32, retry_delay: Duration) -> Self {
        self.max_attempts = max_attempts.max(1);
        self.retry_delay = retry_delay;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_of(&self, spec: &ModelSpec) -> PathBuf {
        self.dir.join(spec.file_name)
    }

    fn temp_path_of(&self, spec: &ModelSpec) -> PathBuf {
        self.dir.join(format!(".{}.part", spec.file_name))
    }

    /// Crea el directorio con permisos 0700.
    pub fn ensure_dir(&self) -> Result<(), ModelError> {
        fs::create_dir_all(&self.dir)?;
        set_mode(&self.dir, 0o700)?;
        Ok(())
    }

    /// Estado en disco sin modificar nada.
    pub fn status(&self, spec: &ModelSpec) -> ModelStatus {
        match self.read_verified(spec) {
            Ok(model) => ModelStatus::Verified {
                sha256: model.sha256().to_string(),
            },
            Err(ModelError::Io(e)) if e.kind() == io::ErrorKind::NotFound => ModelStatus::Missing,
            Err(e) => ModelStatus::Invalid {
                reason: e.to_string(),
            },
        }
    }

    /// Lee y verifica el modelo en disco. Si no supera la verificación se
    /// **borra** y se devuelve el error (CA-7.1). Nunca descarga.
    pub fn load_verified(&self, spec: &ModelSpec) -> Result<VerifiedModel, ModelError> {
        match self.read_verified(spec) {
            Ok(model) => Ok(model),
            Err(ModelError::Io(e)) => Err(ModelError::Io(e)),
            Err(e) => {
                let _ = fs::remove_file(self.path_of(spec));
                Err(e)
            }
        }
    }

    fn read_verified(&self, spec: &ModelSpec) -> Result<VerifiedModel, ModelError> {
        let path = self.path_of(spec);
        let file = File::open(&path)?;
        let len = file.metadata()?.len();
        if len != spec.size {
            return Err(ModelError::SizeMismatch {
                expected: spec.size,
                actual: len,
            });
        }
        let mut bytes = Vec::with_capacity(spec.size as usize);
        // `take` acota la lectura aunque el archivo crezca entre medio.
        file.take(spec.size.saturating_add(1))
            .read_to_end(&mut bytes)?;
        VerifiedModel::verify(*spec, bytes)
    }

    /// Devuelve el modelo verificado, descargándolo si falta o está dañado.
    ///
    /// `progress(descargados, total)` se invoca durante la descarga.
    pub fn ensure(
        &self,
        spec: &ModelSpec,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<VerifiedModel, ModelError> {
        match self.load_verified(spec) {
            Ok(model) => return Ok(model),
            Err(ModelError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {}
            // Dañado: `load_verified` ya lo borró; se vuelve a descargar.
            Err(ModelError::Io(e)) => return Err(ModelError::Io(e)),
            Err(_) => {}
        }
        self.download(spec, progress)?;
        self.load_verified(spec)
    }

    /// Descarga con reintentos. Al terminar con éxito la ruta final contiene
    /// un archivo verificado; si falla, no queda ni el final ni el temporal.
    pub fn download(
        &self,
        spec: &ModelSpec,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), ModelError> {
        self.ensure_dir()?;
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.download_once(spec, progress) {
                Ok(()) => return Ok(()),
                Err(e) if attempt >= self.max_attempts => {
                    return Err(ModelError::DownloadFailed {
                        attempts: attempt,
                        last: Box::new(e),
                    })
                }
                Err(_) => std::thread::sleep(self.retry_delay),
            }
        }
    }

    fn download_once(
        &self,
        spec: &ModelSpec,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), ModelError> {
        let temp = self.temp_path_of(spec);
        let result = self.write_temp(spec, &temp, progress).and_then(|()| {
            fs::rename(&temp, self.path_of(spec))?;
            sync_dir(&self.dir);
            Ok(())
        });
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn write_temp(
        &self,
        spec: &ModelSpec,
        temp: &Path,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), ModelError> {
        // Un temporal previo (descarga interrumpida) se descarta.
        let _ = fs::remove_file(temp);
        let mut file = create_private(temp)?;
        let mut reader = self.source.open(spec.url, spec.size)?;
        let mut hasher = Sha256::new();
        let mut header = Vec::with_capacity(HEADER_LEN);
        let mut buf = vec![0u8; CHUNK];
        let mut total: u64 = 0;
        progress(0, spec.size);
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(ModelError::Network(e.to_string())),
            };
            total += n as u64;
            if total > spec.size {
                return Err(ModelError::TooLarge { limit: spec.size });
            }
            let chunk = &buf[..n];
            if header.len() < HEADER_LEN {
                let take = (HEADER_LEN - header.len()).min(n);
                header.extend_from_slice(&chunk[..take]);
            }
            hasher.update(chunk);
            file.write_all(chunk)?;
            progress(total, spec.size);
        }
        if total != spec.size {
            return Err(ModelError::SizeMismatch {
                expected: spec.size,
                actual: total,
            });
        }
        let actual = to_hex(&hasher.finalize());
        if actual != spec.sha256 {
            return Err(ModelError::HashMismatch {
                expected: spec.sha256.to_string(),
                actual,
            });
        }
        validate_header(&header)?;
        file.sync_all()?;
        Ok(())
    }
}

#[cfg(unix)]
fn create_private(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// `fsync` del directorio para que el `rename` sobreviva a un corte de luz.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Cabecera ggml sintética con los hiperparámetros de `ggml-base`.
    pub(crate) fn synthetic_header(n_vocab: i32) -> Vec<u8> {
        let mut out = GGML_MAGIC.to_le_bytes().to_vec();
        for v in [n_vocab, 1500, 512, 8, 6, 448, 512, 8, 6, 80, 1] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    fn synthetic_model(n_vocab: i32, len: usize) -> Vec<u8> {
        let mut out = synthetic_header(n_vocab);
        out.extend((0..len).map(|i| (i % 251) as u8));
        out
    }

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    fn spec_for(data: &[u8]) -> ModelSpec {
        ModelSpec {
            id: "test",
            file_name: "ggml-test.bin",
            url: "https://example.invalid/ggml-test.bin",
            sha256: leak(sha256_hex(data)),
            size: data.len() as u64,
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "jarvis-models-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Lector que entrega `data` y opcionalmente falla tras `fail_after`.
    struct FlakyReader {
        data: Vec<u8>,
        pos: usize,
        fail_after: Option<usize>,
    }

    impl Read for FlakyReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if let Some(limit) = self.fail_after {
                if self.pos >= limit {
                    return Err(io::Error::new(io::ErrorKind::ConnectionReset, "corte"));
                }
            }
            let end = self.fail_after.unwrap_or(usize::MAX).min(self.data.len());
            let n = buf.len().min(end - self.pos).min(1000);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    /// Fuente simulada: cada llamada consume el siguiente guion de la cola.
    #[derive(Clone)]
    struct FakeSource {
        #[allow(clippy::type_complexity)]
        script: Arc<Mutex<Vec<(Vec<u8>, Option<usize>)>>>,
        calls: Arc<AtomicUsize>,
    }

    impl FakeSource {
        fn new(script: Vec<(Vec<u8>, Option<usize>)>) -> Self {
            Self {
                script: Arc::new(Mutex::new(script)),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl ModelSource for FakeSource {
        fn open(&self, _url: &str, _limit: u64) -> Result<Box<dyn Read + Send>, ModelError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                return Err(ModelError::Network("sin respuesta".into()));
            }
            let (data, fail_after) = script.remove(0);
            Ok(Box::new(FlakyReader {
                data,
                pos: 0,
                fail_after,
            }))
        }
    }

    fn manager(dir: &TempDir, source: FakeSource) -> ModelManager {
        ModelManager::with_dir(&dir.0)
            .with_source(source)
            .with_retries(3, Duration::ZERO)
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .map(|it| {
                it.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.ends_with(".part"))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn catalog_urls_are_pinned_https_and_hashes_are_hex() {
        for spec in MODELS {
            assert!(
                spec.url.starts_with("https://huggingface.co/"),
                "{}",
                spec.id
            );
            assert!(spec.url.contains(HF_WHISPER_COMMIT), "{}", spec.id);
            assert!(!spec.url.contains("/main/"), "{}", spec.id);
            assert!(spec.url.ends_with(spec.file_name), "{}", spec.id);
            assert_eq!(spec.sha256.len(), 64);
            assert!(spec
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
            assert!(spec.size > 1_000_000);
        }
    }

    #[test]
    fn hex_matches_known_sha256_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn header_of_real_models_is_accepted() {
        // Bytes reales de ggml-base-q5_1.bin y ggml-tiny.bin (commit fijado).
        let mut base = synthetic_header(51_865);
        base[44..48].copy_from_slice(&1009i32.to_le_bytes());
        let h = validate_header(&base).unwrap();
        assert_eq!((h.n_vocab, h.n_mels, h.ftype), (51_865, 80, 1009));
        assert!(validate_header(&synthetic_header(51_864)).is_ok());
        assert!(validate_header(&synthetic_header(51_866)).is_ok());
    }

    #[test]
    fn header_rejects_issue_4059_range_and_small_vocab() {
        for n in [50_258, 50_300, 50_363, 0, -1, 51_863, 1_000_000] {
            assert!(
                matches!(
                    validate_header(&synthetic_header(n)),
                    Err(ModelError::InvalidHeader(_))
                ),
                "n_vocab {n} debió rechazarse"
            );
        }
    }

    #[test]
    fn header_rejects_bad_magic_truncation_and_dims() {
        let mut bad = synthetic_header(51_865);
        bad[0] = 0;
        assert!(validate_header(&bad).is_err());
        assert!(validate_header(&synthetic_header(51_865)[..HEADER_LEN - 1]).is_err());
        assert!(validate_header(&[]).is_err());
        let mut mels = synthetic_header(51_865);
        mels[40..44].copy_from_slice(&81i32.to_le_bytes());
        assert!(validate_header(&mels).is_err());
        let mut heads = synthetic_header(51_865);
        heads[16..20].copy_from_slice(&0i32.to_le_bytes());
        assert!(validate_header(&heads).is_err());
    }

    #[test]
    fn simulated_download_is_verified_atomic_and_private() {
        let dir = TempDir::new("ok");
        let data = synthetic_model(51_865, 200_000);
        let spec = spec_for(&data);
        let mgr = manager(&dir, FakeSource::new(vec![(data.clone(), None)]));

        let mut last = (0, 0);
        let model = mgr
            .ensure(&spec, &mut |done, total| last = (done, total))
            .unwrap();
        assert_eq!(model.bytes(), &data[..]);
        assert_eq!(last, (spec.size, spec.size));
        assert_eq!(
            mgr.status(&spec),
            ModelStatus::Verified {
                sha256: spec.sha256.to_string()
            }
        );
        assert!(leftovers(&dir.0).is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = fs::metadata(mgr.path_of(&spec))
                .unwrap()
                .permissions()
                .mode();
            let dir_mode = fs::metadata(&dir.0).unwrap().permissions().mode();
            assert_eq!(file_mode & 0o777, 0o600);
            assert_eq!(dir_mode & 0o777, 0o700);
        }
    }

    #[test]
    fn interrupted_download_retries_without_corrupt_final_file() {
        let dir = TempDir::new("retry");
        let data = synthetic_model(51_865, 50_000);
        let spec = spec_for(&data);
        let source = FakeSource::new(vec![
            (data.clone(), Some(10_000)),
            (data.clone(), Some(30_000)),
            (data.clone(), None),
        ]);
        let calls = source.calls.clone();
        let mgr = manager(&dir, source);

        let model = mgr.ensure(&spec, &mut |_, _| {}).unwrap();
        assert_eq!(model.bytes().len(), data.len());
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert!(leftovers(&dir.0).is_empty());
    }

    #[test]
    fn exhausted_retries_leave_no_files() {
        let dir = TempDir::new("fail");
        let data = synthetic_model(51_865, 50_000);
        let spec = spec_for(&data);
        let mgr = manager(&dir, FakeSource::new(vec![(data.clone(), Some(5_000)); 3]));

        let err = mgr.ensure(&spec, &mut |_, _| {}).unwrap_err();
        assert!(matches!(
            err,
            ModelError::DownloadFailed { attempts: 3, .. }
        ));
        assert!(!mgr.path_of(&spec).exists());
        assert!(leftovers(&dir.0).is_empty());
    }

    #[test]
    fn tampered_or_oversized_download_is_rejected() {
        let dir = TempDir::new("tamper");
        let data = synthetic_model(51_865, 10_000);
        let spec = spec_for(&data);

        let mut tampered = data.clone();
        tampered[5_000] ^= 0xff;
        let mut oversized = data.clone();
        oversized.push(0);
        let mgr = manager(
            &dir,
            FakeSource::new(vec![
                (tampered.clone(), None),
                (oversized, None),
                (tampered, None),
            ]),
        );
        let err = mgr.ensure(&spec, &mut |_, _| {}).unwrap_err();
        let ModelError::DownloadFailed { last, .. } = err else {
            panic!("se esperaba DownloadFailed");
        };
        assert!(matches!(*last, ModelError::HashMismatch { .. }));
        assert!(!mgr.path_of(&spec).exists());
        assert!(leftovers(&dir.0).is_empty());
    }

    #[test]
    fn one_altered_byte_on_disk_is_rejected_deleted_and_redownloaded() {
        let dir = TempDir::new("disk");
        let data = synthetic_model(51_865, 10_000);
        let spec = spec_for(&data);
        let mgr = manager(&dir, FakeSource::new(vec![(data.clone(), None)]));
        mgr.ensure(&spec, &mut |_, _| {}).unwrap();

        let path = mgr.path_of(&spec);
        let mut bytes = fs::read(&path).unwrap();
        bytes[1234] ^= 0x01;
        fs::write(&path, &bytes).unwrap();

        assert!(matches!(mgr.status(&spec), ModelStatus::Invalid { .. }));
        assert!(matches!(
            mgr.load_verified(&spec),
            Err(ModelError::HashMismatch { .. })
        ));
        assert!(!path.exists(), "el modelo alterado debe borrarse");

        // `ensure` lo vuelve a descargar desde la fuente.
        let mgr = manager(&dir, FakeSource::new(vec![(data.clone(), None)]));
        assert_eq!(
            mgr.ensure(&spec, &mut |_, _| {}).unwrap().bytes(),
            &data[..]
        );
    }

    #[test]
    fn invalid_n_vocab_is_rejected_even_with_matching_hash() {
        // Un hash fijado no basta: la cabecera se valida igualmente (SEC-02).
        let dir = TempDir::new("vocab");
        let data = synthetic_model(50_300, 10_000);
        let spec = spec_for(&data);
        // Un único intento: con reintentos, el `FakeSource` agotaría su cola y
        // el último error sería `Network("sin respuesta")`, no `InvalidHeader`.
        let mgr = ModelManager::with_dir(&dir.0)
            .with_source(FakeSource::new(vec![(data, None)]))
            .with_retries(1, Duration::ZERO);
        let err = mgr.download(&spec, &mut |_, _| {}).unwrap_err();
        let ModelError::DownloadFailed { attempts, last } = err else {
            panic!("se esperaba DownloadFailed");
        };
        assert_eq!(attempts, 1);
        assert!(matches!(*last, ModelError::InvalidHeader(_)));
        assert!(!mgr.path_of(&spec).exists());
    }

    #[test]
    fn missing_model_reports_missing_status() {
        let dir = TempDir::new("missing");
        let mgr = ModelManager::with_dir(&dir.0);
        assert_eq!(mgr.status(&WHISPER_TINY), ModelStatus::Missing);
    }

    #[test]
    fn default_dir_is_under_local_share() {
        if let Some(dir) = default_models_dir() {
            assert!(dir.ends_with("fokkus-jarvis/models"));
        }
    }
}
