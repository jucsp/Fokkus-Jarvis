//! Alineación temporal entre la referencia de salida y la captura (AEC).
//!
//! `sonora` define el retardo como
//! `delay = (t_render − t_analyze) + (t_process − t_capture)`:
//! - `t_analyze`: cuando la referencia entra a `process_render_f32`.
//! - `t_render`: cuando esa misma muestra sale por el altavoz.
//! - `t_capture`: cuando el micrófono captó una muestra.
//! - `t_process`: cuando esa muestra entra a `process_capture_f32`.
//!
//! El hilo DSP vacía **ambas** colas en cada vuelta (referencia primero), así
//! que la referencia más nueva y la captura más nueva se procesan casi a la
//! vez. Con eso los términos se reducen a:
//! - `t_render − t_analyze` = latencia de salida de cpal
//!   (`OutputCallbackInfo::timestamp().playback − callback`): la referencia se
//!   publica en el callback, pero suena `playback − callback` después.
//! - `t_process − t_capture` = latencia de entrada de cpal
//!   (`InputCallbackInfo::timestamp().callback − capture`).
//! - Los filtros de remuestreo agregan su propio retardo: el de la captura
//!   suma (la voz llega más tarde) y el de la referencia resta.
//!
//! El resultado es una pista inicial: AEC3 afina el retardo con su estimador
//! interno.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::DSP_RATE;

/// Latencias publicadas por los callbacks de cpal (solo atómicos, sin locks).
#[derive(Debug, Default)]
pub struct LatencyProbe {
    output_us: AtomicU64,
    input_us: AtomicU64,
}

impl LatencyProbe {
    pub fn new() -> Self {
        Self::default()
    }

    /// Callback de salida: `playback − callback`.
    pub fn publish_output(&self, latency: Duration) {
        self.output_us
            .store(duration_to_us(latency), Ordering::Relaxed);
    }

    /// Callback de entrada: `callback − capture`.
    pub fn publish_input(&self, latency: Duration) {
        self.input_us
            .store(duration_to_us(latency), Ordering::Relaxed);
    }

    pub fn output_latency(&self) -> Duration {
        Duration::from_micros(self.output_us.load(Ordering::Relaxed))
    }

    pub fn input_latency(&self) -> Duration {
        Duration::from_micros(self.input_us.load(Ordering::Relaxed))
    }
}

fn duration_to_us(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// Términos del retardo, documentados en el encabezado del módulo.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DelayTerms {
    pub output_latency: Duration,
    pub input_latency: Duration,
    /// Retardo del remuestreador de captura, en muestras a 16 kHz.
    pub capture_filter_samples: usize,
    /// Retardo del remuestreador de referencia, en muestras a 16 kHz.
    pub render_filter_samples: usize,
}

/// Retardo para `set_stream_delay_ms`, recortado al rango de AEC3 [0, 500].
pub fn stream_delay_ms(terms: &DelayTerms) -> i32 {
    let samples_ms = |n: usize| n as f64 * 1000.0 / f64::from(DSP_RATE);
    let ms = terms.output_latency.as_secs_f64() * 1000.0
        + terms.input_latency.as_secs_f64() * 1000.0
        + samples_ms(terms.capture_filter_samples)
        - samples_ms(terms.render_filter_samples);
    ms.round().clamp(0.0, 500.0) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_latencies_and_filter_delays() {
        let terms = DelayTerms {
            output_latency: Duration::from_millis(40),
            input_latency: Duration::from_millis(20),
            capture_filter_samples: 160, // 10 ms
            render_filter_samples: 80,   // 5 ms
        };
        assert_eq!(stream_delay_ms(&terms), 65);
    }

    #[test]
    fn clamps_to_aec_range() {
        let huge = DelayTerms {
            output_latency: Duration::from_secs(2),
            ..DelayTerms::default()
        };
        assert_eq!(stream_delay_ms(&huge), 500);
        let negative = DelayTerms {
            render_filter_samples: 1_600,
            ..DelayTerms::default()
        };
        assert_eq!(stream_delay_ms(&negative), 0);
    }

    #[test]
    fn probe_round_trips_latencies() {
        let probe = LatencyProbe::new();
        probe.publish_output(Duration::from_micros(12_345));
        probe.publish_input(Duration::from_millis(7));
        assert_eq!(probe.output_latency(), Duration::from_micros(12_345));
        assert_eq!(probe.input_latency(), Duration::from_millis(7));
    }
}
