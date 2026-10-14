//! Bus de eventos: nivel de audio (último valor gana) y transiciones (canal).

use std::sync::atomic::{AtomicU32, Ordering};

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::state::{JarvisState, StateTransition};

/// Nivel de audio normalizado en `0.0..=1.0`.
pub type AudioLevel = f32;

/// Capacidad del canal de transiciones de estado.
const STATE_CAPACITY: usize = 64;

/// Registro del nivel de audio con semántica "último valor gana".
///
/// Un consumidor lento (p. ej. el bucle de render a 60 fps) siempre observa la
/// muestra más reciente; nunca lee un valor viejo ni descarta muestras nuevas
/// (H5 del reporte QA).
#[derive(Debug, Default)]
pub struct AudioLevelStore {
    /// Bits del último `f32` publicado (`0.0` al arrancar).
    bits: AtomicU32,
}

impl AudioLevelStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publica un nivel de audio ya saturado a `0.0..=1.0`.
    pub fn store(&self, level: AudioLevel) {
        self.bits.store(level.to_bits(), Ordering::Relaxed);
    }

    /// Lee el último nivel de audio publicado.
    pub fn load(&self) -> AudioLevel {
        f32::from_bits(self.bits.load(Ordering::Relaxed))
    }
}

/// Bus de eventos no bloqueante para el hilo de audio y el de UI.
pub struct EventBus {
    audio: AudioLevelStore,
    state_tx: Sender<StateTransition>,
    state_rx: Receiver<StateTransition>,
}

impl EventBus {
    pub fn new() -> Self {
        let (state_tx, state_rx) = bounded(STATE_CAPACITY);
        Self {
            audio: AudioLevelStore::new(),
            state_tx,
            state_rx,
        }
    }

    /// Último nivel de audio publicado.
    pub fn audio_level(&self) -> AudioLevel {
        self.audio.load()
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
    /// No bloquea y no puede descartarse: sobreescribe el valor anterior.
    pub fn publish_audio_level(&self, level: AudioLevel) {
        let clamped = level.clamp(0.0, 1.0);
        self.audio.store(clamped);
    }

    /// Publica una transición de estado.
    ///
    /// No bloquea: si el consumidor va lento, la transición se descarta
    /// (evento de telemetría, no de control).
    pub fn publish_state_transition(&self, from: JarvisState, to: JarvisState) {
        let _ = self.state_tx.try_send(StateTransition { from, to });
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_level_store_defaults_to_zero() {
        assert_eq!(AudioLevelStore::new().load(), 0.0);
        assert_eq!(AudioLevelStore::default().load(), 0.0);
    }

    #[test]
    fn audio_level_store_last_value_wins() {
        let store = AudioLevelStore::new();
        for level in [0.1, 0.9, 0.25, 0.5, 1.0, 0.0, 0.77] {
            store.store(level);
        }
        assert_eq!(store.load(), 0.77);
    }

    #[test]
    fn publish_audio_level_is_clamped() {
        let bus = EventBus::new();
        bus.publish_audio_level(2.0);
        assert_eq!(bus.audio_level(), 1.0);
        bus.publish_audio_level(-0.5);
        assert_eq!(bus.audio_level(), 0.0);
        bus.publish_audio_level(0.42);
        assert_eq!(bus.audio_level(), 0.42);
    }

    #[test]
    fn state_transitions_flow_through_channel() {
        let bus = EventBus::new();
        bus.publish_state_transition(JarvisState::Idle, JarvisState::Listening);
        let received = bus.state_receiver().recv().unwrap();
        assert_eq!(received.from, JarvisState::Idle);
        assert_eq!(received.to, JarvisState::Listening);
    }
}
