//! Abstracción de la superficie overlay (ventana flotante transparente).
//!
//! Define el contrato [`OverlaySurface`] y sus backends:
//!
//! - [`WaylandLayerSurface`] (solo Linux): capa overlay real vía
//!   `zwlr_layer_shell_v1` (KWin, Hyprland, Sway…).
//! - [`HeadlessSurface`]: sin ventana; valida frames y regula el ritmo del bucle.
//!
//! [`window_watch`] detecta ventanas a pantalla completa (HU-35), [`tray`]
//! publica el icono de la bandeja del sistema (StatusNotifierItem) y
//! [`notification_watch`] observa las notificaciones del escritorio (HU-72).
//!
//! El backend `winit` (X11, Windows, macOS, GNOME) llega en otra tarea.

use std::fmt;
use std::time::Duration;

pub use jarvis_core::Opacity;

mod headless;
pub mod menu;
pub mod notification_watch;
#[cfg(all(test, target_os = "linux"))]
mod test_bus;
pub mod tray;
#[cfg(target_os = "linux")]
mod wayland_layer;
pub mod window_watch;

pub use headless::{HeadlessHud, HeadlessSurface};
pub use menu::{ContextMenu, MenuItem};
#[cfg(target_os = "linux")]
pub use wayland_layer::{anchor_to_layer, WaylandLayerSurface};

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
    /// Sin ventana: solo IPC (fallback cuando no hay overlay disponible).
    Headless,
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
    InvalidSize {
        width: u32,
        height: u32,
    },
    InvalidFrame {
        expected: usize,
        actual: usize,
    },
    BackendUnavailable(&'static str),
    /// Fallo de comunicación con el compositor (conexión, buffer compartido…).
    Backend(String),
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
            SurfaceError::Backend(detail) => write!(f, "error del backend de superficie: {detail}"),
        }
    }
}

impl std::error::Error for SurfaceError {}

/// Estado de la superficie tras despachar sus eventos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceStatus {
    /// La superficie sigue activa.
    Running,
    /// El compositor cerró la superficie o se perdió la conexión.
    Closed,
}

/// Acción que el usuario pide desde la propia superficie (menú contextual).
///
/// Las acciones que solo afectan a la superficie (volver a la posición
/// inicial) las resuelve el backend; estas las ejecuta la aplicación.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellCommand {
    /// Silenciar o reactivar el micrófono.
    ToggleMute,
    /// Abrir el formulario de configuración.
    Configure,
    /// Abrir el selector de voces.
    ChangeVoice,
    /// Abrir el Panel Rápido (QuickPanel PyQt6) en las coordenadas del clic.
    QuickPanel { x: i32, y: i32 },
    /// Abrir el Panel Rápido sin coordenadas (menú de la bandeja): la
    /// posición la decide el propio panel.
    QuickPanelDefault,
    /// Mostrar u ocultar la esfera (clic en el icono de la bandeja). Si está
    /// oculta por cualquier motivo, la vuelve a mostrar.
    ToggleVisibility,
    /// Interrumpir la locución en curso (HU-56): un clic simple sobre la
    /// esfera la corta en el acto, sin esperar a que Jarvis termine.
    Interrupt,
    /// Abrir el detalle de una sub-sesión (submenú de la bandeja, HU-67).
    OpenSession(u64),
    /// Cancelar una sub-sesión (submenú de la bandeja, HU-67).
    CancelSession(u64),
    /// Cerrar Fokkus-Jarvis.
    Quit,
}

/// Capa del HUD de burbujas de conversación (HU-36).
///
/// Es una segunda superficie, anclada junto a la esfera y totalmente
/// *click-through* (región de entrada vacía). Se presenta solo cuando hay
/// globos que mostrar; sin globos se oculta y no consume frames.
pub trait HudSurface {
    /// Cambia el tamaño de la capa. Los frames siguientes deben medir esto.
    fn set_size(&mut self, size: SurfaceSize) -> Result<(), SurfaceError>;

    /// Tamaño actual de la capa.
    fn size(&self) -> SurfaceSize;

    /// `true` si la capa puede presentar ahora (configurada, visible y sin
    /// frame pendiente). Si es `false`, `present` descarta el frame.
    fn wants_frame(&self) -> bool {
        true
    }

    /// Presenta un frame RGBA premultiplicado del tamaño actual.
    fn present(&mut self, frame: &FrameBuffer<'_>) -> Result<(), SurfaceError>;

    /// Muestra u oculta la capa.
    fn set_visible(&mut self, visible: bool);
}

/// Contrato que debe cumplir cualquier backend de ventana flotante.
pub trait OverlaySurface {
    /// Backend efectivo de la superficie (para el log y `--check`).
    fn backend(&self) -> SurfaceBackend;

    /// Despacha los eventos pendientes esperando como máximo `timeout`.
    ///
    /// Es el único punto de espera del bucle principal.
    fn pump_events(&mut self, timeout: Duration) -> Result<SurfaceStatus, SurfaceError>;

    /// `true` si conviene dibujar un frame nuevo (frame callback del compositor
    /// o primer frame pendiente).
    fn wants_frame(&self) -> bool {
        true
    }

    /// Tamaño actual de la superficie.
    fn size(&self) -> SurfaceSize;

    /// Presenta un frame RGBA premultiplicado.
    fn present(&mut self, frame: &FrameBuffer<'_>) -> Result<(), SurfaceError>;

    /// Muestra u oculta la superficie.
    fn set_visible(&mut self, visible: bool);

    /// Re-ancla la superficie (drag de la esfera, tarea 1.4).
    fn set_anchor(&mut self, anchor: Anchor);

    /// Devuelve la capa a su anclaje y margen originales (botón del panel).
    fn reset_position(&mut self) {}

    /// Siguiente acción pedida por el usuario desde la superficie, si la hay.
    fn poll_command(&mut self) -> Option<ShellCommand> {
        None
    }

    /// Informa si el micrófono está silenciado (etiqueta del menú contextual).
    fn sync_muted(&mut self, _muted: bool) {}

    /// Cambia el tamaño lógico de la superficie (HU-33).
    ///
    /// Los backends que no lo soportan devuelven
    /// [`SurfaceError::BackendUnavailable`]: la app lo registra y aplica el
    /// tamaño en el próximo arranque.
    fn set_size(&mut self, size: SurfaceSize) -> Result<(), SurfaceError> {
        let _ = size;
        Err(SurfaceError::BackendUnavailable("resize"))
    }

    /// Opacidad global con la que se presentan los frames siguientes (HU-34).
    ///
    /// Es solo visual: la región de entrada no cambia.
    fn set_opacity(&mut self, _opacity: Opacity) {}

    /// Nombre del output (monitor) donde está la superficie, si se conoce
    /// (HU-35: la directiva de pantalla completa solo aplica en ese monitor).
    fn current_output(&self) -> Option<String> {
        None
    }

    /// Crea (`true`) o destruye (`false`) la capa del HUD (HU-36). Al
    /// destruirla se liberan sus buffers. Sin soporte no hace nada.
    fn set_hud_enabled(&mut self, _enabled: bool) {}

    /// Capa del HUD, si está creada y el backend la soporta.
    fn hud(&mut self) -> Option<&mut dyn HudSurface> {
        None
    }
}

/// Convierte píxeles RGBA premultiplicados (tiny-skia) a `Argb8888` de
/// `wl_shm`, que en little-endian se guarda en memoria como BGRA.
///
/// Solo intercambia R↔B y conserva el alfa: ambos formatos son
/// premultiplicados, así que no se despremultiplica. Equivale a
/// [`rgba_premul_to_argb8888_with_alpha`] con [`Opacity::OPAQUE`].
pub fn rgba_premul_to_argb8888(src: &[u8], dst: &mut [u8]) -> Result<(), SurfaceError> {
    rgba_premul_to_argb8888_with_alpha(src, dst, Opacity::OPAQUE)
}

/// Igual que [`rgba_premul_to_argb8888`], pero modula los cuatro canales por
/// `opacity` en el mismo recorrido (HU-34).
///
/// Con premultiplicado, escalar los cuatro canales por `α` equivale a componer
/// la capa con opacidad `α`. Usa `(c * a + 127) / 255`, igual que
/// `jarvis_render::apply_opacity`, así que ambas rutas son bit a bit
/// equivalentes. Con [`Opacity::OPAQUE`] no multiplica (atajo de coste cero).
pub fn rgba_premul_to_argb8888_with_alpha(
    src: &[u8],
    dst: &mut [u8],
    opacity: Opacity,
) -> Result<(), SurfaceError> {
    if src.len() != dst.len() || !src.len().is_multiple_of(4) {
        return Err(SurfaceError::InvalidFrame {
            expected: src.len(),
            actual: dst.len(),
        });
    }
    let pixels = src
        .as_chunks::<4>()
        .0
        .iter()
        .zip(dst.as_chunks_mut::<4>().0);
    if opacity.is_opaque() {
        for (&[r, g, b, a], d) in pixels {
            *d = [b, g, r, a];
        }
    } else {
        let alpha = u32::from(opacity.as_u8());
        let scale = |c: u8| ((u32::from(c) * alpha + 127) / 255) as u8;
        for (&[r, g, b, a], d) in pixels {
            *d = [scale(b), scale(g), scale(r), scale(a)];
        }
    }
    Ok(())
}

/// Abre la superficie overlay del sistema.
///
/// En Linux con `WAYLAND_DISPLAY` intenta una capa `zwlr_layer_shell_v1`. Si no
/// es posible devuelve [`SurfaceError::BackendUnavailable`] para que la app
/// decida el fallback (normalmente [`HeadlessSurface`]).
pub fn open_overlay(opts: OverlayOptions) -> Result<Box<dyn OverlaySurface>, SurfaceError> {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err(SurfaceError::BackendUnavailable("WAYLAND_DISPLAY"));
        }
        let surface = WaylandLayerSurface::open(opts)?;
        Ok(Box::new(surface))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = opts;
        Err(SurfaceError::BackendUnavailable("winit"))
    }
}

/// Sonda de disponibilidad del overlay sin mapear ninguna capa.
///
/// Abre la conexión con el compositor, comprueba los globals necesarios y la
/// cierra. Pensada para `--check`.
pub fn probe_overlay() -> Result<SurfaceBackend, SurfaceError> {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err(SurfaceError::BackendUnavailable("WAYLAND_DISPLAY"));
        }
        wayland_layer::probe()?;
        Ok(SurfaceBackend::WaylandLayer)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(SurfaceError::BackendUnavailable("winit"))
    }
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

    #[test]
    fn rgba_to_argb8888_swaps_red_and_blue() {
        let src = [10, 20, 30, 40, 255, 0, 128, 255];
        let mut dst = [0u8; 8];
        rgba_premul_to_argb8888(&src, &mut dst).unwrap();
        // BGRA en memoria: R↔B intercambiados, G y A intactos.
        assert_eq!(dst, [30, 20, 10, 40, 128, 0, 255, 255]);
    }

    #[test]
    fn rgba_to_argb8888_keeps_transparent_pixels() {
        let src = [0u8; 4];
        let mut dst = [7u8; 4];
        rgba_premul_to_argb8888(&src, &mut dst).unwrap();
        assert_eq!(dst, [0, 0, 0, 0]);
    }

    #[test]
    fn rgba_to_argb8888_rejects_mismatched_lengths() {
        let src = [0u8; 8];
        let mut dst = [0u8; 4];
        assert_eq!(
            rgba_premul_to_argb8888(&src, &mut dst),
            Err(SurfaceError::InvalidFrame {
                expected: 8,
                actual: 4,
            })
        );
        let mut odd = [0u8; 6];
        assert!(rgba_premul_to_argb8888(&[0u8; 6], &mut odd).is_err());
    }

    #[test]
    fn with_alpha_opaque_matches_plain_conversion() {
        let src = [10, 20, 30, 40, 255, 0, 128, 255];
        let mut plain = [0u8; 8];
        let mut alpha = [0u8; 8];
        rgba_premul_to_argb8888(&src, &mut plain).unwrap();
        rgba_premul_to_argb8888_with_alpha(&src, &mut alpha, Opacity::OPAQUE).unwrap();
        assert_eq!(plain, alpha);
    }

    #[test]
    fn with_alpha_scales_all_channels() {
        // Blanco opaco a 50 % → (128,128,128,128), como `apply_opacity`.
        let src = [255, 255, 255, 255, 200, 100, 0, 200];
        let mut dst = [0u8; 8];
        rgba_premul_to_argb8888_with_alpha(&src, &mut dst, Opacity::from_pct(50)).unwrap();
        assert_eq!(&dst[..4], &[128, 128, 128, 128]);
        // BGRA: (0,100,200,200) × 128/255.
        assert_eq!(&dst[4..], &[0, 50, 100, 100]);
    }

    #[test]
    fn with_alpha_keeps_premultiplied_invariant() {
        // Generador determinista: r,g,b ≤ a antes y después de modular.
        let mut seed = 0x2545_f491_u32;
        let mut src = vec![0u8; 4 * 10_000];
        for px in src.as_chunks_mut::<4>().0 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let a = (seed >> 24) as u8;
            let bytes = seed.to_le_bytes();
            px[0] = bytes[0].min(a);
            px[1] = bytes[1].min(a);
            px[2] = bytes[2].min(a);
            px[3] = a;
        }
        let mut dst = vec![0u8; src.len()];
        for pct in [20, 33, 50, 70, 99] {
            rgba_premul_to_argb8888_with_alpha(&src, &mut dst, Opacity::from_pct(pct)).unwrap();
            for d in dst.as_chunks::<4>().0 {
                assert!(d[0] <= d[3] && d[1] <= d[3] && d[2] <= d[3]);
            }
        }
    }

    #[test]
    fn with_alpha_rejects_mismatched_lengths() {
        let mut dst = [0u8; 4];
        assert!(
            rgba_premul_to_argb8888_with_alpha(&[0u8; 8], &mut dst, Opacity::from_pct(50)).is_err()
        );
    }
}
