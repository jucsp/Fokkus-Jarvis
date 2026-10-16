//! Ojos y manos multiplataforma de Fokkus-Jarvis (HU-38).
//!
//! - [`capture`]: captura de pantalla (portal, spectacle, grim, X11, Windows,
//!   macOS) detrás del trait [`ScreenCapture`].
//! - [`input`]: entrada sintética (portal + libei, XTest, ydotool, `SendInput`,
//!   `CGEventPost`) detrás del trait [`InputSynth`].
//! - [`geometry`]: coordenadas lógicas ↔ píxeles, multimonitor y escalas.
//! - [`guard`]: [`AutomationGuard`] y [`Lease`], la única puerta hacia
//!   captura y entrada (kill switch, *lease*, tasa, exclusiones).
//! - [`audit`]: auditoría JSONL.
//! - [`probe`]: sondeo de capacidades para `--check`.
//!
//! La automatización está apagada por defecto (`[automation] enabled`).

pub mod audit;
pub mod capture;
pub mod geometry;
pub mod guard;
pub mod input;
mod png;
mod portal_dbus;
pub mod probe;
mod proc;

pub use audit::{default_audit_path, AuditLog};
pub use capture::{
    probe_capture_backend, CaptureBackendKind, CaptureError, CaptureTarget, CapturedFrame,
    ScreenCapture,
};
pub use geometry::{DesktopLayout, LRect, OutputInfo, Point};
pub use guard::{
    ActionRiskLevel, ActiveAppProvider, AutomationGuard, GuardConfig, GuardError, KillReason,
    Lease, LeaseId, BUILTIN_EXCLUDED_APPS,
};
pub use input::{
    probe_input_backend, InputAction, InputBackendKind, InputError, InputSynth, Key, MouseButton,
};
pub use probe::{probe, probe_with, Capabilities, ProbeOptions};
