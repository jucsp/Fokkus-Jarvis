//! Motor STT alternativo sobre `sherpa-onnx` (HU-75, fase 1).
//!
//! Sustituye a whisper.cpp cuando se compila la crate con `--features sherpa`.
//! Ventajas frente a [`super::WhisperEngine`]:
//!
//! - **Sin alucinaciones en silencio por diseño**: un CTC/transducer no tiene
//!   el sesgo de whisper hacia texto de subtítulos. Aun así la compuerta RMS de
//!   [`super::prepare_audio`] se aplica igual, como primera barrera.
//! - **Latencia**: sin *prompt* inicial ni decodificador autorregresivo largo,
//!   la inferencia de un turno de 6 s queda por debajo de los ~600 ms de
//!   `base-q5_1`.
//! - Un único `.onnx` + `tokens.txt` para las familias tipo CTC.
//!
//! # Estado (HU-75, fase 2)
//!
//! La dependencia sigue siendo **opcional** en el `Cargo.toml` de
//! `jarvis-audio` (`default = []`), pero `jarvis-app` activa la feature
//! `sherpa` por defecto: el daemon intenta NeMo CTC antes que whisper y solo
//! cae a whisper si el modelo no está en disco o el motor no carga. Dos deudas
//! siguen abiertas:
//!
//! 1. El *build script* de `sherpa-onnx-sys` descarga un archivo nativo
//!    precompilado desde GitHub Releases si no se define `SHERPA_ONNX_LIB_DIR`.
//!    Eso choca de frente con SEC-01 (nada de binarios sin vendorizar, igual
//!    que se hizo con `vendor/whisper-rs-sys`). Antes de empaquetar hay que
//!    vendorizar la biblioteca o fijar `SHERPA_ONNX_LIB_DIR` a una copia local
//!    con hash.
//! 2. El `.onnx` y `tokens.txt` no están en el catálogo soberano de
//!    [`crate::models`]: [`locate_nemo_ctc`] los busca en una ruta **fija**
//!    bajo el directorio de modelos, así que no se aceptan rutas arbitrarias
//!    (SEC-02), pero todavía no hay verificación SHA-256.
//!
//! # Modelos y español
//!
//! `SenseVoice` **no está entrenado en español** (zh/en/ja/ko/yue). Para una
//! interfaz en español están [`SherpaFamily::Nemo`] (NeMo FastConformer CTC
//! int8, el que usa el daemon) y [`SherpaFamily::Whisper`] (encoder/decoder
//! ONNX exportados).

use std::fmt;
use std::path::{Path, PathBuf};

use sherpa_onnx::{
    OfflineNemoEncDecCtcModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
    OfflineSenseVoiceModelConfig, OfflineWhisperModelConfig,
};

use super::{clean_transcript, prepare_audio, SttError, Transcriber, MAX_THREADS, SAMPLE_RATE};

/// Etiqueta del backend para `--check` (CA-5.4).
pub const BACKEND_LABEL: &str = "sherpa-onnx 2.0.1 (offline)";

/// Directorio del modelo NeMo CTC español dentro del directorio de modelos.
///
/// Es una ruta **fija** (SEC-02): el motor nunca acepta rutas del usuario.
pub const NEMO_CTC_DIR: &str = "sherpa-onnx-nemo-fast-conformer-ctc-es-1424-int8";

/// Nombre del `.onnx` cuantizado a int8 dentro de [`NEMO_CTC_DIR`].
pub const NEMO_CTC_MODEL: &str = "model.int8.onnx";

/// Vocabulario que acompaña al modelo dentro de [`NEMO_CTC_DIR`].
pub const NEMO_CTC_TOKENS: &str = "tokens.txt";

/// Familia de modelo ONNX cargada por [`SherpaEngine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SherpaFamily {
    /// `SenseVoice`: un `model.onnx` + `tokens.txt`. Multipropósito
    /// (ASR + emoción + eventos), pero **sin español**.
    SenseVoice,
    /// NeMo FastConformer CTC (`model.int8.onnx` + `tokens.txt`). Es la
    /// familia del modelo español que usa el daemon.
    Nemo,
    /// Whisper exportado a ONNX: `encoder.onnx` + `decoder.onnx` +
    /// `tokens.txt`. Es la vía para transcribir español con sherpa-onnx.
    Whisper,
}

/// Reconocedor *offline* de sherpa-onnx envuelto en el trait [`Transcriber`].
pub struct SherpaEngine {
    recognizer: OfflineRecognizer,
    family: SherpaFamily,
    model_id: String,
    language: String,
}

impl SherpaEngine {
    /// Carga un modelo de la familia [`SherpaFamily::SenseVoice`].
    ///
    /// `language` es el código ISO-639-1 que espera el modelo (`"es"`, `"en"`…);
    /// con `None` el modelo autodetecta. Se acepta `None` porque SenseVoice no
    /// cubre español: forzar `"es"` ahí no mejora nada.
    pub fn sense_voice(
        model: impl AsRef<Path>,
        tokens: impl AsRef<Path>,
        language: Option<&str>,
    ) -> Result<Self, SttError> {
        let model = model.as_ref();
        let tokens = tokens.as_ref();
        require_file(model, "modelo ONNX")?;
        require_file(tokens, "tokens.txt")?;

        let mut config = OfflineRecognizerConfig::default();
        config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
            model: Some(path_string(model)?),
            language: language.map(str::to_string),
            // Normalización inversa de texto: números y puntuación, que es lo
            // que necesitan el corrector léxico y el detector de frases
            // inacabadas de HU-43.
            use_itn: true,
        };
        config.model_config.tokens = Some(path_string(tokens)?);

        Self::build(config, SherpaFamily::SenseVoice, model, language)
    }

    /// Carga un modelo NeMo FastConformer CTC (HU-75): `model.int8.onnx` +
    /// `tokens.txt`.
    ///
    /// El decodificador CTC no acepta *prompt* ni idioma: el texto sale de
    /// `tokens.txt`, así que el idioma del motor es solo informativo (`"es"`).
    pub fn nemo_ctc(model: impl AsRef<Path>, tokens: impl AsRef<Path>) -> Result<Self, SttError> {
        let model = model.as_ref();
        let tokens = tokens.as_ref();
        require_file(model, "modelo ONNX")?;
        require_file(tokens, "tokens.txt")?;

        let mut config = OfflineRecognizerConfig::default();
        config.model_config.nemo_ctc = OfflineNemoEncDecCtcModelConfig {
            model: Some(path_string(model)?),
        };
        config.model_config.tokens = Some(path_string(tokens)?);

        Self::build(config, SherpaFamily::Nemo, model, Some("es"))
    }

    /// Carga un modelo de la familia [`SherpaFamily::Whisper`] (encoder +
    /// decoder ONNX). Es la opción con soporte real de español.
    pub fn whisper(
        encoder: impl AsRef<Path>,
        decoder: impl AsRef<Path>,
        tokens: impl AsRef<Path>,
        language: impl Into<String>,
    ) -> Result<Self, SttError> {
        let encoder = encoder.as_ref();
        let decoder = decoder.as_ref();
        let tokens = tokens.as_ref();
        require_file(encoder, "encoder ONNX")?;
        require_file(decoder, "decoder ONNX")?;
        require_file(tokens, "tokens.txt")?;

        let language = language.into();
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.whisper = OfflineWhisperModelConfig {
            encoder: Some(path_string(encoder)?),
            decoder: Some(path_string(decoder)?),
            language: Some(language.clone()),
            task: Some("transcribe".to_string()),
            ..Default::default()
        };
        config.model_config.tokens = Some(path_string(tokens)?);

        Self::build(
            config,
            SherpaFamily::Whisper,
            encoder,
            Some(language.as_str()),
        )
    }

    /// Rellena los parámetros comunes y crea el reconocedor.
    ///
    /// Sin la comprobación previa de [`require_file`] esto sería una llamada
    /// al enlace nativo con rutas que pueden no existir: sherpa-onnx devuelve
    /// `None` al fallar `create`, pero deja un mensaje en stderr que acabaría
    /// en `journalctl`, así que se filtra antes.
    fn build(
        mut config: OfflineRecognizerConfig,
        family: SherpaFamily,
        model: &Path,
        language: Option<&str>,
    ) -> Result<Self, SttError> {
        config.model_config.num_threads = threads();
        // CPU explícita: el proyecto no asume GPU (ver `WhisperEngine`, que
        // fuerza `use_gpu(false)`).
        config.model_config.provider = Some("cpu".to_string());
        config.model_config.debug = false;

        let recognizer = OfflineRecognizer::create(&config).ok_or_else(|| {
            SttError::Backend(format!(
                "sherpa-onnx no pudo crear el reconocedor con {}",
                model.display()
            ))
        })?;

        Ok(Self {
            recognizer,
            family,
            model_id: model
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| model.display().to_string()),
            language: language.unwrap_or("auto").to_string(),
        })
    }

    pub fn family(&self) -> SherpaFamily {
        self.family
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    /// Etiqueta del backend para `--check`.
    pub fn backend_label(&self) -> &'static str {
        BACKEND_LABEL
    }

    /// Transcribe PCM mono 16 kHz y devuelve el texto limpio.
    ///
    /// Aplica la misma compuerta que whisper: silencio → cadena vacía sin
    /// inferencia (HU-75, CA-75.3). No usa *prompt* inicial: esa es una
    /// muleta de whisper que aquí no existe.
    pub fn transcribe(&self, pcm: &[f32]) -> Result<String, SttError> {
        let Some(audio) = prepare_audio(pcm) else {
            return Ok(String::new());
        };

        let stream = self.recognizer.create_stream();
        stream.accept_waveform(SAMPLE_RATE as i32, &audio);
        self.recognizer.decode(&stream);
        let text = stream
            .get_result()
            .map(|result| result.text)
            .unwrap_or_default();

        Ok(clean_transcript([text.as_str()]))
    }
}

impl Transcriber for SherpaEngine {
    fn transcribe(&self, pcm: &[f32]) -> Result<String, SttError> {
        SherpaEngine::transcribe(self, pcm)
    }
}

impl fmt::Debug for SherpaEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SherpaEngine")
            .field("family", &self.family)
            .field("model_id", &self.model_id)
            .field("language", &self.language)
            .finish_non_exhaustive()
    }
}

/// Localiza el modelo NeMo CTC dentro de `models_dir`.
///
/// Devuelve `(model.int8.onnx, tokens.txt)` solo si **ambos** existen: con
/// media pareja, `create` fallaría y dejaría el mensaje nativo en stderr. La
/// ruta cuelga siempre del directorio de modelos, nunca de la configuración
/// (SEC-02).
pub fn locate_nemo_ctc(models_dir: impl AsRef<Path>) -> Option<(PathBuf, PathBuf)> {
    let dir = models_dir.as_ref().join(NEMO_CTC_DIR);
    let model = dir.join(NEMO_CTC_MODEL);
    let tokens = dir.join(NEMO_CTC_TOKENS);
    if model.is_file() && tokens.is_file() {
        Some((model, tokens))
    } else {
        None
    }
}

/// Hilos de inferencia: los mismos topes que whisper (8).
fn threads() -> i32 {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_THREADS) as i32
}

/// Convierte la ruta al `String` que exige la API de sherpa-onnx.
fn path_string(path: &Path) -> Result<String, SttError> {
    match path.to_str() {
        Some(s) => Ok(s.to_string()),
        None => Err(SttError::Backend(format!(
            "la ruta {} no es UTF-8 y sherpa-onnx solo acepta UTF-8",
            path.display()
        ))),
    }
}

/// Rechaza rutas inexistentes antes de llamar al enlace nativo.
fn require_file(path: &Path, what: &str) -> Result<(), SttError> {
    if path.is_file() {
        Ok(())
    } else {
        Err(SttError::Backend(format!(
            "no existe el {what}: {}",
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_implements_the_transcriber_trait() {
        fn assert_transcriber<T: Transcriber>() {}
        assert_transcriber::<SherpaEngine>();
    }

    /// El orquestador lo transporta en `Box<dyn Transcriber + Send>`
    /// (`SttJob::SetTranscriber`), así que tiene que ser `Send + Sync`.
    #[test]
    fn engine_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SherpaEngine>();
    }

    /// Las rutas se validan antes de tocar el FFI: sin esto, un archivo que no
    /// existe llegaría al `create` nativo.
    #[test]
    fn missing_files_are_rejected_before_the_native_call() {
        let err =
            SherpaEngine::sense_voice("/no/existe/model.onnx", "/no/existe/tokens.txt", Some("es"))
                .expect_err("modelo inexistente");
        assert!(matches!(err, SttError::Backend(_)), "{err:?}");
        assert!(err.to_string().contains("modelo ONNX"), "{err}");

        let err = SherpaEngine::whisper(
            "/no/existe/encoder.onnx",
            "/no/existe/decoder.onnx",
            "/no/existe/tokens.txt",
            "es",
        )
        .expect_err("encoder inexistente");
        assert!(matches!(err, SttError::Backend(_)), "{err:?}");

        let err = SherpaEngine::nemo_ctc("/no/existe/model.int8.onnx", "/no/existe/tokens.txt")
            .expect_err("modelo NeMo inexistente");
        assert!(matches!(err, SttError::Backend(_)), "{err:?}");
        assert!(err.to_string().contains("modelo ONNX"), "{err}");
    }

    /// La ruta del modelo es fija bajo el directorio de modelos: si falta
    /// cualquiera de los dos archivos, no hay motor (SEC-02).
    #[test]
    fn locate_nemo_ctc_needs_both_files() {
        let root = std::env::temp_dir().join(format!("jarvis-sherpa-{}", std::process::id()));
        let dir = root.join(NEMO_CTC_DIR);
        std::fs::create_dir_all(&dir).expect("directorio temporal");
        assert!(locate_nemo_ctc(&root).is_none(), "directorio vacío");

        std::fs::write(dir.join(NEMO_CTC_MODEL), b"x").expect("modelo");
        assert!(locate_nemo_ctc(&root).is_none(), "sin tokens.txt");

        std::fs::write(dir.join(NEMO_CTC_TOKENS), b"x").expect("tokens");
        let (model, tokens) = locate_nemo_ctc(&root).expect("pareja completa");
        assert!(model.ends_with(NEMO_CTC_MODEL), "{}", model.display());
        assert!(tokens.ends_with(NEMO_CTC_TOKENS), "{}", tokens.display());

        std::fs::remove_dir_all(&root).ok();
    }

    /// Con el modelo NeMo CTC en disco, el motor construye y transcribe sin
    /// inventar texto en un tono puro (HU-75, CA-75.3). Se salta cuando el
    /// modelo no está descargado, para no exigir la descarga en `cargo test`.
    #[test]
    fn nemo_ctc_builds_and_transcribes_when_the_model_is_on_disk() {
        let Some(models_dir) = crate::models::default_models_dir() else {
            return;
        };
        let Some((model, tokens)) = locate_nemo_ctc(models_dir) else {
            return;
        };

        let engine = SherpaEngine::nemo_ctc(&model, &tokens).expect("motor NeMo CTC");
        assert_eq!(engine.family(), SherpaFamily::Nemo);
        assert_eq!(engine.language(), "es");
        assert_eq!(engine.backend_label(), BACKEND_LABEL);

        // Un tono audible supera la compuerta RMS y llega a la inferencia real.
        let tone: Vec<f32> = (0..SAMPLE_RATE)
            .map(|i| {
                let phase = 2.0 * std::f32::consts::PI * 220.0 * i as f32 / SAMPLE_RATE as f32;
                0.1 * phase.sin()
            })
            .collect();
        let text = engine.transcribe(&tone).expect("inferencia NeMo CTC");
        let lower = text.to_lowercase();
        assert!(
            !lower.contains("suscr") && !lower.contains("gracias por ver"),
            "un tono no debe alucinar subtítulos: {text:?}"
        );
    }

    /// Silencio → cadena vacía sin inferencia. Con un reconocedor nulo no se
    /// puede construir el motor, así que se comprueba la compuerta que comparte
    /// con whisper en el módulo padre.
    #[test]
    fn silence_short_circuits_before_inference() {
        assert!(prepare_audio(&[0.0; SAMPLE_RATE]).is_none());
    }
}
