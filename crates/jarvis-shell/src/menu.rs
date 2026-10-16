//! Menú contextual de la esfera (clic derecho).
//!
//! Lógica pura e independiente del backend: disposición de las opciones,
//! detección de la opción bajo el puntero y dibujo sobre un buffer
//! `Argb8888` (BGRA en memoria). El texto usa una fuente bitmap 5x7 embebida
//! para no sumar dependencias de tipografía al overlay.

/// Opción del menú contextual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    /// Silencia o reactiva el micrófono.
    ToggleMute,
    /// Abre el formulario de configuración (cerebro CLI/API, barge-in).
    Configure,
    /// Abre el selector de voces de Edge-TTS.
    ChangeVoice,
    /// Devuelve la esfera a su esquina de anclaje original.
    ResetPosition,
    /// Cierra Fokkus-Jarvis.
    Quit,
}

impl MenuItem {
    /// Opciones en el orden en que se muestran.
    pub const ALL: [MenuItem; 5] = [
        MenuItem::ToggleMute,
        MenuItem::Configure,
        MenuItem::ChangeVoice,
        MenuItem::ResetPosition,
        MenuItem::Quit,
    ];

    /// Texto de la opción (en mayúsculas y sin tildes: lo exige la fuente).
    pub fn label(self, muted: bool) -> &'static str {
        match self {
            MenuItem::ToggleMute if muted => "ACTIVAR MICROFONO",
            MenuItem::ToggleMute => "SILENCIAR",
            MenuItem::Configure => "CONFIGURACION",
            MenuItem::ChangeVoice => "CAMBIAR VOZ",
            MenuItem::ResetPosition => "POSICION INICIAL",
            MenuItem::Quit => "SALIR",
        }
    }
}

/// Escala de la fuente 5x7 (cada punto ocupa `SCALE x SCALE` píxeles).
const SCALE: u32 = 2;
/// Avance horizontal por carácter: 5 columnas más 1 de separación.
const GLYPH_ADVANCE: u32 = 6 * SCALE;
const GLYPH_HEIGHT: u32 = 7 * SCALE;
const PADDING_X: u32 = 14;
const PADDING_Y: u32 = 4;
const ITEM_HEIGHT: u32 = 34;
/// Caracteres de la etiqueta más larga ("ACTIVAR MICROFONO").
const MAX_LABEL_CHARS: u32 = 17;

/// Ancho del menú en píxeles.
pub const MENU_WIDTH: u32 = MAX_LABEL_CHARS * GLYPH_ADVANCE + 2 * PADDING_X;
/// Alto del menú en píxeles.
pub const MENU_HEIGHT: u32 = MenuItem::ALL.len() as u32 * ITEM_HEIGHT + 2 * PADDING_Y;

/// Colores `(r, g, b)` del tema oscuro.
const BACKGROUND: (u8, u8, u8) = (0x18, 0x18, 0x26);
const BORDER: (u8, u8, u8) = (0x33, 0x33, 0x40);
const HOVER: (u8, u8, u8) = (0x2A, 0x2A, 0x3A);
const TEXT: (u8, u8, u8) = (0xD6, 0xD6, 0xE2);
const TEXT_HOVER: (u8, u8, u8) = (0x37, 0xEC, 0xBA);
const TEXT_DANGER: (u8, u8, u8) = (0xF8, 0x71, 0x71);

/// Estado visual del menú abierto.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextMenu {
    /// El micrófono está silenciado (cambia la etiqueta de la primera opción).
    pub muted: bool,
    /// Opción bajo el puntero.
    pub hovered: Option<MenuItem>,
}

impl ContextMenu {
    pub fn new(muted: bool) -> Self {
        Self {
            muted,
            hovered: None,
        }
    }

    /// Opción en la posición local `(x, y)` del menú, si la hay.
    pub fn item_at(x: f64, y: f64) -> Option<MenuItem> {
        if x < 0.0 || x >= f64::from(MENU_WIDTH) || y < f64::from(PADDING_Y) {
            return None;
        }
        // Truncar es correcto: `y` ya es ≥ PADDING_Y.
        let index = ((y - f64::from(PADDING_Y)) / f64::from(ITEM_HEIGHT)) as usize;
        MenuItem::ALL.get(index).copied()
    }

    /// Actualiza la opción resaltada; devuelve si cambió (hay que redibujar).
    pub fn hover(&mut self, item: Option<MenuItem>) -> bool {
        let changed = self.hovered != item;
        self.hovered = item;
        changed
    }

    /// Dibuja el menú opaco en `canvas` (`Argb8888`, `MENU_WIDTH x MENU_HEIGHT`).
    ///
    /// Devuelve `false` (sin tocar nada) si el buffer no tiene ese tamaño.
    pub fn render(&self, canvas: &mut [u8]) -> bool {
        let mut target = Canvas { data: canvas };
        if target.data.len() != (MENU_WIDTH * MENU_HEIGHT * 4) as usize {
            return false;
        }
        target.fill_rect(0, 0, MENU_WIDTH, MENU_HEIGHT, BORDER);
        target.fill_rect(1, 1, MENU_WIDTH - 2, MENU_HEIGHT - 2, BACKGROUND);
        for (i, item) in MenuItem::ALL.into_iter().enumerate() {
            let top = PADDING_Y + i as u32 * ITEM_HEIGHT;
            let hovered = self.hovered == Some(item);
            if hovered {
                target.fill_rect(2, top, MENU_WIDTH - 4, ITEM_HEIGHT, HOVER);
            }
            let color = match item {
                MenuItem::Quit => TEXT_DANGER,
                _ if hovered => TEXT_HOVER,
                _ => TEXT,
            };
            let text_y = top + (ITEM_HEIGHT - GLYPH_HEIGHT) / 2;
            target.draw_text(PADDING_X, text_y, item.label(self.muted), color);
        }
        true
    }
}

/// Vista mutable sobre un buffer `Argb8888` de `MENU_WIDTH x MENU_HEIGHT`.
struct Canvas<'a> {
    data: &'a mut [u8],
}

impl Canvas<'_> {
    fn put(&mut self, x: u32, y: u32, (r, g, b): (u8, u8, u8)) {
        if x >= MENU_WIDTH || y >= MENU_HEIGHT {
            return;
        }
        let i = ((y * MENU_WIDTH + x) * 4) as usize;
        self.data[i..i + 4].copy_from_slice(&[b, g, r, 0xFF]);
    }

    fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: (u8, u8, u8)) {
        for py in y..y + h {
            for px in x..x + w {
                self.put(px, py, color);
            }
        }
    }

    fn draw_text(&mut self, x: u32, y: u32, text: &str, color: (u8, u8, u8)) {
        for (i, ch) in text.chars().enumerate() {
            let origin = x + i as u32 * GLYPH_ADVANCE;
            for (row, bits) in glyph(ch).iter().enumerate() {
                for col in 0..5u32 {
                    if bits & (0b10000 >> col) != 0 {
                        let px = origin + col * SCALE;
                        let py = y + row as u32 * SCALE;
                        self.fill_rect(px, py, SCALE, SCALE, color);
                    }
                }
            }
        }
    }
}

/// Filas de la fuente 5x7 (bit 4 = columna izquierda). Solo las letras que
/// usan las etiquetas; cualquier otro carácter se dibuja en blanco.
fn glyph(ch: char) -> [u8; 7] {
    match ch {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        _ => [0; 7],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(canvas: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * MENU_WIDTH + x) * 4) as usize;
        [canvas[i], canvas[i + 1], canvas[i + 2], canvas[i + 3]]
    }

    fn bgra((r, g, b): (u8, u8, u8)) -> [u8; 4] {
        [b, g, r, 0xFF]
    }

    #[test]
    fn every_label_fits_and_has_glyphs() {
        for muted in [false, true] {
            for item in MenuItem::ALL {
                let label = item.label(muted);
                assert!(label.chars().count() as u32 <= MAX_LABEL_CHARS, "{label}");
                for ch in label.chars().filter(|c| *c != ' ') {
                    assert_ne!(glyph(ch), [0; 7], "falta el glifo «{ch}» en «{label}»");
                }
            }
        }
    }

    #[test]
    fn mute_label_reflects_state() {
        assert_eq!(MenuItem::ToggleMute.label(false), "SILENCIAR");
        assert_eq!(MenuItem::ToggleMute.label(true), "ACTIVAR MICROFONO");
    }

    #[test]
    fn item_at_maps_rows_and_rejects_margins() {
        let mid_x = f64::from(MENU_WIDTH) / 2.0;
        let row = |i: u32| f64::from(PADDING_Y + i * ITEM_HEIGHT + ITEM_HEIGHT / 2);
        assert_eq!(
            ContextMenu::item_at(mid_x, row(0)),
            Some(MenuItem::ToggleMute)
        );
        assert_eq!(
            ContextMenu::item_at(mid_x, row(1)),
            Some(MenuItem::Configure)
        );
        assert_eq!(
            ContextMenu::item_at(mid_x, row(2)),
            Some(MenuItem::ChangeVoice)
        );
        assert_eq!(
            ContextMenu::item_at(mid_x, row(3)),
            Some(MenuItem::ResetPosition)
        );
        assert_eq!(ContextMenu::item_at(mid_x, row(4)), Some(MenuItem::Quit));
        // Margen superior (donde aparece el puntero al abrir) y fuera del menú.
        assert_eq!(ContextMenu::item_at(0.0, 0.0), None);
        assert_eq!(ContextMenu::item_at(mid_x, f64::from(MENU_HEIGHT)), None);
        assert_eq!(ContextMenu::item_at(-1.0, row(0)), None);
        assert_eq!(ContextMenu::item_at(f64::from(MENU_WIDTH), row(0)), None);
    }

    #[test]
    fn hover_reports_changes_only() {
        let mut menu = ContextMenu::new(false);
        assert!(menu.hover(Some(MenuItem::Quit)));
        assert!(!menu.hover(Some(MenuItem::Quit)));
        assert!(menu.hover(None));
        assert!(!menu.hover(None));
    }

    #[test]
    fn render_draws_opaque_menu_with_hover() {
        let mut canvas = vec![0u8; (MENU_WIDTH * MENU_HEIGHT * 4) as usize];
        let mut menu = ContextMenu::new(false);
        menu.hover(Some(MenuItem::ResetPosition));
        assert!(menu.render(&mut canvas));

        // Todo el menú es opaco y tiene borde.
        assert!(canvas.as_chunks::<4>().0.iter().all(|px| px[3] == 0xFF));
        assert_eq!(pixel(&canvas, 0, 0), bgra(BORDER));
        // Fondo normal en la primera fila y resaltado en la fila 3
        // (índice 3), donde está ResetPosition tras añadir CambiarVoz.
        let first_top = PADDING_Y + 1;
        let reset_top = PADDING_Y + 3 * ITEM_HEIGHT + 1;
        assert_eq!(pixel(&canvas, MENU_WIDTH - 4, first_top), bgra(BACKGROUND));
        assert_eq!(pixel(&canvas, MENU_WIDTH - 4, reset_top), bgra(HOVER));
        // La primera fila de la «S» de "SILENCIAR" tiene tinta en la columna 1.
        let text_y = PADDING_Y + (ITEM_HEIGHT - GLYPH_HEIGHT) / 2;
        assert_eq!(pixel(&canvas, PADDING_X + SCALE, text_y), bgra(TEXT));
        assert_eq!(pixel(&canvas, PADDING_X, text_y), bgra(BACKGROUND));
    }

    #[test]
    fn render_rejects_wrong_buffer_size() {
        let mut canvas = vec![7u8; 16];
        assert!(!ContextMenu::new(false).render(&mut canvas));
        assert!(canvas.iter().all(|b| *b == 7));
    }
}
