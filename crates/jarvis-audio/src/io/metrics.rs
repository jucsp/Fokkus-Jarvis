//! Métricas de solo lectura del motor de audio (CA-1.1).
//!
//! Los callbacks de cpal solo pueden tocar atómicos; aquí se agrupan los
//! contadores que `--check` y la esfera consultan sin locks.

use std::sync::atomic::{AtomicU64, Ordering};

/// Contadores compartidos del motor de audio.
///
/// `Default` deja todo a cero. Los callbacks incrementan con `Relaxed` (no
/// necesitan sincronizar memoria entre hilos, solo acumular).
#[derive(Debug, Default)]
pub struct AudioMetrics {
    /// *Xruns* reportados por cpal (underrun/overrun).
    xruns: AtomicU64,
    /// Muestras de captura perdidas por llenado del ring buffer.
    input_dropped: AtomicU64,
    /// Muestras de la referencia de AEC perdidas por llenado del ring buffer.
    render_dropped: AtomicU64,
    /// Desconexiones/cambios de dispositivo reportados por cpal.
    disconnects: AtomicU64,
    /// Reconexiones exitosas del supervisor.
    reconnects: AtomicU64,
}

/// Fotografía inmutable de los contadores.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioMetricsSnapshot {
    pub xruns: u64,
    pub input_dropped: u64,
    pub render_dropped: u64,
    pub disconnects: u64,
    pub reconnects: u64,
}

impl AudioMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_xrun(&self) {
        self.xruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_input_dropped(&self, n: u64) {
        self.input_dropped.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_render_dropped(&self, n: u64) {
        self.render_dropped.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_disconnect(&self) {
        self.disconnects.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> AudioMetricsSnapshot {
        AudioMetricsSnapshot {
            xruns: self.xruns.load(Ordering::Relaxed),
            input_dropped: self.input_dropped.load(Ordering::Relaxed),
            render_dropped: self.render_dropped.load(Ordering::Relaxed),
            disconnects: self.disconnects.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_accumulate_and_snapshot() {
        let m = AudioMetrics::new();
        m.add_xrun();
        m.add_xrun();
        m.add_input_dropped(10);
        m.add_render_dropped(3);
        m.add_disconnect();
        m.add_reconnect();

        let s = m.snapshot();
        assert_eq!(s.xruns, 2);
        assert_eq!(s.input_dropped, 10);
        assert_eq!(s.render_dropped, 3);
        assert_eq!(s.disconnects, 1);
        assert_eq!(s.reconnects, 1);
    }
}
