//! Síntesis nativa en memoria con msedge-tts (HU-8), con prosodia por frase
//! (HU-46).

use msedge_tts::tts::client::connect;
use msedge_tts::tts::SpeechConfig;

use super::decode::{decode_mp3, PcmAudio};
use super::directive::SpeechDirective;
use super::TtsError;

/// Formato de audio solicitado a Edge: MP3 24 kHz mono 48 kbps.
pub const EDGE_AUDIO_FORMAT: &str = "audio-24khz-48kbitrate-mono-mp3";

/// Escapa el texto para el SSML: `msedge-tts` lo inserta tal cual dentro de
/// `<prosody>`, así que un `&` o un `<` del LLM rompería el XML y la frase
/// degradaría a `espeak-ng`.
pub fn escape_ssml_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Sintetiza un texto usando Edge-TTS directamente en memoria (sin CLI de Python).
pub fn synthesize_edge_native(text: &str, voice_name: &str) -> Result<PcmAudio, TtsError> {
    synthesize_edge_with(text, voice_name, &SpeechDirective::default())
}

/// Configuración de Edge para una frase: la directiva se traduce al
/// `<prosody>` que arma `msedge-tts` (`rate`/`volume` en %, `pitch` en Hz).
pub fn edge_speech_config(voice_name: &str, directive: &SpeechDirective) -> SpeechConfig {
    SpeechConfig {
        voice_name: voice_name.to_string(),
        audio_format: EDGE_AUDIO_FORMAT.to_string(),
        pitch: i32::from(directive.pitch),
        rate: i32::from(directive.rate),
        volume: i32::from(directive.volume),
    }
}

/// Sintetiza una frase con la prosodia de su directiva. Las pausas y la
/// respiración no van al servidor: las añade el motor sobre el PCM.
pub fn synthesize_edge_with(
    text: &str,
    voice_name: &str,
    directive: &SpeechDirective,
) -> Result<PcmAudio, TtsError> {
    let mut client =
        connect().map_err(|e| TtsError::Network(format!("falló conectar con Edge-TTS: {e}")))?;
    let config = edge_speech_config(voice_name, directive);
    let audio = client
        .synthesize(&escape_ssml_text(text), &config)
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

    #[test]
    fn ssml_text_is_escaped() {
        assert_eq!(escape_ssml_text("A & B < C > D"), "A &amp; B &lt; C &gt; D");
        assert_eq!(escape_ssml_text("Hola, ¿qué tal?"), "Hola, ¿qué tal?");
    }

    #[test]
    fn speech_config_carries_directive_prosody() {
        let neutral = edge_speech_config("es-CL-LorenzoNeural", &SpeechDirective::default());
        assert_eq!((neutral.rate, neutral.pitch, neutral.volume), (0, 0, 0));
        assert_eq!(neutral.voice_name, "es-CL-LorenzoNeural");
        assert_eq!(neutral.audio_format, EDGE_AUDIO_FORMAT);

        let directive = SpeechDirective {
            rate: 8,
            pitch: -2,
            volume: 5,
            ..Default::default()
        };
        let config = edge_speech_config("es-CL-LorenzoNeural", &directive);
        assert_eq!((config.rate, config.pitch, config.volume), (8, -2, 5));
    }
}
