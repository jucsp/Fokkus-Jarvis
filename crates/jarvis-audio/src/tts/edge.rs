//! Síntesis nativa en memoria con msedge-tts (HU-8).

use msedge_tts::tts::client::connect;
use msedge_tts::tts::SpeechConfig;

use super::decode::{decode_mp3, PcmAudio};
use super::TtsError;

/// Formato de audio solicitado a Edge: MP3 24 kHz mono 48 kbps.
pub const EDGE_AUDIO_FORMAT: &str = "audio-24khz-48kbitrate-mono-mp3";

/// Sintetiza un texto usando Edge-TTS directamente en memoria (sin CLI de Python).
pub fn synthesize_edge_native(text: &str, voice_name: &str) -> Result<PcmAudio, TtsError> {
    let mut client =
        connect().map_err(|e| TtsError::Network(format!("falló conectar con Edge-TTS: {e}")))?;
    let config = SpeechConfig {
        voice_name: voice_name.to_string(),
        audio_format: EDGE_AUDIO_FORMAT.to_string(),
        pitch: 0,
        rate: 0,
        volume: 0,
    };
    let audio = client
        .synthesize(text, &config)
        .map_err(|e| TtsError::Network(format!("falló síntesis con Edge-TTS: {e}")))?;

    decode_mp3(&audio.audio_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_constants_are_valid() {
        assert_eq!(EDGE_AUDIO_FORMAT, "audio-24khz-48kbitrate-mono-mp3");
    }
}
