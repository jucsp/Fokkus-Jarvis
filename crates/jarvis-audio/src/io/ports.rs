//! Estado que vive dentro de los callbacks de `cpal` (HU-ORQ-1).
//!
//! [`InputPort`] y [`OutputPort`] se construyen fuera del hilo de tiempo real
//! (ahí se reservan todos los buffers) y luego se mueven al closure del
//! stream. Sus métodos solo usan las funciones de [`super::callbacks`],
//! atómicos y `ringbuf`: cero locks, cero asignaciones, cero I/O (CA-1.3,
//! verificado en `tests/no_alloc.rs`).
//!
//! Los errores del stream llegan por otro closure de cpal; [`StreamFault`]
//! los clasifica y levanta una bandera atómica que el supervisor
//! ([`super::engine`]) consulta para reabrir el dispositivo (CA-1.2).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ringbuf::traits::{Consumer, Producer};
use ringbuf::{HeapCons, HeapProd};

use super::callbacks::{compute_rms, process_input_frame, process_output_frame, Fader};
use super::metrics::AudioMetrics;
use crate::dsp::LatencyProbe;
use crate::playback::AudioPlayback;

/// Frames por tramo de los buffers de trabajo de los callbacks. Un callback
/// más grande se procesa por tramos (nunca se trunca ni se asigna).
pub const CALLBACK_CHUNK_FRAMES: usize = 2_048;

/// Nivel publicado como `f32::to_bits` en un atómico (último valor gana).
#[derive(Debug, Default)]
pub struct LevelMeter(AtomicU32);

impl LevelMeter {
    pub fn store(&self, level: f32) {
        self.0
            .store(level.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// Estado compartido entre los callbacks y el resto del proceso.
#[derive(Debug)]
pub struct PortShared {
    pub metrics: Arc<AudioMetrics>,
    pub latency: Arc<LatencyProbe>,
    pub playback: AudioPlayback,
    /// RMS del micrófono (lo que oye Jarvis).
    pub input_level: LevelMeter,
    /// RMS de lo que realmente sonó por los altavoces (post-fade).
    pub output_level: LevelMeter,
    /// `true` si el hilo DSP consume la referencia de salida (AEC).
    pub render_tap: AtomicBool,
}

impl PortShared {
    pub fn new(playback: AudioPlayback) -> Self {
        Self {
            metrics: Arc::new(AudioMetrics::new()),
            latency: Arc::new(LatencyProbe::new()),
            playback,
            input_level: LevelMeter::default(),
            output_level: LevelMeter::default(),
            render_tap: AtomicBool::new(false),
        }
    }
}

/// Bandera de fallo de un stream, escrita desde el closure de error de cpal.
#[derive(Debug, Clone)]
pub struct StreamFault {
    raised: Arc<AtomicBool>,
    metrics: Arc<AudioMetrics>,
}

impl StreamFault {
    pub fn new(metrics: Arc<AudioMetrics>) -> Self {
        Self {
            raised: Arc::new(AtomicBool::new(false)),
            metrics,
        }
    }

    /// Clasifica un error del stream.
    ///
    /// - *Xrun*: se cuenta; el stream sigue vivo.
    /// - Cambio de ruta o RT denegado: informativos, el stream sigue vivo.
    /// - Cualquier otro (dispositivo desaparecido, stream invalidado, error
    ///   del backend): el stream está muerto y hay que reabrirlo.
    pub fn report(&self, kind: cpal::ErrorKind) {
        match kind {
            cpal::ErrorKind::Xrun => self.metrics.add_xrun(),
            cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied => {}
            _ => self.raise(),
        }
    }

    /// Marca el stream como caído.
    pub fn raise(&self) {
        if !self.raised.swap(true, Ordering::AcqRel) {
            self.metrics.add_disconnect();
        }
    }

    pub fn is_raised(&self) -> bool {
        self.raised.load(Ordering::Acquire)
    }
}

/// Estado del callback de captura.
pub struct InputPort {
    producer: HeapProd<f32>,
    channels: usize,
    /// Tramo mono (salida de la mezcla a mono).
    scratch: Box<[f32]>,
    /// Tramo interleaved convertido desde `i16`.
    convert: Box<[f32]>,
    shared: Arc<PortShared>,
    fault: StreamFault,
}

impl InputPort {
    /// Reserva aquí todos los buffers: el callback ya no asigna.
    pub fn new(producer: HeapProd<f32>, channels: u16, shared: Arc<PortShared>) -> Self {
        let channels = usize::from(channels.max(1));
        let fault = StreamFault::new(Arc::clone(&shared.metrics));
        Self {
            producer,
            channels,
            scratch: vec![0.0; CALLBACK_CHUNK_FRAMES].into_boxed_slice(),
            convert: vec![0.0; CALLBACK_CHUNK_FRAMES * channels].into_boxed_slice(),
            shared,
            fault,
        }
    }

    /// Bandera de fallo para el closure de error y el supervisor.
    pub fn fault(&self) -> StreamFault {
        self.fault.clone()
    }

    /// Callback con muestras `f32` interleaved.
    pub fn on_f32(&mut self, data: &[f32]) {
        push_capture(
            &mut self.producer,
            self.channels,
            &mut self.scratch,
            &self.shared,
            data,
        );
    }

    /// Callback con muestras `i16` interleaved (se convierten por tramos).
    pub fn on_i16(&mut self, data: &[i16]) {
        let step = self.convert.len() - self.convert.len() % self.channels;
        for chunk in data.chunks(step.max(self.channels)) {
            let dst = &mut self.convert[..chunk.len()];
            for (d, s) in dst.iter_mut().zip(chunk) {
                *d = f32::from(*s) / 32_768.0;
            }
            push_capture(
                &mut self.producer,
                self.channels,
                &mut self.scratch,
                &self.shared,
                &self.convert[..chunk.len()],
            );
        }
    }

    /// Latencia de entrada (`callback − capture`) para el AEC.
    pub fn on_latency(&self, latency: Duration) {
        self.shared.latency.publish_input(latency);
    }
}

fn push_capture(
    producer: &mut HeapProd<f32>,
    channels: usize,
    scratch: &mut [f32],
    shared: &PortShared,
    data: &[f32],
) {
    let dropped = process_input_frame(data, channels, scratch, producer);
    if dropped > 0 {
        shared.metrics.add_input_dropped(dropped);
    }
    shared.input_level.store(compute_rms(data));
}

/// Estado del callback de reproducción.
pub struct OutputPort {
    consumer: HeapCons<f32>,
    render: HeapProd<f32>,
    channels: usize,
    scratch: Box<[f32]>,
    /// Tramo interleaved `f32` que luego se convierte a `i16`.
    convert: Box<[f32]>,
    fader: Fader,
    shared: Arc<PortShared>,
    fault: StreamFault,
}

impl OutputPort {
    /// `consumer` es la cola que llena el TTS; `render`, la referencia de AEC.
    pub fn new(
        consumer: HeapCons<f32>,
        render: HeapProd<f32>,
        sample_rate: u32,
        channels: u16,
        shared: Arc<PortShared>,
    ) -> Self {
        let channels = usize::from(channels.max(1));
        let fault = StreamFault::new(Arc::clone(&shared.metrics));
        let fader = Fader::new(
            Fader::frames_for_rate(sample_rate),
            shared.playback.generation(),
        );
        Self {
            consumer,
            render,
            channels,
            scratch: vec![0.0; CALLBACK_CHUNK_FRAMES].into_boxed_slice(),
            convert: vec![0.0; CALLBACK_CHUNK_FRAMES * channels].into_boxed_slice(),
            fader,
            shared,
            fault,
        }
    }

    pub fn fault(&self) -> StreamFault {
        self.fault.clone()
    }

    /// Callback con salida `f32` interleaved.
    pub fn fill_f32(&mut self, out: &mut [f32]) {
        fill_output(
            &mut self.consumer,
            &mut self.render,
            self.channels,
            &mut self.scratch,
            &mut self.fader,
            &self.shared,
            out,
        );
    }

    /// Callback con salida `i16` interleaved.
    pub fn fill_i16(&mut self, out: &mut [i16]) {
        let step = self.convert.len() - self.convert.len() % self.channels;
        for chunk in out.chunks_mut(step.max(self.channels)) {
            let tmp = &mut self.convert[..chunk.len()];
            fill_output(
                &mut self.consumer,
                &mut self.render,
                self.channels,
                &mut self.scratch,
                &mut self.fader,
                &self.shared,
                tmp,
            );
            for (d, s) in chunk.iter_mut().zip(tmp.iter()) {
                *d = (s * 32_767.0) as i16;
            }
        }
    }

    /// Latencia de salida (`playback − callback`) para el AEC.
    pub fn on_latency(&self, latency: Duration) {
        self.shared.latency.publish_output(latency);
    }
}

fn fill_output(
    consumer: &mut HeapCons<f32>,
    render: &mut HeapProd<f32>,
    channels: usize,
    scratch: &mut [f32],
    fader: &mut Fader,
    shared: &PortShared,
    out: &mut [f32],
) {
    let playback = &shared.playback;
    let stop_requested = playback.is_stop_requested();
    if playback.is_paused() {
        fill_paused(consumer, render, channels, scratch, fader, shared, out);
        return;
    }
    let stats = process_output_frame(
        consumer,
        scratch,
        fader,
        stop_requested,
        playback.generation(),
        render,
        out,
        channels,
    );
    // Parada vigente y rampa terminada: lo que un productor rezagado alcanzó
    // a encolar pertenece a la generación cancelada y no debe sonar cuando
    // empiece la próxima locución.
    if stop_requested && !fader.is_active() {
        consumer.clear();
    }
    // Sin DSP nadie vacía la referencia: el ring se llena y sus "pérdidas"
    // no significan nada, así que solo se cuentan con el AEC activo.
    if stats.render_dropped > 0 && shared.render_tap.load(Ordering::Relaxed) {
        shared.metrics.add_render_dropped(stats.render_dropped);
    }
    shared.output_level.store(stats.rms);
}

/// Pausa provisional: silencio sin consumir la cola. La referencia del AEC
/// recibe el silencio que realmente sonó; el *fader* corre sobre ceros para
/// que, si llega una parada durante la pausa, la cola cancelada se descarte
/// sin que suene ni la rampa.
fn fill_paused(
    consumer: &mut HeapCons<f32>,
    render: &mut HeapProd<f32>,
    channels: usize,
    scratch: &mut [f32],
    fader: &mut Fader,
    shared: &PortShared,
    out: &mut [f32],
) {
    let playback = &shared.playback;
    let stop_requested = playback.is_stop_requested();
    out.fill(0.0);
    let mut frames = out.len() / channels.max(1);
    let mut dropped = 0u64;
    while frames > 0 {
        let n = frames.min(scratch.len());
        let chunk = &mut scratch[..n];
        chunk.fill(0.0);
        fader.apply(stop_requested, playback.generation(), chunk);
        dropped += (n - render.push_slice(chunk)) as u64;
        frames -= n;
    }
    if stop_requested {
        consumer.clear();
    }
    if dropped > 0 && shared.render_tap.load(Ordering::Relaxed) {
        shared.metrics.add_render_dropped(dropped);
    }
    shared.output_level.store(0.0);
}
