//! Entrada en Windows con Win32 `SendInput`. UIPI impide inyectar en procesos
//! elevados y en escritorios seguros (UAC): quedan excluidos por diseño.
//!
//! Pendiente de D-38.3 (crate `windows`, feature
//! `Win32_UI_Input_KeyboardAndMouse`).

use super::{InputAction, InputBackendKind, InputError, InputSynth};

/// Motivo de descarte en esta compilación.
pub fn unavailable_reason() -> &'static str {
    if cfg!(windows) {
        "pendiente D-38.3 (crate windows: Win32_UI_Input_KeyboardAndMouse)"
    } else {
        "solo Windows"
    }
}

pub struct SendInputInput;

impl InputSynth for SendInputInput {
    fn kind(&self) -> InputBackendKind {
        InputBackendKind::WindowsSendInput
    }

    fn perform(&mut self, _action: &InputAction) -> Result<(), InputError> {
        Err(InputError::Unavailable(unavailable_reason().into()))
    }
}
