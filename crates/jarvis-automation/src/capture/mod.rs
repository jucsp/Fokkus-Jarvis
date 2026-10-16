//! Ojos: captura de pantalla multiplataforma (patrón Strategy).
//!
//! [`ScreenCapture`] es el contrato; los backends concretos (portal,
//! spectacle, grim, X11, Windows, macOS) se eligen en tiempo de ejecución con
//! [`probe_capture_backend`]. Nadie fuera de este módulo conoce un backend
//! concreto. Las capturas viven en memoria: si un backend necesita archivo, lo
//! escribe en un directorio privado y lo borra al cargarlo.

mod grim;
mod macos;
mod portal;
mod spectacle;
mod windows;
mod x11;

use std::fmt;
use std::time::Instant;

use crate::geometry::{DesktopLayout, LRect};
use crate::png;

pub use grim::GrimCapture;
pub use macos::ScreenCaptureKitCapture;
pub use portal::PortalCapture;
pub use spectacle::SpectacleCapture;
pub use windows::WindowsCapture;
pub use x11::X11Capture;

/// Motivo de descarte del backend X11 mientras no se apruebe `x11rb`.
pub(crate) fn x11_pending() -> &'static str {
    x11::PENDING
}

/// Motivo de descarte de los backends de Windows en esta compilación.
pub(crate) fn windows_reason() -> &'static str {
    windows::unavailable_reason()
}

/// Motivo de descarte del backend de macOS en esta compilación.
pub(crate) fn macos_reason() -> &'static str {
    macos::unavailable_reason()
}

/// Qué capturar.
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureTarget {
    /// El monitor activo (el del foco o el del puntero, según el backend).
    ActiveOutput,
    /// Un monitor por nombre de conector.
    Output(String),
    /// La ventana activa.
    ActiveWindow,
    /// Una zona lógica del escritorio.
    Region(LRect),
}

/// Backends de captura conocidos.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureBackendKind {
    /// XDG Desktop Portal `org.freedesktop.portal.Screenshot`.
    Portal,
    /// `spectacle -b -n` (cliente privilegiado de KWin).
    Spectacle,
    /// `grim` (`wlr-screencopy`, compositores wlroots).
    Grim,
    /// X11 `GetImage`.
    X11,
    /// `Windows.Graphics.Capture`.
    WindowsGraphicsCapture,
    /// GDI `BitBlt` (respaldo en Windows).
    Gdi,
    /// `ScreenCaptureKit` (macOS 12.3+).
    ScreenCaptureKit,
}

impl CaptureBackendKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Portal => "portal Screenshot",
            Self::Spectacle => "spectacle",
            Self::Grim => "grim",
            Self::X11 => "X11 GetImage",
            Self::WindowsGraphicsCapture => "Windows.Graphics.Capture",
            Self::Gdi => "GDI BitBlt",
            Self::ScreenCaptureKit => "ScreenCaptureKit",
        }
    }
}

impl fmt::Display for CaptureBackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Error de captura.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// El backend no existe en esta plataforma/sesión (con motivo).
    Unavailable(String),
    /// El usuario o el sistema denegó el permiso.
    PermissionDenied,
    /// El usuario canceló el diálogo.
    Cancelled,
    /// El backend no respondió a tiempo.
    Timeout,
    /// El backend no soporta ese objetivo.
    UnsupportedTarget(String),
    /// Imagen ilegible.
    Decode(String),
    /// Fallo del backend (proceso, D-Bus, E/S).
    Backend(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(why) => write!(f, "captura no disponible: {why}"),
            Self::PermissionDenied => f.write_str("permiso de captura denegado"),
            Self::Cancelled => f.write_str("captura cancelada por el usuario"),
            Self::Timeout => f.write_str("la captura agotó el tiempo"),
            Self::UnsupportedTarget(why) => write!(f, "objetivo de captura no soportado: {why}"),
            Self::Decode(why) => write!(f, "captura ilegible: {why}"),
            Self::Backend(why) => write!(f, "fallo del backend de captura: {why}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Una captura en memoria, RGBA8 sRGB.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub rgba: Vec<u8>,
    /// Ancho en píxeles (físicos salvo tras [`downscaled`](Self::downscaled)).
    pub width: u32,
    pub height: u32,
    /// Zona capturada en coordenadas lógicas.
    pub logical: LRect,
    /// Escala del output de origen.
    pub scale: f64,
    pub taken_at: Instant,
    pub backend: CaptureBackendKind,
}

impl CapturedFrame {
    /// Construye un frame desde un PNG; la zona lógica se resuelve con
    /// [`resolve_logical`].
    pub fn from_png(
        data: &[u8],
        layout: &DesktopLayout,
        hint: Option<LRect>,
        backend: CaptureBackendKind,
    ) -> Result<Self, CaptureError> {
        let img = png::decode_rgba(data).map_err(CaptureError::Decode)?;
        let (logical, scale) = resolve_logical((img.width, img.height), layout, hint);
        Ok(Self {
            rgba: img.rgba,
            width: img.width,
            height: img.height,
            logical,
            scale,
            taken_at: Instant::now(),
            backend,
        })
    }

    /// Reescala para modelos de visión: el lado mayor queda ≤ `max_edge`
    /// (p. ej. 1568) conservando la proporción. Filtro de caja (promedio de
    /// área): el texto pequeño se degrada menos que con vecino más cercano.
    pub fn downscaled(&self, max_edge: u32) -> CapturedFrame {
        let longest = self.width.max(self.height);
        if max_edge == 0 || longest <= max_edge {
            return self.clone();
        }
        let f = f64::from(max_edge) / f64::from(longest);
        let dw = ((f64::from(self.width) * f).round() as u32).max(1);
        let dh = ((f64::from(self.height) * f).round() as u32).max(1);
        let (sw, sh) = (self.width as usize, self.height as usize);
        let mut out = vec![0u8; dw as usize * dh as usize * 4];
        for dy in 0..dh as usize {
            let y0 = dy * sh / dh as usize;
            let y1 = (((dy + 1) * sh).div_ceil(dh as usize)).clamp(y0 + 1, sh);
            for dx in 0..dw as usize {
                let x0 = dx * sw / dw as usize;
                let x1 = (((dx + 1) * sw).div_ceil(dw as usize)).clamp(x0 + 1, sw);
                let mut acc = [0u64; 4];
                for y in y0..y1 {
                    let row = &self.rgba[(y * sw + x0) * 4..(y * sw + x1) * 4];
                    for px in row.as_chunks::<4>().0 {
                        for (sum, v) in acc.iter_mut().zip(px) {
                            *sum += u64::from(*v);
                        }
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as u64;
                let o = (dy * dw as usize + dx) * 4;
                for c in 0..4 {
                    out[o + c] = ((acc[c] + n / 2) / n) as u8;
                }
            }
        }
        CapturedFrame {
            rgba: out,
            width: dw,
            height: dh,
            logical: self.logical,
            scale: self.scale,
            taken_at: self.taken_at,
            backend: self.backend,
        }
    }

    /// Codifica la captura como PNG (para modelos de visión).
    pub fn encode_png(&self) -> Vec<u8> {
        png::encode_rgba(self.width, self.height, &self.rgba)
    }

    /// Rectángulo de píxeles `(x0, y0, x1, y1)` que cubre una zona lógica,
    /// recortado a la captura. `None` si no se tocan.
    fn pixel_rect(&self, region: &LRect) -> Option<(usize, usize, usize, usize)> {
        let r = region.intersection(&self.logical)?;
        let sx = f64::from(self.width) / self.logical.w;
        let sy = f64::from(self.height) / self.logical.h;
        let x0 = ((r.x - self.logical.x) * sx).floor().max(0.0) as usize;
        let y0 = ((r.y - self.logical.y) * sy).floor().max(0.0) as usize;
        let x1 = (((r.right() - self.logical.x) * sx).ceil() as usize).min(self.width as usize);
        let y1 = (((r.bottom() - self.logical.y) * sy).ceil() as usize).min(self.height as usize);
        (x1 > x0 && y1 > y0).then_some((x0, y0, x1, y1))
    }

    /// Tapa regiones sensibles (campos de contraseña, notificaciones) con un
    /// gris uniforme antes de que la captura salga del proceso. Uniforme y no
    /// un desenfoque: un desenfoque leve puede revertirse.
    pub fn redact(&mut self, regions: &[LRect]) {
        const FILL: [u8; 4] = [128, 128, 128, 255];
        let w = self.width as usize;
        for region in regions {
            let Some((x0, y0, x1, y1)) = self.pixel_rect(region) else {
                continue;
            };
            for y in y0..y1 {
                for px in self.rgba[(y * w + x0) * 4..(y * w + x1) * 4]
                    .as_chunks_mut::<4>()
                    .0
                {
                    *px = FILL;
                }
            }
        }
    }

    /// Recorta a una zona lógica (para backends que solo capturan el
    /// escritorio completo).
    pub fn cropped(&self, region: &LRect) -> Option<CapturedFrame> {
        let (x0, y0, x1, y1) = self.pixel_rect(region)?;
        let w = self.width as usize;
        let mut rgba = Vec::with_capacity((x1 - x0) * (y1 - y0) * 4);
        for y in y0..y1 {
            rgba.extend_from_slice(&self.rgba[(y * w + x0) * 4..(y * w + x1) * 4]);
        }
        let sx = self.logical.w / f64::from(self.width);
        let sy = self.logical.h / f64::from(self.height);
        Some(CapturedFrame {
            rgba,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
            logical: LRect::new(
                self.logical.x + x0 as f64 * sx,
                self.logical.y + y0 as f64 * sy,
                (x1 - x0) as f64 * sx,
                (y1 - y0) as f64 * sy,
            ),
            scale: self.scale,
            taken_at: self.taken_at,
            backend: self.backend,
        })
    }
}

/// Zona lógica y escala de una imagen de `size` píxeles:
///
/// 1. `hint` explícito (el backend sabe qué zona pidió);
/// 2. el único monitor cuyo tamaño físico coincide;
/// 3. el escritorio completo si el layout lo cubre;
/// 4. sin layout: `(0, 0, ancho, alto)` a escala 1.
pub fn resolve_logical(
    size: (u32, u32),
    layout: &DesktopLayout,
    hint: Option<LRect>,
) -> (LRect, f64) {
    let (w, h) = (f64::from(size.0), f64::from(size.1));
    if let Some(r) = hint {
        return (r, if r.w > 0.0 { w / r.w } else { 1.0 });
    }
    if let Some(out) = layout.output_with_physical_size(size) {
        return (out.logical, out.scale);
    }
    if let Some(b) = layout.bounds() {
        return (b, if b.w > 0.0 { w / b.w } else { 1.0 });
    }
    (LRect::new(0.0, 0.0, w, h), 1.0)
}

/// Aplica `target` a una captura del escritorio completo (recorte por
/// monitor o región).
pub(crate) fn crop_to_target(
    frame: CapturedFrame,
    target: &CaptureTarget,
    layout: &DesktopLayout,
) -> Result<CapturedFrame, CaptureError> {
    let zone = match target {
        CaptureTarget::Region(r) => *r,
        CaptureTarget::Output(name) => {
            layout
                .output_named(name)
                .ok_or_else(|| {
                    CaptureError::UnsupportedTarget(format!("monitor desconocido: {name}"))
                })?
                .logical
        }
        CaptureTarget::ActiveOutput | CaptureTarget::ActiveWindow => return Ok(frame),
    };
    frame.cropped(&zone).ok_or_else(|| {
        CaptureError::UnsupportedTarget("la región queda fuera de la pantalla".into())
    })
}

/// Contrato de estrategia de captura.
pub trait ScreenCapture: Send {
    fn kind(&self) -> CaptureBackendKind;
    fn capture(&mut self, target: &CaptureTarget) -> Result<CapturedFrame, CaptureError>;
}

/// Selecciona e instancia el mejor backend de captura disponible para la
/// plataforma/sesión actual. `--check` usa el mismo selector
/// ([`crate::probe::probe`]) para listar elegidos y descartados con motivo.
pub fn probe_capture_backend() -> Result<Box<dyn ScreenCapture>, CaptureError> {
    crate::probe::select_capture(
        &crate::probe::Environment::detect(),
        DesktopLayout::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32, logical: LRect) -> CapturedFrame {
        CapturedFrame {
            rgba: (0..w * h)
                .flat_map(|i| [(i % w) as u8, (i / w) as u8, 0, 255])
                .collect(),
            width: w,
            height: h,
            logical,
            scale: f64::from(w) / logical.w,
            taken_at: Instant::now(),
            backend: CaptureBackendKind::Portal,
        }
    }

    #[test]
    fn downscaled_keeps_aspect_ratio_and_logical_zone() {
        let f = gradient(400, 100, LRect::new(10.0, 20.0, 320.0, 80.0));
        let d = f.downscaled(200);
        assert_eq!((d.width, d.height), (200, 50));
        assert_eq!(d.logical, f.logical);
        assert_eq!(d.rgba.len(), 200 * 50 * 4);
        // Portrait.
        let p = gradient(90, 300, LRect::new(0.0, 0.0, 90.0, 300.0)).downscaled(150);
        assert_eq!((p.width, p.height), (45, 150));
    }

    #[test]
    fn downscaled_is_noop_when_already_small() {
        let f = gradient(64, 32, LRect::new(0.0, 0.0, 64.0, 32.0));
        let d = f.downscaled(1568);
        assert_eq!((d.width, d.height), (64, 32));
        assert_eq!(d.rgba, f.rgba);
    }

    #[test]
    fn downscaled_averages_area() {
        // 2×2 → 1×1: el promedio de los cuatro píxeles.
        let f = CapturedFrame {
            rgba: vec![
                0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255,
            ],
            width: 2,
            height: 2,
            logical: LRect::new(0.0, 0.0, 2.0, 2.0),
            scale: 1.0,
            taken_at: Instant::now(),
            backend: CaptureBackendKind::Grim,
        };
        let d = f.downscaled(1);
        assert_eq!(d.rgba, vec![128, 128, 128, 255]);
    }

    #[test]
    fn redact_leaves_region_uniform_and_rest_untouched() {
        // Escala 2: 100×100 lógicos en 200×200 píxeles.
        let mut f = gradient(200, 200, LRect::new(1000.0, 0.0, 100.0, 100.0));
        let before = f.rgba.clone();
        f.redact(&[LRect::new(1010.0, 10.0, 20.0, 5.0)]);
        let px = |f: &CapturedFrame, x: usize, y: usize| f.rgba[(y * 200 + x) * 4..][..4].to_vec();
        let fill = px(&f, 20, 20);
        for y in 20..30 {
            for x in 20..60 {
                assert_eq!(px(&f, x, y), fill, "({x},{y})");
            }
        }
        assert_eq!(px(&f, 19, 20), before[(20 * 200 + 19) * 4..][..4].to_vec());
        assert_eq!(px(&f, 60, 20), before[(20 * 200 + 60) * 4..][..4].to_vec());
        assert_eq!(px(&f, 20, 30), before[(30 * 200 + 20) * 4..][..4].to_vec());
        // Una región fuera de la captura no toca nada.
        let snapshot = f.rgba.clone();
        f.redact(&[LRect::new(0.0, 0.0, 50.0, 50.0)]);
        assert_eq!(f.rgba, snapshot);
    }

    #[test]
    fn encode_png_roundtrips() {
        let f = gradient(33, 17, LRect::new(0.0, 0.0, 33.0, 17.0));
        let back = CapturedFrame::from_png(
            &f.encode_png(),
            &DesktopLayout::default(),
            None,
            CaptureBackendKind::Spectacle,
        )
        .unwrap();
        assert_eq!((back.width, back.height), (33, 17));
        assert_eq!(back.rgba, f.rgba);
        assert_eq!(back.logical, LRect::new(0.0, 0.0, 33.0, 17.0));
    }

    #[test]
    fn cropped_maps_logical_zone() {
        let f = gradient(200, 200, LRect::new(0.0, 0.0, 100.0, 100.0));
        let c = f.cropped(&LRect::new(25.0, 50.0, 10.0, 10.0)).unwrap();
        assert_eq!((c.width, c.height), (20, 20));
        assert_eq!(c.logical, LRect::new(25.0, 50.0, 10.0, 10.0));
        assert_eq!(&c.rgba[..4], &[50, 100, 0, 255]);
        assert!(f.cropped(&LRect::new(500.0, 0.0, 1.0, 1.0)).is_none());
    }

    #[test]
    fn resolve_logical_prefers_hint_then_matching_output() {
        use crate::geometry::OutputInfo;
        let layout = DesktopLayout::new(vec![
            OutputInfo {
                name: "A".into(),
                logical: LRect::new(0.0, 0.0, 1920.0, 1080.0),
                scale: 1.0,
            },
            OutputInfo {
                name: "B".into(),
                logical: LRect::new(1920.0, 0.0, 2048.0, 1152.0),
                scale: 1.25,
            },
        ]);
        let hint = LRect::new(5.0, 5.0, 100.0, 100.0);
        assert_eq!(
            resolve_logical((200, 200), &layout, Some(hint)),
            (hint, 2.0)
        );
        assert_eq!(
            resolve_logical((2560, 1440), &layout, None),
            (LRect::new(1920.0, 0.0, 2048.0, 1152.0), 1.25)
        );
        let (full, _) = resolve_logical((4480, 1440), &layout, None);
        assert_eq!(full, LRect::new(0.0, 0.0, 3968.0, 1152.0));
        assert_eq!(
            resolve_logical((800, 600), &DesktopLayout::default(), None),
            (LRect::new(0.0, 0.0, 800.0, 600.0), 1.0)
        );
    }
}
