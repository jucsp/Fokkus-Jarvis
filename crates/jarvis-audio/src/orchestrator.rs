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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{select, Receiver, Sender};
use jarvis_core::{JarvisState, SharedCore};

use crate::activation::{Activation, ActivationDetector};
use crate::brain::{BrainError, Completion};
use crate::conversation::{barge_in, ConversationCoordinator, ConversationError, TokenStream};
use crate::dsp::ConnectorSignal;
use crate::events::VoiceEvent;
use crate::io::{to_output_pcm, AudioEngine, EngineShared, PlayOutcome, Player};
use crate::playback::AudioPlayback;
use crate::stt::Transcriber;
use crate::tts::{synth_chime, PcmAudio};

/// Lo que dice Jarvis cuando no hay cerebro (ni CLI ni API key) y la
/// consulta lo necesita.
pub const NO_API_KEY_PHRASE: &str = "No tengo un cerebro configurado, así que solo puedo darte la hora o silenciarme. Revisa la configuración en el menú de la esfera.";

/// Aviso hablado al arrancar en modo CLI sin encontrar el comando.
pub const NO_CLI_NOTICE: &str =
    "Hola. No encuentro el comando de mi cerebro; configúralo desde el menú de la esfera.";

/// Aviso hablado al arrancar sin API key (una sola vez).
pub const NO_API_KEY_NOTICE: &str =
    "Hola. Me falta la clave de Anthropic; guárdala con fokkus-jarvis guion guion set api key.";

/// Respuesta cuando se cae la red a mitad de una respuesta (CA-6.4).
pub const NETWORK_ERROR_PHRASE: &str = "Perdí la conexión. Inténtalo de nuevo en un momento.";

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
            vad_threshold: crate::vad::EndpointConfig::default().threshold,
            barge_in_enabled: false,
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
}

/// Contadores del orquestador (diagnóstico; nunca contienen texto).
#[derive(Debug, Default)]
pub struct VoiceStats {
    pub segments: AtomicU64,
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
}

/// Fotografía de [`VoiceStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VoiceStatsSnapshot {
    pub segments: u64,
    pub rejected: u64,
    pub turns: u64,
    pub local_commands: u64,
    pub barge_ins: u64,
    pub echo_rejected: u64,
    pub last_barge_in_us: u64,
    pub last_filler_ms: u64,
    pub speech_errors: u64,
}

impl VoiceStats {
    pub fn snapshot(&self) -> VoiceStatsSnapshot {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        VoiceStatsSnapshot {
            segments: get(&self.segments),
            rejected: get(&self.rejected),
            turns: get(&self.turns),
            local_commands: get(&self.local_commands),
            barge_ins: get(&self.barge_ins),
            echo_rejected: get(&self.echo_rejected),
            last_barge_in_us: get(&self.last_barge_in_us),
            last_filler_ms: get(&self.last_filler_ms),
            speech_errors: get(&self.speech_errors),
        }
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
            "el navegador",
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
            "el navegador",
        ),
        App::Zen if std::path::Path::new("/opt/zen/zen").is_file() => {
            (vec!["/opt/zen/zen".into()], "el navegador")
        }
        App::Zen => (
            vec!["xdg-open".into(), "https://google.com".into()],
            "el navegador",
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

/// Reconoce una orden local en la consulta (ya sin el vocativo).
pub fn local_command(query: &str) -> Option<LocalCommand> {
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
    // La palabra aislada "silencio" solo cierra la sesión cuando abre la
    // consulta ("silencio", "silencio por favor"); nunca en preguntas como
    // "¿qué es el silencio?" o "el silencio" (HU-23).
    let standalone_standby = text.split_whitespace().next() == Some("silencio");
    if has(STANDBY_PHRASES) || standalone_standby {
        Some(LocalCommand::Standby)
    } else if has(TIME_PHRASES) {
        Some(LocalCommand::Time)
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

/// Frase con la hora: "Es la 1:05." / "Son las 15:30."
pub fn time_phrase(hour: u32, minute: u32) -> String {
    let (verb, article) = if hour == 1 {
        ("Es", "la")
    } else {
        ("Son", "las")
    };
    format!("{verb} {article} {hour}:{minute:02}.")
}

const CONNECTORS: &[&str] = &[
    "y", "e", "o", "u", "pero", "que", "porque", "entonces", "aunque", "ni", "sino", "pues",
    "como", "cuando", "donde", "si",
];

/// `true` si el texto termina en una palabra de enlace (el usuario no acabó).
pub fn ends_with_connector(text: &str) -> bool {
    text.split_whitespace()
        .last()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .is_some_and(|w| CONNECTORS.contains(&w.as_str()))
}

/// Envía locuciones al orquestador (IPC `jarvis.speak`, avisos).
#[derive(Clone)]
pub struct VoiceSpeaker(Sender<BrainJob>);

impl VoiceSpeaker {
    pub fn say(&self, text: impl Into<String>) {
        let _ = self.0.send(BrainJob::Say(text.into()));
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
    stats: Arc<VoiceStats>,
    playback: AudioPlayback,
    brain: BrainSlot,
    barge_in: Arc<AtomicBool>,
    stt_ready: Arc<AtomicBool>,
    activation: Arc<ActivationDetector>,
}

enum ControlMsg {
    Shutdown,
}

enum SttJob {
    Segment {
        pcm: Vec<f32>,
        at: Instant,
        /// Duración del segmento en ms, para calcular cuándo empezó la voz
        /// (HU-29) y comprobar la ventana de seguimiento contra ese instante.
        duration_ms: u32,
    },
    SetTranscriber(Box<dyn Transcriber + Send>),
    Shutdown,
}

struct SttOutcome {
    activation: Activation,
    connector: bool,
    at: Instant,
}

enum BrainJob {
    Turn {
        query: String,
        at: Instant,
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
        } = deps;
        coordinator.set_deferred_finish(true);
        let brain = BrainSlot::new(brain);
        let barge_in = Arc::new(AtomicBool::new(config.barge_in_enabled));

        let stats = Arc::new(VoiceStats::default());
        let stt_ready = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let (control_tx, control_rx) = crossbeam_channel::unbounded();
        let (stt_tx, stt_rx) = crossbeam_channel::bounded::<SttJob>(4);
        let (stt_out_tx, stt_out_rx) = crossbeam_channel::unbounded();
        let (brain_tx, brain_rx) = crossbeam_channel::unbounded();
        let (tts_tx, tts_rx) = crossbeam_channel::unbounded();

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
            threads.push(spawn(
                "jarvis-stt",
                Box::new(move || stt_loop(&stt_rx, &stt_out_tx, &activation, &stt_ready)),
            )?);
        }
        {
            let tts_brain = brain_tx.clone();
            threads.push(spawn(
                "jarvis-tts",
                Box::new(move || tts_loop(&tts_rx, &tts_brain, sink.as_ref())),
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
                stats: Arc::clone(&stats),
                ready: Arc::clone(&ready),
                speaking: None,
                deferred: VecDeque::new(),
            };
            threads.push(spawn(
                "jarvis-brain",
                Box::new(move || brain_state.run(&brain_rx)),
            )?);
        }
        {
            let control = ControlLoop {
                core,
                playback: playback.clone(),
                capture,
                activation: Arc::clone(&activation),
                client: brain.clone(),
                barge_in: Arc::clone(&barge_in),
                stt: stt_tx.clone(),
                brain: brain_tx.clone(),
                stats: Arc::clone(&stats),
                stt_ready: Arc::clone(&stt_ready),
                ready,
                config,
                capture_enabled: None,
                pending: None,
                stt_warned: false,
                candidate: None,
                echo_segment: false,
                last_sounding: None,
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
            stats,
            playback,
            brain,
            barge_in,
            stt_ready,
            activation,
        })
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
        VoiceSpeaker(self.brain_tx.clone())
    }

    pub fn stats(&self) -> VoiceStatsSnapshot {
        self.stats.snapshot()
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
    stats: Arc<VoiceStats>,
    stt_ready: Arc<AtomicBool>,
    /// Micrófono + DSP + STT listos (lo lee el cerebro al cerrar un turno).
    ready: Arc<AtomicBool>,
    config: VoiceConfig,
    capture_enabled: Option<bool>,
    pending: Option<PendingTurn>,
    stt_warned: bool,
    /// Voz detectada mientras Jarvis suena: salida en pausa hasta confirmar.
    candidate: Option<BargeCandidate>,
    /// El segmento abierto resultó ser eco: su fin de turno se descarta.
    echo_segment: bool,
    /// Último instante en que Jarvis sonaba (cola de eco sin barge-in).
    last_sounding: Option<Instant>,
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
            CONFIRM_TICK
        } else {
            self.config.poll
        }
    }

    /// Alinea micrófono, detector y esfera con el estado del núcleo. Así el
    /// silencio pedido por cualquier vía (menú, IPC, voz) tiene el mismo
    /// efecto: estado `Muted`, detector muteado y dispositivo liberado.
    fn reconcile(&mut self) {
        let muted = self.core.is_muted();
        if self.is_sounding() {
            self.last_sounding = Some(Instant::now());
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
        if ready {
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
                if let Some(turn) = self.pending.take() {
                    self.dispatch(turn.query, turn.at);
                }
            }
        }
    }

    fn on_voice(&mut self, event: VoiceEvent) {
        if self.core.is_muted() {
            return;
        }
        match event {
            VoiceEvent::VoiceStart { at } => {
                // Con Jarvis sonando, la voz puede ser su propio eco que el
                // AEC no canceló. Se pausa la salida en el acto (silencio
                // ≤ 50 ms, CA-9.1) y se confirma con Jarvis callado: si la
                // voz sigue, es el usuario; si desaparece, era eco (CA-2.1).
                let sounding = self.is_sounding();
                if sounding {
                    self.last_sounding = Some(Instant::now());
                }
                // El usuario empezó a hablar: mantén viva la ventana de
                // seguimiento para que una frase larga no caduque a mitad de
                // su transcripción (HU-29). Solo se rearma si YA estaba
                // abierta: un ruido ambiente jamás debe abrirla desde cero
                // (eso permitiría comandos sin decir "Jarvis").
                if self.activation.is_follow_up_open() {
                    self.activation.open_follow_up();
                }
                if !self.barge_in.load(Ordering::Relaxed) {
                    // Sin barge-in, todo lo que empieza con Jarvis sonando
                    // (o en su cola) es eco: la locución sigue intacta.
                    let in_tail = self
                        .last_sounding
                        .is_some_and(|last| at.saturating_duration_since(last) < ECHO_TAIL);
                    if sounding || in_tail {
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
                if let Some(pending) = &mut self.pending {
                    // El usuario retoma tras un conector: silencio extendido.
                    if let Some(connector) = self.capture.connector() {
                        connector.signal();
                    }
                    pending.deadline = None;
                }
            }
            VoiceEvent::EndOfTurn { at, segment } => {
                self.stats.segments.fetch_add(1, Ordering::Relaxed);
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
                if let Some(held) = self.pending.take() {
                    let merged = format!("{} {query}", held.query);
                    if outcome.connector {
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
                } else if outcome.connector {
                    self.pending = Some(PendingTurn {
                        query,
                        at: outcome.at,
                        deadline: Some(Instant::now() + self.config.connector_grace),
                    });
                } else {
                    self.dispatch(query, outcome.at);
                }
            }
            Activation::WakeOnly | Activation::Rejected => {
                if matches!(outcome.activation, Activation::Rejected) {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                }
                // La continuación no fue voz útil: sale lo retenido.
                if let Some(held) = self.pending.take() {
                    self.dispatch(held.query, held.at);
                }
            }
        }
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
        let user_spoke = candidate.samples > 0 && candidate.voiced * 2 >= candidate.samples;

        if !user_spoke {
            self.playback.resume();
            self.echo_segment = true;
            let n = self.stats.echo_rejected.fetch_add(1, Ordering::Relaxed) + 1;
            println!("[voz] voz descartada como eco de Jarvis ({n} en la sesión).");
            return;
        }

        let current = self.client.get();
        let client: &dyn TokenStream = match &current {
            Some(client) => client.as_ref(),
            None => &NoBrain,
        };
        let interrupted = match self.core.state() {
            JarvisState::Speaking => barge_in(&self.core, &self.playback, client),
            JarvisState::Thinking => {
                // Habla encima del filler: se calla y el turno nuevo (al
                // cerrar el segmento) reemplaza al que está pensando.
                self.playback.stop();
                true
            }
            _ => {
                // La locución terminó durante la confirmación.
                self.playback.resume();
                false
            }
        };
        if interrupted {
            self.stats.barge_ins.fetch_add(1, Ordering::Relaxed);
            self.stats
                .last_barge_in_us
                .store(silence_us, Ordering::Relaxed);
            // Lo que diga ahora se acepta sin repetir "Jarvis".
            self.activation.open_follow_up();
            println!(
                "[voz] barge-in: Jarvis en silencio {silence_us} µs tras detectar voz; confirmado en {} ms.",
                self.config.barge_in_window.as_millis()
            );
        }
    }

    /// `true` si Jarvis está hablando o diciendo un filler.
    fn is_sounding(&self) -> bool {
        matches!(
            self.core.state(),
            JarvisState::Speaking | JarvisState::Thinking
        )
    }

    /// Entrega un turno al cerebro, cancelando el que esté en curso.
    fn dispatch(&mut self, query: String, at: Instant) {
        if matches!(
            self.core.state(),
            JarvisState::Thinking | JarvisState::Speaking
        ) {
            self.cancel_in_flight();
        }
        let _ = self.brain.send(BrainJob::Turn { query, at });
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
) {
    let mut transcriber: Option<Box<dyn Transcriber + Send>> = None;
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
            } => {
                let Some(t) = transcriber.as_deref() else {
                    continue;
                };
                // En silencio ni siquiera se transcribe (CA-4.3).
                if activation.is_muted() {
                    continue;
                }
                match t.transcribe(&pcm) {
                    Ok(text) => {
                        println!("[stt] escuchado: {text}");
                        // `at` marca el cierre del turno: restamos la duración
                        // del segmento para saber cuándo empezó la voz (HU-29).
                        let duration = Duration::from_millis(u64::from(duration_ms));
                        let started_at = at.checked_sub(duration).unwrap_or(at);
                        let outcome = SttOutcome {
                            connector: ends_with_connector(&text),
                            activation: activation.process_text_started_at(&text, started_at),
                            at,
                        };
                        if out.send(outcome).is_err() {
                            break;
                        }
                    }
                    Err(e) => eprintln!("[stt] falló la transcripción: {e}"),
                }
            }
            SttJob::Shutdown => break,
        }
    }
}

// ------------------------------------------------------------------ jarvis-tts

fn tts_loop(jobs: &Receiver<TtsJob>, brain: &Sender<BrainJob>, sink: &dyn AudioSink) {
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
            TtsJob::Audio { generation, pcm } => {
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
    stats: Arc<VoiceStats>,
    ready: Arc<AtomicBool>,
    /// Locución en curso: (generación, qué hacer al terminar).
    speaking: Option<(u64, AfterSpeech)>,
    /// Locuciones pedidas (IPC, avisos) mientras otra sonaba.
    deferred: VecDeque<String>,
}

impl BrainLoop {
    fn run(mut self, jobs: &Receiver<BrainJob>) {
        for job in jobs {
            match job {
                BrainJob::Turn { query, at } => self.on_turn(&query, at),
                BrainJob::Say(text) => {
                    if self.speaking.is_some() {
                        self.deferred.push_back(text);
                    } else {
                        self.say(&text, AfterSpeech::Listen);
                    }
                }
                BrainJob::SetVoice(voice_id) => self.set_voice(&voice_id),
                BrainJob::SpeechDone {
                    generation,
                    outcome,
                } => self.on_speech_done(generation, outcome),
                BrainJob::Shutdown => break,
            }
        }
    }

    fn on_turn(&mut self, query: &str, _at: Instant) {
        // Lo que quedó de un turno cancelado (barge-in) no debe bloquear este.
        self.speaking = None;
        let _ = self
            .core
            .transition_from(JarvisState::Thinking, JarvisState::Listening);
        let _ = self
            .core
            .transition_from(JarvisState::Speaking, JarvisState::Listening);
        // Feedback inmediato sin palabras: chime suave en lugar del filler
        // hablado (HU-18). Se reproduce en cuanto el turno es aceptado.
        self.play_chime();

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
            None => match self.client.get() {
                Some(client) => self.converse(client.as_ref(), query),
                None => self.say(NO_API_KEY_PHRASE, AfterSpeech::Listen),
            },
        }
    }

    /// Cierre de sesión por voz ("silencio", "descansa"...): cierra la ventana
    /// de seguimiento y vuelve a reposo SIN mutear el micrófono. El wake-word
    /// "Jarvis" reactiva la escucha (HU-17).
    fn standby(&mut self) {
        self.stats.local_commands.fetch_add(1, Ordering::Relaxed);
        self.activation.close_follow_up();
        // Corta cualquier locución o razonamiento en curso.
        self.playback.stop();
        if let Some(client) = self.client.get() {
            client.cancel();
        }
        self.speaking = None;
        let _ = self
            .core
            .transition_from(JarvisState::Speaking, JarvisState::Listening);
        let _ = self
            .core
            .transition_from(JarvisState::Thinking, JarvisState::Listening);
        let _ = self.coordinator.finish_speech(false);
    }

    /// Encola el chime de confirmación para que suene sin bloquear el turno.
    fn play_chime(&self) {
        let _ = self.tts.send(TtsJob::Chime { pcm: synth_chime() });
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
            return;
        }
        self.stats.turns.fetch_add(1, Ordering::Relaxed);
        let generation = self.coordinator.utterance_generation();

        let tts = self.tts.clone();
        let result = self.coordinator.run_turn_streaming_with(
            client,
            query,
            &mut |_phrase: &str, pcm: &PcmAudio| {
                let _ = tts.send(TtsJob::Audio {
                    generation,
                    pcm: pcm.clone(),
                });
            },
        );

        if self.playback.is_stale(generation) {
            // Barge-in o silencio: el hilo de voz ya movió el estado.
            return;
        }
        match result {
            Ok(_) => {
                if self.core.state() == JarvisState::Speaking {
                    self.finish_after(generation, AfterSpeech::Listen);
                } else {
                    // Respuesta vacía: no hubo frase que decir.
                    let _ = self
                        .core
                        .transition_from(JarvisState::Thinking, JarvisState::Listening);
                    self.coordinator.finish_speech(false).ok();
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
            }
            Err(ConversationError::Brain(e)) => {
                self.stats.speech_errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("[cerebro] la respuesta falló: {e}");
                self.speak_now(NETWORK_ERROR_PHRASE, AfterSpeech::Listen);
            }
            Err(e) => {
                // Transición rechazada: otro hilo cambió el estado (silencio).
                eprintln!("[voz] turno interrumpido: {e}");
            }
        }
    }

    /// Locución fuera de un turno con el LLM (hora, avisos, IPC).
    fn say(&mut self, text: &str, after: AfterSpeech) {
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
        self.speak_now(text, after);
    }

    /// Habla desde `Thinking` o continúa en `Speaking`.
    ///
    /// Sintetiza **antes** de pasar a `Speaking`: la esfera y el barge-in solo
    /// ven `Speaking` cuando hay audio listo para sonar, no durante el segundo
    /// que tarda la síntesis por red.
    fn speak_now(&mut self, text: &str, after: AfterSpeech) {
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
            let _ = self.tts.send(TtsJob::Audio { generation, pcm });
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
        if let Some(text) = self.deferred.pop_front() {
            self.say(&text, AfterSpeech::Listen);
        }
    }
}

#[cfg(test)]
mod tests;
