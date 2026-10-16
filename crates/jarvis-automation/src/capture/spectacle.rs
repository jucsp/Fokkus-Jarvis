//! Captura con `spectacle -b -n -o <tmp>` (KDE Plasma).
//!
//! Spectacle es cliente privilegiado de KWin (`org.kde.KWin.ScreenShot2`), así
//! que no pide permiso. Solo sabe escribir a archivo: el PNG va a un
//! temporal en el directorio privado `0700` y se borra en cuanto se carga en
//! memoria (también si la carga falla).

use std::process::Command;
use std::time::Duration;

use super::ScreenCapture;
use super::{crop_to_target, CaptureBackendKind, CaptureError, CaptureTarget, CapturedFrame};
use crate::geometry::DesktopLayout;
use crate::proc::{run_with_timeout, TempFile};

/// Spectacle tarda ~150–300 ms; el margen cubre un arranque en frío.
const TIMEOUT: Duration = Duration::from_secs(5);

pub struct SpectacleCapture {
    layout: DesktopLayout,
}

impl SpectacleCapture {
    pub fn new(layout: DesktopLayout) -> Self {
        Self { layout }
    }

    /// Modo de spectacle para el objetivo pedido.
    fn mode_flag(target: &CaptureTarget) -> &'static str {
        match target {
            CaptureTarget::ActiveOutput => "-m",
            CaptureTarget::ActiveWindow => "-a",
            // Monitor concreto o región: escritorio completo y se recorta.
            CaptureTarget::Output(_) | CaptureTarget::Region(_) => "-f",
        }
    }
}

impl ScreenCapture for SpectacleCapture {
    fn kind(&self) -> CaptureBackendKind {
        CaptureBackendKind::Spectacle
    }

    fn capture(&mut self, target: &CaptureTarget) -> Result<CapturedFrame, CaptureError> {
        let tmp = TempFile::new("png").map_err(|e| CaptureError::Backend(e.to_string()))?;
        let mut cmd = Command::new("spectacle");
        cmd.args(["-b", "-n", Self::mode_flag(target), "-o"])
            .arg(tmp.path());
        let out = run_with_timeout(cmd, TIMEOUT).map_err(|e| {
            if e.contains("tiempo agotado") {
                CaptureError::Timeout
            } else {
                CaptureError::Backend(format!("spectacle: {e}"))
            }
        })?;
        if !out.success {
            return Err(CaptureError::Backend(format!(
                "spectacle terminó con error: {}",
                out.stderr.trim()
            )));
        }
        let data = std::fs::read(tmp.path())
            .map_err(|e| CaptureError::Backend(format!("spectacle no dejó la imagen: {e}")))?;
        drop(tmp); // Borrado inmediato: la captura ya está en memoria.
        let frame = CapturedFrame::from_png(&data, &self.layout, None, self.kind())?;
        crop_to_target(frame, target, &self.layout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::LRect;

    #[test]
    fn mode_flags_match_targets() {
        assert_eq!(
            SpectacleCapture::mode_flag(&CaptureTarget::ActiveOutput),
            "-m"
        );
        assert_eq!(
            SpectacleCapture::mode_flag(&CaptureTarget::ActiveWindow),
            "-a"
        );
        assert_eq!(
            SpectacleCapture::mode_flag(&CaptureTarget::Output("DP-1".into())),
            "-f"
        );
        assert_eq!(
            SpectacleCapture::mode_flag(&CaptureTarget::Region(LRect::new(0.0, 0.0, 1.0, 1.0))),
            "-f"
        );
    }

    /// Captura real del monitor activo (manual: `cargo test -- --ignored`).
    /// La imagen queda solo en memoria y el temporal debe desaparecer.
    #[test]
    #[ignore = "requiere sesión KDE Plasma con spectacle"]
    fn real_capture_leaves_no_temp_file() {
        let started = std::time::Instant::now();
        let frame = SpectacleCapture::new(DesktopLayout::default())
            .capture(&CaptureTarget::ActiveOutput)
            .unwrap();
        eprintln!(
            "spectacle: {}x{} en {} ms",
            frame.width,
            frame.height,
            started.elapsed().as_millis()
        );
        assert!(frame.width > 0 && frame.height > 0);
        assert_eq!(frame.rgba.len(), (frame.width * frame.height * 4) as usize);
        let leftovers: Vec<_> = std::fs::read_dir(crate::proc::private_tmp_dir().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("frame-{}-", std::process::id()))
            })
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
