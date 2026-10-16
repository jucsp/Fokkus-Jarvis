//! Renderizado CPU de la esfera líquida orgánica (blob) de Jarvis.
//!
//! Usa [`tiny_skia`] para producir un `Pixmap` RGBA premultiplicado sin tocar
//! la GPU, lo que mantiene el consumo de RAM bajo en reposo.
//!
//! Para el bucle principal a 60 fps se recomienda [`SphereRenderer::render_into`],
//! que reutiliza el mismo buffer y no asigna memoria por frame.

use std::f32::consts::TAU;
use std::fmt;
use std::time::{Duration, Instant};

use jarvis_core::{JarvisState, Opacity};
use tiny_skia::{
    Color, FillRule, GradientStop, Paint, PathBuilder, Point, RadialGradient, Shader, SpreadMode,
    Stroke, Transform,
};

mod hud;

/// Buffer RGBA premultiplicado reutilizable con [`SphereRenderer::render_into`].
pub use tiny_skia::Pixmap;

pub use hud::{
    bubble_ttl, Bubble, BubbleStack, HudRenderer, Speaker, TextRasterizer, TurnId, FADE_IN,
    FADE_OUT_EVICT, FADE_OUT_TTL,
};

/// Tamaño por defecto de la esfera, en píxeles.
///
/// Alias de compatibilidad: la constante canónica vive en `jarvis_core`.
pub const DEFAULT_SPHERE_SIZE: u32 = jarvis_core::DEFAULT_SPHERE_SIZE_PX;

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
    /// No se pudo cargar la fuente embebida del HUD (HU-36).
    FontLoadFailed,
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
            RenderError::FontLoadFailed => write!(f, "no se pudo cargar la fuente del HUD"),
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
        let mut renderer = Self {
            size: 0,
            center: 0.0,
            base_radius: 0.0,
        };
        renderer.resize(size);
        renderer
    }

    /// Cambia el tamaño sin reasignar nada (el `Pixmap` lo gestiona el llamador).
    pub fn resize(&mut self, size: u32) {
        let size = size.max(1);
        self.size = size;
        self.center = size as f32 / 2.0;
        self.base_radius = self.center * 0.72;
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

/// Multiplica los canales premultiplicados de un `Pixmap` por `opacity`.
///
/// Con premultiplicado, escalar los cuatro canales por `α` equivale a componer
/// la capa con opacidad `α`, sin artefactos en los bordes antialias. Con
/// [`Opacity::OPAQUE`] no toca el buffer (atajo de coste cero).
pub fn apply_opacity(pixmap: &mut Pixmap, opacity: Opacity) {
    if opacity.is_opaque() {
        return;
    }
    let a = u32::from(opacity.as_u8());
    for pixel in pixmap.data_mut().as_chunks_mut::<4>().0 {
        for channel in pixel.iter_mut() {
            let c = u32::from(*channel);
            *channel = ((c * a + 127) / 255) as u8;
        }
    }
}

/// Interpolador temporal lineal de opacidad (fundidos suaves).
#[derive(Debug, Clone, Copy)]
pub struct OpacityFade {
    from: Opacity,
    to: Opacity,
    started_at: Instant,
    duration: Duration,
}

impl OpacityFade {
    /// Crea un fundido ya asentado en `initial`.
    pub fn new(initial: Opacity, duration: Duration) -> Self {
        Self {
            from: initial,
            to: initial,
            started_at: Instant::now(),
            duration,
        }
    }

    /// Cambia el objetivo partiendo del valor interpolado actual (sin saltos).
    pub fn retarget(&mut self, to: Opacity, now: Instant) {
        self.from = self.value_at(now);
        self.to = to;
        self.started_at = now;
    }

    /// Opacidad interpolada en el instante `now`.
    pub fn value_at(&self, now: Instant) -> Opacity {
        if self.from == self.to || self.duration.is_zero() {
            return self.to;
        }
        let elapsed = now.saturating_duration_since(self.started_at);
        let t = (elapsed.as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0);
        let from = f32::from(self.from.as_u8());
        let to = f32::from(self.to.as_u8());
        let value = (from + (to - from) * t).round();
        Opacity::from_raw(value.clamp(0.0, 255.0) as u8)
    }

    /// `true` cuando se alcanzó el objetivo (no quedan pasos intermedios).
    pub fn is_settled(&self, now: Instant) -> bool {
        self.from == self.to || now.saturating_duration_since(self.started_at) >= self.duration
    }
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

    #[test]
    fn resize_recalculates_geometry() {
        let mut renderer = SphereRenderer::new();
        assert_eq!(renderer.size(), 220);

        renderer.resize(440);
        assert_eq!(renderer.size(), 440);
        let pixmap = renderer.render(0.0, JarvisState::Idle, 0.0).unwrap();
        assert_eq!(pixmap.width(), 440);
        assert_eq!(pixmap.height(), 440);
        assert!(alpha_at(&pixmap, 220, 220) > 200);

        renderer.resize(110);
        assert_eq!(renderer.size(), 110);
        let small = renderer.render(0.0, JarvisState::Idle, 0.0).unwrap();
        assert_eq!(small.width(), 110);
        assert_eq!(small.height(), 110);
    }

    #[test]
    fn apply_opacity_scales_premultiplied_channels() {
        let mut pixmap = Pixmap::new(1, 1).unwrap();
        pixmap.data_mut().copy_from_slice(&[255, 255, 255, 255]);
        apply_opacity(&mut pixmap, Opacity::from_pct(50));
        assert_eq!(pixmap.data(), &[128, 128, 128, 128]);

        let mut opaque = Pixmap::new(1, 1).unwrap();
        opaque.data_mut().copy_from_slice(&[10, 20, 30, 40]);
        apply_opacity(&mut opaque, Opacity::OPAQUE);
        assert_eq!(opaque.data(), &[10, 20, 30, 40]);
    }

    #[test]
    fn apply_opacity_keeps_premultiplied_invariant() {
        let mut pixmap = Pixmap::new(64, 64).unwrap();
        let mut seed = 42u32;
        let mut rng = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed
        };
        for pixel in pixmap.data_mut().as_chunks_mut::<4>().0 {
            let a = (rng() % 256) as u8;
            let cap = |v: u32| v.min(u32::from(a)) as u8;
            pixel[0] = cap(rng() % 256);
            pixel[1] = cap(rng() % 256);
            pixel[2] = cap(rng() % 256);
            pixel[3] = a;
        }
        apply_opacity(&mut pixmap, Opacity::from_pct(33));
        for pixel in pixmap.data().as_chunks::<4>().0 {
            let a = pixel[3];
            assert!(pixel[0] <= a && pixel[1] <= a && pixel[2] <= a, "{pixel:?}");
        }
    }

    #[test]
    fn opacity_fade_interpolates_and_retargets() {
        let duration = Duration::from_millis(150);
        let mut fade = OpacityFade::new(Opacity::OPAQUE, duration);
        let t0 = Instant::now();
        assert_eq!(fade.value_at(t0), Opacity::OPAQUE);
        assert!(fade.is_settled(t0));

        fade.retarget(Opacity::from_raw(0), t0);
        assert_eq!(fade.value_at(t0), Opacity::OPAQUE);
        assert_eq!(fade.value_at(t0 + duration / 2), Opacity::from_raw(128));
        assert_eq!(fade.value_at(t0 + duration), Opacity::from_raw(0));
        assert!(!fade.is_settled(t0 + duration / 2));
        assert!(fade.is_settled(t0 + duration));

        // Un retarget a mitad del fundido parte del valor interpolado actual.
        let mid = t0 + duration / 2;
        fade.retarget(Opacity::OPAQUE, mid);
        assert_eq!(fade.value_at(mid), Opacity::from_raw(128));
        assert_eq!(fade.value_at(mid + duration / 2), Opacity::from_raw(192));
        assert_eq!(fade.value_at(mid + duration), Opacity::OPAQUE);
    }
}
