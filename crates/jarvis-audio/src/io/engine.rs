//! Motor de streams de audio: captura y reproducción reales (HU-ORQ-1).
//!
//! [`AudioEngine`] arranca el hilo supervisor `jarvis-audio-io`, que es el
//! único dueño de los streams de `cpal` (en algunos backends `cpal::Stream`
//! no es `Send`) y del hilo DSP. Desde fuera solo se le envían órdenes por
//! canal: habilitar/deshabilitar (mute real, CA-4.3) y apagar.
//!
//! # Sesión
//!
//! Una *sesión* agrupa stream de salida + stream de entrada + hilo DSP:
//!
//! ```text
//! cpal input ─► InputPort ─► ring captura (2 s) ─┐
//!                                                ├─► spawn_dsp_thread ─► VoiceEvent
//! cpal output ◄─ OutputPort ◄─ ring TTS (2 s)    │
//!                    └──────► ring referencia (2 s)┘
//! ```
//!
//! Las tasas negociadas fijan los remuestreadores del DSP, así que cualquier
//! cambio de dispositivo cierra la sesión entera y abre otra (el DSP se
//! relanza de forma ordenada). Mutear cierra la sesión: el micrófono queda
//! libre y PipeWire deja de listar a Jarvis como cliente de grabación.
//!
//! # Dimensionado de los ring buffers
//!
//! Los tres rings guardan **2 s** a la tasa de su dispositivo (≈ 375 KiB a
//! 48 kHz mono `f32` cada uno):
//! - **Captura y referencia:** el hilo DSP los vacía cada ≤ 4 ms, pero
//!   comparte CPU con whisper.cpp (hasta 4 hilos a plena carga). 2 s cubren
//!   cualquier pausa del planificador sin perder voz; si aun así se llenan,
//!   las pérdidas se cuentan en [`AudioMetrics`] en vez de bloquear.
//! - **Reproducción (TTS):** el productor ([`Player::play`]) escribe por
//!   tramos y espera si la cola está llena, así que 2 s bastan para que una
//!   frase nueva empalme sin silencio (CA-8.3) y acotan lo que hay que
//!   descartar en un barge-in (el *fader* vacía la cola en 15 ms).
//!
//! # Detección de desconexión (CA-1.2)
//!
//! El closure de error de cada stream clasifica el error de cpal
//! ([`StreamFault::report`]): *xruns* y cambios de ruta solo se cuentan;
//! cualquier otro error marca el stream como caído. El supervisor revisa las
//! banderas cada 50 ms, cierra la sesión y reabre de inmediato; si el
//! dispositivo aún no existe reintenta con *backoff* exponencial
//! (0,5 s → 30 s). Con un dispositivo faltante desde el arranque (sin
//! micrófono, sin altavoz) la sesión queda parcial y solo se reabre cuando el
//! dispositivo aparece y no hay una locución en curso.

use std::any::Any;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::SampleFormat;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use ringbuf::traits::{Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use super::device::{negotiate_input, negotiate_output, select_input_device, select_output_device};
use super::ports::{InputPort, OutputPort, PortShared, StreamFault};
use super::{AudioError, AudioMetrics};
use crate::dsp::{
    spawn_dsp_thread, ConnectorSignal, DspHandle, DspMetrics, DspThreadConfig, MonoResampler,
    ProcessorConfig,
};
use crate::events::VoiceEvent;
use crate::playback::AudioPlayback;
use crate::tts::PcmAudio;
use crate::vad::EndpointConfig;

/// Segundos de audio que guarda cada ring buffer (ver doc del módulo).
pub const RING_SECONDS: usize = 2;

/// Formato negociado de un stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Stream abierto: soltarlo cierra el dispositivo.
pub struct OpenStream {
    pub format: StreamFormat,
    pub device: String,
    _guard: Box<dyn Any>,
}

impl OpenStream {
    pub fn new(format: StreamFormat, device: impl Into<String>, guard: impl Any) -> Self {
        Self {
            format,
            device: device.into(),
            _guard: Box::new(guard),
        }
    }
}

/// Fábrica de streams. La implementación real es [`CpalBackend`]; los tests
/// usan dobles para probar el supervisor sin hardware.
pub trait AudioBackend {
    /// `true` si hay un dispositivo de entrada utilizable ahora.
    fn has_input(&mut self) -> bool;
    /// `true` si hay un dispositivo de salida utilizable ahora.
    fn has_output(&mut self) -> bool;
    /// Abre la captura. `make_port` recibe el formato negociado y devuelve el
    /// estado que debe moverse al callback.
    fn open_input(
        &mut self,
        make_port: &mut dyn FnMut(StreamFormat) -> InputPort,
    ) -> Result<(OpenStream, StreamFault), AudioError>;
    /// Abre la reproducción.
    fn open_output(
        &mut self,
        make_port: &mut dyn FnMut(StreamFormat) -> OutputPort,
    ) -> Result<(OpenStream, StreamFault), AudioError>;
}

/// Backend real sobre el host por defecto de `cpal` (PipeWire/ALSA en Linux).
pub struct CpalBackend {
    host: cpal::Host,
    input_name: Option<String>,
    output_name: Option<String>,
}

impl CpalBackend {
    pub fn new(input_name: Option<String>, output_name: Option<String>) -> Self {
        Self {
            host: cpal::default_host(),
            input_name,
            output_name,
        }
    }
}

/// Nombre legible de un dispositivo.
pub fn device_label(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "desconocido".to_string())
}

impl AudioBackend for CpalBackend {
    fn has_input(&mut self) -> bool {
        select_input_device(&self.host, self.input_name.as_deref()).is_ok()
    }

    fn has_output(&mut self) -> bool {
        select_output_device(&self.host, self.output_name.as_deref()).is_ok()
    }

    fn open_input(
        &mut self,
        make_port: &mut dyn FnMut(StreamFormat) -> InputPort,
    ) -> Result<(OpenStream, StreamFault), AudioError> {
        let device = select_input_device(&self.host, self.input_name.as_deref())?;
        let negotiated = negotiate_input(&device)?;
        let format = StreamFormat {
            sample_rate: negotiated.config.sample_rate,
            channels: negotiated.config.channels,
        };
        if !matches!(negotiated.format, SampleFormat::F32 | SampleFormat::I16) {
            return Err(AudioError::UnsupportedFormat(negotiated.format));
        }
        let mut port = make_port(format);
        let fault = port.fault();
        let on_error = {
            let fault = fault.clone();
            move |e: cpal::Error| fault.report(e.kind())
        };
        let stream = match negotiated.format {
            SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
                negotiated.config,
                move |data: &[i16], info: &cpal::InputCallbackInfo| {
                    let ts = info.timestamp();
                    port.on_latency(ts.callback.duration_since(ts.capture));
                    port.on_i16(data);
                },
                on_error,
                None,
            ),
            _ => device.build_input_stream::<f32, _, _>(
                negotiated.config,
                move |data: &[f32], info: &cpal::InputCallbackInfo| {
                    let ts = info.timestamp();
                    port.on_latency(ts.callback.duration_since(ts.capture));
                    port.on_f32(data);
                },
                on_error,
                None,
            ),
        }
        .map_err(AudioError::Backend)?;
        stream.play().map_err(AudioError::Backend)?;
        Ok((
            OpenStream::new(format, device_label(&device), stream),
            fault,
        ))
    }

    fn open_output(
        &mut self,
        make_port: &mut dyn FnMut(StreamFormat) -> OutputPort,
    ) -> Result<(OpenStream, StreamFault), AudioError> {
        let device = select_output_device(&self.host, self.output_name.as_deref())?;
        let negotiated = negotiate_output(&device)?;
        let format = StreamFormat {
            sample_rate: negotiated.config.sample_rate,
            channels: negotiated.config.channels,
        };
        if !matches!(negotiated.format, SampleFormat::F32 | SampleFormat::I16) {
            return Err(AudioError::UnsupportedFormat(negotiated.format));
        }
        let mut port = make_port(format);
        let fault = port.fault();
        let on_error = {
            let fault = fault.clone();
            move |e: cpal::Error| fault.report(e.kind())
        };
        let stream = match negotiated.format {
            SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
                negotiated.config,
                move |out: &mut [i16], info: &cpal::OutputCallbackInfo| {
                    let ts = info.timestamp();
                    port.on_latency(ts.playback.duration_since(ts.callback));
                    port.fill_i16(out);
                },
                on_error,
                None,
            ),
            _ => device.build_output_stream::<f32, _, _>(
                negotiated.config,
                move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    let ts = info.timestamp();
                    port.on_latency(ts.playback.duration_since(ts.callback));
                    port.fill_f32(out);
                },
                on_error,
                None,
            ),
        }
        .map_err(AudioError::Backend)?;
        stream.play().map_err(AudioError::Backend)?;
        Ok((
            OpenStream::new(format, device_label(&device), stream),
            fault,
        ))
    }
}

/// Parámetros del motor.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub processor: ProcessorConfig,
    pub endpoint: EndpointConfig,
    /// Primer reintento tras perder o no encontrar un dispositivo.
    pub retry_initial: Duration,
    /// Techo del *backoff* exponencial.
    pub retry_max: Duration,
    /// Periodo de revisión de órdenes y fallos del supervisor.
    pub poll: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            processor: ProcessorConfig::default(),
            endpoint: EndpointConfig::default(),
            retry_initial: Duration::from_millis(500),
            retry_max: Duration::from_secs(30),
            poll: Duration::from_millis(50),
        }
    }
}

/// Descripción de un stream abierto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    pub device: String,
    pub format: StreamFormat,
}

/// Fotografía del estado del motor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineStatus {
    /// Captura pedida (no muteado).
    pub enabled: bool,
    pub input: Option<StreamInfo>,
    pub output: Option<StreamInfo>,
    pub dsp_running: bool,
}

impl EngineStatus {
    /// `true` si el micrófono está abierto y el DSP produce eventos de voz.
    pub fn is_listening(&self) -> bool {
        self.input.is_some() && self.dsp_running
    }
}

/// Productor de la cola de reproducción de la sesión vigente.
struct PlayerSink {
    producer: HeapProd<f32>,
    sample_rate: u32,
}

/// Estado del motor compartido con el resto del proceso.
pub struct EngineShared {
    ports: Arc<PortShared>,
    player: Mutex<Option<PlayerSink>>,
    connector: Mutex<Option<ConnectorSignal>>,
    dsp_metrics: Mutex<Option<Arc<DspMetrics>>>,
    status: Mutex<EngineStatus>,
    sessions_opened: AtomicU64,
    sessions_closed: AtomicU64,
    closing: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl EngineShared {
    fn new(playback: AudioPlayback) -> Self {
        Self {
            ports: Arc::new(PortShared::new(playback)),
            player: Mutex::new(None),
            connector: Mutex::new(None),
            dsp_metrics: Mutex::new(None),
            status: Mutex::new(EngineStatus::default()),
            sessions_opened: AtomicU64::new(0),
            sessions_closed: AtomicU64::new(0),
            closing: AtomicBool::new(false),
        }
    }

    pub fn metrics(&self) -> &Arc<AudioMetrics> {
        &self.ports.metrics
    }

    pub fn playback(&self) -> &AudioPlayback {
        &self.ports.playback
    }

    /// Nivel para la esfera: lo que suena si Jarvis habla, si no lo que oye.
    /// Dos lecturas atómicas: apto para el bucle de render.
    pub fn level(&self) -> f32 {
        self.ports
            .output_level
            .load()
            .max(self.ports.input_level.load())
    }

    pub fn status(&self) -> EngineStatus {
        lock(&self.status).clone()
    }

    /// Señal de conector del DSP vigente, si hay captura.
    pub fn connector(&self) -> Option<ConnectorSignal> {
        lock(&self.connector).clone()
    }

    /// Métricas del DSP vigente, si hay captura.
    pub fn dsp_metrics(&self) -> Option<Arc<DspMetrics>> {
        lock(&self.dsp_metrics).clone()
    }

    /// Sesiones abiertas y cerradas desde el arranque (diagnóstico y tests).
    pub fn session_counts(&self) -> (u64, u64) {
        (
            self.sessions_opened.load(Ordering::Acquire),
            self.sessions_closed.load(Ordering::Acquire),
        )
    }

    /// `true` si queda audio por sonar en la cola de reproducción.
    fn is_playing(&self) -> bool {
        lock(&self.player)
            .as_ref()
            .is_some_and(|sink| sink.producer.occupied_len() > 0)
    }
}

/// Resultado de encolar audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayOutcome {
    /// Todo el audio quedó en la cola (o terminó de sonar, en `wait_drained`).
    Played,
    /// La generación fue cancelada (barge-in) o el motor se apaga.
    Cancelled,
    /// No hay dispositivo de salida.
    NoOutput,
}

/// Escritor de la cola de reproducción, para el hilo TTS.
#[derive(Clone)]
pub struct Player {
    shared: Arc<EngineShared>,
}

/// Espera entre intentos cuando la cola está llena (≈ 10 ms de audio).
const PLAYER_BACKOFF: Duration = Duration::from_millis(10);

impl Player {
    /// Tasa del dispositivo de salida vigente.
    pub fn output_rate(&self) -> Option<u32> {
        lock(&self.shared.player).as_ref().map(|s| s.sample_rate)
    }

    /// Encola PCM mono a la tasa de salida. Espera mientras la cola está
    /// llena y abandona en cuanto `generation` queda obsoleta.
    pub fn play(&self, mut samples: &[f32], generation: u64) -> PlayOutcome {
        let playback = self.shared.playback();
        while !samples.is_empty() {
            if playback.is_stale(generation) || self.shared.closing.load(Ordering::Acquire) {
                return PlayOutcome::Cancelled;
            }
            let pushed = match lock(&self.shared.player).as_mut() {
                Some(sink) => sink.producer.push_slice(samples),
                None => return PlayOutcome::NoOutput,
            };
            samples = &samples[pushed..];
            if !samples.is_empty() {
                std::thread::sleep(PLAYER_BACKOFF);
            }
        }
        PlayOutcome::Played
    }

    /// Encola PCM mono a la tasa de salida sin asociarlo a una generación.
    ///
    /// Lo usan los *chimes* de confirmación (HU-18): no pertenecen a ninguna
    /// locución, así que no se descartan por barge-in. Solo se interrumpen al
    /// cerrar el motor.
    pub fn play_immediate(&self, mut samples: &[f32]) -> PlayOutcome {
        while !samples.is_empty() {
            if self.shared.closing.load(Ordering::Acquire) {
                return PlayOutcome::Cancelled;
            }
            let pushed = match lock(&self.shared.player).as_mut() {
                Some(sink) => sink.producer.push_slice(samples),
                None => return PlayOutcome::NoOutput,
            };
            samples = &samples[pushed..];
            if !samples.is_empty() {
                std::thread::sleep(PLAYER_BACKOFF);
            }
        }
        PlayOutcome::Played
    }

    /// Espera a que la cola se vacíe y el dispositivo reproduzca lo último.
    pub fn wait_drained(&self, generation: u64) -> PlayOutcome {
        let playback = self.shared.playback();
        loop {
            if playback.is_stale(generation) || self.shared.closing.load(Ordering::Acquire) {
                return PlayOutcome::Cancelled;
            }
            let pending = match lock(&self.shared.player).as_ref() {
                Some(sink) => sink.producer.occupied_len(),
                None => return PlayOutcome::NoOutput,
            };
            if pending == 0 {
                break;
            }
            std::thread::sleep(PLAYER_BACKOFF);
        }
        // Lo último ya salió de la cola pero sigue en el buffer del
        // dispositivo: la latencia de salida es lo que falta por sonar.
        let tail = self
            .shared
            .ports
            .latency
            .output_latency()
            .min(Duration::from_millis(250));
        std::thread::sleep(tail);
        if playback.is_stale(generation) {
            PlayOutcome::Cancelled
        } else {
            PlayOutcome::Played
        }
    }
}

/// Convierte PCM de la síntesis (cualquier tasa y canales) a mono a la tasa
/// de salida, compensando el retardo del filtro para no cortar el final.
pub fn to_output_pcm(pcm: &PcmAudio, output_rate: u32) -> Result<Vec<f32>, crate::DspError> {
    let channels = usize::from(pcm.channels.max(1));
    let mono: Vec<f32> = if channels == 1 {
        pcm.samples.clone()
    } else {
        let mut out = vec![0.0; pcm.samples.len() / channels];
        let n = super::callbacks::mix_down_to_mono(&pcm.samples, channels, &mut out);
        out.truncate(n);
        out
    };
    if pcm.sample_rate == output_rate {
        return Ok(mono);
    }
    let mut resampler = MonoResampler::new(pcm.sample_rate, output_rate)?;
    let delay = resampler.output_delay();
    let expected =
        (mono.len() as u64 * u64::from(output_rate)).div_ceil(u64::from(pcm.sample_rate)) as usize;
    let mut out = Vec::with_capacity(expected + delay + output_rate as usize / 50);
    resampler.process(&mono, |chunk| out.extend_from_slice(chunk))?;
    // Silencio de cola para vaciar el filtro (retardo + un tramo de 10 ms).
    let flush = vec![0.0; pcm.sample_rate as usize / 50 + delay * 2 + 1];
    resampler.process(&flush, |chunk| out.extend_from_slice(chunk))?;
    let start = delay.min(out.len());
    out.drain(..start);
    out.truncate(expected);
    Ok(out)
}

enum EngineCommand {
    SetEnabled(bool),
    Shutdown,
}

/// Motor de audio: hilo supervisor + streams + DSP.
pub struct AudioEngine {
    shared: Arc<EngineShared>,
    commands: Sender<EngineCommand>,
    join: Option<JoinHandle<()>>,
}

impl AudioEngine {
    /// Arranca el supervisor con el backend real de `cpal`. Empieza
    /// deshabilitado: la captura se abre con [`set_enabled`](Self::set_enabled).
    pub fn start_cpal(
        config: EngineConfig,
        input_device: Option<String>,
        output_device: Option<String>,
        playback: AudioPlayback,
        events: Sender<VoiceEvent>,
    ) -> Result<Self, AudioError> {
        Self::start(
            config,
            move || CpalBackend::new(input_device, output_device),
            playback,
            events,
        )
    }

    /// Arranca el supervisor con un backend arbitrario, construido dentro del
    /// propio hilo (los streams nunca cruzan de hilo).
    pub fn start<B, F>(
        config: EngineConfig,
        make_backend: F,
        playback: AudioPlayback,
        events: Sender<VoiceEvent>,
    ) -> Result<Self, AudioError>
    where
        B: AudioBackend + 'static,
        F: FnOnce() -> B + Send + 'static,
    {
        let shared = Arc::new(EngineShared::new(playback));
        let (tx, rx) = crossbeam_channel::unbounded();
        let thread_shared = Arc::clone(&shared);
        let join = std::thread::Builder::new()
            .name("jarvis-audio-io".into())
            .spawn(move || {
                let backend = make_backend();
                Supervisor::new(config, backend, thread_shared, events).run(&rx);
            })
            .map_err(AudioError::Spawn)?;
        Ok(Self {
            shared,
            commands: tx,
            join: Some(join),
        })
    }

    /// Abre (`true`) o libera (`false`) micrófono, altavoz y DSP.
    pub fn set_enabled(&self, enabled: bool) {
        let _ = self.commands.send(EngineCommand::SetEnabled(enabled));
    }

    /// Orden de habilitar/liberar clonable para otros hilos.
    pub fn enable_sender(&self) -> impl Fn(bool) + Send + Sync + 'static {
        let commands = self.commands.clone();
        move |enabled| {
            let _ = commands.send(EngineCommand::SetEnabled(enabled));
        }
    }

    pub fn shared(&self) -> &Arc<EngineShared> {
        &self.shared
    }

    pub fn player(&self) -> Player {
        Player {
            shared: Arc::clone(&self.shared),
        }
    }

    pub fn status(&self) -> EngineStatus {
        self.shared.status()
    }

    /// `true` mientras el hilo supervisor sigue vivo.
    pub fn is_running(&self) -> bool {
        self.join.as_ref().is_some_and(|j| !j.is_finished())
    }

    /// Cierra la sesión y espera al supervisor.
    pub fn shutdown(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
        let _ = self.commands.send(EngineCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Sesión abierta: streams + DSP.
struct Session {
    input: Option<(OpenStream, StreamFault)>,
    output: Option<(OpenStream, StreamFault)>,
    /// Consumidor de la referencia cuando no hay DSP que lo use.
    _render_idle: Option<HeapCons<f32>>,
    dsp: Option<DspHandle>,
}

impl Session {
    fn faulted(&self) -> Option<&'static str> {
        if self.input.as_ref().is_some_and(|(_, f)| f.is_raised()) {
            Some("entrada")
        } else if self.output.as_ref().is_some_and(|(_, f)| f.is_raised()) {
            Some("salida")
        } else if self.input.is_some() && !self.dsp.as_ref().is_some_and(DspHandle::is_running) {
            Some("DSP")
        } else {
            None
        }
    }

    fn complete(&self) -> bool {
        self.input.is_some() && self.output.is_some() && self.dsp.is_some()
    }
}

/// Avisos que se emiten una sola vez hasta que la condición cambia.
#[derive(Default)]
struct Notices {
    input_error: Option<String>,
    output_error: Option<String>,
}

impl Notices {
    fn report(slot: &mut Option<String>, what: &str, error: Option<String>) {
        match error {
            Some(e) => {
                if slot.as_deref() != Some(e.as_str()) {
                    eprintln!("[audio] sin {what}: {e} (se reintentará)");
                    *slot = Some(e);
                }
            }
            None => *slot = None,
        }
    }
}

struct Supervisor<B> {
    config: EngineConfig,
    backend: B,
    shared: Arc<EngineShared>,
    events: Sender<VoiceEvent>,
    session: Option<Session>,
    notices: Notices,
}

impl<B: AudioBackend> Supervisor<B> {
    fn new(
        config: EngineConfig,
        backend: B,
        shared: Arc<EngineShared>,
        events: Sender<VoiceEvent>,
    ) -> Self {
        Self {
            config,
            backend,
            shared,
            events,
            session: None,
            notices: Notices::default(),
        }
    }

    fn run(mut self, commands: &Receiver<EngineCommand>) {
        let mut enabled = false;
        let mut retry_at = Instant::now();
        let mut backoff = self.config.retry_initial;
        let mut lost_at: Option<Instant> = None;

        loop {
            match commands.recv_timeout(self.config.poll) {
                Ok(EngineCommand::SetEnabled(wanted)) => {
                    if wanted != enabled {
                        enabled = wanted;
                        lock(&self.shared.status).enabled = enabled;
                        retry_at = Instant::now();
                        backoff = self.config.retry_initial;
                        lost_at = None;
                    }
                }
                Ok(EngineCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let now = Instant::now();

            if let Some(what) = self.session.as_ref().and_then(Session::faulted) {
                eprintln!("[audio] se perdió el dispositivo de {what}; reconectando…");
                self.close_session();
                lost_at = Some(now);
                retry_at = now;
                backoff = self.config.retry_initial;
            }

            if !enabled {
                if self.session.is_some() {
                    self.close_session();
                    println!("[audio] micrófono y altavoz liberados.");
                }
                continue;
            }

            let complete = self.session.as_ref().is_some_and(Session::complete);
            if complete || now < retry_at {
                continue;
            }
            if let Some(session) = &self.session {
                // Sesión parcial: solo vale la pena reabrir si apareció lo que
                // faltaba, y nunca a mitad de una locución.
                let appeared = (session.input.is_none() && self.backend.has_input())
                    || (session.output.is_none() && self.backend.has_output())
                    || (session.input.is_some() && session.dsp.is_none());
                if !appeared || self.shared.is_playing() {
                    retry_at = now + backoff;
                    backoff = (backoff * 2).min(self.config.retry_max);
                    continue;
                }
                self.close_session();
            }

            self.open_session();
            if self.session.as_ref().is_some_and(Session::complete) {
                backoff = self.config.retry_initial;
                if let Some(lost) = lost_at.take() {
                    self.shared.metrics().add_reconnect();
                    println!("[audio] reconectado en {} ms.", lost.elapsed().as_millis());
                }
            } else {
                retry_at = now + backoff;
                backoff = (backoff * 2).min(self.config.retry_max);
            }
        }
        self.close_session();
    }

    fn open_session(&mut self) {
        let ports = Arc::clone(&self.shared.ports);

        // Salida primero: su tasa fija la referencia del AEC.
        let mut render_cons: Option<(HeapCons<f32>, u32)> = None;
        let mut player: Option<PlayerSink> = None;
        let output = self.backend.open_output(&mut |format: StreamFormat| {
            let capacity = ring_capacity(format.sample_rate);
            let (tts_prod, tts_cons) = HeapRb::<f32>::new(capacity).split();
            let (render_prod, render_c) = HeapRb::<f32>::new(capacity).split();
            render_cons = Some((render_c, format.sample_rate));
            player = Some(PlayerSink {
                producer: tts_prod,
                sample_rate: format.sample_rate,
            });
            OutputPort::new(
                tts_cons,
                render_prod,
                format.sample_rate,
                format.channels,
                Arc::clone(&ports),
            )
        });
        let output = match output {
            Ok(opened) => {
                Notices::report(&mut self.notices.output_error, "altavoz", None);
                Some(opened)
            }
            Err(e) => {
                Notices::report(
                    &mut self.notices.output_error,
                    "altavoz",
                    Some(e.to_string()),
                );
                render_cons = None;
                player = None;
                None
            }
        };

        let mut capture_cons: Option<(HeapCons<f32>, u32)> = None;
        let input = self.backend.open_input(&mut |format: StreamFormat| {
            let (prod, cons) = HeapRb::<f32>::new(ring_capacity(format.sample_rate)).split();
            capture_cons = Some((cons, format.sample_rate));
            InputPort::new(prod, format.channels, Arc::clone(&ports))
        });
        let mut input = match input {
            Ok(opened) => {
                Notices::report(&mut self.notices.input_error, "micrófono", None);
                Some(opened)
            }
            Err(e) => {
                Notices::report(
                    &mut self.notices.input_error,
                    "micrófono",
                    Some(e.to_string()),
                );
                None
            }
        };

        // DSP solo con captura. Sin salida, la referencia es un ring vacío.
        let mut dsp = None;
        let mut render_idle = None;
        match (input.is_some(), capture_cons) {
            (true, Some((capture, capture_rate))) => {
                let (render, render_rate) = match render_cons.take() {
                    Some(pair) => pair,
                    None => (HeapRb::<f32>::new(1).split().1, capture_rate),
                };
                let config = DspThreadConfig {
                    capture_rate,
                    render_rate,
                    processor: self.config.processor,
                    endpoint: self.config.endpoint.clone(),
                };
                match spawn_dsp_thread(
                    config,
                    capture,
                    render,
                    Arc::clone(&self.shared.ports.latency),
                    self.events.clone(),
                ) {
                    Ok(handle) => dsp = Some(handle),
                    Err(e) => {
                        eprintln!("[audio] no se pudo arrancar el DSP: {e}; micrófono cerrado");
                        input = None;
                    }
                }
            }
            _ => render_idle = render_cons.take().map(|(cons, _)| cons),
        }

        let tap = dsp.is_some() && output.is_some();
        self.shared.ports.render_tap.store(tap, Ordering::Release);
        *lock(&self.shared.player) = player;
        *lock(&self.shared.connector) = dsp.as_ref().map(DspHandle::connector_signal);
        *lock(&self.shared.dsp_metrics) = dsp.as_ref().map(|d| Arc::clone(d.metrics()));
        {
            let mut status = lock(&self.shared.status);
            status.input = input.as_ref().map(|(s, _)| StreamInfo {
                device: s.device.clone(),
                format: s.format,
            });
            status.output = output.as_ref().map(|(s, _)| StreamInfo {
                device: s.device.clone(),
                format: s.format,
            });
            status.dsp_running = dsp.is_some();
        }
        if let Some((s, _)) = &output {
            println!(
                "[audio] altavoz abierto: {} ({} Hz, {} canales).",
                s.device, s.format.sample_rate, s.format.channels
            );
        }
        if let Some((s, _)) = &input {
            println!(
                "[audio] micrófono abierto: {} ({} Hz, {} canales); DSP con AEC {}.",
                s.device,
                s.format.sample_rate,
                s.format.channels,
                if tap { "activo" } else { "sin referencia" }
            );
        }
        self.shared.sessions_opened.fetch_add(1, Ordering::AcqRel);
        self.session = Some(Session {
            input,
            output,
            _render_idle: render_idle,
            dsp,
        });
    }

    /// Cierra en orden: captura → DSP → salida → cola del TTS.
    fn close_session(&mut self) {
        let Some(mut session) = self.session.take() else {
            return;
        };
        self.shared.ports.render_tap.store(false, Ordering::Release);
        drop(session.input.take());
        if let Some(mut dsp) = session.dsp.take() {
            dsp.stop();
        }
        drop(session.output.take());
        *lock(&self.shared.player) = None;
        *lock(&self.shared.connector) = None;
        *lock(&self.shared.dsp_metrics) = None;
        self.shared.ports.input_level.store(0.0);
        self.shared.ports.output_level.store(0.0);
        {
            let mut status = lock(&self.shared.status);
            status.input = None;
            status.output = None;
            status.dsp_running = false;
        }
        self.shared.sessions_closed.fetch_add(1, Ordering::AcqRel);
    }
}

fn ring_capacity(sample_rate: u32) -> usize {
    (sample_rate as usize).max(8_000) * RING_SECONDS
}

#[cfg(test)]
mod tests;
