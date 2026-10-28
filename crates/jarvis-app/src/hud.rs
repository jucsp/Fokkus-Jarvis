//! HUD de burbujas de conversación (HU-36): de los eventos del orquestador a
//! la capa del shell.
//!
//! [`HudPipeline`] drena [`HudEvent`] sin bloquear, alimenta la
//! [`BubbleStack`], mide la altura necesaria y presenta en la capa del HUD
//! solo cuando algo cambió o hay un fundido en curso. Sin globos la capa se
//! oculta (0 frames); con el HUD desactivado ni siquiera existe (0 bytes).

use std::time::Instant;

use crossbeam_channel::Receiver;
use jarvis_audio::{HudEvent, TaskStatus};
use jarvis_core::{JarvisConfig, Opacity};
use jarvis_render::{BubbleStack, ChipState, HudRenderer, Pixmap, SessionChip};
use jarvis_shell::{FrameBuffer, OverlaySurface, SurfaceSize};

/// Ancho fijo de la capa: los globos del usuario se alinean a la derecha y
/// los de Jarvis a la izquierda dentro de él.
pub const HUD_WIDTH: u32 = 320;

/// Tamaño de letra de los globos, en píxeles lógicos.
const HUD_FONT_PX: f32 = 15.0;

/// Traduce el ciclo de vida de una tarea al estado que pinta su chip (HU-67).
///
/// `Queued` y `Running` tienen chip propio para distinguir "en cola" de "en
/// marcha"; `Failed` y `TimedOut` comparten el rojo porque para el usuario son
/// el mismo desenlace.
fn chip_state(status: &TaskStatus) -> ChipState {
    match status {
        TaskStatus::Queued => ChipState::Queued,
        TaskStatus::Running => ChipState::Working,
        TaskStatus::Done => ChipState::Done,
        TaskStatus::Failed | TaskStatus::TimedOut => ChipState::Failed,
        TaskStatus::Cancelled => ChipState::Cancelled,
    }
}

/// Ajustes del HUD que la app toma de `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HudSettings {
    pub enabled: bool,
    pub max_bubbles: u8,
}

impl HudSettings {
    pub fn from_config(config: &JarvisConfig) -> Self {
        Self {
            enabled: config.hud_enabled(),
            max_bubbles: config.hud_max_bubbles(),
        }
    }
}

impl Default for HudSettings {
    fn default() -> Self {
        Self::from_config(&JarvisConfig::default())
    }
}

/// Contexto del frame que decide si se aceptan globos nuevos (CA-36.7).
#[derive(Debug, Clone, Copy)]
pub struct HudFrame {
    pub settings: HudSettings,
    /// `true` con la esfera atenuada/oculta por pantalla completa fuera de un
    /// turno activo, o en silencio: no se crean globos nuevos.
    pub suppress_new: bool,
    /// Opacidad efectiva de la esfera: el texto usa `max(esto, 60 %)`.
    pub text_alpha: Opacity,
}

/// Estado del HUD en el bucle principal.
pub struct HudPipeline {
    events: Receiver<HudEvent>,
    stack: BubbleStack,
    /// Chips de sub-sesión sobre los globos (HU-67). Se rehacen solo cuando
    /// llega un [`HudEvent::Sessions`], nunca por frame.
    chips: Vec<SessionChip>,
    renderer: Option<HudRenderer>,
    pixmap: Option<Pixmap>,
    settings: Option<HudSettings>,
    /// Hubo eventos aplicados desde el último frame presentado.
    dirty: bool,
    /// La capa del HUD está mapeada.
    shown: bool,
    /// Ya se avisó de un fallo del HUD (se avisa una vez).
    error_logged: bool,
}

impl HudPipeline {
    pub fn new(events: Receiver<HudEvent>) -> Self {
        Self {
            events,
            stack: BubbleStack::new(3),
            chips: Vec::new(),
            renderer: None,
            pixmap: None,
            settings: None,
            dirty: false,
            shown: false,
            error_logged: false,
        }
    }

    /// Número de globos vivos (tests).
    #[cfg(test)]
    pub fn bubbles(&self) -> usize {
        self.stack.len()
    }

    /// Procesa un frame del bucle. Devuelve `true` si presentó en el HUD.
    pub fn frame(&mut self, surface: &mut dyn OverlaySurface, ctx: HudFrame, now: Instant) -> bool {
        self.apply_settings(surface, ctx.settings);
        self.drain(ctx, now);
        self.stack.gc(now);

        if !ctx.settings.enabled {
            return false;
        }
        let Some(hud) = surface.hud() else {
            // Sin capa (backend sin soporte o el compositor la cerró).
            return false;
        };

        // Sin globos **ni chips** la capa se oculta (0 frames). Con
        // sub-sesiones vivas los chips se muestran aunque no haya burbujas.
        if self.stack.is_empty() && self.chips.is_empty() {
            if self.shown {
                hud.set_visible(false);
                self.shown = false;
            }
            self.dirty = false;
            return false;
        }
        if !self.shown {
            hud.set_visible(true);
            self.shown = true;
            self.dirty = true;
        }
        if !self.dirty && !self.stack.is_animating(now) {
            return false;
        }
        if !hud.wants_frame() {
            // Esperando `configure` o el frame callback: se reintenta luego.
            return false;
        }

        let Some(height) = self
            .renderer
            .as_ref()
            .map(|renderer| renderer.measure_with_chips(&self.chips, &self.stack, now).1)
        else {
            return false;
        };
        let Ok(size) = SurfaceSize::new(HUD_WIDTH, height.max(1)) else {
            return false;
        };
        let pixmap_fits = self
            .pixmap
            .as_ref()
            .is_some_and(|p| p.width() == size.width && p.height() == size.height);
        if !pixmap_fits {
            self.pixmap = Pixmap::new(size.width, size.height);
        }
        if hud.size() != size {
            if let Err(e) = hud.set_size(size) {
                self.report(&e);
                return false;
            }
            if !hud.wants_frame() {
                return false;
            }
        }
        let (Some(renderer), Some(pixmap)) = (self.renderer.as_mut(), self.pixmap.as_mut()) else {
            return false;
        };
        renderer.render_with_chips(&self.chips, &self.stack, pixmap, ctx.text_alpha, now);
        let result = FrameBuffer::new(pixmap.data(), pixmap.width(), pixmap.height())
            .and_then(|frame| hud.present(&frame));
        match result {
            Ok(()) => {
                self.dirty = false;
                true
            }
            Err(e) => {
                self.report(&e);
                false
            }
        }
    }

    /// Crea o destruye capa y renderer al cambiar los ajustes (CA-36.4).
    fn apply_settings(&mut self, surface: &mut dyn OverlaySurface, settings: HudSettings) {
        if self.settings == Some(settings) {
            return;
        }
        let previous = self.settings.replace(settings);
        if previous.map(|p| p.max_bubbles) != Some(settings.max_bubbles) {
            self.stack = BubbleStack::new(usize::from(settings.max_bubbles));
        }
        if previous.map(|p| p.enabled) == Some(settings.enabled) {
            return;
        }
        if settings.enabled {
            match HudRenderer::new(HUD_FONT_PX) {
                Ok(renderer) => {
                    self.renderer = Some(renderer);
                    surface.set_hud_enabled(true);
                }
                Err(e) => {
                    eprintln!("[hud] desactivado: {e}");
                    self.renderer = None;
                }
            }
        } else {
            // Los globos visibles desaparecen y se liberan capa y buffers.
            self.stack.clear();
            surface.set_hud_enabled(false);
            self.renderer = None;
            self.pixmap = None;
        }
        self.shown = false;
        self.dirty = true;
    }

    /// Drena los eventos pendientes sin bloquear.
    fn drain(&mut self, ctx: HudFrame, now: Instant) {
        while let Ok(event) = self.events.try_recv() {
            if !ctx.settings.enabled || self.renderer.is_none() {
                // HUD apagado: los eventos se descartan.
                continue;
            }
            self.apply(event, ctx.suppress_new, now);
        }
    }

    fn apply(&mut self, event: HudEvent, suppress_new: bool, now: Instant) {
        match event {
            HudEvent::UserSaid { turn, text } if !suppress_new => {
                self.stack.push_user(turn, text, now);
            }
            HudEvent::JarvisPhrase { turn, text } if !suppress_new => {
                self.stack.push_jarvis(turn, text, now);
            }
            HudEvent::Notice { text } if !suppress_new => self.stack.push_notice(text, now),
            HudEvent::JarvisDone { turn } => self.stack.seal(turn, now),
            HudEvent::JarvisInterrupted { turn } => self.stack.interrupt(turn, now),
            HudEvent::Clear => {
                self.stack.clear();
                self.chips.clear();
            }
            // Chips de sub-sesión (HU-67): se rehacen aquí, nunca por frame.
            // La lista vacía los retira y deja el HUD como estaba (CA-67.1).
            // No los suprime el modo pantalla completa: informan de trabajo en
            // curso y no son globos de conversación.
            HudEvent::Sessions(views) => {
                let chips: Vec<SessionChip> = views
                    .iter()
                    .map(|view| {
                        SessionChip::new(&view.title, chip_state(&view.status), view.focused)
                    })
                    .collect();
                if chips == self.chips {
                    return;
                }
                self.chips = chips;
            }
            _ => return,
        }
        self.dirty = true;
    }

    fn report(&mut self, error: &dyn std::fmt::Display) {
        if !self.error_logged {
            self.error_logged = true;
            eprintln!("[hud] {error} (se omiten repeticiones)");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use jarvis_shell::{HeadlessSurface, OverlaySurface};

    use super::*;

    fn surface() -> HeadlessSurface {
        HeadlessSurface::new(SurfaceSize::new(4, 4).unwrap())
    }

    fn ctx(enabled: bool) -> HudFrame {
        HudFrame {
            settings: HudSettings {
                enabled,
                max_bubbles: 3,
            },
            suppress_new: false,
            text_alpha: Opacity::OPAQUE,
        }
    }

    #[test]
    fn chip_state_maps_every_task_status_hu67() {
        // Cada estado del ciclo de vida tiene su chip; `Failed` y `TimedOut`
        // comparten el rojo porque desde fuera se ven igual.
        assert_eq!(chip_state(&TaskStatus::Queued), ChipState::Queued);
        assert_eq!(chip_state(&TaskStatus::Running), ChipState::Working);
        assert_eq!(chip_state(&TaskStatus::Done), ChipState::Done);
        assert_eq!(chip_state(&TaskStatus::Failed), ChipState::Failed);
        assert_eq!(chip_state(&TaskStatus::TimedOut), ChipState::Failed);
        assert_eq!(chip_state(&TaskStatus::Cancelled), ChipState::Cancelled);
    }

    #[test]
    fn without_events_nothing_is_presented() {
        let (_tx, rx) = crossbeam_channel::bounded(64);
        let mut hud = HudPipeline::new(rx);
        let mut surface = surface();
        let now = Instant::now();
        for i in 0..30 {
            assert!(!hud.frame(&mut surface, ctx(true), now + Duration::from_millis(i * 16)));
        }
        assert_eq!(
            surface.hud_presented_frames(),
            0,
            "CA-36.9: 0 frames sin globos"
        );
        assert!(!surface.hud_layer().unwrap().is_visible());
    }

    #[test]
    fn a_turn_is_rendered_then_settles_and_hides_after_the_ttl() {
        let (tx, rx) = crossbeam_channel::bounded(64);
        let mut hud = HudPipeline::new(rx);
        let mut surface = surface();
        let start = Instant::now();
        tx.send(HudEvent::UserSaid {
            turn: 1,
            text: "qué hora es".into(),
        })
        .unwrap();
        tx.send(HudEvent::JarvisPhrase {
            turn: 1,
            text: "Son las 10:30.".into(),
        })
        .unwrap();
        tx.send(HudEvent::JarvisDone { turn: 1 }).unwrap();

        assert!(hud.frame(&mut surface, ctx(true), start));
        assert_eq!(hud.bubbles(), 2);
        let layer = surface.hud_layer().unwrap();
        assert!(layer.is_visible());

        // Fundido de entrada: sigue presentando.
        assert!(hud.frame(&mut surface, ctx(true), start + Duration::from_millis(50)));
        // En reposo y sin eventos nuevos: no presenta.
        let rest = start + Duration::from_secs(1);
        assert!(!hud.frame(&mut surface, ctx(true), rest));
        let presented = surface.hud_presented_frames();

        // Pasado el TTL (4 s) y su fundido (400 ms) desaparece y se oculta.
        let gone = start + Duration::from_secs(5);
        assert!(!hud.frame(&mut surface, ctx(true), gone));
        assert_eq!(hud.bubbles(), 0);
        assert!(!surface.hud_layer().unwrap().is_visible());
        assert_eq!(surface.hud_presented_frames(), presented);
    }

    #[test]
    fn disabling_destroys_the_layer_and_drops_bubbles() {
        let (tx, rx) = crossbeam_channel::bounded(64);
        let mut hud = HudPipeline::new(rx);
        let mut surface = surface();
        let now = Instant::now();
        tx.send(HudEvent::Notice {
            text: "Tarea lista.".into(),
        })
        .unwrap();
        assert!(hud.frame(&mut surface, ctx(true), now));
        assert_eq!(hud.bubbles(), 1);

        assert!(!hud.frame(&mut surface, ctx(false), now));
        assert_eq!(hud.bubbles(), 0, "CA-36.4: los globos desaparecen");
        assert!(surface.hud().is_none(), "CA-36.4: no hay capa");

        // Apagado, los eventos se drenan y se descartan.
        tx.send(HudEvent::Notice { text: "x".into() }).unwrap();
        assert!(!hud.frame(&mut surface, ctx(false), now));
        assert!(tx.is_empty());
        assert_eq!(hud.bubbles(), 0);
    }

    #[test]
    fn suppressed_frames_do_not_create_bubbles_but_still_seal() {
        let (tx, rx) = crossbeam_channel::bounded(64);
        let mut hud = HudPipeline::new(rx);
        let mut surface = surface();
        let now = Instant::now();
        tx.send(HudEvent::JarvisPhrase {
            turn: 2,
            text: "hola".into(),
        })
        .unwrap();
        hud.frame(&mut surface, ctx(true), now);
        assert_eq!(hud.bubbles(), 1);

        let suppressed = HudFrame {
            suppress_new: true,
            ..ctx(true)
        };
        tx.send(HudEvent::Notice { text: "x".into() }).unwrap();
        tx.send(HudEvent::JarvisDone { turn: 2 }).unwrap();
        hud.frame(&mut surface, suppressed, now);
        assert_eq!(hud.bubbles(), 1, "CA-36.7: sin globos nuevos");
        // El sellado sí se aplica: tras el TTL desaparece.
        hud.frame(&mut surface, suppressed, now + Duration::from_secs(5));
        assert_eq!(hud.bubbles(), 0);
    }

    #[test]
    fn layer_height_follows_the_bubbles() {
        let (tx, rx) = crossbeam_channel::bounded(64);
        let mut hud = HudPipeline::new(rx);
        let mut surface = surface();
        let now = Instant::now();
        tx.send(HudEvent::Notice { text: "uno".into() }).unwrap();
        hud.frame(&mut surface, ctx(true), now);
        let one = surface.hud().unwrap().size();
        assert_eq!(one.width, HUD_WIDTH);

        tx.send(HudEvent::Notice { text: "dos".into() }).unwrap();
        hud.frame(&mut surface, ctx(true), now + Duration::from_millis(16));
        let two = surface.hud().unwrap().size();
        assert!(two.height > one.height, "{one:?} → {two:?}");
        assert!(two.height <= 260);
    }

    #[test]
    fn the_layer_reappears_after_every_hide_cycle() {
        let (tx, rx) = crossbeam_channel::bounded(64);
        let mut hud = HudPipeline::new(rx);
        let mut surface = surface();
        let mut now = Instant::now();

        for turn in 1..=4u64 {
            let before = surface.hud_presented_frames();
            tx.send(HudEvent::UserSaid {
                turn,
                text: format!("frase {turn}"),
            })
            .unwrap();
            assert!(
                hud.frame(&mut surface, ctx(true), now),
                "turno {turn}: usuario"
            );
            assert!(surface.hud_layer().unwrap().is_visible());

            // La respuesta llega con la capa ya visible y cambia su alto.
            now += Duration::from_millis(500);
            tx.send(HudEvent::JarvisPhrase {
                turn,
                text: format!("Respuesta número {turn}."),
            })
            .unwrap();
            tx.send(HudEvent::JarvisDone { turn }).unwrap();
            assert!(
                hud.frame(&mut surface, ctx(true), now),
                "turno {turn}: Jarvis"
            );
            assert_eq!(hud.bubbles(), 2);
            assert!(surface.hud_presented_frames() >= before + 2);

            // Expiran los dos globos: la capa se oculta.
            now += Duration::from_secs(10);
            assert!(!hud.frame(&mut surface, ctx(true), now));
            assert_eq!(hud.bubbles(), 0);
            assert!(!surface.hud_layer().unwrap().is_visible());
            now += Duration::from_millis(16);
        }
    }
}
