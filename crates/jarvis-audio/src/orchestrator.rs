//! Orquestador conversacional: `VoiceEvent` → STT → Claude → TTS (HU-ORQ-3).
//!
//! Vive en `jarvis-audio` (y no en el binario) porque es lógica reutilizable
//! que se prueba de punta a punta con dobles: depende de *traits*
//! ([`CaptureControl`], [`AudioSink`], [`Transcriber`], [`TokenStream`]) y no
//! de `cpal`, whisper ni la red. `jarvis-app` solo hace el cableado.
//!
//! # Hilos (SDD §5)
//!
//! | Hilo | Dueño de | Bloquea en |
//! |---|---|---|
//! | `jarvis-voice` | eventos de voz, silencio, barge-in, ventana de conector | nunca (solo canales, 50 ms) |
//! | `jarvis-stt` | el transcriptor (whisper cargado **una vez**) | inferencia |
//! | `jarvis-brain` | [`ConversationCoordinator`], cliente LLM, fillers | red (LLM + síntesis por frase) |
//! | `jarvis-tts` | la cola de reproducción ([`AudioSink`]) | cola llena / fin de locución |
//!
//! El hilo de voz nunca espera a los demás: por eso el barge-in
//! ([`barge_in`]) corta el audio en el acto aunque el cerebro esté a mitad de
//! una petición HTTP.
//!
//! # Cancelación sin carreras
//!
//! Cada locución tiene una *generación* ([`AudioPlayback`]). Un barge-in
//! (`stop()`) la invalida de forma atómica: el callback de salida hace el
//! *fade-out* y vacía la cola, el hilo TTS descarta lo que aún no encoló y el
//! cerebro aborta el stream en el siguiente token. Nadie compara
//! `utterance_generation` contra un valor que otro hilo esté escribiendo: el
//! único que la escribe es el cerebro (al empezar cada turno) y los demás
//! solo la reciben por mensaje junto al audio. Las transiciones de estado
//! que compiten (fin natural de la locución contra barge-in o silencio) se
//! resuelven con [`SharedCore::transition_from`], bajo un único lock.
//!
//! # Conectores (HU-3 adaptativo)
//!
//! Whisper solo ve turnos cerrados, así que el "conector" se resuelve aquí:
//! si la transcripción termina en "y", "pero", "o"… el turno se retiene
//! `connector_grace` (lo que le falta al silencio de cierre para llegar al
//! extendido). Si el usuario retoma, se avisa al DSP con
//! [`ConnectorSignal::signal`] (el turno nuevo espera el silencio extendido)
//! y ambos textos se unen en una sola consulta.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{select, Receiver, Sender};
use jarvis_automation::{InputAction, Key};
use jarvis_core::{JarvisState, SharedCore};

use crate::activation::{Activation, ActivationDetector};
use crate::brain::{BrainError, Completion};
use crate::conversation::{barge_in, ConversationCoordinator, ConversationError, TokenStream};
use crate::dsp::ConnectorSignal;
use crate::events::{CloseReason, HudEvent, SessionView, TurnId, VoiceEvent};
use crate::io::{to_output_pcm, AudioEngine, EngineShared, PlayOutcome, Player};
use crate::playback::AudioPlayback;
use crate::router::{
    fuse_ack_and_foreground, resolve_in_roster, voice_aliases, BrainTurnPlanner, PlannerTrace,
    RosterEntry, RouteContext, SessionControl, SessionQuery, SessionRef, SpawnSpec,
    TurnConstraints, TurnPlan, TurnPlanner, TurnRouter,
};
use crate::sessions::SessionId;
use crate::speech_queue::{Delivery, DeliveryGate, SpeechPriority};
use crate::stt::Transcriber;
use crate::tasks::{
    task_prompt, BrainFactory, SpeechQueueHandle, TaskConfig, TaskId, TaskManager, TaskOrigin,
    TaskRecord, TaskStatus,
};
use crate::tts::{strip_directives, synth_chime, PcmAudio};

/// Lo que dice Jarvis cuando no hay cerebro (ni CLI ni API key) y la
/// consulta lo necesita.
pub const NO_API_KEY_PHRASE: &str = "No tengo un cerebro configurado, así que solo puedo darte la hora o silenciarme. Revisa la configuración en el menú de la esfera.";

/// Aviso hablado al arrancar en modo CLI sin encontrar el comando.
pub const NO_CLI_NOTICE: &str =
    "Hola. No encuentro el comando de mi cerebro; configúralo desde el menú de la esfera.";

/// Aviso hablado al arrancar sin API key (una sola vez).
pub const NO_API_KEY_NOTICE: &str =
    "Hola. Me falta la clave de Anthropic; guárdala con fokkus-jarvis guion guion set api key.";

/// Respuesta a los comandos de tareas cuando el gestor está desactivado.
pub const NO_TASKS_PHRASE: &str = "No tengo tareas de fondo activadas.";

/// Respuesta cuando se cae la red a mitad de una respuesta (CA-6.4).
pub const NETWORK_ERROR_PHRASE: &str = "Perdí la conexión. Inténtalo de nuevo en un momento.";

/// Caducidad del foco verbal (HU-64, §3.4): cinco minutos sin turnos dirigidos
/// a la sub-sesión devuelven el hilo central al mando.
pub const FOCUS_TIMEOUT: Duration = Duration::from_secs(300);

/// `true` si el foco fijado en `at` ya caducó en `now` (HU-64).
pub fn focus_expired(at: Instant, now: Instant, timeout: Duration) -> bool {
    now.saturating_duration_since(at) >= timeout
}

/// Frase al volver al hilo central (HU-64).
pub const UNFOCUS_PHRASE: &str = "Volvimos al hilo principal.";

/// Frase cuando el usuario nombra una sub-sesión que no existe (HU-64).
pub const NO_SUCH_SESSION_PHRASE: &str = "No encontré ese hilo.";

/// Presupuesto del bloque de roster que se antepone al turno del hilo central
/// (HU-65, §3.5): unos 400 tokens. El bloque viaja en la petición al cerebro,
/// nunca al historial de la conversación.
pub const ROSTER_CONTEXT_MAX_CHARS: usize = 1600;

/// Cabecera del bloque de contexto efímero que el cerebro no lee en voz alta.
pub const ROSTER_CONTEXT_HEADER: &str = "[Sub-sesiones activas]";

/// Tope de caracteres del título de una sub-sesión dentro del roster.
const ROSTER_TITLE_MAX_CHARS: usize = 80;

/// Tope de caracteres de un avance o de un resultado dentro del roster.
const ROSTER_TEXT_MAX_CHARS: usize = 160;

/// Sub-sesión abierta por voz, tal como la nombra el usuario (HU-64).
#[derive(Debug, Clone)]
struct VoiceThread {
    /// "la revisión de PR de Felipe".
    title: String,
    /// "prs", "felipe"…
    aliases: Vec<String>,
}

/// Control del micrófono (lo implementa el motor de audio).
pub trait CaptureControl: Send + Sync {
    /// Abre (`true`) o libera (`false`) los dispositivos.
    fn set_enabled(&self, enabled: bool);
    /// `true` si el micrófono está abierto y el DSP produce eventos.
    fn is_listening(&self) -> bool;
    /// `true` si hay dispositivo de salida.
    fn has_output(&self) -> bool;
    /// Señal de conector del DSP vigente.
    fn connector(&self) -> Option<ConnectorSignal>;
    /// Último puntaje del VAD sobre la voz limpia (post-AEC), si hay DSP.
    fn vad_score(&self) -> Option<f32>;
}

/// Destino del audio sintetizado (lo implementa [`Player`]).
pub trait AudioSink: Send + Sync {
    fn output_rate(&self) -> Option<u32>;
    fn play(&self, samples: &[f32], generation: u64) -> PlayOutcome;
    /// Encola audio no asociado a una locución (chime/earcon de HU-18): no se
    /// descarta por barge-in.
    fn play_immediate(&self, samples: &[f32]) -> PlayOutcome;
    fn wait_drained(&self, generation: u64) -> PlayOutcome;
}

impl AudioSink for Player {
    fn output_rate(&self) -> Option<u32> {
        Player::output_rate(self)
    }

    fn play(&self, samples: &[f32], generation: u64) -> PlayOutcome {
        Player::play(self, samples, generation)
    }

    fn play_immediate(&self, samples: &[f32]) -> PlayOutcome {
        Player::play_immediate(self, samples)
    }

    fn wait_drained(&self, generation: u64) -> PlayOutcome {
        Player::wait_drained(self, generation)
    }
}

/// [`CaptureControl`] sobre un [`AudioEngine`] en marcha.
pub struct EngineControl {
    commands: Box<dyn Fn(bool) + Send + Sync>,
    shared: Arc<EngineShared>,
}

impl EngineControl {
    pub fn new(engine: &AudioEngine) -> Self {
        Self {
            commands: Box::new(engine.enable_sender()),
            shared: Arc::clone(engine.shared()),
        }
    }
}

impl CaptureControl for EngineControl {
    fn set_enabled(&self, enabled: bool) {
        (self.commands)(enabled);
    }

    fn is_listening(&self) -> bool {
        self.shared.status().is_listening()
    }

    fn has_output(&self) -> bool {
        self.shared.status().output.is_some()
    }

    fn connector(&self) -> Option<ConnectorSignal> {
        self.shared.connector()
    }

    fn vad_score(&self) -> Option<f32> {
        self.shared.dsp_metrics().map(|m| m.snapshot().vad_score)
    }
}

/// Hora local `(hora, minuto)`; la inyecta el binario (zona horaria del SO).
pub type LocalClock = Arc<dyn Fn() -> (u32, u32) + Send + Sync>;

/// Lanza una aplicación local (`argv`) sin esperar a que termine. Inyectable
/// para que los tests no abran programas reales.
pub type AppLauncher = Arc<dyn Fn(&[String]) -> std::io::Result<()> + Send + Sync>;

/// Kill switch de la automatización (HU-38): revoca el *lease* activo y
/// devuelve `true` si había una automatización en curso. Lo inyecta el binario
/// con `AutomationGuard::kill(KillReason::Voice)`.
pub type AutomationKill = Arc<dyn Fn() -> bool + Send + Sync>;

/// Ejecuta una acción de entrada sintética (teclas multimedia, texto) a
/// través del `AutomationGuard` y el backend elegido (HU-38). Lo inyecta el
/// binario solo con `[automation] enabled = true`; el error ya viene en
/// texto legible para el log.
pub type AutomationInput = Arc<dyn Fn(InputAction) -> Result<(), String> + Send + Sync>;

/// Parámetros del orquestador.
#[derive(Clone)]
pub struct VoiceConfig {
    /// Espera extra tras un turno que termina en conector (extendido − normal).
    pub connector_grace: Duration,
    /// Periodo de reconciliación de silencio y estado.
    pub poll: Duration,
    /// Frase que se dice una vez en cuanto haya altavoz (p. ej. sin API key).
    pub startup_notice: Option<String>,
    pub clock: LocalClock,
    /// Cómo se abren las aplicaciones de [`LocalCommand::OpenApp`].
    pub launcher: AppLauncher,
    /// Ventana de confirmación del barge-in con Jarvis en pausa.
    pub barge_in_window: Duration,
    /// Puntaje del VAD a partir del cual un frame cuenta como voz.
    pub vad_threshold: f32,
    /// Interrupción por voz. Con `false` (altavoces sin audífonos) la voz que
    /// empieza mientras Jarvis suena se trata como eco: no pausa, no corta y
    /// no se transcribe.
    pub barge_in_enabled: bool,
    /// Retardo del chime de confirmación (HU-42, H-T4). El chime ya no suena al
    /// despachar: si el usuario retoma la voz antes de que venza, se cancela y
    /// nunca lo pisa.
    pub chime_delay: Duration,
    /// Ventana de continuación post-despacho (HU-42, H-T4). La voz que llega
    /// dentro de ella es del usuario: jamás se descarta como eco, aunque
    /// Jarvis acabe de empezar a sonar.
    pub continuation_window: Duration,
    /// Ventana de locución temprana (HU-42, H-T5). Una continuación que llega
    /// en los primeros milisegundos de `Speaking` se fusiona con el turno
    /// pendiente y cancela la locución, en vez de responderse por separado.
    pub speaking_merge_window: Duration,
    /// Gestión de tareas pesadas (HU-37). `None` = desactivada.
    pub tasks: Option<TaskConfig>,
    /// Kill switch por voz de la automatización (HU-38). `None` = sin
    /// automatización: "para" solo responde "Detenido.".
    pub automation_kill: Option<AutomationKill>,
    /// Manos de la automatización para las órdenes locales de pantalla
    /// ("pausa", "escribe en pantalla …"). `None` = automatización apagada.
    pub automation_input: Option<AutomationInput>,
    /// Caducidad del foco verbal de una sub-sesión (HU-64).
    pub focus_timeout: Duration,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            connector_grace: Duration::from_millis(500),
            poll: Duration::from_millis(50),
            startup_notice: None,
            clock: Arc::new(utc_clock),
            launcher: Arc::new(launch_detached),
            barge_in_window: Duration::from_millis(250),
            vad_threshold: crate::vad::EndpointConfig::default().voice_open,
            barge_in_enabled: false,
            chime_delay: CHIME_DELAY,
            continuation_window: CONTINUATION_WINDOW,
            speaking_merge_window: SPEAKING_MERGE_WINDOW,
            tasks: None,
            automation_kill: None,
            automation_input: None,
            focus_timeout: FOCUS_TIMEOUT,
        }
    }
}

/// Lanza `argv` totalmente desacoplado de Jarvis: sin heredar stdio y en su
/// propio grupo de procesos (un Ctrl+C o el cierre de Jarvis no lo matan).
/// Un hilo efímero recoge su estado de salida para no dejar zombis.
pub fn launch_detached(argv: &[String]) -> std::io::Result<()> {
    use std::process::{Command, Stdio};

    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "comando vacío"))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    std::thread::Builder::new()
        .name("jarvis-app-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })?;
    Ok(())
}

/// Hora UTC: respaldo cuando el binario no inyecta la hora local.
pub fn utc_clock() -> (u32, u32) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    (((secs / 3_600) % 24) as u32, ((secs / 60) % 60) as u32)
}

/// Dependencias del orquestador.
pub struct VoiceDeps {
    pub core: SharedCore,
    pub playback: AudioPlayback,
    pub capture: Arc<dyn CaptureControl>,
    pub sink: Arc<dyn AudioSink>,
    pub events: Receiver<VoiceEvent>,
    /// Debe compartir `core` y `playback` (ver
    /// [`ConversationCoordinator::with_shared_core`]).
    pub coordinator: ConversationCoordinator,
    /// `None` sin API key: solo comandos locales.
    pub brain: Option<Arc<dyn TokenStream + Send + Sync>>,
    pub activation: Arc<ActivationDetector>,
    /// Fábrica de cerebros para tareas pesadas (HU-37). `None` = sin tareas.
    pub factory: Option<Arc<dyn BrainFactory>>,
    /// Canal hacia el HUD de burbujas (HU-36). Debe ser acotado: se usa
    /// `try_send` y, si está lleno, el evento se descarta. `None` = sin HUD.
    pub hud: Option<Sender<HudEvent>>,
}

/// Emisor de eventos del HUD que nunca bloquea la voz (HU-36).
#[derive(Clone, Default)]
struct HudTx(Option<Sender<HudEvent>>);

impl HudTx {
    fn emit(&self, event: HudEvent) {
        let _ = self.try_emit(event);
    }

    /// Como [`Self::emit`], pero dice si el evento entró al canal. Sin HUD
    /// cuenta como entregado: no hay nadie a quien reintentárselo.
    fn try_emit(&self, event: HudEvent) -> bool {
        match &self.0 {
            // Canal lleno o HUD cerrado: se descarta, la voz sigue.
            Some(tx) => tx.try_send(event).is_ok(),
            None => true,
        }
    }
}

/// Turno de usuario despachado y aún en curso, compartido entre el hilo de
/// voz y el cerebro (HU-AUDIO-CONTINUITY).
///
/// El hilo de voz lo escribe al despachar y lo lee para fusionar las
/// continuaciones; el cerebro lo limpia cuando el turno termina de forma
/// natural. Así una pausa del usuario no convierte su frase completa en dos
/// turnos separados.
#[derive(Clone, Default)]
struct PendingUserTurn(Arc<Mutex<Option<String>>>);

impl PendingUserTurn {
    fn set(&self, text: String) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(text);
    }

    fn take(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// `true` si hay un turno despachado y aún en curso.
    fn is_some(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    fn clear(&self) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

/// Contadores del orquestador (diagnóstico; nunca contienen texto).
#[derive(Debug, Default)]
pub struct VoiceStats {
    pub segments: AtomicU64,
    /// Fragmentos parciales de turnos largos (HU-44).
    pub partials: AtomicU64,
    pub rejected: AtomicU64,
    pub turns: AtomicU64,
    pub local_commands: AtomicU64,
    pub barge_ins: AtomicU64,
    /// Voz durante una locución que resultó ser eco de Jarvis (CA-2.1).
    pub echo_rejected: AtomicU64,
    /// Voz detectada → Jarvis en silencio (pausa), en µs (último barge-in).
    pub last_barge_in_us: AtomicU64,
    /// Fin de turno → filler en cola, en ms (último turno con filler).
    pub last_filler_ms: AtomicU64,
    pub speech_errors: AtomicU64,
    /// Turnos cerrados por silencio normal (HU-41).
    pub close_silence: AtomicU64,
    /// Turnos cerrados por duración máxima (HU-41).
    pub close_max_timeout: AtomicU64,
    /// Turnos cerrados tras el silencio extendido por conector (HU-41).
    pub close_connector: AtomicU64,
    /// Sub-sesiones abiertas por el router (HU-68).
    pub routed_spawn: AtomicU64,
    /// Turnos que respondió el hilo central (HU-68).
    pub routed_foreground: AtomicU64,
    /// Llamadas al planificador que acabaron en la heurística (HU-68).
    pub planner_fallbacks: AtomicU64,
    /// Latencia de la última llamada al planificador, en ms (HU-68).
    pub planner_latency_ms: AtomicU64,
    /// Turnos con la restricción `no_web` activa (HU-71, CA-71.6).
    pub no_web_turns: AtomicU64,
}

/// Fotografía de [`VoiceStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VoiceStatsSnapshot {
    pub segments: u64,
    pub partials: u64,
    pub rejected: u64,
    pub turns: u64,
    pub local_commands: u64,
    pub barge_ins: u64,
    pub echo_rejected: u64,
    pub last_barge_in_us: u64,
    pub last_filler_ms: u64,
    pub speech_errors: u64,
    pub close_silence: u64,
    pub close_max_timeout: u64,
    pub close_connector: u64,
    pub routed_spawn: u64,
    pub routed_foreground: u64,
    pub planner_fallbacks: u64,
    pub planner_latency_ms: u64,
    pub no_web_turns: u64,
}

impl VoiceStats {
    pub fn snapshot(&self) -> VoiceStatsSnapshot {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        VoiceStatsSnapshot {
            segments: get(&self.segments),
            partials: get(&self.partials),
            rejected: get(&self.rejected),
            turns: get(&self.turns),
            local_commands: get(&self.local_commands),
            barge_ins: get(&self.barge_ins),
            echo_rejected: get(&self.echo_rejected),
            last_barge_in_us: get(&self.last_barge_in_us),
            last_filler_ms: get(&self.last_filler_ms),
            speech_errors: get(&self.speech_errors),
            close_silence: get(&self.close_silence),
            close_max_timeout: get(&self.close_max_timeout),
            close_connector: get(&self.close_connector),
            routed_spawn: get(&self.routed_spawn),
            routed_foreground: get(&self.routed_foreground),
            planner_fallbacks: get(&self.planner_fallbacks),
            planner_latency_ms: get(&self.planner_latency_ms),
            no_web_turns: get(&self.no_web_turns),
        }
    }

    /// Anota la llamada al planificador de un turno (HU-68).
    pub fn count_planner(&self, trace: PlannerTrace) {
        let ms = u64::try_from(trace.latency.as_millis()).unwrap_or(u64::MAX);
        self.planner_latency_ms.store(ms, Ordering::Relaxed);
        if trace.fallback {
            self.planner_fallbacks.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Anota por qué se cerró un turno (HU-41, base de `--diag-turns`).
    pub fn count_close(&self, reason: CloseReason) {
        let counter = match reason {
            CloseReason::Silence => &self.close_silence,
            CloseReason::MaxTimeout => &self.close_max_timeout,
            CloseReason::Connector => &self.close_connector,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Órdenes locales que no necesitan al LLM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalCommand {
    Time,
    /// Cierre de la sesión conversacional: vuelve a reposo sin mutear el
    /// micrófono. Decir "Jarvis" reactiva la escucha (HU-17).
    Standby,
    /// Abre una aplicación del escritorio ("abre Spotify").
    OpenApp {
        /// `argv` del programa a lanzar.
        command: Vec<String>,
        /// Cómo se nombra en la respuesta: "Abriendo {display_name}."
        display_name: String,
    },
    /// Cierra una aplicación común ("cierra Spotify") con `pkill`.
    CloseApp {
        /// Nombre(s) de proceso para `pkill -i -x` (regex extendida, p. ej.
        /// `kwrite|kate`).
        pattern: String,
        /// Cómo se nombra en la respuesta: "Cerrando {display_name}."
        display_name: String,
    },
    /// Cancela una, la más reciente o todas las tareas pesadas en curso
    /// ("cancela la tarea de los correos" / "detén el proceso" / "cancela
    /// todas las tareas").
    CancelTask {
        target: CancelTarget,
    },
    /// Lista las tareas pesadas ("¿qué tareas tengo?").
    ListTasks,
    /// Kill switch de la automatización ("para", "detente", "cancela"). Se
    /// evalúa antes que cualquier otra orden (HU-38, CA-38.5).
    StopAutomation,
    /// Abre una URL en el navegador del sistema ("busca X en YouTube").
    OpenUrl {
        url: String,
        /// Respuesta hablada: "Buscando X en YouTube."
        spoken: String,
    },
    /// Acción de pantalla vía `jarvis-automation` ("pausa", "escribe …").
    Screen(ScreenAction),
}

impl LocalCommand {
    /// `true` si la orden abre la web o el navegador (HU-71): lo que
    /// [`TurnConstraints::no_web`] prohíbe.
    pub fn opens_web(&self) -> bool {
        match self {
            Self::OpenUrl { .. } => true,
            Self::OpenApp {
                command,
                display_name,
            } => display_name == BROWSER_NAME || command.iter().any(|arg| arg.starts_with("http")),
            _ => false,
        }
    }
}

/// Acciones de pantalla que el orquestador resuelve sin LLM (HU-38).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenAction {
    Play,
    Pause,
    Next,
    Previous,
    /// Teclea el texto en la ventana con foco.
    Type(String),
}

impl ScreenAction {
    /// Acción de entrada equivalente. Reproducir y pausar usan la tecla
    /// multimedia (MPRIS la entrega al reproductor aunque no tenga el foco).
    pub fn input_action(&self) -> InputAction {
        match self {
            Self::Play | Self::Pause => InputAction::KeyCombo(vec![Key::MediaPlayPause]),
            Self::Next => InputAction::KeyCombo(vec![Key::MediaNext]),
            Self::Previous => InputAction::KeyCombo(vec![Key::MediaPrevious]),
            Self::Type(text) => InputAction::TypeText(text.clone()),
        }
    }

    /// Confirmación hablada tras ejecutar la acción.
    pub fn done_phrase(&self) -> &'static str {
        match self {
            Self::Play => "Reproduciendo.",
            Self::Pause => "Pausado.",
            Self::Next => "Siguiente.",
            Self::Previous => "Anterior.",
            Self::Type(_) => "Escrito en pantalla.",
        }
    }

    /// Respuesta hablada si el backend falla.
    fn failure_phrase(&self) -> &'static str {
        match self {
            Self::Type(_) => "No pude escribir en pantalla.",
            _ => "No pude controlar la reproducción.",
        }
    }
}

/// Verbos del kill switch por voz (texto ya sin acentos).
const STOP_AUTOMATION_VERBS: &[&str] = &[
    "para",
    "parale",
    "detente",
    "detenete",
    "deten",
    "cancela",
    "cancelalo",
    "alto",
    "basta",
    "stop",
    "frena",
];

/// Palabras que pueden acompañar al verbo sin cambiar la orden: "para ya",
/// "detente por favor", "cancela eso ahora mismo".
const STOP_AUTOMATION_FILLERS: &[&str] = &[
    "ya",
    "por",
    "favor",
    "porfa",
    "jarvis",
    "oye",
    "eso",
    "ahora",
    "mismo",
    "ahi",
    "inmediatamente",
    "todo",
    "ahorita",
];

/// La orden es SOLO el kill switch: un verbo de parada y, como mucho,
/// muletillas. "Para qué sirve esto" o "cancela la tarea de los correos" no
/// cuentan.
fn is_stop_automation(words: &[&str]) -> bool {
    let mut rest = words
        .iter()
        .skip_while(|w| STOP_AUTOMATION_FILLERS.contains(w));
    match rest.next() {
        Some(verb) if STOP_AUTOMATION_VERBS.contains(verb) => {
            rest.all(|w| STOP_AUTOMATION_FILLERS.contains(w) || STOP_AUTOMATION_VERBS.contains(w))
        }
        _ => false,
    }
}

/// Frases que cierran la sesión por voz (HU-17). No son un mute duro: el
/// micrófono sigue abierto esperando el wake-word.
const STANDBY_PHRASES: &[&str] = &[
    "modo silencio",
    "ponte en silencio",
    "deja de escuchar",
    "dejá de escuchar",
    "silénciate",
    "silenciate",
    "cállate",
    "callate",
    "descansa",
    "descansá",
];

const TIME_PHRASES: &[&str] = &[
    "qué hora es",
    "que hora es",
    "dime la hora",
    "qué hora tienes",
    "que hora tienes",
];

/// Qué tareas cancela [`LocalCommand::CancelTask`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelTarget {
    /// Todas las tareas en curso ("cancela todo", "detén los procesos").
    All,
    /// La tarea en curso más reciente ("detén el proceso", "para la tarea").
    Latest,
    /// La tarea cuyo título coincide con lo dicho tras "de" ("cancela la
    /// tarea de los correos").
    Named(String),
}

/// Verbos que cancelan tareas pesadas en cualquier posición (HU-37).
const CANCEL_TASK_VERBS: &[&str] = &["cancela", "cancelar", "cancelame"];

/// Verbos de cancelación que solo cuentan al inicio de la orden (HU-57.3):
/// "para"/"termina" son demasiado comunes dentro de una frase ("un cable
/// para el monitor").
const CANCEL_TASK_LEADING_VERBS: &[&str] = &[
    "cancela",
    "cancelar",
    "cancelame",
    "deten",
    "detener",
    "detenme",
    "para",
    "parar",
    "parame",
    "termina",
    "terminar",
    "aborta",
    "abortar",
    "mata",
    "matar",
];

/// Muletillas que pueden preceder al verbo de cancelación: "ok, detén el
/// proceso", "oye Jarvis, por favor para la tarea".
const CANCEL_TASK_LEAD_FILLERS: &[&str] = &[
    "ok", "okay", "vale", "bueno", "listo", "oye", "jarvis", "por", "favor", "porfa", "ya",
    "ahora", "y", "mejor", "puedes", "podrias", "quiero", "que",
];

/// Determinantes entre el verbo de cancelación y el sustantivo.
const CANCEL_TASK_DETERMINERS: &[&str] = &[
    "el", "la", "los", "las", "ese", "esa", "esos", "esas", "este", "esta", "estos", "estas", "mi",
    "mis", "tu", "tus", "un", "una",
];

/// Sustantivos que nombran una tarea pesada; los plurales cancelan todas.
const CANCEL_TASK_NOUNS: &[&str] = &["tarea", "proceso", "trabajo", "monitor", "ping", "contador"];
const CANCEL_TASK_PLURAL_NOUNS: &[&str] = &[
    "tareas",
    "procesos",
    "trabajos",
    "monitores",
    "pings",
    "contadores",
];

/// Frases que piden listar las tareas pesadas.
const LIST_TASK_PHRASES: &[&str] = &[
    "qué tareas tengo",
    "que tareas tengo",
    "tareas pendientes",
    "lista de tareas",
    "qué tareas hay",
    "que tareas hay",
    "tareas activas",
    "dime las tareas",
    "cómo va el desarrollo",
    "como va el desarrollo",
    "en qué estado va el desarrollo",
    "en que estado va el desarrollo",
    "estado del desarrollo",
    "estado de las tareas",
    "cómo va la tarea",
    "como va la tarea",
    "cómo va el swarm",
    "como va el swarm",
];

/// Verbos que piden abrir una aplicación (texto ya sin acentos).
const OPEN_VERBS: &[&str] = &[
    "abre", "abrir", "abreme", "abri", "lanza", "ejecuta", "inicia", "arranca",
];

/// Verbos que piden cerrar una aplicación (texto ya sin acentos).
const CLOSE_VERBS: &[&str] = &["cierra", "cerrar", "cierrame"];

/// "Pon" solo abre Spotify ("pon Spotify"); "pon la terminal" no es natural.
const PLAY_VERBS: &[&str] = &["pon", "ponme"];

/// Palabras entre el verbo y la aplicación que no cambian el sentido.
const OPEN_FILLERS: &[&str] = &[
    "el", "la", "los", "las", "un", "una", "me", "mi", "carpeta", "de",
];

/// Aplicaciones que se abren localmente, por la primera palabra que las
/// nombra tras el verbo ("abre *la terminal*").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum App {
    Spotify,
    Editor,
    Kate,
    Terminal,
    Browser,
    Zen,
    Telegram,
    Discord,
    Steam,
    Calculator,
    Files,
    Downloads,
}

const APP_KEYWORDS: &[(&str, App)] = &[
    ("spotify", App::Spotify),
    ("editor", App::Editor),
    ("bloc", App::Editor),
    ("kwrite", App::Editor),
    ("kate", App::Kate),
    ("terminal", App::Terminal),
    ("consola", App::Terminal),
    ("konsole", App::Terminal),
    ("navegador", App::Browser),
    ("browser", App::Browser),
    ("internet", App::Browser),
    ("chrome", App::Browser),
    ("google", App::Browser),
    ("firefox", App::Browser),
    ("zen", App::Zen),
    ("telegram", App::Telegram),
    ("discord", App::Discord),
    ("steam", App::Steam),
    ("calculadora", App::Calculator),
    ("kcalc", App::Calculator),
    ("explorador", App::Files),
    ("archivos", App::Files),
    ("carpetas", App::Files),
    ("dolphin", App::Files),
    ("descargas", App::Downloads),
];

/// Minúsculas y sin tildes: "Ábreme el Explorador" → "abreme el explorador".
fn fold_accents(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'ä' | 'â' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            'ñ' => 'n',
            c => c,
        })
        .collect()
}

/// Cómo se nombra el navegador en las respuestas habladas.
const BROWSER_NAME: &str = "el navegador";

/// La aplicación pedida: un verbo de apertura seguido (salvo artículos) del
/// nombre de la aplicación. "¿Cómo abro Spotify?" o "abre el menú" no cuentan.
fn requested_app(words: &[&str]) -> Option<App> {
    words.iter().enumerate().find_map(|(i, word)| {
        let play = PLAY_VERBS.contains(word);
        if !play && !OPEN_VERBS.contains(word) {
            return None;
        }
        let target = words[i + 1..].iter().find(|w| !OPEN_FILLERS.contains(w))?;
        let app = APP_KEYWORDS
            .iter()
            .find(|(keyword, _)| keyword == target)
            .map(|(_, app)| *app)?;
        (!play || app == App::Spotify).then_some(app)
    })
}

/// La aplicación que se pide cerrar: un verbo de cierre seguido (salvo
/// artículos) del nombre de la aplicación. "¿Cómo cierro Spotify?" no cuenta.
fn requested_close(words: &[&str]) -> Option<App> {
    words.iter().enumerate().find_map(|(i, word)| {
        if !CLOSE_VERBS.contains(word) {
            return None;
        }
        let target = words[i + 1..].iter().find(|w| !OPEN_FILLERS.contains(w))?;
        APP_KEYWORDS
            .iter()
            .find(|(keyword, _)| keyword == target)
            .map(|(_, app)| *app)
    })
}

/// Solo Spotify, el editor, la terminal y el navegador se cierran localmente;
/// cualquier otro cierre queda para el LLM.
fn close_app_command(app: App) -> Option<LocalCommand> {
    let (pattern, display_name) = match app {
        App::Spotify => ("spotify", "Spotify"),
        App::Editor | App::Kate => ("kwrite|kate", "el editor de texto"),
        App::Terminal => ("konsole", "la terminal"),
        App::Browser | App::Zen => (
            "zen|zen-bin|chrome|google-chrome|firefox|firefox-bin",
            BROWSER_NAME,
        ),
        _ => return None,
    };
    Some(LocalCommand::CloseApp {
        pattern: pattern.into(),
        display_name: display_name.into(),
    })
}

/// `argv` que cierra los procesos cuyo nombre coincide exactamente con
/// `pattern` (sin distinguir mayúsculas). Se compara el nombre del proceso y
/// no la línea de comandos completa para no matar procesos ajenos que solo
/// mencionan la aplicación en sus argumentos.
pub fn close_app_argv(pattern: &str) -> Vec<String> {
    vec!["pkill".into(), "-i".into(), "-x".into(), pattern.into()]
}

/// Primer ejecutable existente de `candidates` (rutas absolutas o nombres que
/// se buscan en `PATH`); si ninguno existe, el último como respaldo.
fn first_available(candidates: &[String]) -> String {
    let in_path = |name: &str| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
    };
    candidates
        .iter()
        .find(|c| {
            if c.contains('/') {
                std::path::Path::new(c.as_str()).is_file()
            } else {
                in_path(c)
            }
        })
        .or(candidates.last())
        .cloned()
        .unwrap_or_default()
}

fn open_app_command(app: App) -> LocalCommand {
    let home = std::env::var("HOME").unwrap_or_default();
    let (command, display_name): (Vec<String>, &str) = match app {
        App::Spotify => (
            vec![first_available(&[
                format!("{home}/.local/opt/spotify/spotify"),
                format!("{home}/.local/bin/spotify"),
                "spotify".into(),
            ])],
            "Spotify",
        ),
        App::Editor => (
            vec![first_available(&["kwrite".into(), "kate".into()])],
            "el editor de texto",
        ),
        App::Kate => (vec!["kate".into()], "el editor de texto"),
        App::Terminal => (vec!["konsole".into()], "la terminal"),
        App::Browser => (
            vec!["xdg-open".into(), "https://google.com".into()],
            BROWSER_NAME,
        ),
        App::Zen if std::path::Path::new("/opt/zen/zen").is_file() => {
            (vec!["/opt/zen/zen".into()], BROWSER_NAME)
        }
        App::Zen => (
            vec!["xdg-open".into(), "https://google.com".into()],
            BROWSER_NAME,
        ),
        App::Telegram => (vec!["telegram-desktop".into()], "Telegram"),
        App::Discord => (vec!["discord".into()], "Discord"),
        App::Steam => (vec!["steam".into()], "Steam"),
        App::Calculator => (vec!["kcalc".into()], "la calculadora"),
        App::Files => (vec!["dolphin".into()], "el explorador de archivos"),
        App::Downloads => (
            vec!["xdg-open".into(), format!("{home}/Descargas")],
            "la carpeta de Descargas",
        ),
    };
    LocalCommand::OpenApp {
        command,
        display_name: display_name.into(),
    }
}

/// Reconoce la palabra "silencio" como orden aislada de reposo: "silencio",
/// "silencio por favor", "por favor silencio", "guarda silencio", "haz
/// silencio", "ya silencio", etc. No cuenta dentro de preguntas o frases como
/// "¿qué es el silencio?", "el silencio" o "silencio absoluto mientras
/// trabajo" (HU-23, HU-Fix-Silencio).
fn is_standalone_silence(text: &str) -> bool {
    let folded = fold_accents(text);
    let words: Vec<&str> = folded.split_whitespace().collect();
    if words.is_empty() {
        return false;
    }
    const FILLERS: &[&str] = &["por", "favor", "porfa", "porfavor", "ya", "ahora", "mismo"];
    const SILENCE_VERBS: &[&str] = &["guarda", "guardar", "haz", "hacer", "manten", "mantener"];

    // "deja de hablar" / "dejá de hablar": verbo de cierre directo.
    if words[0] == "deja" && words.len() >= 3 && words[1] == "de" && words[2] == "hablar" {
        return words[3..].iter().all(|w| FILLERS.contains(w));
    }

    // El resto de órdenes giran en torno a la palabra "silencio".
    let Some(pos) = words.iter().position(|w| *w == "silencio") else {
        return false;
    };
    let before = &words[..pos];
    let after = &words[pos + 1..];
    let before_ok = before.iter().all(|w| FILLERS.contains(w))
        || (before.len() == 1 && SILENCE_VERBS.contains(&before[0]));
    let after_ok = after.iter().all(|w| FILLERS.contains(w));
    before_ok && after_ok
}

/// Reconoce una orden local en la consulta (ya sin el vocativo).
///
/// Aplica las restricciones duras del turno (HU-71): con `no_web` activo
/// nunca devuelve una orden que abra la web ni el navegador.
pub fn local_command(query: &str) -> Option<LocalCommand> {
    let command = unconstrained_local_command(query)?;
    if turn_constraints(query).no_web && command.opens_web() {
        return None;
    }
    Some(command)
}

fn unconstrained_local_command(query: &str) -> Option<LocalCommand> {
    let text: String = query
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let text = format!(
        " {} ",
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    let has = |phrases: &[&str]| phrases.iter().any(|p| text.contains(&format!(" {p} ")));
    // Prioridad absoluta: el kill switch se reconoce antes que nada (CA-38.5).
    {
        let folded = fold_accents(&text);
        let words: Vec<&str> = folded.split_whitespace().collect();
        // "cancela todo" sigue siendo cancelar todas las tareas (HU-37).
        let cancels_tasks = words.first() == Some(&"cancela") && words.contains(&"todo");
        if !cancels_tasks && is_stop_automation(&words) {
            return Some(LocalCommand::StopAutomation);
        }
    }
    // La palabra aislada "silencio" solo cierra la sesión cuando es toda la
    // orden ("silencio", "silencio por favor"); nunca dentro de una frase como
    // "silencio absoluto mientras trabajo", ni en preguntas como "¿qué es el
    // silencio?" o "el silencio" (HU-23).
    if has(STANDBY_PHRASES) || is_standalone_silence(&text) {
        Some(LocalCommand::Standby)
    } else if has(TIME_PHRASES) {
        Some(LocalCommand::Time)
    } else if let Some(command) = cancel_task_command(&text) {
        Some(command)
    } else if has(LIST_TASK_PHRASES) {
        Some(LocalCommand::ListTasks)
    } else if let Some(command) = web_or_automation_command(query) {
        Some(command)
    } else {
        let folded = fold_accents(&text);
        let words: Vec<&str> = folded.split_whitespace().collect();
        // Con un verbo de cierre nunca se abre nada: "cierra Discord y abre
        // Spotify" va entero al LLM en vez de ejecutarse a medias.
        if words.iter().any(|w| CLOSE_VERBS.contains(w)) {
            requested_close(&words).and_then(close_app_command)
        } else {
            requested_app(&words).map(open_app_command)
        }
    }
}

/// Órdenes de anulación prioritaria (HU-Fix-Silencio): reposo, kill switch y
/// cancelación de tareas. Nunca deben fusionarse con un turno pendiente: se
/// ejecutan de inmediato y en silencio, sin pasar por el LLM.
fn is_priority_override(query: &str) -> bool {
    matches!(
        local_command(query),
        Some(LocalCommand::Standby)
            | Some(LocalCommand::StopAutomation)
            | Some(LocalCommand::CancelTask { .. })
    )
}

/// Reconoce "cancela la tarea de …", "detén el proceso" o "cancela todas
/// las tareas" (HU-37, HU-57.3). "Cancela" vale en cualquier posición; el resto
/// de verbos solo al inicio, tras muletillas.
fn cancel_task_command(text: &str) -> Option<LocalCommand> {
    let folded = fold_accents(text);
    let words: Vec<&str> = folded.split_whitespace().collect();
    let lead = words
        .iter()
        .position(|w| !CANCEL_TASK_LEAD_FILLERS.contains(w))?;
    let pos = if CANCEL_TASK_LEADING_VERBS.contains(&words[lead]) {
        lead
    } else {
        words.iter().position(|w| CANCEL_TASK_VERBS.contains(w))?
    };
    // Tras el verbo solo caben determinantes antes del sustantivo: "detén
    // el proceso" sí, "para qué sirve el ping" no.
    let rest = &words[pos + 1..];
    let i = rest
        .iter()
        .position(|w| !CANCEL_TASK_DETERMINERS.contains(w))?;
    if rest[..=i]
        .iter()
        .any(|w| matches!(*w, "todas" | "todo" | "todos"))
    {
        return Some(LocalCommand::CancelTask {
            target: CancelTarget::All,
        });
    }
    let plural = if CANCEL_TASK_NOUNS.contains(&rest[i]) {
        false
    } else if CANCEL_TASK_PLURAL_NOUNS.contains(&rest[i]) {
        true
    } else {
        return None;
    };
    let after = &rest[i + 1..];
    let named = match after.first() {
        Some(&"de") | Some(&"del") => {
            let mut j = 1;
            while j < after.len() && ["la", "las", "los", "el", "una", "un"].contains(&after[j]) {
                j += 1;
            }
            (j < after.len()).then(|| after[j..].join(" "))
        }
        _ => None,
    };
    let target = match named {
        Some(name) => CancelTarget::Named(name),
        None if plural => CancelTarget::All,
        None => CancelTarget::Latest,
    };
    Some(LocalCommand::CancelTask { target })
}

/// Palabra hablada: la original (mayúsculas, tildes y puntuación, para la
/// búsqueda o el texto a teclear) y su clave de comparación (minúsculas, sin
/// tildes ni puntuación).
struct Token<'a> {
    raw: &'a str,
    key: String,
}

fn tokenize(query: &str) -> Vec<Token<'_>> {
    query
        .split_whitespace()
        .filter_map(|raw| {
            let alnum: String = raw.chars().filter(|c| c.is_alphanumeric()).collect();
            let key = fold_accents(&alnum);
            (!key.is_empty()).then_some(Token { raw, key })
        })
        .collect()
}

/// Une las palabras originales sin la puntuación de los extremos
/// ("Marvel Rivals." → "Marvel Rivals").
fn join_raw(tokens: &[Token<'_>]) -> String {
    tokens
        .iter()
        .map(|t| t.raw)
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| ".,;:!?¿¡\"'«»".contains(c))
        .to_string()
}

/// Codifica `text` para una query string (`application/x-www-form-urlencoded`).
pub fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Verbos que llevan a un sitio sin buscar nada ("metete a YouTube").
const NAV_VERBS: &[&str] = &[
    "abre", "abrir", "abreme", "ve", "vete", "anda", "ir", "vamos", "metete", "meterte", "entra",
    "entrar", "ingresa", "ingresar", "llevame", "inicio",
];

/// Verbos de búsqueda en YouTube ("pon X en YouTube" también busca).
const YOUTUBE_VERBS: &[&str] = &[
    "busca",
    "buscame",
    "buscar",
    "busques",
    "busqueda",
    "pon",
    "ponme",
    "reproduce",
    "reproduceme",
    "reproducir",
];

/// Verbos de búsqueda en Google.
const GOOGLE_VERBS: &[&str] = &[
    "busca", "buscame", "buscar", "busques", "busqueda", "googlea", "googlear",
];

/// Palabras que nombran el sitio de la búsqueda.
const YOUTUBE_SITES: &[&str] = &["youtube"];
const GOOGLE_SITES: &[&str] = &["google", "internet", "web"];

/// Conectores que rodean la consulta y no forman parte de ella:
/// "busca *en* YouTube", "con una búsqueda *de* X".
const SEARCH_EDGE_FILLERS: &[&str] = &[
    "de", "del", "en", "con", "una", "un", "busqueda", "sobre", "y", "por", "a", "al", "la", "el",
    "desde",
];

/// Consultas que no buscan nada concreto: "un video al azar" abre el inicio.
const RANDOM_WORDS: &[&str] = &["azar", "cualquier", "cualquiera", "aleatorio", "random"];
const GENERIC_WORDS: &[&str] = &["un", "una", "video", "videos", "algo", "cancion", "musica"];

fn trim_edges<'t, 'a>(mut words: &'t [Token<'a>]) -> &'t [Token<'a>] {
    while let Some((first, rest)) = words.split_first() {
        if !SEARCH_EDGE_FILLERS.contains(&first.key.as_str()) {
            break;
        }
        words = rest;
    }
    while let Some((last, rest)) = words.split_last() {
        if !SEARCH_EDGE_FILLERS.contains(&last.key.as_str()) {
            break;
        }
        words = rest;
    }
    words
}

/// Petición sobre un sitio: `Some(Some(query))` = buscar, `Some(None)` = ir
/// al inicio (solo si `home` y hay un verbo de navegación), `None` = no es
/// una petición a ese sitio.
fn site_request(
    tokens: &[Token<'_>],
    sites: &[&str],
    verbs: &[&str],
    home: bool,
) -> Option<Option<String>> {
    let site = tokens
        .iter()
        .position(|t| sites.contains(&t.key.as_str()))?;
    // Solo cuentan los verbos en forma afirmativa (HU-71, CA-71.3): "no
    // busques en Google" no es una petición a Google.
    let affirmative =
        |i: usize, list: &[&str]| list.contains(&tokens[i].key.as_str()) && !is_negated(tokens, i);
    let Some(verb) = (0..tokens.len()).find(|&i| affirmative(i, verbs)) else {
        let navigates = (0..site).any(|i| affirmative(i, NAV_VERBS));
        return (home && navigates).then_some(None);
    };
    // "busca X en YouTube" → entre el verbo y el sitio; "busca en YouTube X"
    // y "métete a YouTube y busca X" → lo que sigue.
    let span = if site > verb {
        let between = trim_edges(&tokens[verb + 1..site]);
        if between.is_empty() {
            trim_edges(&tokens[site + 1..])
        } else {
            between
        }
    } else {
        trim_edges(&tokens[verb + 1..])
    };
    let random = span.iter().any(|t| RANDOM_WORDS.contains(&t.key.as_str()))
        || (!span.is_empty() && span.iter().all(|t| GENERIC_WORDS.contains(&t.key.as_str())));
    if span.is_empty() || random {
        return home.then_some(None);
    }
    Some(Some(join_raw(span)))
}

/// Palabras de cortesía o vocativos que pueden abrir la orden.
const LEAD_FILLERS: &[&str] = &["oye", "jarvis", "por", "favor", "porfa", "ahora", "ya"];

/// Palabras que acompañan a una orden multimedia sin cambiarla.
const MEDIA_FILLERS: &[&str] = &[
    "el",
    "la",
    "lo",
    "los",
    "las",
    "un",
    "video",
    "videos",
    "musica",
    "cancion",
    "tema",
    "reproduccion",
    "por",
    "favor",
    "porfa",
    "ya",
    "de",
    "nuevo",
    "otra",
    "vez",
    "oye",
    "jarvis",
    "ahora",
    "a",
    "al",
    "esta",
    "este",
    "eso",
];

/// Sustantivos que confirman que la orden es sobre la reproducción.
const MEDIA_NOUNS: &[&str] = &[
    "video",
    "videos",
    "musica",
    "cancion",
    "tema",
    "reproduccion",
];

/// "reproduce", "pausa el video", "siguiente canción"… La orden debe ser solo
/// eso (salvo muletillas): "reproduce Queen" o "pausa para pensar" no cuentan.
fn media_command(tokens: &[Token<'_>]) -> Option<ScreenAction> {
    let noun = tokens.iter().any(|t| MEDIA_NOUNS.contains(&t.key.as_str()));
    let core: Vec<&str> = tokens
        .iter()
        .map(|t| t.key.as_str())
        .filter(|k| !MEDIA_FILLERS.contains(k))
        .collect();
    let action = match core.as_slice() {
        ["reproduce" | "reproducir" | "reanuda" | "reanudar" | "reanudalo" | "play"]
        | ["pon" | "ponle" | "dale", "play"] => ScreenAction::Play,
        ["continua" | "continuar" | "sigue"] if noun => ScreenAction::Play,
        ["pausa" | "pausar" | "pausalo" | "pausala"] | ["pon" | "ponle" | "dale", "pausa"] => {
            ScreenAction::Pause
        }
        ["deten" | "detener" | "para" | "parar" | "detene"] if noun => ScreenAction::Pause,
        ["siguiente" | "proxima" | "proximo"]
        | ["pasa" | "pon" | "salta" | "cambia" | "pasate", "siguiente" | "proxima" | "proximo"]
        | ["salta" | "saltate" | "cambia" | "pasa"]
            if noun =>
        {
            ScreenAction::Next
        }
        ["anterior"] | ["pon" | "vuelve" | "volver", "anterior"] if noun => ScreenAction::Previous,
        _ => return None,
    };
    Some(action)
}

/// Verbos de escritura en pantalla.
const TYPE_VERBS: &[&str] = &[
    "escribe",
    "escribeme",
    "escribir",
    "teclea",
    "teclear",
    "tipea",
    "tipear",
    "digita",
    "digitar",
];

/// Verbos que piden intentarlo: "*prueba* escribir …", "*intenta* buscar …".
const TRY_VERBS: &[&str] = &["prueba", "intenta", "trata", "puedes", "podrias"];

/// Lugar explícito tras el verbo: "escribe *en pantalla* …".
const TYPE_PLACES: &[&[&str]] = &[
    &["en", "pantalla"],
    &["en", "la", "pantalla"],
    &["aqui"],
    &["ahi"],
    &["lo", "siguiente"],
    &["esto"],
];

/// Campos de texto nombrables: "en *la caja de texto*", "en *el buscador*".
const FIELD_NOUNS: &[&str] = &["caja", "buscador", "campo", "cuadro", "barra", "casilla"];
const FIELD_ARTICLES: &[&str] = &["el", "la", "esta", "este", "esa", "ese"];
const FIELD_KINDS: &[&str] = &["texto", "busqueda", "dialogo"];

/// Inicios que piden redactar (LLM), no teclear literal: "escribe un correo".
const COMPOSE_STARTS: &[&str] = &[
    "un", "una", "unos", "unas", "me", "le", "les", "sobre", "acerca", "algo", "como",
];

/// Piezas que se redactan, no se teclean: "escribe código para …".
const COMPOSE_NOUNS: &[&str] = &[
    "correo", "correos", "mail", "email", "carta", "mensaje", "codigo", "poema", "cuento",
    "ensayo", "resumen", "funcion", "script", "programa", "historia",
];

/// Lo que sigue a "algo" cuando se pide redactar: "escribe algo *sobre* X".
const COMPOSE_AFTER_ALGO: &[&str] = &[
    "sobre",
    "acerca",
    "de",
    "para",
    "que",
    "como",
    "bonito",
    "lindo",
    "corto",
    "largo",
    "gracioso",
    "interesante",
    "creativo",
];

/// Índice tras el verbo si la orden abre (salvo muletillas e "intenta…")
/// con uno de `verbs`. "¿cómo se escribe X?" no cuenta.
fn leading_verb_end(tokens: &[Token<'_>], verbs: &[&str]) -> Option<usize> {
    let mut i = tokens
        .iter()
        .position(|t| !LEAD_FILLERS.contains(&t.key.as_str()))?;
    if TRY_VERBS.contains(&tokens[i].key.as_str()) {
        i += 1;
        if tokens.get(i).is_some_and(|t| t.key == "a" || t.key == "de") {
            i += 1;
        }
    }
    verbs
        .contains(&tokens.get(i)?.key.as_str())
        .then_some(i + 1)
}

/// Palabras que ocupa un lugar de escritura al inicio de `rest`.
fn place_len(rest: &[Token<'_>]) -> Option<usize> {
    let keys: Vec<&str> = rest.iter().map(|t| t.key.as_str()).collect();
    if let Some(place) = TYPE_PLACES.iter().find(|p| keys.starts_with(p)) {
        return Some(place.len());
    }
    // "en [la] caja [de texto]".
    if keys.first() != Some(&"en") {
        return None;
    }
    let mut i = 1;
    if keys.get(i).is_some_and(|k| FIELD_ARTICLES.contains(k)) {
        i += 1;
    }
    if !keys.get(i).is_some_and(|k| FIELD_NOUNS.contains(k)) {
        return None;
    }
    i += 1;
    if keys.get(i) == Some(&"de") && keys.get(i + 1).is_some_and(|k| FIELD_KINDS.contains(k)) {
        i += 2;
    }
    Some(i)
}

/// "escribe en pantalla hola mundo" / "prueba escribir hola" / "escribe en
/// el buscador X" / "escribe X en la caja de texto". Pedir que se redacte
/// algo ("escribe un correo…") va al LLM.
fn type_command(tokens: &[Token<'_>]) -> Option<ScreenAction> {
    let mut rest = &tokens[leading_verb_end(tokens, TYPE_VERBS)?..];
    let mut explicit = false;
    while let Some(len) = place_len(rest) {
        rest = &rest[len..];
        explicit = true;
    }
    // "escribe algo: hola" teclea; "escribe algo sobre X" se redacta.
    if rest.first().is_some_and(|t| t.key == "algo")
        && rest
            .get(1)
            .is_some_and(|t| !COMPOSE_AFTER_ALGO.contains(&t.key.as_str()))
    {
        rest = &rest[1..];
        explicit = true;
    }
    // Lugar al final: "escribe hola mundo en el buscador".
    if let Some(at) = (1..rest.len()).find(|&i| place_len(&rest[i..]) == Some(rest.len() - i)) {
        rest = &rest[..at];
    }
    let first = rest.first()?;
    if !explicit
        && (COMPOSE_STARTS.contains(&first.key.as_str())
            || COMPOSE_NOUNS.contains(&first.key.as_str()))
    {
        return None;
    }
    let text = rest
        .iter()
        .map(|t| t.raw)
        .collect::<Vec<_>>()
        .join(" ")
        .trim_start_matches(|c: char| c == ':' || c == ',' || c == '-' || c.is_whitespace())
        .trim_end_matches('.')
        .to_string();
    (!text.is_empty()).then_some(ScreenAction::Type(text))
}

/// "escribe en YouTube X": teclear en un sitio es buscar en él. Devuelve la
/// consulta, vacía si no la hay (`None` si la orden no es esa).
fn typed_site_query(tokens: &[Token<'_>], sites: &[&str]) -> Option<String> {
    let rest = &tokens[leading_verb_end(tokens, TYPE_VERBS)?..];
    let keys: Vec<&str> = rest.iter().map(|t| t.key.as_str()).collect();
    let site = match keys.as_slice() {
        ["en", site, ..] if sites.contains(site) => 2,
        ["en", "el", site, ..] if sites.contains(site) => 3,
        _ => return None,
    };
    Some(join_raw(trim_edges(&rest[site..])))
}

/// "busca en el buscador X" → teclea X en el campo con foco. "Busca X" sin
/// sitio ya no abre Google: va al router y el cerebro decide (HU-71, CA-71.1).
fn search_in_field(tokens: &[Token<'_>]) -> Option<ScreenAction> {
    let rest = &tokens[leading_verb_end(tokens, GOOGLE_VERBS)?..];
    let len = place_len(rest).filter(|_| rest.first().is_some_and(|t| t.key == "en"))?;
    let query = trim_edges(&rest[len..]);
    (!query.is_empty()).then(|| ScreenAction::Type(join_raw(query)))
}

// ─── Restricciones negativas del turno (HU-71) ──────────────────────────────

/// Palabras que niegan el verbo que sigue en su cláusula.
const NEGATORS: &[&str] = &["no", "nunca", "jamas", "tampoco", "sin", "ni"];

/// Palabras que pueden ir entre la negación y el verbo sin romperla: clíticos
/// ("no *lo* busques"), perífrasis ("no *vayas a* buscar", "no *hace falta
/// que* busques", "ni *se te ocurra* googlearlo") y cortesía.
const NEGATION_BRIDGE: &[&str] = &[
    "lo",
    "la",
    "los",
    "las",
    "le",
    "les",
    "me",
    "te",
    "se",
    "nos",
    "ya",
    "mas",
    "por",
    "favor",
    "porfa",
    "que",
    "quiero",
    "vayas",
    "a",
    "de",
    "ocurra",
    "hace",
    "falta",
    "hay",
    "es",
    "necesario",
    "necesito",
    "trates",
    "intentes",
    "vuelvas",
    "sigas",
    "tienes",
    "jarvis",
    "oye",
];

/// Conectores que abren una cláusula nueva: la negación no los cruza.
const CLAUSE_BREAKS: &[&str] = &["pero", "sino", "y", "o", "mejor", "entonces", "luego"];

/// Verbos de navegación negables: "no *abras* YouTube", "no *vayas* a internet".
const WEB_NAV_VERBS: &[&str] = &[
    "abre",
    "abras",
    "abrir",
    "abrirlo",
    "abrela",
    "abrelo",
    "abreme",
    "ve",
    "vayas",
    "ir",
    "entra",
    "entres",
    "entrar",
    "metete",
    "metas",
    "meterte",
    "navega",
    "navegues",
    "navegar",
    "usa",
    "uses",
    "usar",
    "consulta",
    "consultes",
    "consultar",
    "pon",
    "pongas",
    "reproduce",
    "reproduzcas",
];

/// Palabras que nombran la web o el navegador en una cláusula.
const WEB_WORDS: &[&str] = &[
    "google",
    "internet",
    "web",
    "youtube",
    "navegador",
    "browser",
    "chrome",
    "firefox",
    "zen",
    "pagina",
    "sitio",
    "online",
    "linea",
];

/// `true` si el token abre cláusula nueva: puntuación al final del anterior
/// ("no sé, búscalo…") o conector ("…pero busca…").
fn starts_clause(tokens: &[Token<'_>], i: usize) -> bool {
    i == 0
        || tokens[i - 1].raw.ends_with(|c: char| ",;.:!?".contains(c))
        || CLAUSE_BREAKS.contains(&tokens[i].key.as_str())
}

/// `true` si el verbo de `tokens[verb]` está negado en su cláusula: una
/// negación lo precede y entre ambos solo hay [`NEGATION_BRIDGE`].
///
/// "no sé" no niega lo que sigue: "sé" con tilde no es el clítico "se".
fn is_negated(tokens: &[Token<'_>], verb: usize) -> bool {
    let mut i = verb;
    while !starts_clause(tokens, i) {
        i -= 1;
        let token = &tokens[i];
        if NEGATORS.contains(&token.key.as_str()) {
            return true;
        }
        let accented_se = token.key == "se" && token.raw.to_lowercase().contains('é');
        if accented_se || !NEGATION_BRIDGE.contains(&token.key.as_str()) {
            return false;
        }
    }
    false
}

/// Rango `[inicio, fin)` de la cláusula que contiene `tokens[i]`.
fn clause_of(tokens: &[Token<'_>], i: usize) -> std::ops::Range<usize> {
    let start = (0..=i)
        .rev()
        .find(|&j| starts_clause(tokens, j))
        .unwrap_or(0);
    let end = (i + 1..tokens.len())
        .find(|&j| starts_clause(tokens, j))
        .unwrap_or(tokens.len());
    start..end
}

/// `true` si el turno prohíbe la web: un verbo de búsqueda negado ("no lo
/// busques", "sin buscar", "ni se te ocurra googlearlo") o un verbo de
/// navegación negado en una cláusula que nombra la web ("no abras YouTube").
fn forbids_web(tokens: &[Token<'_>]) -> bool {
    (0..tokens.len()).any(|i| {
        let key = tokens[i].key.as_str();
        // "busca", "búscalo", "busques" (busq-), "googlearlo"…
        let search = ((key.starts_with("busc") || key.starts_with("busq"))
            && !key.starts_with("buscador"))
            || key.starts_with("googl");
        let navigates = WEB_NAV_VERBS.contains(&key)
            && tokens[clause_of(tokens, i)]
                .iter()
                .any(|t| WEB_WORDS.contains(&t.key.as_str()));
        (search || navigates) && is_negated(tokens, i)
    })
}

/// Petición web afirmativa, explícita y con sitio en el mismo turno.
fn explicit_site_request(tokens: &[Token<'_>]) -> bool {
    typed_site_query(tokens, YOUTUBE_SITES).is_some()
        || typed_site_query(tokens, GOOGLE_SITES).is_some()
        || site_request(tokens, YOUTUBE_SITES, YOUTUBE_VERBS, true).is_some()
        || matches!(
            site_request(tokens, GOOGLE_SITES, GOOGLE_VERBS, false),
            Some(Some(_))
        )
}

/// Restricciones duras del turno (HU-71, CA-71.4), extraídas del texto
/// **antes** de [`local_command`] y del router.
///
/// Una negación ("no busques en Google, dime X") activa `no_web`. La única
/// excepción es que el mismo turno pida, en otra cláusula y en afirmativo,
/// un sitio concreto: "no busques en Google, busca en YouTube X" abre YouTube
/// (CA-71.5), porque la orden explícita es lo último que dijo el usuario.
pub fn turn_constraints(query: &str) -> TurnConstraints {
    let tokens = tokenize(query);
    TurnConstraints {
        no_web: forbids_web(&tokens) && !explicit_site_request(&tokens),
    }
}

fn youtube_search(q: &str) -> LocalCommand {
    LocalCommand::OpenUrl {
        url: format!(
            "https://www.youtube.com/results?search_query={}",
            url_encode(q)
        ),
        spoken: format!("Buscando {q} en YouTube."),
    }
}

fn google_search(q: &str) -> LocalCommand {
    LocalCommand::OpenUrl {
        url: format!("https://www.google.com/search?q={}", url_encode(q)),
        spoken: format!("Buscando {q} en Google."),
    }
}

/// Órdenes web (YouTube, Google) y de pantalla (multimedia, escribir) que el
/// orquestador resuelve al instante, sin pasar por el cerebro CLI.
pub fn web_or_automation_command(query: &str) -> Option<LocalCommand> {
    let tokens = tokenize(query);
    let youtube_home = || LocalCommand::OpenUrl {
        url: "https://www.youtube.com".into(),
        spoken: "Abriendo YouTube.".into(),
    };
    if let Some(q) = typed_site_query(&tokens, YOUTUBE_SITES) {
        return Some(if q.is_empty() {
            youtube_home()
        } else {
            youtube_search(&q)
        });
    }
    if let Some(q) = typed_site_query(&tokens, GOOGLE_SITES) {
        return Some(if q.is_empty() {
            LocalCommand::OpenUrl {
                url: "https://www.google.com".into(),
                spoken: "Abriendo Google.".into(),
            }
        } else {
            google_search(&q)
        });
    }
    if let Some(request) = site_request(&tokens, YOUTUBE_SITES, YOUTUBE_VERBS, true) {
        return Some(match request {
            Some(q) => youtube_search(&q),
            None => youtube_home(),
        });
    }
    if let Some(Some(q)) = site_request(&tokens, GOOGLE_SITES, GOOGLE_VERBS, false) {
        return Some(google_search(&q));
    }
    media_command(&tokens)
        .or_else(|| type_command(&tokens))
        .or_else(|| search_in_field(&tokens))
        .map(LocalCommand::Screen)
}

/// Frase con la hora: "Es la 1:05." / "Son las 15:30."
pub fn time_phrase(hour: u32, minute: u32) -> String {
    let (verb, article) = if hour == 1 {
        ("Es", "la")
    } else {
        ("Son", "las")
    };
    format!("{verb} {article} {hour}:{minute:02}.")
}

/// Resultado de resolver una [`SessionRef`] hablada (HU-64).
#[derive(Debug)]
enum Resolution {
    Found(TaskRecord),
    /// Varias sub-sesiones coinciden: hay que preguntar cuál.
    Ambiguous(Vec<TaskRecord>),
    Missing,
}

/// Resuelve una [`SessionRef`] contra las sub-sesiones abiertas por voz (HU-64).
///
/// El roster manda para alias, ordinales ("el primero" = la más antigua,
/// ordenadas por apertura) y "la última"; el id hablado también vale para
/// tareas que no abrió el router (p. ej. `task run`).
fn resolve_session(
    tasks: &TaskManager,
    roster: &[RosterEntry],
    focus: Option<SessionId>,
    reference: &SessionRef,
) -> Resolution {
    let ids = match reference {
        SessionRef::Id(id) => vec![*id],
        other => resolve_in_roster(other, roster, focus),
    };
    let mut records: Vec<TaskRecord> = ids
        .into_iter()
        .filter_map(|id| tasks.get(TaskId::from(id)))
        .collect();
    match records.len() {
        0 => Resolution::Missing,
        1 => Resolution::Found(records.remove(0)),
        _ => Resolution::Ambiguous(records),
    }
}

/// Id de sub-sesión de una tarea del gestor (los ids hablados caben en `u32`).
fn session_id(id: TaskId) -> SessionId {
    SessionId::try_from(id).unwrap_or(SessionId::MAX)
}

/// Máximo de sub-sesiones que se publican al HUD y a la bandeja (HU-67).
pub const MAX_SESSION_VIEWS: usize = 8;

/// Sub-sesiones visibles (HU-67): las vivas (en cola o trabajando) y la
/// enfocada aunque ya haya terminado, por id y con tope
/// [`MAX_SESSION_VIEWS`]. `title` da el título hablado de cada registro.
pub fn session_views(
    records: &[TaskRecord],
    focus: Option<SessionId>,
    title: impl Fn(&TaskRecord) -> String,
) -> Vec<SessionView> {
    let mut views: Vec<SessionView> = records
        .iter()
        .filter(|record| {
            matches!(record.status, TaskStatus::Queued | TaskStatus::Running)
                || focus == Some(session_id(record.id))
        })
        .map(|record| SessionView {
            id: record.id,
            title: title(record),
            status: record.status.clone(),
            focused: focus == Some(session_id(record.id)),
            has_detail: record.detail_path.is_some(),
        })
        .collect();
    views.sort_by_key(|view| view.id);
    if views.len() > MAX_SESSION_VIEWS {
        // Se conservan las más recientes, sin perder la enfocada.
        let focused = views.iter().position(|view| view.focused);
        let keep_from = views.len() - MAX_SESSION_VIEWS;
        match focused {
            Some(i) if i < keep_from => {
                let pinned = views.remove(i);
                views.drain(..keep_from);
                views.insert(0, pinned);
            }
            _ => {
                views.drain(..keep_from);
            }
        }
    }
    views
}

/// Estado en palabras para el roster (§3.5).
fn status_word(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "en cola",
        TaskStatus::Running => "trabajando",
        TaskStatus::Done => "lista",
        TaskStatus::Failed => "falló",
        TaskStatus::Cancelled => "cancelada",
        TaskStatus::TimedOut => "agotó el tiempo",
    }
}

/// Bloque de roster para el contexto del hilo central (HU-65, §3.5).
///
/// Una línea por sub-sesión: `#id título — estado (duración)`, con el avance o
/// el último resultado cuando existen. Las activas van primero y, dentro de
/// cada grupo, la más reciente: es el orden en que el modelo las necesita para
/// responder "¿cómo va…?" o "¿encontró algo grave?".
///
/// Devuelve `None` si no hay sub-sesiones (no se gasta contexto en un bloque
/// vacío) y **nunca** supera `max_chars`, aunque haya cientos de sesiones.
pub fn format_roster_context(tasks: &[TaskRecord], max_chars: usize) -> Option<String> {
    format_roster_context_with_progress(tasks, &HashMap::new(), max_chars)
}

/// Como [`format_roster_context`], con el avance en curso de cada sub-sesión
/// (`PROGRESO:` del subagente). El avance no se habla: alimenta el roster.
pub fn format_roster_context_with_progress(
    tasks: &[TaskRecord],
    progress: &HashMap<TaskId, String>,
    max_chars: usize,
) -> Option<String> {
    if tasks.is_empty() || max_chars == 0 {
        return None;
    }
    let mut ordered: Vec<&TaskRecord> = tasks.iter().collect();
    ordered.sort_by_key(|record| {
        (
            !matches!(record.status, TaskStatus::Queued | TaskStatus::Running),
            std::cmp::Reverse((record.created_at_ms, record.id)),
        )
    });

    let now = now_ms();
    // La cabecera cuenta dentro del presupuesto, igual que cada separador.
    let mut block = String::from(ROSTER_CONTEXT_HEADER);
    let mut used = block.chars().count();
    let mut lines = 0usize;
    for record in ordered {
        let line = roster_line(record, progress.get(&record.id), now);
        let available = max_chars.saturating_sub(used + 1);
        let line = if line.chars().count() <= available {
            line
        } else if lines == 0 && available > 0 {
            // La primera línea siempre entra recortada: mejor contexto parcial
            // que un bloque sin sesiones.
            clip(&line, available)
        } else {
            break;
        };
        used += line.chars().count() + 1;
        block.push('\n');
        block.push_str(&line);
        lines += 1;
    }
    (lines > 0).then_some(block)
}

/// Una línea del roster (§3.5).
fn roster_line(record: &TaskRecord, progress: Option<&String>, now: u64) -> String {
    let mut line = format!(
        "#{} {} — {} ({})",
        record.id,
        clip(&record.title, ROSTER_TITLE_MAX_CHARS),
        status_word(record.status.clone()),
        duration_phrase(record, now)
    );
    if let Some(progress) = progress
        .map(String::as_str)
        .filter(|text| !text.trim().is_empty())
    {
        line.push_str(&format!(
            " — progreso: \"{}\"",
            clip(progress, ROSTER_TEXT_MAX_CHARS)
        ));
    }
    if let Some(summary) = record
        .spoken_summary
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        line.push_str(&format!(
            " — resultado: \"{}\"",
            clip(summary, ROSTER_TEXT_MAX_CHARS)
        ));
    }
    line
}

/// Duración legible de una sub-sesión ("4 min"): hasta su fin si terminó, o
/// hasta ahora si sigue viva.
fn duration_phrase(record: &TaskRecord, now: u64) -> String {
    let end = record.finished_at_ms.unwrap_or(now);
    let secs = end.saturating_sub(record.created_at_ms) / 1_000;
    if secs >= 60 {
        format!("{} min", secs / 60)
    } else {
        format!("{secs} s")
    }
}

/// Milisegundos desde la época (marca de tiempo de los registros de tarea).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// Recorta a `max_chars` caracteres (en una sola línea) con puntos suspensivos.
///
/// Un título o un resumen gigante no puede comerse el presupuesto del bloque.
fn clip(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let mut out: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// "¿La revisión de PR de Felipe o la de Oscar?"
fn disambiguation_phrase(titles: &[String]) -> String {
    match titles {
        [] => NO_SUCH_SESSION_PHRASE.to_string(),
        [one] => format!("¿{one}?"),
        [init @ .., last] => format!("¿{} o {last}?", init.join(", ")),
    }
}

/// Frase con el estado de una sub-sesión ("va en curso", "terminó: …").
fn describe_task(record: &TaskRecord) -> String {
    let title = &record.title;
    match record.status {
        TaskStatus::Queued => format!("{title} está en cola."),
        TaskStatus::Running => format!("{title} sigue en curso."),
        TaskStatus::Done => match record.spoken_summary.as_deref() {
            Some(summary) => format!("{title} terminó: {summary}"),
            None => format!("{title} terminó."),
        },
        TaskStatus::Failed => format!("{title} falló."),
        TaskStatus::Cancelled => format!("{title} se canceló."),
        TaskStatus::TimedOut => format!("{title} tardó demasiado y lo detuve."),
    }
}

/// `true` si el texto termina inacabado: conector, muletilla, preposición,
/// artículo o auxiliar colgado (HU-43). Se conserva el nombre histórico.
pub fn ends_with_connector(text: &str) -> bool {
    crate::incompleteness::is_incomplete(text)
}

/// Envía locuciones al orquestador (IPC `jarvis.speak`, avisos).
#[derive(Clone)]
pub struct VoiceSpeaker {
    tx: Sender<BrainJob>,
    queue: SpeechQueueHandle,
}

impl VoiceSpeaker {
    /// Encola una locución en la cola priorizada (HU-37): se dirá cuando el
    /// *gate* lo permita. Devuelve la posición (1 = la siguiente; 0 = rechazada
    /// por cola llena).
    pub fn enqueue(&self, text: impl Into<String>, priority: SpeechPriority) -> u32 {
        self.queue.push(Delivery::new(text, priority))
    }

    pub fn say(&self, text: impl Into<String>) {
        let _ = self.tx.send(BrainJob::Say(text.into()));
    }
}

/// Cerebro compartido por los hilos y reemplazable en caliente
/// (configuración desde el menú).
type SharedBrain = Arc<dyn TokenStream + Send + Sync>;

#[derive(Clone)]
struct BrainSlot(Arc<RwLock<Option<SharedBrain>>>);

impl BrainSlot {
    fn new(brain: Option<SharedBrain>) -> Self {
        Self(Arc::new(RwLock::new(brain)))
    }

    fn get(&self) -> Option<SharedBrain> {
        self.0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn replace(&self, brain: Option<SharedBrain>) -> Option<SharedBrain> {
        std::mem::replace(
            &mut *self
                .0
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            brain,
        )
    }
}

/// Orquestador en marcha.
pub struct VoiceOrchestrator {
    control_tx: Sender<ControlMsg>,
    stt_tx: Sender<SttJob>,
    brain_tx: Sender<BrainJob>,
    tts_tx: Sender<TtsJob>,
    threads: Vec<(&'static str, JoinHandle<()>)>,
    /// Núcleo compartido: la interrupción manual (HU-56) lo necesita aquí.
    core: SharedCore,
    stats: Arc<VoiceStats>,
    playback: AudioPlayback,
    brain: BrainSlot,
    barge_in: Arc<AtomicBool>,
    stt_ready: Arc<AtomicBool>,
    activation: Arc<ActivationDetector>,
    tasks: Option<TaskManager>,
    gaming_mode: Arc<AtomicBool>,
    queue: SpeechQueueHandle,
    /// Turno de usuario despachado y aún en curso (HU-AUDIO-CONTINUITY).
    pending_user_turn: PendingUserTurn,
    /// El hilo de voz tiene un turno a medio capturar: frase retenida tras un
    /// conector o continuación esperada (HU-55). Lo publica `ControlLoop`.
    turn_open: Arc<AtomicBool>,
}

enum ControlMsg {
    Shutdown,
    /// Interrupción manual (clic en la esfera, HU-56): el hilo de voz olvida
    /// la confirmación pendiente y el segmento marcado como eco.
    Interrupt,
}

enum SttJob {
    Segment {
        pcm: Vec<f32>,
        at: Instant,
        /// Duración del segmento en ms, para calcular cuándo empezó la voz
        /// (HU-29) y comprobar la ventana de seguimiento contra ese instante.
        duration_ms: u32,
        /// Qué parte del turno es (HU-44).
        part: TurnPart,
    },
    SetTranscriber(Box<dyn Transcriber + Send>),
    Shutdown,
}

/// Parte de un turno que llega al STT (HU-44).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnPart {
    /// Fragmento intermedio de un turno largo: se transcribe y acumula.
    Partial(u32),
    /// Cierre del turno tras `n` fragmentos parciales (0 = turno normal).
    Final(u32),
}

/// Texto acumulado de un turno largo mientras sigue abierto (HU-44). Solo
/// guarda texto: el audio de cada fragmento se libera al transcribirlo.
struct LongTurnText {
    text: String,
    started_at: Instant,
}

struct SttOutcome {
    activation: Activation,
    /// La frase quedó a medias (HU-43): se espera la continuación.
    incomplete: bool,
    at: Instant,
}

enum BrainJob {
    Turn {
        query: String,
        at: Instant,
        /// Fragmento nuevo del usuario para el HUD cuando este turno continúa
        /// uno en curso (HU-AUDIO-CONTINUITY). `Some` solo en continuaciones.
        hud_fragment: Option<String>,
    },
    Say(String),
    /// Cambio de voz en caliente (formulario CAMBIAR VOZ).
    SetVoice(String),
    SpeechDone {
        generation: u64,
        outcome: PlayOutcome,
    },
    Shutdown,
}

enum TtsJob {
    Audio {
        generation: u64,
        pcm: PcmAudio,
        /// Subtítulo del HUD que se publica cuando la frase empieza a sonar.
        caption: Option<HudEvent>,
    },
    /// Chime de confirmación de turno aceptado (HU-18), sin generación.
    Chime {
        pcm: PcmAudio,
    },
    End {
        generation: u64,
    },
    Shutdown,
}

impl VoiceOrchestrator {
    /// Lanza los cuatro hilos. La escucha arranca en cuanto haya transcriptor
    /// ([`set_transcriber`](Self::set_transcriber)) y micrófono.
    pub fn start(deps: VoiceDeps, config: VoiceConfig) -> std::io::Result<Self> {
        let VoiceDeps {
            core,
            playback,
            capture,
            sink,
            events,
            mut coordinator,
            brain,
            activation,
            factory,
            hud,
        } = deps;
        let hud = HudTx(hud);
        coordinator.set_deferred_finish(true);
        let brain = BrainSlot::new(brain);
        let barge_in = Arc::new(AtomicBool::new(config.barge_in_enabled));
        let gaming_mode = Arc::new(AtomicBool::new(false));
        let last_voice_activity = Arc::new(Mutex::new(None::<Instant>));
        let pending_user_turn = PendingUserTurn::default();
        let stt_in_flight = Arc::new(AtomicUsize::new(0));
        let turn_open = Arc::new(AtomicBool::new(false));

        let stats = Arc::new(VoiceStats::default());
        let stt_ready = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let (control_tx, control_rx) = crossbeam_channel::unbounded();
        let (stt_tx, stt_rx) = crossbeam_channel::bounded::<SttJob>(4);
        let (stt_out_tx, stt_out_rx) = crossbeam_channel::unbounded();
        let (brain_tx, brain_rx) = crossbeam_channel::unbounded();
        let (tts_tx, tts_rx) = crossbeam_channel::unbounded();
        let queue = SpeechQueueHandle::new();

        // Planificador de turnos compuestos (HU-63). Usa un cerebro propio del
        // *factory* para no interferir con la conversación ni con las tareas, y
        // solo se consulta en turnos compuestos.
        let planner: Option<Arc<dyn TurnPlanner>> = factory.as_ref().map(|factory| {
            Arc::new(BrainTurnPlanner::new(Arc::clone(factory))) as Arc<dyn TurnPlanner>
        });

        // Gestor de tareas pesadas: solo si hay fábrica y configuración. Si no
        // se puede preparar (directorio de estado), la voz sigue sin tareas.
        let tasks = match (factory, config.tasks.clone()) {
            (Some(factory), Some(cfg)) => match TaskManager::start(factory, cfg, queue.clone()) {
                Ok(tasks) => Some(tasks),
                Err(e) => {
                    eprintln!("[tareas] gestor de tareas desactivado: {e}");
                    None
                }
            },
            _ => None,
        };

        let mut threads = Vec::new();
        let spawn = |name: &'static str, f: Box<dyn FnOnce() + Send>| {
            std::thread::Builder::new()
                .name(name.into())
                .spawn(f)
                .map(|join| (name, join))
        };

        {
            let activation = Arc::clone(&activation);
            let stt_ready = Arc::clone(&stt_ready);
            let stt_in_flight = Arc::clone(&stt_in_flight);
            threads.push(spawn(
                "jarvis-stt",
                Box::new(move || {
                    stt_loop(
                        &stt_rx,
                        &stt_out_tx,
                        &activation,
                        &stt_ready,
                        &stt_in_flight,
                    );
                }),
            )?);
        }
        {
            let tts_brain = brain_tx.clone();
            let tts_hud = hud.clone();
            let tts_playback = playback.clone();
            threads.push(spawn(
                "jarvis-tts",
                Box::new(move || {
                    tts_loop(&tts_rx, &tts_brain, sink.as_ref(), &tts_hud, &tts_playback);
                }),
            )?);
        }
        {
            let brain_state = BrainLoop {
                coordinator,
                client: brain.clone(),
                core: core.clone(),
                playback: playback.clone(),
                activation: Arc::clone(&activation),
                tts: tts_tx.clone(),
                clock: Arc::clone(&config.clock),
                launcher: Arc::clone(&config.launcher),
                automation_kill: config.automation_kill.clone(),
                automation_input: config.automation_input.clone(),
                stats: Arc::clone(&stats),
                ready: Arc::clone(&ready),
                speaking: None,
                tasks: tasks.clone(),
                planner,
                focus: None,
                focus_at: None,
                focus_timeout: config.focus_timeout,
                constraints: TurnConstraints::none(),
                threads: HashMap::new(),
                last_assistant_response: None,
                queue: queue.clone(),
                last_voice_activity: Arc::clone(&last_voice_activity),
                gaming_mode: Arc::clone(&gaming_mode),
                quiet_gap: config.tasks.clone().unwrap_or_default().quiet_gap,
                held: false,
                hud,
                hud_turn: 0,
                hud_user_turn: 0,
                hud_open: None,
                pending_user_turn: pending_user_turn.clone(),
                sessions_key: None,
                sessions_shown: Vec::new(),
            };
            threads.push(spawn(
                "jarvis-brain",
                Box::new(move || brain_state.run(&brain_rx)),
            )?);
        }
        {
            let control = ControlLoop {
                core: core.clone(),
                playback: playback.clone(),
                capture,
                activation: Arc::clone(&activation),
                client: brain.clone(),
                barge_in: Arc::clone(&barge_in),
                stt: stt_tx.clone(),
                brain: brain_tx.clone(),
                tts: tts_tx.clone(),
                stats: Arc::clone(&stats),
                stt_ready: Arc::clone(&stt_ready),
                stt_in_flight,
                ready,
                config,
                capture_enabled: None,
                pending: None,
                pending_user_turn: pending_user_turn.clone(),
                stt_warned: false,
                candidate: None,
                echo_segment: false,
                last_sounding: None,
                last_turn_end: None,
                speech_started: None,
                continuation_expected: false,
                chime_deadline: None,
                last_voice_activity,
                turn_open: Arc::clone(&turn_open),
            };
            threads.push(spawn(
                "jarvis-voice",
                Box::new(move || control.run(events, &control_rx, &stt_out_rx)),
            )?);
        }

        Ok(Self {
            control_tx,
            stt_tx,
            brain_tx,
            tts_tx,
            threads,
            core,
            stats,
            playback,
            brain,
            barge_in,
            stt_ready,
            activation,
            tasks,
            gaming_mode,
            queue,
            pending_user_turn,
            turn_open,
        })
    }

    /// Gestor de tareas pesadas (HU-37), si está activo. Es un *handle*
    /// clonable: lo usa el servidor IPC (`jarvis.dispatchTask`…).
    pub fn tasks(&self) -> Option<TaskManager> {
        self.tasks.clone()
    }

    /// `true` si hay un turno de escucha activo (HU-55): la esfera pasa de la
    /// escucha pasiva (verde) a la captura en vivo (ámbar).
    ///
    /// Cuenta como turno activo la ventana de seguimiento abierta (se aceptan
    /// frases sin repetir "Jarvis"), una frase retenida tras un conector, una
    /// continuación esperada o un turno despachado que aún no termina.
    pub fn is_active_turn(&self) -> bool {
        self.turn_open.load(Ordering::Acquire)
            || self.pending_user_turn.is_some()
            || self.activation.is_follow_up_open()
    }

    /// Número de tareas pesadas en cola o en curso (HU-37); 0 sin gestor.
    pub fn active_tasks_count(&self) -> usize {
        self.tasks.as_ref().map_or(0, TaskManager::active_count)
    }

    /// `true` si hay tareas en segundo plano vivas (HU-55).
    pub fn has_background_tasks(&self) -> bool {
        self.tasks
            .as_ref()
            .is_some_and(TaskManager::has_active_tasks)
    }

    /// Modo juego: retiene las locuciones no críticas (CA-37.8).
    pub fn set_gaming_mode(&self, enabled: bool) {
        self.gaming_mode.store(enabled, Ordering::Relaxed);
    }

    /// Reemplaza el cerebro (cambio de configuración). Cancela el turno que
    /// estuviera usando el anterior.
    pub fn set_brain(&self, brain: Option<SharedBrain>) {
        if let Some(old) = self.brain.replace(brain) {
            old.cancel();
        }
    }

    /// `true` si hay cerebro (CLI o API) para las consultas abiertas.
    pub fn has_brain(&self) -> bool {
        self.brain.get().is_some()
    }

    /// Activa o desactiva la interrupción por voz en caliente.
    pub fn set_barge_in(&self, enabled: bool) {
        self.barge_in.store(enabled, Ordering::Relaxed);
    }

    /// Interrupción manual de la locución en curso (HU-56). Un clic simple en
    /// la esfera o un atajo la cortan en el acto, sin esperar la confirmación
    /// del barge-in por voz ni la confirmación de eco.
    ///
    /// Hace lo mismo que el barge-in por voz cuando Jarvis habla
    /// ([`barge_in`]: corte acústico instantáneo + cancelación del LLM +
    /// vuelta a `Listening`), cancela el razonamiento si aún no había audio y
    /// ordena al hilo de voz olvidar la confirmación pendiente y el segmento
    /// marcado como eco. Es idempotente y no toca las tareas del Swarm
    /// (CA-9.4).
    ///
    /// Devuelve `true` si había algo activo (locución o razonamiento) que
    /// interrumpir.
    pub fn interrupt(&mut self) -> bool {
        let state = self.core.state();
        let speaking = state == JarvisState::Speaking;
        let thinking = state == JarvisState::Thinking;
        if !speaking && !thinking {
            // Nada en curso: el clic no tiene efecto.
            return false;
        }
        let current = self.brain.get();
        let client: &dyn TokenStream = match &current {
            Some(client) => client.as_ref(),
            None => &NoBrain,
        };
        if speaking {
            barge_in(&self.core, &self.playback, client);
        }
        // Corte del audio aunque el núcleo ya hubiera salido de `Speaking`
        // (fin natural en carrera): `stop` invalida la generación en curso.
        self.playback.stop();
        if thinking {
            client.cancel();
        }
        // Red de seguridad ante la carrera `Speaking`↔`Thinking`: la esfera
        // vuelve a escuchar en vez de quedarse pintando "pensando".
        for from in [JarvisState::Speaking, JarvisState::Thinking] {
            let _ = self.core.transition_from(from, JarvisState::Listening);
        }
        // El hilo de voz olvida la confirmación a medias y el eco marcado.
        let _ = self.control_tx.try_send(ControlMsg::Interrupt);
        true
    }

    /// Activa o desactiva la escucha continua en caliente (HU-13).
    pub fn set_continuous_listening(&self, enabled: bool) {
        self.activation.set_continuous(enabled);
    }

    /// Cambia la voz activa en caliente sin reiniciar el proceso.
    ///
    /// El hilo del cerebro sustituye el motor TTS y relanza en segundo plano la
    /// pre-síntesis de fillers de la voz nueva.
    pub fn set_voice(&self, voice_id: &str) {
        let _ = self.brain_tx.send(BrainJob::SetVoice(voice_id.to_string()));
    }

    /// Instala el transcriptor (whisper cargado una sola vez).
    pub fn set_transcriber(&self, transcriber: Box<dyn Transcriber + Send>) {
        let _ = self.stt_tx.send(SttJob::SetTranscriber(transcriber));
    }

    /// `true` cuando el hilo STT ya tiene transcriptor.
    pub fn has_transcriber(&self) -> bool {
        self.stt_ready.load(Ordering::Acquire)
    }

    pub fn speaker(&self) -> VoiceSpeaker {
        VoiceSpeaker {
            tx: self.brain_tx.clone(),
            queue: self.queue.clone(),
        }
    }

    pub fn stats(&self) -> VoiceStatsSnapshot {
        self.stats.snapshot()
    }

    /// Contadores vivos, para leerlos desde otro hilo (`jarvis.getState`,
    /// HU-68) sin pasar por el orquestador.
    pub fn stats_handle(&self) -> Arc<VoiceStats> {
        Arc::clone(&self.stats)
    }

    /// Detiene los hilos. Corta el audio y cancela el LLM para que nada quede
    /// esperando; los hilos que siguen ocupados al vencer `deadline`
    /// (inferencia de whisper, una petición de red) se abandonan y se
    /// devuelven sus nombres: el proceso está saliendo y el SO los recoge.
    pub fn shutdown(&mut self, deadline: Duration) -> Vec<&'static str> {
        self.playback.stop();
        if let Some(brain) = self.brain.get() {
            brain.cancel();
        }
        let _ = self.control_tx.send(ControlMsg::Shutdown);
        let _ = self.stt_tx.try_send(SttJob::Shutdown);
        let _ = self.brain_tx.send(BrainJob::Shutdown);
        let _ = self.tts_tx.send(TtsJob::Shutdown);
        if let Some(tasks) = &self.tasks {
            // Mata los árboles de procesos de las tareas en curso.
            tasks.shutdown(deadline / 2);
        }

        let limit = Instant::now() + deadline;
        let mut pending = std::mem::take(&mut self.threads);
        while !pending.is_empty() && Instant::now() < limit {
            let (done, busy): (Vec<_>, Vec<_>) =
                pending.into_iter().partition(|(_, j)| j.is_finished());
            for (_, join) in done {
                let _ = join.join();
            }
            pending = busy;
            if !pending.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        pending.into_iter().map(|(name, _)| name).collect()
    }
}

impl Drop for VoiceOrchestrator {
    fn drop(&mut self) {
        if !self.threads.is_empty() {
            self.shutdown(Duration::from_millis(500));
        }
    }
}

// ---------------------------------------------------------------- jarvis-voice

/// Turno retenido porque terminó en conector.
struct PendingTurn {
    query: String,
    at: Instant,
    /// `None` mientras se espera la transcripción de la continuación.
    deadline: Option<Instant>,
}

struct ControlLoop {
    core: SharedCore,
    playback: AudioPlayback,
    capture: Arc<dyn CaptureControl>,
    activation: Arc<ActivationDetector>,
    client: BrainSlot,
    /// Interrupción por voz (ver [`VoiceConfig::barge_in_enabled`]).
    barge_in: Arc<AtomicBool>,
    stt: Sender<SttJob>,
    brain: Sender<BrainJob>,
    /// Salida TTS directa para el chime diferido (HU-42): no pasa por el
    /// cerebro, que puede estar ocupado con un turno.
    tts: Sender<TtsJob>,
    stats: Arc<VoiceStats>,
    stt_ready: Arc<AtomicBool>,
    /// Segmentos que whisper está transcribiendo ahora mismo (HU-42, H-T6).
    stt_in_flight: Arc<AtomicUsize>,
    /// Micrófono + DSP + STT listos (lo lee el cerebro al cerrar un turno).
    ready: Arc<AtomicBool>,
    config: VoiceConfig,
    capture_enabled: Option<bool>,
    pending: Option<PendingTurn>,
    /// Turno de usuario despachado y aún en curso (HU-AUDIO-CONTINUITY).
    pending_user_turn: PendingUserTurn,
    stt_warned: bool,
    /// Voz detectada mientras Jarvis suena: salida en pausa hasta confirmar.
    candidate: Option<BargeCandidate>,
    /// El segmento abierto resultó ser eco: su fin de turno se descarta.
    echo_segment: bool,
    /// Último instante en que Jarvis sonaba (cola de eco sin barge-in).
    last_sounding: Option<Instant>,
    /// Instante del último despacho: origen de la ventana de continuación
    /// (HU-42, H-T4). Dentro de ella la voz nunca es eco.
    last_turn_end: Option<Instant>,
    /// Instante en que empezó la locución en curso (HU-42, H-T5).
    speech_started: Option<Instant>,
    /// La voz que empieza continúa el turno recién despachado: no es eco ni
    /// una consulta nueva (HU-42, H-T5).
    continuation_expected: bool,
    /// Cuándo debe sonar el chime de confirmación si nadie retoma la voz
    /// (HU-42, H-T4). `None` = no hay chime pendiente.
    chime_deadline: Option<Instant>,
    /// Última voz del usuario: el *gate* de la cola hablada espera un hueco
    /// de silencio tras ella (CA-37.5).
    last_voice_activity: Arc<Mutex<Option<Instant>>>,
    /// Publica si hay un turno a medio capturar (HU-55, ver
    /// [`VoiceOrchestrator::is_active_turn`]).
    turn_open: Arc<AtomicBool>,
}

/// Barge-in por confirmar (ver [`ControlLoop::on_voice`]).
struct BargeCandidate {
    voice_at: Instant,
    paused_at: Instant,
    samples: u32,
    voiced: u32,
}

/// Muestreo del VAD durante la confirmación.
const CONFIRM_TICK: Duration = Duration::from_millis(10);

/// Sin barge-in, voz que empieza hasta este tiempo después de que Jarvis
/// calla aún se considera eco (latencia del altavoz + reverberación).
const ECHO_TAIL: Duration = Duration::from_millis(400);

/// Duración máxima de un eco físico de altavoz: si un segmento marcado como
/// eco dura más, es voz real del usuario que empezó a hablar encima de Jarvis
/// (HU-29). Nunca se descarta ciegamente un segmento más largo que esto.
const ECHO_MAX_MS: u32 = 700;

/// Retardo por defecto del chime de confirmación (HU-42, H-T4): el tiempo que
/// se le da al usuario para retomar su frase antes de que suene nada.
pub const CHIME_DELAY: Duration = Duration::from_millis(450);

/// Ventana de continuación por defecto tras un despacho (HU-42, H-T4).
pub const CONTINUATION_WINDOW: Duration = Duration::from_millis(1_500);

/// Ventana de locución temprana por defecto (HU-42, H-T5).
pub const SPEAKING_MERGE_WINDOW: Duration = Duration::from_millis(800);

impl ControlLoop {
    fn run(
        mut self,
        mut events: Receiver<VoiceEvent>,
        control: &Receiver<ControlMsg>,
        stt_out: &Receiver<SttOutcome>,
    ) {
        self.reconcile();
        loop {
            select! {
                recv(events) -> msg => match msg {
                    Ok(event) => self.on_voice(event),
                    // El motor se apagó: no hay más eventos.
                    Err(_) => events = crossbeam_channel::never(),
                },
                recv(stt_out) -> msg => {
                    if let Ok(outcome) = msg {
                        self.on_transcribed(outcome);
                    }
                }
                recv(control) -> msg => match msg {
                    Ok(ControlMsg::Shutdown) | Err(_) => break,
                    Ok(ControlMsg::Interrupt) => self.on_interrupt(),
                },
                default(self.tick()) => {}
            }
            self.evaluate_candidate();
            self.reconcile();
        }
        // Libera el micrófono al salir.
        self.capture.set_enabled(false);
    }

    fn tick(&self) -> Duration {
        if self.candidate.is_some() {
            return CONFIRM_TICK;
        }
        let tick = self.config.poll;
        // El chime diferido (HU-42) necesita despertar a su hora aunque no
        // llegue ningún evento de voz.
        match self.chime_deadline {
            Some(deadline) => tick.min(deadline.saturating_duration_since(Instant::now())),
            None => tick,
        }
    }

    /// Alinea micrófono, detector y esfera con el estado del núcleo. Así el
    /// silencio pedido por cualquier vía (menú, IPC, voz) tiene el mismo
    /// efecto: estado `Muted`, detector muteado y dispositivo liberado.
    fn reconcile(&mut self) {
        let muted = self.core.is_muted();
        if self.is_sounding() {
            let now = Instant::now();
            self.last_sounding = Some(now);
            // Inicio de la locución: base de la ventana de fusión temprana
            // (HU-42, H-T5).
            if self.speech_started.is_none() {
                self.speech_started = Some(now);
            }
        } else {
            self.speech_started = None;
        }
        if self.capture_enabled != Some(!muted) {
            self.capture_enabled = Some(!muted);
            if muted {
                self.activation.mute();
                self.activation.close_follow_up();
                self.cancel_in_flight();
                self.pending = None;
                self.candidate = None;
                self.echo_segment = false;
                self.continuation_expected = false;
                self.chime_deadline = None;
            } else {
                self.activation.unmute();
            }
            self.capture.set_enabled(!muted);
        }

        if !muted && self.capture.has_output() {
            if let Some(notice) = self.config.startup_notice.take() {
                let _ = self.brain.send(BrainJob::Say(notice));
            }
        }

        let stt_ready = self.stt_ready.load(Ordering::Acquire);
        let ready = !muted && stt_ready && self.capture.is_listening();
        self.ready.store(ready, Ordering::Release);
        // En reposo (`standby`) el micrófono sigue armado, pero la esfera no
        // promete escuchar: solo la palabra de activación abre turno (HU-17).
        if ready && !self.activation.is_standby() {
            let _ = self
                .core
                .transition_from(JarvisState::Idle, JarvisState::Listening);
        } else if !muted {
            // La esfera no promete escuchar si el micrófono o el STT faltan.
            let _ = self
                .core
                .transition_from(JarvisState::Listening, JarvisState::Idle);
        }

        if let Some(PendingTurn {
            deadline: Some(deadline),
            ..
        }) = &self.pending
        {
            if Instant::now() >= *deadline {
                if self.stt_in_flight.load(Ordering::Acquire) > 0 {
                    // Carrera STT/`VoiceStart` (HU-42, H-T6): hay una
                    // transcripción en vuelo, así que la continuación del
                    // usuario puede estar a punto de llegar. Se aplaza el
                    // despacho en vez de cortar su frase en dos.
                    if let Some(pending) = &mut self.pending {
                        pending.deadline = Some(Instant::now() + self.config.connector_grace);
                    }
                } else if let Some(turn) = self.pending.take() {
                    self.dispatch(turn.query, turn.at);
                }
            }
        }
        self.flush_chime();
        // `reconcile` corre tras cada evento y cada tick: es el punto único
        // donde se publica el turno abierto para la esfera (HU-55).
        self.turn_open.store(
            self.pending.is_some() || self.continuation_expected,
            Ordering::Release,
        );
    }

    /// Emite el chime diferido si venció su retardo y Jarvis sigue callado
    /// (HU-42, H-T4). Cualquier voz del usuario lo cancela antes.
    fn flush_chime(&mut self) {
        let Some(deadline) = self.chime_deadline else {
            return;
        };
        if self.core.is_muted() || self.is_sounding() {
            // Silencio pedido o Jarvis ya habla: el chime sobra.
            self.chime_deadline = None;
            return;
        }
        if Instant::now() >= deadline {
            self.chime_deadline = None;
            let _ = self.tts.send(TtsJob::Chime { pcm: synth_chime() });
        }
    }

    fn on_voice(&mut self, event: VoiceEvent) {
        if self.core.is_muted() {
            return;
        }
        match event {
            VoiceEvent::VoiceStart { at } => {
                self.mark_voice(at);
                // Con Jarvis sonando, la voz puede ser su propio eco que el
                // AEC no canceló. Se pausa la salida en el acto (silencio
                // ≤ 50 ms, CA-9.1) y se confirma con Jarvis callado: si la
                // voz sigue, es el usuario; si desaparece, era eco (CA-2.1).
                let sounding = self.is_sounding();
                if sounding {
                    let now = Instant::now();
                    self.last_sounding = Some(now);
                    // La locución pudo empezar después del último tick: sin
                    // esto la ventana de fusión temprana no existiría aún.
                    self.speech_started.get_or_insert(now);
                }
                // El usuario empezó a hablar: mantén viva la ventana de
                // seguimiento para que una frase larga no caduque a mitad de
                // su transcripción (HU-29). Solo se rearma si YA estaba
                // abierta: un ruido ambiente jamás debe abrirla desde cero
                // (eso permitiría comandos sin decir "Jarvis").
                if self.activation.is_follow_up_open() {
                    self.activation.open_follow_up();
                }
                // Ventana de continuación (HU-42, H-T4): la voz que llega
                // justo después de un despacho es del usuario, nunca eco del
                // chime ni de la cola de la respuesta anterior. Además, el
                // chime diferido se cancela: nadie pisa al que retoma.
                let in_window = self.in_continuation_window(at);
                if in_window {
                    self.chime_deadline = None;
                }
                // Si Jarvis ya suena con la respuesta a ESTE turno, la ventana
                // no exime: sin barge-in, esa voz puede ser eco del parlante
                // (HU-29); un segmento largo igual se rescata por ECHO_MAX_MS.
                let continuing = in_window && !self.sounded_since_dispatch();
                if continuing {
                    self.echo_segment = false;
                }
                if !self.barge_in.load(Ordering::Relaxed) {
                    // Sin barge-in, todo lo que empieza con Jarvis sonando
                    // (o en su cola) es eco: la locución sigue intacta.
                    let in_tail = self
                        .last_sounding
                        .is_some_and(|last| at.saturating_duration_since(last) < ECHO_TAIL);
                    if !continuing && (sounding || in_tail) {
                        self.echo_segment = true;
                        return;
                    }
                } else if sounding && self.candidate.is_none() {
                    self.playback.pause();
                    self.candidate = Some(BargeCandidate {
                        voice_at: at,
                        paused_at: Instant::now(),
                        samples: 0,
                        voiced: 0,
                    });
                }
                // La voz en curso continúa el turno recién despachado (Jarvis
                // piensa o acaba de empezar a hablar): lo que transcriba se
                // fusiona en vez de responderse por separado (HU-42, H-T5).
                if self.starts_continuation() {
                    self.continuation_expected = true;
                    self.chime_deadline = None;
                }
                // Carrera STT/`VoiceStart` (HU-42, H-T6): si el usuario retoma
                // mientras whisper transcribe el fragmento anterior, se avisa
                // al DSP (silencio extendido) y no se despacha la mitad.
                let transcribing = self.stt_in_flight.load(Ordering::Acquire) > 0;
                if self.pending.is_some() || transcribing {
                    if let Some(connector) = self.capture.connector() {
                        connector.signal();
                    }
                    self.continuation_expected = true;
                }
                if let Some(pending) = &mut self.pending {
                    // El usuario retoma tras un conector: silencio extendido.
                    pending.deadline = None;
                }
            }
            VoiceEvent::Partial { at, segment, index } => {
                // Turno largo (HU-44): el usuario sigue hablando. Un fragmento
                // de `max_segment_ms` jamás es eco, y lo retenido no vence
                // mientras tanto.
                self.mark_voice(at);
                self.stats.partials.fetch_add(1, Ordering::Relaxed);
                self.echo_segment = false;
                self.chime_deadline = None;
                if self.activation.is_follow_up_open() {
                    self.activation.open_follow_up();
                }
                if let Some(pending) = &mut self.pending {
                    pending.deadline = None;
                }
                if !self.stt_ready.load(Ordering::Acquire) {
                    return;
                }
                let job = SttJob::Segment {
                    pcm: segment.pcm,
                    at,
                    duration_ms: segment.duration_ms,
                    part: TurnPart::Partial(index),
                };
                if self.stt.try_send(job).is_err() {
                    eprintln!("[voz] fragmento descartado: la transcripción va atrasada.");
                }
            }
            VoiceEvent::EndOfTurn {
                at,
                segment,
                partials,
                close_reason,
            } => {
                self.mark_voice(at);
                self.stats.segments.fetch_add(1, Ordering::Relaxed);
                // Razón del cierre (HU-41): se cuenta siempre, incluso si el
                // turno luego se descarta como eco o sin transcriptor.
                self.stats.count_close(close_reason);
                if std::mem::take(&mut self.echo_segment) && segment.duration_ms <= ECHO_MAX_MS {
                    // Eco corto de la voz de Jarvis: no se transcribe. Un eco
                    // físico nunca supera ~700 ms; si dura más es voz real del
                    // usuario y debe procesarse (HU-29).
                    return;
                }
                if !self.stt_ready.load(Ordering::Acquire) {
                    if !self.stt_warned {
                        self.stt_warned = true;
                        eprintln!("[voz] turno descartado: el modelo de voz aún no está cargado.");
                    }
                    return;
                }
                let job = SttJob::Segment {
                    pcm: segment.pcm,
                    at,
                    duration_ms: segment.duration_ms,
                    part: TurnPart::Final(partials),
                };
                if self.stt.try_send(job).is_err() {
                    eprintln!("[voz] turno descartado: la transcripción va atrasada.");
                }
            }
        }
    }

    fn on_transcribed(&mut self, outcome: SttOutcome) {
        if self.core.is_muted() {
            return;
        }
        match outcome.activation {
            Activation::Accepted { query } => {
                // Hubo voz del usuario que continúa el turno recién
                // despachado (HU-42, H-T5).
                let continuation = std::mem::take(&mut self.continuation_expected);
                // Órdenes de anulación prioritaria (silencio, kill switch,
                // cancelar tareas): cortan cualquier razonamiento en curso y se
                // despachan en el acto. Jamás se fusionan con una continuación
                // retenida, o "silencio" acabaría dentro de "busca X y silencio"
                // y no se reconocería (HU-Fix-Silencio).
                if is_priority_override(&query) {
                    self.cancel_in_flight();
                    self.pending = None;
                    self.pending_user_turn.clear();
                    self.dispatch(query, outcome.at);
                    return;
                }
                if let Some(held) = self.pending.take() {
                    let merged = format!("{} {query}", held.query);
                    if outcome.incomplete {
                        // La continuación también termina en conector: se
                        // encadena y se vuelve a esperar la siguiente parte
                        // antes de despachar (HU-29).
                        self.pending = Some(PendingTurn {
                            query: merged,
                            at: held.at,
                            deadline: Some(Instant::now() + self.config.connector_grace),
                        });
                    } else {
                        self.dispatch(merged, held.at);
                    }
                } else {
                    // Fusión en `Thinking` (HU-AUDIO-CONTINUITY) y también en
                    // los primeros milisegundos de `Speaking` (HU-42, H-T5):
                    // una respuesta local rápida no debe partir la frase del
                    // usuario en dos turnos.
                    let may_merge = continuation
                        || self.core.state() == JarvisState::Thinking
                        || self.in_early_speech();
                    let previous = if may_merge {
                        self.pending_user_turn.take()
                    } else {
                        self.pending_user_turn.clear();
                        None
                    };
                    if let Some(previous) = previous {
                        // El usuario pausó, Jarvis empezó a procesar y siguió
                        // hablando: se fusiona con el turno en curso y se vuelve a
                        // despachar completo (HU-AUDIO-CONTINUITY).
                        let merged = format!("{previous} {query}");
                        self.cancel_in_flight();
                        if outcome.incomplete {
                            self.pending = Some(PendingTurn {
                                query: merged,
                                at: outcome.at,
                                deadline: Some(Instant::now() + self.config.connector_grace),
                            });
                        } else {
                            self.dispatch_with_fragment(merged, Some(query), outcome.at);
                        }
                    } else if outcome.incomplete {
                        self.pending = Some(PendingTurn {
                            query,
                            at: outcome.at,
                            deadline: Some(Instant::now() + self.config.connector_grace),
                        });
                    } else {
                        self.dispatch(query, outcome.at);
                    }
                }
            }
            Activation::WakeOnly | Activation::Rejected => {
                if matches!(outcome.activation, Activation::Rejected) {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                }
                // La continuación no fue voz útil: se olvida la fusión y sale
                // lo retenido.
                self.continuation_expected = false;
                if let Some(held) = self.pending.take() {
                    self.dispatch(held.query, held.at);
                }
            }
        }
    }

    /// Interrupción manual desde la superficie (HU-56): la locución ya está
    /// cortada por [`VoiceOrchestrator::interrupt`], así que aquí solo se
    /// olvida lo que quedaba a medias en el hilo de voz: la confirmación del
    /// barge-in pendiente (con su pausa) y el segmento marcado como eco.
    fn on_interrupt(&mut self) {
        self.candidate = None;
        self.echo_segment = false;
        self.continuation_expected = false;
        // La pausa solo servía para confirmar la voz que disparó el barge-in;
        // desde aquí la locución cortada no debe reanudarse.
        self.playback.resume();
    }

    /// Muestrea el VAD con Jarvis en pausa y, al cerrar la ventana, decide.
    fn evaluate_candidate(&mut self) {
        let Some(candidate) = &mut self.candidate else {
            return;
        };
        if let Some(score) = self.capture.vad_score() {
            candidate.samples += 1;
            if score >= self.config.vad_threshold {
                candidate.voiced += 1;
            }
        }
        if candidate.paused_at.elapsed() < self.config.barge_in_window {
            return;
        }
        let Some(candidate) = self.candidate.take() else {
            return;
        };
        let silence_us = u64::try_from(
            candidate
                .paused_at
                .saturating_duration_since(candidate.voice_at)
                .as_micros(),
        )
        .unwrap_or(u64::MAX);
        // HU-56: el usuario puede hablar bajo o empezar tarde dentro de la
        // ventana. Bastan 2 frames con voz (o un tercio de la ventana) para
        // que su voz no se confunda con el eco del altavoz.
        let user_spoke = candidate.samples > 0
            && (candidate.voiced * 3 >= candidate.samples || candidate.voiced >= 2);

        if !user_spoke {
            self.playback.resume();
            self.echo_segment = true;
            self.continuation_expected = false;
            let n = self.stats.echo_rejected.fetch_add(1, Ordering::Relaxed) + 1;
            println!("[voz] voz descartada como eco de Jarvis ({n} en la sesión).");
            return;
        }

        let current = self.client.get();
        let client: &dyn TokenStream = match &current {
            Some(client) => client.as_ref(),
            None => &NoBrain,
        };
        // Se evalúa antes del barge-in, que saca al núcleo de `Speaking`.
        let early = self.in_early_speech();
        let interrupted = match self.core.state() {
            JarvisState::Speaking => barge_in(&self.core, &self.playback, client),
            _ => {
                self.playback.resume();
                false
            }
        };
        if interrupted {
            self.stats.barge_ins.fetch_add(1, Ordering::Relaxed);
            self.stats
                .last_barge_in_us
                .store(silence_us, Ordering::Relaxed);
            if early {
                // La respuesta apenas había empezado: lo que diga ahora es la
                // continuación de su propia frase, así que el turno pendiente
                // se conserva para fusionarse (HU-42, H-T5).
                self.continuation_expected = true;
                println!("[voz] barge-in temprano: el turno se fusionará.");
            } else {
                // La respuesta interrumpida deja de ser el turno pendiente: lo
                // que diga ahora es una pregunta nueva, no una continuación.
                self.pending_user_turn.clear();
                self.continuation_expected = false;
            }
            // Lo que diga ahora se acepta sin repetir "Jarvis".
            self.activation.open_follow_up();
            println!(
                "[voz] barge-in: Jarvis en silencio {silence_us} µs tras detectar voz; confirmado en {} ms.",
                self.config.barge_in_window.as_millis()
            );
        }
    }

    fn mark_voice(&self, at: Instant) {
        *self
            .last_voice_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(at);
    }

    /// `true` si Jarvis está emitiendo audio por los altavoces.
    fn is_sounding(&self) -> bool {
        self.core.state() == JarvisState::Speaking
    }

    /// `true` si `at` cae en la ventana de continuación post-despacho
    /// (HU-42, H-T4): la voz de ahí es del usuario, nunca eco.
    fn in_continuation_window(&self, at: Instant) -> bool {
        self.last_turn_end
            .is_some_and(|end| at.saturating_duration_since(end) < self.config.continuation_window)
    }

    /// `true` si Jarvis sonó después del último despacho, es decir, con la
    /// respuesta al turno en curso (HU-42): esa voz sí puede ser eco.
    fn sounded_since_dispatch(&self) -> bool {
        match (self.last_sounding, self.last_turn_end) {
            (Some(sounding), Some(end)) => sounding > end,
            _ => false,
        }
    }

    /// `true` si la locución en curso apenas empezó (HU-42, H-T5).
    fn in_early_speech(&self) -> bool {
        self.core.state() == JarvisState::Speaking
            && self
                .speech_started
                .is_some_and(|started| started.elapsed() < self.config.speaking_merge_window)
    }

    /// `true` si la voz que empieza continúa el turno recién despachado: Jarvis
    /// aún piensa o acaba de empezar a hablar (HU-42, H-T5).
    fn starts_continuation(&self) -> bool {
        self.pending_user_turn.is_some()
            && (self.core.state() == JarvisState::Thinking || self.in_early_speech())
    }

    /// Entrega un turno al cerebro, cancelando el que esté en curso.
    fn dispatch(&mut self, query: String, at: Instant) {
        self.dispatch_with_fragment(query, None, at);
    }

    /// Como [`dispatch`](Self::dispatch), indicando el fragmento nuevo del
    /// usuario para el HUD cuando este turno continúa uno en curso
    /// (HU-AUDIO-CONTINUITY).
    fn dispatch_with_fragment(&mut self, query: String, hud_fragment: Option<String>, at: Instant) {
        if matches!(
            self.core.state(),
            JarvisState::Thinking | JarvisState::Speaking
        ) {
            self.cancel_in_flight();
        }
        // Origen de la ventana de continuación (HU-42, H-T4): lo que diga el
        // usuario a partir de ahora no es eco.
        self.last_turn_end = Some(Instant::now());
        self.continuation_expected = false;
        // El chime ya no suena al aceptar el turno: se difiere para no pisar
        // una continuación (HU-42, H-T4). Si nadie retoma, sonará al vencer.
        self.chime_deadline = Some(Instant::now() + self.config.chime_delay);
        self.pending_user_turn.set(query.clone());
        let _ = self.brain.send(BrainJob::Turn {
            query,
            at,
            hud_fragment,
        });
    }

    fn cancel_in_flight(&self) {
        self.playback.stop();
        if let Some(client) = self.client.get() {
            client.cancel();
        }
    }
}

/// Cliente nulo para el barge-in sin API key.
struct NoBrain;

impl TokenStream for NoBrain {
    fn stream_tokens(
        &self,
        _user_text: &str,
        _on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError> {
        Err(BrainError::NoApiKey)
    }

    fn cancel(&self) {}
}

// ------------------------------------------------------------------ jarvis-stt

fn stt_loop(
    jobs: &Receiver<SttJob>,
    out: &Sender<SttOutcome>,
    activation: &ActivationDetector,
    ready: &AtomicBool,
    in_flight: &AtomicUsize,
) {
    let mut transcriber: Option<Box<dyn Transcriber + Send>> = None;
    // Texto de un turno largo aún abierto (HU-44).
    let mut long_turn: Option<LongTurnText> = None;
    for job in jobs {
        match job {
            SttJob::SetTranscriber(t) => {
                transcriber = Some(t);
                ready.store(true, Ordering::Release);
            }
            SttJob::Segment {
                pcm,
                at,
                duration_ms,
                part,
            } => {
                // El hilo de voz consulta este contador para no despachar un
                // turno retenido mientras la continuación se transcribe
                // (HU-42, H-T6).
                in_flight.fetch_add(1, Ordering::AcqRel);
                // `at` marca el cierre del segmento: restamos su duración para
                // saber cuándo empezó la voz (HU-29).
                let duration = Duration::from_millis(u64::from(duration_ms));
                let segment_start = at.checked_sub(duration).unwrap_or(at);
                let text = match transcriber.as_deref() {
                    None => None,
                    // En silencio ni siquiera se transcribe (CA-4.3).
                    Some(_) if activation.is_muted() => None,
                    Some(t) => t
                        .transcribe(&pcm)
                        .inspect_err(|e| eprintln!("[stt] falló la transcripción: {e}"))
                        .ok(),
                };
                drop(pcm);
                let outcome = match (part, text) {
                    (TurnPart::Partial(index), Some(text)) => {
                        println!("[stt] fragmento {} de un turno largo: {text}", index + 1);
                        // El primer fragmento abre el acumulador; si se perdió,
                        // se abre con el que llegue (mejor media frase que nada).
                        let acc = match long_turn.as_mut() {
                            Some(acc) if index > 0 => acc,
                            _ => long_turn.insert(LongTurnText {
                                text: String::new(),
                                started_at: segment_start,
                            }),
                        };
                        push_words(&mut acc.text, &text);
                        None
                    }
                    (TurnPart::Final(partials), Some(text)) => {
                        // Un cierre sin parciales nunca hereda texto viejo.
                        let previous = long_turn.take().filter(|_| partials > 0);
                        let (text, started_at) = match previous {
                            Some(mut acc) => {
                                push_words(&mut acc.text, &text);
                                (acc.text, acc.started_at)
                            }
                            None => (text, segment_start),
                        };
                        println!("[stt] escuchado: {text}");
                        let cue = crate::incompleteness::incompleteness(&text);
                        if let Some(cue) = cue {
                            println!(
                                "[stt] frase incompleta ({}): se espera la continuación.",
                                cue.as_str()
                            );
                        }
                        Some(SttOutcome {
                            incomplete: cue.is_some(),
                            activation: activation.process_text_started_at(&text, started_at),
                            at,
                        })
                    }
                    (_, None) => {
                        // Silencio, sin transcriptor o fallo: el turno largo
                        // en curso ya no puede completarse con coherencia.
                        long_turn = None;
                        None
                    }
                };
                let sent = outcome.is_none_or(|outcome| out.send(outcome).is_ok());
                in_flight.fetch_sub(1, Ordering::AcqRel);
                if !sent {
                    break;
                }
            }
            SttJob::Shutdown => break,
        }
    }
}

/// Añade `words` a `text` separándolos con un espacio.
fn push_words(text: &mut String, words: &str) {
    let words = words.trim();
    if words.is_empty() {
        return;
    }
    if !text.is_empty() {
        text.push(' ');
    }
    text.push_str(words);
}

// ------------------------------------------------------------------ jarvis-tts

fn tts_loop(
    jobs: &Receiver<TtsJob>,
    brain: &Sender<BrainJob>,
    sink: &dyn AudioSink,
    hud: &HudTx,
    playback: &AudioPlayback,
) {
    // Generación sin salida de audio: se avisa al cerebro igual.
    let mut no_output: Option<u64> = None;
    // Segundos encolados de la generación en curso (diagnóstico sin texto).
    let mut queued: (u64, f32) = (u64::MAX, 0.0);
    let account = |queued: &mut (u64, f32), generation: u64, pcm: &PcmAudio, outcome| {
        if outcome == PlayOutcome::Played {
            if queued.0 != generation {
                *queued = (generation, 0.0);
            }
            queued.1 += pcm.duration_seconds();
        }
    };
    for job in jobs {
        match job {
            TtsJob::Audio {
                generation,
                pcm,
                caption,
            } => {
                // El subtítulo se publica cuando la frase entra a sonar (no
                // al llegar el token), y nunca para una locución cancelada.
                if let Some(caption) = caption {
                    if !playback.is_stale(generation) {
                        hud.emit(caption);
                    }
                }
                let outcome = play(sink, generation, &pcm, &mut no_output);
                account(&mut queued, generation, &pcm, outcome);
            }
            TtsJob::Chime { pcm } => {
                let _ = play_immediate_audio(sink, &pcm);
            }
            TtsJob::End { generation } => {
                let outcome = if no_output == Some(generation) {
                    PlayOutcome::NoOutput
                } else {
                    sink.wait_drained(generation)
                };
                if outcome == PlayOutcome::Played && queued.0 == generation {
                    println!("[voz] locución reproducida ({:.1} s de audio).", queued.1);
                }
                if brain
                    .send(BrainJob::SpeechDone {
                        generation,
                        outcome,
                    })
                    .is_err()
                {
                    break;
                }
            }
            TtsJob::Shutdown => break,
        }
    }
}

/// Reproduce un chime adaptándolo a la tasa de salida, sin generación de turno.
fn play_immediate_audio(sink: &dyn AudioSink, pcm: &PcmAudio) -> PlayOutcome {
    let Some(rate) = sink.output_rate() else {
        return PlayOutcome::NoOutput;
    };
    match to_output_pcm(pcm, rate) {
        Ok(samples) => sink.play_immediate(&samples),
        Err(e) => {
            eprintln!("[tts] no se pudo adaptar el chime a {rate} Hz: {e}");
            PlayOutcome::Cancelled
        }
    }
}

fn play(
    sink: &dyn AudioSink,
    generation: u64,
    pcm: &PcmAudio,
    no_output: &mut Option<u64>,
) -> PlayOutcome {
    let Some(rate) = sink.output_rate() else {
        *no_output = Some(generation);
        return PlayOutcome::NoOutput;
    };
    match to_output_pcm(pcm, rate) {
        Ok(samples) => {
            let outcome = sink.play(&samples, generation);
            if outcome == PlayOutcome::NoOutput {
                *no_output = Some(generation);
            }
            outcome
        }
        Err(e) => {
            eprintln!("[tts] no se pudo adaptar el audio a {rate} Hz: {e}");
            PlayOutcome::Cancelled
        }
    }
}

// ---------------------------------------------------------------- jarvis-brain

/// Qué hace Jarvis al terminar una locución.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum AfterSpeech {
    Listen,
    Mute,
}

struct BrainLoop {
    coordinator: ConversationCoordinator,
    client: BrainSlot,
    core: SharedCore,
    playback: AudioPlayback,
    activation: Arc<ActivationDetector>,
    tts: Sender<TtsJob>,
    clock: LocalClock,
    launcher: AppLauncher,
    /// Kill switch de la automatización (HU-38).
    automation_kill: Option<AutomationKill>,
    /// Entrada sintética para órdenes de pantalla (HU-38).
    automation_input: Option<AutomationInput>,
    stats: Arc<VoiceStats>,
    ready: Arc<AtomicBool>,
    /// Locución en curso: (generación, qué hacer al terminar).
    speaking: Option<(u64, AfterSpeech)>,
    /// Gestor de tareas pesadas (HU-37); `None` = desactivado.
    tasks: Option<TaskManager>,
    /// Planificador de turnos compuestos (HU-63); `None` = solo heurística.
    planner: Option<Arc<dyn TurnPlanner>>,
    /// Sub-sesión enfocada por voz (HU-64): a quién van las continuaciones.
    focus: Option<SessionId>,
    /// Cuándo se dirigió el usuario por última vez a la sub-sesión enfocada.
    focus_at: Option<Instant>,
    /// Caducidad del foco (HU-64); configurable para poder probarla.
    focus_timeout: Duration,
    /// Restricciones duras del turno en curso (HU-71). Las fija
    /// [`Self::on_turn`] y las lee [`Self::converse`] para el prompt.
    constraints: TurnConstraints,
    /// Sub-sesiones abiertas por voz (id → título hablado y alias). El
    /// registro del gestor guarda un título corto pensado para elegir el
    /// *prompt*; de oído se usan estos. Es el roster que resuelve las
    /// referencias habladas (HU-64).
    threads: HashMap<SessionId, VoiceThread>,
    /// Última respuesta completa del asistente (contexto para aprobar
    /// desarrollos, Issue #7).
    last_assistant_response: Option<String>,
    /// Cola hablada priorizada: avisos (IPC) y resultados de tareas.
    queue: SpeechQueueHandle,
    /// Última voz del usuario (la escribe el hilo de voz).
    last_voice_activity: Arc<Mutex<Option<Instant>>>,
    gaming_mode: Arc<AtomicBool>,
    /// Silencio mínimo tras la voz del usuario antes de entregar.
    quiet_gap: Duration,
    /// Hubo resultados retenidos por silencio/modo juego: al salir se
    /// anuncian antes de entregarlos (CA-37.8).
    held: bool,
    /// Eventos hacia el HUD de burbujas (HU-36).
    hud: HudTx,
    /// Último turno numerado para el HUD.
    hud_turn: TurnId,
    /// Turno del usuario para el HUD; se reutiliza en continuaciones.
    hud_user_turn: TurnId,
    /// Turno cuya respuesta aún no se cerró en el HUD.
    hud_open: Option<TurnId>,
    /// Turno de usuario despachado y en curso (se limpia al terminar).
    pending_user_turn: PendingUserTurn,
    /// Última clave publicada al HUD (revisión del gestor, foco visible):
    /// mientras no cambie no se recalcula la lista de sub-sesiones (HU-67).
    sessions_key: Option<(u64, Option<SessionId>)>,
    /// Última lista de sub-sesiones entregada al HUD (HU-67).
    sessions_shown: Vec<SessionView>,
}

/// Cómo se subtitula una locución en el HUD (HU-36).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Caption {
    /// Respuesta al turno abierto (si no hay turno abierto, se trata como aviso).
    Reply,
    /// Aviso fuera de turno: IPC y resultados de tareas.
    Notice,
}

/// Cada cuánto el cerebro evalúa la cola hablada sin esperar otro evento.
const QUEUE_TICK: Duration = Duration::from_millis(250);

impl BrainLoop {
    fn run(mut self, jobs: &Receiver<BrainJob>) {
        loop {
            select! {
                recv(jobs) -> job => match job {
                    Ok(BrainJob::Turn {
                        query,
                        at,
                        hud_fragment,
                    }) => self.on_turn(&query, at, hud_fragment),
                    Ok(BrainJob::Say(text)) => {
                        if self.speaking.is_some() {
                            // Otra locución suena: espera su turno en la cola.
                            self.queue.push(Delivery::new(text, SpeechPriority::Notice));
                        } else {
                            self.say_as(&text, AfterSpeech::Listen, Caption::Notice);
                        }
                    }
                    Ok(BrainJob::SetVoice(voice_id)) => self.set_voice(&voice_id),
                    Ok(BrainJob::SpeechDone {
                        generation,
                        outcome,
                    }) => self.on_speech_done(generation, outcome),
                    Ok(BrainJob::Shutdown) | Err(_) => break,
                },
                default(QUEUE_TICK) => {}
            }
            self.drain_queue();
            self.publish_sessions(Instant::now());
        }
    }

    /// Publica al HUD las sub-sesiones visibles si cambiaron (HU-67).
    ///
    /// Se evalúa en cada vuelta del bucle (como mucho cada [`QUEUE_TICK`]):
    /// la revisión del gestor y el foco son lecturas baratas, y la lista solo
    /// se reconstruye cuando cambian. El render no consulta nada por frame.
    /// Si el canal del HUD está lleno, la clave no se da por publicada y se
    /// reintenta en la vuelta siguiente.
    fn publish_sessions(&mut self, now: Instant) {
        let Some(tasks) = &self.tasks else {
            return;
        };
        let focus = self.visible_focus(now);
        let key = (tasks.revision(), focus);
        if self.sessions_key == Some(key) {
            return;
        }
        let views = session_views(&tasks.list(), focus, |record| self.spoken_title(record));
        if views == self.sessions_shown || self.hud.try_emit(HudEvent::Sessions(views.clone())) {
            self.sessions_key = Some(key);
            self.sessions_shown = views;
        }
    }

    /// Foco que se muestra: el vigente, salvo que ya haya caducado. No lo
    /// suelta (eso lo hace el siguiente turno, que además lo anuncia).
    fn visible_focus(&self, now: Instant) -> Option<SessionId> {
        let at = self.focus_at?;
        self.focus
            .filter(|_| !focus_expired(at, now, self.focus_timeout))
    }

    /// Dice la siguiente locución de la cola si el *gate* lo permite: Jarvis
    /// en reposo/escucha, sin hablar, tras un hueco de silencio del usuario y
    /// sin silencio ni modo juego (salvo `Critical`).
    fn drain_queue(&mut self) {
        if self.queue.pending() == 0 {
            self.held = false;
            return;
        }
        let gate = DeliveryGate {
            state: self.core.state(),
            speaking: self.speaking.is_some(),
            last_voice_activity: *self
                .last_voice_activity
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            quiet_gap: self.quiet_gap,
            muted: self.core.is_muted(),
            gaming_mode: self.gaming_mode.load(Ordering::Relaxed),
        };
        let now = Instant::now();
        if gate.muted || gate.gaming_mode {
            self.held = true;
        } else if self.held && gate.allows(SpeechPriority::TaskResult, now) {
            self.held = false;
            let n = self.queue.pending();
            let text = if n == 1 {
                "Tienes 1 resultado pendiente.".to_string()
            } else {
                format!("Tienes {n} resultados pendientes.")
            };
            self.say_as(&text, AfterSpeech::Listen, Caption::Notice);
            return;
        }
        let next = self.queue.lock().pop_ready(&gate, now);
        if let Some(delivery) = next {
            self.say_as(&delivery.text, AfterSpeech::Listen, Caption::Notice);
        }
    }

    /// Turno conversacional enrutado (HU-63, HU-64).
    ///
    /// Sustituye la bifurcación binaria `Heavy/Light` de HU-37: el
    /// [`TurnRouter`] decide si el turno se responde en el hilo central, si abre
    /// una sub-sesión, si va a una ya abierta (direccionamiento o foco), o las
    /// dos cosas a la vez.
    fn route_turn(&mut self, query: &str) {
        let notice = self.expire_focus(Instant::now());
        let roster = self.roster();
        let (plan, trace) = TurnRouter::route_traced(
            query,
            &RouteContext {
                planner: self.planner.as_deref(),
                roster: &roster,
                focus: self.focus,
                constraints: self.constraints,
            },
        );
        if let Some(trace) = trace {
            self.stats.count_planner(trace);
        }
        // El plan solo puede sumar restricciones, nunca quitarlas (CA-71.4b).
        self.constraints = self.constraints.union(plan.constraints);
        self.apply_plan(plan, query, notice);
    }

    /// Suelta el foco si pasó `focus_timeout` sin turnos dirigidos a él y
    /// devuelve el aviso hablado (§3.4, etapa 5).
    fn expire_focus(&mut self, now: Instant) -> Option<String> {
        let at = self.focus_at?;
        if !focus_expired(at, now, self.focus_timeout) {
            return None;
        }
        self.clear_focus();
        Some(UNFOCUS_PHRASE.to_string())
    }

    fn clear_focus(&mut self) {
        self.focus = None;
        self.focus_at = None;
    }

    /// Fija (o refresca) el foco en `id`.
    fn set_focus(&mut self, id: SessionId) {
        self.focus = Some(id);
        self.focus_at = Some(Instant::now());
    }

    /// Refresca la caducidad si el turno fue dirigido a la sub-sesión enfocada.
    fn touch_focus(&mut self, id: SessionId) {
        if self.focus == Some(id) {
            self.focus_at = Some(Instant::now());
        }
    }

    /// Sub-sesiones abiertas por voz que siguen en el registro, por id.
    fn roster(&self) -> Vec<RosterEntry> {
        let Some(tasks) = &self.tasks else {
            return Vec::new();
        };
        let mut roster: Vec<RosterEntry> = self
            .threads
            .iter()
            .filter_map(|(id, thread)| {
                let record = tasks.get(TaskId::from(*id))?;
                Some(RosterEntry {
                    id: *id,
                    title: thread.title.clone(),
                    aliases: thread.aliases.clone(),
                    status: status_word(record.status).to_string(),
                    progress: None,
                })
            })
            .collect();
        roster.sort_by_key(|entry| entry.id);
        roster
    }

    /// Título hablado de una sub-sesión (el del gestor si no la abrió la voz).
    fn spoken_title(&self, record: &TaskRecord) -> String {
        self.threads
            .get(&session_id(record.id))
            .map(|thread| thread.title.clone())
            .unwrap_or_else(|| record.title.clone())
    }

    /// Bloque de contexto `[Sub-sesiones activas]` para el turno del hilo
    /// central (HU-65, §3.5): qué hilos existen, cómo van y qué encontraron.
    ///
    /// Es efímero: [`Self::converse`] lo pasa como contexto de la petición, no
    /// como turno del usuario, así que no entra al historial ni al HUD.
    fn roster_context(&self) -> Option<String> {
        let tasks = self.tasks.as_ref()?;
        let records: Vec<TaskRecord> = tasks
            .list()
            .into_iter()
            .map(|record| TaskRecord {
                title: self.spoken_title(&record),
                ..record
            })
            .collect();
        format_roster_context(&records, ROSTER_CONTEXT_MAX_CHARS)
    }

    /// Resuelve la referencia o devuelve la frase con la que preguntar.
    fn resolve(&self, tasks: &TaskManager, reference: &SessionRef) -> Result<TaskRecord, String> {
        let roster = self.roster();
        match resolve_session(tasks, &roster, self.focus, reference) {
            Resolution::Found(record) => Ok(record),
            Resolution::Ambiguous(records) => Err(disambiguation_phrase(
                &records
                    .iter()
                    .map(|record| self.spoken_title(record))
                    .collect::<Vec<_>>(),
            )),
            Resolution::Missing => Err(NO_SUCH_SESSION_PHRASE.to_string()),
        }
    }

    /// Ejecuta el plan en orden (§3.4): `control` → `spawn` → `route`/`query` →
    /// `foreground`, fundiendo los acuses con la respuesta del hilo central en
    /// una sola locución. `notice` (foco caducado) abre la locución.
    fn apply_plan(&mut self, plan: TurnPlan, query: &str, notice: Option<String>) {
        if plan.is_empty() {
            self.count_foreground();
            match notice {
                Some(notice) => self.converse_or_say(&fuse_ack_and_foreground(&notice, query)),
                None => self.converse_or_say(query),
            }
            return;
        }
        // Sin gestor de tareas no hay sub-sesiones donde despachar: el hilo
        // central atiende el turno completo, como antes de HU-63.
        if !plan.spawn.is_empty() && self.tasks.is_none() {
            self.count_foreground();
            self.converse_or_say(query);
            return;
        }

        let mut phrases: Vec<String> = notice.into_iter().collect();
        for control in &plan.control {
            if let Some(phrase) = self.apply_session_control(control) {
                phrases.push(phrase);
            }
        }
        for spec in &plan.spawn {
            if let Some(phrase) = self.spawn_session(spec) {
                phrases.push(phrase);
            }
        }
        for (id, instruction) in &plan.route {
            if let Some(phrase) = self.apply_session_route(*id, instruction) {
                phrases.push(phrase);
            }
        }
        for session_query in &plan.query {
            if let Some(phrase) = self.apply_session_query(session_query) {
                phrases.push(phrase);
            }
        }

        let ack = phrases.join(" ");
        match (plan.foreground, ack.is_empty()) {
            // Acuse + respuesta en una sola locución (CA-1).
            (Some(foreground), false) => {
                self.count_foreground();
                let fused = fuse_ack_and_foreground(&ack, &foreground);
                self.converse_or_say(&fused);
            }
            (Some(foreground), true) => {
                self.count_foreground();
                self.converse_or_say(&foreground);
            }
            (None, false) => self.say(&ack, AfterSpeech::Listen),
            (None, true) => {
                self.count_foreground();
                self.converse_or_say(query);
            }
        }
    }

    /// El hilo central responde este turno (HU-68).
    fn count_foreground(&self) {
        self.stats.routed_foreground.fetch_add(1, Ordering::Relaxed);
    }

    /// Abre la sub-sesión de un `SpawnSpec` y devuelve su acuse hablado.
    fn spawn_session(&mut self, spec: &SpawnSpec) -> Option<String> {
        let tasks = self.tasks.clone()?;
        self.stats.local_commands.fetch_add(1, Ordering::Relaxed);
        let title = spec.task_title();
        let prompt = self.constrained(spec.task_prompt(self.last_assistant_response.as_deref()));
        let id = tasks.submit(title, prompt, TaskOrigin::Voice);
        self.stats.routed_spawn.fetch_add(1, Ordering::Relaxed);
        let aliases = if spec.aliases.is_empty() {
            voice_aliases(&spec.title, &spec.instruction, 3)
        } else {
            spec.aliases.clone()
        };
        self.threads.insert(
            session_id(id),
            VoiceThread {
                title: spec.title.clone(),
                aliases,
            },
        );
        println!(
            "[tareas] #{id} sub-sesión «{}» abierta en segundo plano (perfil {:?}).",
            spec.title, spec.profile
        );
        Some(spec.ack_phrase())
    }

    /// Pasa una instrucción nueva a una sub-sesión ya abierta.
    ///
    /// La sub-sesión es todavía una tarea del gestor (un disparo): la
    /// indicación se encola como un turno nuevo con el mismo título, y el hilo
    /// hablado sigue siendo el original. Pasará a `SessionManager::send` cuando
    /// el gestor de HU-62 esté cableado en el orquestador.
    /// Antepone al prompt de una sub-sesión la instrucción dura del turno
    /// (HU-71): un `no_web` dicho al abrirla también la obliga a ella.
    fn constrained(&self, prompt: String) -> String {
        match self.constraints.directive() {
            Some(directive) => format!("{directive}\n\n{prompt}"),
            None => prompt,
        }
    }

    fn apply_session_route(&mut self, id: SessionId, instruction: &str) -> Option<String> {
        let tasks = self.tasks.clone()?;
        let Some(record) = tasks.get(TaskId::from(id)) else {
            return Some(NO_SUCH_SESSION_PHRASE.to_string());
        };
        self.stats.local_commands.fetch_add(1, Ordering::Relaxed);
        self.touch_focus(id);
        tasks.submit(
            record.title.clone(),
            self.constrained(task_prompt(instruction)),
            TaskOrigin::Voice,
        );
        Some(format!(
            "Le pasé la indicación a {}.",
            self.spoken_title(&record)
        ))
    }

    /// Enfoca, desenfoca, cierra o cancela una sub-sesión.
    fn apply_session_control(&mut self, control: &SessionControl) -> Option<String> {
        if *control == SessionControl::Unfocus {
            self.clear_focus();
            return Some(UNFOCUS_PHRASE.to_string());
        }
        let tasks = self.tasks.clone()?;
        let reference = match control {
            SessionControl::Focus(r) | SessionControl::Cancel(r) | SessionControl::Close(r) => r,
            SessionControl::Unfocus => unreachable!("atendido arriba"),
        };
        let record = match self.resolve(&tasks, reference) {
            Ok(record) => record,
            Err(phrase) => return Some(phrase),
        };
        self.stats.local_commands.fetch_add(1, Ordering::Relaxed);
        let id = session_id(record.id);
        let title = self.spoken_title(&record);
        match control {
            SessionControl::Focus(_) => {
                self.set_focus(id);
                Some(format!("Estoy en {title}."))
            }
            SessionControl::Cancel(_) | SessionControl::Close(_) => {
                tasks.cancel(record.id);
                self.threads.remove(&id);
                if self.focus == Some(id) {
                    self.clear_focus();
                }
                let verb = if matches!(control, SessionControl::Cancel(_)) {
                    "Cancelé"
                } else {
                    "Cerré"
                };
                Some(format!("{verb} {title}."))
            }
            SessionControl::Unfocus => unreachable!("atendido arriba"),
        }
    }

    /// Responde "¿cómo va X?", "léeme el detalle" o el listado de hilos.
    fn apply_session_query(&mut self, query: &SessionQuery) -> Option<String> {
        let tasks = self.tasks.clone()?;
        self.stats.local_commands.fetch_add(1, Ordering::Relaxed);
        let reference = match query {
            SessionQuery::Status(r) | SessionQuery::ReadDetail(r) => r,
            SessionQuery::ListAll => {
                let titles: Vec<String> = self
                    .roster()
                    .into_iter()
                    .filter(|entry| {
                        entry.status == status_word(TaskStatus::Running)
                            || entry.status == status_word(TaskStatus::Queued)
                    })
                    .map(|entry| entry.title)
                    .collect();
                return Some(match titles.as_slice() {
                    [] => "No tienes hilos en curso.".to_string(),
                    [one] => format!("Tienes un hilo en curso: {one}."),
                    many => format!("Tienes {} hilos en curso: {}.", many.len(), many.join(", ")),
                });
            }
        };
        let record = match self.resolve(&tasks, reference) {
            Ok(record) => record,
            Err(phrase) => return Some(phrase),
        };
        self.touch_focus(session_id(record.id));
        let title = self.spoken_title(&record);
        Some(match query {
            SessionQuery::ReadDetail(_) => match &record.detail_path {
                Some(path) => format!("El detalle de {title} quedó en {}.", path.display()),
                None => format!("{title} todavía no tiene detalle."),
            },
            _ => describe_task(&TaskRecord { title, ..record }),
        })
    }

    /// Turno del hilo central con el cerebro conversacional (o el aviso de que
    /// no hay cerebro configurado).
    fn converse_or_say(&mut self, text: &str) {
        match self.client.get() {
            Some(client) => self.converse(client.as_ref(), text),
            None => self.say(NO_API_KEY_PHRASE, AfterSpeech::Listen),
        }
    }

    /// "Cancela la tarea de los correos" / "cancela todas las tareas".
    fn cancel_tasks(&mut self, target: &CancelTarget) {
        self.stats.local_commands.fetch_add(1, Ordering::Relaxed);
        let Some(tasks) = self.tasks.clone() else {
            self.say(NO_TASKS_PHRASE, AfterSpeech::Listen);
            return;
        };
        let phrase = match target {
            CancelTarget::All => match tasks.cancel_all() {
                0 => "No hay tareas en curso.".to_string(),
                1 => "Cancelé la tarea en curso.".to_string(),
                n => format!("Cancelé {n} tareas."),
            },
            CancelTarget::Latest => {
                let latest = tasks
                    .active()
                    .into_iter()
                    .max_by_key(|r| (r.created_at_ms, r.id));
                match latest {
                    None => "No hay tareas en curso.".to_string(),
                    Some(record) => {
                        tasks.cancel(record.id);
                        format!("Detuve la tarea sobre {}.", record.title)
                    }
                }
            }
            CancelTarget::Named(target) => {
                let active: Vec<_> = tasks.active().into_iter().map(|r| r.id).collect();
                let matches: Vec<_> = tasks
                    .find_by_title(target)
                    .into_iter()
                    .filter(|id| active.contains(id))
                    .collect();
                match matches.as_slice() {
                    [] => "No encontré esa tarea en curso.".to_string(),
                    [id] => {
                        tasks.cancel(*id);
                        let title = tasks.get(*id).map(|r| r.title).unwrap_or_default();
                        format!("Cancelé la tarea sobre {title}.")
                    }
                    many => format!(
                        "Hay {} tareas que coinciden. ¿Cuál cancelo, la primera o la segunda?",
                        many.len()
                    ),
                }
            }
        };
        self.say(&phrase, AfterSpeech::Listen);
    }

    /// "¿Qué tareas tengo?": tareas vivas y resultados por decir.
    fn list_tasks(&mut self) {
        self.stats.local_commands.fetch_add(1, Ordering::Relaxed);
        let Some(tasks) = self.tasks.clone() else {
            self.say(NO_TASKS_PHRASE, AfterSpeech::Listen);
            return;
        };
        let active = tasks.active();
        let mut phrase = match active.as_slice() {
            [] => "No tienes tareas en curso.".to_string(),
            [one] => format!("Tienes una tarea en curso: {}.", one.title),
            many => {
                let titles: Vec<&str> = many.iter().map(|r| r.title.as_str()).collect();
                format!(
                    "Tienes {} tareas en curso: {}.",
                    many.len(),
                    titles.join(", ")
                )
            }
        };
        match self.queue.pending() {
            0 => {}
            1 => phrase.push_str(" Y 1 resultado pendiente."),
            n => phrase.push_str(&format!(" Y {n} resultados pendientes.")),
        }
        self.say(&phrase, AfterSpeech::Listen);
    }

    fn on_turn(&mut self, query: &str, _at: Instant, hud_fragment: Option<String>) {
        // Lo que quedó de un turno cancelado (barge-in) no debe bloquear este.
        self.speaking = None;
        if let Some(fragment) = hud_fragment {
            // Continuación del turno en curso (HU-AUDIO-CONTINUITY): la burbuja
            // del usuario crece con el fragmento y la nueva respuesta abre una
            // burbuja de Jarvis nueva (la parcial quedó cancelada).
            self.close_hud(true);
            self.hud.emit(HudEvent::UserSaid {
                turn: self.hud_user_turn,
                text: fragment,
            });
            self.hud_turn += 1;
            self.hud_open = Some(self.hud_turn);
        } else {
            // HUD (HU-36): si la respuesta anterior seguía abierta, se cortó.
            self.close_hud(true);
            self.hud_turn += 1;
            self.hud_user_turn = self.hud_turn;
            self.hud.emit(HudEvent::UserSaid {
                turn: self.hud_turn,
                text: query.to_string(),
            });
            self.hud_open = Some(self.hud_turn);
        }
        let _ = self
            .core
            .transition_from(JarvisState::Thinking, JarvisState::Listening);
        let _ = self
            .core
            .transition_from(JarvisState::Speaking, JarvisState::Listening);
        // El chime de confirmación (HU-18) ya no se reproduce aquí: el hilo de
        // voz lo difiere y lo cancela si el usuario retoma su frase (HU-42).

        // HU-71: las restricciones se extraen antes que cualquier ruta, y
        // `local_command` ya no devuelve nada que abra la web si `no_web`.
        self.constraints = turn_constraints(query);
        if self.constraints.no_web {
            self.stats.no_web_turns.fetch_add(1, Ordering::Relaxed);
            println!(
                "[voz] restricciones del turno: {}",
                self.constraints.label()
            );
        }
        match local_command(query) {
            Some(LocalCommand::Time) => {
                self.stats.local_commands.fetch_add(1, Ordering::Relaxed);
                let (h, m) = (self.clock)();
                self.say(&time_phrase(h, m), AfterSpeech::Listen);
            }
            Some(LocalCommand::Standby) => self.standby(),
            Some(LocalCommand::OpenApp {
                command,
                display_name,
            }) => {
                self.stats.local_commands.fetch_add(1, Ordering::Relaxed);
                match (self.launcher)(&command) {
                    Ok(()) => self.say(&format!("Abriendo {display_name}."), AfterSpeech::Listen),
                    Err(e) => {
                        eprintln!("[voz] no se pudo abrir {display_name} ({command:?}): {e}");
                        self.say(
                            &format!("No pude abrir {display_name}."),
                            AfterSpeech::Listen,
                        );
                    }
                }
            }
            Some(LocalCommand::CloseApp {
                pattern,
                display_name,
            }) => {
                self.stats.local_commands.fetch_add(1, Ordering::Relaxed);
                let argv = close_app_argv(&pattern);
                match (self.launcher)(&argv) {
                    Ok(()) => self.say(&format!("Cerrando {display_name}."), AfterSpeech::Listen),
                    Err(e) => {
                        eprintln!("[voz] no se pudo cerrar {display_name} ({argv:?}): {e}");
                        self.say(
                            &format!("No pude cerrar {display_name}."),
                            AfterSpeech::Listen,
                        );
                    }
                }
            }
            Some(LocalCommand::CancelTask { target }) => self.cancel_tasks(&target),
            Some(LocalCommand::ListTasks) => self.list_tasks(),
            Some(LocalCommand::StopAutomation) => self.stop_automation(),
            Some(LocalCommand::OpenUrl { url, spoken }) => {
                self.stats.local_commands.fetch_add(1, Ordering::Relaxed);
                let argv = vec!["xdg-open".to_string(), url];
                match (self.launcher)(&argv) {
                    Ok(()) => self.say(&spoken, AfterSpeech::Listen),
                    Err(e) => {
                        eprintln!("[voz] no se pudo abrir {argv:?}: {e}");
                        self.say("No pude abrir el navegador.", AfterSpeech::Listen);
                    }
                }
            }
            Some(LocalCommand::Screen(action)) => self.screen_action(&action),
            // HU-63: el turno conversacional (o compuesto) lo decide el router.
            None => self.route_turn(query),
        }
    }

    /// Acción de pantalla (HU-38) por el guard de la automatización. Sin
    /// automatización lo dice en vez de delegar al cerebro CLI.
    fn screen_action(&mut self, action: &ScreenAction) {
        self.stats.local_commands.fetch_add(1, Ordering::Relaxed);
        let Some(input) = self.automation_input.clone() else {
            self.say(
                "La automatización está desactivada. Actívala en la configuración de Jarvis.",
                AfterSpeech::Listen,
            );
            return;
        };
        match input(action.input_action()) {
            Ok(()) => self.say(action.done_phrase(), AfterSpeech::Listen),
            Err(e) => {
                eprintln!("[voz] acción de pantalla {action:?} falló: {e}");
                self.say(action.failure_phrase(), AfterSpeech::Listen);
            }
        }
    }

    /// Kill switch por voz (HU-38): revoca la automatización en curso antes de
    /// cualquier otra cosa y confirma de palabra.
    fn stop_automation(&mut self) {
        self.stats.local_commands.fetch_add(1, Ordering::Relaxed);
        let aborted = self.automation_kill.as_ref().is_some_and(|kill| kill());
        if aborted {
            println!("[voz] kill switch: automatización detenida por voz");
            self.say(
                "Detenido. Dejé de controlar el equipo.",
                AfterSpeech::Listen,
            );
        } else {
            self.say("Detenido.", AfterSpeech::Listen);
        }
    }

    /// Cierre de sesión por voz ("silencio", "descansa"...): cierra la ventana
    /// de seguimiento y vuelve a reposo SIN mutear el micrófono. El wake-word
    /// "Jarvis" reactiva la escucha (HU-17).
    ///
    /// El reposo apaga también la escucha continua: si siguiera activa,
    /// cualquier ruido ambiente abriría turno y la orden de silencio no
    /// serviría de nada. La esfera queda en `Idle`, no en `Listening`, porque
    /// no hay ningún turno abierto.
    fn standby(&mut self) {
        self.stats.local_commands.fetch_add(1, Ordering::Relaxed);
        // Primero el reposo: el hilo de voz consulta la bandera en cada tick y
        // no debe devolver el estado a `Listening` mientras se cierra el turno.
        self.activation.enter_standby();
        // Corta cualquier locución o razonamiento en curso.
        self.playback.stop();
        if let Some(client) = self.client.get() {
            client.cancel();
        }
        self.speaking = None;
        self.pending_user_turn.clear();
        self.close_hud(true);
        let _ = self.coordinator.finish_speech(false);
        for from in [
            JarvisState::Speaking,
            JarvisState::Thinking,
            JarvisState::Listening,
        ] {
            let _ = self.core.transition_from(from, JarvisState::Idle);
        }
    }

    /// Cierra en el HUD la respuesta del turno abierto: `JarvisDone` (empieza
    /// su TTL) o `JarvisInterrupted` (se cierra con "…").
    fn close_hud(&mut self, interrupted: bool) {
        if let Some(turn) = self.hud_open.take() {
            self.hud.emit(if interrupted {
                HudEvent::JarvisInterrupted { turn }
            } else {
                HudEvent::JarvisDone { turn }
            });
        }
    }

    /// Subtítulo del HUD para `text` según su tipo y el turno abierto.
    fn caption(&self, text: &str, caption: Caption) -> HudEvent {
        match (caption, self.hud_open) {
            (Caption::Reply, Some(turn)) => HudEvent::JarvisPhrase {
                turn,
                text: text.to_string(),
            },
            _ => HudEvent::Notice {
                text: text.to_string(),
            },
        }
    }

    /// Cambia la voz del coordinador y recalienta su caché de fillers.
    fn set_voice(&mut self, voice_id: &str) {
        self.coordinator.set_voice(voice_id);
        println!("[voz] voz activa: {voice_id}; preparando sus fillers…");
        let engine = self.coordinator.tts().for_voice(voice_id);
        let voice_id = voice_id.to_string();
        let spawned = std::thread::Builder::new()
            .name("jarvis-voice-fillers".to_string())
            .spawn(move || match engine.pre_synthesize_fillers() {
                Ok(0) => {}
                Ok(n) => println!("[voz] {n} fillers de {voice_id} pre-sintetizados."),
                Err(e) => eprintln!("[voz] no se pudieron pre-sintetizar los fillers: {e}"),
            });
        if let Err(e) = spawned {
            eprintln!("[voz] no se pudo lanzar la pre-síntesis de fillers: {e}");
        }
    }

    /// Turno completo con Claude: chime de confirmación + respuesta en streaming.
    fn converse(&mut self, client: &(dyn TokenStream + Send + Sync), query: &str) {
        // Silenciado o en un estado que no admite turnos: se descarta.
        if let Err(e) = self.coordinator.handle_user_utterance(query) {
            eprintln!("[voz] turno descartado: {e}");
            self.pending_user_turn.clear();
            self.close_hud(false);
            return;
        }
        self.stats.turns.fetch_add(1, Ordering::Relaxed);
        let generation = self.coordinator.utterance_generation();
        // HU-65: el roster viaja como contexto efímero de la petición. El
        // historial de la conversación guarda solo el turno del usuario.
        let context = self.roster_context();
        // HU-71 (CA-71.4c): la restricción va como instrucción dura en el
        // contexto efímero del turno, no en el historial.
        let context = match (context, self.constraints.directive()) {
            (Some(roster), Some(directive)) => Some(format!("{directive}\n\n{roster}")),
            (None, Some(directive)) => Some(directive.to_string()),
            (context, None) => context,
        };

        let tts = self.tts.clone();
        let hud_turn = self.hud_open;
        let result = self.coordinator.run_turn_streaming_with_context(
            client,
            context.as_deref(),
            query,
            &mut |phrase: &str, pcm: &PcmAudio| {
                let _ = tts.send(TtsJob::Audio {
                    generation,
                    pcm: pcm.clone(),
                    caption: hud_turn.map(|turn| HudEvent::JarvisPhrase {
                        turn,
                        text: phrase.to_string(),
                    }),
                });
            },
        );

        if self.playback.is_stale(generation) {
            // Barge-in o silencio: el hilo de voz ya movió el estado.
            self.close_hud(true);
            return;
        }
        match result {
            Ok(completion) => {
                let spoken = strip_directives(&completion.text);
                if !spoken.trim().is_empty() {
                    self.last_assistant_response = Some(spoken);
                }
                if self.core.state() == JarvisState::Speaking {
                    self.finish_after(generation, AfterSpeech::Listen);
                } else {
                    // Respuesta vacía: no hubo frase que decir.
                    let _ = self
                        .core
                        .transition_from(JarvisState::Thinking, JarvisState::Listening);
                    self.coordinator.finish_speech(false).ok();
                    self.pending_user_turn.clear();
                    self.close_hud(false);
                }
            }
            Err(ConversationError::Brain(BrainError::Cancelled)) => {
                // Cancelación limpia (barge-in o hablar antes del primer
                // token): de vuelta a escuchar sin anunciar un fallo de red
                // (HU-32).
                let _ = self
                    .core
                    .transition_from(JarvisState::Speaking, JarvisState::Listening);
                let _ = self
                    .core
                    .transition_from(JarvisState::Thinking, JarvisState::Listening);
                self.coordinator.finish_speech(false).ok();
                self.close_hud(true);
            }
            Err(ConversationError::Brain(e)) => {
                self.stats.speech_errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("[cerebro] la respuesta falló: {e}");
                self.pending_user_turn.clear();
                self.speak_now(NETWORK_ERROR_PHRASE, AfterSpeech::Listen);
            }
            Err(e) => {
                // Transición rechazada: otro hilo cambió el estado (silencio).
                eprintln!("[voz] turno interrumpido: {e}");
                self.pending_user_turn.clear();
                self.close_hud(true);
            }
        }
    }

    /// Locución fuera de un turno con el LLM (hora, comandos locales): se
    /// subtitula como respuesta al turno abierto.
    fn say(&mut self, text: &str, after: AfterSpeech) {
        self.say_as(text, after, Caption::Reply);
    }

    /// Como [`say`](Self::say), indicando cómo se subtitula en el HUD.
    fn say_as(&mut self, text: &str, after: AfterSpeech, caption: Caption) {
        {
            let mut core = self.core.lock();
            if core.state() == JarvisState::Idle {
                let _ = core.transition(JarvisState::Listening);
            }
            if core.state() == JarvisState::Listening {
                let _ = core.transition(JarvisState::Thinking);
            }
            if core.state() != JarvisState::Thinking {
                // Silenciado: no se habla.
                return;
            }
        }
        self.speak_now_as(text, after, caption);
    }

    /// Habla desde `Thinking` o continúa en `Speaking`.
    ///
    /// Sintetiza **antes** de pasar a `Speaking`: la esfera y el barge-in solo
    /// ven `Speaking` cuando hay audio listo para sonar, no durante el segundo
    /// que tarda la síntesis por red.
    fn speak_now(&mut self, text: &str, after: AfterSpeech) {
        self.speak_now_as(text, after, Caption::Reply);
    }

    fn speak_now_as(&mut self, text: &str, after: AfterSpeech, caption: Caption) {
        let pcm = match self.coordinator.synthesize_phrase(text) {
            Ok(pcm) => Some(pcm),
            Err(e) => {
                self.stats.speech_errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("[tts] no se pudo sintetizar la respuesta: {e}");
                None
            }
        };
        let generation = match self.core.state() {
            JarvisState::Thinking if pcm.is_some() => {
                if self.coordinator.begin_speaking().is_err() {
                    return;
                }
                self.coordinator.utterance_generation()
            }
            JarvisState::Thinking => {
                // Nada que decir: de vuelta a escuchar.
                let _ = self
                    .core
                    .transition_from(JarvisState::Thinking, JarvisState::Listening);
                return;
            }
            // Ya estaba hablando (fallo de red a mitad de respuesta): se
            // encola detrás y se cierra la locución igual.
            JarvisState::Speaking => self.coordinator.utterance_generation(),
            // Silencio o barge-in mientras se sintetizaba.
            _ => return,
        };
        if let Some(pcm) = pcm {
            let caption = Some(self.caption(text, caption));
            let _ = self.tts.send(TtsJob::Audio {
                generation,
                pcm,
                caption,
            });
        }
        self.finish_after(generation, after);
    }

    fn finish_after(&mut self, generation: u64, after: AfterSpeech) {
        self.speaking = Some((generation, after));
        let _ = self.tts.send(TtsJob::End { generation });
    }

    fn on_speech_done(&mut self, generation: u64, outcome: PlayOutcome) {
        let Some((current, after)) = self.speaking else {
            return;
        };
        if current != generation {
            return;
        }
        self.speaking = None;
        self.pending_user_turn.clear();
        let cut = outcome == PlayOutcome::Cancelled || self.playback.is_stale(generation);
        self.close_hud(cut);
        if outcome == PlayOutcome::NoOutput {
            eprintln!("[voz] no hay altavoz: la respuesta no se pudo reproducir.");
        }
        if outcome != PlayOutcome::Cancelled && !self.playback.is_stale(generation) {
            let _ = self.coordinator.finish_speech(true);
            match after {
                AfterSpeech::Mute => {
                    let _ = self.core.transition(JarvisState::Muted);
                }
                AfterSpeech::Listen => {
                    if self.ready.load(Ordering::Acquire) {
                        let _ = self
                            .core
                            .transition_from(JarvisState::Idle, JarvisState::Listening);
                    }
                    self.activation.open_follow_up();
                }
            }
        }
        // La cola hablada se drena en `run` tras cada evento (y cada tick).
    }
}

#[cfg(test)]
mod tests;
