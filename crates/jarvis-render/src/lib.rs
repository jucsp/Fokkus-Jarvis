//! Renderizado CPU de la esfera líquida orgánica (blob) de Jarvis.
//!
//! Usa [`tiny_skia`] para producir un `Pixmap` RGBA premultiplicado sin tocar
//! la GPU, lo que mantiene el consumo de RAM bajo en reposo.
//!
//! Para el bucle principal a 60 fps se recomienda [`SphereRenderer::render_into`],
//! que reutiliza el mismo buffer y no asigna memoria por frame.

use std::f32::consts::TAU;
use std::fmt;

use jarvis_core::JarvisState;
use tiny_skia::{
    Color, FillRule, GradientStop, Paint, PathBuilder, Point, RadialGradient, Shader, SpreadMode,
    Stroke, Transform,
};

/// Buffer RGBA premultiplicado reutilizable con [`SphereRenderer::render_into`].
pub use tiny_skia::Pixmap;

/// Tamaño por defecto de la esfera, en píxeles.
pub const DEFAULT_SPHERE_SIZE: u32 = 220;

/// Número de segmentos del contorno orgánico (suficiente para 220 px).
const BLOB_SEGMENTS: usize = 96;

/// Número de anillos del halo exterior.
const HALO_RINGS: usize = 10;

/// Velocidad angular base del blob (rad/s) para la animación continua.
const BLOB_ANIMATION_SPEED: f32 = 2.0;

/// Velocidad de pulsación de la onda interna (rad/s).
const WAVE_ANIMATION_SPEED: f32 = 3.0;

/// Error de renderizado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderError {
    /// No se pudo reservar el `Pixmap` (tamaño inválido o falta de memoria).
    AllocFailed { size: u32 },
    /// No se pudo construir el contorno del blob.
    PathBuildFailed,
    /// El buffer suministrado a `render_into` no tiene el tamaño del renderer.
    InvalidBuffer {
        expected: (u32, u32),
        actual: (u32, u32),
    },
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::AllocFailed { size } => {
                write!(f, "no se pudo reservar el pixmap de {size}x{size}")
            }
            RenderError::PathBuildFailed => write!(f, "no se pudo construir el contorno del blob"),
            RenderError::InvalidBuffer { expected, actual } => {
                write!(f, "buffer de tamaño {actual:?}; se esperaba {expected:?}")
            }
        }
    }
}

impl std::error::Error for RenderError {}

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

    /// Renderiza un frame en un `Pixmap` nuevo de `size x size`.
    ///
    /// Conveniente para tests, snapshots y el arranque. En el bucle principal
    /// se debe usar [`SphereRenderer::render_into`] para evitar asignaciones.
    pub fn render(&self, level: f32, state: JarvisState, t: f32) -> Result<Pixmap, RenderError> {
        let mut pixmap = Pixmap::new(self.size, self.size)
            .ok_or(RenderError::AllocFailed { size: self.size })?;
        self.render_into(&mut pixmap, level, state, t)?;
        Ok(pixmap)
    }

    /// Renderiza un frame sobre un `Pixmap` preexistente, sin asignar memoria.
    ///
    /// El buffer debe tener exactamente `size x size` píxeles. Se limpia al
    /// inicio para que no queden restos de frames anteriores.
    pub fn render_into(
        &self,
        pixmap: &mut Pixmap,
        level: f32,
        state: JarvisState,
        t: f32,
    ) -> Result<(), RenderError> {
        if pixmap.width() != self.size || pixmap.height() != self.size {
            return Err(RenderError::InvalidBuffer {
                expected: (self.size, self.size),
                actual: (pixmap.width(), pixmap.height()),
            });
        }

        let level = level.clamp(0.0, 1.0);
        let palette = palette_for(state);

        pixmap.fill(Color::TRANSPARENT);

        self.draw_halo(pixmap, palette, level);
        let blob = self.build_blob_path(level, state, t)?;
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
        self.draw_inner_wave(pixmap, palette, level, t);

        Ok(())
    }

    fn phase_for(state: JarvisState, t: f32) -> f32 {
        let base = match state {
            JarvisState::Idle => 0.0,
            JarvisState::Listening => 0.55,
            JarvisState::Thinking => 1.25,
            JarvisState::Speaking => 0.95,
            JarvisState::Muted => 0.0,
        };
        // El tiempo desplaza la fase de forma continua: los armónicos rotan y
        // la esfera "respira" incluso con nivel de audio constante (H1).
        base + t * BLOB_ANIMATION_SPEED
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

    fn build_blob_path(
        &self,
        level: f32,
        state: JarvisState,
        t: f32,
    ) -> Result<tiny_skia::Path, RenderError> {
        let phase = Self::phase_for(state, t);
        let mut builder = PathBuilder::new();
        for i in 0..BLOB_SEGMENTS {
            let u = i as f32 / BLOB_SEGMENTS as f32;
            let angle = u * TAU;
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
        builder.finish().ok_or(RenderError::PathBuildFailed)
    }

    fn draw_halo(&self, pixmap: &mut Pixmap, palette: Palette, level: f32) {
        for i in 0..HALO_RINGS {
            let u = i as f32 / HALO_RINGS as f32;
            let alpha = ((1.0 - u) * (1.0 - u) * 90.0) as u8;
            let extra = 0.04 + u * 0.55;
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

    fn draw_inner_wave(&self, pixmap: &mut Pixmap, palette: Palette, level: f32, t: f32) {
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

        // Onda interna concéntrica que pulsa con el nivel de audio y el tiempo.
        let pulse = (t * WAVE_ANIMATION_SPEED).sin() * 0.06;
        let ring_radius = self.base_radius * (0.45 + level * 0.35 + pulse);
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
        let pixmap = renderer.render(0.0, JarvisState::Idle, 0.0).unwrap();
        assert_eq!(pixmap.width(), 220);
        assert_eq!(pixmap.height(), 220);
        assert_eq!(pixmap.data().len(), 220 * 220 * 4);
    }

    #[test]
    fn center_is_opaque() {
        let renderer = SphereRenderer::new();
        let pixmap = renderer.render(0.5, JarvisState::Idle, 0.0).unwrap();
        assert!(alpha_at(&pixmap, 110, 110) > 200);
    }

    #[test]
    fn corners_are_transparent() {
        let renderer = SphereRenderer::new();
        let pixmap = renderer.render(0.5, JarvisState::Speaking, 0.0).unwrap();
        assert_eq!(alpha_at(&pixmap, 0, 0), 0);
        assert_eq!(alpha_at(&pixmap, 219, 219), 0);
    }

    #[test]
    fn renders_every_state_without_panicking() {
        let renderer = SphereRenderer::new();
        for state in JarvisState::ALL {
            for level in [0.0, 0.25, 0.5, 0.75, 1.0] {
                for t in [0.0, 0.25, 0.5, 1.0] {
                    let pixmap = renderer.render(level, state, t).unwrap();
                    assert_eq!(pixmap.width(), 220);
                    assert_eq!(pixmap.height(), 220);
                }
            }
        }
    }

    #[test]
    fn out_of_range_level_is_clamped() {
        let renderer = SphereRenderer::new();
        let over = renderer.render(2.0, JarvisState::Idle, 0.0).unwrap();
        let max = renderer.render(1.0, JarvisState::Idle, 0.0).unwrap();
        assert_eq!(over.data(), max.data());

        let under = renderer.render(-1.0, JarvisState::Idle, 0.0).unwrap();
        let min = renderer.render(0.0, JarvisState::Idle, 0.0).unwrap();
        assert_eq!(under.data(), min.data());
    }

    #[test]
    fn animation_changes_output_over_time() {
        let renderer = SphereRenderer::new();
        let a = renderer.render(0.5, JarvisState::Idle, 0.0).unwrap();
        let b = renderer.render(0.5, JarvisState::Idle, 0.5).unwrap();
        // La deformación depende del tiempo: dos instantes distintos deben
        // producir píxeles distintos aunque el nivel sea el mismo (H1).
        assert_ne!(a.data(), b.data());
    }

    #[test]
    fn render_into_reuses_buffer_without_panicking() {
        let renderer = SphereRenderer::new();
        let mut buffer = Pixmap::new(220, 220).unwrap();
        let expected_len = buffer.data().len();

        renderer
            .render_into(&mut buffer, 0.5, JarvisState::Speaking, 0.0)
            .unwrap();
        assert_eq!(buffer.data().len(), expected_len);
        assert!(alpha_at(&buffer, 110, 110) > 200);

        renderer
            .render_into(&mut buffer, 0.9, JarvisState::Listening, 1.0)
            .unwrap();
        assert_eq!(buffer.data().len(), expected_len);
        assert!(alpha_at(&buffer, 110, 110) > 200);
    }

    #[test]
    fn render_into_rejects_wrong_size_buffer() {
        let renderer = SphereRenderer::new();
        let mut buffer = Pixmap::new(100, 100).unwrap();
        let err = renderer
            .render_into(&mut buffer, 0.0, JarvisState::Idle, 0.0)
            .unwrap_err();
        assert_eq!(
            err,
            RenderError::InvalidBuffer {
                expected: (220, 220),
                actual: (100, 100),
            }
        );
    }

    /// Criterio 1.3: render 220x220 < 2 ms/frame en release.
    /// Ejecutar con `cargo test -p jarvis-render --release -- --ignored`.
    #[test]
    #[ignore = "benchmark: solo significativo en release"]
    fn render_budget_under_2ms_in_release() {
        let renderer = SphereRenderer::new();
        let mut buffer = Pixmap::new(220, 220).unwrap();
        let frames = 300u32;
        let start = std::time::Instant::now();
        for i in 0..frames {
            let level = (i % 100) as f32 / 100.0;
            let t = i as f32 / 60.0;
            renderer
                .render_into(&mut buffer, level, JarvisState::Speaking, t)
                .unwrap();
            std::hint::black_box(&buffer);
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
        let pixmap = renderer.render(0.25, JarvisState::Muted, 0.0).unwrap();
        assert_eq!(pixmap.width(), 128);
        assert_eq!(pixmap.height(), 128);
        assert!(alpha_at(&pixmap, 64, 64) > 200);
    }
}
