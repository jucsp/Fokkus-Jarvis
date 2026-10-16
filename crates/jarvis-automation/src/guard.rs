//! `AutomationGuard`: la única puerta hacia los ojos y las manos (§1.2).
//!
//! Ninguna captura ni acción de entrada ocurre sin un [`Lease`] vigente. El
//! *lease* comprueba, antes de cada operación: kill switch, expiración, tasa
//! (*token bucket*), lista de exclusión y que la app destino siga activa. Todo
//! queda en la auditoría JSONL.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::audit::{result_json, AuditLog};
use crate::capture::{CaptureError, CaptureTarget, CapturedFrame, ScreenCapture};
use crate::input::{InputAction, InputError, InputSynth};

/// Clasificación de riesgo de una acción (la fija el planificador HU-39 con
/// reglas locales; el guard la audita y la expone en el *lease*).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionRiskLevel {
    /// Multimedia, scroll, navegación pasiva, juegos: autónomo.
    Low,
    /// Reversible: borradores, pestañas, búsquedas.
    Medium,
    /// Envíos, borrados, terminal, compras, credenciales: verificación
    /// estricta + confirmación vocal.
    High,
}

impl ActionRiskLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Por qué se activó el kill switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillReason {
    /// "para", "detente", "cancela".
    Voice,
    /// El usuario movió el mouse físico.
    PhysicalMouse,
    /// `Esc` dos veces.
    DoubleEscape,
    /// Expiración o permiso revocado.
    Timeout,
    /// Apagado de Jarvis.
    Shutdown,
}

impl KillReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Voice => "voz",
            Self::PhysicalMouse => "mouse físico",
            Self::DoubleEscape => "doble Esc",
            Self::Timeout => "tiempo agotado",
            Self::Shutdown => "apagado",
        }
    }
}

/// Error del guard.
#[derive(Debug, Clone, PartialEq)]
pub enum GuardError {
    /// `[automation] enabled = false`.
    Disabled,
    /// El kill switch revocó el *lease*.
    Killed(KillReason),
    /// El *lease* caducó.
    LeaseExpired,
    /// Ya hay otro *lease* activo (uno a la vez).
    LeaseActive,
    /// Se superó el tope de acciones por segundo.
    RateLimited,
    /// La ventana destino o la activa está en la lista de exclusión.
    ExcludedWindow(String),
    /// La app destino del *lease* ya no es la activa (TOCTOU).
    TargetNotActive {
        expected: String,
        active: String,
    },
    Capture(CaptureError),
    Input(InputError),
}

impl fmt::Display for GuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("la automatización está desactivada"),
            Self::Killed(r) => write!(f, "automatización detenida ({})", r.label()),
            Self::LeaseExpired => f.write_str("el permiso de automatización caducó"),
            Self::LeaseActive => f.write_str("ya hay una automatización en curso"),
            Self::RateLimited => f.write_str("demasiadas acciones por segundo"),
            Self::ExcludedWindow(app) => write!(f, "ventana excluida de la automatización: {app}"),
            Self::TargetNotActive { expected, active } => {
                write!(f, "la ventana activa es {active}, no {expected}")
            }
            Self::Capture(e) => e.fmt(f),
            Self::Input(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for GuardError {}

/// Identificador de un *lease*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LeaseId(pub u64);

/// Apps que nunca se capturan ni controlan, además de las del usuario:
/// gestores de contraseñas, autenticación de privilegios y pantalla de
/// bloqueo (CA-38.7). Se comparan como fragmento, sin mayúsculas.
pub const BUILTIN_EXCLUDED_APPS: &[&str] = &[
    "keepassxc",
    "bitwarden",
    "1password",
    "kwalletmanager",
    "seahorse",
    "polkit",
    "pkexec",
    "sudo",
    "ksshaskpass",
    "ssh-askpass",
    "pinentry",
    "kscreenlocker",
    "lockscreen",
    "gnome-shell-lock",
    "consent.exe",
    "credentialuibroker",
];

/// Configuración efectiva del guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardConfig {
    pub enabled: bool,
    pub max_actions_per_sec: u32,
    pub lease_duration: Duration,
    pub excluded_apps: Vec<String>,
}

impl From<&jarvis_core::AutomationConfig> for GuardConfig {
    fn from(cfg: &jarvis_core::AutomationConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            max_actions_per_sec: cfg.max_actions_per_sec,
            lease_duration: Duration::from_secs(cfg.lease_secs),
            excluded_apps: cfg.excluded_apps.clone(),
        }
    }
}

/// Devuelve la app (id o clase) de la ventana activa; la inyecta el binario
/// desde el monitor de ventanas de HU-35.
pub type ActiveAppProvider = Box<dyn Fn() -> Option<String> + Send + Sync>;

/// *Token bucket*: ráfaga de hasta `capacity` y recarga continua.
#[derive(Debug)]
struct TokenBucket {
    capacity: f64,
    tokens: f64,
    per_sec: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(per_sec: u32, now: Instant) -> Self {
        let per_sec = f64::from(per_sec.max(1));
        Self {
            capacity: per_sec,
            tokens: per_sec,
            per_sec,
            last: now,
        }
    }

    fn try_take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.per_sec).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

struct Inner {
    active: Option<u64>,
    bucket: TokenBucket,
    last_kill: Option<KillReason>,
}

/// Caracteres por tramo de [`InputAction::TypeText`]: entre tramos se mira el
/// kill switch, así un texto largo se corta en ≤ 100 ms (CA-38.5).
const TYPE_CHUNK_CHARS: usize = 12;

pub struct AutomationGuard {
    cfg: GuardConfig,
    inner: Mutex<Inner>,
    /// Sube con cada kill: un *lease* de una generación anterior está revocado.
    generation: AtomicU64,
    killed: AtomicBool,
    next_id: AtomicU64,
    /// Indicador visual "actuando" (anillo ámbar de la esfera, CA-38.6).
    acting: Arc<AtomicBool>,
    active_app: Option<ActiveAppProvider>,
    audit: Option<AuditLog>,
}

impl AutomationGuard {
    pub fn new(cfg: GuardConfig) -> Self {
        let bucket = TokenBucket::new(cfg.max_actions_per_sec, Instant::now());
        Self {
            cfg,
            inner: Mutex::new(Inner {
                active: None,
                bucket,
                last_kill: None,
            }),
            generation: AtomicU64::new(0),
            killed: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            acting: Arc::new(AtomicBool::new(false)),
            active_app: None,
            audit: None,
        }
    }

    /// Guard desde `[automation]`, con auditoría en la ruta por defecto.
    pub fn from_config(cfg: &jarvis_core::AutomationConfig) -> Self {
        Self::new(GuardConfig::from(cfg)).with_audit(AuditLog::new(
            crate::audit::default_audit_path(),
            cfg.audit_log_text,
        ))
    }

    pub fn with_audit(mut self, audit: AuditLog) -> Self {
        self.audit = Some(audit);
        self
    }

    pub fn with_active_app(mut self, provider: ActiveAppProvider) -> Self {
        self.active_app = Some(provider);
        self
    }

    pub fn config(&self) -> &GuardConfig {
        &self.cfg
    }

    /// Bandera del indicador "actuando" para el renderer.
    pub fn acting_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.acting)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, event: serde_json::Value) {
        if let Some(audit) = &self.audit {
            audit.record(event);
        }
    }

    /// Entrada de la lista de exclusión que coincide con `app`, si alguna.
    pub fn excluded_match(&self, app: &str) -> Option<String> {
        let app = app.trim().to_lowercase();
        if app.is_empty() {
            return None;
        }
        BUILTIN_EXCLUDED_APPS
            .iter()
            .map(|s| s.to_string())
            .chain(
                self.cfg
                    .excluded_apps
                    .iter()
                    .map(|s| s.trim().to_lowercase()),
            )
            .find(|entry| !entry.is_empty() && app.contains(entry.as_str()))
    }

    fn active_app(&self) -> Option<String> {
        self.active_app.as_ref().and_then(|f| f())
    }

    /// *Lease* de riesgo medio para `purpose` sobre `target_app`.
    pub fn acquire(&self, purpose: &str, target_app: &str) -> Result<Lease<'_>, GuardError> {
        self.acquire_with_risk(purpose, target_app, ActionRiskLevel::Medium)
    }

    /// Abre un *lease*. Falla si la automatización está apagada, si la app
    /// destino (o la activa) está excluida o si ya hay otro *lease* activo.
    pub fn acquire_with_risk(
        &self,
        purpose: &str,
        target_app: &str,
        risk: ActionRiskLevel,
    ) -> Result<Lease<'_>, GuardError> {
        if !self.cfg.enabled {
            return Err(GuardError::Disabled);
        }
        for app in [Some(target_app.to_string()), self.active_app()]
            .into_iter()
            .flatten()
        {
            if self.excluded_match(&app).is_some() {
                self.record(json!({
                    "event": "lease_denied", "app": app, "purpose": purpose,
                    "reason": "excluded_window",
                }));
                return Err(GuardError::ExcludedWindow(app));
            }
        }
        let mut inner = self.lock();
        if inner.active.is_some() {
            return Err(GuardError::LeaseActive);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        inner.active = Some(id);
        // Un lease nuevo es una decisión explícita: limpia el kill anterior.
        self.killed.store(false, Ordering::SeqCst);
        let generation = self.generation.load(Ordering::SeqCst);
        drop(inner);
        self.acting.store(true, Ordering::SeqCst);
        let expires_at = Instant::now() + self.cfg.lease_duration;
        self.record(json!({
            "event": "lease_acquired", "lease": id, "app": target_app,
            "purpose": purpose, "risk": risk.label(),
            "lease_secs": self.cfg.lease_duration.as_secs_f64(),
        }));
        Ok(Lease {
            guard: self,
            id,
            generation,
            expires_at,
            target_app: target_app.to_string(),
            risk,
            actions: 0,
        })
    }

    /// Kill switch: revoca el *lease* activo y aborta lo pendiente. Idempotente
    /// y seguro desde cualquier hilo. Devuelve `true` si había un *lease*.
    pub fn kill(&self, reason: KillReason) -> bool {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.killed.store(true, Ordering::SeqCst);
        self.acting.store(false, Ordering::SeqCst);
        let mut inner = self.lock();
        inner.last_kill = Some(reason);
        let active = inner.active;
        drop(inner);
        // Sin lease no hay nada que auditar ("para" en plena charla).
        if let Some(lease) = active {
            self.record(json!({
                "event": "kill", "reason": reason.label(), "lease": lease,
            }));
        }
        active.is_some()
    }

    /// `true` desde un kill hasta el siguiente `acquire`.
    pub fn is_killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }

    /// `true` si hay un *lease* abierto y no revocado.
    pub fn is_acting(&self) -> bool {
        self.acting.load(Ordering::SeqCst)
    }

    fn last_kill(&self) -> KillReason {
        self.lock().last_kill.unwrap_or(KillReason::Shutdown)
    }

    fn take_token(&self) -> bool {
        self.lock().bucket.try_take(Instant::now())
    }
}

/// Permiso temporal de automatización. Única puerta hacia
/// [`InputSynth::perform`] y [`ScreenCapture::capture`].
pub struct Lease<'g> {
    guard: &'g AutomationGuard,
    id: u64,
    generation: u64,
    expires_at: Instant,
    target_app: String,
    risk: ActionRiskLevel,
    actions: u64,
}

impl Lease<'_> {
    pub fn id(&self) -> LeaseId {
        LeaseId(self.id)
    }

    pub fn risk(&self) -> ActionRiskLevel {
        self.risk
    }

    pub fn target_app(&self) -> &str {
        &self.target_app
    }

    /// Tiempo restante antes de caducar.
    pub fn remaining(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }

    fn check_killed(&self) -> Result<(), GuardError> {
        if self.guard.generation.load(Ordering::SeqCst) != self.generation {
            return Err(GuardError::Killed(self.guard.last_kill()));
        }
        Ok(())
    }

    /// Validaciones previas a toda operación (sin consumir tasa).
    fn check(&self) -> Result<(), GuardError> {
        self.check_killed()?;
        if Instant::now() >= self.expires_at {
            return Err(GuardError::LeaseExpired);
        }
        if let Some(active) = self.guard.active_app() {
            if self.guard.excluded_match(&active).is_some() {
                return Err(GuardError::ExcludedWindow(active));
            }
            let (a, t) = (active.to_lowercase(), self.target_app.to_lowercase());
            if !t.is_empty() && !a.contains(&t) && !t.contains(&a) {
                return Err(GuardError::TargetNotActive {
                    expected: self.target_app.clone(),
                    active,
                });
            }
        }
        Ok(())
    }

    fn audit_action(&self, action: &InputAction, result: &Result<(), GuardError>) {
        if let Some(audit) = &self.guard.audit {
            let r = result.as_ref().map(|_| ()).map_err(|e| e.to_string());
            audit.action(self.id, &self.target_app, action, &r);
        }
    }

    /// Ejecuta una acción de entrada tras validar kill, expiración, tasa,
    /// exclusiones y ventana activa. El texto largo se teclea por tramos
    /// comprobando el kill switch entre ellos.
    pub fn perform(
        &mut self,
        synth: &mut dyn InputSynth,
        action: InputAction,
    ) -> Result<(), GuardError> {
        let result = self.perform_inner(synth, &action);
        self.audit_action(&action, &result);
        if let Err(GuardError::Input(InputError::PermissionDenied)) = &result {
            // Permiso revocado en Ajustes: se corta todo (§5).
            self.guard.kill(KillReason::Timeout);
        }
        result
    }

    fn perform_inner(
        &mut self,
        synth: &mut dyn InputSynth,
        action: &InputAction,
    ) -> Result<(), GuardError> {
        self.check()?;
        if !self.guard.take_token() {
            return Err(GuardError::RateLimited);
        }
        self.actions += 1;
        match action {
            InputAction::TypeText(text) if text.chars().count() > TYPE_CHUNK_CHARS => {
                let chars: Vec<char> = text.chars().collect();
                for chunk in chars.chunks(TYPE_CHUNK_CHARS) {
                    self.check_killed()?;
                    synth
                        .perform(&InputAction::TypeText(chunk.iter().collect()))
                        .map_err(GuardError::Input)?;
                }
                Ok(())
            }
            _ => synth.perform(action).map_err(GuardError::Input),
        }
    }

    /// Captura tras validar kill, expiración, exclusiones y ventana activa.
    pub fn capture(
        &mut self,
        cap: &mut dyn ScreenCapture,
        target: &CaptureTarget,
    ) -> Result<CapturedFrame, GuardError> {
        let result = self
            .check()
            .and_then(|()| cap.capture(target).map_err(GuardError::Capture));
        self.guard.record(json!({
            "event": "capture", "lease": self.id, "app": self.target_app,
            "backend": cap.kind().label(), "target": format!("{target:?}"),
            "result": result_json(&result.as_ref().map(|_| ()).map_err(|e| e.to_string())),
        }));
        result
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut inner = self.guard.lock();
        if inner.active == Some(self.id) {
            inner.active = None;
            drop(inner);
            self.guard.acting.store(false, Ordering::SeqCst);
        } else {
            drop(inner);
        }
        self.guard.record(json!({
            "event": "lease_released", "lease": self.id, "actions": self.actions,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditLog;
    use crate::capture::{CaptureBackendKind, CaptureTarget};
    use crate::geometry::{LRect, Point};
    use crate::input::{InputBackendKind, MouseButton};
    use std::sync::atomic::AtomicUsize;

    /// Backend de entrada falso: cuenta acciones y puede tardar.
    struct FakeSynth {
        calls: Arc<AtomicUsize>,
        typed: Arc<Mutex<String>>,
        delay: Duration,
        fail: Option<InputError>,
    }

    impl FakeSynth {
        fn new() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                typed: Arc::new(Mutex::new(String::new())),
                delay: Duration::ZERO,
                fail: None,
            }
        }
    }

    impl InputSynth for FakeSynth {
        fn kind(&self) -> InputBackendKind {
            InputBackendKind::Ydotool
        }
        fn perform(&mut self, action: &InputAction) -> Result<(), InputError> {
            std::thread::sleep(self.delay);
            if let Some(e) = &self.fail {
                return Err(e.clone());
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let InputAction::TypeText(t) = action {
                self.typed.lock().unwrap().push_str(t);
            }
            Ok(())
        }
    }

    struct FakeCapture;

    impl ScreenCapture for FakeCapture {
        fn kind(&self) -> CaptureBackendKind {
            CaptureBackendKind::Spectacle
        }
        fn capture(&mut self, _t: &CaptureTarget) -> Result<CapturedFrame, CaptureError> {
            Ok(CapturedFrame {
                rgba: vec![0; 16],
                width: 2,
                height: 2,
                logical: LRect::new(0.0, 0.0, 2.0, 2.0),
                scale: 1.0,
                taken_at: Instant::now(),
                backend: CaptureBackendKind::Spectacle,
            })
        }
    }

    fn cfg() -> GuardConfig {
        GuardConfig {
            enabled: true,
            max_actions_per_sec: 10,
            lease_duration: Duration::from_secs(120),
            excluded_apps: vec!["banco-estado".into()],
        }
    }

    fn click() -> InputAction {
        InputAction::Click {
            at: Point::new(10.0, 10.0),
            button: MouseButton::Left,
            count: 1,
        }
    }

    #[test]
    fn disabled_guard_refuses_leases() {
        let guard = AutomationGuard::new(GuardConfig {
            enabled: false,
            ..cfg()
        });
        assert_eq!(
            guard.acquire("x", "firefox").err(),
            Some(GuardError::Disabled)
        );
        assert!(!guard.is_acting());
    }

    #[test]
    fn default_core_config_yields_disabled_guard() {
        let guard =
            AutomationGuard::new(GuardConfig::from(&jarvis_core::AutomationConfig::default()));
        assert_eq!(guard.config().max_actions_per_sec, 10);
        assert_eq!(guard.config().lease_duration, Duration::from_secs(120));
        assert_eq!(
            guard.acquire("x", "firefox").err(),
            Some(GuardError::Disabled)
        );
    }

    #[test]
    fn lease_performs_and_drives_acting_indicator() {
        let guard = AutomationGuard::new(cfg());
        let mut synth = FakeSynth::new();
        let calls = Arc::clone(&synth.calls);
        {
            let mut lease = guard.acquire("buscar", "firefox").unwrap();
            assert!(guard.is_acting());
            lease.perform(&mut synth, click()).unwrap();
            lease
                .perform(&mut synth, InputAction::TypeText("hola".into()))
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        }
        assert!(!guard.is_acting());
    }

    #[test]
    fn only_one_lease_at_a_time() {
        let guard = AutomationGuard::new(cfg());
        let lease = guard.acquire("a", "firefox").unwrap();
        assert_eq!(
            guard.acquire("b", "firefox").err(),
            Some(GuardError::LeaseActive)
        );
        drop(lease);
        assert!(guard.acquire("b", "firefox").is_ok());
    }

    #[test]
    fn lease_expires() {
        let guard = AutomationGuard::new(GuardConfig {
            lease_duration: Duration::from_millis(30),
            ..cfg()
        });
        let mut synth = FakeSynth::new();
        let mut lease = guard.acquire("a", "firefox").unwrap();
        lease.perform(&mut synth, click()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            lease.perform(&mut synth, click()),
            Err(GuardError::LeaseExpired)
        );
        assert_eq!(
            lease
                .capture(&mut FakeCapture, &CaptureTarget::ActiveOutput)
                .err(),
            Some(GuardError::LeaseExpired)
        );
    }

    #[test]
    fn token_bucket_limits_bursts_and_refills() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::new(10, t0);
        for _ in 0..10 {
            assert!(bucket.try_take(t0));
        }
        assert!(!bucket.try_take(t0));
        // 100 ms a 10/s recargan exactamente un token.
        assert!(bucket.try_take(t0 + Duration::from_millis(100)));
        assert!(!bucket.try_take(t0 + Duration::from_millis(100)));
        // Nunca acumula más que la ráfaga.
        let later = t0 + Duration::from_secs(60);
        for _ in 0..10 {
            assert!(bucket.try_take(later));
        }
        assert!(!bucket.try_take(later));
    }

    #[test]
    fn guard_rate_limits_a_runaway_planner() {
        let guard = AutomationGuard::new(GuardConfig {
            max_actions_per_sec: 3,
            ..cfg()
        });
        let mut synth = FakeSynth::new();
        let mut lease = guard.acquire("a", "firefox").unwrap();
        let results: Vec<_> = (0..5).map(|_| lease.perform(&mut synth, click())).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 3);
        assert_eq!(results[4], Err(GuardError::RateLimited));
        assert_eq!(synth.calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn kill_from_another_thread_aborts_perform() {
        let guard = Arc::new(AutomationGuard::new(cfg()));
        let mut synth = FakeSynth::new();
        let mut lease = guard.acquire("a", "firefox").unwrap();
        lease.perform(&mut synth, click()).unwrap();
        let g = Arc::clone(&guard);
        std::thread::spawn(move || assert!(g.kill(KillReason::Voice)))
            .join()
            .unwrap();
        assert!(guard.is_killed());
        assert!(!guard.is_acting());
        assert_eq!(
            lease.perform(&mut synth, click()),
            Err(GuardError::Killed(KillReason::Voice))
        );
        assert_eq!(
            lease
                .capture(&mut FakeCapture, &CaptureTarget::ActiveOutput)
                .err(),
            Some(GuardError::Killed(KillReason::Voice))
        );
        // Idempotente.
        guard.kill(KillReason::Voice);
        drop(lease);
        // Un lease nuevo limpia el estado de kill.
        let _lease = guard.acquire("b", "firefox").unwrap();
        assert!(!guard.is_killed());
    }

    #[test]
    fn kill_cuts_long_typing_within_100ms() {
        let guard = Arc::new(AutomationGuard::new(cfg()));
        let mut synth = FakeSynth::new();
        synth.delay = Duration::from_millis(20);
        let typed = Arc::clone(&synth.typed);
        let g = Arc::clone(&guard);
        let killer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            g.kill(KillReason::Voice);
            Instant::now()
        });
        let mut lease = guard.acquire("a", "kwrite").unwrap();
        let text = "x".repeat(TYPE_CHUNK_CHARS * 40);
        let result = lease.perform(&mut synth, InputAction::TypeText(text.clone()));
        let returned = Instant::now();
        let killed_at = killer.join().unwrap();
        assert_eq!(result, Err(GuardError::Killed(KillReason::Voice)));
        assert!(returned.saturating_duration_since(killed_at) <= Duration::from_millis(100));
        assert!(typed.lock().unwrap().len() < text.len());
    }

    #[test]
    fn excluded_apps_are_refused_for_input_and_capture() {
        let guard = AutomationGuard::new(cfg());
        for app in [
            "org.keepassxc.KeePassXC",
            "polkit-kde-authentication-agent-1",
            "Banco-Estado App",
        ] {
            assert_eq!(
                guard.acquire("x", app).err(),
                Some(GuardError::ExcludedWindow(app.to_string())),
                "{app}"
            );
        }
        // La ventana activa pasa a ser excluida en mitad del lease.
        let active = Arc::new(Mutex::new("firefox".to_string()));
        let a = Arc::clone(&active);
        let guard = AutomationGuard::new(cfg())
            .with_active_app(Box::new(move || Some(a.lock().unwrap().clone())));
        let mut lease = guard.acquire("x", "firefox").unwrap();
        let mut synth = FakeSynth::new();
        lease.perform(&mut synth, click()).unwrap();
        *active.lock().unwrap() = "kscreenlocker_greet".into();
        assert_eq!(
            lease.perform(&mut synth, click()),
            Err(GuardError::ExcludedWindow("kscreenlocker_greet".into()))
        );
        assert!(matches!(
            lease.capture(&mut FakeCapture, &CaptureTarget::ActiveOutput),
            Err(GuardError::ExcludedWindow(_))
        ));
    }

    #[test]
    fn lease_refuses_when_target_app_is_no_longer_active() {
        let active = Arc::new(Mutex::new("org.telegram.desktop".to_string()));
        let a = Arc::clone(&active);
        let guard = AutomationGuard::new(cfg())
            .with_active_app(Box::new(move || Some(a.lock().unwrap().clone())));
        let mut synth = FakeSynth::new();
        let mut lease = guard.acquire("enviar", "telegram").unwrap();
        lease.perform(&mut synth, click()).unwrap();
        *active.lock().unwrap() = "konsole".into();
        assert_eq!(
            lease.perform(&mut synth, click()),
            Err(GuardError::TargetNotActive {
                expected: "telegram".into(),
                active: "konsole".into()
            })
        );
    }

    #[test]
    fn permission_denied_kills_the_lease() {
        let guard = AutomationGuard::new(cfg());
        let mut synth = FakeSynth::new();
        synth.fail = Some(InputError::PermissionDenied);
        let mut lease = guard.acquire("x", "firefox").unwrap();
        assert_eq!(
            lease.perform(&mut synth, click()),
            Err(GuardError::Input(InputError::PermissionDenied))
        );
        assert!(guard.is_killed());
        synth.fail = None;
        assert_eq!(
            lease.perform(&mut synth, click()),
            Err(GuardError::Killed(KillReason::Timeout))
        );
    }

    #[test]
    fn capture_goes_through_the_lease() {
        let guard = AutomationGuard::new(cfg());
        let mut lease = guard
            .acquire_with_risk("ver", "firefox", ActionRiskLevel::Low)
            .unwrap();
        assert_eq!(lease.risk(), ActionRiskLevel::Low);
        let frame = lease
            .capture(&mut FakeCapture, &CaptureTarget::ActiveOutput)
            .unwrap();
        assert_eq!((frame.width, frame.height), (2, 2));
    }

    #[test]
    fn audit_trail_covers_lease_lifecycle_without_plain_text() {
        let dir = std::env::temp_dir().join(format!("jarvis-guard-audit-{}", std::process::id()));
        let path = dir.join("automation.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        let guard = AutomationGuard::new(cfg()).with_audit(AuditLog::new(&path, false));
        let mut synth = FakeSynth::new();
        {
            let mut lease = guard
                .acquire_with_risk("enviar", "whatsapp", ActionRiskLevel::High)
                .unwrap();
            lease
                .perform(&mut synth, InputAction::TypeText("Llego a las 8".into()))
                .unwrap();
            guard.kill(KillReason::Voice);
            let _ = lease.perform(&mut synth, click());
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("Llego a las 8"), "{raw}");
        let events: Vec<String> = raw
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["event"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            events,
            [
                "lease_acquired",
                "action",
                "kill",
                "action",
                "lease_released"
            ]
        );
        assert!(raw.contains("\"risk\":\"high\""));
        assert!(raw.contains("automatización detenida (voz)"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
