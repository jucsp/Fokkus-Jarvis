//! Activación por voz y ventana de seguimiento (HU-4).
//!
//! [`ActivationDetector`] decide si una transcripción abre un turno de
//! conversación:
//!
//! * **Palabra de activación** `"Jarvis"` (configurable), detectada sobre el
//!   texto ya transcrito por cualquier [`crate::stt::Transcriber`] (HU-5) o
//!   directamente sobre un texto/segmento. La comparación es insensible a
//!   mayúsculas y tolerante a puntuación: `"¡Hola Jarvis!"`, `"Jarvis, qué
//!   hora es"` y `"jarvis?"` activan.
//! * **Ventana de seguimiento** de 8 s (configurable): tras una respuesta o un
//!   turno previo, los turnos siguientes dentro de la ventana se aceptan sin
//!   repetir `"Jarvis"`. La abre el coordinador con [`open_follow_up`] o la
//!   propia detección de la palabra clave. Cada turno aceptado **renueva** la
//!   ventana otros 8 s, de modo que la conversación fluye turno a turno y
//!   caduca sola tras 8 s de silencio (HU-17, HU-29).
//! * **Modo silencio**: [`mute`] rechaza toda entrada (CA-4.3) sin llegar a
//!   transcribir; [`unmute`] reactiva la captura.
//! * **Escucha continua** ([`with_continuous`]/[`set_continuous`]): cuando está
//!   activa, cualquier frase no vacía se acepta sin exigir `"Jarvis"`. Si la
//!   frase incluye la palabra clave, esta se elimina igual que en el modo
//!   normal.
//!
//! [`open_follow_up`]: ActivationDetector::open_follow_up
//! [`mute`]: ActivationDetector::mute
//! [`unmute`]: ActivationDetector::unmute
//! [`with_continuous`]: ActivationDetector::with_continuous
//! [`set_continuous`]: ActivationDetector::set_continuous

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::stt::{SttError, Transcriber};

/// Palabra de activación por defecto.
pub const DEFAULT_WAKE_WORD: &str = "jarvis";

/// Variantes fonéticas que whisper en español suele devolver para `"jarvis"`
/// (HU-32). Se aplican solo cuando la palabra configurada es la de por defecto.
const JARVIS_PHONETIC_VARIANTS: &[&str] = &[
    "jarvis", "yarvis", "jarbis", "yarbis", "llarvis", "llarbis", "charvis", "chárvis", "yárvis",
    "jervis", "sharvis",
];

/// Ventana de seguimiento por defecto (8 s, HU-29; antes 5 s, HU-17).
pub const DEFAULT_FOLLOW_UP: Duration = Duration::from_secs(8);

/// Resultado de procesar una transcripción o un texto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    /// Turno aceptado con la consulta limpia (sin el vocativo).
    Accepted {
        /// Intención pura para el LLM: `"Jarvis, qué hora es"` → `"qué hora es"`.
        query: String,
    },
    /// Se detectó la palabra clave pero no hay consulta (p. ej. solo `"Jarvis"`).
    /// La ventana de seguimiento queda abierta.
    WakeOnly,
    /// Sin activación: fuera de la ventana y sin palabra clave, o en silencio.
    Rejected,
}

impl Activation {
    /// `true` si el turno fue aceptado con una consulta.
    pub fn is_accepted(&self) -> bool {
        matches!(self, Activation::Accepted { .. })
    }

    /// Consulta aceptada, si existe.
    pub fn query(&self) -> Option<&str> {
        match self {
            Activation::Accepted { query } => Some(query),
            _ => None,
        }
    }
}

/// Detector de activación por voz con ventana de seguimiento y modo silencio.
///
/// Es `Send + Sync`: puede compartirse entre el hilo DSP/orquestador y el IPC.
#[derive(Debug)]
pub struct ActivationDetector {
    wake_word: String,
    follow_up: Duration,
    muted: AtomicBool,
    continuous: AtomicBool,
    /// Reposo por orden de silencio (HU-17): el micrófono sigue armado pero
    /// solo la palabra de activación abre turno.
    standby: AtomicBool,
    follow_up_until: Mutex<Option<Instant>>,
}

impl ActivationDetector {
    /// Detector con palabra clave `"Jarvis"` y ventana de 8 s.
    pub fn new() -> Self {
        Self {
            wake_word: DEFAULT_WAKE_WORD.to_string(),
            follow_up: DEFAULT_FOLLOW_UP,
            muted: AtomicBool::new(false),
            continuous: AtomicBool::new(false),
            standby: AtomicBool::new(false),
            follow_up_until: Mutex::new(None),
        }
    }

    /// Cambia la palabra de activación (por defecto `"Jarvis"`).
    pub fn with_wake_word(mut self, wake_word: impl Into<String>) -> Self {
        self.wake_word = wake_word.into();
        self
    }

    /// Cambia la duración de la ventana de seguimiento.
    pub fn with_follow_up(mut self, window: Duration) -> Self {
        self.follow_up = window;
        self
    }

    /// Activa (`true`) o desactiva (`false`) la escucha continua.
    pub fn with_continuous(mut self, enabled: bool) -> Self {
        self.continuous = AtomicBool::new(enabled);
        self
    }

    /// Palabra de activación configurada.
    pub fn wake_word(&self) -> &str {
        &self.wake_word
    }

    /// Duración de la ventana de seguimiento configurada.
    pub fn follow_up(&self) -> Duration {
        self.follow_up
    }

    /// Silencia la captura: toda entrada se rechaza (CA-4.3).
    pub fn mute(&self) {
        self.muted.store(true, Ordering::SeqCst);
    }

    /// Reactiva la captura.
    pub fn unmute(&self) {
        self.muted.store(false, Ordering::SeqCst);
        // Desmutear devuelve a escuchar: el reposo no sobrevive a un mute.
        self.leave_standby();
    }

    /// `true` si el asistente está silenciado.
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    /// Activa o desactiva la escucha continua en caliente.
    pub fn set_continuous(&self, enabled: bool) {
        self.continuous.store(enabled, Ordering::SeqCst);
        if enabled {
            // Escuchar siempre es incompatible con el reposo: activarla es una
            // orden explícita de volver a escuchar.
            self.leave_standby();
        }
    }

    /// `true` si la escucha continua está activa.
    pub fn is_continuous(&self) -> bool {
        self.continuous.load(Ordering::SeqCst)
    }

    /// Entra en reposo por orden de silencio ("silencio", "deja de escuchar").
    ///
    /// Cierra la ventana de seguimiento y apaga la escucha continua: a partir
    /// de aquí solo la palabra de activación abre turno (HU-17). El micrófono
    /// **no** se libera: no es un mute duro, la esfera debe verse en reposo.
    pub fn enter_standby(&self) {
        self.close_follow_up();
        self.continuous.store(false, Ordering::SeqCst);
        self.standby.store(true, Ordering::SeqCst);
    }

    /// Sale del reposo (palabra de activación, desmuteo o escucha continua).
    pub fn leave_standby(&self) {
        self.standby.store(false, Ordering::SeqCst);
    }

    /// `true` si está en reposo esperando la palabra de activación.
    pub fn is_standby(&self) -> bool {
        self.standby.load(Ordering::SeqCst)
    }

    /// Abre (o rearma) la ventana de seguimiento a partir de ahora.
    ///
    /// El coordinador la llama tras completar una respuesta para permitir
    /// preguntas de seguimiento sin repetir `"Jarvis"`.
    pub fn open_follow_up(&self) {
        self.open_follow_up_at(Instant::now());
    }

    /// Igual que [`open_follow_up`] pero con un instante explícito (tests).
    ///
    /// [`open_follow_up`]: ActivationDetector::open_follow_up
    pub fn open_follow_up_at(&self, now: Instant) {
        // La ventana nunca puede quedar anclada al pasado: si `now` corresponde
        // al inicio de una locución larga en diferido, `now + follow_up` podría
        // expirar antes de que whisper devuelva la transcripción (HU-32). Se
        // garantiza que siga viva al menos `follow_up` desde el instante real.
        let until = now + self.follow_up;
        let floor = Instant::now() + self.follow_up;
        *self.follow_up_until.lock().unwrap() = Some(until.max(floor));
    }

    /// Cierra la ventana de seguimiento de inmediato.
    pub fn close_follow_up(&self) {
        *self.follow_up_until.lock().unwrap() = None;
    }

    /// `true` si la ventana de seguimiento está abierta ahora.
    pub fn is_follow_up_open(&self) -> bool {
        self.is_follow_up_open_at(Instant::now())
    }

    /// Igual que [`is_follow_up_open`] pero con un instante explícito (tests).
    ///
    /// [`is_follow_up_open`]: ActivationDetector::is_follow_up_open
    pub fn is_follow_up_open_at(&self, now: Instant) -> bool {
        self.follow_up_until
            .lock()
            .unwrap()
            .is_some_and(|until| now < until)
    }

    /// Procesa un texto ya transcrito y decide si abre turno.
    ///
    /// - Silenciado → [`Activation::Rejected`] siempre.
    /// - Con escucha continua activa → cualquier frase no vacía se acepta; si
    ///   incluye la palabra clave, esta se elimina igual que en el modo normal.
    /// - Con palabra clave → [`Activation::Accepted`] (consulta limpia) o
    ///   [`Activation::WakeOnly`] (solo `"Jarvis"`); abre la ventana.
    /// - Sin palabra clave pero dentro de la ventana → [`Activation::Accepted`]
    ///   y renueva la ventana (HU-17).
    /// - En cualquier otro caso → [`Activation::Rejected`] (CA-4.2).
    pub fn process_text(&self, text: &str) -> Activation {
        self.process_text_at(text, Instant::now())
    }

    /// Igual que [`process_text`] pero con un instante explícito (tests).
    ///
    /// [`process_text`]: ActivationDetector::process_text
    pub fn process_text_at(&self, text: &str, now: Instant) -> Activation {
        self.process_at(text, now)
    }

    /// Igual que [`process_text_at`], pero pensado para transcripciones en
    /// diferido: `started_at` es el instante en que empezó la voz del
    /// segmento, no el instante en que terminó la transcripción.
    ///
    /// Así, una frase larga iniciada dentro de la ventana de seguimiento se
    /// acepta aunque whisper devuelva el texto después de que la ventana haya
    /// expirado (HU-29). La ventana se renueva a partir de `started_at`.
    ///
    /// [`process_text_at`]: ActivationDetector::process_text_at
    pub fn process_text_started_at(&self, text: &str, started_at: Instant) -> Activation {
        self.process_at(text, started_at)
    }

    /// Lógica común de decisión: `now` es el instante de referencia para
    /// comprobar y renovar la ventana de seguimiento.
    fn process_at(&self, text: &str, now: Instant) -> Activation {
        if self.is_muted() {
            return Activation::Rejected;
        }
        if self.is_continuous() {
            return self.process_continuous(text, now);
        }
        if contains_wake_word(text, &self.wake_word) {
            // Decir la palabra de activación saca del reposo (HU-17): la sesión
            // vuelve a estar viva aunque se hubiera pedido silencio.
            self.leave_standby();
            self.open_follow_up_at(now);
            let query = strip_wake_word(text, &self.wake_word);
            if query.is_empty() {
                Activation::WakeOnly
            } else {
                Activation::Accepted { query }
            }
        } else if self.follow_up_open(now) {
            let query = collapse_whitespace(text);
            if query.is_empty() {
                Activation::Rejected
            } else {
                // Cada turno aceptado renueva la ventana (HU-17): la sesión
                // sigue viva mientras el usuario continúe hablando.
                self.open_follow_up_at(now);
                Activation::Accepted { query }
            }
        } else {
            Activation::Rejected
        }
    }

    /// Decisión en escucha continua: cualquier frase no vacía abre turno. Si
    /// incluye la palabra clave, se elimina como en el modo normal.
    fn process_continuous(&self, text: &str, now: Instant) -> Activation {
        if contains_wake_word(text, &self.wake_word) {
            self.open_follow_up_at(now);
            let query = strip_wake_word(text, &self.wake_word);
            if query.is_empty() {
                Activation::WakeOnly
            } else {
                Activation::Accepted { query }
            }
        } else {
            let query = collapse_whitespace(text);
            if query.is_empty() {
                Activation::Rejected
            } else {
                Activation::Accepted { query }
            }
        }
    }

    /// Transcribe `pcm` (16 kHz mono) con el transcriptor dado y decide si
    /// abre turno.
    ///
    /// En modo silencio no se invoca siquiera al transcriptor: se rechaza la
    /// entrada de inmediato (CA-4.3).
    pub fn process_segment(
        &self,
        transcriber: &dyn Transcriber,
        pcm: &[f32],
    ) -> Result<Activation, SttError> {
        if self.is_muted() {
            return Ok(Activation::Rejected);
        }
        let text = transcriber.transcribe(pcm)?;
        Ok(self.process_text(&text))
    }

    fn follow_up_open(&self, now: Instant) -> bool {
        self.follow_up_until
            .lock()
            .unwrap()
            .is_some_and(|until| now < until)
    }
}

impl Default for ActivationDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// `true` si `text` contiene la palabra (o frase) de activación como token
/// completo, ignorando mayúsculas y puntuación.
///
/// La coincidencia es por token/frase delimitada por no-alfanuméricos:
/// `"jarvis"` activa pero `"jarvisito"` no.
pub fn contains_wake_word(text: &str, wake_word: &str) -> bool {
    let needle = normalize(wake_word);
    if needle.is_empty() {
        return false;
    }
    let haystack = normalize(text);
    let haystack = format!(" {haystack} ");

    // La palabra por defecto tolera las variantes fonéticas de whisper en
    // español (HU-32): "yarvis", "llarvis", "chárvis"… equivalen a "jarvis".
    if needle == DEFAULT_WAKE_WORD {
        return JARVIS_PHONETIC_VARIANTS.iter().any(|variant| {
            let variant = format!(" {variant} ");
            haystack.contains(variant.as_str())
        });
    }

    let needle = format!(" {needle} ");
    haystack.contains(needle.as_str())
}

/// Elimina el vocativo del texto, dejando solo la intención del usuario.
///
/// Se eliminan las palabras cuyo núcleo alfanumérico coincide con algún token
/// de la palabra de activación; el resto se rejunta con un solo espacio,
/// conservando la puntuación original de las palabras que quedan.
///
/// * `"Jarvis, qué hora es"` → `"qué hora es"`
/// * `"jarvis?"` → `""`
pub fn strip_wake_word(text: &str, wake_word: &str) -> String {
    let mut wake_tokens: std::collections::HashSet<String> = normalize(wake_word)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    // Las variantes fonéticas de "jarvis" también se limpian del vocativo
    // (HU-32): "yarvis, qué hora es" → "qué hora es".
    if wake_tokens.contains(DEFAULT_WAKE_WORD) {
        wake_tokens.extend(JARVIS_PHONETIC_VARIANTS.iter().map(|v| v.to_string()));
    }
    if wake_tokens.is_empty() {
        return collapse_whitespace(text);
    }
    // Una palabra puramente de puntuación (",", "…") se conserva.
    let kept: Vec<&str> = text
        .split_whitespace()
        .filter(|word| {
            let core = alphanumeric_lower(word);
            core.is_empty() || !wake_tokens.contains(&core)
        })
        .collect();
    kept.join(" ")
}

/// Normaliza a minúsculas y sustituye cualquier no-alfanumérico por un espacio,
/// colapsando los espacios repetidos. Base común de [`contains_wake_word`] y
/// [`strip_wake_word`].
fn normalize(text: &str) -> String {
    let spaced: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Núcleo alfanumérico en minúsculas de una palabra: `"Jarvis,"` → `"jarvis"`.
fn alphanumeric_lower(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Colapsa espacios y recorta extremos: `"  qué   hora  "` → `"qué hora"`.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    #[test]
    fn wake_word_detection_is_case_insensitive_and_punctuation_tolerant() {
        for text in [
            "Jarvis",
            "jarvis",
            "JARVIS",
            "¡Hola Jarvis!",
            "Jarvis, qué hora es",
            "jarvis?",
            "¿Jarvis?",
            "… Jarvis …",
            "el jarvis de mi equipo",
        ] {
            assert!(contains_wake_word(text, "Jarvis"), "debió activar: {text}");
        }
    }

    #[test]
    fn wake_word_does_not_match_partial_words() {
        for text in [
            "jarvisito",
            "el jarviss",
            "jarvismensaje",
            "hola",
            "qué hora es",
            "",
        ] {
            assert!(
                !contains_wake_word(text, "Jarvis"),
                "no debió activar: {text}"
            );
        }
    }

    #[test]
    fn wake_word_matches_spanish_phonetic_variants() {
        for text in [
            "yarvis, qué hora es",
            "jarbis",
            "yarbis, hola",
            "llarvis, dime",
            "llarbis",
            "charvis",
            "chárvis",
            "yárvis",
            "jervis",
            "sharvis",
            "Jarvis",
        ] {
            assert!(contains_wake_word(text, "Jarvis"), "debió activar: {text}");
        }
        // La limpieza del vocativo también reconoce las variantes (HU-32).
        assert_eq!(
            strip_wake_word("yarvis, qué hora es", "Jarvis"),
            "qué hora es"
        );
        assert_eq!(strip_wake_word("chárvis?", "Jarvis"), "");
        assert_eq!(strip_wake_word("llarvis, abre", "Jarvis"), "abre");
    }

    #[test]
    fn strip_wake_word_extracts_clean_query() {
        assert_eq!(
            strip_wake_word("Jarvis, qué hora es", "Jarvis"),
            "qué hora es"
        );
        assert_eq!(strip_wake_word("jarvis?", "Jarvis"), "");
        assert_eq!(strip_wake_word("Jarvis", "Jarvis"), "");
        assert_eq!(strip_wake_word("¡Hola Jarvis!", "Jarvis"), "¡Hola");
        assert_eq!(strip_wake_word("qué hora es", "Jarvis"), "qué hora es");
        assert_eq!(
            strip_wake_word("Jarvis, Jarvis, repite", "Jarvis"),
            "repite"
        );
        assert_eq!(
            strip_wake_word("  Jarvis,  abre  el  repo ", "Jarvis"),
            "abre el repo"
        );
    }

    #[test]
    fn process_text_with_wake_word_is_accepted_and_cleans_vocative() {
        let detector = ActivationDetector::new();
        assert_eq!(
            detector.process_text("Jarvis, qué hora es"),
            Activation::Accepted {
                query: "qué hora es".into()
            }
        );
        assert!(detector.is_follow_up_open());
    }

    #[test]
    fn process_text_wake_only_opens_window_without_query() {
        let detector = ActivationDetector::new();
        assert_eq!(detector.process_text("Jarvis"), Activation::WakeOnly);
        assert!(detector.is_follow_up_open());
        assert_eq!(detector.process_text("jarvis?"), Activation::WakeOnly);
    }

    #[test]
    fn process_text_without_wake_word_outside_window_is_rejected() {
        let detector = ActivationDetector::new();
        assert_eq!(detector.process_text("qué hora es"), Activation::Rejected);
        assert!(!detector.is_follow_up_open());
    }

    #[test]
    fn follow_up_window_accepts_and_renews_without_wake_word() {
        let detector = ActivationDetector::new(); // ventana de 8 s
        let t0 = Instant::now();
        detector.open_follow_up_at(t0);

        // A los 4 s: aceptado y la ventana se renueva 8 s más (hasta 12 s).
        assert_eq!(
            detector.process_text_at("qué hora es", t0 + Duration::from_secs(4)),
            Activation::Accepted {
                query: "qué hora es".into()
            }
        );
        // A los 8 s: sigue dentro de la ventana renovada y vuelve a renovar
        // (hasta 16 s).
        assert_eq!(
            detector.process_text_at("y mañana", t0 + Duration::from_secs(8)),
            Activation::Accepted {
                query: "y mañana".into()
            }
        );
        // La última renovación caduca a los 16 s: a los 17 s ya no hay sesión.
        assert_eq!(
            detector.process_text_at("otra cosa", t0 + Duration::from_secs(17)),
            Activation::Rejected
        );
        assert!(!detector.is_follow_up_open_at(t0 + Duration::from_secs(17)));
    }

    #[test]
    fn phrase_started_within_window_is_accepted_even_if_transcribed_after_expiry() {
        let detector = ActivationDetector::new(); // ventana de 8 s
        let t0 = Instant::now();
        detector.open_follow_up_at(t0);

        // La frase empieza a los 6 s (dentro de la ventana) pero whisper
        // devuelve el texto recién a los 10 s, cuando la ventana ya expiró
        // (HU-29): debe aceptarse usando el instante de inicio de la voz.
        assert_eq!(
            detector.process_text_started_at(
                "una frase larga sobre el clima",
                t0 + Duration::from_secs(6),
            ),
            Activation::Accepted {
                query: "una frase larga sobre el clima".into()
            }
        );
        // La ventana se renovó a partir del inicio de la frase (hasta 14 s).
        assert!(detector.is_follow_up_open_at(t0 + Duration::from_secs(13)));
        assert!(!detector.is_follow_up_open_at(t0 + Duration::from_secs(15)));
    }

    #[test]
    fn follow_up_window_never_anchors_to_a_past_started_at() {
        let detector = ActivationDetector::new(); // ventana de 8 s
        detector.open_follow_up();
        // Locución larga en diferido: empezó hace 30 s, así que
        // `started_at + 8 s` ya expiró. La ventana debe renovarse desde el
        // presente y seguir abierta, no quedar anclada al pasado (HU-32).
        let past = Instant::now() - Duration::from_secs(30);
        assert_eq!(
            detector.process_text_started_at("una continuación muy larga", past),
            Activation::Accepted {
                query: "una continuación muy larga".into()
            }
        );
        assert!(detector.is_follow_up_open());
    }

    #[test]
    fn follow_up_window_collapses_whitespace() {
        let detector = ActivationDetector::new();
        detector.open_follow_up();
        assert_eq!(
            detector.process_text("  qué   hora   es  "),
            Activation::Accepted {
                query: "qué hora es".into()
            }
        );
    }

    #[test]
    fn close_follow_up_expires_window_immediately() {
        let detector = ActivationDetector::new();
        detector.open_follow_up();
        assert!(detector.is_follow_up_open());
        detector.close_follow_up();
        assert!(!detector.is_follow_up_open());
        assert_eq!(detector.process_text("qué hora es"), Activation::Rejected);
    }

    #[test]
    fn mute_rejects_everything_and_does_not_open_window() {
        let detector = ActivationDetector::new();
        detector.mute();
        assert!(detector.is_muted());

        // La palabra clave no activa ni abre la ventana mientras está en silencio.
        assert_eq!(
            detector.process_text("Jarvis, qué hora es"),
            Activation::Rejected
        );
        assert!(!detector.is_follow_up_open());

        // Tampoco se acepta estando dentro de una ventana abierta a mano.
        detector.open_follow_up();
        assert_eq!(detector.process_text("qué hora es"), Activation::Rejected);

        detector.unmute();
        assert!(!detector.is_muted());
        assert_eq!(detector.process_text("Jarvis"), Activation::WakeOnly);
    }

    #[test]
    fn custom_wake_word_and_window_are_respected() {
        let detector = ActivationDetector::new()
            .with_wake_word("ok google")
            .with_follow_up(Duration::from_millis(50));

        assert_eq!(
            detector.process_text("Ok Google, dime algo"),
            Activation::Accepted {
                query: "dime algo".into()
            }
        );

        let t0 = Instant::now();
        detector.open_follow_up_at(t0);
        assert_eq!(
            detector.process_text_at("dime más", t0 + Duration::from_millis(10)),
            Activation::Accepted {
                query: "dime más".into()
            }
        );
        assert_eq!(
            detector.process_text_at("dime más", t0 + Duration::from_millis(60)),
            Activation::Rejected
        );
    }

    #[test]
    fn continuous_listening_accepts_any_non_empty_text() {
        let detector = ActivationDetector::new().with_continuous(true);
        assert!(detector.is_continuous());
        assert_eq!(
            detector.process_text("qué hora es"),
            Activation::Accepted {
                query: "qué hora es".into()
            }
        );
        // Sigue limpiando el vocativo si aparece.
        assert_eq!(
            detector.process_text("Jarvis, abre el repo"),
            Activation::Accepted {
                query: "abre el repo".into()
            }
        );
        assert_eq!(detector.process_text("Jarvis"), Activation::WakeOnly);
        assert_eq!(detector.process_text(""), Activation::Rejected);
        assert_eq!(detector.process_text("   "), Activation::Rejected);
    }

    #[test]
    fn continuous_listening_can_be_toggled_hot() {
        let detector = ActivationDetector::new();
        assert!(!detector.is_continuous());
        assert_eq!(detector.process_text("qué hora es"), Activation::Rejected);

        detector.set_continuous(true);
        assert!(detector.is_continuous());
        assert_eq!(
            detector.process_text("qué hora es"),
            Activation::Accepted {
                query: "qué hora es".into()
            }
        );

        detector.set_continuous(false);
        assert_eq!(detector.process_text("qué hora es"), Activation::Rejected);
    }

    #[test]
    fn continuous_listening_respects_mute() {
        let detector = ActivationDetector::new().with_continuous(true);
        detector.mute();
        assert_eq!(detector.process_text("qué hora es"), Activation::Rejected);
        assert_eq!(detector.process_text("Jarvis, hola"), Activation::Rejected);
    }

    struct FakeTranscriber {
        text: String,
        calls: Arc<AtomicUsize>,
    }

    impl Transcriber for FakeTranscriber {
        fn transcribe(&self, _pcm: &[f32]) -> Result<String, SttError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.text.clone())
        }
    }

    #[test]
    fn process_segment_uses_transcriber_and_respects_window() {
        let calls = Arc::new(AtomicUsize::new(0));
        let t1 = FakeTranscriber {
            text: "Jarvis, qué hora es".into(),
            calls: calls.clone(),
        };
        let detector = ActivationDetector::new();

        assert_eq!(
            detector.process_segment(&t1, &[0.0; 160]).unwrap(),
            Activation::Accepted {
                query: "qué hora es".into()
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Dentro de la ventana abierta por la palabra clave: se acepta sin
        // repetir "Jarvis".
        let t2 = FakeTranscriber {
            text: "qué hora es".into(),
            calls: calls.clone(),
        };
        assert_eq!(
            detector.process_segment(&t2, &[0.0; 160]).unwrap(),
            Activation::Accepted {
                query: "qué hora es".into()
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn process_segment_skips_transcription_when_muted() {
        let calls = Arc::new(AtomicUsize::new(0));
        let t = FakeTranscriber {
            text: "Jarvis".into(),
            calls: calls.clone(),
        };
        let detector = ActivationDetector::new();
        detector.mute();

        assert_eq!(
            detector.process_segment(&t, &[0.0; 160]).unwrap(),
            Activation::Rejected
        );
        // En silencio no se transcribe nada (CA-4.3).
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn detector_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ActivationDetector>();
    }

    #[test]
    fn activation_helpers_report_acceptance_and_query() {
        assert!(Activation::Accepted { query: "x".into() }.is_accepted());
        assert!(!Activation::WakeOnly.is_accepted());
        assert!(!Activation::Rejected.is_accepted());
        assert_eq!(
            Activation::Accepted { query: "x".into() }.query(),
            Some("x")
        );
        assert_eq!(Activation::WakeOnly.query(), None);
    }
}
