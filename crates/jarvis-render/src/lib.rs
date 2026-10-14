//! Renderizado CPU de la esfera líquida orgánica (blob) de Jarvis.
//!
//! Usa [`tiny_skia`] para producir un `Pixmap` RGBA premultiplicado sin tocar
//! la GPU, lo que mantiene el consumo de RAM bajo en reposo.

use std::f32::consts::TAU;

use jarvis_core::JarvisState;
use tiny_skia::{
    Color, FillRule, GradientStop, Paint, PathBuilder, Pixmap, Point, RadialGradient, Shader,
    SpreadMode, Stroke, Transform,
};

/// Tamaño por defecto de la esfera, en píxeles.
pub const DEFAULT_SPHERE_SIZE: u32 = 220;

/// Número de segmentos del contorno orgánico (suficiente para 220 px).
const BLOB_SEGMENTS: usize = 96;

/// Número de anillos del halo exterior.
const HALO_RINGS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Palette {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

impl Palette {
    const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
}

fn palette_for(state: JarvisState) -> Palette {
    match state {
        JarvisState::Idle => Palette::rgb(0, 188, 212), // cian
        JarvisState::Listening => Palette::rgb(76, 175, 80), // verde
        JarvisState::Thinking => Palette::rgb(156, 39, 176), // púrpura
        JarvisState::Speaking => Palette::rgb(33, 150, 243), // azul
        JarvisState::Muted => Palette::rgb(158, 158, 158), // gris
    }
}

/// Mezcla un color base hacia un tono `target` (0..255) con factor `t` (0..1).
fn mix_towards(base: Palette, target: u8, t: f32) -> Palette {
    let blend = |v: u8| (v as f32 * (1.0 - t) + target as f32 * t).round() as u8;
    Palette {
        r: blend(base.r),
        g: blend(base.g),
        b: blend(base.b),
        a: base.a,
    }
}

/// Renderizador de la esfera líquida.
#[derive(Debug, Clone, Copy)]
pub struct SphereRenderer {
    size: u32,
    center: f32,
    base_radius: f32,
}

impl Default for SphereRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl SphereRenderer {
    pub fn new() -> Self {
        Self::with_size(DEFAULT_SPHERE_SIZE)
    }

    pub fn with_size(size: u32) -> Self {
        let size = size.max(1);
        let center = size as f32 / 2.0;
        let base_radius = center * 0.72;
        Self {
            size,
            center,
            base_radius,
        }
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    /// Renderiza un frame de `size x size` para el estado y nivel dados.
    ///
    /// `level` se satura a `0.0..=1.0`. El fondo queda transparente para que la
    /// ventana overlay lo muestre sobre el escritorio.
    pub fn render(&self, level: f32, state: JarvisState) -> Pixmap {
        let level = level.clamp(0.0, 1.0);
        let palette = palette_for(state);

        let mut pixmap = Pixmap::new(self.size, self.size).expect("tamaño de pixmap válido");

        self.draw_halo(&mut pixmap, palette, level);
        let blob = self.build_blob_path(level, state);
        let paint = Paint {
            shader: sphere_shader(self.center, self.base_radius, palette),
            anti_alias: true,
            ..Default::default()
        };
        pixmap.fill_path(
            &blob,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
        self.draw_inner_wave(&mut pixmap, palette, level);

        pixmap
    }

    fn phase_for(state: JarvisState) -> f32 {
        match state {
            JarvisState::Idle => 0.0,
            JarvisState::Listening => 0.55,
            JarvisState::Thinking => 1.25,
            JarvisState::Speaking => 0.95,
            JarvisState::Muted => 0.0,
        }
    }

    fn blob_radius(&self, angle: f32, level: f32, phase: f32) -> f32 {
        let mut factor = 1.0;
        factor += 0.06 * (3.0 * angle + phase).sin();
        factor += 0.035 * (5.0 * angle - 1.7 * phase).sin();
        factor += 0.02 * (8.0 * angle + 0.6 * phase).sin();
        let drive = 0.02 + level * 0.10;
        factor += drive * (7.0 * angle + phase).sin();
        factor * self.base_radius
    }

    fn build_blob_path(&self, level: f32, state: JarvisState) -> tiny_skia::Path {
        let phase = Self::phase_for(state);
        let mut builder = PathBuilder::new();
        for i in 0..BLOB_SEGMENTS {
            let t = i as f32 / BLOB_SEGMENTS as f32;
            let angle = t * TAU;
            let radius = self.blob_radius(angle, level, phase);
            let x = self.center + radius * angle.cos();
            let y = self.center + radius * angle.sin();
            if i == 0 {
                builder.move_to(x, y);
            } else {
                builder.line_to(x, y);
            }
        }
        builder.close();
        builder.finish().expect("contorno de blob válido")
    }

    fn draw_halo(&self, pixmap: &mut Pixmap, palette: Palette, level: f32) {
        for i in 0..HALO_RINGS {
            let t = i as f32 / HALO_RINGS as f32;
            let alpha = ((1.0 - t) * (1.0 - t) * 90.0) as u8;
            let extra = 0.04 + t * 0.55;
            let radius = self.base_radius * (1.0 + extra) * (1.0 + 0.06 * level);
            let mut paint = Paint::default();
            paint.set_color_rgba8(palette.r, palette.g, palette.b, alpha);
            paint.anti_alias = true;
            if let Some(circle) = PathBuilder::from_circle(self.center, self.center, radius) {
                pixmap.fill_path(
                    &circle,
                    &paint,
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }
    }

    fn draw_inner_wave(&self, pixmap: &mut Pixmap, palette: Palette, level: f32) {
        // Brillo especular desplazado hacia arriba-izquierda.
        let highlight_radius = self.base_radius * 0.34 * (1.0 + 0.3 * level);
        let hx = self.center - self.base_radius * 0.28;
        let hy = self.center - self.base_radius * 0.32;
        let mut highlight = Paint::default();
        highlight.set_color_rgba8(255, 255, 255, 70);
        highlight.anti_alias = true;
        if let Some(circle) = PathBuilder::from_circle(hx, hy, highlight_radius) {
            pixmap.fill_path(
                &circle,
                &highlight,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }

        // Onda interna concéntrica que pulsa con el nivel de audio.
        let ring_radius = self.base_radius * (0.45 + level * 0.35);
        let mut ring = Paint::default();
        ring.set_color_rgba8(palette.r, palette.g, palette.b, 120);
        ring.anti_alias = true;
        let stroke = Stroke {
            width: 2.0 + level * 4.0,
            ..Default::default()
        };
        if let Some(circle) = PathBuilder::from_circle(self.center, self.center, ring_radius) {
            pixmap.stroke_path(&circle, &ring, &stroke, Transform::identity(), None);
        }
    }
}

fn sphere_shader(center: f32, radius: f32, palette: Palette) -> Shader<'static> {
    let light = mix_towards(palette, 255, 0.45);
    let dark = mix_towards(palette, 20, 0.40);
    let stops = vec![
        GradientStop::new(0.0, Color::from_rgba8(light.r, light.g, light.b, 255)),
        GradientStop::new(0.6, Color::from_rgba8(palette.r, palette.g, palette.b, 255)),
        GradientStop::new(1.0, Color::from_rgba8(dark.r, dark.g, dark.b, 255)),
    ];
    RadialGradient::new(
        Point::from_xy(center, center),
        Point::from_xy(center, center),
        radius,
        stops,
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap_or_else(|| Shader::SolidColor(Color::from_rgba8(palette.r, palette.g, palette.b, 255)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(pixmap: &Pixmap, x: u32, y: u32) -> u8 {
        let idx = ((y * pixmap.width() + x) * 4 + 3) as usize;
        pixmap.data()[idx]
    }

    #[test]
    fn renders_default_sized_pixmap() {
        let renderer = SphereRenderer::new();
        let pixmap = renderer.render(0.0, JarvisState::Idle);
        assert_eq!(pixmap.width(), 220);
        assert_eq!(pixmap.height(), 220);
        assert_eq!(pixmap.data().len(), 220 * 220 * 4);
    }

    #[test]
    fn center_is_opaque() {
        let renderer = SphereRenderer::new();
        let pixmap = renderer.render(0.5, JarvisState::Idle);
        assert!(alpha_at(&pixmap, 110, 110) > 200);
    }

    #[test]
    fn corners_are_transparent() {
        let renderer = SphereRenderer::new();
        let pixmap = renderer.render(0.5, JarvisState::Speaking);
        assert_eq!(alpha_at(&pixmap, 0, 0), 0);
        assert_eq!(alpha_at(&pixmap, 219, 219), 0);
    }

    #[test]
    fn renders_every_state_without_panicking() {
        let renderer = SphereRenderer::new();
        for state in JarvisState::ALL {
            for level in [0.0, 0.25, 0.5, 0.75, 1.0] {
                let pixmap = renderer.render(level, state);
                assert_eq!(pixmap.width(), 220);
                assert_eq!(pixmap.height(), 220);
            }
        }
    }

    #[test]
    fn out_of_range_level_is_clamped() {
        let renderer = SphereRenderer::new();
        let over = renderer.render(2.0, JarvisState::Idle);
        let max = renderer.render(1.0, JarvisState::Idle);
        assert_eq!(over.data(), max.data());

        let under = renderer.render(-1.0, JarvisState::Idle);
        let min = renderer.render(0.0, JarvisState::Idle);
        assert_eq!(under.data(), min.data());
    }

    /// Criterio 1.3: render 220x220 < 2 ms/frame en release.
    /// Ejecutar con `cargo test -p jarvis-render --release -- --ignored`.
    #[test]
    #[ignore = "benchmark: solo significativo en release"]
    fn render_budget_under_2ms_in_release() {
        let renderer = SphereRenderer::new();
        let frames = 300u32;
        let start = std::time::Instant::now();
        for i in 0..frames {
            let level = (i % 100) as f32 / 100.0;
            std::hint::black_box(renderer.render(level, JarvisState::Speaking));
        }
        let per_frame = start.elapsed() / frames;
        println!("render 220x220: {per_frame:?}/frame");
        assert!(
            per_frame.as_micros() < 2_000,
            "{per_frame:?}/frame excede 2 ms"
        );
    }

    #[test]
    fn supports_custom_size() {
        let renderer = SphereRenderer::with_size(128);
        let pixmap = renderer.render(0.25, JarvisState::Muted);
        assert_eq!(pixmap.width(), 128);
        assert_eq!(pixmap.height(), 128);
        assert!(alpha_at(&pixmap, 64, 64) > 200);
    }
}
