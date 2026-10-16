//! Entrada X11 XTest (solo sesiones Xorg).
//!
//! Pendiente de aprobar `x11rb` (análisis de seguridad): el backend se sondea
//! pero informa su motivo en vez de fingir la acción.

use super::{InputAction, InputBackendKind, InputError, InputSynth};

/// Motivo por el que el backend aún no actúa.
pub const PENDING: &str = "backend XTest pendiente de aprobar x11rb (análisis de seguridad)";

pub struct XtestInput;

impl InputSynth for XtestInput {
    fn kind(&self) -> InputBackendKind {
        InputBackendKind::X11Xtest
    }

    fn perform(&mut self, _action: &InputAction) -> Result<(), InputError> {
        Err(InputError::Unavailable(PENDING.into()))
    }
}
