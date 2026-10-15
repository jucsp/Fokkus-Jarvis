//! Motor de síntesis offline Piper y fallback a espeak-ng (HU-8).

use std::process::Command;

use super::decode::PcmAudio;
use super::TtsError;

/// Fallback local ejecutando espeak-ng para generar WAV en memoria y retornar PCM f32.
pub fn synthesize_local_fallback(text: &str, voice_lang: &str) -> Result<PcmAudio, TtsError> {
    let lang = voice_lang.split('-').next().unwrap_or("es");
    let temp_wav = std::env::temp_dir().join(format!(
        "jarvis-fallback-{}-{}.wav",
        std::process::id(),
        rand_suffix()
    ));

    let status = Command::new("espeak-ng")
        .arg("-v")
        .arg(lang)
        .arg("-w")
        .arg(&temp_wav)
        .arg(text)
        .status()
        .map_err(|e| TtsError::Command {
            backend: "espeak-ng",
            source: e,
        })?;

    if !status.success() {
        let _ = std::fs::remove_file(&temp_wav);
        return Err(TtsError::CommandFailed {
            backend: "espeak-ng",
            status,
        });
    }

    let bytes = std::fs::read(&temp_wav).map_err(|e| TtsError::Io {
        path: temp_wav.clone(),
        source: e,
    })?;
    let _ = std::fs::remove_file(&temp_wav);

    parse_wav_pcm(&bytes)
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// Parsea un buffer WAV PCM estándar y lo convierte a PcmAudio.
pub fn parse_wav_pcm(bytes: &[u8]) -> Result<PcmAudio, TtsError> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(TtsError::Decode("cabecera WAV inválida".into()));
    }
    let channels = u16::from_le_bytes([bytes[22], bytes[23]]);
    let sample_rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
    let bits_per_sample = u16::from_le_bytes([bytes[34], bytes[35]]);

    let mut offset = 36;
    while offset + 8 <= bytes.len() {
        if &bytes[offset..offset + 4] == b"data" {
            let data_len = u32::from_le_bytes([
                bytes[offset + 4],
                bytes[offset + 5],
                bytes[offset + 6],
                bytes[offset + 7],
            ]) as usize;
            let data_start = offset + 8;
            let data_end = (data_start + data_len).min(bytes.len());
            let data = &bytes[data_start..data_end];

            let mut samples = Vec::new();
            if bits_per_sample == 16 {
                #[allow(clippy::chunks_exact_to_as_chunks)]
                for chunk in data.chunks_exact(2) {
                    let s = i16::from_le_bytes([chunk[0], chunk[1]]);
                    samples.push(s as f32 / 32768.0);
                }
            } else if bits_per_sample == 8 {
                for &b in data {
                    samples.push((b as f32 - 128.0) / 128.0);
                }
            } else {
                return Err(TtsError::Decode(format!(
                    "formato WAV con {bits_per_sample} bits no soportado"
                )));
            }
            return Ok(PcmAudio {
                samples,
                sample_rate,
                channels,
            });
        }
        offset += 1;
    }

    Err(TtsError::Decode(
        "no se encontró chunk de datos en WAV".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_wav_rejects_empty_or_invalid() {
        assert!(matches!(parse_wav_pcm(&[]), Err(TtsError::Decode(_))));
        assert!(matches!(
            parse_wav_pcm(b"NOT_A_WAV_FILE_HEADER"),
            Err(TtsError::Decode(_))
        ));
    }

    #[test]
    fn parse_wav_accepts_valid_minimal_pcm() {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&36u32.to_le_bytes()); // chunk size
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes()); // subchunk1 size
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // 1 channel
        wav.extend_from_slice(&16000u32.to_le_bytes()); // 16kHz
        wav.extend_from_slice(&32000u32.to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&4u32.to_le_bytes()); // 4 bytes of data = 2 samples
        wav.extend_from_slice(&0i16.to_le_bytes());
        wav.extend_from_slice(&16384i16.to_le_bytes());

        let pcm = parse_wav_pcm(&wav).unwrap();
        assert_eq!(pcm.channels, 1);
        assert_eq!(pcm.sample_rate, 16000);
        assert_eq!(pcm.samples.len(), 2);
        assert_eq!(pcm.samples[0], 0.0);
        assert!((pcm.samples[1] - 0.5).abs() < 0.01);
    }
}
