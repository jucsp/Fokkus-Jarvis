//! HUD de burbujas de conversación efímeras (HU-36).
//!
//! Lógica pura de la pila de burbujas ([`BubbleStack`], testeable con
//! [`Instant`] inyectado) y rasterizado de texto en CPU con `fontdue` y la
//! fuente Noto Sans embebida. El dibujo final vive en [`HudRenderer`].

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use jarvis_core::Opacity;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Transform};

use crate::{Pixmap, RenderError};

/// Identificador de turno de conversación (compartido con `jarvis-audio`).
pub type TurnId = u64;

/// Quién emite la burbuja.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    User,
    Jarvis,
    System,
}

/// Duración del fundido de entrada de una burbuja.
pub const FADE_IN: Duration = Duration::from_millis(150);

/// Duración del fundido de salida por vencimiento del TTL.
pub const FADE_OUT_TTL: Duration = Duration::from_millis(400);

/// Duración del fundido de salida por desalojo FIFO (la cola se llenó).
pub const FADE_OUT_EVICT: Duration = Duration::from_millis(250);

/// TTL de una burbuja: `max(4 s, 60 ms × caracteres)` con tope de 12 s.
pub fn bubble_ttl(chars: usize) -> Duration {
    let ms = (chars as u64).saturating_mul(60).clamp(4_000, 12_000);
    Duration::from_millis(ms)
}

/// Una burbuja de conversación.
#[derive(Debug, Clone)]
pub struct Bubble {
    pub speaker: Speaker,
    pub turn: Option<TurnId>,
    pub text: String,
    pub born: Instant,
    /// Momento en que terminó de sonar (o apareció, si es del usuario): el TTL
    /// empieza a contar desde aquí.
    pub sealed_at: Option<Instant>,
    pub interrupted: bool,
    /// Momento en que empezó su fundido de salida por desalojo FIFO.
    /// Es interno: cuando la cola está llena y entra una nueva, la más antigua
    /// se desvanece durante [`FADE_OUT_EVICT`] antes de ser retirada por `gc`.
    evicted_at: Option<Instant>,
}

impl Bubble {
    /// Ventana temporal `(inicio, fin)` del fundido de salida, si lo hay.
    fn fade_out_window(&self) -> Option<(Instant, Instant)> {
        if let Some(evicted) = self.evicted_at {
            Some((evicted, evicted + FADE_OUT_EVICT))
        } else if let Some(sealed) = self.sealed_at {
            let start = sealed + bubble_ttl(self.text.chars().count());
            Some((start, start + FADE_OUT_TTL))
        } else {
            None
        }
    }

    /// Opacidad (0.0..=1.0) en `now`, aplicando fade-in y fade-out.
    fn alpha(&self, now: Instant) -> f32 {
        let fade_in = FADE_IN.as_secs_f32();
        let age = now.saturating_duration_since(self.born).as_secs_f32();
        let mut alpha = (age / fade_in).clamp(0.0, 1.0);

        if let Some((start, end)) = self.fade_out_window() {
            let fade = (end - start).as_secs_f32();
            if fade > 0.0 {
                let since = now.saturating_duration_since(start).as_secs_f32();
                alpha = alpha.min(1.0 - (since / fade).clamp(0.0, 1.0));
            } else {
                alpha = 0.0;
            }
        }
        alpha.clamp(0.0, 1.0)
    }

    /// `true` si la burbuja está animando (entrando o saliendo) en `now`.
    fn animating(&self, now: Instant) -> bool {
        if now < self.born + FADE_IN {
            return true;
        }
        if let Some((start, end)) = self.fade_out_window() {
            if now >= start && now < end {
                return true;
            }
        }
        false
    }

    /// `true` si el fundido de salida ya terminó y la burbuja debe retirarse.
    fn expired(&self, now: Instant) -> bool {
        match self.fade_out_window() {
            Some((_, end)) => now >= end,
            None => false,
        }
    }
}

/// Pila acotada de burbujas con FIFO, fusión por turno y TTL.
pub struct BubbleStack {
    bubbles: VecDeque<Bubble>,
    max: usize,
}

impl BubbleStack {
    /// Crea una pila con `max` burbujas simultáneas, saturado a 1..=3.
    pub fn new(max: usize) -> Self {
        Self {
            bubbles: VecDeque::new(),
            max: max.clamp(1, 3),
        }
    }

    /// Número de burbujas actualmente en la pila (incluye las que se están
    /// desvaneciendo por desalojo).
    pub fn len(&self) -> usize {
        self.bubbles.len()
    }

    /// `true` si no hay ninguna burbuja.
    pub fn is_empty(&self) -> bool {
        self.bubbles.is_empty()
    }

    /// Vacía la pila de inmediato (`Clear` del HUD).
    pub fn clear(&mut self) {
        self.bubbles.clear();
    }

    /// Alta de una burbuja del usuario: nace y queda sellada al instante.
    ///
    /// Si el turno ya tiene una burbuja (continuación tras una pausa), el
    /// texto nuevo se concatena y se renueva su TTL sin duplicarla
    /// (HU-AUDIO-CONTINUITY).
    pub fn push_user(&mut self, turn: TurnId, text: impl Into<String>, now: Instant) {
        let text = text.into();
        if let Some(existing) = self
            .bubbles
            .iter_mut()
            .rev()
            .find(|b| b.speaker == Speaker::User && b.turn == Some(turn))
        {
            if !text.is_empty() {
                if !existing.text.is_empty() {
                    existing.text.push(' ');
                }
                existing.text.push_str(&text);
            }
            existing.sealed_at = Some(now);
            existing.evicted_at = None;
            return;
        }
        let bubble = Bubble {
            speaker: Speaker::User,
            turn: Some(turn),
            text,
            born: now,
            sealed_at: Some(now),
            interrupted: false,
            evicted_at: None,
        };
        self.push(bubble, now);
    }

    /// Alta de una frase de Jarvis. Si el turno sigue abierto (no sellado), se
    /// concatena a la burbuja existente en lugar de crear una nueva.
    pub fn push_jarvis(&mut self, turn: TurnId, text: impl Into<String>, now: Instant) {
        let text = text.into();
        if let Some(existing) =
            self.bubbles.iter_mut().rev().find(|b| {
                b.speaker == Speaker::Jarvis && b.turn == Some(turn) && b.sealed_at.is_none()
            })
        {
            if !text.is_empty() {
                if !existing.text.is_empty() {
                    existing.text.push(' ');
                }
                existing.text.push_str(&text);
            }
            return;
        }
        // Frase tardía de un turno ya cerrado (sellado o interrumpido): no
        // abre una burbuja nueva.
        if self
            .bubbles
            .iter()
            .any(|b| b.speaker == Speaker::Jarvis && b.turn == Some(turn))
        {
            return;
        }
        let bubble = Bubble {
            speaker: Speaker::Jarvis,
            turn: Some(turn),
            text,
            born: now,
            sealed_at: None,
            interrupted: false,
            evicted_at: None,
        };
        self.push(bubble, now);
    }

    /// Alta de un aviso del sistema (IPC, tareas): se sella al nacer.
    pub fn push_notice(&mut self, text: impl Into<String>, now: Instant) {
        let bubble = Bubble {
            speaker: Speaker::System,
            turn: None,
            text: text.into(),
            born: now,
            sealed_at: Some(now),
            interrupted: false,
            evicted_at: None,
        };
        self.push(bubble, now);
    }

    /// Sella la burbuja de Jarvis del turno dado: su TTL empieza a contar.
    pub fn seal(&mut self, turn: TurnId, now: Instant) {
        if let Some(bubble) =
            self.bubbles.iter_mut().rev().find(|b| {
                b.speaker == Speaker::Jarvis && b.turn == Some(turn) && b.sealed_at.is_none()
            })
        {
            bubble.sealed_at = Some(now);
        }
    }

    /// Marca interrupción: añade "…", sella y empieza el TTL (barge-in).
    pub fn interrupt(&mut self, turn: TurnId, now: Instant) {
        if let Some(bubble) = self
            .bubbles
            .iter_mut()
            .rev()
            .find(|b| b.speaker == Speaker::Jarvis && b.turn == Some(turn))
        {
            if !bubble.text.ends_with('…') {
                bubble.text.push('…');
            }
            bubble.interrupted = true;
            bubble.sealed_at = Some(now);
        }
    }

    /// Inserta una burbuja desalojando la más antigua si la pila está llena.
    fn push(&mut self, bubble: Bubble, now: Instant) {
        if self.bubbles.len() >= self.max {
            if let Some(oldest) = self.bubbles.iter_mut().find(|b| b.evicted_at.is_none()) {
                oldest.evicted_at = Some(now);
            }
        }
        self.bubbles.push_back(bubble);
    }

    /// Burbujas vivas con su opacidad en `now`.
    ///
    /// Incluye las que están en su ventana de entrada aunque su opacidad aún
    /// sea `0.0` (recién nacidas): así ocupan su sitio en el layout desde el
    /// primer frame. Solo se excluyen las que ya terminaron de desvanecerse.
    pub fn visible(&self, now: Instant) -> impl Iterator<Item = (&Bubble, f32)> {
        self.bubbles
            .iter()
            .filter(move |b| !b.expired(now))
            .map(move |b| (b, b.alpha(now)))
            .filter(move |(b, alpha)| *alpha > 0.0 || now < b.born + FADE_IN)
    }

    /// Retira las burbujas cuyo fundido de salida ya terminó.
    pub fn gc(&mut self, now: Instant) {
        self.bubbles.retain(|b| !b.expired(now));
    }

    /// `true` si hay burbujas entrando o saliendo (el bucle debe presentar).
    pub fn is_animating(&self, now: Instant) -> bool {
        self.bubbles.iter().any(|b| b.animating(now))
    }
}

/// Glifo rasterizado cacheado.
struct Glyph {
    metrics: fontdue::Metrics,
    bitmap: Vec<u8>,
}

/// Caché de glifos. Acotada: al superar 512 glifos se limpia (evita crecer sin
/// límite con emojis/caracteres raros sin sacrificar el coste por frame).
const GLYPH_CACHE_CAP: usize = 512;

/// Rasterizador de texto en CPU sobre la fuente Noto Sans embebida.
pub struct TextRasterizer {
    font: fontdue::Font,
    px: f32,
    line_height: f32,
    ascent: f32,
    cache: HashMap<(char, u32), Glyph>,
}

impl TextRasterizer {
    /// Carga la fuente embebida y prepara el rasterizador a `font_px`.
    pub fn new(font_px: f32) -> Result<Self, RenderError> {
        let font_bytes: &[u8] = include_bytes!("../assets/NotoSans-Regular.ttf");
        let font = fontdue::Font::from_bytes(font_bytes, fontdue::FontSettings::default())
            .map_err(|_| RenderError::FontLoadFailed)?;
        let line_metrics = font.horizontal_line_metrics(font_px);
        let line_height = line_metrics
            .as_ref()
            .map(|m| m.new_line_size)
            .unwrap_or(font_px * 1.4);
        let ascent = line_metrics
            .as_ref()
            .map(|m| m.ascent)
            .unwrap_or(font_px * 0.8);
        Ok(Self {
            font,
            px: font_px,
            line_height,
            ascent,
            cache: HashMap::new(),
        })
    }

    /// Ancho en píxeles lógicos de una línea de texto.
    pub fn measure(&self, text: &str) -> f32 {
        text.chars()
            .map(|c| self.font.metrics(c, self.px).advance_width)
            .sum()
    }

    /// Alto de línea en píxeles lógicos.
    pub fn line_height(&self) -> f32 {
        self.line_height
    }

    /// Ascenso de la fuente (para alinear la primera línea).
    fn ascent(&self) -> f32 {
        self.ascent
    }

    /// Divide `text` en líneas que no exceden `max_width`, troceando con guion
    /// las palabras que solas no caben.
    pub fn wrap(&self, text: &str, max_width: f32) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();

        for word in text.split_whitespace() {
            if self.measure(word) > max_width {
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                }
                lines.extend(self.break_word(word, max_width));
                continue;
            }

            let candidate = if current.is_empty() {
                word.to_string()
            } else {
                format!("{current} {word}")
            };
            if self.measure(&candidate) <= max_width {
                current = candidate;
            } else {
                lines.push(std::mem::take(&mut current));
                current = word.to_string();
            }
        }

        if !current.is_empty() {
            lines.push(current);
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        lines
    }

    /// Igual que [`TextRasterizer::wrap`] pero conservando solo las últimas
    /// `max_lines` líneas (efecto teleprompter del HUD).
    pub fn wrap_lines(&self, text: &str, max_width: f32, max_lines: usize) -> Vec<String> {
        if max_lines == 0 {
            return Vec::new();
        }
        let mut lines = self.wrap(text, max_width);
        if lines.len() > max_lines {
            lines = lines.split_off(lines.len() - max_lines);
        }
        lines
    }

    /// Trocea una palabra larga en trozos que caben, con guion intermedio.
    fn break_word(&self, word: &str, max_width: f32) -> Vec<String> {
        let mut pieces = Vec::new();
        let mut current = String::new();
        let hyphen_width = self.measure("-");

        for ch in word.chars() {
            let mut candidate = current.clone();
            candidate.push(ch);
            if !current.is_empty() && self.measure(&candidate) + hyphen_width > max_width {
                current.push('-');
                pieces.push(std::mem::take(&mut current));
            }
            current.push(ch);
        }
        if !current.is_empty() {
            pieces.push(current);
        }
        pieces
    }

    /// Devuelve métricas y bitmap de un glifo, cacheado por `(char, px)`.
    fn glyph_data(&mut self, ch: char) -> (fontdue::Metrics, &[u8]) {
        let key = (ch, self.px.to_bits());
        if !self.cache.contains_key(&key) {
            if self.cache.len() >= GLYPH_CACHE_CAP {
                self.cache.clear();
            }
            let (metrics, bitmap) = self.font.rasterize(ch, self.px);
            self.cache.insert(key, Glyph { metrics, bitmap });
        }
        let glyph = self.cache.get(&key).expect("glifo recién cacheado");
        (glyph.metrics, glyph.bitmap.as_slice())
    }

    /// Dibuja una línea de texto en blanco con opacidad `alpha` (0..=255),
    /// componiendo source-over sobre el contenido existente del `Pixmap`.
    fn draw_line(&mut self, pixmap: &mut Pixmap, text: &str, x: f32, baseline_y: f32, alpha: u8) {
        if alpha == 0 || text.is_empty() {
            return;
        }
        let baseline_y = baseline_y.round() as i32;
        let mut pen_x = x;

        for ch in text.chars() {
            let (metrics, bitmap) = self.glyph_data(ch);
            let draw_x = pen_x.round() as i32 + metrics.xmin;
            let draw_y = baseline_y - metrics.ymin - metrics.height as i32;

            for row in 0..metrics.height {
                let y = draw_y + row as i32;
                if y < 0 || y >= pixmap.height() as i32 {
                    continue;
                }
                let row_base = row * metrics.width;
                for col in 0..metrics.width {
                    let coverage = bitmap[row_base + col];
                    if coverage == 0 {
                        continue;
                    }
                    let x = draw_x + col as i32;
                    if x < 0 || x >= pixmap.width() as i32 {
                        continue;
                    }
                    let a = (u32::from(coverage) * u32::from(alpha) / 255) as u8;
                    blend_white_pixel(pixmap, x as u32, y as u32, a);
                }
            }
            pen_x += metrics.advance_width;
        }
    }
}

/// Compone un píxel blanco premultiplicado `(a, a, a, a)` sobre `dst`.
fn blend_white_pixel(pixmap: &mut Pixmap, x: u32, y: u32, a: u8) {
    let idx = ((y * pixmap.width() + x) * 4) as usize;
    let data = pixmap.data_mut();
    if idx + 3 >= data.len() {
        return;
    }
    let inv = 255 - u32::from(a);
    let dr = u32::from(data[idx]);
    let dg = u32::from(data[idx + 1]);
    let db = u32::from(data[idx + 2]);
    let da = u32::from(data[idx + 3]);
    data[idx] = (u32::from(a) + dr * inv / 255) as u8;
    data[idx + 1] = (u32::from(a) + dg * inv / 255) as u8;
    data[idx + 2] = (u32::from(a) + db * inv / 255) as u8;
    data[idx + 3] = (u32::from(a) + da * inv / 255) as u8;
}

/// Ancho máximo del texto dentro de una burbuja, en píxeles lógicos.
const MAX_TEXT_WIDTH: f32 = 280.0;

/// Máximo de líneas visibles por burbuja (teleprompter).
const MAX_LINES: usize = 3;

/// Relleno interior de la burbuja.
const BUBBLE_PADDING: f32 = 10.0;

/// Separación vertical entre burbujas.
const BUBBLE_GAP: f32 = 8.0;

/// Radio de las esquinas redondeadas.
const CORNER_RADIUS: f32 = 12.0;

/// Alto máximo de la superficie del HUD.
const MAX_HUD_HEIGHT: u32 = 260;

/// Ancho máximo de la superficie del HUD.
const MAX_HUD_WIDTH: u32 = 320;

/// Opacidad del fondo negro de la burbuja (55 %).
const BACKGROUND_ALPHA: f32 = 0.55;

/// Renderizador del HUD: mide y dibuja [`BubbleStack`] sobre un `Pixmap`.
pub struct HudRenderer {
    raster: TextRasterizer,
}

impl HudRenderer {
    pub fn new(font_px: f32) -> Result<Self, RenderError> {
        Ok(Self {
            raster: TextRasterizer::new(font_px)?,
        })
    }

    /// Ancho y alto en píxeles necesarios para el stack actual. `(0, 0)` si no
    /// hay burbujas visibles.
    pub fn measure(&self, stack: &BubbleStack, now: Instant) -> (u32, u32) {
        let mut total_height = 0.0f32;
        let mut max_width = 0.0f32;
        let mut count = 0u32;

        for (bubble, _) in stack.visible(now) {
            let lines = self
                .raster
                .wrap_lines(&bubble.text, MAX_TEXT_WIDTH, MAX_LINES);
            let text_width = lines
                .iter()
                .map(|l| self.raster.measure(l))
                .fold(0.0f32, f32::max);
            let bubble_width = text_width + 2.0 * BUBBLE_PADDING;
            let bubble_height =
                lines.len() as f32 * self.raster.line_height() + 2.0 * BUBBLE_PADDING;

            max_width = max_width.max(bubble_width);
            total_height += bubble_height;
            count += 1;
        }

        if count == 0 {
            return (0, 0);
        }
        total_height += BUBBLE_GAP * (count - 1) as f32;

        let width = (max_width.ceil() as u32).clamp(1, MAX_HUD_WIDTH);
        let height = (total_height.ceil() as u32).clamp(1, MAX_HUD_HEIGHT);
        (width, height)
    }

    /// Dibuja el stack sobre `pixmap` (se limpia al inicio).
    ///
    /// El texto se dibuja en blanco con una opacidad mínima de `max(min_alpha,
    /// 60 %)`, modulada por la opacidad de animación de cada burbuja.
    pub fn render_into(
        &mut self,
        stack: &BubbleStack,
        pixmap: &mut Pixmap,
        min_alpha: Opacity,
        now: Instant,
    ) {
        pixmap.fill(Color::TRANSPARENT);

        let surface_width = pixmap.width() as f32;
        let min_fraction = f32::from(min_alpha.as_u8()) / 255.0;
        let mut y = 0.0f32;

        // Las burbujas con opacidad 0 (recién nacidas) ocupan su sitio igual
        // que en `measure`; `fill_rounded_rect` y `draw_line` no pintan nada.
        for (bubble, alpha) in stack.visible(now) {
            let lines = self
                .raster
                .wrap_lines(&bubble.text, MAX_TEXT_WIDTH, MAX_LINES);
            let text_width = lines
                .iter()
                .map(|l| self.raster.measure(l))
                .fold(0.0f32, f32::max);
            let bubble_width = text_width + 2.0 * BUBBLE_PADDING;
            let bubble_height =
                lines.len() as f32 * self.raster.line_height() + 2.0 * BUBBLE_PADDING;

            let x = match bubble.speaker {
                Speaker::User => (surface_width - bubble_width).max(0.0),
                Speaker::Jarvis | Speaker::System => 0.0,
            };

            let bg_byte = (BACKGROUND_ALPHA * alpha * 255.0).round().clamp(0.0, 255.0) as u8;
            fill_rounded_rect(
                pixmap,
                x,
                y,
                bubble_width,
                bubble_height,
                CORNER_RADIUS,
                bg_byte,
            );

            let text_opacity = min_fraction.max(0.6) * alpha;
            let text_byte = (text_opacity * 255.0).round().clamp(0.0, 255.0) as u8;
            let mut baseline = y + BUBBLE_PADDING + self.raster.ascent();
            for line in &lines {
                self.raster
                    .draw_line(pixmap, line, x + BUBBLE_PADDING, baseline, text_byte);
                baseline += self.raster.line_height();
            }

            y += bubble_height + BUBBLE_GAP;
        }
    }
}

/// Rellena un rectángulo redondeado negro con la opacidad indicada.
fn fill_rounded_rect(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, alpha: u8) {
    if alpha == 0 || w <= 0.0 || h <= 0.0 {
        return;
    }
    let mut paint = Paint::default();
    paint.set_color_rgba8(0, 0, 0, alpha);
    paint.anti_alias = true;
    if let Some(path) = rounded_rect_path(x, y, w, h, r) {
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// Construye la trayectoria de un rectángulo con esquinas redondeadas.
fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = r.clamp(0.0, (w / 2.0).min(h / 2.0));
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn bubble_ttl_clamps_to_contract() {
        assert_eq!(bubble_ttl(0), Duration::from_secs(4));
        assert_eq!(bubble_ttl(10), Duration::from_secs(4));
        assert_eq!(bubble_ttl(100), Duration::from_millis(6_000));
        assert_eq!(bubble_ttl(1_000), Duration::from_secs(12));
    }

    #[test]
    fn user_bubble_is_sealed_on_birth_and_fades_in() {
        let mut stack = BubbleStack::new(3);
        let now = base();
        stack.push_user(7, "qué hora es", now);

        assert_eq!(stack.len(), 1);
        let (bubble, alpha) = stack.visible(now).next().unwrap();
        assert_eq!(bubble.speaker, Speaker::User);
        assert_eq!(bubble.turn, Some(7));
        assert_eq!(bubble.sealed_at, Some(now));
        assert!(alpha < 1.0, "recién nacida debe estar entrando");
        assert!(stack.is_animating(now));
    }

    #[test]
    fn user_fragments_merge_on_same_turn_and_renew_ttl() {
        let mut stack = BubbleStack::new(3);
        let now = base();
        stack.push_user(7, "hola", now);

        let later = now + Duration::from_secs(10);
        stack.push_user(7, "mundo", later);

        assert_eq!(stack.len(), 1, "el fragmento no debe duplicar la burbuja");
        let bubble = stack.bubbles.front().unwrap();
        assert_eq!(bubble.text, "hola mundo");
        assert_eq!(bubble.sealed_at, Some(later), "el TTL debe renovarse");
        assert!(bubble.evicted_at.is_none());

        // El TTL renovado la mantiene viva aunque el original ya habría vencido.
        let ttl = bubble_ttl(bubble.text.chars().count());
        let before_expiry = later + ttl - Duration::from_millis(1);
        assert!(!stack.is_animating(before_expiry));
        stack.gc(before_expiry);
        assert_eq!(stack.len(), 1, "el TTL renovado debe mantenerla viva");
    }

    #[test]
    fn jarvis_phrases_merge_on_same_open_turn() {
        let mut stack = BubbleStack::new(3);
        let now = base();
        stack.push_jarvis(3, "hola", now);
        stack.push_jarvis(3, "mundo", now);

        assert_eq!(stack.len(), 1);
        let (bubble, _) = stack.visible(now).next().unwrap();
        assert_eq!(bubble.text, "hola mundo");
        assert_eq!(bubble.sealed_at, None);
    }

    #[test]
    fn late_phrase_of_a_sealed_turn_is_dropped() {
        let mut stack = BubbleStack::new(3);
        let now = base();
        stack.push_jarvis(4, "hola", now);
        stack.interrupt(4, now);
        stack.push_jarvis(4, "tarde", now);
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.bubbles.front().unwrap().text, "hola…");
    }

    #[test]
    fn newborn_bubble_takes_layout_space() {
        let mut stack = BubbleStack::new(3);
        let now = base();
        stack.push_user(1, "hola", now);
        let renderer = HudRenderer::new(16.0).unwrap();
        let (w, h) = renderer.measure(&stack, now);
        assert!(w > 0 && h > 0, "la recién nacida debe medir: {w}x{h}");
    }

    #[test]
    fn different_turns_do_not_merge() {
        let mut stack = BubbleStack::new(3);
        let now = base();
        stack.push_jarvis(1, "primera", now);
        stack.push_jarvis(2, "segunda", now);
        assert_eq!(stack.len(), 2);
    }

    #[test]
    fn seal_marks_the_jarvis_turn_done() {
        let mut stack = BubbleStack::new(3);
        let now = base();
        stack.push_jarvis(2, "hola", now);
        stack.seal(2, now);
        assert_eq!(stack.bubbles.front().unwrap().sealed_at, Some(now));
    }

    #[test]
    fn interrupt_appends_ellipsis_and_seals() {
        let mut stack = BubbleStack::new(3);
        let now = base();
        stack.push_jarvis(5, "respuesta", now);
        stack.interrupt(5, now + Duration::from_millis(10));

        let bubble = stack.bubbles.front().unwrap();
        assert!(bubble.interrupted);
        assert!(bubble.text.ends_with('…'));
        assert!(bubble.sealed_at.is_some());
    }

    #[test]
    fn fourth_bubble_evicts_oldest_with_fade() {
        let mut stack = BubbleStack::new(3);
        let now = base();
        for i in 0u64..3 {
            stack.push_user(i, format!("mensaje {i}"), now);
        }
        assert_eq!(stack.len(), 3);

        stack.push_user(9, "cuarto", now);
        assert_eq!(stack.len(), 4); // incluye al desalojado en fundido de salida
        assert!(stack.bubbles.front().unwrap().evicted_at.is_some());

        // Tras el fundido de desalojo, `gc` retira al desalojado.
        stack.gc(now + FADE_OUT_EVICT);
        assert_eq!(stack.len(), 3);
    }

    #[test]
    fn ttl_expires_then_gc_removes() {
        let mut stack = BubbleStack::new(3);
        let now = base();
        stack.push_user(1, "abc", now); // 3 chars → TTL 4 s

        let fading = now + Duration::from_secs(4) + Duration::from_millis(399);
        assert!(!stack.is_empty());
        assert!(stack.is_animating(fading), "debe estar desvaneciéndose");
        stack.gc(fading);
        assert!(!stack.is_empty(), "aún no venció el fundido");

        let expired = now + Duration::from_secs(4) + Duration::from_millis(400);
        stack.gc(expired);
        assert!(stack.is_empty());
    }

    #[test]
    fn is_animating_false_at_rest() {
        let mut stack = BubbleStack::new(3);
        let now = base();
        stack.push_user(1, "hola", now);
        let settled = now + Duration::from_millis(200);
        assert!(!stack.is_animating(settled));
    }

    #[test]
    fn clear_empties_the_stack() {
        let mut stack = BubbleStack::new(3);
        stack.push_user(1, "hola", base());
        stack.clear();
        assert!(stack.is_empty());
    }

    fn raster() -> TextRasterizer {
        TextRasterizer::new(16.0).unwrap()
    }

    #[test]
    fn wrap_splits_words_to_fit_width() {
        let r = raster();
        // Un ancho que deja entrar una sola de las dos palabras, pero no ambas.
        let single = r.measure("uno").max(r.measure("dos"));
        let lines = r.wrap("uno dos", single + 1.0);
        assert_eq!(lines.join("|"), "uno|dos");
    }

    #[test]
    fn wrap_hyphenates_overlong_words() {
        let r = raster();
        let word = "supercalifragilisticoespialidoso";
        let lines = r.wrap(word, 60.0);
        assert!(
            lines.len() > 1,
            "la palabra larga debe trocearse: {lines:?}"
        );
        for line in &lines[..lines.len() - 1] {
            assert!(line.ends_with('-'), "trozo intermedio sin guion: {line}");
        }
        assert!(!lines.last().unwrap().ends_with('-'));
    }

    #[test]
    fn tildes_and_enye_measure_positive() {
        let r = raster();
        assert!(r.measure("ñandú") > 0.0);
        let lines = r.wrap("café ñandú", 200.0);
        assert_eq!(lines.join(" "), "café ñandú");
    }

    #[test]
    fn measure_reports_bubble_size_and_render_draws() {
        let mut stack = BubbleStack::new(3);
        let now = base();
        stack.push_user(1, "hola", now);

        let mut renderer = HudRenderer::new(16.0).unwrap();
        let settled = now + Duration::from_millis(200);
        let (w, h) = renderer.measure(&stack, settled);
        assert!(w > 0 && h > 0, "dimensión inválida: {w}x{h}");

        let mut pixmap = Pixmap::new(w, h).unwrap();
        renderer.render_into(&stack, &mut pixmap, Opacity::OPAQUE, settled);

        let mut background_found = false;
        let mut text_found = false;
        for px in pixmap.data().as_chunks::<4>().0 {
            if px[3] >= 130 && px[0] == 0 {
                background_found = true;
            }
            if px[0] > 0 && px[3] > 140 {
                text_found = true;
            }
        }
        assert!(background_found, "fondo negro de la burbuja ausente");
        assert!(text_found, "texto blanco ausente");
    }

    #[test]
    fn render_empty_stack_is_fully_transparent() {
        let stack = BubbleStack::new(3);
        let mut renderer = HudRenderer::new(16.0).unwrap();
        let mut pixmap = Pixmap::new(320, 260).unwrap();
        renderer.render_into(&stack, &mut pixmap, Opacity::OPAQUE, base());
        for px in pixmap.data().as_chunks::<4>().0 {
            assert_eq!(px[3], 0, "sin burbujas el HUD debe ser transparente");
        }
    }
}
