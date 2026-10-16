//! Entrada/salida de audio nativa con `cpal` (HU-1).
//!
//! - [`callbacks`]: funciones puras que ejecutan los callbacks de tiempo real
//!   (sin locks ni asignaciones, CA-1.3).
//! - [`device`]: selección de dispositivos y negociación de formato.
//! - [`metrics`]: contadores atómicos de solo lectura (xruns, pérdidas).

pub mod callbacks;
pub mod device;
pub mod engine;
pub mod metrics;
pub mod ports;

use std::fmt;

pub use callbacks::{
    compute_rms, mix_down_to_mono, process_input_frame, process_output_frame, sanitize_sample,
    sanitize_slice, Fader, OutputFrameStats,
};
pub use device::{
    negotiate_input, negotiate_output, select_input_device, select_output_device, Negotiated,
};
pub use engine::{
    device_label, to_output_pcm, AudioBackend, AudioEngine, CpalBackend, EngineConfig,
    EngineShared, EngineStatus, OpenStream, PlayOutcome, Player, StreamFormat, StreamInfo,
};
pub use metrics::{AudioMetrics, AudioMetricsSnapshot};
pub use ports::{InputPort, LevelMeter, OutputPort, PortShared, StreamFault};

/// Errores del motor de E/S de audio.
#[derive(Debug)]
pub enum AudioError {
    /// Error devuelto por el backend de `cpal`.
    Backend(cpal::Error),
    /// No hay dispositivo de entrada por defecto.
    NoInputDevice,
    /// No hay dispositivo de salida por defecto.
    NoOutputDevice,
    /// Ningún dispositivo de entrada coincide con el nombre configurado.
    NoSuchInputDevice(String),
    /// Ningún dispositivo de salida coincide con el nombre configurado.
    NoSuchOutputDevice(String),
    /// El dispositivo no ofrece ninguna configuración utilizable.
    NoSupportedConfig,
    /// El dispositivo solo ofrece un formato de muestra no soportado.
    UnsupportedFormat(cpal::SampleFormat),
    /// No se pudo crear el hilo supervisor.
    Spawn(std::io::Error),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(e) => write!(f, "error del backend de audio: {e}"),
            Self::NoInputDevice => write!(f, "no hay dispositivo de entrada por defecto"),
            Self::NoOutputDevice => write!(f, "no hay dispositivo de salida por defecto"),
            Self::NoSuchInputDevice(name) => {
                write!(f, "no existe un dispositivo de entrada llamado «{name}»")
            }
            Self::NoSuchOutputDevice(name) => {
                write!(f, "no existe un dispositivo de salida llamado «{name}»")
            }
            Self::NoSupportedConfig => {
                write!(f, "el dispositivo no ofrece una configuración soportada")
            }
            Self::UnsupportedFormat(format) => {
                write!(f, "formato de muestra no soportado: {format}")
            }
            Self::Spawn(e) => write!(f, "no se pudo crear el hilo de audio: {e}"),
        }
    }
}

impl std::error::Error for AudioError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Backend(e) => Some(e),
            Self::Spawn(e) => Some(e),
            _ => None,
        }
    }
}
