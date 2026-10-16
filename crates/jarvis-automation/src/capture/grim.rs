//! Captura con `grim` (compositores wlroots: Sway, Hyprland, river…).
//!
//! `grim` usa `zwlr_screencopy_manager_v1`, que KWin y Mutter no implementan.
//! Escribe el PNG por stdout (`-`): no toca el disco.

use std::process::Command;
use std::time::Duration;

use super::{CaptureBackendKind, CaptureError, CaptureTarget, CapturedFrame, ScreenCapture};
use crate::geometry::DesktopLayout;
use crate::proc::run_with_timeout;

const TIMEOUT: Duration = Duration::from_secs(5);

pub struct GrimCapture {
    layout: DesktopLayout,
}

impl GrimCapture {
    pub fn new(layout: DesktopLayout) -> Self {
        Self { layout }
    }

    /// Argumentos de `grim` para el objetivo (sin el `-` final).
    fn args(&self, target: &CaptureTarget) -> Result<Vec<String>, CaptureError> {
        Ok(match target {
            CaptureTarget::Output(name) => vec!["-o".into(), name.clone()],
            CaptureTarget::Region(r) => vec![
                "-g".into(),
                format!(
                    "{},{} {}x{}",
                    r.x.round(),
                    r.y.round(),
                    r.w.round(),
                    r.h.round()
                ),
            ],
            // Sin protocolo de foco en wlroots: el escritorio completo.
            CaptureTarget::ActiveOutput => Vec::new(),
            CaptureTarget::ActiveWindow => {
                return Err(CaptureError::UnsupportedTarget(
                    "grim no conoce la ventana activa; usa Region con su geometría".into(),
                ))
            }
        })
    }
}

impl ScreenCapture for GrimCapture {
    fn kind(&self) -> CaptureBackendKind {
        CaptureBackendKind::Grim
    }

    fn capture(&mut self, target: &CaptureTarget) -> Result<CapturedFrame, CaptureError> {
        let mut cmd = Command::new("grim");
        cmd.args(self.args(target)?).arg("-");
        let out = run_with_timeout(cmd, TIMEOUT).map_err(|e| {
            if e.contains("tiempo agotado") {
                CaptureError::Timeout
            } else {
                CaptureError::Backend(format!("grim: {e}"))
            }
        })?;
        if !out.success {
            return Err(CaptureError::Backend(format!(
                "grim terminó con error: {}",
                out.stderr.trim()
            )));
        }
        let hint = match target {
            CaptureTarget::Region(r) => Some(*r),
            CaptureTarget::Output(name) => self.layout.output_named(name).map(|o| o.logical),
            _ => None,
        };
        CapturedFrame::from_png(&out.stdout, &self.layout, hint, self.kind())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::LRect;

    #[test]
    fn region_args_use_grim_geometry_syntax() {
        let g = GrimCapture::new(DesktopLayout::default());
        assert_eq!(
            g.args(&CaptureTarget::Region(LRect::new(
                -10.0, 20.4, 300.0, 200.0
            )))
            .unwrap(),
            vec!["-g".to_string(), "-10,20 300x200".to_string()]
        );
        assert_eq!(
            g.args(&CaptureTarget::Output("DP-1".into())).unwrap(),
            vec!["-o".to_string(), "DP-1".to_string()]
        );
        assert!(g.args(&CaptureTarget::ActiveWindow).is_err());
    }
}
