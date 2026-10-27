//! Detección de voz (VAD) con `earshot` y *endpointing* de turno (HU-3).
//!
//! [`VadDetector`] envuelve la red neuronal embebida de `earshot` (Rust puro,
//! ~8 KiB por detector) y [`endpoint::Endpointer`] convierte los puntajes en
//! eventos de voz con pre-roll y fin de turno adaptativo.

pub mod endpoint;

pub use endpoint::{
    EndpointConfig, Endpointer, TurnPauseStats, TurnPauseSummary, FRAME_SAMPLES, SAMPLE_RATE,
};

use crate::io::callbacks::sanitize_slice;

/// Envoltura de `earshot::Detector`.
///
/// Sanea cada frame antes de predecir (regla 5 del brief): `earshot` asume
/// muestras finitas en `[-1, 1]` y devuelve `-1.0` si el frame no mide 256.
pub struct VadDetector {
    detector: Box<earshot::Detector>,
}

impl VadDetector {
    /// Crea un detector directamente en el heap (evita 8 KiB en la pila).
    pub fn new() -> Self {
        Self {
            detector: earshot::Detector::default_boxed(),
        }
    }

    /// Puntaje de voz `0.0..=1.0` de un frame de 256 muestras a 16 kHz.
    ///
    /// El frame se sanea en el lugar; si no mide exactamente 256 muestras se
    /// devuelve `0.0` (silencio) en lugar de propagar el `-1.0` de earshot.
    pub fn score(&mut self, frame: &mut [f32]) -> f32 {
        sanitize_slice(frame);
        if frame.len() != endpoint::FRAME_SAMPLES {
            return 0.0;
        }
        self.detector.predict_f32(frame).clamp(0.0, 1.0)
    }

    /// Reinicia el estado interno (cambio de dispositivo o nueva secuencia).
    pub fn reset(&mut self) {
        self.detector.reset();
    }
}

impl Default for VadDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_scores_low() {
        let mut vad = VadDetector::new();
        let mut frame = [0.0f32; endpoint::FRAME_SAMPLES];
        let score = vad.score(&mut frame);
        assert!(
            score < 0.5,
            "el silencio digital debe puntuar bajo: {score}"
        );
    }

    /// Ruido blanco uniforme determinista (LCG) con amplitud de pico `peak`.
    fn white_noise(seed: &mut u32, peak: f32, frame: &mut [f32]) {
        for sample in frame.iter_mut() {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *sample = ((*seed >> 8) as f32 / 16_777_216.0 * 2.0 - 1.0) * peak;
        }
    }

    #[test]
    fn silence_and_white_noise_calibration() {
        // Calibración inicial para CA-3.1 (earshot 1.2.2, 2 s por señal).
        // Ruido uniforme con RMS −20 dBFS: pico = 0,1 × √3.
        const FRAMES: usize = 125;
        let mut vad = VadDetector::new();
        let mut frame = [0.0f32; endpoint::FRAME_SAMPLES];
        let mut silence_max = 0.0f32;
        for _ in 0..FRAMES {
            frame.fill(0.0);
            silence_max = silence_max.max(vad.score(&mut frame));
        }

        vad.reset();
        let mut seed = 0x1234_5678u32;
        let (mut sum, mut noise_max) = (0.0f32, 0.0f32);
        let (mut streak, mut max_streak) = (0usize, 0usize);
        let threshold = EndpointConfig::default().voice_open;
        for _ in 0..FRAMES {
            white_noise(&mut seed, 0.1 * 3f32.sqrt(), &mut frame);
            let score = vad.score(&mut frame);
            sum += score;
            noise_max = noise_max.max(score);
            streak = if score >= threshold { streak + 1 } else { 0 };
            max_streak = max_streak.max(streak);
        }
        let noise_mean = sum / FRAMES as f32;
        eprintln!(
            "calibración earshot: silencio máx {silence_max:.3}; \
             ruido blanco −20 dBFS media {noise_mean:.3}, máx {noise_max:.3}, \
             racha máx ≥ umbral {max_streak} frames"
        );
        // Medido el 2026-10-15: silencio 0,15–0,28 (decae tras el arranque);
        // ruido −20 dBFS media 0,25, picos de 0,54 y rachas de hasta 3 frames
        // sobre el umbral 0,45: con ruido crudo el hangover por defecto (2)
        // abriría turno. En producción el VAD ve la señal ya pasada por NS
        // (ver `dsp::tests`); CA-3.1 se calibra con el corpus real.
        assert!(silence_max < 0.3, "el silencio digital debe puntuar bajo");
        assert!(noise_mean < threshold);
        assert!((0.0..=1.0).contains(&noise_mean) && (0.0..=1.0).contains(&noise_max));
    }

    #[test]
    fn wrong_frame_length_returns_silence() {
        let mut vad = VadDetector::new();
        let mut short = [0.5f32; 10];
        assert_eq!(vad.score(&mut short), 0.0);
    }

    #[test]
    fn nan_samples_do_not_panic() {
        let mut vad = VadDetector::new();
        let mut frame = [f32::NAN; endpoint::FRAME_SAMPLES];
        let score = vad.score(&mut frame);
        assert!(score.is_finite());
    }
}
