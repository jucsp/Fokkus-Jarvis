//! Entrada por XDG Portal `RemoteDesktop` + `libei` (preferente en Wayland:
//! KDE 6.1+, GNOME). El usuario ve y acepta el diálogo del portal y puede
//! revocarlo; el *restore token* irá al llavero (CA-38.4).
//!
//! Pendiente de D-38.1 (crates `ashpd` + `reis`, análisis de seguridad). El
//! sondeo sí consulta el portal para que `--check` diga si el compositor lo
//! soporta; la acción informa el motivo en vez de fingir que actuó.

use super::{InputAction, InputBackendKind, InputError, InputSynth};

/// Motivo por el que el backend aún no actúa.
pub const PENDING: &str = "falta el cliente libei (pendiente D-38.1: ashpd + reis)";

pub struct LibeiInput;

impl InputSynth for LibeiInput {
    fn kind(&self) -> InputBackendKind {
        InputBackendKind::PortalLibei
    }

    fn perform(&mut self, _action: &InputAction) -> Result<(), InputError> {
        Err(InputError::Unavailable(PENDING.into()))
    }
}
