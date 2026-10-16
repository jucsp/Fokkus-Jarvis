//! Captura en Windows: `Windows.Graphics.Capture` con respaldo GDI `BitBlt`.
//!
//! Pendiente de D-38.3 (crate `windows` con features mínimas). El backend
//! existe para que la selección y `--check` sean idénticos en todas las
//! plataformas; mientras tanto informa el motivo.

use super::{CaptureBackendKind, CaptureError, CaptureTarget, CapturedFrame, ScreenCapture};

/// Motivo de descarte en esta compilación.
pub fn unavailable_reason() -> &'static str {
    if cfg!(windows) {
        "pendiente D-38.3 (crate windows: Graphics_Capture / GDI)"
    } else {
        "solo Windows"
    }
}

pub struct WindowsCapture {
    kind: CaptureBackendKind,
}

impl WindowsCapture {
    pub fn new(kind: CaptureBackendKind) -> Self {
        Self { kind }
    }
}

impl ScreenCapture for WindowsCapture {
    fn kind(&self) -> CaptureBackendKind {
        self.kind
    }

    fn capture(&mut self, _target: &CaptureTarget) -> Result<CapturedFrame, CaptureError> {
        Err(CaptureError::Unavailable(unavailable_reason().into()))
    }
}
