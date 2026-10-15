//! Remuestreo mono de la tasa del dispositivo a 16 kHz con `rubato` (HU-2).
//!
//! Se usa el remuestreador **síncrono por FFT** (`Fft` + `FixedSync::Input`):
//! la relación entre la tasa del dispositivo y 16 kHz es fija y racional
//! (48 000/16 000 = 3, 44 100/16 000 = 441/160…), así que no hace falta el
//! remuestreador asíncrono (`Async`), más caro, cuya ventaja es ajustar la
//! relación en caliente. La deriva entre los relojes del micrófono y del
//! altavoz la absorbe el estimador de retardo interno de AEC3.
//!
//! Todos los buffers se reservan al construir: `process` no asigna memoria.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

use super::DspError;
use crate::io::callbacks::sanitize_slice;

/// Tramo de paso cuando las tasas coinciden (10 ms a 16 kHz).
const PASSTHROUGH_CHUNK: usize = 160;

/// Remuestreador mono con buffers preasignados.
pub struct MonoResampler {
    /// `None` si la tasa de entrada ya es la de salida.
    inner: Option<Fft<f32>>,
    input: Vec<f32>,
    filled: usize,
    output: Vec<f32>,
    input_rate: u32,
}

impl MonoResampler {
    /// Crea un remuestreador `input_rate` → `output_rate`.
    ///
    /// El tramo de entrada es de 10 ms; rubato lo redondea a un múltiplo de su
    /// tamaño de FFT.
    pub fn new(input_rate: u32, output_rate: u32) -> Result<Self, DspError> {
        if input_rate == 0 || output_rate == 0 {
            return Err(DspError::InvalidSampleRate(input_rate.min(output_rate)));
        }
        if input_rate == output_rate {
            return Ok(Self {
                inner: None,
                input: vec![0.0; PASSTHROUGH_CHUNK],
                filled: 0,
                output: Vec::new(),
                input_rate,
            });
        }
        let chunk = (input_rate as usize / 100).max(1);
        let inner = Fft::<f32>::new(
            input_rate as usize,
            output_rate as usize,
            chunk,
            1,
            FixedSync::Input,
        )
        .map_err(DspError::ResamplerConstruction)?;
        Ok(Self {
            input: vec![0.0; inner.input_frames_max()],
            output: vec![0.0; inner.output_frames_max()],
            inner: Some(inner),
            filled: 0,
            input_rate,
        })
    }

    /// Tasa de entrada configurada.
    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    /// Retardo propio del filtro, en muestras de salida (0 si es paso directo).
    pub fn output_delay(&self) -> usize {
        self.inner.as_ref().map_or(0, |r| r.output_delay())
    }

    /// Remuestrea `samples` y entrega cada tramo de salida a `on_output`.
    ///
    /// La entrada se sanea al copiarla y la salida se vuelve a sanear: el
    /// filtro puede sobrepasar ±1 con señales saturadas (fenómeno de Gibbs).
    pub fn process(
        &mut self,
        mut samples: &[f32],
        mut on_output: impl FnMut(&[f32]),
    ) -> Result<(), DspError> {
        while !samples.is_empty() {
            let need = match &self.inner {
                Some(r) => r.input_frames_next(),
                None => self.input.len(),
            };
            let take = (need - self.filled).min(samples.len());
            let dst = &mut self.input[self.filled..self.filled + take];
            dst.copy_from_slice(&samples[..take]);
            sanitize_slice(dst);
            self.filled += take;
            samples = &samples[take..];
            if self.filled < need {
                break;
            }
            self.filled = 0;

            let Some(resampler) = self.inner.as_mut() else {
                on_output(&self.input[..need]);
                continue;
            };
            let input = InterleavedSlice::new(&self.input[..need], 1, need)
                .map_err(|_| DspError::BufferSize)?;
            let out_len = self.output.len();
            let mut output = InterleavedSlice::new_mut(&mut self.output[..], 1, out_len)
                .map_err(|_| DspError::BufferSize)?;
            let (_, produced) = resampler
                .process_into_buffer(&input, &mut output, None)
                .map_err(DspError::Resample)?;
            sanitize_slice(&mut self.output[..produced]);
            on_output(&self.output[..produced]);
        }
        Ok(())
    }

    /// Descarta lo acumulado y el estado del filtro.
    pub fn reset(&mut self) {
        self.filled = 0;
        if let Some(r) = self.inner.as_mut() {
            r.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Remuestrea `seconds` de un tono y devuelve la salida completa.
    fn run(rate: u32, freq: f32, seconds: f32) -> Vec<f32> {
        let mut rs = MonoResampler::new(rate, 16_000).expect("tasa válida");
        let n = (rate as f32 * seconds) as usize;
        let input: Vec<f32> = (0..n)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin())
            .collect();
        let mut out = Vec::new();
        // Tramos irregulares, como los que entrega cpal.
        for chunk in input.chunks(333) {
            rs.process(chunk, |o| out.extend_from_slice(o))
                .expect("remuestreo");
        }
        out
    }

    #[test]
    fn output_length_matches_ratio_for_common_rates() {
        for rate in [8_000u32, 16_000, 22_050, 44_100, 48_000, 96_000] {
            let out = run(rate, 440.0, 1.0);
            // 1 s de entrada → ~16 000 muestras, salvo el último tramo parcial.
            assert!(
                (15_500..=16_000).contains(&out.len()),
                "{rate} Hz produjo {} muestras",
                out.len()
            );
        }
    }

    #[test]
    fn tone_energy_is_preserved_after_startup() {
        let out = run(48_000, 1_000.0, 1.0);
        let tail = &out[out.len() / 2..];
        let rms = (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt();
        // Seno de amplitud 0,5 → RMS 0,354.
        assert!((rms - 0.354).abs() < 0.02, "RMS {rms}");
    }

    #[test]
    fn rejects_zero_rate() {
        assert!(matches!(
            MonoResampler::new(0, 16_000),
            Err(DspError::InvalidSampleRate(0))
        ));
    }
}
