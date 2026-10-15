//! Procesador de señal de voz: AEC3 + supresión de ruido + AGC2 (HU-2).
//!
//! [`EchoProcessor`] es el *trait* interno que aísla el motor: hoy lo
//! implementa [`SonoraProcessor`] (`sonora` 0.2, Rust puro); el plan B de
//! CA-2.3 (`webrtc-audio-processing` del sistema) se enchufaría con otra
//! implementación sin tocar el resto de la cadena.

use sonora::config::{AdaptiveDigital, EchoCanceller, GainController2, NoiseSuppression};
use sonora::{AudioProcessing, Config, StreamConfig};

use super::{DspError, DSP_FRAME, DSP_RATE};

/// Motor de cancelación de eco y limpieza de voz a 16 kHz mono, 10 ms.
pub trait EchoProcessor: Send {
    /// Informa un frame de 160 muestras de la referencia (lo que sonó).
    fn process_render(&mut self, frame: &[f32]) -> Result<(), DspError>;

    /// Limpia un frame de 160 muestras del micrófono hacia `output`.
    fn process_capture(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), DspError>;

    /// Retardo estimado entre referencia y captura, en milisegundos.
    fn set_stream_delay_ms(&mut self, delay_ms: i32);

    /// ERLE (*Echo Return Loss Enhancement*) en dB, si el motor lo calcula.
    fn erle_db(&self) -> Option<f64>;
}

/// Etapas activas del procesador.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessorConfig {
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    pub gain_control: bool,
}

impl Default for ProcessorConfig {
    /// AEC3 + NS + AGC2, como exige el contrato.
    fn default() -> Self {
        Self {
            echo_cancellation: true,
            noise_suppression: true,
            gain_control: true,
        }
    }
}

/// Implementación con `sonora`.
pub struct SonoraProcessor {
    apm: AudioProcessing,
    /// Destino obligatorio de `process_render_f32` (la referencia no cambia).
    render_out: [f32; DSP_FRAME],
}

impl SonoraProcessor {
    pub fn new(config: ProcessorConfig) -> Self {
        let apm_config = Config {
            echo_canceller: config.echo_cancellation.then(EchoCanceller::default),
            noise_suppression: config.noise_suppression.then(NoiseSuppression::default),
            gain_controller2: config.gain_control.then(|| GainController2 {
                adaptive_digital: Some(AdaptiveDigital::default()),
                ..GainController2::default()
            }),
            ..Config::default()
        };
        let stream = StreamConfig::new(DSP_RATE, 1);
        Self {
            apm: AudioProcessing::builder()
                .config(apm_config)
                .capture_config(stream)
                .render_config(stream)
                .build(),
            render_out: [0.0; DSP_FRAME],
        }
    }
}

impl EchoProcessor for SonoraProcessor {
    fn process_render(&mut self, frame: &[f32]) -> Result<(), DspError> {
        if frame.len() != DSP_FRAME {
            return Err(DspError::FrameLength(frame.len()));
        }
        self.apm
            .process_render_f32(&[frame], &mut [&mut self.render_out[..]])
            .map_err(DspError::Processor)
    }

    fn process_capture(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), DspError> {
        if input.len() != DSP_FRAME || output.len() != DSP_FRAME {
            return Err(DspError::FrameLength(input.len().max(output.len())));
        }
        self.apm
            .process_capture_f32(&[input], &mut [output])
            .map_err(DspError::Processor)
    }

    fn set_stream_delay_ms(&mut self, delay_ms: i32) {
        // `Err(StreamParameterClamped)` solo avisa que se recortó a [0, 500];
        // el valor recortado se aplica igual.
        let _ = self.apm.set_stream_delay_ms(delay_ms);
    }

    fn erle_db(&self) -> Option<f64> {
        self.apm.statistics().echo_return_loss_enhancement
    }
}
