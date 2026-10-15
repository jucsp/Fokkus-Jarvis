//! Funciones puras de procesamiento de los callbacks de audio de `cpal`.
//!
//! Los callbacks de cpal corren en un hilo de tiempo real y están sujetos a
//! **CA-1.3**: cero *locks*, cero asignaciones de heap y cero syscalls
//! bloqueantes. Por eso toda la lógica de muestras vive aquí, recibiendo slices
//! y referencias a los extremos del `ringbuf`, y **nunca** asigna memoria.
//! El test `tests/no_alloc.rs` ejecuta estas funciones bajo un *global
//! allocator* contador y exige 0 asignaciones.

use ringbuf::traits::{Consumer, Producer};

/// Sanea una muestra: `NaN`/`±inf` → `0.0` y recorte a `[-1.0, 1.0]`.
///
/// Es el guardián de entrada de `earshot` y `sonora`, que asumen muestras
/// finitas y acotadas (regla 5 del brief).
#[inline]
pub fn sanitize_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// Sanea un slice completo en el lugar (sin asignar).
#[inline]
pub fn sanitize_slice(samples: &mut [f32]) {
    for sample in samples.iter_mut() {
        *sample = sanitize_sample(*sample);
    }
}

/// Mezcla `data` (interleaved, `channels` canales) a mono y escribe el resultado
/// en `out`. Devuelve el número de frames escritos.
///
/// Mezcla promediando los canales; si `channels == 0` no escribe nada.
pub fn mix_down_to_mono(data: &[f32], channels: usize, out: &mut [f32]) -> usize {
    if channels == 0 {
        return 0;
    }
    let frames = data.len() / channels;
    let n = frames.min(out.len());
    for i in 0..n {
        let mut sum = 0.0f32;
        for ch in 0..channels {
            sum += data[i * channels + ch];
        }
        out[i] = sanitize_sample(sum / channels as f32);
    }
    n
}

/// RMS normalizado de un slice mono (0.0..=1.0 para muestras en [-1, 1]).
pub fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt().clamp(0.0, 1.0)
}

/// Procesa un frame de **captura**: mezcla a mono y empuja al ring buffer.
///
/// `scratch` es un buffer preasignado; si el callback trae más frames de los que
/// caben, se procesa por tramos (nunca se trunca ni se entra en pánico).
/// Devuelve el número de muestras perdidas porque el buffer estaba lleno (no
/// bloquea ni aloca; el llamador acumula el conteo en un atómico).
pub fn process_input_frame(
    data: &[f32],
    channels: usize,
    scratch: &mut [f32],
    producer: &mut impl Producer<Item = f32>,
) -> u64 {
    if channels == 0 || scratch.is_empty() {
        return 0;
    }
    let mut dropped = 0u64;
    for chunk in data.chunks(scratch.len() * channels) {
        let n = mix_down_to_mono(chunk, channels, scratch);
        let pushed = producer.push_slice(&scratch[..n]);
        dropped += (n - pushed) as u64;
    }
    dropped
}

/// Estadísticas de un frame de salida procesado.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputFrameStats {
    /// Muestras que no cupieron en el ring buffer de referencia de AEC.
    pub render_dropped: u64,
    /// RMS de lo que realmente sonó (post-fade).
    pub rms: f32,
}

/// Procesa un frame de **reproducción**: consume del mezclador, aplica el
/// *fade-out* si corresponde, publica la referencia de AEC y rellena la salida
/// interleaved (mono → todos los canales).
///
/// `generation` es [`crate::AudioPlayback::generation`]: un cambio respecto de
/// la última vista dispara la rampa aunque la parada ya se haya limpiado con
/// `begin_utterance`. Como en la captura, `scratch` se recorre por tramos.
#[allow(clippy::too_many_arguments)]
pub fn process_output_frame(
    consumer: &mut impl Consumer<Item = f32>,
    scratch: &mut [f32],
    fader: &mut Fader,
    stop_requested: bool,
    generation: u64,
    render: &mut impl Producer<Item = f32>,
    out: &mut [f32],
    channels: usize,
) -> OutputFrameStats {
    if channels == 0 || scratch.is_empty() {
        out.fill(0.0);
        return OutputFrameStats {
            render_dropped: 0,
            rms: 0.0,
        };
    }

    let mut render_dropped = 0u64;
    let mut sum_sq = 0.0f32;
    let mut total = 0usize;
    for chunk in out.chunks_mut(scratch.len() * channels) {
        let frames = chunk.len() / channels;
        let popped = consumer.pop_slice(&mut scratch[..frames]);
        // El resto del tramo queda en silencio si la cola no alcanzó.
        scratch[popped..frames].fill(0.0);

        // Parada por barge-in: rampa de 15 ms y, al terminar, se vacía la cola
        // de la generación cancelada.
        if fader.apply(stop_requested, generation, &mut scratch[..frames]) {
            consumer.clear();
        }

        // Referencia de AEC: lo que realmente sonó, ya con el fade aplicado.
        render_dropped += (frames - render.push_slice(&scratch[..frames])) as u64;
        sum_sq += scratch[..frames].iter().map(|s| s * s).sum::<f32>();
        total += frames;

        // Mono → interleaved; un frame parcial al final queda en silencio.
        for (i, sample) in scratch[..frames].iter().enumerate() {
            chunk[i * channels..(i + 1) * channels].fill(*sample);
        }
        chunk[frames * channels..].fill(0.0);
    }

    let rms = if total == 0 {
        0.0
    } else {
        (sum_sq / total as f32).sqrt().clamp(0.0, 1.0)
    };
    OutputFrameStats {
        render_dropped,
        rms,
    }
}

/// Aplicador de *fade-out* lineal sin estado global ni locks.
///
/// Vive dentro del closure del callback de salida (mutable, un solo hilo).
/// La rampa se dispara cuando cambia la generación de [`crate::AudioPlayback`]
/// (cada `stop()` la incrementa), cae a cero exactamente en `fade_frames`
/// muestras y, una vez iniciada, **siempre** termina y pide drenar la cola: el
/// audio pendiente pertenece a la generación cancelada y no debe sonar aunque
/// `begin_utterance` limpie la bandera de parada a mitad de rampa.
#[derive(Debug, Clone)]
pub struct Fader {
    /// Longitud de la rampa en muestras (15 ms × tasa del dispositivo).
    fade_frames: usize,
    /// Muestras restantes de la rampa actual (0 = sin rampa).
    remaining: usize,
    /// Última generación observada.
    seen_generation: u64,
}

impl Fader {
    /// Crea un *fader* con la rampa indicada (se fuerza al menos 1 muestra).
    /// `generation` es la generación vigente al abrir el stream.
    pub fn new(fade_frames: usize, generation: u64) -> Self {
        Self {
            fade_frames: fade_frames.max(1),
            remaining: 0,
            seen_generation: generation,
        }
    }

    /// Muestras de rampa para 15 ms a `sample_rate` (redondeo hacia arriba).
    pub fn frames_for_rate(sample_rate: u32) -> usize {
        (sample_rate as usize * 15).div_ceil(1000)
    }

    /// Longitud de la rampa en muestras.
    pub fn fade_frames(&self) -> usize {
        self.fade_frames
    }

    /// `true` si hay una rampa en curso.
    pub fn is_active(&self) -> bool {
        self.remaining > 0
    }

    /// Aplica el fade (o silencio) a `buf` según la generación y la parada.
    ///
    /// Devuelve `true` exactamente una vez por parada: cuando la rampa terminó y
    /// el llamador debe vaciar la cola de la generación cancelada.
    pub fn apply(&mut self, stop_requested: bool, generation: u64, buf: &mut [f32]) -> bool {
        if generation != self.seen_generation {
            self.seen_generation = generation;
            self.remaining = self.fade_frames;
        }

        if self.remaining > 0 {
            let done = self.fade_frames - self.remaining;
            let end = buf.len().min(self.remaining);
            for (i, sample) in buf[..end].iter_mut().enumerate() {
                let gain = 1.0 - ((done + i) as f32 / self.fade_frames as f32);
                *sample *= gain.max(0.0);
            }
            buf[end..].fill(0.0);
            self.remaining -= end;
            return self.remaining == 0;
        }

        // Parada vigente sin rampa pendiente: silencio total hasta la próxima
        // locución (descarta lo que un productor rezagado haya encolado).
        if stop_requested {
            buf.fill(0.0);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::traits::{Observer, Split};
    use ringbuf::HeapRb;

    #[test]
    fn sanitize_sample_maps_non_finite_to_zero() {
        assert_eq!(sanitize_sample(f32::NAN), 0.0);
        assert_eq!(sanitize_sample(f32::INFINITY), 0.0);
        assert_eq!(sanitize_sample(f32::NEG_INFINITY), 0.0);
    }

    #[test]
    fn sanitize_sample_clamps() {
        assert_eq!(sanitize_sample(1.5), 1.0);
        assert_eq!(sanitize_sample(-2.0), -1.0);
        assert_eq!(sanitize_sample(0.5), 0.5);
    }

    #[test]
    fn mix_down_to_mono_averages_channels() {
        let data = [1.0f32, 0.0, 0.5, 0.5, -1.0, -1.0]; // 3 frames estéreo
        let mut out = [0.0f32; 3];
        let n = mix_down_to_mono(&data, 2, &mut out);
        assert_eq!(n, 3);
        assert_eq!(out, [0.5, 0.5, -1.0]);
    }

    #[test]
    fn mix_down_to_mono_handles_zero_channels_and_short_output() {
        let data = [1.0f32, 2.0];
        let mut out = [0.0f32; 1];
        assert_eq!(mix_down_to_mono(&data, 0, &mut out), 0);
        // 1 frame estéreo (1.0 + 2.0) / 2 = 1.5 → saneado a 1.0.
        assert_eq!(mix_down_to_mono(&data, 2, &mut out), 1);
        assert_eq!(out[0], 1.0);
    }

    #[test]
    fn compute_rms_square_and_silence() {
        assert_eq!(compute_rms(&[]), 0.0);
        assert_eq!(compute_rms(&[0.0; 100]), 0.0);
        let square: Vec<f32> = (0..100)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        assert!((compute_rms(&square) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn process_input_frame_counts_dropped_samples_with_full_buffer() {
        let rb = HeapRb::<f32>::new(4);
        let (mut prod, cons) = rb.split();
        let mut scratch = [0.0f32; 8];
        // Caben 4 de 8 muestras → 4 perdidas.
        assert_eq!(
            process_input_frame(&[0.5f32; 8], 1, &mut scratch, &mut prod),
            4
        );
        // Con el buffer lleno se pierde todo el callback siguiente.
        assert_eq!(
            process_input_frame(&[0.5f32; 6], 1, &mut scratch, &mut prod),
            6
        );
        assert_eq!(cons.occupied_len(), 4);
    }

    #[test]
    fn process_input_frame_processes_callbacks_larger_than_scratch() {
        let rb = HeapRb::<f32>::new(64);
        let (mut prod, cons) = rb.split();
        let mut scratch = [0.0f32; 3];
        // 10 frames estéreo con un scratch de 3: se procesa en 4 tramos.
        let dropped = process_input_frame(&[0.25f32; 20], 2, &mut scratch, &mut prod);
        assert_eq!(dropped, 0);
        assert_eq!(cons.occupied_len(), 10);
    }

    #[test]
    fn process_output_frame_interleaves_mono_to_stereo() {
        let rb = HeapRb::<f32>::new(8);
        let (mut prod, mut cons) = rb.split();
        let render_rb = HeapRb::<f32>::new(8);
        let (mut render_prod, render_cons) = render_rb.split();

        prod.push_slice(&[0.25, 0.5, 0.75]);
        let mut scratch = [0.0f32; 2];
        let mut out = [0.0f32; 8];
        let mut fader = Fader::new(720, 0);

        let stats = process_output_frame(
            &mut cons,
            &mut scratch,
            &mut fader,
            false,
            0,
            &mut render_prod,
            &mut out,
            2,
        );

        // La cuarta muestra no existía en la cola → silencio.
        assert_eq!(out, [0.25, 0.25, 0.5, 0.5, 0.75, 0.75, 0.0, 0.0]);
        assert_eq!(stats.render_dropped, 0);
        assert_eq!(render_cons.occupied_len(), 4);
        assert!(stats.rms > 0.0);
    }

    #[test]
    fn frames_for_rate_is_15ms() {
        assert_eq!(Fader::frames_for_rate(48_000), 720);
        // 661,5 → 662 (dentro de ± 1 frame).
        assert_eq!(Fader::frames_for_rate(44_100), 662);
    }

    #[test]
    fn fader_reaches_zero_at_exactly_fade_frames_at_48k() {
        let mut fader = Fader::new(Fader::frames_for_rate(48_000), 0);
        let mut buf = [1.0f32; 720];
        // stop() incrementa la generación: dispara la rampa completa.
        let drain = fader.apply(true, 1, &mut buf);
        assert!(drain, "la rampa debe completarse y solicitar drenado");
        assert_eq!(buf[0], 1.0);
        assert!((buf[360] - 0.5).abs() < 1e-6);
        // Última muestra de la rampa: ganancia 1/720 ≈ 0 (dentro de 1 frame).
        assert!(buf[719] <= 1.0 / 720.0 + 1e-6);
        let mut after = [1.0f32; 4];
        assert!(!fader.apply(true, 1, &mut after));
        assert_eq!(after, [0.0; 4]);
    }

    #[test]
    fn fader_reaches_zero_at_44100_within_one_frame() {
        let mut fader = Fader::new(Fader::frames_for_rate(44_100), 0);
        let mut buf = [1.0f32; 700];
        assert!(fader.apply(true, 1, &mut buf));
        assert_eq!(buf[0], 1.0);
        assert!(buf[661] <= 1.0 / 662.0 + 1e-6);
        assert!(buf[662..].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn fader_ramp_is_continuous_across_callbacks() {
        let mut fader = Fader::new(720, 0);
        let mut first = [1.0f32; 480];
        assert!(!fader.apply(true, 1, &mut first));
        let mut second = [1.0f32; 480];
        assert!(fader.apply(true, 1, &mut second));
        // La rampa continúa donde quedó (muestra global 480 + i).
        assert!((second[0] - (1.0 - 480.0 / 720.0)).abs() < 1e-6);
        assert!((second[239] - (1.0 - 719.0 / 720.0)).abs() < 1e-6);
        assert!(second[240..].iter().all(|s| *s == 0.0));
        // Parada aún activa: no re-dispara el fade y mantiene silencio.
        let mut third = [1.0f32; 480];
        assert!(!fader.apply(true, 1, &mut third));
        assert_eq!(third, [0.0f32; 480]);
    }

    #[test]
    fn generation_change_fades_even_if_stop_was_already_cleared() {
        // stop() + begin_utterance() entre dos callbacks: la bandera ya está
        // limpia, pero la generación cambió y la cola vieja debe cancelarse.
        let mut fader = Fader::new(720, 3);
        let mut buf = [1.0f32; 480];
        assert!(!fader.apply(false, 4, &mut buf));
        assert!(buf[479] < 1.0, "debe estar atenuando");
        let mut rest = [1.0f32; 480];
        assert!(
            fader.apply(false, 4, &mut rest),
            "debe drenar la cola vieja"
        );
        // Nueva locución sin parada: suena intacta.
        let mut next = [0.8f32; 16];
        assert!(!fader.apply(false, 4, &mut next));
        assert_eq!(next, [0.8f32; 16]);
    }

    #[test]
    fn output_frame_drains_queue_of_cancelled_generation() {
        let rb = HeapRb::<f32>::new(4096);
        let (mut prod, mut cons) = rb.split();
        let render_rb = HeapRb::<f32>::new(4096);
        let (mut render_prod, _render_cons) = render_rb.split();
        prod.push_slice(&[0.5f32; 3000]);

        let mut scratch = [0.0f32; 1024];
        let mut out = [0.0f32; 1024];
        let mut fader = Fader::new(720, 0);
        process_output_frame(
            &mut cons,
            &mut scratch,
            &mut fader,
            true,
            1,
            &mut render_prod,
            &mut out,
            1,
        );
        // Rampa completa dentro del callback → cola vaciada.
        assert_eq!(cons.occupied_len(), 0);
        assert!(out[720..].iter().all(|s| *s == 0.0));
    }
}
