//! Punto de entrada de Fokkus-Jarvis.
//!
//! Orquesta el arranque completo del asistente combinando `jarvis-core`,
//! `jarvis-render`, `jarvis-shell`, `jarvis-audio` y `jarvis-ipc`:
//!
//! 1. Analiza los argumentos de línea de comandos (`--version`, `--check`,
//!    y los subcomandos de ojos y manos `input`/`capture`).
//! 2. Adquiere el lock de instancia única.
//! 3. Carga la configuración persistente (`~/.config/fokkus-jarvis/config.toml`).
//! 4. Crea el **único** `JarvisCore` ([`SharedCore`]) que comparten la
//!    esfera, el servidor IPC y el orquestador de voz.
//! 5. Arranca el servidor IPC en `~/.fokkus/jarvis.sock` (Issue #5).
//! 6. Abre la capa overlay (o el modo headless).
//! 7. Arranca el motor de audio (`jarvis-audio-io`: streams `cpal` + DSP) y
//!    el orquestador de voz (STT → Claude → TTS con barge-in). Solo lanzan
//!    hilos: abrir dispositivos y cargar whisper ocurre fuera del render.
//! 8. Entra en el bucle principal ([`runtime::run_loop`]) hasta SIGINT/SIGTERM,
//!    "Salir" en el menú o el cierre de la capa, y apaga en orden.
//!
//! Sin terminal (lanzado desde el `.desktop`) toda la salida va al log
//! persistente ([`logging`]); si falta el modelo whisper se descarga en
//! segundo plano y la escucha arranca al quedar cargado.

mod diag;
mod hud;
mod logging;
mod presence;
mod runtime;
mod settings;

use std::cell::{Cell, RefCell};
use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jarvis_audio::io::{AudioBackend, CpalBackend, InputPort, OutputPort, PortShared};
use jarvis_audio::{
    ActivationDetector, AudioEngine, AudioPlayback, ConversationCoordinator, DspThreadConfig,
    EdgeTtsEngine, EndpointConfig, EngineConfig, EngineControl, ModelManager, ModelStatus,
    SpeechPriority, TaskConfig, TaskManager, TaskOrigin, TaskStatus, TtsBackend, VoiceConfig,
    VoiceDeps, VoiceOrchestrator, VoiceSpeaker, WhisperEngine, WHISPER_BASE_Q5_1,
};
use jarvis_automation::{
    ActionRiskLevel, AutomationGuard, Capabilities, CaptureTarget, InputAction, InputSynth, Key,
    KillReason, MouseButton, Point, ProbeOptions,
};
use jarvis_core::single_instance::{self, InstanceError};
use jarvis_core::{
    sphere_size_for_pct, FullscreenBehavior, JarvisConfig, JarvisCore, Opacity, SharedCore,
};
use jarvis_ipc::{
    speech_priority, IpcClient, IpcError, IpcServer, ServerState, SpeechSink, TaskPort, Utterance,
};
use jarvis_shell::{
    HeadlessSurface, OverlayOptions, OverlaySurface, ShellCommand, SurfaceError, SurfaceSize,
};
use ringbuf::traits::Split;
use ringbuf::HeapRb;
use runtime::{LoopExit, SphereLook, SphereView, SttAvailability};

/// Ruta por defecto de `fokkus-jarvis capture`.
const DEFAULT_CAPTURE_PATH: &str = "/tmp/jarvis-screen.png";

/// Acción solicitada en la línea de comandos.
#[derive(Debug, Clone, PartialEq)]
enum CliAction {
    /// Imprime la versión y sale con éxito.
    Version,
    /// Diagnostica la salud de los subsistemas y sale.
    Check,
    /// Muestra la ayuda y sale.
    Help,
    /// Lee la API key de Anthropic por stdin y la guarda en el llavero.
    SetApiKey,
    /// Arranca el asistente (comportamiento por defecto).
    Run,
    /// `input type|key|click`: una acción de entrada sintética (HU-38).
    Input(InputAction),
    /// `capture [ruta]`: guarda la pantalla activa como PNG (HU-38).
    Capture(PathBuf),
    /// `dev run <tarea>`: despacha un desarrollo al gestor de tareas (HU-37).
    DevRun(String),
    /// `--diag-turns [segundos]`: escucha y vuelca las métricas de cierre de
    /// turno y pausas intra-turno (HU-41).
    DiagTurns(Duration),
}

/// Resultado de una sonda de salud sobre un subsistema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    /// Subsistema operativo.
    Ok,
    /// Subsistema degradado (el asistente puede arrancar igualmente).
    Warn,
    /// Subsistema en fallo.
    Fail,
}

impl Health {
    /// Etiqueta legible del estado de salud.
    fn label(self) -> &'static str {
        match self {
            Health::Ok => "OK",
            Health::Warn => "AVISO",
            Health::Fail => "FALLO",
        }
    }
}

/// Sumidero de locuciones del servidor IPC: `jarvis.speak` de Fokkus IDE se
/// dice en voz alta por el orquestador. El orquestador arranca después del
/// IPC, así que el destino se instala cuando existe.
#[derive(Default)]
struct VoiceSpeechSink {
    speaker: Mutex<Option<VoiceSpeaker>>,
}

impl VoiceSpeechSink {
    fn connect(&self, speaker: VoiceSpeaker) {
        *self
            .speaker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(speaker);
    }
}

impl SpeechSink for VoiceSpeechSink {
    fn speak(&self, utterance: &Utterance) {
        let speaker = self
            .speaker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        match speaker {
            Some(speaker) => speaker.say(utterance.text.clone()),
            None => eprintln!("[ipc] locución descartada: el motor de voz aún no arranca."),
        }
    }

    fn enqueue(&self, utterance: &Utterance, priority: &str) -> Option<u32> {
        let speaker = self
            .speaker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()?;
        Some(speaker.enqueue(utterance.text.clone(), parse_priority(priority)))
    }
}

/// Prioridad IPC (`notice`, `task_result`…) → prioridad de la cola hablada.
fn parse_priority(priority: &str) -> SpeechPriority {
    match priority {
        speech_priority::CRITICAL => SpeechPriority::Critical,
        speech_priority::REPLY => SpeechPriority::Reply,
        speech_priority::TASK_RESULT => SpeechPriority::TaskResult,
        _ => SpeechPriority::Notice,
    }
}

/// Puerto IPC de tareas (`jarvis.dispatchTask`…) sobre el gestor de tareas
/// del orquestador (HU-37).
struct VoiceTaskPort(TaskManager);

impl TaskPort for VoiceTaskPort {
    fn dispatch(&self, title: String, prompt: String) -> u64 {
        let title = if title.is_empty() {
            jarvis_audio::tasks::task_title(&prompt)
        } else {
            title
        };
        let prompt = ipc_task_prompt(&title, &prompt);
        self.0.submit(title, prompt, TaskOrigin::Ipc)
    }

    fn list(&self) -> Vec<serde_json::Value> {
        self.0
            .list()
            .iter()
            .filter_map(|record| serde_json::to_value(record).ok())
            .collect()
    }

    fn get(&self, id: u64) -> Option<serde_json::Value> {
        self.0
            .get(id)
            .and_then(|record| serde_json::to_value(record).ok())
    }

    fn cancel(&self, id: u64) -> bool {
        let active = self
            .0
            .get(id)
            .is_some_and(|r| matches!(r.status, TaskStatus::Queued | TaskStatus::Running));
        active && self.0.cancel(id)
    }
}

/// Elige el *prompt* del subagente según el título de la tarea despachada por
/// IPC: los desarrollos usan el prompt de ingeniería (que sí ejecuta), el resto
/// conserva el prompt de solo lectura (HU-37).
fn ipc_task_prompt(title: &str, prompt: &str) -> String {
    if title == jarvis_audio::tasks::DEV_TASK_TITLE {
        jarvis_audio::tasks::dev_task_prompt(prompt, None)
    } else {
        jarvis_audio::tasks::task_prompt(prompt)
    }
}

/// Configuración del gestor de tareas a partir de `config.toml` (HU-37).
fn task_config(config: &JarvisConfig) -> TaskConfig {
    TaskConfig {
        max_parallel: usize::from(config.tasks_max_parallel()),
        timeout: Duration::from_secs(u64::from(config.tasks_timeout_secs())),
        quiet_gap: Duration::from_millis(u64::from(config.tasks_quiet_gap_ms())),
        ..TaskConfig::default()
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Ok(CliAction::Version) => {
            println!("Fokkus-Jarvis {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(CliAction::Check) => run_health_check(),
        Ok(CliAction::Help) => {
            print_usage();
            ExitCode::SUCCESS
        }
        Ok(CliAction::SetApiKey) => run_set_api_key(),
        Ok(CliAction::Run) => run(),
        Ok(CliAction::Input(action)) => run_input(action),
        Ok(CliAction::Capture(path)) => run_capture(&path),
        Ok(CliAction::DevRun(task)) => run_dev_task(&task),
        Ok(CliAction::DiagTurns(window)) => diag::run(window),
        Err(message) => {
            eprintln!("{message}");
            print_usage();
            ExitCode::from(2)
        }
    }
}

/// Interpreta los argumentos de línea de comandos.
fn parse_args(args: &[String]) -> Result<CliAction, String> {
    match args.first().map(String::as_str) {
        None => Ok(CliAction::Run),
        Some("-v" | "--version") => Ok(CliAction::Version),
        Some("-c" | "--check") => Ok(CliAction::Check),
        Some("-h" | "--help") => Ok(CliAction::Help),
        Some("--set-api-key") => Ok(CliAction::SetApiKey),
        Some("--diag-turns") => diag::parse_secs(&args[1..]).map(CliAction::DiagTurns),
        Some("input") => parse_input(&args[1..]).map(CliAction::Input),
        Some("dev") => parse_dev(&args[1..]).map(CliAction::DevRun),
        Some("capture") => match &args[1..] {
            [] => Ok(CliAction::Capture(PathBuf::from(DEFAULT_CAPTURE_PATH))),
            [path] => Ok(CliAction::Capture(PathBuf::from(path))),
            _ => Err("uso: capture [ruta.png]".into()),
        },
        Some(other) => Err(format!("argumento desconocido: {other}")),
    }
}

/// `input type <texto…>` | `input key <combo>` | `input click [x y]`.
fn parse_input(args: &[String]) -> Result<InputAction, String> {
    match args.split_first() {
        Some((verb, rest)) if verb == "type" => {
            let text = rest.join(" ");
            if text.is_empty() {
                return Err("uso: input type <texto>".into());
            }
            Ok(InputAction::TypeText(text))
        }
        Some((verb, [combo])) if verb == "key" => {
            Key::parse_combo(combo).map(InputAction::KeyCombo)
        }
        Some((verb, _)) if verb == "key" => Err("uso: input key <tecla_o_combo>".into()),
        Some((verb, rest)) if verb == "click" => match rest {
            [] => Ok(InputAction::ClickHere {
                button: MouseButton::Left,
                count: 1,
            }),
            [x, y] => {
                let coord = |v: &str| {
                    v.parse::<f64>()
                        .ok()
                        .filter(|n| n.is_finite() && *n >= 0.0)
                        .ok_or_else(|| format!("coordenada inválida: {v}"))
                };
                Ok(InputAction::Click {
                    at: Point::new(coord(x)?, coord(y)?),
                    button: MouseButton::Left,
                    count: 1,
                })
            }
            _ => Err("uso: input click [x y]".into()),
        },
        _ => Err("uso: input type <texto> | input key <tecla_o_combo> | input click [x y]".into()),
    }
}

/// `dev run <palabras…>`: une la tarea con espacios y la despacha en segundo
/// plano. La tarea viaja cruda; el servidor construye el *prompt* (HU-37).
fn parse_dev(args: &[String]) -> Result<String, String> {
    match args.split_first() {
        Some((verb, rest)) if verb == "run" => {
            let task = rest.join(" ").trim().to_string();
            if task.is_empty() {
                Err("uso: dev run <tarea>".into())
            } else {
                Ok(task)
            }
        }
        _ => Err("uso: dev run <tarea>".into()),
    }
}

/// Texto de ayuda del binario.
fn print_usage() {
    println!("Uso: jarvis [OPCIONES]");
    println!();
    println!("Opciones:");
    println!("  -v, --version   Muestra la versión y sale.");
    println!("  -c, --check     Diagnostica los subsistemas y sale.");
    println!("  -h, --help      Muestra esta ayuda y sale.");
    println!("  --set-api-key   Lee la API key de Anthropic por stdin y la guarda");
    println!("                  en el llavero del sistema.");
    println!("  --diag-turns [segundos]");
    println!(
        "                  Escucha (por defecto {} s) y vuelca por qué se cierra",
        diag::DEFAULT_SECS
    );
    println!("                  cada turno y las pausas P50/P90; luego sale.");
    println!();
    println!("Ojos y manos (requieren [automation] enabled = true):");
    println!("  input type <texto>          Escribe el texto en la ventana con foco.");
    println!("  input key <tecla_o_combo>   Pulsa una tecla o atajo (Enter, Tab, Ctrl+L…).");
    println!("  input click [x y]           Clic en (x, y) o donde esté el puntero.");
    println!("  capture [ruta.png]          Captura la pantalla activa (por defecto");
    println!("                              {DEFAULT_CAPTURE_PATH}).");
    println!();
    println!("Desarrollo en segundo plano:");
    println!("  dev run <tarea>             Despacha un desarrollo al gestor de tareas.");
    println!();
    println!("Sin opciones, arranca Fokkus-Jarvis, levanta el servidor IPC y deja");
    println!("el socket listo para conexiones de Fokkus IDE.");
    if let Some(path) = logging::log_path() {
        println!("Sin terminal, la salida se escribe en {}.", path.display());
    }
}

/// Guarda en el llavero la API key leída por stdin (nunca por argv, para que
/// no quede en el historial de la shell ni en `ps`).
fn run_set_api_key() -> ExitCode {
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!("Pega la API key de Anthropic y pulsa Enter:");
    }
    let mut key = String::new();
    if let Err(e) = std::io::stdin().lock().read_line(&mut key) {
        eprintln!("No se pudo leer la API key: {e}");
        return ExitCode::FAILURE;
    }
    match jarvis_audio::store_api_key(&key) {
        Ok(()) => {
            println!("API key guardada en el llavero del sistema.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("No se pudo guardar la API key: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `[automation]` para los subcomandos `input`/`capture`, o `None` (con el
/// aviso ya impreso) si la automatización está apagada.
fn cli_automation_config() -> Option<jarvis_core::AutomationConfig> {
    let config = JarvisConfig::load().unwrap_or_else(|e| {
        eprintln!("No se pudo leer la configuración ({e}); se usan los valores por defecto.");
        JarvisConfig::default()
    });
    if !config.automation.enabled {
        eprintln!(
            "La automatización está desactivada: actívala en {} con [automation] enabled = true.",
            JarvisConfig::path().display()
        );
        return None;
    }
    Some(config.automation)
}

fn automation_failure(what: &str, err: impl std::fmt::Display) -> ExitCode {
    eprintln!("No se pudo {what}: {err}");
    ExitCode::FAILURE
}

/// `fokkus-jarvis input …`: una acción de entrada bajo un *lease* de riesgo
/// bajo, con la misma auditoría que las órdenes de voz.
fn run_input(action: InputAction) -> ExitCode {
    let Some(cfg) = cli_automation_config() else {
        return ExitCode::FAILURE;
    };
    let guard = AutomationGuard::from_config(&cfg);
    let mut synth =
        match jarvis_automation::probe::probe_input_backend_with(&ProbeOptions::from(&cfg)) {
            Ok(synth) => synth,
            Err(e) => return automation_failure("abrir la entrada sintética", e),
        };
    let result = guard
        .acquire_with_risk("cli input", "", ActionRiskLevel::Low)
        .and_then(|mut lease| lease.perform(synth.as_mut(), action));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => automation_failure("actuar", e),
    }
}

/// `fokkus-jarvis capture [ruta]`: captura el monitor activo y lo guarda como
/// PNG; imprime la ruta al terminar.
fn run_capture(path: &std::path::Path) -> ExitCode {
    let Some(cfg) = cli_automation_config() else {
        return ExitCode::FAILURE;
    };
    let guard = AutomationGuard::from_config(&cfg);
    let mut backend = match jarvis_automation::probe_capture_backend() {
        Ok(backend) => backend,
        Err(e) => return automation_failure("abrir la captura", e),
    };
    let frame = match guard
        .acquire_with_risk("cli capture", "", ActionRiskLevel::Low)
        .and_then(|mut lease| lease.capture(backend.as_mut(), &CaptureTarget::ActiveOutput))
    {
        Ok(frame) => frame,
        Err(e) => return automation_failure("capturar la pantalla", e),
    };
    if let Err(e) = std::fs::write(path, frame.encode_png()) {
        return automation_failure(&format!("guardar {}", path.display()), e);
    }
    println!("{}", path.display());
    ExitCode::SUCCESS
}

/// `fokkus-jarvis dev run <tarea>`: despacha el desarrollo al gestor de tareas
/// del orquestador a través del socket IPC y cierra el turno en segundos.
fn run_dev_task(task: &str) -> ExitCode {
    let mut client = match IpcClient::connect_default() {
        Ok(client) => client,
        Err(e) => {
            eprintln!("No se pudo conectar con Jarvis (¿está corriendo?): {e}");
            return ExitCode::FAILURE;
        }
    };
    match client.dispatch_task(jarvis_audio::tasks::DEV_TASK_TITLE, task) {
        Ok(result) => {
            println!(
                "Desarrollo #{} despachado en segundo plano ({}).",
                result.id, result.status
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("No se pudo despachar el desarrollo: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Recarga `config.toml` tras guardar el formulario y aplica en caliente el
/// cerebro y el barge-in; la voz se aplica en [`apply_loaded_voice`].
/// Devuelve la configuración recargada (fuente de verdad de la esfera).
fn apply_config(
    core: &SharedCore,
    orchestrator: Option<&VoiceOrchestrator>,
    task_factory: &settings::TaskBrainFactory,
) -> Option<JarvisConfig> {
    let config = match JarvisConfig::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("[config] no se pudo recargar: {e}");
            return None;
        }
    };
    if let Some(orchestrator) = orchestrator {
        orchestrator.set_barge_in(config.barge_in_enabled);
        orchestrator.set_continuous_listening(config.continuous_listening);
        orchestrator.set_voice(&config.voice_id);
        orchestrator.set_brain(settings::build_brain(&config).ok());
        orchestrator.set_gaming_mode(config.gaming_mode);
        // Las tareas nuevas usan el cerebro nuevo; las en curso no cambian.
        task_factory.update(&config);
        if !orchestrator.has_brain() {
            orchestrator.speaker().say(
                "No pude activar el cerebro con esa configuración. Solo puedo darte la hora o silenciarme.",
            );
        }
    }
    println!(
        "[config] aplicada: cerebro={:?}, barge-in={}, escucha-continua={}, esfera={} % / opacidad {} %",
        config.brain_mode,
        if config.barge_in_enabled {
            "activo"
        } else {
            "apagado"
        },
        if config.continuous_listening {
            "activa"
        } else {
            "apagada"
        },
        config.sphere_scale_pct(),
        config.sphere_opacity_pct(),
    );
    apply_loaded_voice(core, orchestrator, &config);
    Some(config)
}

/// Aplica la voz ya cargada en `config` y actualiza el núcleo compartido.
fn apply_loaded_voice(
    core: &SharedCore,
    orchestrator: Option<&VoiceOrchestrator>,
    config: &JarvisConfig,
) {
    if let Some(orchestrator) = orchestrator {
        orchestrator.set_voice(&config.voice_id);
    }
    println!(
        "[voz] aplicada: {} ({})",
        config.voice_id, config.voice_lang
    );
    core.lock().set_config(config.clone());
}

/// Arranca el asistente completo y lo mantiene vivo como daemon.
fn run() -> ExitCode {
    // Antes que nada: sin terminal, la salida va al log persistente.
    let log_file = logging::redirect_if_detached();

    // Instancia única: impide que corran dos procesos concurrentes.
    let lock = match single_instance::acquire() {
        Ok(lock) => lock,
        Err(InstanceError::AlreadyRunning) => {
            eprintln!("Fokkus-Jarvis ya está en ejecución.");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("No se pudo adquirir el lock de instancia única: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Señales antes de levantar nada: un Ctrl+C temprano también apaga en orden.
    let shutdown = runtime::install_shutdown_flag();

    // Configuración persistente. Si el archivo está corrupto, se degrada a los
    // valores por defecto para no bloquear el arranque.
    // Primer arranque: deja escrita la configuración por defecto (cerebro
    // CLI `agy -p`, barge-in apagado) para que sea visible y editable.
    let config_path = JarvisConfig::path();
    match JarvisConfig::ensure_file(&config_path) {
        Ok(true) => println!(
            "[config] configuración por defecto creada en {}",
            config_path.display()
        ),
        Ok(false) => {}
        Err(e) => eprintln!("[config] {e}"),
    }
    let config = match JarvisConfig::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Configuración ilegible ({e}); usando valores por defecto.");
            JarvisConfig::default()
        }
    };

    // Un único núcleo: la esfera, el IPC y el orquestador ven el mismo estado.
    let core = SharedCore::new(JarvisCore::new(config.clone()));
    let playback = AudioPlayback::new();
    let tts = EdgeTtsEngine::with_voice(config.voice_id.clone());
    let tts_backend = tts.backend();
    let coordinator =
        ConversationCoordinator::with_shared_core(tts, core.clone(), playback.clone());

    // Servidor IPC para Fokkus IDE (Issue #5): hilo de fondo no bloqueante.
    let speech_sink = Arc::new(VoiceSpeechSink::default());
    let server_state =
        Arc::new(ServerState::with_shared_core(core.clone()).with_speech_sink(speech_sink.clone()));
    let mut server = IpcServer::new(Arc::clone(&server_state));
    if let Err(e) = server.start() {
        eprintln!("No se pudo arrancar el servidor IPC: {e}");
        return ExitCode::FAILURE;
    }
    let socket_path = server.socket_path().to_path_buf();

    // Pre-síntesis de fillers en segundo plano: nunca bloquea el arranque.
    spawn_filler_pre_synthesis(config.voice_id.clone());

    // Superficie overlay con la esfera líquida (o headless si no hay overlay).
    // Tamaño y opacidad persistidos (HU-33/HU-34); la vista previa de los
    // ajustes los cambia en vivo a través de `sphere_look`.
    let persisted_look = Cell::new(SphereLook::from_config(&config));
    let sphere_look = Cell::new(persisted_look.get());
    let mut view = match SphereView::new(persisted_look.get().size) {
        Ok(view) => view,
        Err(e) => {
            eprintln!("No se pudo reservar el buffer de la esfera: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut surface = match prepare_surface(view.size()) {
        Ok(surface) => surface,
        Err(e) => {
            eprintln!("No se pudo preparar la superficie overlay: {e}");
            return ExitCode::FAILURE;
        }
    };
    surface.set_visible(true);

    // Motor de audio: solo lanza el supervisor; los dispositivos se abren en
    // su hilo cuando el orquestador lo habilita.
    let (events_tx, events_rx) = crossbeam_channel::unbounded();
    // Endpointing desde `[voice_endpoint]` (HU-41), ya saturado a rango.
    let voice_endpoint = config.voice_endpoint();
    let engine_config = EngineConfig {
        endpoint: EndpointConfig::from(&voice_endpoint),
        ..EngineConfig::default()
    };
    let mut engine =
        match AudioEngine::start_cpal(engine_config, None, None, playback.clone(), events_tx) {
            Ok(engine) => Some(engine),
            Err(e) => {
                eprintln!(
                    "[audio] no se pudo arrancar el motor de audio: {e}; Jarvis no escuchará."
                );
                None
            }
        };

    // Cerebro según la configuración: CLI local (por defecto `agy -p`) o la
    // API. Sin cerebro solo hay comandos locales y se avisa por voz.
    let (brain, brain_notice) = match settings::build_brain(&config) {
        Ok(brain) => (Some(brain), None),
        Err(notice) => (None, Some(notice)),
    };

    // Fábrica de cerebros aislados para las tareas de fondo (HU-37).
    let task_factory = Arc::new(settings::TaskBrainFactory::new(&config));

    // HUD de burbujas (HU-36): canal acotado; si se llena, el orquestador
    // descarta el evento y la voz nunca espera al render.
    let (hud_tx, hud_rx) = crossbeam_channel::bounded(64);
    let mut hud = hud::HudPipeline::new(hud_rx);

    // Ojos y manos (HU-38): el guard existe siempre para que el kill switch
    // por voz funcione; con `[automation] enabled = false` no concede leases.
    let automation = Arc::new(AutomationGuard::from_config(&config.automation));
    if config.automation.enabled {
        println!(
            "[automatización] habilitada (lease {} s, {} acciones/s)",
            config.automation.lease_secs, config.automation.max_actions_per_sec
        );
    }

    // Orquestador de voz (HU-9).
    let mut orchestrator = engine.as_ref().and_then(|engine| {
        let deps = VoiceDeps {
            core: core.clone(),
            playback: playback.clone(),
            capture: Arc::new(EngineControl::new(engine)),
            sink: Arc::new(engine.player()),
            events: events_rx,
            coordinator,
            brain,
            activation: Arc::new(
                ActivationDetector::new().with_continuous(config.continuous_listening),
            ),
            factory: Some(Arc::clone(&task_factory) as Arc<dyn jarvis_audio::BrainFactory>),
            hud: Some(hud_tx),
        };
        let gaming_mode = config.gaming_mode;
        let config = VoiceConfig {
            startup_notice: brain_notice.map(str::to_string),
            clock: Arc::new(runtime::local_time),
            barge_in_enabled: config.barge_in_enabled,
            connector_grace: Duration::from_millis(u64::from(voice_endpoint.connector_grace_ms)),
            vad_threshold: voice_endpoint.voice_open,
            tasks: Some(task_config(&config)),
            automation_kill: Some({
                let automation = Arc::clone(&automation);
                Arc::new(move || automation.kill(KillReason::Voice))
            }),
            automation_input: config
                .automation
                .enabled
                .then(|| automation_input(Arc::clone(&automation), &config.automation)),
            ..VoiceConfig::default()
        };
        match VoiceOrchestrator::start(deps, config) {
            Ok(orchestrator) => {
                speech_sink.connect(orchestrator.speaker());
                orchestrator.set_gaming_mode(gaming_mode);
                if let Some(tasks) = orchestrator.tasks() {
                    server_state.set_task_port(Arc::new(VoiceTaskPort(tasks)));
                }
                Some(orchestrator)
            }
            Err(e) => {
                eprintln!("[voz] no se pudo arrancar el orquestador: {e}; Jarvis no escuchará.");
                None
            }
        }
    });

    // STT: sonda única. whisper se verifica (y si falta, se descarga) y se
    // carga una sola vez en segundo plano; la escucha arranca al instalarlo.
    let model_manager = ModelManager::new().ok();
    let stt = match &model_manager {
        Some(mgr) => SttAvailability::probe(mgr),
        None => SttAvailability::NoDataDir,
    };
    if let Some(warning) = stt.warning() {
        eprintln!("{warning}");
    }
    let mut stt_pending = match model_manager {
        Some(manager) if orchestrator.is_some() => runtime::spawn_stt_loader(manager),
        _ => None,
    };

    let backend = surface.backend();
    let initial_side = view.size();
    let mut on_first_frame = || {
        println!(
            "Fokkus-Jarvis {} — voz={} ({}) · TTS={:?} · modelo STT={} · estado={:?}",
            env!("CARGO_PKG_VERSION"),
            config.voice_id,
            config.voice_lang,
            tts_backend,
            stt.label(),
            core.state(),
        );
        println!(
            "Esfera renderizada: {}x{} px (backend: {:?})",
            initial_side, initial_side, backend,
        );
        println!(
            "Fokkus-Jarvis iniciado correctamente. Socket IPC activo en {} — listo para conexiones de Fokkus IDE.",
            socket_path.display(),
        );
        if let Some(path) = &log_file {
            println!("Log persistente: {}", path.display());
        }
    };
    let audio_shared = engine.as_ref().map(|e| Arc::clone(e.shared()));
    // Panel Rápido abierto desde el clic derecho (uno a la vez).
    let quick_panel: RefCell<Option<settings::QuickPanel>> = RefCell::new(None);
    // Ajustes Unificados abiertos desde el panel o el menú (uno a la vez).
    let unified_settings: RefCell<Option<settings::UnifiedSettings>> = RefCell::new(None);
    // "Volver a posición inicial": el panel sale con código 3 y el bucle
    // restablece el anclaje de la esfera en el siguiente frame.
    let reset_position = AtomicBool::new(false);
    let mut source = || {
        if let Some(orchestrator) = &orchestrator {
            runtime::poll_stt_loader(&mut stt_pending, |engine: WhisperEngine| {
                orchestrator.set_transcriber(Box::new(engine))
            });
        }
        let closed_panel = quick_panel.borrow().as_ref().and_then(|panel| panel.poll());
        if let Some(outcome) = closed_panel {
            quick_panel.borrow_mut().take();
            match outcome {
                settings::PanelOutcome::Saved => {
                    if let Some(config) = apply_config(&core, orchestrator.as_ref(), &task_factory)
                    {
                        persisted_look.set(SphereLook::from_config(&config));
                        sphere_look.set(persisted_look.get());
                    }
                }
                settings::PanelOutcome::Cancelled => {}
                settings::PanelOutcome::ResetPosition => {
                    reset_position.store(true, Ordering::SeqCst);
                }
                settings::PanelOutcome::OpenSettings(tab) => {
                    let mut form = unified_settings.borrow_mut();
                    if form.is_none() {
                        *form = settings::UnifiedSettings::open(&config_path, tab.as_deref());
                    }
                }
            }
        }
        // Vista previa en vivo: se queda con el último valor de cada tipo y
        // el bucle lo aplica antes de dibujar este frame.
        if let Some(form) = unified_settings.borrow().as_ref() {
            let mut next = sphere_look.get();
            while let Some(preview) = form.poll_preview() {
                match preview {
                    settings::SettingsPreview::SphereScale(pct) => {
                        next.size = sphere_size_for_pct(pct);
                    }
                    settings::SettingsPreview::SphereOpacity(pct) => {
                        next.opacity = Opacity::from_pct(pct);
                    }
                }
            }
            sphere_look.set(next);
        }
        let closed_settings = unified_settings
            .borrow()
            .as_ref()
            .and_then(|form| form.poll());
        if let Some(outcome) = closed_settings {
            unified_settings.borrow_mut().take();
            match outcome {
                settings::FormOutcome::Saved => {
                    // El TOML es la fuente de verdad: se relee y se aplica.
                    if let Some(config) = apply_config(&core, orchestrator.as_ref(), &task_factory)
                    {
                        persisted_look.set(SphereLook::from_config(&config));
                    }
                }
                settings::FormOutcome::ResetPosition => {
                    // "Volver a posición inicial" también guarda el resto.
                    if let Some(config) = apply_config(&core, orchestrator.as_ref(), &task_factory)
                    {
                        persisted_look.set(SphereLook::from_config(&config));
                    }
                    reset_position.store(true, Ordering::SeqCst);
                }
                // Cancelado, cerrado o proceso muerto: se descarta la vista previa.
                settings::FormOutcome::Cancelled => {}
            }
            sphere_look.set(persisted_look.get());
        }
        let level = audio_shared.as_ref().map_or(0.0, |shared| shared.level());
        (core.state(), level)
    };
    // Menú contextual de la esfera: "Salir" lo resuelve el propio bucle. El
    // orquestador aplica el silencio (micrófono liberado) en ≤ 50 ms.
    let mut on_command = |command: ShellCommand| match command {
        ShellCommand::ToggleMute => {
            let muted = !server_state.is_muted();
            match server_state.set_muted(muted) {
                Ok(result) => println!(
                    "[shell] micrófono {}",
                    if result.muted { "silenciado" } else { "activo" }
                ),
                Err(e) => eprintln!("[shell] no se pudo cambiar el silencio: {}", e.message),
            }
        }
        ShellCommand::Configure => {
            let mut form = unified_settings.borrow_mut();
            if form.is_none() {
                *form = settings::UnifiedSettings::open(&config_path, Some("cerebro"));
            }
        }
        ShellCommand::ChangeVoice => {
            let mut form = unified_settings.borrow_mut();
            if form.is_none() {
                *form = settings::UnifiedSettings::open(&config_path, Some("voz"));
            }
        }
        ShellCommand::QuickPanel { x, y } => {
            let mut panel = quick_panel.borrow_mut();
            if panel.is_none() {
                *panel =
                    settings::QuickPanel::open(&config_path, Some((x, y)), server_state.is_muted());
            }
        }
        ShellCommand::QuickPanelDefault => {
            let mut panel = quick_panel.borrow_mut();
            if panel.is_none() {
                *panel = settings::QuickPanel::open(&config_path, None, server_state.is_muted());
            }
        }
        // Los resuelve el propio bucle.
        ShellCommand::ToggleVisibility | ShellCommand::Quit => {}
    };
    // Icono en la bandeja del sistema: sin bandeja (o fuera de Linux) Jarvis
    // sigue funcionando con el menú contextual de la esfera.
    let mut tray = start_tray();
    let tray_commands = tray.as_ref().map(|tray| tray.commands());
    // Detección de pantalla completa (HU-35): con `ignore` no carga nada.
    let mut fullscreen = presence::FullscreenMonitor::start(
        std::process::id(),
        persisted_look.get().fullscreen,
        jarvis_shell::window_watch::detect_watcher,
    );
    let exit = runtime::run_loop(
        surface.as_mut(),
        &mut view,
        &sphere_look,
        &mut source,
        &shutdown,
        &reset_position,
        None,
        Some(&mut fullscreen),
        Some(&mut hud),
        tray_commands.as_ref(),
        &mut on_first_frame,
        &mut on_command,
    );

    // Apagado ordenado: automatización en curso → script de KWin →
    // superficie → orquestador (y coordinador) → streams y DSP → IPC (borra
    // el socket) → lock.
    if automation.is_acting() {
        automation.kill(KillReason::Shutdown);
    }
    fullscreen.shutdown();
    drop(fullscreen);
    if let Some(tray) = tray.as_mut() {
        tray.shutdown();
    }
    drop(tray);
    surface.set_visible(false);
    drop(surface);
    if let Some(orchestrator) = orchestrator.as_mut() {
        let abandoned = orchestrator.shutdown(Duration::from_millis(300));
        if !abandoned.is_empty() {
            eprintln!("[voz] hilos ocupados al salir: {}", abandoned.join(", "));
        }
    }
    drop(orchestrator);
    if let Some(engine) = engine.as_mut() {
        engine.shutdown();
    }
    drop(engine);
    server.shutdown();
    drop(server);
    drop(lock);

    match exit {
        LoopExit::Signal => println!("Fokkus-Jarvis detenido por señal."),
        LoopExit::SurfaceClosed => {
            println!("Fokkus-Jarvis detenido: el compositor cerró la capa.")
        }
        LoopExit::UserQuit => println!("Fokkus-Jarvis cerrado desde el menú contextual."),
        LoopExit::MaxFrames => {}
    }
    ExitCode::SUCCESS
}

/// Publica el icono de la bandeja (StatusNotifierItem). `None` si no hay bus
/// de sesión o la plataforma no lo soporta.
fn start_tray() -> Option<jarvis_shell::tray::TrayIcon> {
    match jarvis_shell::tray::TrayIcon::start() {
        Ok(tray) => {
            println!("[tray] icono publicado en la bandeja del sistema");
            Some(tray)
        }
        Err(e) => {
            eprintln!("[tray] sin icono en la bandeja: {e}");
            None
        }
    }
}

/// Comprueba la salud de los subsistemas y sale.
///
/// Reporta el estado de la configuración, el lock de instancia única, los
/// backends TTS/STT y el socket IPC. Devuelve un código de error solo si algún
/// subsistema queda en [`Health::Fail`]; las degradaciones ([`Health::Warn`])
/// se informan pero no bloquean la salida.
fn run_health_check() -> ExitCode {
    println!(
        "Fokkus-Jarvis {} — diagnóstico de subsistemas",
        env!("CARGO_PKG_VERSION")
    );

    let mut checks: Vec<(&'static str, Health, String)> = Vec::new();

    // 1. Configuración persistente.
    let config = match JarvisConfig::load() {
        Ok(config) => {
            checks.push((
                "configuración",
                Health::Ok,
                format!("voz={} ({})", config.voice_id, config.voice_lang),
            ));
            config
        }
        Err(e) => {
            checks.push((
                "configuración",
                Health::Warn,
                format!("ilegible ({e}); se usan valores por defecto"),
            ));
            JarvisConfig::default()
        }
    };

    // 2. Lock de instancia única.
    match single_instance::acquire() {
        Ok(lock) => checks.push((
            "lock",
            Health::Ok,
            format!("adquirido en {}", lock.path().display()),
        )),
        Err(InstanceError::AlreadyRunning) => checks.push((
            "lock",
            Health::Warn,
            "otra instancia está en ejecución".to_string(),
        )),
        Err(e) => checks.push(("lock", Health::Fail, format!("no se pudo adquirir: {e}"))),
    }

    // 3. Backend TTS.
    let tts = EdgeTtsEngine::with_voice(config.voice_id.clone());
    match tts.backend() {
        TtsBackend::EdgeTts => {
            checks.push(("TTS", Health::Ok, format!("edge-tts ({})", tts.voice_id())))
        }
        TtsBackend::EspeakNg => checks.push((
            "TTS",
            Health::Warn,
            "fallback espeak-ng (falta edge-tts)".to_string(),
        )),
        TtsBackend::Unavailable => checks.push((
            "TTS",
            Health::Warn,
            "sin backend (ni edge-tts ni espeak-ng)".to_string(),
        )),
    }

    // 4. Backend STT: el modelo se verifica y se carga en whisper de verdad.
    match ModelManager::new() {
        Ok(manager) => match manager.status(&WHISPER_BASE_Q5_1) {
            ModelStatus::Verified { .. } => {
                let started = std::time::Instant::now();
                match manager
                    .load_verified(&WHISPER_BASE_Q5_1)
                    .map_err(|e| e.to_string())
                    .and_then(|model| {
                        WhisperEngine::from_verified(model).map_err(|e| e.to_string())
                    }) {
                    Ok(engine) => checks.push((
                        "STT",
                        Health::Ok,
                        format!(
                            "{} cargado en whisper en {} ms (sha256 {})",
                            engine.model_id(),
                            started.elapsed().as_millis(),
                            engine.model_sha256()
                        ),
                    )),
                    Err(e) => checks.push(("STT", Health::Fail, format!("no carga: {e}"))),
                }
            }
            ModelStatus::Missing => checks.push((
                "STT",
                Health::Warn,
                format!(
                    "modelo no descargado aún en {} (se descarga al arrancar)",
                    manager.path_of(&WHISPER_BASE_Q5_1).display()
                ),
            )),
            ModelStatus::Invalid { reason } => {
                checks.push(("STT", Health::Fail, format!("modelo corrupto: {reason}")))
            }
        },
        Err(_) => checks.push((
            "STT",
            Health::Warn,
            "no se pudo resolver ~/.local/share".to_string(),
        )),
    }

    // 5. Dispositivos de audio: se abren de verdad y se sueltan al terminar.
    checks.extend(probe_audio_streams());

    // 6. API key de Claude / Anthropic.
    match jarvis_audio::brain::resolve_api_key(None) {
        Ok(_) => checks.push((
            "API Anthropic",
            Health::Ok,
            "clave de API Anthropic configurada (keyring/entorno)".to_string(),
        )),
        Err(_) => checks.push((
            "API Anthropic",
            Health::Warn,
            "falta ANTHROPIC_API_KEY en entorno o keyring".to_string(),
        )),
    }

    // 7. Socket IPC.
    let mut server = IpcServer::new(Arc::new(ServerState::with_default_config()));
    match server.bind() {
        Ok(()) => checks.push((
            "socket IPC",
            Health::Ok,
            format!("enlazable en {}", server.socket_path().display()),
        )),
        Err(IpcError::AlreadyRunning(_)) => checks.push((
            "socket IPC",
            Health::Warn,
            "otra instancia ya escucha en el socket".to_string(),
        )),
        Err(e) => checks.push(("socket IPC", Health::Fail, format!("{e}"))),
    }
    drop(server);

    // 8. Superficie overlay.
    match jarvis_shell::probe_overlay() {
        Ok(backend) => checks.push((
            "superficie",
            Health::Ok,
            format!("{backend:?} (zwlr_layer_shell_v1 disponible)"),
        )),
        Err(e) => checks.push((
            "superficie",
            Health::Warn,
            format!("overlay no disponible ({e}); modo headless, solo IPC"),
        )),
    }

    // 9. Detección de pantalla completa (HU-35). Nunca se finge detección.
    let fullscreen = match config.fullscreen_behavior() {
        FullscreenBehavior::Ignore => (
            Health::Ok,
            "desactivada (fullscreen_behavior = ignore)".to_string(),
        ),
        behavior => match jarvis_shell::window_watch::probe(behavior) {
            Ok(kind) => (Health::Ok, format!("{kind} ({behavior:?})")),
            Err(reason) => (Health::Warn, format!("no disponible ({reason})")),
        },
    };
    checks.push(("detección de pantalla completa", fullscreen.0, fullscreen.1));

    // 10. Ojos y manos (HU-38): backends elegidos y descartados con motivo.
    checks.extend(automation_checks(&config.automation));

    // Informe.
    for (name, health, detail) in &checks {
        let status = health.label();
        println!("  [{status}] {name}: {detail}");
    }

    if checks.iter().any(|(_, health, _)| *health == Health::Fail) {
        eprintln!("Diagnóstico: hay subsistemas en fallo.");
        ExitCode::from(1)
    } else {
        println!("Diagnóstico: todos los subsistemas operativos.");
        ExitCode::SUCCESS
    }
}

/// Manos de las órdenes de pantalla por voz (HU-38): cada acción abre un
/// *lease* de riesgo bajo en el guard y la ejecuta con el backend de entrada
/// elegido por el sondeo, que se instancia en la primera orden.
fn automation_input(
    guard: Arc<AutomationGuard>,
    cfg: &jarvis_core::AutomationConfig,
) -> jarvis_audio::AutomationInput {
    let opts = ProbeOptions::from(cfg);
    let synth: Mutex<Option<Box<dyn InputSynth>>> = Mutex::new(None);
    Arc::new(move |action: InputAction| {
        let mut synth = synth.lock().unwrap_or_else(|e| e.into_inner());
        if synth.is_none() {
            let backend = jarvis_automation::probe::probe_input_backend_with(&opts)
                .map_err(|e| e.to_string())?;
            *synth = Some(backend);
        }
        let backend = synth.as_mut().expect("backend recién instanciado");
        let mut lease = guard
            .acquire_with_risk("orden de voz", "", ActionRiskLevel::Low)
            .map_err(|e| e.to_string())?;
        lease
            .perform(backend.as_mut(), action)
            .map_err(|e| e.to_string())
    })
}

/// Sondea los backends de captura y entrada (CA-38.1). Con la automatización
/// apagada solo informa (`Ok`); encendida y sin backend, avisa (`Warn`).
fn automation_checks(cfg: &jarvis_core::AutomationConfig) -> Vec<(&'static str, Health, String)> {
    let caps = jarvis_automation::probe_with(&ProbeOptions::from(cfg));
    let status = if cfg.enabled {
        "habilitada"
    } else {
        "deshabilitada ([automation] enabled = false)"
    };
    let health = |chosen: bool| {
        if chosen || !cfg.enabled {
            Health::Ok
        } else {
            Health::Warn
        }
    };
    let detail = |lines: Vec<String>| {
        if lines.is_empty() {
            "sin backends para esta plataforma".to_string()
        } else {
            lines.join("; ")
        }
    };
    let capture_chosen = caps.chosen_capture();
    let input_chosen = caps.chosen_input();
    vec![
        ("automatización", Health::Ok, status.to_string()),
        (
            "captura de pantalla",
            health(capture_chosen.is_some()),
            detail(Capabilities::report_lines(&caps.capture, capture_chosen)),
        ),
        (
            "entrada sintética",
            health(input_chosen.is_some()),
            detail(Capabilities::report_lines(&caps.input, input_chosen)),
        ),
    ]
}

/// Abre los streams reales de entrada y salida, corre el DSP medio segundo y
/// los suelta (HU-11): comprueba que el audio fluye, no solo que el
/// dispositivo existe. Al volver, el micrófono queda libre.
fn probe_audio_streams() -> Vec<(&'static str, Health, String)> {
    const WINDOW: Duration = Duration::from_millis(500);
    let mut backend = CpalBackend::new(None, None);
    let has_input = backend.has_input();
    let has_output = backend.has_output();
    let shared = Arc::new(PortShared::new(AudioPlayback::new()));
    let mut checks = Vec::new();

    let mut render = None;
    let output = has_output.then(|| {
        backend.open_output(&mut |format| {
            let capacity = format.sample_rate as usize * 2;
            let (_tts, tts_cons) = HeapRb::<f32>::new(capacity).split();
            let (render_prod, render_cons) = HeapRb::<f32>::new(capacity).split();
            render = Some((render_cons, format.sample_rate));
            OutputPort::new(
                tts_cons,
                render_prod,
                format.sample_rate,
                format.channels,
                Arc::clone(&shared),
            )
        })
    });
    let mut capture = None;
    let input = has_input.then(|| {
        backend.open_input(&mut |format| {
            let (prod, cons) = HeapRb::<f32>::new(format.sample_rate as usize * 2).split();
            capture = Some((cons, format.sample_rate));
            InputPort::new(prod, format.channels, Arc::clone(&shared))
        })
    });

    let describe = |s: &jarvis_audio::io::OpenStream| {
        format!(
            "{} ({} Hz, {} canales)",
            s.device, s.format.sample_rate, s.format.channels
        )
    };
    match &output {
        None => checks.push(("altavoz", Health::Warn, "no se detectó altavoz".to_string())),
        Some(Err(e)) => checks.push(("altavoz", Health::Fail, format!("no abre: {e}"))),
        Some(Ok((stream, _))) => checks.push(("altavoz", Health::Ok, describe(stream))),
    }
    match &input {
        None => checks.push((
            "micrófono",
            Health::Warn,
            "no se detectó micrófono".to_string(),
        )),
        Some(Err(e)) => checks.push(("micrófono", Health::Fail, format!("no abre: {e}"))),
        Some(Ok((stream, _))) => checks.push(("micrófono", Health::Ok, describe(stream))),
    }

    // DSP real sobre lo capturado: prueba que el micrófono entrega audio.
    if let Some((capture_cons, capture_rate)) = capture.filter(|_| matches!(input, Some(Ok(_)))) {
        let output_ok = matches!(output, Some(Ok(_)));
        let (render_cons, render_rate) = match render.filter(|_| output_ok) {
            Some(pair) => pair,
            None => (HeapRb::<f32>::new(1).split().1, capture_rate),
        };
        shared
            .render_tap
            .store(output_ok, std::sync::atomic::Ordering::Release);
        let (events_tx, _events_rx) = crossbeam_channel::unbounded();
        let config = DspThreadConfig {
            capture_rate,
            render_rate,
            processor: Default::default(),
            endpoint: Default::default(),
        };
        match jarvis_audio::spawn_dsp_thread(
            config,
            capture_cons,
            render_cons,
            Arc::clone(&shared.latency),
            events_tx,
        ) {
            Ok(mut dsp) => {
                std::thread::sleep(WINDOW);
                let m = dsp.metrics().snapshot();
                let io = shared.metrics.snapshot();
                drop(input);
                drop(output);
                dsp.stop();
                let detail = format!(
                    "{} frames de captura y {} de referencia en {} ms · retardo AEC {} ms · xruns {} · errores {}",
                    m.capture_frames,
                    m.render_frames,
                    WINDOW.as_millis(),
                    m.stream_delay_ms,
                    io.xruns,
                    m.processor_errors + m.resampler_errors,
                );
                let health = if m.capture_frames == 0 || (output_ok && m.render_frames == 0) {
                    Health::Fail
                } else {
                    Health::Ok
                };
                checks.push(("DSP", health, detail));
            }
            Err(e) => checks.push(("DSP", Health::Fail, format!("no arranca: {e}"))),
        }
    }
    checks
}

/// Lanza la pre-síntesis de fillers en un hilo de fondo.
///
/// No bloquea el arranque: si no hay backend TTS disponible simplemente no hace
/// nada y el asistente arranca igual.
fn spawn_filler_pre_synthesis(voice_id: String) {
    let spawned = std::thread::Builder::new()
        .name("jarvis-fillers".to_string())
        .spawn(move || {
            let engine = EdgeTtsEngine::with_voice(voice_id);
            if engine.backend() == TtsBackend::Unavailable {
                return;
            }
            match engine.pre_synthesize_fillers() {
                Ok(0) => {} // todo estaba ya en caché
                Ok(n) => println!("[fillers] {n} frases de relleno pre-sintetizadas."),
                Err(e) => eprintln!("[fillers] no se pudieron pre-sintetizar: {e}"),
            }
        });
    if let Err(e) = spawned {
        eprintln!("[fillers] no se pudo lanzar la pre-síntesis: {e}");
    }
}

/// Abre la superficie overlay a partir del tamaño del renderizador.
///
/// Si no hay overlay disponible (sin `WAYLAND_DISPLAY`, sin layer-shell…)
/// avisa y degrada a [`HeadlessSurface`]: el proceso sigue vivo como daemon IPC.
fn prepare_surface(side: u32) -> Result<Box<dyn OverlaySurface>, SurfaceError> {
    let size = SurfaceSize::new(side, side)?;
    let options = OverlayOptions {
        size,
        ..OverlayOptions::default()
    };
    let surface = match jarvis_shell::open_overlay(options) {
        Ok(surface) => surface,
        Err(e) => {
            eprintln!("[shell] overlay no disponible ({e}); modo headless, solo IPC");
            Box::new(HeadlessSurface::new(size))
        }
    };
    // Al abrir, superficie y renderer deben coincidir (luego cambian juntos).
    if surface.size() != size {
        return Err(SurfaceError::InvalidSize {
            width: surface.size().width,
            height: surface.size().height,
        });
    }
    Ok(surface)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_args_recognizes_every_action() {
        assert_eq!(parse_args(&args(&[])), Ok(CliAction::Run));
        assert_eq!(parse_args(&args(&["--version"])), Ok(CliAction::Version));
        assert_eq!(parse_args(&args(&["-c"])), Ok(CliAction::Check));
        assert_eq!(parse_args(&args(&["--help"])), Ok(CliAction::Help));
        assert_eq!(
            parse_args(&args(&["--set-api-key"])),
            Ok(CliAction::SetApiKey)
        );
        assert!(parse_args(&args(&["--desconocida"])).is_err());
    }

    #[test]
    fn diag_turns_flag_parses_window() {
        assert_eq!(
            parse_args(&args(&["--diag-turns"])),
            Ok(CliAction::DiagTurns(Duration::from_secs(
                diag::DEFAULT_SECS
            )))
        );
        assert_eq!(
            parse_args(&args(&["--diag-turns", "20"])),
            Ok(CliAction::DiagTurns(Duration::from_secs(20)))
        );
        assert!(parse_args(&args(&["--diag-turns", "x"])).is_err());
    }

    #[test]
    fn input_subcommands_build_input_actions() {
        assert_eq!(
            parse_args(&args(&["input", "type", "hola mundo"])),
            Ok(CliAction::Input(InputAction::TypeText("hola mundo".into())))
        );
        // Sin comillas, las palabras se unen con un espacio.
        assert_eq!(
            parse_args(&args(&["input", "type", "hola", "mundo"])),
            Ok(CliAction::Input(InputAction::TypeText("hola mundo".into())))
        );
        assert_eq!(
            parse_args(&args(&["input", "key", "Enter"])),
            Ok(CliAction::Input(InputAction::KeyCombo(vec![Key::Enter])))
        );
        assert_eq!(
            parse_args(&args(&["input", "key", "Ctrl+L"])),
            Ok(CliAction::Input(InputAction::KeyCombo(vec![
                Key::Ctrl,
                Key::Char('l')
            ])))
        );
        assert_eq!(
            parse_args(&args(&["input", "click"])),
            Ok(CliAction::Input(InputAction::ClickHere {
                button: MouseButton::Left,
                count: 1
            }))
        );
        assert_eq!(
            parse_args(&args(&["input", "click", "640", "360.5"])),
            Ok(CliAction::Input(InputAction::Click {
                at: Point::new(640.0, 360.5),
                button: MouseButton::Left,
                count: 1
            }))
        );
    }

    #[test]
    fn input_subcommands_reject_bad_usage() {
        for bad in [
            &["input"][..],
            &["input", "type"],
            &["input", "key"],
            &["input", "key", "Ctrl+"],
            &["input", "key", "Enter", "Tab"],
            &["input", "click", "10"],
            &["input", "click", "x", "10"],
            &["input", "click", "-5", "10"],
            &["input", "mueve"],
        ] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn capture_defaults_to_tmp() {
        assert_eq!(
            parse_args(&args(&["capture"])),
            Ok(CliAction::Capture(PathBuf::from(DEFAULT_CAPTURE_PATH)))
        );
        assert_eq!(
            parse_args(&args(&["capture", "/home/x/p.png"])),
            Ok(CliAction::Capture(PathBuf::from("/home/x/p.png")))
        );
        assert!(parse_args(&args(&["capture", "a.png", "b.png"])).is_err());
    }

    #[test]
    fn dev_run_parses_joined_task() {
        assert_eq!(
            parse_args(&args(&["dev", "run", "refactoriza", "el", "parser"])),
            Ok(CliAction::DevRun("refactoriza el parser".into()))
        );
    }

    #[test]
    fn dev_run_rejects_bad_usage() {
        for bad in [&["dev"][..], &["dev", "run"], &["dev", "foo", "x"]] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ipc_task_prompt_selects_dev_or_read_only_prompt() {
        let dev = ipc_task_prompt(jarvis_audio::tasks::DEV_TASK_TITLE, "x");
        assert!(dev.contains("subagente de ingeniería"), "{dev}");
        assert!(!dev.contains("Solo lees"), "{dev}");

        let read_only = ipc_task_prompt("tus correos", "x");
        assert!(read_only.contains("Solo lees"), "{read_only}");
    }
}
