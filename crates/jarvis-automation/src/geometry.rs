//! Geometría del escritorio en coordenadas LÓGICAS del compositor.
//!
//! Toda la API pública de la automatización trabaja en coordenadas lógicas
//! (las que ve el usuario con la escala aplicada). La conversión a píxeles
//! físicos vive solo aquí: multimonitor con offsets negativos y escalas
//! fraccionarias (1.25, 1.5) incluidas.

use serde::{Deserialize, Serialize};

use crate::capture::CapturedFrame;

/// Punto en coordenadas lógicas globales del escritorio.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Rectángulo en coordenadas lógicas (`x`, `y` = esquina superior izquierda).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl LRect {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    /// Borde derecho (exclusivo).
    pub fn right(&self) -> f64 {
        self.x + self.w
    }

    /// Borde inferior (exclusivo).
    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }

    /// `true` si `p` cae dentro (borde izquierdo/superior incluidos,
    /// derecho/inferior excluidos: dos outputs contiguos no se solapan).
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }

    /// Intersección con `other`, o `None` si no se tocan.
    pub fn intersection(&self, other: &LRect) -> Option<LRect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        (r > x && b > y).then(|| LRect::new(x, y, r - x, b - y))
    }

    /// Menor rectángulo que contiene a ambos.
    pub fn union(&self, other: &LRect) -> LRect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let r = self.right().max(other.right());
        let b = self.bottom().max(other.bottom());
        LRect::new(x, y, r - x, b - y)
    }

    /// Centro del rectángulo.
    pub fn center(&self) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// Un monitor: nombre del conector, zona lógica y escala.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub name: String,
    pub logical: LRect,
    pub scale: f64,
}

impl OutputInfo {
    /// Tamaño en píxeles físicos (`lógico × escala`, redondeado).
    pub fn physical_size(&self) -> (u32, u32) {
        (
            (self.logical.w * self.scale).round() as u32,
            (self.logical.h * self.scale).round() as u32,
        )
    }
}

/// Disposición de los monitores del escritorio.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DesktopLayout {
    pub outputs: Vec<OutputInfo>,
}

impl DesktopLayout {
    pub fn new(outputs: Vec<OutputInfo>) -> Self {
        Self { outputs }
    }

    /// Monitor que contiene el punto lógico `p`.
    pub fn output_at(&self, p: Point) -> Option<&OutputInfo> {
        self.outputs.iter().find(|o| o.logical.contains(p))
    }

    /// Monitor por nombre de conector (`DP-1`, `eDP-1`…).
    pub fn output_named(&self, name: &str) -> Option<&OutputInfo> {
        self.outputs.iter().find(|o| o.name == name)
    }

    /// Zona lógica que cubre todos los monitores.
    pub fn bounds(&self) -> Option<LRect> {
        let mut it = self.outputs.iter().map(|o| o.logical);
        let first = it.next()?;
        Some(it.fold(first, |acc, r| acc.union(&r)))
    }

    /// Píxel `(px, py)` de una captura → punto lógico global.
    ///
    /// Usa la zona lógica y el tamaño en píxeles de la propia captura, así que
    /// también es correcto para capturas reescaladas con
    /// [`CapturedFrame::downscaled`] (la relación lógico/píxel ya no es la
    /// escala del output). Se toma el centro del píxel.
    pub fn frame_to_logical(&self, frame: &CapturedFrame, px: (u32, u32)) -> Point {
        let sx = frame.logical.w / f64::from(frame.width.max(1));
        let sy = frame.logical.h / f64::from(frame.height.max(1));
        Point::new(
            frame.logical.x + (f64::from(px.0) + 0.5) * sx,
            frame.logical.y + (f64::from(px.1) + 0.5) * sy,
        )
    }

    /// Punto lógico global → (monitor, píxel físico dentro de ese monitor).
    pub fn logical_to_physical(&self, p: Point) -> Option<(&OutputInfo, (u32, u32))> {
        let out = self.output_at(p)?;
        let x = ((p.x - out.logical.x) * out.scale).floor() as u32;
        let y = ((p.y - out.logical.y) * out.scale).floor() as u32;
        Some((out, (x, y)))
    }

    /// Escala del monitor cuyo tamaño físico coincide con `size`, si hay uno
    /// solo: los backends que solo devuelven la imagen (spectacle `-m`) la
    /// usan para recuperar la zona lógica capturada.
    pub fn output_with_physical_size(&self, size: (u32, u32)) -> Option<&OutputInfo> {
        let mut matches = self.outputs.iter().filter(|o| o.physical_size() == size);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CaptureBackendKind, CapturedFrame};
    use std::time::Instant;

    /// Escenario del contrato §7: 1920×1080@1.0 en (0,0) y un 2560×1440@1.25
    /// (2048×1152 lógicos) en (1920,−200).
    fn layout() -> DesktopLayout {
        DesktopLayout::new(vec![
            OutputInfo {
                name: "eDP-1".into(),
                logical: LRect::new(0.0, 0.0, 1920.0, 1080.0),
                scale: 1.0,
            },
            OutputInfo {
                name: "DP-1".into(),
                logical: LRect::new(1920.0, -200.0, 2048.0, 1152.0),
                scale: 1.25,
            },
        ])
    }

    fn frame(logical: LRect, width: u32, height: u32, scale: f64) -> CapturedFrame {
        CapturedFrame {
            rgba: vec![0; (width * height * 4) as usize],
            width,
            height,
            logical,
            scale,
            taken_at: Instant::now(),
            backend: CaptureBackendKind::Spectacle,
        }
    }

    fn close(a: Point, b: Point) -> bool {
        (a.x - b.x).abs() < 1e-6 && (a.y - b.y).abs() < 1e-6
    }

    #[test]
    fn output_at_respects_edges() {
        let l = layout();
        assert_eq!(l.output_at(Point::new(0.0, 0.0)).unwrap().name, "eDP-1");
        assert_eq!(
            l.output_at(Point::new(1919.9, 1079.9)).unwrap().name,
            "eDP-1"
        );
        // El borde derecho del primero es el izquierdo del segundo.
        assert_eq!(l.output_at(Point::new(1920.0, 0.0)).unwrap().name, "DP-1");
        // Offset negativo: solo el segundo monitor llega a y = −200.
        assert_eq!(
            l.output_at(Point::new(2000.0, -200.0)).unwrap().name,
            "DP-1"
        );
        assert!(l.output_at(Point::new(100.0, -1.0)).is_none());
        assert!(l.output_at(Point::new(3968.0, 0.0)).is_none());
        assert!(l.output_at(Point::new(-0.1, 10.0)).is_none());
    }

    #[test]
    fn frame_to_logical_on_scaled_output_corners_and_center() {
        let l = layout();
        let out = l.output_named("DP-1").unwrap();
        assert_eq!(out.physical_size(), (2560, 1440));
        let f = frame(out.logical, 2560, 1440, 1.25);
        // Esquina superior izquierda: centro del primer píxel físico.
        assert!(close(
            l.frame_to_logical(&f, (0, 0)),
            Point::new(1920.0 + 0.4, -200.0 + 0.4)
        ));
        // Esquina inferior derecha.
        assert!(close(
            l.frame_to_logical(&f, (2559, 1439)),
            Point::new(1920.0 + 2559.5 / 1.25, -200.0 + 1439.5 / 1.25)
        ));
        // Centro.
        let c = l.frame_to_logical(&f, (1280, 720));
        assert!((c.x - (1920.0 + 1024.4)).abs() < 1e-6);
        assert!((c.y - (-200.0 + 576.4)).abs() < 1e-6);
        assert_eq!(l.output_at(c).unwrap().name, "DP-1");
    }

    #[test]
    fn frame_to_logical_on_unscaled_output() {
        let l = layout();
        let f = frame(LRect::new(0.0, 0.0, 1920.0, 1080.0), 1920, 1080, 1.0);
        assert!(close(l.frame_to_logical(&f, (0, 0)), Point::new(0.5, 0.5)));
        assert!(close(
            l.frame_to_logical(&f, (1919, 1079)),
            Point::new(1919.5, 1079.5)
        ));
    }

    #[test]
    fn frame_to_logical_survives_downscale() {
        let l = layout();
        let out = l.output_named("DP-1").unwrap();
        let f = frame(out.logical, 2560, 1440, 1.25).downscaled(1280);
        assert_eq!((f.width, f.height), (1280, 720));
        // Un píxel del reescalado equivale a 1.6 px lógicos.
        let p = l.frame_to_logical(&f, (640, 360));
        assert!((p.x - (1920.0 + 640.5 * 1.6)).abs() < 1e-6);
        assert!((p.y - (-200.0 + 360.5 * 1.6)).abs() < 1e-6);
    }

    #[test]
    fn logical_to_physical_roundtrip_within_two_pixels() {
        let l = layout();
        let out = l.output_named("DP-1").unwrap().clone();
        let f = frame(out.logical, 2560, 1440, 1.25);
        for px in [(0, 0), (1, 1), (777, 333), (2559, 1439), (1280, 720)] {
            let p = l.frame_to_logical(&f, px);
            let (o, back) = l.logical_to_physical(p).unwrap();
            assert_eq!(o.name, "DP-1");
            assert!(back.0.abs_diff(px.0) <= 2 && back.1.abs_diff(px.1) <= 2);
        }
    }

    #[test]
    fn bounds_cover_negative_offsets() {
        let b = layout().bounds().unwrap();
        assert_eq!(b, LRect::new(0.0, -200.0, 3968.0, 1280.0));
        assert!(DesktopLayout::default().bounds().is_none());
    }

    #[test]
    fn output_with_physical_size_requires_a_unique_match() {
        let l = layout();
        assert_eq!(
            l.output_with_physical_size((2560, 1440)).unwrap().name,
            "DP-1"
        );
        assert!(l.output_with_physical_size((800, 600)).is_none());
        let twins = DesktopLayout::new(vec![
            OutputInfo {
                name: "A".into(),
                logical: LRect::new(0.0, 0.0, 1920.0, 1080.0),
                scale: 1.0,
            },
            OutputInfo {
                name: "B".into(),
                logical: LRect::new(1920.0, 0.0, 1920.0, 1080.0),
                scale: 1.0,
            },
        ]);
        assert!(twins.output_with_physical_size((1920, 1080)).is_none());
    }

    #[test]
    fn rect_intersection_and_contains() {
        let a = LRect::new(0.0, 0.0, 10.0, 10.0);
        let b = LRect::new(5.0, -5.0, 10.0, 10.0);
        assert_eq!(a.intersection(&b), Some(LRect::new(5.0, 0.0, 5.0, 5.0)));
        assert!(a.intersection(&LRect::new(10.0, 0.0, 1.0, 1.0)).is_none());
        assert!(a.contains(a.center()));
    }
}
