//! Motor de audio de Fokkus-Jarvis (Fase 2).
//!
//! Agrupa:
//! - [`voices`]: catálogo de 15 voces neuronales.
//! - [`tts`]: síntesis Edge-TTS con fallback a espeak-ng y caché de fillers.
//! - [`playback`]: control de reproducción con parada instantánea (barge-in).
//! - [`stt`]: transcripción local con Whisper (binario del sistema).

pub mod playback;
pub mod stt;
pub mod tts;
pub mod voices;

pub use playback::AudioPlayback;
pub use stt::{SttError, WhisperEngine};
pub use tts::{EdgeTtsEngine, FillerCategory, TtsBackend, TtsError};
pub use voices::{resolve_voice, Gender, VoiceInfo, DEFAULT_VOICE_ID, DEFAULT_VOICE_LANG, VOICES};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_exactly_fifteen_voices() {
        assert_eq!(VOICES.len(), 15);
    }

    #[test]
    fn catalog_entries_are_complete_and_unique() {
        let mut ids = std::collections::HashSet::new();
        for voice in VOICES.iter() {
            assert!(!voice.id.is_empty());
            assert!(!voice.name.is_empty());
            assert!(!voice.lang.is_empty());
            assert!(!voice.region.is_empty());
            assert!(!voice.preview_text.is_empty());
            assert!(ids.insert(voice.id), "id duplicado: {}", voice.id);
        }
    }

    #[test]
    fn default_voice_is_in_catalog() {
        let voice = resolve_voice(DEFAULT_VOICE_ID);
        assert_eq!(voice.id, DEFAULT_VOICE_ID);
        assert_eq!(voice.lang, DEFAULT_VOICE_LANG);
    }

    #[test]
    fn resolve_voice_falls_back_to_default() {
        assert_eq!(resolve_voice("xx-XX-Desconocida").id, DEFAULT_VOICE_ID);
    }

    #[test]
    fn resolve_voice_matches_lang() {
        assert_eq!(resolve_voice("es-MX").lang, "es-MX");
        assert_eq!(resolve_voice("es-CL").id, DEFAULT_VOICE_ID);
    }

    #[test]
    fn sanitization_is_used_in_cache_paths() {
        let cache =
            std::env::temp_dir().join(format!("jarvis-lib-sanitize-{}", std::process::id()));
        let engine = EdgeTtsEngine::new().with_cache_dir(cache);
        let path = engine.filler_cache_path("texto con espacios");
        let voice_dir = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy();
        assert!(!voice_dir.contains('/'));
        assert!(!voice_dir.contains(' '));
    }

    #[test]
    fn fillers_catalog_is_non_empty_and_trimmed() {
        let fillers: Vec<&str> = tts::all_fillers().collect();
        assert!(!fillers.is_empty());
        for filler in &fillers {
            assert_eq!(*filler, filler.trim());
            assert!(!filler.is_empty());
        }
        let joined = fillers.join("\n");
        assert!(joined.contains("Dame un segundo, lo estoy revisando..."));
        assert!(joined.contains("Entendido, déjame ver..."));
        assert!(joined.contains("Estoy consultando eso ahora mismo..."));
    }
}
