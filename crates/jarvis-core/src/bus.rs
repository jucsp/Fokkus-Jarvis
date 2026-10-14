//! Bus de eventos con canales dedicados (audio y transiciones).

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::state::{JarvisState, StateTransition};

/// Nivel de audio normalizado en `0.0..=1.0`.
pub type AudioLevel = f32;

/// Capacidad de cada canal. Los productores de audio usan `try_send`, por lo
/// que un consumidor lento descarta muestras en vez de bloquear el callback.
const CHANNEL_CAPACITY: usize = 64;

/// Bus de eventos no bloqueante para el hilo de audio.
pub struct EventBus {
    audio_tx: Sender<AudioLevel>,
    audio_rx: Receiver<AudioLevel>,
    state_tx: Sender<StateTransition>,
    state_rx: Receiver<StateTransition>,
}

impl EventBus {
    pub fn new() -> Self {
        let (audio_tx, audio_rx) = bounded(CHANNEL_CAPACITY);
        let (state_tx, state_rx) = bounded(CHANNEL_CAPACITY);
        Self {
            audio_tx,
            audio_rx,
            state_tx,
            state_rx,
        }
    }

    /// Productor clonable para el nivel de audio.
    pub fn audio_level_sender(&self) -> Sender<AudioLevel> {
        self.audio_tx.clone()
    }

    /// Receptor del nivel de audio.
    pub fn audio_level_receiver(&self) -> &Receiver<AudioLevel> {
        &self.audio_rx
    }

    /// Productor clonable para transiciones de estado.
    pub fn state_sender(&self) -> Sender<StateTransition> {
        self.state_tx.clone()
    }

    /// Receptor de transiciones de estado.
    pub fn state_receiver(&self) -> &Receiver<StateTransition> {
        &self.state_rx
    }

    /// Publica un nivel de audio saturado a `0.0..=1.0`.
    ///
    /// No bloquea: si el canal está lleno, descarta la muestra.
    pub fn publish_audio_level(&self, level: AudioLevel) {
        let clamped = level.clamp(0.0, 1.0);
        let _ = self.audio_tx.try_send(clamped);
    }

    /// Publica una transición de estado.
    ///
    /// No bloquea.
    pub fn publish_state_transition(&self, from: JarvisState, to: JarvisState) {
        let _ = self.state_tx.try_send(StateTransition { from, to });
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
