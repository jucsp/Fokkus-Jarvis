//! Transcripción nativa en memoria con `whisper-rs` (HU-5).
//!
//! El contexto de whisper.cpp se carga **una vez** desde un
//! [`VerifiedModel`] (hash y cabecera ya comprobados por [`crate::models`]) y
//! se reutiliza en cada turno. Se transcribe directamente `&[f32]` mono a
//! 16 kHz: sin archivos WAV en disco ni binarios externos.

use std::fmt;
use std::sync::{Mutex, Once};

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperError,
    WhisperState,
};

use crate::models::{validate_header, ModelError, VerifiedModel};

/// Frecuencia de muestreo que espera whisper.
pub const SAMPLE_RATE: usize = 16_000;

/// whisper.cpp descarta entradas de menos de 1 s; se rellena con silencio
/// hasta 1,1 s para que un "Jarvis" corto (~500 ms) sí se transcriba.
pub const MIN_SAMPLES: usize = SAMPLE_RATE * 11 / 10;

/// Idioma por defecto.
pub const DEFAULT_LANGUAGE: &str = "es";

/// *Prompt* inicial por defecto (HU-43, ampliado en HU-70). Sesga el
/// decodificador hacia "Jarvis" y, sobre todo, hacia un estilo con
/// disfluencias: whisper imita el prompt, así que con él conserva las
/// muletillas ("eh", "o sea") y los puntos suspensivos en vez de borrarlas y
/// cerrar la frase con un punto. Sin esas marcas el detector de incompletitud
/// no puede ver que el usuario no terminó.
///
/// Desde HU-70 añade un **sesgo léxico** corto con los tecnicismos que más
/// deforma ("Docker", "Kubernetes", "Spectacle"…). Las muletillas de HU-43 van
/// **delante** y el sesgo al final, que es la parte que whisper pesa menos:
/// así no se pierde el estilo con disfluencias.
///
/// El riesgo que documentaba HU-43 (whisper puede repetir palabras de contenido
/// del prompt cuando el audio es silencio) queda acotado porque en silencio no
/// se transcribe (CA-4.3) y porque la compuerta de activación descarta todo
/// turno que no diga "Jarvis".
pub const DEFAULT_INITIAL_PROMPT: &str = "Jarvis, eh... este, mmm, o sea, como que, igual, Docker, Kubernetes, Spectacle, Wayland, GitHub, JSON, deploy...";

/// Máximo de hilos de CPU para la inferencia.
pub const MAX_THREADS: usize = 8;

/// Etiqueta del backend para `--check` (CA-5.4).
pub const BACKEND_LABEL: &str = "whisper-rs (whisper.cpp 1.9.x)";

/// Error de transcripción.
#[derive(Debug)]
pub enum SttError {
    /// El modelo no superó la validación previa a whisper.cpp.
    Model(ModelError),
    /// whisper.cpp no pudo cargar el modelo.
    Load(WhisperError),
    /// whisper.cpp falló durante la inferencia.
    Inference(WhisperError),
    /// El idioma o el *prompt* contienen bytes nulos.
    InvalidParameter(&'static str),
}

impl fmt::Display for SttError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SttError::Model(e) => write!(f, "modelo rechazado: {e}"),
            SttError::Load(e) => write!(f, "whisper.cpp no pudo cargar el modelo: {e}"),
            SttError::Inference(e) => write!(f, "falló la transcripción: {e}"),
            SttError::InvalidParameter(what) => write!(f, "parámetro inválido: {what}"),
        }
    }
}

impl std::error::Error for SttError {}

/// Algo capaz de transcribir PCM 16 kHz mono.
///
/// Lo implementa [`WhisperEngine`]; existe para que el filtro de activación
/// (HU-4) pueda probarse con transcriptores simulados.
pub trait Transcriber {
    fn transcribe(&self, pcm: &[f32]) -> Result<String, SttError>;
}

/// Motor de transcripción whisper enlazado.
pub struct WhisperEngine {
    // Se conserva el contexto para poder crear estados adicionales; el estado
    // ya guarda su propia referencia.
    _ctx: WhisperContext,
    state: Mutex<WhisperState>,
    model_id: &'static str,
    sha256: &'static str,
    language: String,
    initial_prompt: Option<String>,
    n_threads: i32,
}

impl WhisperEngine {
    /// Carga el contexto desde los bytes ya verificados.
    ///
    /// La cabecera se vuelve a validar aquí: es la última barrera antes de
    /// whisper.cpp (SEC-02).
    pub fn from_verified(model: VerifiedModel) -> Result<Self, SttError> {
        validate_header(model.bytes()).map_err(SttError::Model)?;
        silence_native_logs();

        let mut params = WhisperContextParameters::default();
        params.use_gpu(false);
        let ctx = WhisperContext::new_from_buffer_with_params(model.bytes(), params)
            .map_err(SttError::Load)?;
        let state = ctx.create_state().map_err(SttError::Load)?;
        let spec = *model.spec();
        // `model` (y sus bytes) se libera aquí: whisper.cpp ya copió los tensores.
        drop(model);

        let n_threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(MAX_THREADS) as i32;
        Ok(Self {
            _ctx: ctx,
            state: Mutex::new(state),
            model_id: spec.id,
            sha256: spec.sha256,
            language: DEFAULT_LANGUAGE.to_string(),
            initial_prompt: Some(DEFAULT_INITIAL_PROMPT.to_string()),
            n_threads,
        })
    }

    pub fn with_language(mut self, lang: impl Into<String>) -> Self {
        self.language = lang.into();
        self
    }

    /// *Prompt* inicial que sesga el decodificador. Reemplaza al de
    /// disfluencias por defecto ([`DEFAULT_INITIAL_PROMPT`]).
    pub fn with_initial_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.initial_prompt = Some(prompt.into());
        self
    }

    /// Transcribe sin *prompt* inicial.
    pub fn without_initial_prompt(mut self) -> Self {
        self.initial_prompt = None;
        self
    }

    pub fn initial_prompt(&self) -> Option<&str> {
        self.initial_prompt.as_deref()
    }

    pub fn with_threads(mut self, n_threads: usize) -> Self {
        self.n_threads = n_threads.clamp(1, 64) as i32;
        self
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    pub fn model_id(&self) -> &str {
        self.model_id
    }

    pub fn model_sha256(&self) -> &str {
        self.sha256
    }

    /// Transcribe PCM mono 16 kHz y devuelve el texto limpio de los segmentos.
    pub fn transcribe(&self, pcm: &[f32]) -> Result<String, SttError> {
        let Some(audio) = prepare_audio(pcm) else {
            return Ok(String::new());
        };
        if self.language.contains('\0') {
            return Err(SttError::InvalidParameter("idioma"));
        }
        if self
            .initial_prompt
            .as_deref()
            .is_some_and(|p| p.contains('\0'))
        {
            return Err(SttError::InvalidParameter("prompt inicial"));
        }

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(&self.language));
        params.set_translate(false);
        params.set_n_threads(self.n_threads);
        params.set_no_context(true);
        params.set_no_timestamps(true);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        // Sin *fallback* de temperatura: una sola pasada greedy (latencia).
        params.set_temperature(0.0);
        params.set_temperature_inc(0.0);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        if let Some(prompt) = &self.initial_prompt {
            params.set_initial_prompt(prompt);
        }

        // Un pánico previo con el lock tomado no invalida el estado nativo.
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.full(params, &audio).map_err(SttError::Inference)?;
        let segments: Vec<String> = state
            .as_iter()
            .filter_map(|segment| segment.to_str_lossy().ok().map(|s| s.into_owned()))
            .collect();
        Ok(clean_transcript(segments.iter().map(String::as_str)))
    }
}

impl Transcriber for WhisperEngine {
    fn transcribe(&self, pcm: &[f32]) -> Result<String, SttError> {
        WhisperEngine::transcribe(self, pcm)
    }
}

impl fmt::Debug for WhisperEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WhisperEngine")
            .field("model_id", &self.model_id)
            .field("language", &self.language)
            .field("n_threads", &self.n_threads)
            .finish_non_exhaustive()
    }
}

/// Redirige los logs de whisper.cpp/ggml fuera de stderr: con
/// `log_backend` desactivado quedan descartados, así ninguna transcripción
/// llega a `journalctl` (CA-10.2).
fn silence_native_logs() {
    static ONCE: Once = Once::new();
    ONCE.call_once(whisper_rs::install_logging_hooks);
}

/// Sanea y rellena el audio. `None` si no hay muestras (0 ms).
///
/// NaN/±inf pasan a 0 y el resto se acota a `[-1, 1]` para que whisper.cpp
/// nunca reciba valores fuera de rango (SEC-01/SEC-08).
pub fn prepare_audio(pcm: &[f32]) -> Option<Vec<f32>> {
    if pcm.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(pcm.len().max(MIN_SAMPLES));
    out.extend(pcm.iter().map(|&s| {
        if s.is_finite() {
            s.clamp(-1.0, 1.0)
        } else {
            0.0
        }
    }));
    if out.len() < MIN_SAMPLES {
        out.resize(MIN_SAMPLES, 0.0);
    }
    Some(out)
}

/// Une los segmentos en una sola línea, colapsa espacios y quita las
/// anotaciones de no-habla de whisper (`[música]`, `(risas)`,
/// `[BLANK_AUDIO]`…).
pub fn clean_transcript<'a>(segments: impl IntoIterator<Item = &'a str>) -> String {
    let mut words: Vec<String> = Vec::new();
    for segment in segments {
        let stripped = strip_annotations(segment);
        words.extend(stripped.split_whitespace().map(str::to_string));
    }
    words.join(" ")
}

fn strip_annotations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth_square = 0u32;
    let mut depth_round = 0u32;
    let mut depth_star = false;
    for ch in text.chars() {
        match ch {
            '[' => depth_square += 1,
            ']' => depth_square = depth_square.saturating_sub(1),
            '(' => depth_round += 1,
            ')' => depth_round = depth_round.saturating_sub(1),
            '*' => depth_star = !depth_star,
            _ if depth_square == 0 && depth_round == 0 && !depth_star => out.push(ch),
            _ => {}
        }
        if matches!(ch, ']' | ')' | '*' | '[' | '(') {
            out.push(' ');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_audio_returns_none_for_zero_ms() {
        assert!(prepare_audio(&[]).is_none());
    }

    #[test]
    fn prepare_audio_pads_short_input_with_silence() {
        // 10 ms (CA-5.3): se rellena hasta el mínimo de whisper.
        let audio = prepare_audio(&[0.5; 160]).unwrap();
        assert_eq!(audio.len(), MIN_SAMPLES);
        assert!(audio[..160].iter().all(|&s| s == 0.5));
        assert!(audio[160..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn prepare_audio_keeps_long_input_length() {
        let thirty_s = vec![0.1; SAMPLE_RATE * 30];
        assert_eq!(prepare_audio(&thirty_s).unwrap().len(), SAMPLE_RATE * 30);
    }

    #[test]
    fn prepare_audio_sanitizes_non_finite_and_clamps() {
        let audio =
            prepare_audio(&[f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 3.0, -3.0, 0.25]).unwrap();
        assert_eq!(&audio[..6], &[0.0, 0.0, 0.0, 1.0, -1.0, 0.25]);
    }

    #[test]
    fn clean_transcript_joins_and_collapses_whitespace() {
        assert_eq!(
            clean_transcript(["  Hola   Jarvis, ", " ¿qué hora es? "]),
            "Hola Jarvis, ¿qué hora es?"
        );
    }

    #[test]
    fn clean_transcript_drops_non_speech_annotations() {
        assert_eq!(clean_transcript(["[BLANK_AUDIO]"]), "");
        assert_eq!(
            clean_transcript(["[Música] hola (risas) mundo *aplausos*"]),
            "hola mundo"
        );
        assert_eq!(clean_transcript(["a]b)c"]), "a b c");
    }

    #[test]
    fn default_prompt_biases_towards_jarvis_and_spanish_disfluencies() {
        let prompt = DEFAULT_INITIAL_PROMPT;
        assert!(prompt.starts_with("Jarvis"));
        assert!(!prompt.contains('\0'));
        for filler in ["eh", "este", "mmm", "o sea", "como que", "igual", "..."] {
            assert!(prompt.contains(filler), "falta {filler:?}");
        }
        // El propio prompt termina abierto: whisper no debe aprender a cerrar
        // cada frase con un punto.
        assert!(crate::incompleteness::is_incomplete(prompt));
    }

    /// CA-70.7: el sesgo léxico cabe en el presupuesto de tokens de whisper
    /// (`n_text_ctx / 2` ≈ 224) y no desplaza las disfluencias de HU-43.
    #[test]
    fn default_prompt_biases_the_technical_lexicon_within_the_token_budget() {
        let prompt = DEFAULT_INITIAL_PROMPT;
        let words = prompt.split_whitespace().count();
        assert!(
            words <= 30,
            "{words} palabras: demasiado largo para el sesgo"
        );
        assert!(
            prompt.chars().count() <= 160,
            "{} caracteres",
            prompt.chars().count()
        );
        for term in [
            "Docker",
            "Kubernetes",
            "Spectacle",
            "Wayland",
            "GitHub",
            "JSON",
        ] {
            assert!(prompt.contains(term), "falta el sesgo {term}");
        }
        let bias = prompt.find("Docker").expect("sesgo léxico");
        for filler in ["eh", "este", "mmm", "o sea", "como que", "igual"] {
            let at = prompt.find(filler).expect(filler);
            assert!(at < bias, "la muletilla {filler:?} quedó tras el sesgo");
        }
        // Y sigue siendo una frase abierta.
        assert!(crate::incompleteness::is_incomplete(prompt));
    }

    #[test]
    fn engine_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WhisperEngine>();
    }

    /// Integración real: requiere `ggml-tiny.bin` verificado en el directorio
    /// de modelos (`cargo test -p jarvis-audio -- --ignored`).
    #[test]
    #[ignore = "requiere el modelo ggml-tiny descargado"]
    fn real_tiny_model_handles_0ms_10ms_and_30s() {
        let manager = crate::models::ModelManager::new().unwrap();
        let model = manager
            .ensure(&crate::models::WHISPER_TINY, &mut |_, _| {})
            .expect("modelo tiny");
        let engine = WhisperEngine::from_verified(model).expect("contexto whisper");
        assert_eq!(engine.language(), "es");
        assert_eq!(engine.initial_prompt(), Some(DEFAULT_INITIAL_PROMPT));
        assert_eq!(engine.transcribe(&[]).unwrap(), "");
        let _ = engine.transcribe(&[0.0; 160]).unwrap();
        let noise: Vec<f32> = (0..SAMPLE_RATE * 30)
            .map(|i| ((i * 7919) % 2000) as f32 / 20_000.0 - 0.05)
            .collect();
        let _ = engine.transcribe(&noise).unwrap();
        // El contexto se reutiliza entre llamadas.
        let _ = engine.transcribe(&[0.0; SAMPLE_RATE]).unwrap();
    }
}
