//! Eventos de voz que el hilo DSP publica hacia el coordinador conversacional.
//!
//! Se transportan por `crossbeam-channel`: el DSP produce y el orquestador
//! consume. Los eventos llevan marcas de tiempo monotónicas ([`Instant`]) porque
//! de ellas dependen las métricas de latencia del contrato v0.2.0 (filler
//! ≤ 300 ms y barge-in ≤ 50 ms).

use std::time::Instant;

use crate::tasks::TaskStatus;

/// Segmento de audio limpio y normalizado listo para la transcripción.
#[derive(Debug, Clone)]
pub struct Segment {
    /// PCM mono a 16 kHz en `f32`, muestras en `[-1.0, 1.0]`.
    pub pcm: Vec<f32>,
    /// Duración del segmento en milisegundos.
    pub duration_ms: u32,
}

/// Razón por la que el *endpointer* cerró el turno (HU-41).
///
/// Viaja dentro de [`VoiceEvent::EndOfTurn`] para que el orquestador (y el
/// volcado de diagnóstico `--diag-turns`) pueda auditar por qué se cortó cada
/// turno: un exceso de `Silence` significa que el usuario no había terminado y
/// conviene subir `silence_ms`; muchos `Connector` apuntan a un conector
/// demasiado largo o a un silencio extendido excesivo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// Venció el silencio normal (`silence_ms`).
    Silence,
    /// Se alcanzó el techo total del turno (`max_turn_ms`, HU-44).
    MaxTimeout,
    /// Cerró tras el silencio extendido por un conector pendiente
    /// (`extended_silence_ms`).
    Connector,
}

impl CloseReason {
    /// Etiqueta estable (minúsculas) para logs y el volcado de diagnóstico.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Silence => "silence",
            Self::MaxTimeout => "max_timeout",
            Self::Connector => "connector",
        }
    }
}

/// Eventos emitidos por el detector de voz y fin de turno (HU-3).
#[derive(Debug, Clone)]
pub enum VoiceEvent {
    /// Se detectó inicio de voz tras el *hangover* de apertura.
    ///
    /// Durante el estado `Speaking` este evento es el gancho del barge-in
    /// (HU-9): el coordinador lo consumirá para cortar la locución. Aquí solo
    /// se emite; HU-9 no está implementado en este módulo.
    VoiceStart {
        /// Marca de tiempo monotónica del inicio detectado.
        at: Instant,
    },
    /// Fragmento intermedio de un turno largo (HU-44): el usuario lleva
    /// `max_segment_ms` hablando y el turno sigue abierto. Se transcribe y
    /// acumula, pero **no** se despacha; el texto completo sale con el
    /// [`EndOfTurn`](Self::EndOfTurn) que cierra el turno.
    Partial {
        /// Marca de tiempo monotónica del corte del fragmento.
        at: Instant,
        /// Audio del fragmento (el primero incluye el *pre-roll*).
        segment: Segment,
        /// Posición del fragmento dentro del turno (0 = el primero).
        index: u32,
    },
    /// Se detectó el fin de turno (o se alcanzó el techo total del turno).
    EndOfTurn {
        /// Marca de tiempo monotónica del cierre.
        at: Instant,
        /// Audio del turno, incluyendo el *pre-roll* de 300 ms. Si hubo
        /// fragmentos [`Partial`](Self::Partial), solo el último tramo.
        segment: Segment,
        /// Fragmentos parciales emitidos antes de este cierre (HU-44).
        partials: u32,
        /// Por qué se cerró el turno (HU-41).
        close_reason: CloseReason,
    },
}

/// Identificador de turno de conversación para el HUD (HU-36).
pub type TurnId = u64;

/// Eventos del orquestador hacia el HUD de burbujas efímeras (HU-36).
///
/// Viajan por un canal acotado con `try_send`: si el HUD va atrasado el
/// evento se descarta y la voz nunca se frena.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HudEvent {
    /// Consulta aceptada del usuario, ya limpia del vocativo "Jarvis".
    UserSaid { turn: TurnId, text: String },
    /// Frase de Jarvis que empieza a sonar en el turno `turn`.
    JarvisPhrase { turn: TurnId, text: String },
    /// La respuesta del turno terminó de sonar: empieza su TTL.
    JarvisDone { turn: TurnId },
    /// La respuesta del turno se cortó (barge-in, nuevo turno).
    JarvisInterrupted { turn: TurnId },
    /// Aviso fuera de turno (IPC, resultados de tareas de HU-37).
    Notice { text: String },
    /// Vaciar el HUD.
    Clear,
    /// Sub-sesiones visibles (HU-67): chips junto a la esfera y submenú de la
    /// bandeja. Se emite solo cuando la lista cambia; vacía = sin chips.
    Sessions(Vec<SessionView>),
}

/// Sub-sesión tal como se muestra en el HUD y la bandeja (HU-67).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub id: u64,
    /// Título hablado ("la revisión de PR de Felipe").
    pub title: String,
    pub status: TaskStatus,
    /// Es la sub-sesión enfocada por voz (HU-64).
    pub focused: bool,
    /// Ya dejó detalle en disco (habilita "Abrir detalle" en la bandeja).
    pub has_detail: bool,
}
