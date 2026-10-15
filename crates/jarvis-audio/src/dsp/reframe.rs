//! Re-encuadre de muestras a frames de largo fijo, sin asignar por frame.
//!
//! `sonora` procesa frames de 160 muestras (10 ms a 16 kHz) y `earshot` exige
//! 256 (16 ms). El remuestreador entrega tramos de largo variable. Un
//! [`Reframer`] acumula en un buffer preasignado y entrega cada frame completo
//! a un closure; el resto queda pendiente para la próxima llamada.

/// Acumulador de largo fijo con buffer preasignado.
#[derive(Debug, Clone)]
pub struct Reframer {
    buf: Vec<f32>,
    len: usize,
}

impl Reframer {
    /// Crea un acumulador de `frame_len` muestras (se fuerza al menos 1).
    pub fn new(frame_len: usize) -> Self {
        Self {
            buf: vec![0.0; frame_len.max(1)],
            len: 0,
        }
    }

    /// Largo del frame que se entrega.
    pub fn frame_len(&self) -> usize {
        self.buf.len()
    }

    /// Muestras acumuladas que aún no completan un frame.
    pub fn pending(&self) -> usize {
        self.len
    }

    /// Agrega `samples` y llama a `on_frame` por cada frame completo.
    ///
    /// El frame se presta mutable para que el consumidor pueda sanearlo o
    /// procesarlo en el lugar; su contenido se descarta tras la llamada.
    pub fn push(&mut self, mut samples: &[f32], mut on_frame: impl FnMut(&mut [f32])) {
        let frame_len = self.buf.len();
        while !samples.is_empty() {
            let take = (frame_len - self.len).min(samples.len());
            self.buf[self.len..self.len + take].copy_from_slice(&samples[..take]);
            self.len += take;
            samples = &samples[take..];
            if self.len == frame_len {
                on_frame(&mut self.buf);
                self.len = 0;
            }
        }
    }

    /// Descarta lo acumulado (cambio de dispositivo o reinicio).
    pub fn reset(&mut self) {
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_full_frames_and_keeps_remainder() {
        let mut r = Reframer::new(4);
        let mut frames = Vec::new();
        r.push(&[1.0, 2.0, 3.0], |f| frames.push(f.to_vec()));
        assert!(frames.is_empty());
        assert_eq!(r.pending(), 3);
        r.push(&[4.0, 5.0, 6.0, 7.0, 8.0, 9.0], |f| frames.push(f.to_vec()));
        assert_eq!(
            frames,
            vec![vec![1.0, 2.0, 3.0, 4.0], vec![5.0, 6.0, 7.0, 8.0]]
        );
        assert_eq!(r.pending(), 1);
    }

    #[test]
    fn handles_empty_and_single_sample_pushes() {
        let mut r = Reframer::new(2);
        let mut count = 0;
        r.push(&[], |_| count += 1);
        r.push(&[0.5], |_| count += 1);
        assert_eq!(count, 0);
        r.push(&[0.5], |_| count += 1);
        assert_eq!(count, 1);
    }

    #[test]
    fn regroups_160_into_256_without_losing_samples() {
        let mut r = Reframer::new(256);
        let mut total = 0usize;
        let mut next = 0.0f32;
        let mut contiguous = true;
        for chunk in 0..16 {
            let frame: Vec<f32> = (0..160).map(|i| (chunk * 160 + i) as f32).collect();
            r.push(&frame, |f| {
                total += f.len();
                for s in f.iter() {
                    contiguous &= *s == next;
                    next += 1.0;
                }
            });
        }
        // 16 × 160 = 2560 = 10 × 256.
        assert_eq!(total, 2560);
        assert_eq!(r.pending(), 0);
        assert!(contiguous, "el orden de las muestras debe preservarse");
    }
}
