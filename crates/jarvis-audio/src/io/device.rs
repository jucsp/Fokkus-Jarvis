//! Selección y negociación de dispositivos de audio con `cpal` (HU-1).
//!
//! Preferimos `f32` a 48 kHz (o 44,1 kHz) porque es el formato nativo de la
//! cadena DSP; si el dispositivo no ofrece `f32`, se acepta `i16` y se convierte
//! en el callback sin romper el contrato de tiempo real. La selección por nombre
//! usa `DeviceTrait::description()` (cpal 0.18 eliminó `name()`).

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, Device, SampleFormat, StreamConfig, SupportedStreamConfig};

use super::AudioError;

/// Selecciona el dispositivo de entrada: por nombre (subcadena, sin mayúsculas)
/// o el dispositivo por defecto.
pub fn select_input_device(host: &cpal::Host, name: Option<&str>) -> Result<Device, AudioError> {
    match name {
        Some(wanted) => host
            .input_devices()
            .map_err(AudioError::Backend)?
            .find(|d| device_matches(d, wanted))
            .ok_or(AudioError::NoSuchInputDevice(wanted.to_string())),
        None => host.default_input_device().ok_or(AudioError::NoInputDevice),
    }
}

/// Selecciona el dispositivo de salida: por nombre o el dispositivo por defecto.
pub fn select_output_device(host: &cpal::Host, name: Option<&str>) -> Result<Device, AudioError> {
    match name {
        Some(wanted) => host
            .output_devices()
            .map_err(AudioError::Backend)?
            .find(|d| device_matches(d, wanted))
            .ok_or(AudioError::NoSuchOutputDevice(wanted.to_string())),
        None => host
            .default_output_device()
            .ok_or(AudioError::NoOutputDevice),
    }
}

fn device_matches(device: &Device, wanted: &str) -> bool {
    let needle = wanted.to_ascii_lowercase();
    if needle.is_empty() {
        return false;
    }
    let name = device
        .description()
        .map(|d| d.name().to_ascii_lowercase())
        .unwrap_or_default();
    if name.contains(&needle) {
        return true;
    }
    // `Display` del dispositivo como respaldo (p. ej. "ALSA ...").
    device.to_string().to_ascii_lowercase().contains(&needle)
}

/// Resultado de la negociación: configuración del stream + formato de muestras.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    pub config: StreamConfig,
    pub format: SampleFormat,
}

/// Negocia la configuración de entrada preferida (`f32` → 48 kHz → 44,1 kHz).
pub fn negotiate_input(device: &Device) -> Result<Negotiated, AudioError> {
    let ranges: Vec<_> = device
        .supported_input_configs()
        .map_err(AudioError::Backend)?
        .collect();
    negotiate(&ranges, device.default_input_config().ok())
}

/// Negocia la configuración de salida preferida (`f32` → 48 kHz → 44,1 kHz).
pub fn negotiate_output(device: &Device) -> Result<Negotiated, AudioError> {
    let ranges: Vec<_> = device
        .supported_output_configs()
        .map_err(AudioError::Backend)?
        .collect();
    negotiate(&ranges, device.default_output_config().ok())
}

/// Elige la mejor configuración soportada.
///
/// Prioridad: `f32` 48 kHz → `f32` 44,1 kHz → `f32` máxima tasa → `i16` 48 kHz
/// → `i16` 44,1 kHz → `i16` máxima → la configuración por defecto del
/// dispositivo. Devuelve error si no hay ninguna.
fn negotiate(
    ranges: &[cpal::SupportedStreamConfigRange],
    fallback: Option<SupportedStreamConfig>,
) -> Result<Negotiated, AudioError> {
    for format in [SampleFormat::F32, SampleFormat::I16] {
        let same_format: Vec<_> = ranges
            .iter()
            .copied()
            .filter(|r| r.sample_format() == format)
            .collect();

        // 1. Tasa estándar (48 kHz, si no 44,1 kHz).
        if let Some(conf) = same_format
            .iter()
            .find_map(|r| r.try_with_standard_sample_rate())
        {
            return Ok(Negotiated {
                config: stream_config(&conf),
                format,
            });
        }
        // 2. Máxima tasa disponible en este formato.
        if let Some(conf) = same_format
            .iter()
            .copied()
            .max_by_key(|r| r.max_sample_rate())
            .map(|r| r.with_max_sample_rate())
        {
            return Ok(Negotiated {
                config: stream_config(&conf),
                format,
            });
        }
    }

    // 3. Configuración por defecto del dispositivo (puede tener otro formato).
    if let Some(conf) = fallback {
        return Ok(Negotiated {
            config: conf.config(),
            format: conf.sample_format(),
        });
    }

    Err(AudioError::NoSupportedConfig)
}

fn stream_config(conf: &SupportedStreamConfig) -> StreamConfig {
    StreamConfig {
        channels: conf.channels(),
        sample_rate: conf.sample_rate(),
        buffer_size: BufferSize::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_f32_48k_over_i16() {
        let ranges = vec![
            cpal::SupportedStreamConfigRange::new(
                2,
                44_100,
                48_000,
                cpal::SupportedBufferSize::Unknown,
                SampleFormat::I16,
            ),
            cpal::SupportedStreamConfigRange::new(
                2,
                44_100,
                48_000,
                cpal::SupportedBufferSize::Unknown,
                SampleFormat::F32,
            ),
        ];
        let negotiated = negotiate(&ranges, None).expect("debe negociar");
        assert_eq!(negotiated.format, SampleFormat::F32);
        assert_eq!(negotiated.config.sample_rate, 48_000);
        assert_eq!(negotiated.config.channels, 2);
    }

    #[test]
    fn falls_back_to_i16_when_no_f32() {
        let ranges = vec![cpal::SupportedStreamConfigRange::new(
            1,
            8_000,
            48_000,
            cpal::SupportedBufferSize::Unknown,
            SampleFormat::I16,
        )];
        let negotiated = negotiate(&ranges, None).expect("debe negociar");
        assert_eq!(negotiated.format, SampleFormat::I16);
        assert_eq!(negotiated.config.sample_rate, 48_000);
    }

    #[test]
    fn errors_without_any_supported_config() {
        assert!(matches!(
            negotiate(&[], None),
            Err(AudioError::NoSupportedConfig)
        ));
    }
}
