//! Motor de audio de Fokkus-Jarvis (Fase 2).
//!
//! Agrupa:
//! - [`voices`]: catálogo de 19 voces neuronales.
//! - [`tts`]: síntesis Edge-TTS con fallback a espeak-ng y caché de fillers.
//! - [`playback`]: control de reproducción con parada instantánea (barge-in).
//! - [`stt`]: transcripción local con Whisper (binario del sistema).
//! - [`conversation`]: gestor de diálogo con fillers contextuales e interrupción
//!   humana (Issues #3 y #4).
//! - [`io`]: captura y reproducción con `cpal` (callbacks sin locks ni
//!   asignaciones, métricas atómicas).
//! - [`dsp`]: remuestreo a 16 kHz, AEC3 + NS + AGC2 y re-encuadre (hilo
//!   `jarvis-audio-dsp`).
//! - [`vad`]: detección de voz con `earshot` y *endpointing* de turno.
//! - [`events`]: eventos de voz hacia el coordinador.
//! - [`incompleteness`]: detector léxico de frases inacabadas en español
//!   (HU-43).
//! - [`activation`]: palabra de activación "Jarvis", ventana de seguimiento y
//!   modo silencio (HU-4).
//! - [`models`]: descarga y verificación segura de modelos whisper (HU-7).
//! - [`brain`]: cliente de la API de Mensajes de Anthropic con streaming SSE y
//!   memoria de turnos (HU-6).
//! - [`agent`]: matriz de riesgo y confirmación vocal del agente visual
//!   (HU-39).
//! - [`orchestrator`]: cableado de punta a punta `VoiceEvent` → STT → Claude →
//!   TTS con barge-in y silencio real (HU-9).
//! - [`text`]: normalización léxica única de las órdenes de voz (tildes,
//!   enclíticos y variantes fonéticas de STT) para las heurísticas (HU-59).
//! - [`sessions`]: multi-sesiones y subhilos de conversación con persistencia
//!   (HU-62).

pub mod activation;
pub mod agent;
pub mod brain;
pub mod conversation;
pub mod dsp;
pub mod events;
pub mod incompleteness;
pub mod io;
pub mod models;
pub mod orchestrator;
pub mod playback;
pub mod sessions;
pub mod speech_queue;
pub mod stt;
pub mod tasks;
pub mod text;
pub mod tts;
pub mod vad;
pub mod voices;

pub use activation::{Activation, ActivationDetector};
pub use brain::{
    resolve_api_key, store_api_key, BrainError, CancellationToken, ClaudeClient, CliBrain,
    Completion, Message, SseEvent, SseParser, DEFAULT_CLI_COMMAND, SUBAGENT_SYSTEM_PROMPT,
};
pub use conversation::{
    barge_in, classify_intent, BackgroundTasks, ConversationCoordinator, ConversationError,
    TokenStream,
};
pub use dsp::{
    spawn_dsp_thread, ConnectorSignal, DspError, DspHandle, DspMetrics, DspMetricsSnapshot,
    DspThreadConfig, LatencyProbe, ProcessorConfig,
};
pub use events::{CloseReason, HudEvent, Segment, TurnId, VoiceEvent};
pub use incompleteness::{incompleteness, incompleteness_score, is_incomplete, IncompleteCue};
pub use io::{
    AudioEngine, AudioError, AudioMetrics, AudioMetricsSnapshot, EngineConfig, EngineStatus,
    PlayOutcome, Player,
};
pub use models::{
    default_models_dir, validate_header, HttpSource, ModelError, ModelHeader, ModelManager,
    ModelSource, ModelSpec, ModelStatus, VerifiedModel, MODELS, WHISPER_BASE_Q5_1, WHISPER_TINY,
};
pub use orchestrator::{
    AppLauncher, AudioSink, AutomationInput, AutomationKill, CaptureControl, EngineControl,
    LocalClock, VoiceConfig, VoiceDeps, VoiceOrchestrator, VoiceSpeaker, VoiceStatsSnapshot,
};
pub use playback::AudioPlayback;
pub use speech_queue::{
    Delivery, DeliveryGate, SpeechPriority, SpeechQueue, SPEECH_QUEUE_CAPACITY,
};
pub use stt::{SttError, Transcriber, WhisperEngine};
pub use tasks::{
    classify_workload, dev_task_prompt, split_task_output, summarize, task_title, BrainFactory,
    TaskConfig, TaskId, TaskManager, TaskOrigin, TaskRecord, TaskStatus, Workload, DEV_TASK_TITLE,
};
pub use tts::{
    strip_directives, DirectedPhrase, EdgeTtsEngine, FillerCategory, Mood, PcmAudio, PhraseGrouper,
    PhraseSplitter, SpeechDirective, TtsBackend, TtsError,
};
pub use vad::{EndpointConfig, Endpointer, TurnPauseStats, TurnPauseSummary, VadDetector};
pub use voices::{resolve_voice, Gender, VoiceInfo, DEFAULT_VOICE_ID, DEFAULT_VOICE_LANG, VOICES};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_exactly_nineteen_voices() {
        assert_eq!(VOICES.len(), 19);
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
