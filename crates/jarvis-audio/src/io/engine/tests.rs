//! Supervisor del motor con un backend doble (sin hardware).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;

/// Estado observable del backend doble.
#[derive(Default)]
struct FakeState {
    input_present: AtomicBool,
    output_present: AtomicBool,
    /// Streams abiertos que aún no se soltaron.
    live: AtomicUsize,
    input_opens: AtomicUsize,
    output_opens: AtomicUsize,
    input_port: Mutex<Option<InputPort>>,
    output_port: Mutex<Option<OutputPort>>,
    input_fault: Mutex<Option<StreamFault>>,
}

impl FakeState {
    fn new(input: bool, output: bool) -> Arc<Self> {
        let state = Self::default();
        state.input_present.store(input, Ordering::SeqCst);
        state.output_present.store(output, Ordering::SeqCst);
        Arc::new(state)
    }
}

/// Guard de un stream: al soltarlo, el "dispositivo" queda libre.
struct LiveGuard {
    state: Arc<FakeState>,
    input: bool,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.state.live.fetch_sub(1, Ordering::SeqCst);
        if self.input {
            *self.state.input_port.lock().unwrap() = None;
        } else {
            *self.state.output_port.lock().unwrap() = None;
        }
    }
}

struct FakeBackend(Arc<FakeState>);

const FORMAT: StreamFormat = StreamFormat {
    sample_rate: 48_000,
    channels: 2,
};

impl AudioBackend for FakeBackend {
    fn has_input(&mut self) -> bool {
        self.0.input_present.load(Ordering::SeqCst)
    }

    fn has_output(&mut self) -> bool {
        self.0.output_present.load(Ordering::SeqCst)
    }

    fn open_input(
        &mut self,
        make_port: &mut dyn FnMut(StreamFormat) -> InputPort,
    ) -> Result<(OpenStream, StreamFault), AudioError> {
        if !self.has_input() {
            return Err(AudioError::NoInputDevice);
        }
        let port = make_port(FORMAT);
        let fault = port.fault();
        *self.0.input_port.lock().unwrap() = Some(port);
        *self.0.input_fault.lock().unwrap() = Some(fault.clone());
        self.0.input_opens.fetch_add(1, Ordering::SeqCst);
        self.0.live.fetch_add(1, Ordering::SeqCst);
        let guard = LiveGuard {
            state: Arc::clone(&self.0),
            input: true,
        };
        Ok((OpenStream::new(FORMAT, "mic-doble", guard), fault))
    }

    fn open_output(
        &mut self,
        make_port: &mut dyn FnMut(StreamFormat) -> OutputPort,
    ) -> Result<(OpenStream, StreamFault), AudioError> {
        if !self.has_output() {
            return Err(AudioError::NoOutputDevice);
        }
        let port = make_port(FORMAT);
        let fault = port.fault();
        *self.0.output_port.lock().unwrap() = Some(port);
        self.0.output_opens.fetch_add(1, Ordering::SeqCst);
        self.0.live.fetch_add(1, Ordering::SeqCst);
        let guard = LiveGuard {
            state: Arc::clone(&self.0),
            input: false,
        };
        Ok((OpenStream::new(FORMAT, "altavoz-doble", guard), fault))
    }
}

fn fast_config() -> EngineConfig {
    EngineConfig {
        retry_initial: Duration::from_millis(20),
        retry_max: Duration::from_millis(80),
        poll: Duration::from_millis(5),
        ..EngineConfig::default()
    }
}

fn start(state: &Arc<FakeState>) -> (AudioEngine, crossbeam_channel::Receiver<VoiceEvent>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    let backend_state = Arc::clone(state);
    let engine = AudioEngine::start(
        fast_config(),
        move || FakeBackend(backend_state),
        AudioPlayback::new(),
        tx,
    )
    .expect("el supervisor debe arrancar");
    (engine, rx)
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < deadline, "timeout esperando: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn starts_disabled_and_opens_a_complete_session_when_enabled() {
    let state = FakeState::new(true, true);
    let (engine, _rx) = start(&state);
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(
        state.live.load(Ordering::SeqCst),
        0,
        "deshabilitado no abre"
    );

    engine.set_enabled(true);
    wait_until("sesión completa", || engine.status().is_listening());
    let status = engine.status();
    assert!(status.enabled);
    assert_eq!(status.input.unwrap().device, "mic-doble");
    assert_eq!(status.output.unwrap().format, FORMAT);
    assert_eq!(state.live.load(Ordering::SeqCst), 2);
    assert!(engine.shared().connector().is_some());
}

#[test]
fn mute_cycles_release_every_stream_and_dsp_thread() {
    let state = FakeState::new(true, true);
    let (mut engine, _rx) = start(&state);
    for cycle in 0..15 {
        engine.set_enabled(true);
        wait_until("abrir", || engine.status().is_listening());
        engine.set_enabled(false);
        wait_until("liberar", || {
            state.live.load(Ordering::SeqCst) == 0 && !engine.status().dsp_running
        });
        let (opened, closed) = engine.shared().session_counts();
        assert_eq!(opened, closed, "ciclo {cycle}: sesión sin cerrar");
    }
    assert_eq!(state.input_opens.load(Ordering::SeqCst), 15);
    assert!(engine.shared().connector().is_none());
    assert!(engine.shared().dsp_metrics().is_none());

    engine.shutdown();
    assert!(!engine.is_running());
    assert_eq!(state.live.load(Ordering::SeqCst), 0);
}

#[test]
fn shutdown_closes_an_open_session() {
    let state = FakeState::new(true, true);
    let (mut engine, _rx) = start(&state);
    engine.set_enabled(true);
    wait_until("abrir", || engine.status().is_listening());
    let started = Instant::now();
    engine.shutdown();
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(state.live.load(Ordering::SeqCst), 0);
    let (opened, closed) = engine.shared().session_counts();
    assert_eq!(opened, closed);
}

#[test]
fn a_stream_fault_reopens_the_session_and_counts_a_reconnect() {
    let state = FakeState::new(true, true);
    let (engine, _rx) = start(&state);
    engine.set_enabled(true);
    wait_until("abrir", || engine.status().is_listening());

    state
        .input_fault
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .report(cpal::ErrorKind::DeviceNotAvailable);
    wait_until("reconexión", || {
        engine.shared().metrics().snapshot().reconnects == 1
    });
    let metrics = engine.shared().metrics().snapshot();
    assert_eq!(metrics.disconnects, 1);
    assert_eq!(state.input_opens.load(Ordering::SeqCst), 2);
    assert_eq!(state.live.load(Ordering::SeqCst), 2);
    assert!(engine.status().is_listening());
}

#[test]
fn xruns_are_counted_without_reopening() {
    let state = FakeState::new(true, true);
    let (engine, _rx) = start(&state);
    engine.set_enabled(true);
    wait_until("abrir", || engine.status().is_listening());
    let fault = state.input_fault.lock().unwrap().clone().unwrap();
    fault.report(cpal::ErrorKind::Xrun);
    fault.report(cpal::ErrorKind::DeviceChanged);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(engine.shared().metrics().snapshot().xruns, 1);
    assert_eq!(state.input_opens.load(Ordering::SeqCst), 1);
}

#[test]
fn missing_microphone_keeps_output_and_recovers_when_it_appears() {
    let state = FakeState::new(false, true);
    let (engine, _rx) = start(&state);
    engine.set_enabled(true);
    wait_until("salida sola", || engine.status().output.is_some());
    let status = engine.status();
    assert!(status.input.is_none());
    assert!(!status.is_listening());
    assert!(engine.player().output_rate().is_some());

    state.input_present.store(true, Ordering::SeqCst);
    wait_until("micrófono recuperado", || engine.status().is_listening());
    assert_eq!(state.live.load(Ordering::SeqCst), 2);
}

#[test]
fn missing_speaker_still_listens_without_aec_reference() {
    let state = FakeState::new(true, false);
    let (engine, _rx) = start(&state);
    engine.set_enabled(true);
    wait_until("captura sin salida", || engine.status().is_listening());
    assert!(engine.status().output.is_none());
    assert_eq!(engine.player().output_rate(), None);
    assert_eq!(engine.player().play(&[0.1; 16], 0), PlayOutcome::NoOutput);
}

#[test]
fn captured_audio_reaches_the_dsp_thread() {
    let state = FakeState::new(true, true);
    let (engine, _rx) = start(&state);
    engine.set_enabled(true);
    wait_until("abrir", || engine.status().is_listening());

    // 100 ms estéreo a 48 kHz.
    let block: Vec<f32> = (0..9_600).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
    state
        .input_port
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .on_f32(&block);
    let metrics = engine.shared().dsp_metrics().unwrap();
    wait_until("frames en el DSP", || {
        metrics.snapshot().capture_frames >= 5
    });
    assert!(engine.shared().level() > 0.0, "nivel real del micrófono");
}

#[test]
fn player_feeds_the_output_and_barge_in_cancels_it() {
    let state = FakeState::new(true, true);
    let (engine, _rx) = start(&state);
    engine.set_enabled(true);
    wait_until("abrir", || engine.status().is_listening());
    let player = engine.player();
    let playback = engine.shared().playback().clone();
    let generation = playback.begin_utterance();

    assert_eq!(player.play(&[0.5; 4_800], generation), PlayOutcome::Played);
    let mut out = vec![0.0f32; 9_600];
    state
        .output_port
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .fill_f32(&mut out);
    assert!(out.iter().all(|s| (*s - 0.5).abs() < 1e-6));
    assert!(engine.shared().level() > 0.4, "nivel de lo que sonó");
    assert_eq!(player.wait_drained(generation), PlayOutcome::Played);

    // Cola llena (> 2 s) y barge-in desde otro hilo: `play` abandona.
    let stopper = {
        let playback = playback.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            playback.stop();
        })
    };
    let long = vec![0.5f32; 48_000 * 3];
    assert_eq!(player.play(&long, generation), PlayOutcome::Cancelled);
    stopper.join().unwrap();

    // El callback descarta la cola de la generación cancelada.
    let mut out = vec![1.0f32; 4_096];
    let mut guard = state.output_port.lock().unwrap();
    let port = guard.as_mut().unwrap();
    for _ in 0..4 {
        port.fill_f32(&mut out);
    }
    assert!(out.iter().all(|s| *s == 0.0));
    drop(guard);
    assert!(!engine.shared().is_playing());
}

#[test]
fn synthesis_pcm_is_converted_to_mono_at_the_output_rate() {
    let mono = PcmAudio::new(vec![0.25; 24_000], 24_000, 1);
    let out = to_output_pcm(&mono, 48_000).unwrap();
    assert_eq!(out.len(), 48_000);
    // Lejos de los bordes del filtro, la continua se conserva.
    assert!((out[24_000] - 0.25).abs() < 0.02);

    let stereo = PcmAudio::new(vec![0.5, -0.5, 0.5, -0.5], 48_000, 2);
    assert_eq!(to_output_pcm(&stereo, 48_000).unwrap(), vec![0.0, 0.0]);
}

#[test]
fn pause_keeps_the_queue_and_stop_during_pause_discards_it_silently() {
    let state = FakeState::new(true, true);
    let (engine, _rx) = start(&state);
    engine.set_enabled(true);
    wait_until("abrir", || engine.status().is_listening());
    let player = engine.player();
    let playback = engine.shared().playback().clone();
    let generation = playback.begin_utterance();
    assert_eq!(player.play(&[0.5; 4_800], generation), PlayOutcome::Played);

    let fill = |out: &mut [f32]| {
        state
            .output_port
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .fill_f32(out)
    };
    // Pausa: silencio, la cola no se consume.
    playback.pause();
    let mut out = vec![1.0f32; 4_800];
    fill(&mut out);
    assert!(out.iter().all(|s| *s == 0.0));
    assert!(engine.shared().is_playing(), "la cola sigue intacta");

    // Reanudar: suena lo que estaba en cola.
    playback.resume();
    fill(&mut out);
    assert!(out.iter().all(|s| (*s - 0.5).abs() < 1e-6));

    // Barge-in confirmado durante la pausa: ni la rampa llega a sonar.
    let generation = playback.begin_utterance();
    assert_eq!(player.play(&[0.5; 4_800], generation), PlayOutcome::Played);
    playback.pause();
    playback.stop();
    fill(&mut out);
    assert!(out.iter().all(|s| *s == 0.0));
    assert!(!engine.shared().is_playing(), "cola cancelada descartada");

    // La próxima locución suena entera, sin rampa heredada.
    let generation = playback.begin_utterance();
    assert_eq!(player.play(&[0.25; 2_400], generation), PlayOutcome::Played);
    let mut out = vec![0.0f32; 2_400];
    fill(&mut out);
    assert!(out.iter().all(|s| (*s - 0.25).abs() < 1e-6));
}
