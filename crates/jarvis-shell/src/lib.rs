//! Abstracción de la superficie overlay (ventana flotante transparente).
//!
//! En Fase 1 solo se definen el trait y los tipos; los backends reales
//! (`wayland_layer` con `zwlr_layer_shell_v1` y `winit_window`) llegan en las
//! tareas 1.4 y 1.5.

use std::fmt;

/// Tamaño de una superficie en píxeles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceSize {
    pub width: u32,
    pub height: u32,
}

impl SurfaceSize {
    pub fn new(width: u32, height: u32) -> Result<Self, SurfaceError> {
        if width == 0 || height == 0 {
            return Err(SurfaceError::InvalidSize { width, height });
        }
        Ok(Self { width, height })
    }

    /// Número de píxeles de la superficie.
    pub fn pixel_count(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

/// Frame de píxeles RGBA premultiplicado (sin stride/alineación).
#[derive(Debug, Clone, Copy)]
pub struct FrameBuffer<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
}

impl<'a> FrameBuffer<'a> {
    pub fn new(data: &'a [u8], width: u32, height: u32) -> Result<Self, SurfaceError> {
        let expected = width as usize * height as usize * 4;
        if data.len() != expected {
            return Err(SurfaceError::InvalidFrame {
                expected,
                actual: data.len(),
            });
        }
        Ok(Self {
            data,
            width,
            height,
        })
    }
}

/// Esquina de anclaje de la superficie en el escritorio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    TopLeft,
    TopRight,
    BottomLeft,
    #[default]
    BottomRight,
    Center,
}

/// Interactividad del teclado en una capa overlay Wayland.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardInteractivity {
    None,
    Exclusive,
    #[default]
    OnDemand,
}

/// Backend de superficie seleccionado en arranque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceBackend {
    /// Capa overlay real vía `zwlr_layer_shell_v1` (KWin, Hyprland, Sway…).
    WaylandLayer,
    /// Ventana `winit` frameless/transparente (X11, Windows, macOS, GNOME).
    Winit,
}

impl SurfaceBackend {
    /// Selección provisional de backend.
    ///
    /// La sonda real del global `zwlr_layer_shell_v1` se implementa en la
    /// tarea 1.4; de momento se usa una heurística por variable de entorno.
    pub fn detect() -> Self {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            SurfaceBackend::WaylandLayer
        } else {
            SurfaceBackend::Winit
        }
    }
}

/// Opciones de la superficie overlay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayOptions {
    pub anchor: Anchor,
    /// Margen respecto al borde anclado, en píxeles.
    pub margin: u32,
    pub size: SurfaceSize,
    /// `exclusive_zone` para layer-shell (0 = no reserva espacio).
    pub exclusive_zone: i32,
    pub keyboard_interactivity: KeyboardInteractivity,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            anchor: Anchor::default(),
            margin: 12,
            size: SurfaceSize {
                width: 220,
                height: 220,
            },
            exclusive_zone: 0,
            keyboard_interactivity: KeyboardInteractivity::default(),
        }
    }
}

/// Error de superficie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceError {
    InvalidSize { width: u32, height: u32 },
    InvalidFrame { expected: usize, actual: usize },
    BackendUnavailable(&'static str),
}

impl fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SurfaceError::InvalidSize { width, height } => {
                write!(f, "tamaño de superficie inválido: {width}x{height}")
            }
            SurfaceError::InvalidFrame { expected, actual } => {
                write!(
                    f,
                    "frame inválido: se esperaban {expected} bytes, se recibieron {actual}"
                )
            }
            SurfaceError::BackendUnavailable(backend) => {
                write!(f, "backend de superficie no disponible: {backend}")
            }
        }
    }
}

impl std::error::Error for SurfaceError {}

/// Contrato que debe cumplir cualquier backend de ventana flotante.
pub trait OverlaySurface {
    /// Tamaño actual de la superficie.
    fn size(&self) -> SurfaceSize;

    /// Presenta un frame RGBA premultiplicado.
    fn present(&mut self, frame: &FrameBuffer<'_>) -> Result<(), SurfaceError>;

    /// Muestra u oculta la superficie.
    fn set_visible(&mut self, visible: bool);

    /// Re-ancla la superficie (drag de la esfera, tarea 1.4).
    fn set_anchor(&mut self, anchor: Anchor);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_size_validates() {
        assert!(SurfaceSize::new(220, 220).is_ok());
        assert_eq!(
            SurfaceSize::new(0, 10),
            Err(SurfaceError::InvalidSize {
                width: 0,
                height: 10,
            })
        );
    }

    #[test]
    fn frame_buffer_validates_length() {
        let data = [0u8; 220 * 220 * 4];
        assert!(FrameBuffer::new(&data, 220, 220).is_ok());
        assert!(FrameBuffer::new(&data[..100], 220, 220).is_err());
    }

    #[test]
    fn default_overlay_options_match_spec() {
        let opts = OverlayOptions::default();
        assert_eq!(opts.anchor, Anchor::BottomRight);
        assert_eq!(opts.exclusive_zone, 0);
        assert_eq!(opts.size.width, 220);
        assert_eq!(opts.size.height, 220);
        assert_eq!(opts.keyboard_interactivity, KeyboardInteractivity::OnDemand);
    }
}
