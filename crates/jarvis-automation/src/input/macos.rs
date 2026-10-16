//! Entrada en macOS con `CGEventPost` (CoreGraphics Event Services). Requiere
//! el permiso TCC de Accesibilidad. Sin implementación nativa todavía.

use super::{InputAction, InputBackendKind, InputError, InputSynth};

/// Motivo de descarte en esta compilación.
pub fn unavailable_reason() -> &'static str {
    if cfg!(target_os = "macos") {
        "pendiente de implementar (CGEventPost + permiso de Accesibilidad)"
    } else {
        "solo macOS"
    }
}

pub struct CgEventInput;

impl InputSynth for CgEventInput {
    fn kind(&self) -> InputBackendKind {
        InputBackendKind::CGEvent
    }

    fn perform(&mut self, _action: &InputAction) -> Result<(), InputError> {
        Err(InputError::Unavailable(unavailable_reason().into()))
    }
}
