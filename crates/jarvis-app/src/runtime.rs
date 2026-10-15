//! Bucle principal del daemon: ritmo de frames, render de la esfera y apagado.
//!
//! Todo lo que no toca el sistema (reloj de frames, disponibilidad de STT) es
//! puro y se prueba sin compositor ni whisper.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jarvis_audio::models::{ModelManager, ModelStatus, WHISPER_BASE_Q5_1};
use jarvis_audio::ConversationCoordinator;
use jarvis_core::{AudioLevel, JarvisState};
use jarvis_render::{Pixmap, SphereRenderer};
use jarvis_shell::{FrameBuffer, OverlaySurface, SurfaceStatus};

/// Intervalo entre frames en estados activos (60 FPS).
pub const ACTIVE_FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667);

/// Intervalo entre frames en `Idle`/`Muted` (30 FPS) para ahorrar CPU.
pub const IDLE_FRAME_INTERVAL: Duration = Duration::from_nanos(33_333_333);

/// Reloj de frames puro: decide cuándo toca el siguiente frame y el `t` de la
/// animación, sin dormir ni consultar el reloj del sistema.
#[derive(Debug, Clone, Copy)]
pub struct FrameClock {
    start: Instant,
    deadline: Instant,
}

impl FrameClock {
    /// Reloj cuyo primer frame vence inmediatamente.
    pub fn new(now: Instant) -> Self {
        Self {
            start: now,
            deadline: now,
        }
    }

    /// Intervalo objetivo según el estado del asistente.
    pub fn interval_for(state: JarvisState) -> Duration {
        match state {
            JarvisState::Listening | JarvisState::Thinking | JarvisState::Speaking => {
                ACTIVE_FRAME_INTERVAL
            }
            JarvisState::Idle | JarvisState::Muted => IDLE_FRAME_INTERVAL,
        }
    }

    /// `true` si el frame ya venció en `now`.
    pub fn is_due(&self, now: Instant) -> bool {
        now >= self.deadline
    }

    /// Tiempo de espera hasta el próximo frame (cero si ya venció).
    pub fn timeout(&self, now: Instant) -> Duration {
        self.deadline.saturating_duration_since(now)
    }

    /// Tiempo de animación en segundos desde el arranque.
    pub fn t(&self, now: Instant) -> f32 {
        now.saturating_duration_since(self.start).as_secs_f32()
    }

    /// Programa el siguiente frame tras dibujar en `now`.
    ///
    /// Mantiene la cadencia si se va a tiempo; si el frame llegó tarde no
    /// recupera los perdidos: reprograma desde `now`.
    pub fn advance(&mut self, now: Instant, state: JarvisState) -> Instant {
        let interval = Self::interval_for(state);
        let next = self.deadline + interval;
        self.deadline = if next > now { next } else { now + interval };
        self.deadline
    }
}

/// Motivo de salida del bucle principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopExit {
    /// SIGINT/SIGTERM (o flag de apagado activado).
    Signal,
    /// El compositor cerró la superficie o se perdió la conexión.
    SurfaceClosed,
    /// Se alcanzó el número máximo de frames (tests).
    MaxFrames,
}

/// Fuente del estado a dibujar en cada frame: `(estado, nivel de audio)`.
pub type FrameSource<'a> = dyn FnMut() -> (JarvisState, AudioLevel) + 'a;

/// Registro de errores del bucle: cada tipo se avisa una sola vez para no
/// inundar stderr a 60 Hz.
#[derive(Debug, Default)]
struct ErrorOnce {
    seen: HashSet<&'static str>,
}

impl ErrorOnce {
    fn report(&mut self, kind: &'static str, error: &dyn std::fmt::Display) {
        if self.seen.insert(kind) {
            eprintln!("[bucle] {kind}: {error} (se omiten repeticiones)");
        }
    }
}

/// Bucle principal: despacha eventos, renderiza y presenta la esfera.
///
/// El único punto de espera es [`OverlaySurface::pump_events`]. El `pixmap`
/// se reutiliza en cada frame (cero asignaciones). `on_first_frame` se invoca
/// una vez tras presentar el primer frame.
pub fn run_loop(
    surface: &mut dyn OverlaySurface,
    renderer: &SphereRenderer,
    pixmap: &mut Pixmap,
    source: &mut FrameSource<'_>,
    shutdown: &AtomicBool,
    max_frames: Option<u64>,
    on_first_frame: &mut dyn FnMut(),
) -> LoopExit {
    let mut clock = FrameClock::new(Instant::now());
    let mut errors = ErrorOnce::default();
    let mut presented: u64 = 0;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            return LoopExit::Signal;
        }
        if max_frames.is_some_and(|max| presented >= max) {
            return LoopExit::MaxFrames;
        }

        match surface.pump_events(clock.timeout(Instant::now())) {
            Ok(SurfaceStatus::Running) => {}
            Ok(SurfaceStatus::Closed) => return LoopExit::SurfaceClosed,
            Err(e) => errors.report("eventos de la superficie", &e),
        }
        if shutdown.load(Ordering::SeqCst) {
            return LoopExit::Signal;
        }

        // `pump_events` vuelve antes del deadline si llegan eventos.
        let now = Instant::now();
        if !clock.is_due(now) {
            continue;
        }

        let (state, level) = source();
        if surface.wants_frame() {
            match render_and_present(surface, renderer, pixmap, level, state, clock.t(now)) {
                Ok(()) => {
                    presented += 1;
                    if presented == 1 {
                        on_first_frame();
                    }
                }
                Err((kind, e)) => errors.report(kind, &e),
            }
        }
        clock.advance(now, state);
    }
}

fn render_and_present(
    surface: &mut dyn OverlaySurface,
    renderer: &SphereRenderer,
    pixmap: &mut Pixmap,
    level: AudioLevel,
    state: JarvisState,
    t: f32,
) -> Result<(), (&'static str, Box<dyn std::error::Error>)> {
    renderer
        .render_into(pixmap, level, state, t)
        .map_err(|e| ("render", e.into()))?;
    let frame = FrameBuffer::new(pixmap.data(), pixmap.width(), pixmap.height())
        .map_err(|e| ("frame", e.into()))?;
    surface
        .present(&frame)
        .map_err(|e| ("presentación", e.into()))
}

/// Registra SIGINT y SIGTERM sobre un flag compartido (sin `process::exit`).
#[cfg(unix)]
pub fn install_shutdown_flag() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        if let Err(e) = signal_hook::flag::register(signal, Arc::clone(&flag)) {
            eprintln!("[señales] no se pudo registrar la señal {signal}: {e}");
        }
    }
    flag
}

/// Stub para plataformas sin señales POSIX: el flag nunca se activa por ahora.
#[cfg(not(unix))]
pub fn install_shutdown_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// Disponibilidad del reconocimiento de voz (whisper) al arrancar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SttAvailability {
    Ready,
    MissingModel(Option<PathBuf>),
    InvalidModel(String),
    NoDataDir,
}

impl SttAvailability {
    /// Sonda única sobre el gestor de modelos.
    pub fn probe(manager: &ModelManager) -> Self {
        match manager.status(&WHISPER_BASE_Q5_1) {
            ModelStatus::Verified { .. } => SttAvailability::Ready,
            ModelStatus::Missing => {
                SttAvailability::MissingModel(Some(manager.path_of(&WHISPER_BASE_Q5_1)))
            }
            ModelStatus::Invalid { reason } => SttAvailability::InvalidModel(reason),
        }
    }

    /// `true` si el modelo falta o está dañado y debe (re)descargarse.
    pub fn needs_download(&self) -> bool {
        matches!(
            self,
            SttAvailability::MissingModel(_) | SttAvailability::InvalidModel(_)
        )
    }

    /// Etiqueta corta para la línea de arranque (`STT=<estado>`).
    pub fn label(&self) -> &'static str {
        match self {
            SttAvailability::Ready => "listo",
            SttAvailability::MissingModel(_) => "sin-modelo",
            SttAvailability::InvalidModel(_) => "modelo-invalido",
            SttAvailability::NoDataDir => "sin-datadir",
        }
    }

    /// Aviso para stderr cuando el STT queda deshabilitado.
    pub fn warning(&self) -> Option<String> {
        match self {
            SttAvailability::Ready => None,
            SttAvailability::MissingModel(path) => Some(format!(
                "[stt] deshabilitado: modelo no encontrado en {}",
                path.as_deref()
                    .map_or_else(|| "(desconocido)".to_string(), |p| p.display().to_string())
            )),
            SttAvailability::InvalidModel(reason) => Some(format!(
                "[stt] deshabilitado: modelo corrupto o inválido ({reason})"
            )),
            SttAvailability::NoDataDir => {
                Some("[stt] deshabilitado: sin directorio de datos (~/.local/share)".to_string())
            }
        }
    }
}

/// Entra en escucha solo si el STT está disponible.
///
/// Sin STT (o si la transición falla) avisa una vez y deja la esfera en
/// `Idle`: nunca termina el proceso. Devuelve si se entró en `Listening`.
pub fn start_listening_if_ready(
    coordinator: &mut ConversationCoordinator,
    stt: &SttAvailability,
) -> bool {
    if let Some(warning) = stt.warning() {
        eprintln!("{warning}");
        return false;
    }
    match coordinator.start_listening() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[stt] no se pudo entrar en escucha: {e}");
            false
        }
    }
}

/// Registro del progreso de descarga en tramos de 10 % (no inunda el log).
#[derive(Debug, Default)]
pub struct ProgressLog {
    last_decile: Option<u64>,
}

impl ProgressLog {
    /// Devuelve el porcentaje a registrar si se cruzó un tramo nuevo.
    pub fn update(&mut self, done: u64, total: u64) -> Option<u64> {
        if total == 0 {
            return None;
        }
        let percent = done.min(total).saturating_mul(100) / total;
        let decile = percent / 10;
        if self.last_decile.is_some_and(|last| decile <= last) {
            return None;
        }
        self.last_decile = Some(decile);
        Some(decile * 10)
    }
}

/// Descarga y verifica el modelo whisper en un hilo de fondo (primera
/// ejecución, HU-7). Nunca bloquea el arranque: el flag devuelto pasa a `true`
/// cuando el modelo queda verificado en disco.
pub fn spawn_model_download(manager: ModelManager) -> Option<Arc<AtomicBool>> {
    let ready = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ready);
    let spawned = std::thread::Builder::new()
        .name("jarvis-models".to_string())
        .spawn(move || {
            let spec = WHISPER_BASE_Q5_1;
            println!(
                "[stt] descargando {} ({} MB) en {}…",
                spec.id,
                spec.size / (1024 * 1024),
                manager.path_of(&spec).display()
            );
            let mut log = ProgressLog::default();
            let mut progress = |done: u64, total: u64| {
                if let Some(percent) = log.update(done, total) {
                    println!("[stt] descarga del modelo: {percent} %");
                }
            };
            match manager.ensure(&spec, &mut progress) {
                Ok(model) => {
                    println!("[stt] modelo verificado (sha256 {}).", model.sha256());
                    flag.store(true, Ordering::SeqCst);
                }
                Err(e) => eprintln!("[stt] no se pudo descargar el modelo: {e}"),
            }
        });
    match spawned {
        Ok(_) => Some(ready),
        Err(e) => {
            eprintln!("[stt] no se pudo lanzar la descarga del modelo: {e}");
            None
        }
    }
}

/// Si la descarga pendiente terminó, entra en escucha y la da por resuelta.
///
/// Pensada para invocarse en cada frame: solo actúa una vez.
pub fn poll_model_ready(
    pending: &mut Option<Arc<AtomicBool>>,
    coordinator: &mut ConversationCoordinator,
) -> bool {
    if !pending
        .as_ref()
        .is_some_and(|ready| ready.load(Ordering::SeqCst))
    {
        return false;
    }
    *pending = None;
    start_listening_if_ready(coordinator, &SttAvailability::Ready)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_audio::EdgeTtsEngine;
    use jarvis_shell::{HeadlessSurface, SurfaceSize};

    #[test]
    fn frame_clock_uses_60fps_when_active_and_30fps_when_idle() {
        for state in [
            JarvisState::Listening,
            JarvisState::Thinking,
            JarvisState::Speaking,
        ] {
            assert_eq!(FrameClock::interval_for(state), ACTIVE_FRAME_INTERVAL);
        }
        for state in [JarvisState::Idle, JarvisState::Muted] {
            assert_eq!(FrameClock::interval_for(state), IDLE_FRAME_INTERVAL);
        }

        let start = Instant::now();
        let mut clock = FrameClock::new(start);
        assert!(clock.is_due(start));
        let next = clock.advance(start, JarvisState::Speaking);
        assert_eq!(next, start + ACTIVE_FRAME_INTERVAL);
        let next = clock.advance(next, JarvisState::Idle);
        assert_eq!(next, start + ACTIVE_FRAME_INTERVAL + IDLE_FRAME_INTERVAL);
    }

    #[test]
    fn frame_clock_does_not_catch_up_after_a_late_frame() {
        let start = Instant::now();
        let mut clock = FrameClock::new(start);
        clock.advance(start, JarvisState::Speaking);
        // El frame llega 200 ms tarde: no se encadenan frames atrasados.
        let late = start + Duration::from_millis(200);
        let next = clock.advance(late, JarvisState::Speaking);
        assert_eq!(next, late + ACTIVE_FRAME_INTERVAL);
        assert_eq!(clock.timeout(late), ACTIVE_FRAME_INTERVAL);
        assert!((clock.t(late) - 0.2).abs() < 1e-3);
    }

    fn headless(renderer: &SphereRenderer) -> HeadlessSurface {
        HeadlessSurface::new(SurfaceSize::new(renderer.size(), renderer.size()).unwrap())
    }

    #[test]
    fn loop_presents_exactly_max_frames() {
        let renderer = SphereRenderer::with_size(32);
        let mut surface = headless(&renderer);
        let mut pixmap = Pixmap::new(32, 32).unwrap();
        let shutdown = AtomicBool::new(false);
        let mut first = 0;
        let exit = run_loop(
            &mut surface,
            &renderer,
            &mut pixmap,
            &mut || (JarvisState::Speaking, 0.5),
            &shutdown,
            Some(5),
            &mut || first += 1,
        );
        assert_eq!(exit, LoopExit::MaxFrames);
        assert_eq!(surface.presented_frames(), 5);
        assert_eq!(first, 1);
    }

    #[test]
    fn loop_stops_on_shutdown_flag_within_500ms() {
        let renderer = SphereRenderer::with_size(32);
        let mut surface = headless(&renderer);
        let mut pixmap = Pixmap::new(32, 32).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let trigger = Arc::clone(&shutdown);
        let raised_at = Arc::new(std::sync::Mutex::new(None));
        let raised_at_thread = Arc::clone(&raised_at);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            *raised_at_thread.lock().unwrap() = Some(Instant::now());
            trigger.store(true, Ordering::SeqCst);
        });

        let exit = run_loop(
            &mut surface,
            &renderer,
            &mut pixmap,
            &mut || (JarvisState::Idle, 0.0),
            &shutdown,
            None,
            &mut || {},
        );
        let stopped = Instant::now();
        handle.join().unwrap();

        assert_eq!(exit, LoopExit::Signal);
        let raised = raised_at.lock().unwrap().unwrap();
        assert!(stopped.duration_since(raised) < Duration::from_millis(500));
        assert!(surface.presented_frames() > 0);
    }

    #[test]
    fn stt_availability_classifies_parts() {
        // Un directorio sin modelo se clasifica como modelo faltante.
        let dir = std::env::temp_dir().join(format!("jarvis-stt-probe-{}", std::process::id()));
        let manager = ModelManager::with_dir(&dir);
        match SttAvailability::probe(&manager) {
            SttAvailability::MissingModel(Some(path)) => {
                assert_eq!(path, manager.path_of(&WHISPER_BASE_Q5_1));
            }
            other => panic!("se esperaba MissingModel(Some), se obtuvo {other:?}"),
        }

        // Etiquetas y avisos del resto de variantes.
        assert_eq!(SttAvailability::Ready.label(), "listo");
        assert_eq!(
            SttAvailability::InvalidModel("roto".to_string()).label(),
            "modelo-invalido"
        );
        assert_eq!(SttAvailability::NoDataDir.label(), "sin-datadir");
        assert!(SttAvailability::Ready.warning().is_none());
        assert!(SttAvailability::InvalidModel("roto".to_string())
            .warning()
            .is_some());
        assert!(SttAvailability::NoDataDir.warning().is_some());
    }

    #[test]
    fn only_missing_or_invalid_models_are_downloaded() {
        assert!(SttAvailability::MissingModel(None).needs_download());
        assert!(SttAvailability::InvalidModel("roto".to_string()).needs_download());
        assert!(!SttAvailability::Ready.needs_download());
        assert!(!SttAvailability::NoDataDir.needs_download());
    }

    #[test]
    fn progress_is_logged_by_deciles() {
        let mut log = ProgressLog::default();
        assert_eq!(log.update(0, 0), None);
        assert_eq!(log.update(0, 100), Some(0));
        assert_eq!(log.update(5, 100), None);
        assert_eq!(log.update(10, 100), Some(10));
        assert_eq!(log.update(11, 100), None);
        // Un salto grande registra solo el tramo alcanzado.
        assert_eq!(log.update(73, 100), Some(70));
        assert_eq!(log.update(100, 100), Some(100));
        assert_eq!(log.update(100, 100), None);
    }

    #[test]
    fn model_ready_flag_starts_listening_once() {
        let mut coordinator = ConversationCoordinator::new(EdgeTtsEngine::new());
        let flag = Arc::new(AtomicBool::new(false));
        let mut pending = Some(Arc::clone(&flag));

        // Descarga en curso: sigue en Idle.
        assert!(!poll_model_ready(&mut pending, &mut coordinator));
        assert!(pending.is_some());
        assert_eq!(coordinator.state(), JarvisState::Idle);

        flag.store(true, Ordering::SeqCst);
        assert!(poll_model_ready(&mut pending, &mut coordinator));
        assert!(pending.is_none());
        assert_eq!(coordinator.state(), JarvisState::Listening);

        // Ya resuelto: no vuelve a actuar.
        assert!(!poll_model_ready(&mut pending, &mut coordinator));
    }

    #[test]
    fn missing_stt_keeps_assistant_idle() {
        let mut coordinator = ConversationCoordinator::new(EdgeTtsEngine::new());
        let entered =
            start_listening_if_ready(&mut coordinator, &SttAvailability::MissingModel(None));
        assert!(!entered);
        assert_eq!(coordinator.state(), JarvisState::Idle);

        let entered = start_listening_if_ready(&mut coordinator, &SttAvailability::Ready);
        assert!(entered);
        assert_eq!(coordinator.state(), JarvisState::Listening);
    }
}
