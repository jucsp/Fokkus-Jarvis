//! Render local de la prosodia (HU-46): silencios PCM y respiración.
//!
//! Edge-TTS resuelve el ritmo, el tono y el volumen en el servidor; las
//! pausas y la inhalación se añaden aquí sobre el PCM decodificado, sin red
//! y para cualquier backend.

use super::decode::PcmAudio;
use super::directive::SpeechDirective;

/// Duración de la inhalación sintética.
pub const BREATH_MS: u32 = 320;
/// Silencio entre la inhalación y la frase.
const BREATH_GAP_MS: u32 = 70;
/// Pico de la inhalación: apenas audible bajo la voz.
const BREATH_PEAK: f32 = 0.035;

/// Muestras interleaved de `ms` milisegundos para el formato dado.
fn samples_for(ms: u32, sample_rate: u32, channels: u16) -> usize {
    (u64::from(sample_rate) * u64::from(ms) / 1_000) as usize * usize::from(channels)
}

/// Inhalación suave: ruido blanco filtrado en banda (~300-2500 Hz) con una
/// envolvente que sube despacio y cae rápido, como al tomar aire.
///
/// Es determinista (misma semilla) para que los tests y la caché sean
/// estables; mono replicado en todos los canales.
pub fn synth_breath(sample_rate: u32, channels: u16) -> Vec<f32> {
    if sample_rate == 0 || channels == 0 {
        return Vec::new();
    }
    let frames = (u64::from(sample_rate) * u64::from(BREATH_MS) / 1_000) as usize;
    let dt = 1.0 / sample_rate as f32;
    let lp_alpha = {
        let rc = 1.0 / (std::f32::consts::TAU * 2_500.0);
        dt / (rc + dt)
    };
    let hp_alpha = {
        let rc = 1.0 / (std::f32::consts::TAU * 300.0);
        rc / (rc + dt)
    };

    let mut seed: u32 = 0x1234_5678;
    let mut lp = 0.0f32;
    let mut hp = 0.0f32;
    let mut prev_lp = 0.0f32;
    let mut raw = Vec::with_capacity(frames);
    for _ in 0..frames {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let noise = (seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
        lp += lp_alpha * (noise - lp);
        hp = hp_alpha * (hp + lp - prev_lp);
        prev_lp = lp;
        raw.push(hp);
    }

    let peak = raw.iter().fold(0.0f32, |m, s| m.max(s.abs())).max(1e-6);
    let mut out = Vec::with_capacity(frames * usize::from(channels));
    for (i, s) in raw.iter().enumerate() {
        let x = i as f32 / frames as f32;
        // Sube durante el 65 % y cae en el resto, sin bordes abruptos.
        let env = if x < 0.65 {
            (x / 0.65 * std::f32::consts::FRAC_PI_2).sin().powi(2)
        } else {
            ((1.0 - x) / 0.35 * std::f32::consts::FRAC_PI_2)
                .sin()
                .powi(2)
        };
        let v = s / peak * BREATH_PEAK * env;
        for _ in 0..channels {
            out.push(v);
        }
    }
    out
}

/// Aplica las pausas y la respiración de la directiva al PCM de una frase.
pub fn apply_directive_pcm(pcm: PcmAudio, directive: &SpeechDirective) -> PcmAudio {
    if directive.pause_before_ms == 0 && directive.pause_after_ms == 0 && !directive.breath {
        return pcm;
    }
    let rate = pcm.sample_rate;
    let channels = pcm.channels;
    let before = samples_for(directive.pause_before_ms, rate, channels);
    let breath = if directive.breath {
        synth_breath(rate, channels)
    } else {
        Vec::new()
    };
    let gap = if breath.is_empty() {
        0
    } else {
        samples_for(BREATH_GAP_MS, rate, channels)
    };
    let after = samples_for(directive.pause_after_ms, rate, channels);

    let mut samples = Vec::with_capacity(before + breath.len() + gap + pcm.samples.len() + after);
    samples.resize(before, 0.0);
    samples.extend_from_slice(&breath);
    samples.resize(samples.len() + gap, 0.0);
    samples.extend_from_slice(&pcm.samples);
    samples.resize(samples.len() + after, 0.0);
    PcmAudio::new(samples, rate, channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice(rate: u32, channels: u16) -> PcmAudio {
        PcmAudio::new(
            vec![0.5; rate as usize / 10 * usize::from(channels)],
            rate,
            channels,
        )
    }

    #[test]
    fn neutral_directive_leaves_pcm_untouched() {
        let pcm = voice(24_000, 1);
        assert_eq!(
            apply_directive_pcm(pcm.clone(), &SpeechDirective::default()),
            pcm
        );
    }

    #[test]
    fn pauses_add_exact_silence_around_the_phrase() {
        let pcm = voice(24_000, 1);
        let directive = SpeechDirective {
            pause_before_ms: 250,
            pause_after_ms: 400,
            ..Default::default()
        };
        let out = apply_directive_pcm(pcm.clone(), &directive);
        let before = 6_000;
        let after = 9_600;
        assert_eq!(out.samples.len(), before + pcm.samples.len() + after);
        assert!(out.samples[..before].iter().all(|s| *s == 0.0));
        assert_eq!(
            &out.samples[before..before + pcm.samples.len()],
            &pcm.samples[..]
        );
        assert!(out.samples[before + pcm.samples.len()..]
            .iter()
            .all(|s| *s == 0.0));
        assert!((out.duration_seconds() - (0.1 + 0.25 + 0.4)).abs() < 1e-3);
    }

    #[test]
    fn pauses_respect_stereo_interleaving() {
        let pcm = voice(16_000, 2);
        let directive = SpeechDirective {
            pause_before_ms: 100,
            ..Default::default()
        };
        let out = apply_directive_pcm(pcm.clone(), &directive);
        assert_eq!(out.samples.len(), 3_200 + pcm.samples.len());
        assert_eq!(out.samples.len() % 2, 0);
    }

    #[test]
    fn breath_is_soft_shaped_and_precedes_the_voice() {
        let breath = synth_breath(24_000, 1);
        assert_eq!(breath.len(), 24_000 * BREATH_MS as usize / 1_000);
        let peak = breath.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.01 && peak <= BREATH_PEAK + 1e-4, "pico {peak}");
        // Bordes suaves: sin clic al inicio ni al final.
        assert!(breath[..24].iter().all(|s| s.abs() < 0.002));
        assert!(breath[breath.len() - 24..].iter().all(|s| s.abs() < 0.002));
        // Determinista.
        assert_eq!(breath, synth_breath(24_000, 1));

        let pcm = voice(24_000, 1);
        let directive = SpeechDirective {
            breath: true,
            ..Default::default()
        };
        let out = apply_directive_pcm(pcm.clone(), &directive);
        let gap = 24_000 * BREATH_GAP_MS as usize / 1_000;
        assert_eq!(out.samples.len(), breath.len() + gap + pcm.samples.len());
        assert_eq!(&out.samples[..breath.len()], &breath[..]);
        assert_eq!(out.samples[breath.len() + gap..], pcm.samples[..]);
    }

    #[test]
    fn degenerate_formats_do_not_panic() {
        assert!(synth_breath(0, 1).is_empty());
        assert!(synth_breath(24_000, 0).is_empty());
        let empty = PcmAudio::new(Vec::new(), 0, 0);
        let directive = SpeechDirective {
            pause_before_ms: 300,
            breath: true,
            ..Default::default()
        };
        assert!(apply_directive_pcm(empty, &directive).samples.is_empty());
    }
}
