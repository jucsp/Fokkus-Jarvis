//! Integración del orquestador con dobles de captura, salida, STT y LLM.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jarvis_core::{JarvisConfig, JarvisCore, JarvisState, SharedCore};

use super::*;
use crate::events::Segment;
use crate::stt::SttError;
use crate::tts::{EdgeTtsEngine, FillerCategory};

// ------------------------------------------------------------------ dobles

struct FakeCapture {
    enabled: AtomicBool,
    mic: AtomicBool,
    connector: ConnectorSignal,
    /// `f32::to_bits` del puntaje del VAD que "ve" el DSP.
    vad: std::sync::atomic::AtomicU32,
}

impl FakeCapture {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            enabled: AtomicBool::new(false),
            mic: AtomicBool::new(true),
            connector: ConnectorSignal::detached(),
            vad: std::sync::atomic::AtomicU32::new(0),
        })
    }

    fn set_vad(&self, score: f32) {
        self.vad.store(score.to_bits(), Ordering::SeqCst);
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }
}

impl CaptureControl for FakeCapture {
    fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    fn is_listening(&self) -> bool {
        self.enabled() && self.mic.load(Ordering::SeqCst)
    }

    fn has_output(&self) -> bool {
        self.enabled()
    }

    fn connector(&self) -> Option<ConnectorSignal> {
        self.enabled().then(|| self.connector.clone())
    }

    fn vad_score(&self) -> Option<f32> {
        self.enabled()
            .then(|| f32::from_bits(self.vad.load(Ordering::SeqCst)))
    }
}

/// Salida que registra lo encolado y tarda `drain` en "reproducirlo".
struct FakeSink {
    playback: AudioPlayback,
    drain: Duration,
    played: Mutex<Vec<(u64, usize)>>,
    drained: Mutex<Vec<u64>>,
}

const SINK_RATE: u32 = 16_000;

impl AudioSink for FakeSink {
    fn output_rate(&self) -> Option<u32> {
        Some(SINK_RATE)
    }

    fn play(&self, samples: &[f32], generation: u64) -> PlayOutcome {
        if self.playback.is_stale(generation) {
            return PlayOutcome::Cancelled;
        }
        self.played
            .lock()
            .unwrap()
            .push((generation, samples.len()));
        PlayOutcome::Played
    }

    fn play_immediate(&self, _samples: &[f32]) -> PlayOutcome {
        PlayOutcome::Played
    }

    fn wait_drained(&self, generation: u64) -> PlayOutcome {
        let until = Instant::now() + self.drain;
        while Instant::now() < until {
            if self.playback.is_stale(generation) {
                return PlayOutcome::Cancelled;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        self.drained.lock().unwrap().push(generation);
        PlayOutcome::Played
    }
}

/// Guion de transcripciones: el primer valor del PCM indica la línea.
const SCRIPT: &[&str] = &[
    "esto no va contigo",                                    // 0: sin palabra clave
    "Jarvis, ¿qué tal el clima?",                            // 1
    "¿y mañana?",                                            // 2: seguimiento
    "cuéntame otra cosa",                                    // 3: tras barge-in
    "Jarvis, qué hora es",                                   // 4: local
    "Jarvis, deja de escuchar",                              // 5: silencio por voz
    "Jarvis, busca el archivo y",                            // 6: conector
    "ábrelo",                                                // 7: continuación
    "Jarvis, abre el menú y",                                // 8: conector inicial
    "ajústalo y",                                            // 9: continuación también conector
    "guárdalo",                                              // 10: cierre
    "Jarvis, ábreme Spotify",                                // 11: abrir aplicación
    "Jarvis, cierra Spotify",                                // 12: cerrar aplicación
    "Jarvis, revisa mis correos y dime si hay algo urgente", // 13: tarea pesada
    "Jarvis, cancela todas las tareas",                      // 14
    "Jarvis, qué tareas tengo",                              // 15
];

struct ScriptTranscriber;

impl Transcriber for ScriptTranscriber {
    fn transcribe(&self, pcm: &[f32]) -> Result<String, SttError> {
        Ok(SCRIPT[pcm[0] as usize].to_string())
    }
}

/// LLM falso: emite `tokens` con `delay` entre cada uno.
#[derive(Default)]
struct FakeBrain {
    tokens: Mutex<Vec<String>>,
    delay: Mutex<Duration>,
    fail_network: AtomicBool,
    cancelled: AtomicBool,
    queries: Mutex<Vec<String>>,
}

impl FakeBrain {
    fn new(reply: &str) -> Arc<Self> {
        let brain = Self::default();
        brain.set_reply(reply, Duration::ZERO);
        Arc::new(brain)
    }

    fn set_reply(&self, reply: &str, delay: Duration) {
        *self.tokens.lock().unwrap() = reply.split_inclusive(' ').map(String::from).collect();
        *self.delay.lock().unwrap() = delay;
    }

    fn queries(&self) -> Vec<String> {
        self.queries.lock().unwrap().clone()
    }
}

impl TokenStream for FakeBrain {
    fn stream_tokens(
        &self,
        user_text: &str,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError> {
        // Como `ClaudeClient::stream`: cada turno arranca sin cancelación.
        self.cancelled.store(false, Ordering::SeqCst);
        self.queries.lock().unwrap().push(user_text.to_string());
        if self.fail_network.load(Ordering::SeqCst) {
            return Err(BrainError::Network("red caída".into()));
        }
        let tokens = self.tokens.lock().unwrap().clone();
        let delay = *self.delay.lock().unwrap();
        for token in &tokens {
            // Espera cancelable (como matar el árbol del CLI a mitad).
            let until = Instant::now() + delay;
            loop {
                if self.cancelled.load(Ordering::SeqCst) {
                    return Err(BrainError::Cancelled);
                }
                let now = Instant::now();
                if now >= until {
                    break;
                }
                std::thread::sleep((until - now).min(Duration::from_millis(2)));
            }
            on_token(token);
        }
        Ok(Completion {
            text: tokens.concat(),
            ..Completion::default()
        })
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

// ----------------------------------------------------------------- arnés

struct Harness {
    orchestrator: VoiceOrchestrator,
    core: SharedCore,
    events: Sender<VoiceEvent>,
    capture: Arc<FakeCapture>,
    sink: Arc<FakeSink>,
    activation: Arc<ActivationDetector>,
    playback: AudioPlayback,
    cache: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.cache);
    }
}

/// WAV PCM 16 bits mono de `samples` muestras a 16 kHz.
fn wav(samples: usize) -> Vec<u8> {
    let data_len = (samples * 2) as u32;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&16_000u32.to_le_bytes());
    wav.extend_from_slice(&32_000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for _ in 0..samples {
        wav.extend_from_slice(&1_000i16.to_le_bytes());
    }
    wav
}

/// Muestras del filler de prueba (distinto del largo de cualquier frase).
const FILLER_SAMPLES: usize = 12_345;

fn harness(name: &str, brain: Option<Arc<FakeBrain>>, config: VoiceConfig) -> Harness {
    harness_full(name, brain, config, Duration::ZERO, None)
}

fn harness_with_synth_delay(
    name: &str,
    brain: Option<Arc<FakeBrain>>,
    config: VoiceConfig,
    synth_delay: Duration,
) -> Harness {
    harness_full(name, brain, config, synth_delay, None)
}

fn harness_full(
    name: &str,
    brain: Option<Arc<FakeBrain>>,
    config: VoiceConfig,
    synth_delay: Duration,
    factory: Option<Arc<dyn BrainFactory>>,
) -> Harness {
    let cache = std::env::temp_dir().join(format!("jarvis-orq-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache);
    // Con espeak como backend la caché de fillers usa `.wav`.
    let tts = EdgeTtsEngine::new()
        .with_cache_dir(cache.clone())
        .with_edge_tts_binary(None)
        .with_espeak_binary(Some(PathBuf::from("/bin/espeak-ng")));
    let filler = tts.filler_cache_path(FillerCategory::General.phrases()[0]);
    std::fs::create_dir_all(filler.parent().unwrap()).unwrap();
    std::fs::write(&filler, wav(FILLER_SAMPLES)).unwrap();

    let core = SharedCore::new(JarvisCore::new(JarvisConfig::default()));
    let playback = AudioPlayback::new();
    let coordinator =
        ConversationCoordinator::with_shared_core(tts, core.clone(), playback.clone())
            .with_synthesizer(move |text: &str| {
                std::thread::sleep(synth_delay);
                Ok(PcmAudio::new(vec![0.2; 100 * text.len()], SINK_RATE, 1))
            });
    let capture = FakeCapture::new();
    let sink = Arc::new(FakeSink {
        playback: playback.clone(),
        drain: Duration::from_millis(150),
        played: Mutex::new(Vec::new()),
        drained: Mutex::new(Vec::new()),
    });
    let activation = Arc::new(ActivationDetector::new());
    let (events_tx, events_rx) = crossbeam_channel::unbounded();
    let deps = VoiceDeps {
        core: core.clone(),
        playback: playback.clone(),
        capture: capture.clone(),
        sink: sink.clone(),
        events: events_rx,
        coordinator,
        brain: brain.map(|b| b as Arc<dyn TokenStream + Send + Sync>),
        activation: Arc::clone(&activation),
        factory,
    };
    let orchestrator = VoiceOrchestrator::start(deps, config).expect("hilos");
    orchestrator.set_transcriber(Box::new(ScriptTranscriber));
    let h = Harness {
        orchestrator,
        core,
        events: events_tx,
        capture,
        sink,
        activation,
        playback,
        cache,
    };
    h.wait_state(JarvisState::Listening);
    h
}

fn fast() -> VoiceConfig {
    VoiceConfig {
        connector_grace: Duration::from_millis(60),
        poll: Duration::from_millis(5),
        barge_in_enabled: true,
        ..VoiceConfig::default()
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < deadline, "timeout esperando: {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

impl Harness {
    fn say_line(&self, line: usize) {
        self.say_line_with_duration(line, 100);
    }

    fn say_line_with_duration(&self, line: usize, duration_ms: u32) {
        self.events
            .send(VoiceEvent::EndOfTurn {
                at: Instant::now(),
                segment: Segment {
                    pcm: vec![line as f32; 1_600],
                    duration_ms,
                },
            })
            .unwrap();
    }

    fn voice_start(&self) {
        self.events
            .send(VoiceEvent::VoiceStart { at: Instant::now() })
            .unwrap();
    }

    fn wait_state(&self, state: JarvisState) {
        wait_until(&format!("estado {state:?}"), || self.core.state() == state);
    }

    fn stats(&self) -> VoiceStatsSnapshot {
        self.orchestrator.stats()
    }

    fn played(&self) -> Vec<(u64, usize)> {
        self.sink.played.lock().unwrap().clone()
    }

    /// Espera a que se reproduzca algo nuevo y la esfera vuelva a escuchar.
    fn wait_spoken_and_listening(&self, played_before: usize, drained_before: usize) {
        wait_until("audio reproducido", || self.played().len() > played_before);
        wait_until("locución terminada", || {
            self.sink.drained.lock().unwrap().len() > drained_before
        });
        self.wait_state(JarvisState::Listening);
    }

    fn drained(&self) -> usize {
        self.sink.drained.lock().unwrap().len()
    }

    fn task_status(&self) -> Vec<crate::tasks::TaskStatus> {
        let tasks = self.orchestrator.tasks().expect("gestor de tareas");
        tasks.list().into_iter().map(|r| r.status).collect()
    }
}

/// Fábrica de cerebros de tarea: cada uno es un `FakeBrain` nuevo con la
/// respuesta y latencia indicadas (HU-37).
struct FakeFactory {
    reply: String,
    delay: Duration,
    spawned: Mutex<Vec<Arc<FakeBrain>>>,
}

impl FakeFactory {
    fn new(reply: &str, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            reply: reply.to_string(),
            delay,
            spawned: Mutex::new(Vec::new()),
        })
    }
}

impl BrainFactory for FakeFactory {
    fn spawn(&self, _timeout: Duration) -> Result<Arc<dyn TokenStream + Send + Sync>, BrainError> {
        let brain = Arc::new(FakeBrain::default());
        // Dos tokens: la latencia total ≈ 2 × delay.
        let half = self
            .reply
            .char_indices()
            .map(|(i, _)| i)
            .find(|i| *i >= self.reply.len() / 2)
            .unwrap_or(0);
        let (a, b) = self.reply.split_at(half);
        *brain.tokens.lock().unwrap() = vec![a.to_string(), b.to_string()];
        *brain.delay.lock().unwrap() = self.delay;
        self.spawned.lock().unwrap().push(Arc::clone(&brain));
        Ok(brain)
    }
}

fn tasks_config(name: &str) -> VoiceConfig {
    VoiceConfig {
        tasks: Some(crate::tasks::TaskConfig {
            max_parallel: 2,
            timeout: Duration::from_secs(30),
            quiet_gap: Duration::ZERO,
            dir: std::env::temp_dir()
                .join(format!("jarvis-orq-tasks-{name}-{}", std::process::id())),
        }),
        ..fast()
    }
}

fn task_harness(name: &str, brain: Arc<FakeBrain>, factory: Arc<FakeFactory>) -> Harness {
    harness_full(
        name,
        Some(brain),
        tasks_config(name),
        Duration::ZERO,
        Some(factory as Arc<dyn BrainFactory>),
    )
}

// ----------------------------------------------------------------- tests

#[test]
fn wake_word_reply_barge_in_and_follow_up_question() {
    let brain = FakeBrain::new("Hoy está despejado. Máxima de veinte grados.");
    let h = harness("flujo", Some(brain.clone()), fast());
    assert!(h.capture.enabled(), "el micrófono se abre al arrancar");

    // Sin "Jarvis" y sin ventana: no llega al LLM (CA-4.2).
    h.say_line(0);
    wait_until("rechazo", || h.stats().rejected == 1);
    assert!(brain.queries().is_empty());
    assert_eq!(h.core.state(), JarvisState::Listening);

    // "Jarvis, ¿qué tal el clima?": chime + respuesta + vuelta a escuchar.
    h.say_line(1);
    h.wait_spoken_and_listening(0, 0);
    assert_eq!(brain.queries(), vec!["¿qué tal el clima?".to_string()]);
    let played = h.played();
    assert!(
        played.len() >= 2,
        "las dos frases de la respuesta: {played:?}"
    );
    assert!(
        played.iter().all(|(g, _)| *g == played[0].0),
        "una sola generación por turno"
    );
    assert!(
        h.activation.is_follow_up_open(),
        "ventana de seguimiento abierta"
    );

    // Pregunta de seguimiento sin "Jarvis", respuesta lenta e interrupción.
    brain.set_reply(
        "Mañana llueve por la mañana. Luego despeja. Y por la noche hiela. Fin.",
        Duration::from_millis(30),
    );
    h.say_line(2);
    h.wait_state(JarvisState::Speaking);
    let spoken_gen = h.playback.generation();
    // El usuario habla y sigue hablando con Jarvis en pausa.
    h.capture.set_vad(0.9);
    let started = Instant::now();
    h.voice_start();
    wait_until("pausa", || h.playback.is_paused());
    let silence = started.elapsed();
    assert!(
        silence < Duration::from_millis(50),
        "silencio tardó {silence:?}"
    );
    h.wait_state(JarvisState::Listening);
    h.capture.set_vad(0.0);
    assert!(h.stats().last_barge_in_us < 50_000);
    assert!(
        h.playback.is_stale(spoken_gen),
        "la locución quedó cancelada"
    );
    wait_until("conteo de barge-in", || h.stats().barge_ins == 1);
    wait_until("LLM cancelado", || brain.cancelled.load(Ordering::SeqCst));

    // Lo que dice tras interrumpir se acepta sin repetir "Jarvis".
    brain.set_reply("Claro, otra cosa.", Duration::ZERO);
    let (played_before, drained_before) = (h.played().len(), h.drained());
    h.say_line(3);
    h.wait_spoken_and_listening(played_before, drained_before);
    assert_eq!(brain.queries().len(), 3);
    assert_eq!(brain.queries()[2], "cuéntame otra cosa");
    assert_eq!(h.stats().turns, 3);
    assert_eq!(h.stats().speech_errors, 0);
}

#[test]
fn twenty_follow_up_turns_leave_a_consistent_state() {
    let brain = FakeBrain::new("Hecho. Listo.");
    let h = harness("estres", Some(brain.clone()), fast());
    h.say_line(1);
    h.wait_spoken_and_listening(0, 0);
    for i in 0..20 {
        let (played, drained) = (h.played().len(), h.drained());
        h.say_line(2 + i % 2);
        h.wait_spoken_and_listening(played, drained);
        assert_eq!(h.core.state(), JarvisState::Listening, "turno {i}");
    }
    assert_eq!(brain.queries().len(), 21);
    assert_eq!(h.stats().turns, 21);
}

#[test]
fn mute_from_any_path_releases_the_microphone_and_unmute_restores_it() {
    let brain = FakeBrain::new("Respuesta.");
    let h = harness("mute", Some(brain.clone()), fast());

    // Como lo haría el menú de la esfera o `jarvis.setMuted` por IPC.
    h.core.transition(JarvisState::Muted).unwrap();
    wait_until("micrófono liberado", || !h.capture.enabled());
    assert!(h.activation.is_muted());

    h.say_line(1);
    std::thread::sleep(Duration::from_millis(60));
    assert!(brain.queries().is_empty(), "en silencio nada llega al LLM");
    assert_eq!(h.core.state(), JarvisState::Muted);

    h.core.transition(JarvisState::Idle).unwrap();
    wait_until("micrófono reabierto", || h.capture.enabled());
    h.wait_state(JarvisState::Listening);
    assert!(!h.activation.is_muted());
}

#[test]
fn sphere_does_not_claim_to_listen_without_a_microphone() {
    let h = harness("sin-mic", None, fast());
    h.capture.mic.store(false, Ordering::SeqCst);
    h.wait_state(JarvisState::Idle);
    h.capture.mic.store(true, Ordering::SeqCst);
    h.wait_state(JarvisState::Listening);
}

#[test]
fn local_commands_work_without_api_key_and_standby_closes_the_window() {
    let h = harness("local", None, fast());

    h.say_line(4); // "Jarvis, qué hora es"
    h.wait_spoken_and_listening(0, 0);
    assert_eq!(h.stats().local_commands, 1);

    // Sin API key, una consulta general recibe el aviso hablado.
    let (played, drained) = (h.played().len(), h.drained());
    h.say_line(1);
    h.wait_spoken_and_listening(played, drained);
    assert_eq!(h.stats().turns, 0);

    // "Jarvis, deja de escuchar": reposo, no un silencio duro (HU-17). La
    // ventana de seguimiento se cierra y el micrófono sigue abierto; decir
    // "Jarvis" vuelve a abrir turno.
    h.say_line(5);
    wait_until("comando de reposo", || h.stats().local_commands == 2);
    h.wait_state(JarvisState::Listening);
    assert!(h.capture.enabled(), "el micrófono sigue abierto en reposo");
    assert!(!h.activation.is_muted(), "el reposo no mutea el detector");
    assert!(
        !h.activation.is_follow_up_open(),
        "la ventana de seguimiento se cierra"
    );
}

#[test]
fn connector_holds_the_turn_and_merges_the_continuation() {
    let brain = FakeBrain::new("Abierto.");
    let h = harness("conector", Some(brain.clone()), fast());

    h.say_line(6); // "Jarvis, busca el archivo y"
    wait_until("transcripción", || h.stats().segments == 1);
    std::thread::sleep(Duration::from_millis(20));
    h.voice_start();
    wait_until("señal al DSP", || h.capture.connector.is_pending());
    h.say_line(7); // "ábrelo"
    h.wait_spoken_and_listening(0, 0);
    assert_eq!(
        brain.queries(),
        vec!["busca el archivo y ábrelo".to_string()]
    );
}

#[test]
fn connector_chains_when_the_continuation_also_ends_in_connector() {
    let brain = FakeBrain::new("Hecho.");
    let config = VoiceConfig {
        connector_grace: Duration::from_millis(500),
        ..fast()
    };
    let h = harness("conector-encadenado", Some(brain.clone()), config);

    h.say_line(8); // "Jarvis, abre el menú y"
    wait_until("transcripción", || h.stats().segments == 1);
    std::thread::sleep(Duration::from_millis(20));
    h.voice_start();
    h.say_line(9); // "ajústalo y" (también conector)
    wait_until("transcripción", || h.stats().segments == 2);
    std::thread::sleep(Duration::from_millis(20));
    h.say_line(10); // "guárdalo"
    h.wait_spoken_and_listening(0, 0);
    assert_eq!(
        brain.queries(),
        vec!["abre el menú y ajústalo y guárdalo".to_string()]
    );
}

#[test]
fn connector_without_continuation_is_sent_after_the_grace_period() {
    let brain = FakeBrain::new("Buscando.");
    let h = harness("conector-solo", Some(brain.clone()), fast());
    h.say_line(6);
    h.wait_spoken_and_listening(0, 0);
    assert_eq!(brain.queries(), vec!["busca el archivo y".to_string()]);
}

#[test]
fn network_failure_mid_turn_is_announced_and_returns_to_listening() {
    let brain = FakeBrain::new("No importa.");
    brain.fail_network.store(true, Ordering::SeqCst);
    let h = harness("red", Some(brain.clone()), fast());
    let started = Instant::now();
    h.say_line(1);
    h.wait_spoken_and_listening(0, 0);
    assert!(started.elapsed() < Duration::from_secs(2), "CA-6.4");
    assert_eq!(h.stats().speech_errors, 1);
    // Solo el aviso de red (el chime se reproduce por la vía inmediata).
    assert!(!h.played().is_empty());
}

#[test]
fn ipc_speech_is_spoken_and_queued_behind_the_current_one() {
    let h = harness("ipc", None, fast());
    let speaker = h.orchestrator.speaker();
    speaker.say("Tarea del swarm completada.");
    speaker.say("Segunda notificación.");
    wait_until("dos locuciones", || h.drained() == 2);
    h.wait_state(JarvisState::Listening);
    assert_eq!(h.played().len(), 2);
}

#[test]
fn shutdown_is_fast_and_releases_the_microphone() {
    let brain = FakeBrain::new("Una respuesta larga que no termina nunca. Otra. Otra. Otra.");
    brain.set_reply(
        "Una respuesta larga. Otra frase. Y otra más. Y otra.",
        Duration::from_millis(40),
    );
    let mut h = harness("apagado", Some(brain), fast());
    h.say_line(1);
    h.wait_state(JarvisState::Speaking);
    let started = Instant::now();
    let abandoned = h.orchestrator.shutdown(Duration::from_millis(500));
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(abandoned.is_empty(), "hilos colgados: {abandoned:?}");
    assert!(!h.capture.enabled());
}

#[test]
fn local_command_and_connector_parsing() {
    assert_eq!(local_command("qué hora es"), Some(LocalCommand::Time));
    assert_eq!(local_command("¿Qué hora es?"), Some(LocalCommand::Time));
    assert_eq!(
        local_command("deja de escuchar, por favor"),
        Some(LocalCommand::Standby)
    );
    assert_eq!(local_command("¿qué es el silencio?"), None);
    assert_eq!(local_command("explícame qué horario tiene"), None);

    assert_eq!(local_command("abre el menú y"), None);
    assert_eq!(local_command("¿cómo abro Spotify?"), None);

    assert_eq!(time_phrase(1, 5), "Es la 1:05.");
    assert_eq!(time_phrase(15, 30), "Son las 15:30.");

    assert!(ends_with_connector("busca el archivo y"));
    assert!(ends_with_connector("quiero eso, pero..."));
    assert!(!ends_with_connector("busca el archivo"));
    assert!(!ends_with_connector(""));
}

#[test]
fn noise_during_synthesis_does_not_cancel_the_locution() {
    let h = harness_with_synth_delay("sintesis", None, fast(), Duration::from_millis(150));
    h.say_line(4); // "Jarvis, qué hora es"
    h.wait_state(JarvisState::Thinking);
    // Ruido mientras se sintetiza: no hay audio sonando, no es barge-in.
    h.voice_start();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(h.core.state(), JarvisState::Thinking);
    h.wait_spoken_and_listening(0, 0);
    assert_eq!(h.stats().barge_ins, 0);
}

#[test]
fn own_echo_pauses_briefly_then_resumes_and_is_not_transcribed() {
    let brain = FakeBrain::new("Uno. Dos. Tres. Cuatro.");
    brain.set_reply("Uno. Dos. Tres. Cuatro.", Duration::from_millis(20));
    let h = harness("eco", Some(brain.clone()), fast());
    h.say_line(1);
    h.wait_state(JarvisState::Speaking);
    let generation = h.playback.generation();

    // VAD alto solo por el eco: con Jarvis en pausa el VAD cae a cero.
    h.capture.set_vad(0.0);
    h.voice_start();
    wait_until("pausa", || h.playback.is_paused());
    wait_until("reanudación", || !h.playback.is_paused());
    assert!(!h.playback.is_stale(generation), "la locución sigue viva");
    assert_eq!(h.stats().echo_rejected, 1);
    assert_eq!(h.stats().barge_ins, 0);

    // El segmento del eco se cierra y no llega a whisper ni al LLM.
    h.say_line(3);
    h.wait_spoken_and_listening(0, 0);
    assert_eq!(brain.queries().len(), 1, "el eco no generó un turno");
    assert_eq!(h.stats().turns, 1);
}

#[test]
fn without_barge_in_speaker_echo_neither_pauses_nor_cuts_the_locution() {
    let brain = FakeBrain::new("Uno. Dos. Tres. Cuatro.");
    brain.set_reply("Uno. Dos. Tres. Cuatro.", Duration::from_millis(20));
    let config = VoiceConfig {
        barge_in_enabled: false,
        ..fast()
    };
    let h = harness("sin-barge-in", Some(brain.clone()), config);
    h.say_line(1);
    h.wait_state(JarvisState::Speaking);
    let generation = h.playback.generation();

    // Eco fuerte del altavoz: con barge-in habría pausa y corte.
    h.capture.set_vad(1.0);
    h.voice_start();
    std::thread::sleep(Duration::from_millis(40));
    assert!(!h.playback.is_paused(), "sin barge-in no se pausa");
    assert!(!h.playback.is_stale(generation), "la locución sigue viva");

    // El segmento del eco se cierra y no llega al LLM.
    h.say_line(3);
    h.wait_spoken_and_listening(0, 0);
    assert_eq!(h.stats().barge_ins, 0);
    assert_eq!(brain.queries().len(), 1, "el eco no generó un turno");
}

#[test]
fn long_user_speech_during_speaker_echo_is_not_discarded() {
    let brain = FakeBrain::new("Uno. Dos. Tres. Cuatro.");
    brain.set_reply("Uno. Dos. Tres. Cuatro.", Duration::from_millis(20));
    let config = VoiceConfig {
        barge_in_enabled: false,
        ..fast()
    };
    let h = harness("eco-largo", Some(brain.clone()), config);
    h.say_line(1);
    h.wait_state(JarvisState::Speaking);

    // Voz marcada como eco al empezar encima de Jarvis, pero el segmento
    // dura 5 s: es voz real, no eco (HU-29), y debe transcribirse.
    h.capture.set_vad(1.0);
    h.voice_start();
    h.say_line_with_duration(3, 5_000);

    wait_until("turno transcrito", || brain.queries().len() == 2);
    assert_eq!(brain.queries()[1], "cuéntame otra cosa");
}

/// `(argv[0] sin ruta, display_name)` de la orden, o `None` si no es OpenApp.
fn open_app(query: &str) -> Option<(String, String)> {
    match local_command(query)? {
        LocalCommand::OpenApp {
            command,
            display_name,
        } => {
            let program = command.first()?.rsplit('/').next()?.to_string();
            Some((program, display_name))
        }
        _ => None,
    }
}

#[test]
fn open_app_commands_are_recognized() {
    let app = |program: &str, display: &str| Some((program.to_string(), display.to_string()));

    for query in [
        "abre Spotify",
        "abrir spotify",
        "Pon Spotify",
        "Ábreme Spotify, por favor",
    ] {
        assert_eq!(open_app(query), app("spotify", "Spotify"), "{query}");
    }
    for query in [
        "abre un editor de texto",
        "abre el bloc de notas",
        "abrir editor",
    ] {
        let (program, display) = open_app(query).unwrap_or_else(|| panic!("{query}"));
        assert!(
            ["kwrite", "kate"].contains(&program.as_str()),
            "{query}: {program}"
        );
        assert_eq!(display, "el editor de texto");
    }
    assert_eq!(open_app("abre kate"), app("kate", "el editor de texto"));
    for query in [
        "abre la terminal",
        "Abrir terminal",
        "abre una consola",
        "abre konsole",
    ] {
        assert_eq!(open_app(query), app("konsole", "la terminal"), "{query}");
    }
    for query in [
        "abre el navegador",
        "abre internet",
        "abre Google Chrome",
        "abre firefox",
    ] {
        assert_eq!(open_app(query), app("xdg-open", "el navegador"), "{query}");
    }
    assert_eq!(
        open_app("abre zen").map(|(_, d)| d).as_deref(),
        Some("el navegador")
    );
    assert_eq!(
        open_app("abre Telegram"),
        app("telegram-desktop", "Telegram")
    );
    assert_eq!(open_app("abre discord"), app("discord", "Discord"));
    assert_eq!(open_app("abre steam"), app("steam", "Steam"));
    assert_eq!(
        open_app("abre la calculadora"),
        app("kcalc", "la calculadora")
    );
    for query in [
        "abre el explorador",
        "abre los archivos",
        "abre dolphin",
        "abre carpetas",
    ] {
        assert_eq!(
            open_app(query),
            app("dolphin", "el explorador de archivos"),
            "{query}"
        );
    }

    // "carpeta" y "de" son relleno: la carpeta de descargas no es el explorador.
    let downloads = format!("{}/Descargas", std::env::var("HOME").unwrap_or_default());
    for query in [
        "abre descargas",
        "abrir descargas",
        "abre la carpeta de descargas",
    ] {
        assert_eq!(
            local_command(query),
            Some(LocalCommand::OpenApp {
                command: vec!["xdg-open".into(), downloads.clone()],
                display_name: "la carpeta de Descargas".into(),
            }),
            "{query}"
        );
    }

    // El navegador abre la búsqueda por defecto con el navegador del sistema.
    assert_eq!(
        local_command("abre el navegador"),
        Some(LocalCommand::OpenApp {
            command: vec!["xdg-open".into(), "https://google.com".into()],
            display_name: "el navegador".into(),
        })
    );

    // Sin verbo de apertura, o "pon" con algo que no es Spotify: al LLM.
    assert_eq!(local_command("me gusta spotify"), None);
    assert_eq!(local_command("pon la terminal"), None);
    assert_eq!(local_command("abre el archivo de configuración"), None);
}

#[test]
fn open_app_launches_detached_and_answers_without_llm() {
    let launched = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let config = VoiceConfig {
        launcher: {
            let launched = Arc::clone(&launched);
            Arc::new(move |argv: &[String]| {
                launched.lock().unwrap().push(argv.to_vec());
                Ok(())
            })
        },
        ..fast()
    };
    let h = harness("open-app", None, config);

    h.say_line(11); // "Jarvis, ábreme Spotify"
    h.wait_spoken_and_listening(0, 0);
    assert_eq!(h.stats().local_commands, 1);
    assert_eq!(h.stats().turns, 0, "no pasa por el LLM");
    let launched = launched.lock().unwrap();
    assert_eq!(launched.len(), 1);
    assert!(launched[0][0].ends_with("spotify"), "{:?}", launched[0]);
}

#[test]
fn close_app_commands_are_recognized() {
    let close = |pattern: &str, display: &str| {
        Some(LocalCommand::CloseApp {
            pattern: pattern.into(),
            display_name: display.into(),
        })
    };
    for query in [
        "cierra spotify",
        "Cerrar Spotify",
        "ciérrame Spotify, por favor",
    ] {
        assert_eq!(local_command(query), close("spotify", "Spotify"), "{query}");
    }
    for query in [
        "cierra el editor",
        "cierra kwrite",
        "cerrar editor",
        "cierra bloc de notas",
        "cierra kate",
    ] {
        assert_eq!(
            local_command(query),
            close("kwrite|kate", "el editor de texto"),
            "{query}"
        );
    }
    for query in ["cierra la terminal", "cerrar la terminal", "cierra consola"] {
        assert_eq!(
            local_command(query),
            close("konsole", "la terminal"),
            "{query}"
        );
    }
    for query in [
        "cierra el navegador",
        "cerrar el navegador",
        "cierra firefox",
    ] {
        assert_eq!(
            local_command(query),
            close(
                "zen|zen-bin|chrome|google-chrome|firefox|firefox-bin",
                "el navegador"
            ),
            "{query}"
        );
    }
    assert_eq!(
        close_app_argv("konsole"),
        vec!["pkill", "-i", "-x", "konsole"]
    );

    // Fuera del set local, preguntas y órdenes mixtas: al LLM.
    assert_eq!(local_command("cierra Telegram"), None);
    assert_eq!(local_command("cierra Obsidian"), None);
    assert_eq!(local_command("¿cómo cierro Spotify?"), None);
    assert_eq!(local_command("cierra discord y abre spotify"), None);
    assert_eq!(local_command("abre Obsidian"), None);
}

#[test]
fn close_app_runs_pkill_and_answers_without_llm() {
    let launched = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
    let config = VoiceConfig {
        launcher: {
            let launched = Arc::clone(&launched);
            Arc::new(move |argv: &[String]| {
                launched.lock().unwrap().push(argv.to_vec());
                Ok(())
            })
        },
        ..fast()
    };
    let h = harness("close-app", None, config);

    h.say_line(12); // "Jarvis, cierra Spotify"
    h.wait_spoken_and_listening(0, 0);
    assert_eq!(h.stats().local_commands, 1);
    assert_eq!(h.stats().turns, 0, "no pasa por el LLM");
    assert_eq!(
        *launched.lock().unwrap(),
        vec![vec!["pkill", "-i", "-x", "spotify"]]
    );
}

// ------------------------------------------------------------ HU-37 tareas

const TASK_REPLY: &str = "RESUMEN_HABLADO: Tienes dos correos urgentes. DETALLE: - a - b";

#[test]
fn heavy_turn_acks_fast_and_a_light_turn_runs_while_the_task_works() {
    use crate::tasks::TaskStatus;
    let brain = FakeBrain::new("Respuesta ligera.");
    let factory = FakeFactory::new(TASK_REPLY, Duration::from_millis(400));
    let h = task_harness("task-ack", brain.clone(), factory.clone());

    let started = Instant::now();
    h.say_line(13);
    h.wait_spoken_and_listening(0, 0);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "acuse en {:?} (CA-37.1)",
        started.elapsed()
    );
    assert!(
        brain.queries().is_empty(),
        "el cerebro conversacional no recibe la tarea"
    );
    assert_eq!(h.task_status(), vec![TaskStatus::Running]);

    // Turno ligero mientras la tarea sigue (CA-37.2).
    let (p, d) = (h.played().len(), h.drained());
    h.say_line(1);
    h.wait_spoken_and_listening(p, d);
    assert_eq!(brain.queries(), vec!["¿qué tal el clima?".to_string()]);
    assert_eq!(h.task_status(), vec![TaskStatus::Running], "la tarea sigue");

    // Al terminar, el resultado se locuta solo (CA-37.4).
    let (p, d) = (h.played().len(), h.drained());
    wait_until("tarea terminada", || {
        h.task_status() == vec![TaskStatus::Done]
    });
    h.wait_spoken_and_listening(p, d);
    let spawned = factory.spawned.lock().unwrap();
    assert_eq!(spawned.len(), 1, "un cerebro aislado por tarea");
    assert!(spawned[0].queries()[0].contains("revisa mis correos"));
}

#[test]
fn barge_in_cancels_the_reply_but_not_the_background_task() {
    use crate::tasks::TaskStatus;
    let brain = FakeBrain::new("x");
    let factory = FakeFactory::new(TASK_REPLY, Duration::from_millis(300));
    let h = task_harness("task-barge", brain.clone(), factory);

    h.say_line(13);
    h.wait_spoken_and_listening(0, 0);

    brain.set_reply(
        "Mañana llueve por la mañana. Luego despeja. Y por la noche hiela. Fin.",
        Duration::from_millis(30),
    );
    h.say_line(2);
    h.wait_state(JarvisState::Speaking);
    h.capture.set_vad(0.9);
    h.voice_start();
    wait_until("barge-in", || h.stats().barge_ins == 1);
    h.capture.set_vad(0.0);

    wait_until("tarea terminada (CA-37.3)", || {
        h.task_status() == vec![TaskStatus::Done]
    });
}

#[test]
fn results_are_held_while_muted_and_announced_on_unmute() {
    use crate::tasks::TaskStatus;
    let brain = FakeBrain::new("x");
    let factory = FakeFactory::new(TASK_REPLY, Duration::from_millis(150));
    let h = task_harness("task-mute", brain, factory);

    h.say_line(13);
    h.wait_spoken_and_listening(0, 0);
    h.core.transition(JarvisState::Muted).unwrap();
    wait_until("tarea terminada", || {
        h.task_status() == vec![TaskStatus::Done]
    });
    let played = h.played().len();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        h.played().len(),
        played,
        "en silencio no se locuta (CA-37.8)"
    );

    let drained = h.drained();
    h.core.transition(JarvisState::Idle).unwrap();
    // Primero "Tienes 1 resultado pendiente." y luego el resultado.
    wait_until("anuncio y resultado", || h.drained() >= drained + 2);
    h.wait_state(JarvisState::Listening);
}

#[test]
fn voice_commands_list_and_cancel_tasks() {
    use crate::tasks::TaskStatus;
    let brain = FakeBrain::new("x");
    let factory = FakeFactory::new(TASK_REPLY, Duration::from_secs(5));
    let h = task_harness("task-cancel", brain.clone(), factory);

    h.say_line(13);
    h.wait_spoken_and_listening(0, 0);
    let (p, d) = (h.played().len(), h.drained());
    h.say_line(15);
    h.wait_spoken_and_listening(p, d);

    let (p, d) = (h.played().len(), h.drained());
    h.say_line(14);
    h.wait_spoken_and_listening(p, d);
    wait_until("cancelada (CA-37.7)", || {
        h.task_status() == vec![TaskStatus::Cancelled]
    });
    assert!(brain.queries().is_empty(), "comandos locales, sin LLM");
}

#[test]
fn heavy_turn_without_task_manager_goes_to_the_conversational_brain() {
    let brain = FakeBrain::new("Respuesta.");
    let h = harness("task-off", Some(brain.clone()), fast());
    h.say_line(13);
    h.wait_spoken_and_listening(0, 0);
    assert_eq!(brain.queries().len(), 1);
    assert!(h.orchestrator.tasks().is_none());
}

#[test]
fn queued_speech_reports_position_and_is_spoken_in_order() {
    let brain = FakeBrain::new("x");
    let h = harness("queue", Some(brain), fast());
    let speaker = h.orchestrator.speaker();
    let a = speaker.enqueue("primero", SpeechPriority::Notice);
    let b = speaker.enqueue("segundo", SpeechPriority::Notice);
    assert!(a >= 1 && b == a + 1, "posiciones {a} {b}");
    wait_until("dos locuciones", || h.drained() >= 2);
    h.wait_state(JarvisState::Listening);
}

#[test]
fn local_command_recognizes_task_commands() {
    assert_eq!(
        local_command("cancela todas las tareas"),
        Some(LocalCommand::CancelTask {
            spoken_target: None
        })
    );
    assert_eq!(
        local_command("cancela la tarea de los correos"),
        Some(LocalCommand::CancelTask {
            spoken_target: Some("correos".into())
        })
    );
    assert_eq!(
        local_command("¿qué tareas tengo?"),
        Some(LocalCommand::ListTasks)
    );
}
