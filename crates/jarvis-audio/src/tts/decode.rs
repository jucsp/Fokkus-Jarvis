//! Decodificación MP3 a PCM en memoria con symphonia (HU-8).

use std::io::Cursor;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use super::TtsError;

/// Buffer de audio PCM decodificado.
#[derive(Debug, Clone, PartialEq)]
pub struct PcmAudio {
    /// Muestras de audio interleaved f32 en [-1.0, 1.0].
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

impl PcmAudio {
    pub fn new(samples: Vec<f32>, sample_rate: u32, channels: u16) -> Self {
        Self {
            samples,
            sample_rate,
            channels,
        }
    }

    pub fn duration_seconds(&self) -> f32 {
        if self.sample_rate == 0 || self.channels == 0 {
            0.0
        } else {
            self.samples.len() as f32 / (self.sample_rate as f32 * self.channels as f32)
        }
    }
}

/// Decodifica un buffer de bytes MP3 en memoria a PCM f32.
pub fn decode_mp3(bytes: &[u8]) -> Result<PcmAudio, TtsError> {
    if bytes.is_empty() {
        return Err(TtsError::Decode("buffer de audio vacío".to_string()));
    }

    let src = Box::new(Cursor::new(bytes.to_vec()));
    let mss = MediaSourceStream::new(src, Default::default());

    let mut hint = Hint::new();
    hint.with_extension("mp3");

    let meta_opts: MetadataOptions = Default::default();
    let fmt_opts: FormatOptions = Default::default();

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, fmt_opts, meta_opts)
        .map_err(|e| TtsError::Decode(format!("falló probe MP3: {e}")))?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| TtsError::Decode("sin pista de audio en flujo MP3".to_string()))?;

    let codec_params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| TtsError::Decode("parámetros de códec faltantes".to_string()))?;

    let dec_opts: AudioDecoderOptions = Default::default();
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(codec_params, &dec_opts)
        .map_err(|e| TtsError::Decode(format!("falló crear decodificador: {e}")))?;

    let track_id = track.id;
    let sample_rate = codec_params.sample_rate.unwrap_or(24_000);
    let channels = codec_params
        .channels
        .as_ref()
        .map(|c| c.count() as u16)
        .unwrap_or(1);

    let mut samples = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => break,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(e) => return Err(TtsError::Decode(format!("error leyendo paquete: {e}"))),
        };

        if packet.track_id != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                decoded.copy_to_vec_interleaved::<f32>(&mut samples);
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(e) => return Err(TtsError::Decode(format!("error decodificando: {e}"))),
        }
    }

    Ok(PcmAudio {
        samples,
        sample_rate,
        channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_empty_returns_error() {
        assert!(matches!(decode_mp3(&[]), Err(TtsError::Decode(_))));
    }

    #[test]
    fn decode_corrupt_bytes_returns_error_without_panic() {
        assert!(matches!(
            decode_mp3(&[1, 2, 3, 4, 5]),
            Err(TtsError::Decode(_))
        ));
    }
}
