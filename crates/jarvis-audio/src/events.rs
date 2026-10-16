//! Eventos de voz que el hilo DSP publica hacia el coordinador conversacional.
//!
//! Se transportan por `crossbeam-channel`: el DSP produce y el orquestador
//! consume. Los eventos llevan marcas de tiempo monotónicas ([`Instant`]) porque
//! de ellas dependen las métricas de latencia del contrato v0.2.0 (filler
//! ≤ 300 ms y barge-in ≤ 50 ms).

use std::time::Instant;

/// Segmento de audio limpio y normalizado listo para la transcripción.
#[derive(Debug, Clone)]
pub struct Segment {
    /// PCM mono a 16 kHz en `f32`, muestras en `[-1.0, 1.0]`.
    pub pcm: Vec<f32>,
    /// Duración del segmento en milisegundos.
    pub duration_ms: u32,
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
    /// Se detectó el fin de turno (o se alcanzó la duración máxima).
    EndOfTurn {
        /// Marca de tiempo monotónica del cierre.
        at: Instant,
        /// Audio del turno completo, incluyendo el *pre-roll* de 300 ms.
        segment: Segment,
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
}
