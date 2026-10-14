//! Control de reproducción de audio con parada instantánea (*barge-in*).
//!
//! `AudioPlayback` no genera audio por sí mismo: expone la señal de parada y un
//! contador de generación para que el callback de salida (cpal, tarea 2.2)
//! decida en tiempo real si debe silenciar la cola. El patrón está alineado con
//! el dictamen §4.2: `AtomicBool` + rampa de 15 ms, sin locks en el hilo de
//! audio.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Default)]
struct PlaybackInner {
    stop_requested: AtomicBool,
    generation: AtomicU64,
}

/// Señal de reproducción/cancelación compartida y clonable.
#[derive(Debug, Clone)]
pub struct AudioPlayback {
    inner: Arc<PlaybackInner>,
}

impl AudioPlayback {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(PlaybackInner::default()),
        }
    }

    /// Inicia una nueva locución: limpia la parada previa y devuelve la
    /// generación actual, que el productor debe adjuntar a sus segmentos.
    pub fn begin_utterance(&self) -> u64 {
        self.inner.stop_requested.store(false, Ordering::Relaxed);
        self.inner.generation.load(Ordering::Acquire)
    }

    /// Parada instantánea: marca la señal e invalida la generación activa.
    pub fn stop(&self) {
        self.inner.stop_requested.store(true, Ordering::Relaxed);
        self.inner.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// `true` si se ha solicitado parada desde el último `begin_utterance`.
    pub fn is_stop_requested(&self) -> bool {
        self.inner.stop_requested.load(Ordering::Relaxed)
    }

    /// Generación actual (se incrementa en cada `stop`).
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    /// `true` si una generación capturada con `begin_utterance` ya fue cancelada.
    pub fn is_stale(&self, generation: u64) -> bool {
        generation != self.generation()
    }
}

impl Default for AudioPlayback {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_playback_is_not_stopped() {
        let pb = AudioPlayback::new();
        assert!(!pb.is_stop_requested());
        assert_eq!(pb.generation(), 0);
    }

    #[test]
    fn begin_utterance_clears_stop_and_returns_generation() {
        let pb = AudioPlayback::new();
        pb.stop();
        assert!(pb.is_stop_requested());
        let gen = pb.begin_utterance();
        assert!(!pb.is_stop_requested());
        assert_eq!(gen, pb.generation());
    }

    #[test]
    fn stop_bumps_generation_and_flags() {
        let pb = AudioPlayback::new();
        let gen = pb.begin_utterance();
        assert!(!pb.is_stale(gen));
        pb.stop();
        assert!(pb.is_stop_requested());
        assert!(pb.is_stale(gen));
        assert_eq!(pb.generation(), gen + 1);
    }

    #[test]
    fn clone_shares_control_state() {
        let a = AudioPlayback::new();
        let b = a.clone();
        let gen = b.begin_utterance();
        a.stop();
        assert!(b.is_stop_requested());
        assert!(b.is_stale(gen));
    }
}
