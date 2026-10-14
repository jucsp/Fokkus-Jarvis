//! Punto de entrada de Fokkus-Jarvis.
//!
//! Orquesta el arranque completo del asistente combinando `jarvis-core`,
//! `jarvis-render`, `jarvis-shell`, `jarvis-audio` y `jarvis-ipc`:
//!
//! 1. Analiza los argumentos de línea de comandos (`--version`, `--check`).
//! 2. Adquiere el lock de instancia única.
//! 3. Carga la configuración persistente (`~/.config/fokkus-jarvis/config.toml`).
//! 4. Instancia el núcleo ([`JarvisCore`]), el motor TTS ([`EdgeTtsEngine`]), la
//!    señal de reproducción ([`AudioPlayback`]) y el gestor conversacional
//!    ([`ConversationCoordinator`]) de los Issues #3 y #4.
//! 5. Arranca el servidor IPC en `~/.fokkus/jarvis.sock` en un hilo de fondo
//!    no bloqueante, listo para Fokkus IDE (Issue #5).
//! 6. Pre-sintetiza los fillers de caché en segundo plano si hay backend TTS.
//! 7. Renderiza la esfera líquida en la superficie overlay.

use std::process::ExitCode;
use std::sync::Arc;

use jarvis_audio::{
    AudioPlayback, ConversationCoordinator, EdgeTtsEngine, TtsBackend, WhisperEngine,
};
use jarvis_core::single_instance::{self, InstanceError};
use jarvis_core::{JarvisConfig, JarvisCore};
use jarvis_ipc::{IpcError, IpcServer, ServerState, SpeechSink, Utterance};
use jarvis_render::SphereRenderer;
use jarvis_shell::{
    Anchor, FrameBuffer, OverlaySurface, SurfaceBackend, SurfaceError, SurfaceSize,
};

/// Acción solicitada en la línea de comandos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliAction {
    /// Imprime la versión y sale con éxito.
    Version,
    /// Diagnostica la salud de los subsistemas y sale.
    Check,
    /// Muestra la ayuda y sale.
    Help,
    /// Arranca el asistente (comportamiento por defecto).
    Run,
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

/// Sumidero de locuciones del servidor IPC: arranca una locución en el motor de
/// audio cuando Fokkus IDE pide hablar por el socket.
struct PlaybackSpeechSink {
    playback: AudioPlayback,
}

impl SpeechSink for PlaybackSpeechSink {
    fn speak(&self, _utterance: &Utterance) {
        self.playback.begin_utterance();
    }
}

/// Superficie de prueba que valida los frames sin abrir una ventana real.
struct HeadlessSurface {
    size: SurfaceSize,
}

impl OverlaySurface for HeadlessSurface {
    fn size(&self) -> SurfaceSize {
        self.size
    }

    fn present(&mut self, frame: &FrameBuffer<'_>) -> Result<(), SurfaceError> {
        let expected = self.size.width as usize * self.size.height as usize * 4;
        if frame.data.len() != expected {
            return Err(SurfaceError::InvalidFrame {
                expected,
                actual: frame.data.len(),
            });
        }
        Ok(())
    }

    fn set_visible(&mut self, _visible: bool) {}

    fn set_anchor(&mut self, _anchor: Anchor) {}
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
        Ok(CliAction::Run) => run(),
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
        Some(other) => Err(format!("argumento desconocido: {other}")),
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
    println!();
    println!("Sin opciones, arranca Fokkus-Jarvis, levanta el servidor IPC y deja");
    println!("el socket listo para conexiones de Fokkus IDE.");
}

/// Arranca el asistente completo.
fn run() -> ExitCode {
    // Instancia única: impide que corran dos procesos concurrentes.
    let _lock = match single_instance::acquire() {
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

    // Configuración persistente. Si el archivo está corrupto, se degrada a los
    // valores por defecto para no bloquear el arranque.
    let config = match JarvisConfig::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Configuración ilegible ({e}); usando valores por defecto.");
            JarvisConfig::default()
        }
    };

    // Subsistemas del diálogo (Issues #3 y #4).
    let core = JarvisCore::new(config.clone());
    let tts = EdgeTtsEngine::with_voice(config.voice_id.clone());
    let tts_backend = tts.backend();
    let playback = AudioPlayback::new();
    let mut coordinator = ConversationCoordinator::with_config(tts, config.clone());

    // Servidor IPC para Fokkus IDE (Issue #5): hilo de fondo no bloqueante.
    let sink = Arc::new(PlaybackSpeechSink { playback });
    let state = Arc::new(ServerState::new(JarvisCore::new(config.clone())).with_speech_sink(sink));
    let mut server = IpcServer::new(state);
    if let Err(e) = server.start() {
        eprintln!("No se pudo arrancar el servidor IPC: {e}");
        return ExitCode::FAILURE;
    }
    let socket_path = server.socket_path().to_path_buf();

    // Pre-síntesis de fillers en segundo plano: nunca bloquea el arranque.
    spawn_filler_pre_synthesis(config.voice_id.clone());

    // Superficie overlay con la esfera líquida.
    let renderer = SphereRenderer::new();
    let mut surface = match prepare_surface(&renderer) {
        Ok(surface) => surface,
        Err(e) => {
            eprintln!("No se pudo preparar la superficie overlay: {e}");
            return ExitCode::FAILURE;
        }
    };
    surface.set_visible(true);
    if let Err(e) = present_frame(&renderer, &mut surface, &core, 0.0) {
        eprintln!("No se pudo presentar el primer frame: {e}");
        return ExitCode::FAILURE;
    }

    // El asistente queda a la escucha del usuario.
    if let Err(e) = coordinator.start_listening() {
        eprintln!("No se pudo entrar en escucha: {e}");
        return ExitCode::FAILURE;
    }

    println!(
        "Fokkus-Jarvis {} — voz={} ({}) · TTS={:?} · estado={:?}",
        env!("CARGO_PKG_VERSION"),
        config.voice_id,
        config.voice_lang,
        tts_backend,
        coordinator.state(),
    );
    println!(
        "Esfera renderizada: {}x{} px (backend detectado: {:?})",
        renderer.size(),
        renderer.size(),
        SurfaceBackend::detect(),
    );
    println!(
        "Fokkus-Jarvis iniciado correctamente. Socket IPC activo en {} — listo para conexiones de Fokkus IDE.",
        socket_path.display(),
    );

    ExitCode::SUCCESS
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

    // 4. Backend STT (Whisper).
    let stt = WhisperEngine::new();
    match stt.binary() {
        Some(binary) => {
            let model_ready = stt.model().is_some_and(|path| path.is_file());
            if model_ready {
                checks.push((
                    "STT",
                    Health::Ok,
                    format!("whisper ({}) con modelo local", binary.display()),
                ));
            } else {
                checks.push((
                    "STT",
                    Health::Warn,
                    format!("whisper ({}) sin modelo local", binary.display()),
                ));
            }
        }
        None => checks.push((
            "STT",
            Health::Warn,
            "sin whisper-cli/whisper-cpp en PATH".to_string(),
        )),
    }

    // 5. Socket IPC.
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

/// Prepara la superficie overlay a partir del tamaño del renderizador.
fn prepare_surface(renderer: &SphereRenderer) -> Result<HeadlessSurface, SurfaceError> {
    let size = SurfaceSize::new(renderer.size(), renderer.size())?;
    Ok(HeadlessSurface { size })
}

/// Renderiza y presenta un frame de la esfera.
fn present_frame(
    renderer: &SphereRenderer,
    surface: &mut HeadlessSurface,
    core: &JarvisCore,
    t: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    let frame = renderer.render(core.audio_level(), core.state(), t)?;
    let frame_buffer = FrameBuffer::new(frame.data(), frame.width(), frame.height())?;
    surface.present(&frame_buffer)?;
    Ok(())
}
