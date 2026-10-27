//! Motor de síntesis de voz (Edge-TTS) con fallback a `espeak-ng` y caché de
//! frases de relleno (*fillers*) para acceso instantáneo sin red.
//!
//! La pre-síntesis guarda cada frase en
//! `~/.cache/fokkus-jarvis/fillers/<voz>/<hash>.mp3|.wav`; el nombre se deriva
//! de un hash FNV-1a estable (no criptográfico) del texto, suficiente como
//! clave de caché.

pub mod decode;
pub mod directive;
pub mod edge;
pub mod grouping;
pub mod phrase;
pub mod piper;
pub mod prosody;

pub use decode::{decode_mp3, PcmAudio};
pub use directive::{strip_directives, DirectedPhrase, Mood, SpeechDirective};
pub use edge::{synthesize_edge_native, synthesize_edge_with};
pub use grouping::PhraseGrouper;
pub use phrase::PhraseSplitter;
pub use piper::{parse_wav_pcm, synthesize_local_fallback};
pub use prosody::apply_directive_pcm;

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::voices::{resolve_voice, DEFAULT_VOICE_ID};

/// Categorías contextuales de fillers (dictamen §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillerCategory {
    General,
    Dev,
    Inbox,
}

impl FillerCategory {
    pub fn phrases(self) -> &'static [&'static str] {
        match self {
            FillerCategory::General => FILLERS_GENERAL,
            FillerCategory::Dev => FILLERS_DEV,
            FillerCategory::Inbox => FILLERS_INBOX,
        }
    }
}

/// Fillers de consultas generales.
pub const FILLERS_GENERAL: &[&str] = &[
    "Dame un segundo, lo estoy revisando...",
    "Entendido, déjame ver...",
    "Estoy consultando eso ahora mismo...",
    "Un momento, ya lo compruebo...",
    "Déjame pensar un instante...",
    "Voy a revisarlo, dame un momento...",
    "Ahora mismo te lo confirmo...",
    "Estoy en ello, un segundo...",
    "Mmm...",
    "A ver...",
    "Déjame pensar un segundo...",
    "Pensando...",
    "Veamos...",
    "Estoy en ello...",
    "Consultando...",
    "Dame un instante...",
    "Un momento...",
    "Ya mismo te respondo...",
    "Déjame ver...",
    "Estoy pensando...",
];

/// Fillers para órdenes de desarrollo.
pub const FILLERS_DEV: &[&str] = &[
    "Comprendido, analizando el repositorio...",
    "Preparando la tarea...",
    "Revisando el código, un momento...",
    "Entendido, voy a implementarlo...",
    "Analizando la base de código...",
    "Mmm, revisando el repo...",
    "A ver el código...",
    "Pensando la arquitectura...",
    "Déjame ver el código...",
    "Veamos los cambios...",
    "En ello con el código...",
    "Consultando el repositorio...",
    "Dame un segundo técnico...",
    "Revisando la implementación...",
    "Preparando los cambios...",
];

/// Fillers para la bandeja de entrada / tickets.
pub const FILLERS_INBOX: &[&str] = &[
    "Revisando la bandeja de entrada...",
    "Consultando los tickets pendientes...",
    "Un momento, reviso la bandeja...",
    "Mmm, revisando la bandeja...",
    "A ver los mensajes...",
    "Pensando el ticket...",
    "Veamos los mensajes...",
    "En ello con los tickets...",
    "Veamos qué llegó...",
    "Dame un instante para leerlo...",
    "Revisando los mensajes...",
    "Comprobando los tickets...",
];

/// Backend TTS seleccionado en tiempo de ejecución.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtsBackend {
    EdgeTts,
    EspeakNg,
    Unavailable,
}

/// Error del motor TTS.
#[derive(Debug)]
pub enum TtsError {
    NoBackend,
    Network(String),
    Decode(String),
    Piper(String),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Command {
        backend: &'static str,
        source: std::io::Error,
    },
    CommandFailed {
        backend: &'static str,
        status: std::process::ExitStatus,
    },
}

impl fmt::Display for TtsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TtsError::NoBackend => {
                write!(f, "no hay backend TTS disponible (edge-tts ni espeak-ng)")
            }
            TtsError::Network(msg) => {
                write!(f, "error de red: {msg}")
            }
            TtsError::Decode(msg) => {
                write!(f, "error decodificando audio: {msg}")
            }
            TtsError::Piper(msg) => {
                write!(f, "error de Piper: {msg}")
            }
            TtsError::Io { path, source } => {
                write!(f, "error de I/O en {}: {source}", path.display())
            }
            TtsError::Command { backend, source } => {
                write!(f, "no se pudo ejecutar {backend}: {source}")
            }
            TtsError::CommandFailed { backend, status } => {
                write!(f, "{backend} terminó con {status}")
            }
        }
    }
}

impl std::error::Error for TtsError {}

/// Motor Edge-TTS con fallback a espeak-ng y caché de fillers.
pub struct EdgeTtsEngine {
    edge_tts_bin: Option<PathBuf>,
    espeak_bin: Option<PathBuf>,
    cache_dir: PathBuf,
    voice_id: String,
    voice_lang: String,
    /// Cursor de rotación para no repetir siempre el primer filler cacheado.
    filler_cursor: AtomicU64,
}

impl EdgeTtsEngine {
    pub fn new() -> Self {
        Self::with_voice(DEFAULT_VOICE_ID)
    }

    pub fn with_voice(voice_id: impl Into<String>) -> Self {
        let id = voice_id.into();
        let voice = resolve_voice(&id);
        Self {
            edge_tts_bin: resolve_edge_tts(),
            espeak_bin: resolve_espeak_ng(),
            cache_dir: cache_root(),
            voice_id: voice.id.to_string(),
            voice_lang: voice.lang.to_string(),
            filler_cursor: AtomicU64::new(0),
        }
    }

    /// Motor con la misma caché y binarios que `self`, pero con otra voz.
    ///
    /// Lo usa el cambio de voz en caliente: la caché de fillers es global y
    /// por voz, así que solo cambia el directorio de voz al resolver rutas.
    pub fn for_voice(&self, voice_id: &str) -> Self {
        let voice = resolve_voice(voice_id);
        Self {
            edge_tts_bin: self.edge_tts_bin.clone(),
            espeak_bin: self.espeak_bin.clone(),
            cache_dir: self.cache_dir.clone(),
            voice_id: voice.id.to_string(),
            voice_lang: voice.lang.to_string(),
            filler_cursor: AtomicU64::new(0),
        }
    }

    pub fn with_cache_dir(mut self, dir: PathBuf) -> Self {
        self.cache_dir = dir;
        self
    }

    pub fn with_edge_tts_binary(mut self, bin: Option<PathBuf>) -> Self {
        self.edge_tts_bin = bin;
        self
    }

    pub fn with_espeak_binary(mut self, bin: Option<PathBuf>) -> Self {
        self.espeak_bin = bin;
        self
    }

    pub fn voice_id(&self) -> &str {
        &self.voice_id
    }

    pub fn voice_lang(&self) -> &str {
        &self.voice_lang
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn edge_tts_binary(&self) -> Option<&Path> {
        self.edge_tts_bin.as_deref()
    }

    pub fn espeak_binary(&self) -> Option<&Path> {
        self.espeak_bin.as_deref()
    }

    pub fn backend(&self) -> TtsBackend {
        if self.edge_tts_bin.is_some() {
            TtsBackend::EdgeTts
        } else if self.espeak_bin.is_some() {
            TtsBackend::EspeakNg
        } else {
            TtsBackend::Unavailable
        }
    }

    pub fn filler_cache_path(&self, text: &str) -> PathBuf {
        let voice_dir = sanitize_component(&self.voice_id);
        let hash = fnv1a64(text);
        let ext = self.audio_extension();
        self.cache_dir
            .join(voice_dir)
            .join(format!("{hash:016x}.{ext}"))
    }

    /// Devuelve un filler ya sintetizado y cacheado, sin red.
    ///
    /// En lugar de repetir siempre el primero, rota entre los disponibles con
    /// un cursor atómico (independiente de la hora del sistema).
    pub fn get_cached_filler(&self) -> Option<PathBuf> {
        self.pick_cached_filler(all_fillers())
    }

    /// Devuelve un filler cacheado de una categoría concreta (sin red),
    /// rotando entre los disponibles.
    ///
    /// Es la consulta usada por el `ConversationCoordinator` para disparar la
    /// frase de relleno contextual en < 300 ms: solo comprueba el sistema de
    /// archivos local, nunca invoca al backend TTS.
    pub fn get_cached_filler_for(&self, category: FillerCategory) -> Option<PathBuf> {
        self.pick_cached_filler(category.phrases().iter().copied())
    }

    /// Recolecta los fillers cacheados de `texts` y elige uno por rotación.
    fn pick_cached_filler(&self, texts: impl Iterator<Item = &'static str>) -> Option<PathBuf> {
        let cached: Vec<PathBuf> = texts
            .map(|text| self.filler_cache_path(text))
            .filter(|path| path.is_file())
            .collect();
        if cached.is_empty() {
            return None;
        }
        let index = self.filler_cursor.fetch_add(1, Ordering::Relaxed) as usize;
        cached.get(index % cached.len()).cloned()
    }

    /// Sintetiza (si faltan) todos los fillers en el directorio de caché.
    pub fn pre_synthesize_fillers(&self) -> Result<usize, TtsError> {
        if self.backend() == TtsBackend::Unavailable {
            return Err(TtsError::NoBackend);
        }
        let mut generated = 0usize;
        for text in all_fillers() {
            let path = self.filler_cache_path(text);
            if path.is_file() {
                continue;
            }
            self.synthesize_to_file(text, &path)?;
            generated += 1;
        }
        Ok(generated)
    }

    /// Sintetiza un texto a un archivo de audio usando el backend disponible.
    pub fn synthesize_to_file(&self, text: &str, out_path: &Path) -> Result<(), TtsError> {
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| TtsError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        match self.backend() {
            TtsBackend::EdgeTts => {
                let bin = self.edge_tts_bin.as_ref().ok_or(TtsError::NoBackend)?;
                let status = Command::new(bin)
                    .arg("--voice")
                    .arg(&self.voice_id)
                    .arg("--text")
                    .arg(text)
                    .arg("--write-media")
                    .arg(out_path)
                    .status()
                    .map_err(|source| TtsError::Command {
                        backend: "edge-tts",
                        source,
                    })?;
                if !status.success() {
                    return Err(TtsError::CommandFailed {
                        backend: "edge-tts",
                        status,
                    });
                }
            }
            TtsBackend::EspeakNg => {
                let bin = self.espeak_bin.as_ref().ok_or(TtsError::NoBackend)?;
                let lang = base_lang(&self.voice_lang);
                let status = Command::new(bin)
                    .arg("-v")
                    .arg(lang)
                    .arg("-w")
                    .arg(out_path)
                    .arg(text)
                    .status()
                    .map_err(|source| TtsError::Command {
                        backend: "espeak-ng",
                        source,
                    })?;
                if !status.success() {
                    return Err(TtsError::CommandFailed {
                        backend: "espeak-ng",
                        status,
                    });
                }
            }
            TtsBackend::Unavailable => return Err(TtsError::NoBackend),
        }
        Ok(())
    }

    /// Sintetiza una frase en memoria con Edge-TTS nativo y, si falla la red,
    /// degrada al fallback local de `espeak-ng`.
    pub fn synthesize_phrase_native(&self, text: &str) -> Result<PcmAudio, TtsError> {
        self.synthesize_directed(&DirectedPhrase::plain(text))
    }

    /// Sintetiza una frase con su prosodia (HU-46): Edge aplica ritmo, tono y
    /// volumen; las pausas y la respiración se añaden en local, también
    /// sobre el fallback de `espeak-ng`.
    pub fn synthesize_directed(&self, phrase: &DirectedPhrase) -> Result<PcmAudio, TtsError> {
        let pcm = match synthesize_edge_with(&phrase.text, &self.voice_id, &phrase.directive) {
            Ok(audio) => audio,
            Err(TtsError::Network(_)) => synthesize_local_fallback(&phrase.text, &self.voice_lang)?,
            Err(e) => return Err(e),
        };
        Ok(apply_directive_pcm(pcm, &phrase.directive))
    }

    fn audio_extension(&self) -> &'static str {
        match self.backend() {
            TtsBackend::EspeakNg => "wav",
            _ => "mp3",
        }
    }
}

impl Default for EdgeTtsEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Ruta raíz del caché de fillers: `~/.cache/fokkus-jarvis/fillers`.
pub fn cache_root() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("fokkus-jarvis").join("fillers");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home)
                .join(".cache")
                .join("fokkus-jarvis")
                .join("fillers");
        }
    }
    std::env::temp_dir().join("fokkus-jarvis").join("fillers")
}

/// Todas las frases de relleno, en orden estable (general → dev → inbox).
pub fn all_fillers() -> impl Iterator<Item = &'static str> {
    FillerCategory::General
        .phrases()
        .iter()
        .chain(FillerCategory::Dev.phrases().iter())
        .chain(FillerCategory::Inbox.phrases().iter())
        .copied()
}

/// Sanitiza un componente de nombre de archivo (solo alfanumérico, `-` y `_`).
pub fn sanitize_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

/// Busca un ejecutable en una variable `PATH` (lista separada por `:`).
pub(crate) fn find_executable_in_path(binary: &str, path_var: &str) -> Option<PathBuf> {
    if binary.is_empty() || binary.contains('/') {
        return None;
    }
    path_var
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(binary))
        .find(|candidate| candidate.is_file())
}

/// Resuelve `edge-tts`: primero en `PATH`, luego en `~/.local/bin/edge-tts`.
pub fn resolve_edge_tts() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?.into_string().ok()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_edge_tts_from(&path_var, home.as_deref().unwrap_or(Path::new("")))
}

pub(crate) fn resolve_edge_tts_from(path_var: &str, home: &Path) -> Option<PathBuf> {
    if let Some(found) = find_executable_in_path("edge-tts", path_var) {
        return Some(found);
    }
    let candidate = home.join(".local/bin/edge-tts");
    candidate.is_file().then_some(candidate)
}

/// Resuelve `espeak-ng` (o `espeak`) en `PATH`.
pub fn resolve_espeak_ng() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?.into_string().ok()?;
    resolve_espeak_ng_in_path(&path_var)
}

pub(crate) fn resolve_espeak_ng_in_path(path_var: &str) -> Option<PathBuf> {
    find_executable_in_path("espeak-ng", path_var)
        .or_else(|| find_executable_in_path("espeak", path_var))
}

fn base_lang(lang: &str) -> &str {
    lang.split('-').next().unwrap_or("es")
}

/// Genera en memoria un *chime* de confirmación suave (HU-18).
///
/// Son dos notas armónicas breves (~120 ms en total, 16 kHz mono) con ataque
/// corto y decaimiento exponencial: un tono elegante que confirma el turno
/// aceptado sin palabras y sin depender de la red ni de la voz sintética.
pub fn synth_chime() -> PcmAudio {
    use std::f32::consts::TAU;

    const RATE: u32 = 16_000;
    const TOTAL_MS: f32 = 120.0;
    const NOTE_MS: f32 = 55.0;
    const TOTAL: usize = (RATE as f32 * TOTAL_MS / 1_000.0) as usize;
    const NOTE: usize = (RATE as f32 * NOTE_MS / 1_000.0) as usize;
    const F1: f32 = 880.0;
    const F2: f32 = 1_174.66;
    const ATTACK_S: f32 = 0.004;

    let mut samples = Vec::with_capacity(TOTAL);
    for i in 0..TOTAL {
        let t = i as f32 / RATE as f32;
        let (freq, local) = if i < NOTE {
            (F1, i as f32 / NOTE as f32)
        } else {
            (F2, (i - NOTE) as f32 / NOTE as f32)
        };
        let attack = (t / ATTACK_S).min(1.0);
        let decay = (-3.0 * local).exp();
        let sample = (TAU * freq * t).sin() * 0.16 * attack * decay;
        samples.push(sample);
    }
    PcmAudio::new(samples, RATE, 1)
}

/// FNV-1a 64 bits (clave de caché estable; no criptográfico).
fn fnv1a64(text: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("jarvis-tts-{name}-{}", std::process::id()))
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sanitize_component_keeps_safe_chars() {
        assert_eq!(
            sanitize_component("es-CL-LorenzoNeural"),
            "es-CL-LorenzoNeural"
        );
        assert_eq!(sanitize_component("a_b-1"), "a_b-1");
    }

    #[test]
    fn sanitize_component_replaces_unsafe_chars() {
        assert_eq!(sanitize_component("es CL/Lorenzo?"), "es_CL_Lorenzo_");
    }

    #[test]
    fn find_executable_in_path_resolves() {
        let dir = temp_dir("find");
        cleanup(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("edge-tts"), b"").unwrap();
        let path_var = format!("/nope:{}:/otro:", dir.display());
        assert_eq!(
            find_executable_in_path("edge-tts", &path_var),
            Some(dir.join("edge-tts"))
        );
        assert_eq!(find_executable_in_path("ausente", &path_var), None);
        cleanup(&dir);
    }

    #[test]
    fn resolve_edge_tts_prefers_path_over_home() {
        let path_dir = temp_dir("path");
        let home = temp_dir("home");
        cleanup(&path_dir);
        cleanup(&home);
        std::fs::create_dir_all(&path_dir).unwrap();
        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        std::fs::write(path_dir.join("edge-tts"), b"").unwrap();
        std::fs::write(home.join(".local/bin/edge-tts"), b"").unwrap();

        let found = resolve_edge_tts_from(&path_dir.display().to_string(), &home);
        assert_eq!(found, Some(path_dir.join("edge-tts")));
        cleanup(&path_dir);
        cleanup(&home);
    }

    #[test]
    fn resolve_edge_tts_falls_back_to_home() {
        let path_dir = temp_dir("path2");
        let home = temp_dir("home2");
        cleanup(&path_dir);
        cleanup(&home);
        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        std::fs::write(home.join(".local/bin/edge-tts"), b"").unwrap();

        let found = resolve_edge_tts_from(&path_dir.display().to_string(), &home);
        assert_eq!(found, Some(home.join(".local/bin/edge-tts")));
        cleanup(&path_dir);
        cleanup(&home);
    }

    #[test]
    fn backend_selection_prefers_edge_then_espeak() {
        let engine = EdgeTtsEngine::new()
            .with_edge_tts_binary(None)
            .with_espeak_binary(None);
        assert_eq!(engine.backend(), TtsBackend::Unavailable);

        let engine = EdgeTtsEngine::new()
            .with_edge_tts_binary(None)
            .with_espeak_binary(Some(PathBuf::from("/bin/espeak-ng")));
        assert_eq!(engine.backend(), TtsBackend::EspeakNg);

        let engine = EdgeTtsEngine::new()
            .with_edge_tts_binary(Some(PathBuf::from("/bin/edge-tts")))
            .with_espeak_binary(Some(PathBuf::from("/bin/espeak-ng")));
        assert_eq!(engine.backend(), TtsBackend::EdgeTts);
    }

    #[test]
    fn filler_cache_path_is_deterministic_and_scoped() {
        let cache = temp_dir("cachepath");
        cleanup(&cache);
        let engine = EdgeTtsEngine::new()
            .with_cache_dir(cache.clone())
            .with_edge_tts_binary(Some(PathBuf::from("/bin/edge-tts")));
        let a = engine.filler_cache_path("hola");
        let b = engine.filler_cache_path("hola");
        assert_eq!(a, b);
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("mp3"));
        assert!(a.starts_with(&cache));
        cleanup(&cache);
    }

    #[test]
    fn get_cached_filler_returns_none_when_empty() {
        let cache = temp_dir("emptyfillers");
        cleanup(&cache);
        std::fs::create_dir_all(&cache).unwrap();
        let engine = EdgeTtsEngine::new().with_cache_dir(cache.clone());
        assert_eq!(engine.get_cached_filler(), None);
        cleanup(&cache);
    }

    #[test]
    fn get_cached_filler_returns_the_only_cached_file() {
        let cache = temp_dir("cachedfiller");
        cleanup(&cache);
        std::fs::create_dir_all(&cache).unwrap();
        let engine = EdgeTtsEngine::new().with_cache_dir(cache.clone());
        let first = engine.filler_cache_path(FILLERS_GENERAL[0]);
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::write(&first, b"audio").unwrap();

        assert_eq!(engine.get_cached_filler(), Some(first));
        cleanup(&cache);
    }

    #[test]
    fn get_cached_filler_rotates_across_cached_files() {
        let cache = temp_dir("rotatefillers");
        cleanup(&cache);
        std::fs::create_dir_all(&cache).unwrap();
        let engine = EdgeTtsEngine::new().with_cache_dir(cache.clone());
        let first = engine.filler_cache_path(FILLERS_GENERAL[0]);
        let second = engine.filler_cache_path(FILLERS_GENERAL[1]);
        for path in [&first, &second] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"audio").unwrap();
        }

        let a = engine.get_cached_filler().unwrap();
        let b = engine.get_cached_filler().unwrap();
        let c = engine.get_cached_filler().unwrap();
        assert_ne!(a, b, "la rotación alterna entre los fillers cacheados");
        assert_eq!(a, c, "tras una vuelta completa vuelve al primero");
        cleanup(&cache);
    }

    #[test]
    fn for_voice_keeps_cache_and_binaries_but_changes_voice() {
        let cache = temp_dir("forvoice");
        cleanup(&cache);
        let engine = EdgeTtsEngine::new()
            .with_cache_dir(cache.clone())
            .with_edge_tts_binary(Some(PathBuf::from("/bin/edge-tts")))
            .with_espeak_binary(None);
        let swapped = engine.for_voice("es-ES-ElviraNeural");
        assert_eq!(swapped.voice_id(), "es-ES-ElviraNeural");
        assert_eq!(swapped.voice_lang(), "es-ES");
        assert_eq!(swapped.cache_dir(), engine.cache_dir());
        assert_eq!(swapped.edge_tts_binary(), engine.edge_tts_binary());
        assert_eq!(swapped.espeak_binary(), engine.espeak_binary());
        cleanup(&cache);
    }

    #[test]
    fn synth_chime_is_brief_quiet_and_mono() {
        let pcm = synth_chime();
        assert_eq!(pcm.sample_rate, 16_000);
        assert_eq!(pcm.channels, 1);
        let duration = pcm.duration_seconds();
        assert!(
            (0.10..=0.15).contains(&duration),
            "chime de ~120 ms, duración: {duration} s"
        );
        let peak = pcm.samples.iter().copied().fold(0.0f32, f32::max).abs();
        assert!(peak > 0.0, "el chime no es silencio");
        assert!(peak < 0.3, "chime sutil (pico < 0.3): {peak}");
        // Decae al silencio al final: la cola no corta en seco.
        let tail = &pcm.samples[pcm.samples.len() - 16..];
        assert!(
            tail.iter().all(|s| s.abs() < 0.02),
            "decaimiento suave en la cola: {tail:?}"
        );
    }

    #[test]
    fn pre_synthesize_errors_without_backend() {
        let cache = temp_dir("nobackend");
        cleanup(&cache);
        let engine = EdgeTtsEngine::new()
            .with_cache_dir(cache.clone())
            .with_edge_tts_binary(None)
            .with_espeak_binary(None);
        assert!(matches!(
            engine.pre_synthesize_fillers(),
            Err(TtsError::NoBackend)
        ));
        cleanup(&cache);
    }

    #[test]
    #[cfg(unix)]
    fn pre_synthesize_uses_fake_edge_tts_to_cache_all_fillers() {
        use std::os::unix::fs::PermissionsExt;

        let cache = temp_dir("synth");
        cleanup(&cache);

        // Script falso de edge-tts: crea el archivo indicado en --write-media.
        let bin = temp_dir("fakebin").join("edge-tts");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        let script = r#"#!/bin/sh
prev=""
for arg in "$@"; do
  if [ "$prev" = "--write-media" ]; then
    : > "$arg"
    exit 0
  fi
  prev="$arg"
done
exit 1
"#;
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let engine = EdgeTtsEngine::new()
            .with_cache_dir(cache.clone())
            .with_edge_tts_binary(Some(bin.clone()))
            .with_espeak_binary(None);

        let total = all_fillers().count();
        let generated = engine.pre_synthesize_fillers().expect("síntesis con fake");
        assert_eq!(generated, total);

        // Segunda pasada: todo cacheado, no se regenera.
        assert_eq!(engine.pre_synthesize_fillers().unwrap(), 0);
        assert!(engine.get_cached_filler().is_some());

        cleanup(&cache);
        let _ = std::fs::remove_file(&bin);
    }
}
