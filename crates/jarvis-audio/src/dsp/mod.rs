//! Cadena DSP del hilo `jarvis-audio-dsp` (HU-2).
//!
//! ```text
//! captura (tasa del disp.) ─► MonoResampler ─► Reframer(160) ─► AEC3+NS+AGC2 ─► Reframer(256) ─► VAD
//! referencia (tasa salida) ─► MonoResampler ─► Reframer(160) ─► process_render ─┘
//! ```
//!
//! - [`resample`]: remuestreo a 16 kHz con `rubato`.
//! - [`reframe`]: acumuladores de largo fijo sin asignaciones por frame.
//! - [`processor`]: *trait* [`EchoProcessor`] e implementación con `sonora`.
//! - [`delay`]: cálculo del retardo referencia → micrófono para AEC3.
//! - [`thread`]: hilo que une la cadena con el VAD y el *endpointer*.

pub mod delay;
pub mod processor;
pub mod reframe;
pub mod resample;
pub mod thread;

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::vad::TurnPauseStats;

pub use delay::{stream_delay_ms, DelayTerms, LatencyProbe};
pub use processor::{EchoProcessor, ProcessorConfig, SonoraProcessor};
pub use reframe::Reframer;
pub use resample::MonoResampler;
pub use thread::{spawn_dsp_thread, ConnectorSignal, DspHandle, DspThreadConfig};

/// Tasa de trabajo de la cadena DSP y del VAD.
pub const DSP_RATE: u32 = 16_000;
/// Frame de `sonora`: 10 ms a 16 kHz.
pub const DSP_FRAME: usize = 160;

/// Errores de la cadena DSP.
#[derive(Debug)]
pub enum DspError {
    /// Tasa de muestreo nula o no soportada.
    InvalidSampleRate(u32),
    /// `rubato` rechazó la configuración del remuestreador.
    ResamplerConstruction(rubato::ResamplerConstructionError),
    /// `rubato` falló al procesar un tramo.
    Resample(rubato::ResampleError),
    /// Buffer de remuestreo con tamaño inconsistente.
    BufferSize,
    /// Frame de largo distinto de 160 muestras.
    FrameLength(usize),
    /// `sonora` devolvió error en un frame.
    Processor(sonora::Error),
    /// No se pudo crear el hilo DSP.
    Spawn(std::io::Error),
}

impl fmt::Display for DspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate(rate) => write!(f, "tasa de muestreo inválida: {rate} Hz"),
            Self::ResamplerConstruction(e) => write!(f, "no se pudo crear el remuestreador: {e}"),
            Self::Resample(e) => write!(f, "error de remuestreo: {e}"),
            Self::BufferSize => write!(f, "buffer de remuestreo con tamaño inconsistente"),
            Self::FrameLength(len) => {
                write!(f, "frame de {len} muestras (se esperaban {DSP_FRAME})")
            }
            Self::Processor(e) => write!(f, "error del procesador de voz: {e:?}"),
            Self::Spawn(e) => write!(f, "no se pudo crear el hilo DSP: {e}"),
        }
    }
}

impl std::error::Error for DspError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ResamplerConstruction(e) => Some(e),
            Self::Resample(e) => Some(e),
            Self::Spawn(e) => Some(e),
            _ => None,
        }
    }
}

/// Métricas de solo lectura de la cadena DSP (para `--check` y la esfera).
///
/// El ERLE y el retardo estimado solo se actualizan con el diagnóstico activo
/// ([`DspMetrics::set_diagnostics`]): viven en memoria, el audio nunca se
/// escribe a disco (contrato HU-10).
#[derive(Debug, Default)]
pub struct DspMetrics {
    capture_frames: AtomicU64,
    render_frames: AtomicU64,
    processor_errors: AtomicU64,
    resampler_errors: AtomicU64,
    stream_delay_ms: AtomicI32,
    /// `f32::to_bits` del último puntaje del VAD.
    vad_score: AtomicU32,
    /// `f64::to_bits` del ERLE; `u64::MAX` = sin dato.
    erle_db: AtomicU64,
    diagnostics: AtomicBool,
    /// Pausas intra-turno de los turnos ya cerrados (HU-41). El hilo DSP lo
    /// bloquea una vez por turno, nunca por frame.
    pauses: Mutex<TurnPauseStats>,
}

/// Fotografía de [`DspMetrics`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DspMetricsSnapshot {
    pub capture_frames: u64,
    pub render_frames: u64,
    pub processor_errors: u64,
    pub resampler_errors: u64,
    pub stream_delay_ms: i32,
    pub vad_score: f32,
    pub erle_db: Option<f64>,
}

impl DspMetrics {
    pub fn new() -> Self {
        let m = Self::default();
        m.erle_db.store(u64::MAX, Ordering::Relaxed);
        m
    }

    /// Activa o desactiva el diagnóstico de ERLE (solo en memoria).
    pub fn set_diagnostics(&self, enabled: bool) {
        self.diagnostics.store(enabled, Ordering::Relaxed);
        if !enabled {
            self.erle_db.store(u64::MAX, Ordering::Relaxed);
        }
    }

    pub fn diagnostics_enabled(&self) -> bool {
        self.diagnostics.load(Ordering::Relaxed)
    }

    pub(crate) fn set_erle(&self, erle: Option<f64>) {
        let bits = erle
            .filter(|v| v.is_finite())
            .map_or(u64::MAX, f64::to_bits);
        self.erle_db.store(bits, Ordering::Relaxed);
    }

    pub(crate) fn set_vad_score(&self, score: f32) {
        self.vad_score.store(score.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn set_stream_delay(&self, ms: i32) {
        self.stream_delay_ms.store(ms, Ordering::Relaxed);
    }

    /// Suma las pausas de un turno recién cerrado al histograma (HU-41).
    pub(crate) fn absorb_pauses(&self, turn: &TurnPauseStats) {
        if turn.recorded() == 0 {
            return;
        }
        self.pauses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .absorb(turn);
    }

    /// Copia del histograma de pausas intra-turno (base de `--diag-turns`).
    pub fn pause_stats(&self) -> TurnPauseStats {
        self.pauses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn snapshot(&self) -> DspMetricsSnapshot {
        let erle = self.erle_db.load(Ordering::Relaxed);
        DspMetricsSnapshot {
            capture_frames: self.capture_frames.load(Ordering::Relaxed),
            render_frames: self.render_frames.load(Ordering::Relaxed),
            processor_errors: self.processor_errors.load(Ordering::Relaxed),
            resampler_errors: self.resampler_errors.load(Ordering::Relaxed),
            stream_delay_ms: self.stream_delay_ms.load(Ordering::Relaxed),
            vad_score: f32::from_bits(self.vad_score.load(Ordering::Relaxed)),
            erle_db: (erle != u64::MAX).then(|| f64::from_bits(erle)),
        }
    }
}

/// Cadena remuestreo → AEC3/NS/AGC2 → re-encuadre a 256, sin hilos.
///
/// Es la pieza que el hilo DSP ejecuta y la que prueban los tests: no asigna
/// por frame y nunca entra en pánico por el contenido de la señal.
pub struct DspPipeline {
    capture_rs: MonoResampler,
    render_rs: MonoResampler,
    capture_10ms: Reframer,
    render_10ms: Reframer,
    vad_frames: Reframer,
    processor: Box<dyn EchoProcessor>,
    clean: [f32; DSP_FRAME],
    metrics: Arc<DspMetrics>,
}

impl DspPipeline {
    /// `vad_frame` es el largo de frame del VAD (256 para `earshot`).
    pub fn new(
        capture_rate: u32,
        render_rate: u32,
        vad_frame: usize,
        processor: Box<dyn EchoProcessor>,
        metrics: Arc<DspMetrics>,
    ) -> Result<Self, DspError> {
        Ok(Self {
            capture_rs: MonoResampler::new(capture_rate, DSP_RATE)?,
            render_rs: MonoResampler::new(render_rate, DSP_RATE)?,
            capture_10ms: Reframer::new(DSP_FRAME),
            render_10ms: Reframer::new(DSP_FRAME),
            vad_frames: Reframer::new(vad_frame),
            processor,
            clean: [0.0; DSP_FRAME],
            metrics,
        })
    }

    /// Términos de retardo propios de la cadena (filtros de remuestreo).
    pub fn delay_terms(&self) -> DelayTerms {
        DelayTerms {
            capture_filter_samples: self.capture_rs.output_delay(),
            render_filter_samples: self.render_rs.output_delay(),
            ..DelayTerms::default()
        }
    }

    pub fn set_stream_delay_ms(&mut self, ms: i32) {
        self.processor.set_stream_delay_ms(ms);
        self.metrics.set_stream_delay(ms);
    }

    /// ERLE actual del procesador (diagnóstico).
    pub fn erle_db(&self) -> Option<f64> {
        self.processor.erle_db()
    }

    /// Alimenta la referencia de salida (a la tasa del dispositivo de salida).
    pub fn push_render(&mut self, samples: &[f32]) {
        let Self {
            render_rs,
            render_10ms,
            processor,
            metrics,
            ..
        } = self;
        let result = render_rs.process(samples, |chunk| {
            render_10ms.push(chunk, |frame| {
                metrics.render_frames.fetch_add(1, Ordering::Relaxed);
                if processor.process_render(frame).is_err() {
                    metrics.processor_errors.fetch_add(1, Ordering::Relaxed);
                }
            });
        });
        if result.is_err() {
            metrics.resampler_errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Alimenta la captura y entrega cada frame limpio de `vad_frame` muestras.
    ///
    /// Si el procesador falla en un frame, se cuenta y el frame pasa sin
    /// procesar: el hilo nunca se cae por un error de `sonora`.
    pub fn push_capture(&mut self, samples: &[f32], mut on_frame: impl FnMut(&mut [f32])) {
        let Self {
            capture_rs,
            capture_10ms,
            vad_frames,
            processor,
            clean,
            metrics,
            ..
        } = self;
        let result = capture_rs.process(samples, |chunk| {
            capture_10ms.push(chunk, |frame| {
                metrics.capture_frames.fetch_add(1, Ordering::Relaxed);
                if processor.process_capture(frame, clean).is_err() {
                    metrics.processor_errors.fetch_add(1, Ordering::Relaxed);
                    clean.copy_from_slice(frame);
                }
                crate::io::callbacks::sanitize_slice(clean);
                vad_frames.push(clean, &mut on_frame);
            });
        });
        if result.is_err() {
            metrics.resampler_errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Descarta el estado acumulado (reconexión de dispositivo).
    pub fn reset(&mut self) {
        self.capture_rs.reset();
        self.render_rs.reset();
        self.capture_10ms.reset();
        self.render_10ms.reset();
        self.vad_frames.reset();
    }
}

#[cfg(test)]
mod tests;
