//! Presencia de la esfera frente a ventanas a pantalla completa (HU-35).
//!
//! [`FullscreenPolicy`] es una máquina de estados pura: recibe eventos de la
//! ventana activa, el estado de Jarvis y el reloj, y decide una
//! [`PresenceDirective`]. [`FullscreenMonitor`] la conecta con el
//! [`WindowWatcher`] del shell y la consulta el bucle principal en cada frame.

use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use jarvis_core::{
    is_effectively_fullscreen, FullscreenBehavior, JarvisConfig, JarvisState, Opacity,
    OutputGeometry, WindowSnapshot,
};
use jarvis_shell::window_watch::{WindowEvent, WindowWatcher};

/// Tiempo que un cambio de ventana debe mantenerse para aplicarse (CA-35.6).
pub const FULLSCREEN_HYSTERESIS: Duration = Duration::from_millis(300);

/// Tiempo que la esfera sigue visible tras volver a `Idle` (CA-35.4).
pub const TURN_GRACE: Duration = Duration::from_secs(2);

/// Cómo debe presentarse la esfera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceDirective {
    /// Opacidad de usuario sin factor automático.
    Normal,
    /// Factor de atenuación automático (se multiplica por el de usuario).
    Dimmed(Opacity),
    /// Superficie oculta; la escucha sigue activa.
    Hidden,
}

/// Porcentaje 0–100 a opacidad cruda, sin el piso del 20 % de usuario
/// (`fullscreen_dim_pct` admite hasta 5 %).
fn pct_to_opacity(pct: u8) -> Opacity {
    let pct = u32::from(pct.min(100));
    Opacity::from_raw(((pct * 255 + 50) / 100) as u8)
}

/// Ajustes de pantalla completa que el bucle puede cambiar en caliente.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FullscreenSettings {
    pub behavior: FullscreenBehavior,
    /// Factor de atenuación (`fullscreen_dim_pct`).
    pub dim: Opacity,
    /// Factor durante un turno de voz (`fullscreen_active_pct`).
    pub active: Opacity,
}

impl FullscreenSettings {
    pub fn from_config(config: &JarvisConfig) -> Self {
        Self {
            behavior: config.fullscreen_behavior(),
            dim: pct_to_opacity(config.fullscreen_dim_pct()),
            active: pct_to_opacity(config.fullscreen_active_pct()),
        }
    }
}

impl Default for FullscreenSettings {
    fn default() -> Self {
        Self::from_config(&JarvisConfig::default())
    }
}

/// Máquina de estados de la presencia (histéresis + turno de voz).
#[derive(Debug, Clone)]
pub struct FullscreenPolicy {
    settings: FullscreenSettings,
    /// Estado de pantalla completa estable (ya aplicado).
    applied: bool,
    /// Candidato distinto de `applied` y desde cuándo se mantiene.
    pending: Option<(bool, Instant)>,
    /// Última ventana activa ajena a Jarvis.
    window: Option<(WindowSnapshot, OutputGeometry)>,
    /// Output donde está la esfera (`None` = desconocido: se asume el mismo).
    sphere_output: Option<String>,
    /// Hay un turno de voz en curso.
    in_turn: bool,
    /// Momento en que terminó el último turno.
    turn_ended: Option<Instant>,
}

impl FullscreenPolicy {
    /// Constructor del contrato SDD (§4); el bucle usa [`Self::with_settings`].
    #[allow(dead_code)]
    pub fn new(config: &JarvisConfig) -> Self {
        Self::with_settings(FullscreenSettings::from_config(config))
    }

    pub fn with_settings(settings: FullscreenSettings) -> Self {
        Self {
            settings,
            applied: false,
            pending: None,
            window: None,
            sphere_output: None,
            in_turn: false,
            turn_ended: None,
        }
    }

    pub fn settings(&self) -> FullscreenSettings {
        self.settings
    }

    pub fn set_settings(&mut self, settings: FullscreenSettings) {
        self.settings = settings;
    }

    /// App de la ventana activa (para el log).
    pub fn app_id(&self) -> Option<&str> {
        self.window.as_ref().map(|(w, _)| w.app_id.as_str())
    }

    /// Nuevo evento del monitor de ventanas.
    pub fn on_window(&mut self, event: &WindowEvent, sphere_output: Option<&str>, now: Instant) {
        self.sphere_output = sphere_output.map(str::to_string);
        match event {
            // Las ventanas propias (Ajustes, Panel) no cambian nada.
            WindowEvent::Active(window, _) if window.own => return,
            WindowEvent::Active(window, output) => {
                self.window = Some((window.clone(), output.clone()));
            }
            WindowEvent::None => self.window = None,
            // Sin fuente no se finge detección: se vuelve a Normal ya.
            WindowEvent::SourceLost(_) => {
                self.window = None;
                self.applied = false;
                self.pending = None;
                return;
            }
        }
        self.reevaluate(now);
    }

    /// La esfera cambió de monitor.
    pub fn set_sphere_output(&mut self, sphere_output: Option<&str>, now: Instant) {
        if self.sphere_output.as_deref() != sphere_output {
            self.sphere_output = sphere_output.map(str::to_string);
            self.reevaluate(now);
        }
    }

    /// Estado de Jarvis en `now` (abre o cierra el turno de voz).
    pub fn on_state(&mut self, state: JarvisState, now: Instant) {
        let active = matches!(
            state,
            JarvisState::Listening | JarvisState::Thinking | JarvisState::Speaking
        );
        if active {
            self.in_turn = true;
            self.turn_ended = None;
        } else if self.in_turn {
            self.in_turn = false;
            self.turn_ended = Some(now);
        }
    }

    /// Directiva vigente en `now`.
    pub fn directive(&self, now: Instant) -> PresenceDirective {
        let behavior = self.settings.behavior;
        if behavior == FullscreenBehavior::Ignore || !self.fullscreen_at(now) {
            return PresenceDirective::Normal;
        }
        let in_grace = self
            .turn_ended
            .is_some_and(|ended| now.saturating_duration_since(ended) < TURN_GRACE);
        if self.in_turn || in_grace {
            return PresenceDirective::Dimmed(self.settings.active);
        }
        match behavior {
            FullscreenBehavior::Dim => PresenceDirective::Dimmed(self.settings.dim),
            FullscreenBehavior::Hide => PresenceDirective::Hidden,
            FullscreenBehavior::Ignore => PresenceDirective::Normal,
        }
    }

    /// Pantalla completa efectiva en `now`, contando un candidato que ya
    /// superó la histéresis.
    pub fn fullscreen_at(&self, now: Instant) -> bool {
        match self.pending {
            Some((candidate, since))
                if now.saturating_duration_since(since) >= FULLSCREEN_HYSTERESIS =>
            {
                candidate
            }
            _ => self.applied,
        }
    }

    fn candidate(&self) -> bool {
        self.window.as_ref().is_some_and(|(window, output)| {
            is_effectively_fullscreen(window, output)
                && self
                    .sphere_output
                    .as_deref()
                    .is_none_or(|sphere| sphere == output.name)
        })
    }

    fn reevaluate(&mut self, now: Instant) {
        self.applied = self.fullscreen_at(now);
        let candidate = self.candidate();
        if candidate == self.applied {
            self.pending = None;
        } else if self.pending.map(|(c, _)| c) != Some(candidate) {
            self.pending = Some((candidate, now));
        }
    }
}

/// Fábrica de monitores (inyectable en tests).
pub type WatcherFactory = fn(u32, FullscreenBehavior) -> Box<dyn WindowWatcher>;

/// Une el monitor de ventanas del shell con [`FullscreenPolicy`].
pub struct FullscreenMonitor {
    watcher: Box<dyn WindowWatcher>,
    events: Receiver<WindowEvent>,
    policy: FullscreenPolicy,
    jarvis_pid: u32,
    factory: WatcherFactory,
    last: PresenceDirective,
}

impl FullscreenMonitor {
    /// Arranca el monitor adecuado para `settings.behavior`.
    pub fn start(jarvis_pid: u32, settings: FullscreenSettings, factory: WatcherFactory) -> Self {
        let watcher = factory(jarvis_pid, settings.behavior);
        println!(
            "[pantalla] detección de pantalla completa: {}",
            watcher.describe()
        );
        Self {
            events: watcher.events(),
            watcher,
            policy: FullscreenPolicy::with_settings(settings),
            jarvis_pid,
            factory,
            last: PresenceDirective::Normal,
        }
    }

    /// Aplica ajustes, drena los eventos pendientes y devuelve la directiva.
    pub fn update(
        &mut self,
        settings: FullscreenSettings,
        state: JarvisState,
        sphere_output: Option<&str>,
        now: Instant,
    ) -> PresenceDirective {
        self.apply_settings(settings);
        self.policy.set_sphere_output(sphere_output, now);
        while let Ok(event) = self.events.try_recv() {
            self.policy.on_window(&event, sphere_output, now);
        }
        self.policy.on_state(state, now);
        let directive = self.policy.directive(now);
        if directive != self.last {
            self.log_change(directive);
            self.last = directive;
        }
        directive
    }

    /// Detiene el monitor (descarga el script de KWin).
    pub fn shutdown(&mut self) {
        self.watcher.shutdown();
    }

    fn apply_settings(&mut self, settings: FullscreenSettings) {
        let previous = self.policy.settings();
        if settings == previous {
            return;
        }
        let was_ignored = previous.behavior == FullscreenBehavior::Ignore;
        let is_ignored = settings.behavior == FullscreenBehavior::Ignore;
        if was_ignored != is_ignored {
            // Activar o desactivar la detección carga o descarga el backend.
            self.watcher.shutdown();
            self.watcher = (self.factory)(self.jarvis_pid, settings.behavior);
            self.events = self.watcher.events();
            self.policy = FullscreenPolicy::with_settings(settings);
            println!(
                "[pantalla] detección de pantalla completa: {}",
                self.watcher.describe()
            );
        } else {
            self.policy.set_settings(settings);
        }
    }

    fn log_change(&self, directive: PresenceDirective) {
        let app = self.policy.app_id().unwrap_or("?");
        match directive {
            PresenceDirective::Hidden => {
                println!("[pantalla] oculta por pantalla completa ({app})")
            }
            PresenceDirective::Dimmed(o) if o == self.policy.settings().active => {
                println!("[pantalla] visible durante el turno de voz ({app})")
            }
            PresenceDirective::Dimmed(_) => {
                println!("[pantalla] atenuada por pantalla completa ({app})")
            }
            PresenceDirective::Normal => println!("[pantalla] presencia normal"),
        }
    }
}

impl Drop for FullscreenMonitor {
    fn drop(&mut self) {
        self.watcher.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jarvis_core::Rect;
    use jarvis_shell::window_watch::{NullWatcher, WatcherKind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const FULL_HD: Rect = Rect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };

    fn window(app: &str, fullscreen: bool, output: &str, own: bool) -> WindowEvent {
        WindowEvent::Active(
            WindowSnapshot {
                app_id: app.to_string(),
                fullscreen,
                decorated: !fullscreen,
                geometry: FULL_HD,
                output: output.to_string(),
                own,
            },
            OutputGeometry {
                name: output.to_string(),
                rect: FULL_HD,
            },
        )
    }

    fn settings(behavior: FullscreenBehavior) -> FullscreenSettings {
        FullscreenSettings {
            behavior,
            ..FullscreenSettings::default()
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn defaults_come_from_config() {
        let s = FullscreenSettings::default();
        assert_eq!(s.behavior, FullscreenBehavior::Dim);
        assert_eq!(s.dim, Opacity::from_raw(64)); // 25 %
        assert_eq!(s.active, Opacity::from_raw(153)); // 60 %
        assert_eq!(pct_to_opacity(5), Opacity::from_raw(13));
        assert_eq!(pct_to_opacity(100), Opacity::OPAQUE);
        let p = FullscreenPolicy::new(&JarvisConfig::default());
        assert_eq!(p.settings(), s);
    }

    #[test]
    fn fullscreen_dims_only_after_hysteresis() {
        let t0 = Instant::now();
        let mut p = FullscreenPolicy::with_settings(settings(FullscreenBehavior::Dim));
        p.on_window(&window("mpv", true, "DP-1", false), Some("DP-1"), t0);
        assert_eq!(p.directive(t0 + ms(299)), PresenceDirective::Normal);
        assert_eq!(
            p.directive(t0 + ms(300)),
            PresenceDirective::Dimmed(p.settings().dim)
        );

        // Salir de pantalla completa también espera la histéresis.
        p.on_window(&WindowEvent::None, Some("DP-1"), t0 + ms(1000));
        assert_eq!(
            p.directive(t0 + ms(1200)),
            PresenceDirective::Dimmed(p.settings().dim)
        );
        assert_eq!(p.directive(t0 + ms(1300)), PresenceDirective::Normal);
    }

    #[test]
    fn rapid_alt_tab_never_flickers() {
        let t0 = Instant::now();
        let mut p = FullscreenPolicy::with_settings(settings(FullscreenBehavior::Dim));
        p.on_window(&window("game", true, "DP-1", false), None, t0);
        let dimmed = PresenceDirective::Dimmed(p.settings().dim);
        assert_eq!(p.directive(t0 + ms(400)), dimmed);

        // Alt+Tab alternando cada 100 ms: el estado estable no cambia.
        let mut t = t0 + ms(400);
        for i in 0..10 {
            let ev = if i % 2 == 0 {
                window("plasmashell", false, "DP-1", false)
            } else {
                window("game", true, "DP-1", false)
            };
            p.on_window(&ev, None, t);
            assert_eq!(p.directive(t), dimmed, "parpadeo en el paso {i}");
            t += ms(100);
        }
    }

    #[test]
    fn hide_turn_and_grace_sequence() {
        let t0 = Instant::now();
        let mut p = FullscreenPolicy::with_settings(settings(FullscreenBehavior::Hide));
        p.on_window(&window("mpv", true, "DP-1", false), Some("DP-1"), t0);
        let t = t0 + ms(300);
        assert_eq!(p.directive(t), PresenceDirective::Hidden);

        // Wake-word: la esfera aparece al 60 % mientras dura el turno.
        p.on_state(JarvisState::Listening, t);
        let active = PresenceDirective::Dimmed(p.settings().active);
        assert_eq!(p.directive(t), active);
        p.on_state(JarvisState::Speaking, t + ms(3000));
        assert_eq!(p.directive(t + ms(3000)), active);

        // Vuelve a Idle: sigue visible 2 s y luego se oculta.
        let idle = t + ms(5000);
        p.on_state(JarvisState::Idle, idle);
        assert_eq!(p.directive(idle + ms(1999)), active);
        assert_eq!(p.directive(idle + ms(2000)), PresenceDirective::Hidden);
    }

    #[test]
    fn own_windows_are_ignored() {
        let t0 = Instant::now();
        let mut p = FullscreenPolicy::with_settings(settings(FullscreenBehavior::Dim));
        p.on_window(&window("mpv", true, "DP-1", false), None, t0);
        // Abrir los Ajustes de Jarvis no restaura la esfera.
        p.on_window(&window("python3", false, "DP-1", true), None, t0 + ms(400));
        assert_eq!(
            p.directive(t0 + ms(1000)),
            PresenceDirective::Dimmed(p.settings().dim)
        );
        assert_eq!(p.app_id(), Some("mpv"));
    }

    #[test]
    fn other_monitor_does_nothing() {
        let t0 = Instant::now();
        let mut p = FullscreenPolicy::with_settings(settings(FullscreenBehavior::Hide));
        p.on_window(&window("mpv", true, "HDMI-A-1", false), Some("DP-1"), t0);
        assert_eq!(p.directive(t0 + ms(1000)), PresenceDirective::Normal);

        // Si la esfera se mueve a ese monitor, la directiva cambia.
        p.set_sphere_output(Some("HDMI-A-1"), t0 + ms(1000));
        assert_eq!(p.directive(t0 + ms(1300)), PresenceDirective::Hidden);
    }

    #[test]
    fn source_lost_restores_immediately() {
        let t0 = Instant::now();
        let mut p = FullscreenPolicy::with_settings(settings(FullscreenBehavior::Hide));
        p.on_window(&window("mpv", true, "DP-1", false), None, t0);
        assert_eq!(p.directive(t0 + ms(300)), PresenceDirective::Hidden);
        p.on_window(
            &WindowEvent::SourceLost("KWin reiniciado".into()),
            None,
            t0 + ms(400),
        );
        assert_eq!(p.directive(t0 + ms(400)), PresenceDirective::Normal);
    }

    #[test]
    fn ignore_is_always_normal() {
        let t0 = Instant::now();
        let mut p = FullscreenPolicy::with_settings(settings(FullscreenBehavior::Ignore));
        p.on_window(&window("mpv", true, "DP-1", false), None, t0);
        assert_eq!(p.directive(t0 + ms(1000)), PresenceDirective::Normal);
    }

    static STARTED: AtomicUsize = AtomicUsize::new(0);

    fn counting_factory(_pid: u32, behavior: FullscreenBehavior) -> Box<dyn WindowWatcher> {
        STARTED.fetch_add(1, Ordering::SeqCst);
        Box::new(NullWatcher::new(format!("{behavior:?}")))
    }

    /// Fábrica sin contador: los tests que no miden arranques no deben
    /// tocar `STARTED`, porque corren en paralelo con el que sí lo mide.
    fn null_factory(_pid: u32, behavior: FullscreenBehavior) -> Box<dyn WindowWatcher> {
        Box::new(NullWatcher::new(format!("{behavior:?}")))
    }

    #[test]
    fn monitor_restarts_the_watcher_only_when_toggling_ignore() {
        let start = STARTED.load(Ordering::SeqCst);
        let now = Instant::now();
        let dim = settings(FullscreenBehavior::Dim);
        let mut m = FullscreenMonitor::start(1, dim, counting_factory);
        assert_eq!(m.watcher.kind(), WatcherKind::Unavailable);
        m.update(dim, JarvisState::Idle, None, now);
        // Dim → Hide: mismo backend, solo cambia la política.
        m.update(
            settings(FullscreenBehavior::Hide),
            JarvisState::Idle,
            None,
            now,
        );
        assert_eq!(STARTED.load(Ordering::SeqCst) - start, 1);
        // Hide → Ignore → Dim: se descarga y se vuelve a cargar.
        m.update(
            settings(FullscreenBehavior::Ignore),
            JarvisState::Idle,
            None,
            now,
        );
        m.update(dim, JarvisState::Idle, None, now);
        assert_eq!(STARTED.load(Ordering::SeqCst) - start, 3);
        m.shutdown();
    }

    #[test]
    fn monitor_drains_window_events() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let t0 = Instant::now();
        let mut m = FullscreenMonitor::start(1, settings(FullscreenBehavior::Hide), null_factory);
        m.events = rx;
        tx.send(window("mpv", true, "DP-1", false)).unwrap();
        let s = settings(FullscreenBehavior::Hide);
        assert_eq!(
            m.update(s, JarvisState::Idle, Some("DP-1"), t0),
            PresenceDirective::Normal
        );
        assert_eq!(
            m.update(s, JarvisState::Idle, Some("DP-1"), t0 + ms(350)),
            PresenceDirective::Hidden
        );
        assert_eq!(m.last, PresenceDirective::Hidden);
    }
}
