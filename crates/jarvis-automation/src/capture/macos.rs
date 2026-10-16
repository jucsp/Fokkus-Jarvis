//! Captura en macOS: `ScreenCaptureKit` (12.3+) con respaldo
//! `CGWindowListCreateImage`. Requiere el permiso TCC de Grabación de
//! pantalla. Sin implementación nativa todavía: informa el motivo.

use super::{CaptureBackendKind, CaptureError, CaptureTarget, CapturedFrame, ScreenCapture};

/// Motivo de descarte en esta compilación.
pub fn unavailable_reason() -> &'static str {
    if cfg!(target_os = "macos") {
        "pendiente de implementar (ScreenCaptureKit + permiso TCC)"
    } else {
        "solo macOS"
    }
}

pub struct ScreenCaptureKitCapture;

impl ScreenCapture for ScreenCaptureKitCapture {
    fn kind(&self) -> CaptureBackendKind {
        CaptureBackendKind::ScreenCaptureKit
    }

    fn capture(&mut self, _target: &CaptureTarget) -> Result<CapturedFrame, CaptureError> {
        Err(CaptureError::Unavailable(unavailable_reason().into()))
    }
}
