//! Bucle principal del daemon: ritmo de frames, render de la esfera y apagado.
//!
//! Todo lo que no toca el sistema (reloj de frames, disponibilidad de STT) es
//! puro y se prueba sin compositor ni whisper.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use jarvis_audio::models::{ModelManager, ModelStatus, WHISPER_BASE_Q5_1};
use jarvis_audio::WhisperEngine;
use jarvis_core::{AudioLevel, JarvisConfig, JarvisState, Opacity};
use jarvis_render::{OpacityFade, Pixmap, RenderError, SphereRenderer};
use jarvis_shell::{FrameBuffer, OverlaySurface, ShellCommand, SurfaceSize, SurfaceStatus};

use crate::hud::{HudFrame, HudPipeline, HudSettings};
use crate::presence::{FullscreenMonitor, FullscreenSettings, PresenceDirective};

/// Intervalo entre frames en estados activos (60 FPS).
pub const ACTIVE_FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667);

/// Intervalo entre frames en `Idle`/`Muted` (60 FPS) para una animación fluida.
pub const IDLE_FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667);

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

/// Duración del fundido entre opacidades (HU-34).
pub const OPACITY_FADE: Duration = Duration::from_millis(150);

/// Apariencia pedida para la esfera: la persistida o la de la vista previa.
///
/// La app la publica en un [`Cell`] y el bucle la aplica antes de dibujar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SphereLook {
    /// Lado de la superficie en píxeles lógicos.
    pub size: u32,
    /// Opacidad de usuario (20–100 %).
    pub opacity: Opacity,
    /// Reacción a ventanas a pantalla completa (HU-35).
    pub fullscreen: FullscreenSettings,
    /// Burbujas de conversación (HU-36).
    pub hud: HudSettings,
}

impl SphereLook {
    /// Apariencia persistida en `config.toml`.
    pub fn from_config(config: &JarvisConfig) -> Self {
        Self {
            size: config.sphere_size_px(),
            opacity: config.sphere_opacity(),
            fullscreen: FullscreenSettings::from_config(config),
            hud: HudSettings::from_config(config),
        }
    }
}

/// Renderer y buffer de la esfera, que cambian juntos de tamaño (HU-33).
pub struct SphereView {
    pub renderer: SphereRenderer,
    pub pixmap: Pixmap,
}

impl SphereView {
    /// Vista de `size × size` píxeles.
    pub fn new(size: u32) -> Result<Self, RenderError> {
        let renderer = SphereRenderer::with_size(size);
        let pixmap = Pixmap::new(renderer.size(), renderer.size())
            .ok_or(RenderError::AllocFailed { size })?;
        Ok(Self { renderer, pixmap })
    }

    /// Lado actual en píxeles.
    pub fn size(&self) -> u32 {
        self.renderer.size()
    }

    /// Reasigna el `Pixmap` solo si el tamaño cambió. Devuelve `true` si cambió.
    pub fn resize(&mut self, size: u32) -> Result<bool, RenderError> {
        if size == self.size() {
            return Ok(false);
        }
        self.pixmap = Pixmap::new(size, size).ok_or(RenderError::AllocFailed { size })?;
        self.renderer.resize(size);
        Ok(true)
    }
}

/// Lleva superficie y vista al tamaño pedido. Si la superficie lo rechaza
/// (backend sin soporte o compositor sin respuesta) se mantiene el anterior.
fn apply_size(
    surface: &mut dyn OverlaySurface,
    view: &mut SphereView,
    size: u32,
) -> Result<(), (&'static str, Box<dyn std::error::Error>)> {
    let surface_size = SurfaceSize::new(size, size).map_err(|e| ("tamaño", e.into()))?;
    surface
        .set_size(surface_size)
        .map_err(|e| ("tamaño de la superficie", e.into()))?;
    if let Err(e) = view.resize(size) {
        // Sin buffer nuevo: la superficie vuelve al tamaño de la vista.
        let current = view.size();
        if let Ok(previous) = SurfaceSize::new(current, current) {
            let _ = surface.set_size(previous);
        }
        return Err(("buffer de la esfera", e.into()));
    }
    Ok(())
}

/// Motivo de salida del bucle principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopExit {
    /// SIGINT/SIGTERM (o flag de apagado activado).
    Signal,
    /// El compositor cerró la superficie o se perdió la conexión.
    SurfaceClosed,
    /// El usuario eligió "Salir" en el menú contextual.
    UserQuit,
    /// Se alcanzó el número máximo de frames (tests).
    MaxFrames,
}

/// Fuente del estado a dibujar en cada frame: `(estado, nivel de audio)`.
pub type FrameSource<'a> = dyn FnMut() -> (JarvisState, AudioLevel) + 'a;

/// Ejecuta las acciones del menú contextual que no terminan el bucle.
pub type CommandHandler<'a> = dyn FnMut(ShellCommand) + 'a;

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
/// El único punto de espera es [`OverlaySurface::pump_events`]. El `Pixmap`
/// de `view` se reutiliza en cada frame (solo se reasigna al cambiar de
/// tamaño). Antes de dibujar aplica `look` (config o vista previa): como
/// máximo un cambio de tamaño por frame y la opacidad con un fundido de
/// [`OPACITY_FADE`]. `presence` (HU-35) multiplica esa opacidad por el factor
/// de pantalla completa u oculta la superficie tras fundirla a transparente.
/// `hud` (HU-36) drena los eventos del orquestador y presenta las burbujas en
/// su propia capa solo cuando cambian o animan.
/// `on_first_frame` se invoca una vez tras presentar el primer frame. "Salir"
/// en el menú contextual termina el bucle; el resto de acciones se entregan a
/// `on_command`.
#[allow(clippy::too_many_arguments)]
pub fn run_loop(
    surface: &mut dyn OverlaySurface,
    view: &mut SphereView,
    look: &Cell<SphereLook>,
    source: &mut FrameSource<'_>,
    shutdown: &AtomicBool,
    reset_position: &AtomicBool,
    max_frames: Option<u64>,
    mut presence: Option<&mut FullscreenMonitor>,
    mut hud: Option<&mut HudPipeline>,
    on_first_frame: &mut dyn FnMut(),
    on_command: &mut CommandHandler<'_>,
) -> LoopExit {
    let mut clock = FrameClock::new(Instant::now());
    let mut errors = ErrorOnce::default();
    let mut presented: u64 = 0;
    let mut opacity_target = look.get().opacity;
    let mut fade = OpacityFade::new(opacity_target, OPACITY_FADE);
    surface.set_opacity(opacity_target);
    // La superficie está mapeada (la oculta solo la directiva `Hidden`).
    let mut shown = true;
    // Último tamaño rechazado: no se reintenta en cada frame.
    let mut rejected_size: Option<u32> = None;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            return LoopExit::Signal;
        }
        if reset_position.swap(false, Ordering::SeqCst) {
            surface.reset_position();
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
        while let Some(command) = surface.poll_command() {
            match command {
                ShellCommand::Quit => return LoopExit::UserQuit,
                other => on_command(other),
            }
        }

        // `pump_events` vuelve antes del deadline si llegan eventos.
        let now = Instant::now();
        if !clock.is_due(now) {
            continue;
        }

        let (state, level) = source();
        surface.sync_muted(state == JarvisState::Muted);

        let target = look.get();
        if target.size != view.size() && rejected_size != Some(target.size) {
            match apply_size(surface, view, target.size) {
                Ok(()) => rejected_size = None,
                Err((kind, e)) => {
                    errors.report(kind, &e);
                    rejected_size = Some(target.size);
                }
            }
        }
        let directive = match presence.as_deref_mut() {
            Some(monitor) => monitor.update(
                target.fullscreen,
                state,
                surface.current_output().as_deref(),
                now,
            ),
            None => PresenceDirective::Normal,
        };
        let (show, auto) = match directive {
            PresenceDirective::Normal => (true, Opacity::OPAQUE),
            PresenceDirective::Dimmed(factor) => (true, factor),
            // Se funde a transparente y luego se desmapea.
            PresenceDirective::Hidden => (false, Opacity::from_raw(0)),
        };
        let wanted = target.opacity.mul(auto);
        if wanted != opacity_target {
            opacity_target = wanted;
            fade.retarget(opacity_target, now);
        }
        if show && !shown {
            surface.set_visible(true);
            shown = true;
        } else if !show && shown && fade.is_settled(now) {
            surface.set_visible(false);
            shown = false;
        }
        surface.set_opacity(fade.value_at(now));

        if surface.wants_frame() {
            match render_and_present(surface, view, level, state, clock.t(now)) {
                Ok(()) => {
                    presented += 1;
                    if presented == 1 {
                        on_first_frame();
                    }
                }
                Err((kind, e)) => errors.report(kind, &e),
            }
        }
        if let Some(hud) = hud.as_deref_mut() {
            // Con la esfera atenuada/oculta fuera de un turno, o en silencio,
            // no aparecen globos nuevos (CA-36.7).
            let quiet = !matches!(directive, PresenceDirective::Normal)
                && !matches!(state, JarvisState::Thinking | JarvisState::Speaking);
            let frame = HudFrame {
                settings: target.hud,
                suppress_new: quiet || state == JarvisState::Muted,
                text_alpha: fade.value_at(now),
            };
            hud.frame(surface, frame, now);
        }
        clock.advance(now, state);
    }
}

fn render_and_present(
    surface: &mut dyn OverlaySurface,
    view: &mut SphereView,
    level: AudioLevel,
    state: JarvisState,
    t: f32,
) -> Result<(), (&'static str, Box<dyn std::error::Error>)> {
    let SphereView { renderer, pixmap } = view;
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
    // SIGUSR1 es "volver a posición inicial" (enviado por el panel); la acción
    // por defecto en Linux es terminar el proceso, así que se ignora.
    unsafe {
        libc::signal(libc::SIGUSR1, libc::SIG_IGN);
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

/// Resultado de la carga de whisper en segundo plano.
pub type SttLoad<T> = Receiver<Result<T, String>>;

/// Verifica (y si falta, descarga) el modelo whisper y lo carga **una sola
/// vez** en un hilo de fondo (HU-7). Nunca bloquea el arranque ni el render:
/// el resultado llega por el canal y lo recoge [`poll_stt_loader`].
pub fn spawn_stt_loader(manager: ModelManager) -> Option<SttLoad<WhisperEngine>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new()
        .name("jarvis-models".to_string())
        .spawn(move || {
            let spec = WHISPER_BASE_Q5_1;
            // Dañado también se descarga: `ensure` lo borra antes.
            if !matches!(manager.status(&spec), ModelStatus::Verified { .. }) {
                println!(
                    "[stt] descargando {} ({} MB) en {}…",
                    spec.id,
                    spec.size / (1024 * 1024),
                    manager.path_of(&spec).display()
                );
            }
            let mut log = ProgressLog::default();
            let mut progress = |done: u64, total: u64| {
                if let Some(percent) = log.update(done, total) {
                    println!("[stt] descarga del modelo: {percent} %");
                }
            };
            let started = Instant::now();
            let result = manager
                .ensure(&spec, &mut progress)
                .map_err(|e| format!("modelo no disponible: {e}"))
                .and_then(|model| {
                    let sha = model.sha256().to_string();
                    WhisperEngine::from_verified(model)
                        .map(|engine| engine.with_initial_prompt("Jarvis."))
                        .map_err(|e| format!("whisper no pudo cargar el modelo: {e}"))
                        .inspect(|_| {
                            println!(
                                "[stt] whisper cargado: {} (sha256 {sha}) en {} ms.",
                                spec.id,
                                started.elapsed().as_millis()
                            )
                        })
                });
            let _ = tx.send(result);
        });
    match spawned {
        Ok(_) => Some(rx),
        Err(e) => {
            eprintln!("[stt] no se pudo lanzar la carga del modelo: {e}");
            None
        }
    }
}

/// Si la carga terminó, entrega el motor a `install` y la da por resuelta.
///
/// Pensada para invocarse en cada frame: `try_recv` es O(1) y solo actúa una
/// vez. Devuelve `true` cuando instaló el transcriptor.
pub fn poll_stt_loader<T>(pending: &mut Option<SttLoad<T>>, install: impl FnOnce(T)) -> bool {
    let Some(rx) = pending.as_ref() else {
        return false;
    };
    match rx.try_recv() {
        Ok(Ok(engine)) => {
            *pending = None;
            install(engine);
            true
        }
        Ok(Err(e)) => {
            *pending = None;
            eprintln!("[stt] {e}; Jarvis no podrá escuchar en esta sesión.");
            false
        }
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => {
            *pending = None;
            eprintln!("[stt] la carga del modelo terminó sin resultado.");
            false
        }
    }
}

/// Hora local del sistema (zona horaria del SO) para "¿qué hora es?".
#[cfg(unix)]
pub fn local_time() -> (u32, u32) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as libc::time_t;
    // SAFETY: `localtime_r` solo escribe en `tm`, que vive en esta pila.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return jarvis_audio::orchestrator::utc_clock();
    }
    (tm.tm_hour as u32, tm.tm_min as u32)
}

/// Sin `localtime_r`: hora UTC.
#[cfg(not(unix))]
pub fn local_time() -> (u32, u32) {
    jarvis_audio::orchestrator::utc_clock()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_core::FullscreenBehavior;
    use jarvis_shell::window_watch::{WatcherKind, WindowEvent, WindowWatcher};
    use jarvis_shell::{HeadlessSurface, SurfaceSize};

    #[test]
    fn frame_clock_uses_60fps_in_every_state() {
        assert_eq!(IDLE_FRAME_INTERVAL, ACTIVE_FRAME_INTERVAL);
        for state in [
            JarvisState::Listening,
            JarvisState::Thinking,
            JarvisState::Speaking,
            JarvisState::Idle,
            JarvisState::Muted,
        ] {
            assert_eq!(FrameClock::interval_for(state), ACTIVE_FRAME_INTERVAL);
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

    fn headless(view: &SphereView) -> HeadlessSurface {
        HeadlessSurface::new(SurfaceSize::new(view.size(), view.size()).unwrap())
    }

    fn look(size: u32) -> Cell<SphereLook> {
        Cell::new(SphereLook {
            size,
            opacity: Opacity::OPAQUE,
            fullscreen: FullscreenSettings::default(),
            hud: HudSettings::default(),
        })
    }

    #[test]
    fn loop_presents_exactly_max_frames() {
        let mut view = SphereView::new(32).unwrap();
        let mut surface = headless(&view);
        let shutdown = AtomicBool::new(false);
        let mut first = 0;
        let exit = run_loop(
            &mut surface,
            &mut view,
            &look(32),
            &mut || (JarvisState::Speaking, 0.5),
            &shutdown,
            &AtomicBool::new(false),
            Some(5),
            None,
            None,
            &mut || first += 1,
            &mut |_| {},
        );
        assert_eq!(exit, LoopExit::MaxFrames);
        assert_eq!(surface.presented_frames(), 5);
        assert_eq!(first, 1);
    }

    #[test]
    fn loop_stops_on_shutdown_flag_within_500ms() {
        let mut view = SphereView::new(32).unwrap();
        let mut surface = headless(&view);
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
            &mut view,
            &look(32),
            &mut || (JarvisState::Idle, 0.0),
            &shutdown,
            &AtomicBool::new(false),
            None,
            None,
            None,
            &mut || {},
            &mut |_| {},
        );
        let stopped = Instant::now();
        handle.join().unwrap();

        assert_eq!(exit, LoopExit::Signal);
        let raised = raised_at.lock().unwrap().unwrap();
        assert!(stopped.duration_since(raised) < Duration::from_millis(500));
        assert!(surface.presented_frames() > 0);
    }

    #[test]
    fn menu_commands_reach_the_app_and_quit_ends_the_loop() {
        let mut view = SphereView::new(32).unwrap();
        let mut surface = headless(&view);
        surface.push_command(ShellCommand::ToggleMute);
        surface.push_command(ShellCommand::Quit);
        // Lo que quede tras "Salir" no se ejecuta.
        surface.push_command(ShellCommand::ToggleMute);
        let shutdown = AtomicBool::new(false);
        let mut handled = Vec::new();
        let exit = run_loop(
            &mut surface,
            &mut view,
            &look(32),
            &mut || (JarvisState::Idle, 0.0),
            &shutdown,
            &AtomicBool::new(false),
            None,
            None,
            None,
            &mut || {},
            &mut |command| handled.push(command),
        );
        assert_eq!(exit, LoopExit::UserQuit);
        assert_eq!(handled, vec![ShellCommand::ToggleMute]);
    }

    #[test]
    fn loop_tells_the_surface_when_muted() {
        let mut view = SphereView::new(32).unwrap();
        let mut surface = headless(&view);
        let shutdown = AtomicBool::new(false);
        run_loop(
            &mut surface,
            &mut view,
            &look(32),
            &mut || (JarvisState::Muted, 0.0),
            &shutdown,
            &AtomicBool::new(false),
            Some(1),
            None,
            None,
            &mut || {},
            &mut |_| {},
        );
        assert!(surface.is_muted());
    }

    #[test]
    fn sphere_view_reallocates_only_on_size_change() {
        let mut view = SphereView::new(32).unwrap();
        assert_eq!(view.resize(32), Ok(false));
        assert_eq!(view.resize(64), Ok(true));
        assert_eq!(view.size(), 64);
        assert_eq!((view.pixmap.width(), view.pixmap.height()), (64, 64));
    }

    #[test]
    fn loop_applies_the_latest_previewed_size_and_opacity() {
        let mut view = SphereView::new(32).unwrap();
        let mut surface = headless(&view);
        let look = look(32);
        let shutdown = AtomicBool::new(false);
        let mut frames = 0;
        // Muchos cambios por frame: solo cuenta el último publicado.
        let mut source = || {
            frames += 1;
            if frames == 2 {
                for size in [40, 48, 56, 64] {
                    look.set(SphereLook {
                        size,
                        opacity: Opacity::from_pct(60),
                        fullscreen: FullscreenSettings::default(),
                        hud: HudSettings::default(),
                    });
                }
            }
            (JarvisState::Idle, 0.0)
        };
        let exit = run_loop(
            &mut surface,
            &mut view,
            &look,
            &mut source,
            &shutdown,
            &AtomicBool::new(false),
            Some(3),
            None,
            None,
            &mut || {},
            &mut |_| {},
        );
        assert_eq!(exit, LoopExit::MaxFrames);
        assert_eq!(view.size(), 64);
        assert_eq!(surface.size(), SurfaceSize::new(64, 64).unwrap());
        assert_eq!(surface.presented_frames(), 3);
        // El fundido ya arrancó hacia el 60 %.
        assert!(surface.opacity() < Opacity::OPAQUE);
        assert!(surface.opacity() >= Opacity::from_pct(60));
    }

    #[test]
    fn loop_reverts_to_the_persisted_look() {
        let persisted = SphereLook {
            size: 32,
            opacity: Opacity::OPAQUE,
            fullscreen: FullscreenSettings::default(),
            hud: HudSettings::default(),
        };
        let mut view = SphereView::new(32).unwrap();
        let mut surface = headless(&view);
        let look = Cell::new(SphereLook {
            size: 48,
            opacity: Opacity::from_pct(30),
            fullscreen: FullscreenSettings::default(),
            hud: HudSettings::default(),
        });
        let shutdown = AtomicBool::new(false);
        let mut frames = 0;
        // Vista previa en el primer frame; "Cancelar" en el segundo.
        let mut source = || {
            frames += 1;
            if frames == 2 {
                look.set(persisted);
            }
            (JarvisState::Idle, 0.0)
        };
        run_loop(
            &mut surface,
            &mut view,
            &look,
            &mut source,
            &shutdown,
            &AtomicBool::new(false),
            Some(2),
            None,
            None,
            &mut || {},
            &mut |_| {},
        );
        assert_eq!(view.size(), 32);
        assert_eq!(surface.size(), SurfaceSize::new(32, 32).unwrap());
    }

    /// Monitor que emite una ventana a pantalla completa y nada más.
    struct FakeWatcher(crossbeam_channel::Receiver<WindowEvent>);

    impl WindowWatcher for FakeWatcher {
        fn kind(&self) -> WatcherKind {
            WatcherKind::KwinScript
        }
        fn events(&self) -> crossbeam_channel::Receiver<WindowEvent> {
            self.0.clone()
        }
        fn shutdown(&mut self) {}
    }

    fn fullscreen_watcher(_pid: u32, _b: FullscreenBehavior) -> Box<dyn WindowWatcher> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let rect = jarvis_core::Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        tx.send(WindowEvent::Active(
            jarvis_core::WindowSnapshot {
                app_id: "mpv".into(),
                fullscreen: true,
                decorated: false,
                geometry: rect,
                output: "DP-1".into(),
                own: false,
            },
            jarvis_core::OutputGeometry {
                name: "DP-1".into(),
                rect,
            },
        ))
        .unwrap();
        Box::new(FakeWatcher(rx))
    }

    fn run_with_presence(behavior: FullscreenBehavior, frames: u64) -> HeadlessSurface {
        let mut view = SphereView::new(32).unwrap();
        let mut surface = headless(&view);
        surface.set_visible(true);
        let fullscreen = FullscreenSettings {
            behavior,
            ..FullscreenSettings::default()
        };
        let look = Cell::new(SphereLook {
            size: 32,
            opacity: Opacity::OPAQUE,
            fullscreen,
            hud: HudSettings::default(),
        });
        let mut monitor = FullscreenMonitor::start(1, fullscreen, fullscreen_watcher);
        // 60 FPS: suficientes frames para cruzar histéresis + fundido.
        run_loop(
            &mut surface,
            &mut view,
            &look,
            &mut || (JarvisState::Idle, 0.0),
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            Some(frames),
            Some(&mut monitor),
            None,
            &mut || {},
            &mut |_| {},
        );
        surface
    }

    #[test]
    fn loop_drives_the_hud_only_when_there_are_bubbles() {
        let mut view = SphereView::new(32).unwrap();
        let mut surface = headless(&view);
        let (tx, rx) = crossbeam_channel::bounded(64);
        let mut hud = HudPipeline::new(rx);
        let shutdown = AtomicBool::new(false);
        // Sin eventos: la capa existe pero no presenta (CA-36.9).
        run_loop(
            &mut surface,
            &mut view,
            &look(32),
            &mut || (JarvisState::Listening, 0.0),
            &shutdown,
            &AtomicBool::new(false),
            Some(5),
            None,
            Some(&mut hud),
            &mut || {},
            &mut |_| {},
        );
        assert!(surface.hud_layer().is_some(), "HUD habilitado por defecto");
        assert_eq!(surface.hud_presented_frames(), 0);

        tx.send(jarvis_audio::HudEvent::UserSaid {
            turn: 1,
            text: "qué hora es".into(),
        })
        .unwrap();
        run_loop(
            &mut surface,
            &mut view,
            &look(32),
            &mut || (JarvisState::Thinking, 0.0),
            &shutdown,
            &AtomicBool::new(false),
            Some(3),
            None,
            Some(&mut hud),
            &mut || {},
            &mut |_| {},
        );
        assert!(surface.hud_presented_frames() >= 1);
        assert!(surface.hud_layer().unwrap().is_visible());
    }

    #[test]
    fn loop_dims_the_sphere_on_fullscreen() {
        // ~0,6 s: 300 ms de histéresis + 150 ms de fundido.
        let surface = run_with_presence(FullscreenBehavior::Dim, 36);
        assert!(surface.is_visible());
        assert_eq!(surface.opacity(), FullscreenSettings::default().dim);
    }

    #[test]
    fn loop_hides_the_sphere_on_fullscreen() {
        let surface = run_with_presence(FullscreenBehavior::Hide, 36);
        assert!(!surface.is_visible());
    }

    #[test]
    fn loop_ignores_fullscreen_when_disabled() {
        let surface = run_with_presence(FullscreenBehavior::Ignore, 36);
        assert!(surface.is_visible());
        assert_eq!(surface.opacity(), Opacity::OPAQUE);
    }

    #[test]
    fn opacity_fade_settles_on_the_target() {
        let start = Instant::now();
        let mut fade = OpacityFade::new(Opacity::OPAQUE, OPACITY_FADE);
        fade.retarget(Opacity::from_pct(20), start);
        assert_eq!(fade.value_at(start + OPACITY_FADE), Opacity::from_pct(20));
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
    fn stt_loader_installs_once_and_is_then_resolved() {
        let (tx, rx) = crossbeam_channel::bounded::<Result<u32, String>>(1);
        let mut pending = Some(rx);
        let mut installed = Vec::new();

        // Carga en curso: no instala nada y sigue pendiente.
        assert!(!poll_stt_loader(&mut pending, |v| installed.push(v)));
        assert!(pending.is_some());

        tx.send(Ok(7)).unwrap();
        assert!(poll_stt_loader(&mut pending, |v| installed.push(v)));
        assert_eq!(installed, vec![7]);
        assert!(pending.is_none());

        // Ya resuelto: no vuelve a actuar.
        assert!(!poll_stt_loader(&mut pending, |v| installed.push(v)));
    }

    #[test]
    fn stt_loader_failure_is_resolved_without_installing() {
        let (tx, rx) = crossbeam_channel::bounded::<Result<u32, String>>(1);
        let mut pending = Some(rx);
        tx.send(Err("sin red".to_string())).unwrap();
        assert!(!poll_stt_loader(&mut pending, |_| panic!(
            "no debe instalar"
        )));
        assert!(pending.is_none());

        let (tx, rx) = crossbeam_channel::bounded::<Result<u32, String>>(1);
        let mut pending = Some(rx);
        drop(tx);
        assert!(!poll_stt_loader(&mut pending, |_| panic!(
            "no debe instalar"
        )));
        assert!(pending.is_none());
    }

    #[test]
    fn local_time_is_a_valid_clock_reading() {
        let (h, m) = local_time();
        assert!(h < 24 && m < 60);
    }
}
