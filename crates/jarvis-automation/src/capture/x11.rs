//! Captura X11 (`GetImage`) para sesiones Xorg.
//!
//! Pendiente de aprobación del crate `x11rb` (análisis de seguridad, mismo
//! criterio que D-38.1): hasta entonces el backend se declara y se sondea,
//! pero informa su motivo de descarte en vez de fingir una captura.

use super::{CaptureBackendKind, CaptureError, CaptureTarget, CapturedFrame, ScreenCapture};

/// Motivo por el que el backend aún no captura.
pub const PENDING: &str = "backend X11 pendiente de aprobar x11rb (análisis de seguridad)";

pub struct X11Capture;

impl ScreenCapture for X11Capture {
    fn kind(&self) -> CaptureBackendKind {
        CaptureBackendKind::X11
    }

    fn capture(&mut self, _target: &CaptureTarget) -> Result<CapturedFrame, CaptureError> {
        Err(CaptureError::Unavailable(PENDING.into()))
    }
}
