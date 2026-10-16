//! Superficie sin ventana: valida los frames y regula el ritmo del bucle.

use std::collections::VecDeque;
use std::time::Duration;

use crate::{
    Anchor, FrameBuffer, HudSurface, Opacity, OverlaySurface, ShellCommand, SurfaceBackend,
    SurfaceError, SurfaceSize, SurfaceStatus,
};

/// Capa de HUD simulada: valida los frames y cuenta los presentados (HU-36).
#[derive(Debug, Clone)]
pub struct HeadlessHud {
    size: SurfaceSize,
    visible: bool,
    presented: u64,
}

impl HeadlessHud {
    fn new() -> Self {
        Self {
            size: SurfaceSize {
                width: 1,
                height: 1,
            },
            visible: false,
            presented: 0,
        }
    }

    /// Frames del HUD presentados correctamente.
    pub fn presented_frames(&self) -> u64 {
        self.presented
    }

    /// Si la capa está visible.
    pub fn is_visible(&self) -> bool {
        self.visible
    }
}

impl HudSurface for HeadlessHud {
    fn set_size(&mut self, size: SurfaceSize) -> Result<(), SurfaceError> {
        self.size = size;
        Ok(())
    }

    fn size(&self) -> SurfaceSize {
        self.size
    }

    fn present(&mut self, frame: &FrameBuffer<'_>) -> Result<(), SurfaceError> {
        let expected = self.size.pixel_count() * 4;
        if frame.width != self.size.width
            || frame.height != self.size.height
            || frame.data.len() != expected
        {
            return Err(SurfaceError::InvalidFrame {
                expected,
                actual: frame.data.len(),
            });
        }
        self.presented += 1;
        Ok(())
    }

    fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }
}

/// Superficie que valida los frames sin abrir una ventana real.
///
/// Se usa como fallback (modo solo IPC) y en los tests del bucle principal.
#[derive(Debug, Clone)]
pub struct HeadlessSurface {
    size: SurfaceSize,
    visible: bool,
    anchor: Anchor,
    presented: u64,
    commands: VecDeque<ShellCommand>,
    muted: bool,
    opacity: Opacity,
    hud: Option<HeadlessHud>,
    /// Frames de HUD presentados en capas ya destruidas (para los tests).
    hud_presented_total: u64,
}

impl HeadlessSurface {
    pub fn new(size: SurfaceSize) -> Self {
        Self {
            size,
            visible: false,
            anchor: Anchor::default(),
            presented: 0,
            commands: VecDeque::new(),
            muted: false,
            opacity: Opacity::OPAQUE,
            hud: None,
            hud_presented_total: 0,
        }
    }

    /// Capa de HUD simulada, si está creada.
    pub fn hud_layer(&self) -> Option<&HeadlessHud> {
        self.hud.as_ref()
    }

    /// Frames de HUD presentados en total (incluye capas ya destruidas).
    pub fn hud_presented_frames(&self) -> u64 {
        self.hud_presented_total + self.hud.as_ref().map_or(0, |h| h.presented)
    }

    /// Encola una acción como si el usuario la eligiera en el menú (tests).
    pub fn push_command(&mut self, command: ShellCommand) {
        self.commands.push_back(command);
    }

    /// Último estado de silencio informado con [`OverlaySurface::sync_muted`].
    pub fn is_muted(&self) -> bool {
        self.muted
    }

    /// Número de frames presentados correctamente.
    pub fn presented_frames(&self) -> u64 {
        self.presented
    }

    /// Si la superficie está marcada como visible.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Anclaje actual.
    pub fn anchor(&self) -> Anchor {
        self.anchor
    }

    /// Última opacidad informada con [`OverlaySurface::set_opacity`].
    pub fn opacity(&self) -> Opacity {
        self.opacity
    }
}

impl OverlaySurface for HeadlessSurface {
    fn backend(&self) -> SurfaceBackend {
        SurfaceBackend::Headless
    }

    /// Sin compositor no hay eventos: duerme hasta `timeout` para no consumir
    /// un núcleo entero.
    fn pump_events(&mut self, timeout: Duration) -> Result<SurfaceStatus, SurfaceError> {
        if !timeout.is_zero() {
            std::thread::sleep(timeout);
        }
        Ok(SurfaceStatus::Running)
    }

    fn size(&self) -> SurfaceSize {
        self.size
    }

    fn present(&mut self, frame: &FrameBuffer<'_>) -> Result<(), SurfaceError> {
        let expected = self.size.pixel_count() * 4;
        if frame.data.len() != expected {
            return Err(SurfaceError::InvalidFrame {
                expected,
                actual: frame.data.len(),
            });
        }
        self.presented += 1;
        Ok(())
    }

    fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    fn set_anchor(&mut self, anchor: Anchor) {
        self.anchor = anchor;
    }

    fn poll_command(&mut self) -> Option<ShellCommand> {
        self.commands.pop_front()
    }

    fn sync_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    /// Sin compositor el cambio es inmediato: los frames siguientes se validan
    /// contra el tamaño nuevo.
    fn set_size(&mut self, size: SurfaceSize) -> Result<(), SurfaceError> {
        self.size = size;
        Ok(())
    }

    fn set_opacity(&mut self, opacity: Opacity) {
        self.opacity = opacity;
    }

    fn set_hud_enabled(&mut self, enabled: bool) {
        match (enabled, self.hud.is_some()) {
            (true, false) => self.hud = Some(HeadlessHud::new()),
            (false, true) => {
                if let Some(hud) = self.hud.take() {
                    self.hud_presented_total += hud.presented;
                }
            }
            _ => {}
        }
    }

    fn hud(&mut self) -> Option<&mut dyn HudSurface> {
        self.hud.as_mut().map(|hud| hud as &mut dyn HudSurface)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_validates_frame_length() {
        let size = SurfaceSize::new(4, 4).unwrap();
        let mut surface = HeadlessSurface::new(size);
        let good = [0u8; 4 * 4 * 4];
        let frame = FrameBuffer::new(&good, 4, 4).unwrap();
        assert!(surface.present(&frame).is_ok());

        let small = [0u8; 2 * 2 * 4];
        let frame = FrameBuffer::new(&small, 2, 2).unwrap();
        assert_eq!(
            surface.present(&frame),
            Err(SurfaceError::InvalidFrame {
                expected: 64,
                actual: 16,
            })
        );
        assert_eq!(surface.presented_frames(), 1);
    }

    #[test]
    fn headless_set_size_revalidates_frames() {
        let mut surface = HeadlessSurface::new(SurfaceSize::new(4, 4).unwrap());
        let bigger = SurfaceSize::new(8, 8).unwrap();
        assert_eq!(surface.set_size(bigger), Ok(()));
        assert_eq!(surface.size(), bigger);

        let old = [0u8; 4 * 4 * 4];
        assert!(surface
            .present(&FrameBuffer::new(&old, 4, 4).unwrap())
            .is_err());
        let new = [0u8; 8 * 8 * 4];
        assert!(surface
            .present(&FrameBuffer::new(&new, 8, 8).unwrap())
            .is_ok());
    }

    #[test]
    fn headless_remembers_opacity() {
        let mut surface = HeadlessSurface::new(SurfaceSize::new(2, 2).unwrap());
        assert_eq!(surface.opacity(), Opacity::OPAQUE);
        surface.set_opacity(Opacity::from_pct(40));
        assert_eq!(surface.opacity(), Opacity::from_pct(40));
    }

    #[test]
    fn headless_hud_is_created_validated_and_destroyed() {
        let mut surface = HeadlessSurface::new(SurfaceSize::new(2, 2).unwrap());
        assert!(surface.hud().is_none(), "sin habilitar no hay capa");
        surface.set_hud_enabled(true);
        let hud = surface.hud().expect("capa creada");
        hud.set_size(SurfaceSize::new(3, 2).unwrap()).unwrap();
        hud.set_visible(true);
        let data = [0u8; 3 * 2 * 4];
        assert!(hud.present(&FrameBuffer::new(&data, 3, 2).unwrap()).is_ok());
        let wrong = [0u8; 2 * 2 * 4];
        assert!(hud
            .present(&FrameBuffer::new(&wrong, 2, 2).unwrap())
            .is_err());
        assert!(surface.hud_layer().unwrap().is_visible());
        assert_eq!(surface.hud_presented_frames(), 1);

        surface.set_hud_enabled(false);
        assert!(surface.hud().is_none());
        assert_eq!(surface.hud_presented_frames(), 1);
    }

    #[test]
    fn headless_always_wants_frames_and_keeps_running() {
        let mut surface = HeadlessSurface::new(SurfaceSize::new(2, 2).unwrap());
        assert!(surface.wants_frame());
        assert_eq!(surface.backend(), SurfaceBackend::Headless);
        assert_eq!(
            surface.pump_events(Duration::ZERO),
            Ok(SurfaceStatus::Running)
        );
    }
}
