//! Backend Wayland: capa overlay vía `zwlr_layer_shell_v1` (KWin, Hyprland, Sway…).
//!
//! La superficie se presenta con buffers `wl_shm` en formato `Argb8888` (doble
//! buffer reutilizado) y el ritmo lo marca el compositor con frame callbacks:
//! si la capa no es visible, no se dibuja.
//!
//! Puntero: la región de entrada es el círculo de la esfera (las esquinas
//! transparentes dejan pasar los clics). Arrastrar con el botón izquierdo
//! reposiciona la capa ajustando sus márgenes; el clic derecho emite
//! [`ShellCommand::QuickPanel`] para que la aplicación abra el Panel Rápido
//! (PyQt6) en lugar del antiguo menú nativo.
//!
//! HUD (HU-36): una segunda capa overlay en la misma conexión muestra las
//! burbujas de conversación junto a la esfera. Su región de entrada es vacía
//! (100 % *click-through*) y solo se mapea mientras hay globos.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use smithay_client_toolkit::compositor::{
    CompositorHandler, CompositorState, FrameCallbackData, Region,
};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{
    wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    self, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};

use crate::{
    rgba_premul_to_argb8888_with_alpha, Anchor, FrameBuffer, HudSurface, KeyboardInteractivity,
    Opacity, OverlayOptions, OverlaySurface, ShellCommand, SurfaceBackend, SurfaceError,
    SurfaceSize, SurfaceStatus,
};

/// Namespace de la capa (lo usan los compositores para reglas por app).
const LAYER_NAMESPACE: &str = "fokkus-jarvis";

/// Namespace de la capa del HUD (HU-36).
const HUD_NAMESPACE: &str = "fokkus-jarvis-hud";

/// Separación (px) entre la esfera y el HUD.
const HUD_GAP: i32 = 8;

/// Salida supuesta mientras el compositor no informa la real.
const FALLBACK_OUTPUT: (i32, i32) = (1920, 1080);

/// Número de buffers `wl_shm` reutilizados (doble buffer).
const BUFFER_COUNT: usize = 2;

/// Botones del puntero (códigos evdev de `linux/input-event-codes.h`).
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// Desplazamiento mínimo (px) para que una pulsación se considere arrastre.
const DRAG_THRESHOLD: f64 = 4.0;

/// Alto (px) de cada franja con la que se aproxima el círculo de entrada.
const INPUT_STRIP_HEIGHT: u32 = 4;

/// Márgenes `(top, right, bottom, left)` de la capa.
/// Espera máxima del `configure` tras un cambio de tamaño (HU-33).
const RESIZE_CONFIGURE_TIMEOUT: Duration = Duration::from_millis(250);

type Margins = (i32, i32, i32, i32);

/// Traduce el anclaje lógico a los bordes de layer-shell.
///
/// `Center` no ancla ningún borde: el compositor centra la capa.
pub fn anchor_to_layer(anchor: Anchor) -> wlr_layer::Anchor {
    match anchor {
        Anchor::TopLeft => wlr_layer::Anchor::TOP | wlr_layer::Anchor::LEFT,
        Anchor::TopRight => wlr_layer::Anchor::TOP | wlr_layer::Anchor::RIGHT,
        Anchor::BottomLeft => wlr_layer::Anchor::BOTTOM | wlr_layer::Anchor::LEFT,
        Anchor::BottomRight => wlr_layer::Anchor::BOTTOM | wlr_layer::Anchor::RIGHT,
        Anchor::Center => wlr_layer::Anchor::empty(),
    }
}

/// Interactividad de teclado efectiva en layer-shell.
///
/// Todavía no se procesa el teclado (no se enlaza xkbcommon), así que cualquier
/// valor se mapea a `None`: la esfera nunca roba el foco. `OnDemand` y
/// `Exclusive` se activarán cuando lleguen los atajos.
fn keyboard_to_layer(_value: KeyboardInteractivity) -> wlr_layer::KeyboardInteractivity {
    wlr_layer::KeyboardInteractivity::None
}

/// Márgenes `(top, right, bottom, left)` aplicados solo a los bordes anclados.
fn margins_for(anchor: Anchor, margin: u32) -> Margins {
    let m = i32::try_from(margin).unwrap_or(i32::MAX);
    let edges = anchor_to_layer(anchor);
    let pick = |edge: wlr_layer::Anchor| if edges.contains(edge) { m } else { 0 };
    (
        pick(wlr_layer::Anchor::TOP),
        pick(wlr_layer::Anchor::RIGHT),
        pick(wlr_layer::Anchor::BOTTOM),
        pick(wlr_layer::Anchor::LEFT),
    )
}

/// Aplica un arrastre `(dx, dy)` a los márgenes de los bordes anclados.
///
/// Mover hacia la derecha/abajo reduce el margen derecho/inferior y aumenta el
/// izquierdo/superior. Cada margen se acota a `[0, max]`, donde `max` es el
/// hueco libre de la salida en ese eje (si se conoce). Sin bordes anclados
/// (`Center`) la capa no se mueve.
fn drag_margins(
    anchor: Anchor,
    current: Margins,
    (dx, dy): (i32, i32),
    max: Option<(i32, i32)>,
) -> Margins {
    let edges = anchor_to_layer(anchor);
    let (max_x, max_y) = max.unwrap_or((i32::MAX, i32::MAX));
    let clamp = |v: i32, hi: i32| v.clamp(0, hi.max(0));
    let (mut top, mut right, mut bottom, mut left) = current;
    if edges.contains(wlr_layer::Anchor::LEFT) {
        left = clamp(left.saturating_add(dx), max_x);
    } else if edges.contains(wlr_layer::Anchor::RIGHT) {
        right = clamp(right.saturating_sub(dx), max_x);
    }
    if edges.contains(wlr_layer::Anchor::TOP) {
        top = clamp(top.saturating_add(dy), max_y);
    } else if edges.contains(wlr_layer::Anchor::BOTTOM) {
        bottom = clamp(bottom.saturating_sub(dy), max_y);
    }
    (top, right, bottom, left)
}

/// Franjas horizontales `(x, y, ancho, alto)` que aproximan el círculo
/// inscrito en la superficie; forman la región de entrada de la esfera.
fn circle_strips(size: SurfaceSize) -> Vec<(i32, i32, i32, i32)> {
    let (w, h) = (f64::from(size.width), f64::from(size.height));
    let radius = w.min(h) / 2.0;
    let (cx, cy) = (w / 2.0, h / 2.0);
    let mut strips = Vec::new();
    let mut y = 0;
    while y < size.height {
        let strip_h = INPUT_STRIP_HEIGHT.min(size.height - y);
        // Se usa el punto de la franja más cercano al centro: el círculo queda
        // cubierto por completo (nunca se recorta la esfera).
        let top = f64::from(y);
        let bottom = f64::from(y + strip_h);
        let nearest = cy.clamp(top, bottom);
        let dy = (nearest - cy).abs();
        if dy < radius {
            let half = (radius * radius - dy * dy).sqrt();
            let x0 = (cx - half).floor().max(0.0);
            let x1 = (cx + half).ceil().min(w);
            // Valores acotados al tamaño de la superficie (u32 → i32 seguro).
            strips.push((x0 as i32, y as i32, (x1 - x0) as i32, strip_h as i32));
        }
        y += strip_h;
    }
    strips
}

/// Datos de la esfera con los que se coloca el HUD (HU-36).
#[derive(Debug, Clone, Copy, PartialEq)]
struct HudPlacement {
    anchor: Anchor,
    /// Márgenes actuales de la esfera (incluyen el arrastre).
    sphere_margins: Margins,
    sphere: SurfaceSize,
    output: Option<(i32, i32)>,
}

/// Anclaje y márgenes del HUD para que quede pegado a la esfera.
///
/// Esfera anclada abajo: el HUD va **encima**; anclada arriba: **debajo**. Si
/// no cabe en ese lado se pasa al opuesto y, si tampoco, se satura dentro de
/// la salida. En horizontal se alinea con el borde anclado de la esfera sin
/// salirse de la salida. `Center` ancla solo arriba (el compositor centra en
/// horizontal) y deja el HUD encima de la esfera centrada.
fn hud_layout(p: &HudPlacement, hud: SurfaceSize) -> (wlr_layer::Anchor, Margins) {
    let edges = anchor_to_layer(p.anchor);
    let (out_w, out_h) = p.output.unwrap_or(FALLBACK_OUTPUT);
    let sphere_h = i32::try_from(p.sphere.height).unwrap_or(i32::MAX);
    let hud_w = i32::try_from(hud.width).unwrap_or(i32::MAX);
    let hud_h = i32::try_from(hud.height).unwrap_or(i32::MAX);
    let max_x = (out_w - hud_w).max(0);
    let max_y = (out_h - hud_h).max(0);
    let (top, right, bottom, left) = p.sphere_margins;

    if edges.is_empty() {
        let above = (out_h - sphere_h) / 2 - HUD_GAP - hud_h;
        return (wlr_layer::Anchor::TOP, (above.clamp(0, max_y), 0, 0, 0));
    }

    // Distancia al borde anclado: primero el lado preferido y, si no cabe,
    // el opuesto (la esfera ocupa `[edge, edge + sphere_h)`).
    let place = |edge: i32| {
        let preferred = edge.saturating_add(sphere_h).saturating_add(HUD_GAP);
        if preferred <= max_y {
            preferred
        } else {
            let opposite = edge - HUD_GAP - hud_h;
            if opposite >= 0 {
                opposite
            } else {
                preferred.clamp(0, max_y)
            }
        }
    };
    let mut margins = (0, 0, 0, 0);
    if edges.contains(wlr_layer::Anchor::BOTTOM) {
        margins.2 = place(bottom);
    } else if edges.contains(wlr_layer::Anchor::TOP) {
        margins.0 = place(top);
    }
    if edges.contains(wlr_layer::Anchor::RIGHT) {
        margins.1 = right.clamp(0, max_x);
    } else if edges.contains(wlr_layer::Anchor::LEFT) {
        margins.3 = left.clamp(0, max_x);
    }
    (edges, margins)
}

/// Capa overlay del HUD de burbujas (HU-36): sin entrada y sin teclado.
struct HudLayer {
    layer: LayerSurface,
    pool: SlotPool,
    buffers: Vec<Buffer>,
    size: SurfaceSize,
    placement: HudPlacement,
    configured: bool,
    frame_pending: bool,
    visible: bool,
    conn: Connection,
    qh: QueueHandle<LayerState>,
}

impl HudLayer {
    /// Aplica anclaje y márgenes según la esfera (sin commit).
    fn reposition(&self) {
        let (anchor, (top, right, bottom, left)) = hud_layout(&self.placement, self.size);
        self.layer.set_anchor(anchor);
        self.layer.set_margin(top, right, bottom, left);
    }

    fn free_buffer(&mut self) -> Result<Option<usize>, SurfaceError> {
        for (i, buffer) in self.buffers.iter().enumerate() {
            if buffer.canvas(&mut self.pool).is_some() {
                return Ok(Some(i));
            }
        }
        if self.buffers.len() >= BUFFER_COUNT {
            return Ok(None);
        }
        let width = i32::try_from(self.size.width).map_err(backend_error)?;
        let height = i32::try_from(self.size.height).map_err(backend_error)?;
        let (buffer, _) = self
            .pool
            .create_buffer(width, height, width * 4, wl_shm::Format::Argb8888)
            .map_err(backend_error)?;
        self.buffers.push(buffer);
        Ok(Some(self.buffers.len() - 1))
    }
}

impl HudSurface for HudLayer {
    /// Cambia el tamaño sin esperar al compositor: mientras llega el
    /// `configure` los frames se descartan ([`HudSurface::wants_frame`]).
    fn set_size(&mut self, size: SurfaceSize) -> Result<(), SurfaceError> {
        if size == self.size {
            return Ok(());
        }
        self.size = size;
        self.buffers.clear();
        let needed = size.pixel_count() * 4 * BUFFER_COUNT;
        if self.pool.len() < needed {
            self.pool.resize(needed).map_err(backend_error)?;
        }
        self.layer.set_size(size.width, size.height);
        self.reposition();
        if self.visible {
            self.configured = false;
            self.frame_pending = false;
            self.layer.commit();
            self.conn.flush().map_err(backend_error)?;
        }
        Ok(())
    }

    fn size(&self) -> SurfaceSize {
        self.size
    }

    fn wants_frame(&self) -> bool {
        self.configured && self.visible && !self.frame_pending
    }

    fn present(&mut self, frame: &FrameBuffer<'_>) -> Result<(), SurfaceError> {
        let size = self.size;
        let expected = size.pixel_count() * 4;
        if frame.width != size.width || frame.height != size.height || frame.data.len() != expected
        {
            return Err(SurfaceError::InvalidFrame {
                expected,
                actual: frame.data.len(),
            });
        }
        if !self.wants_frame() {
            return Ok(());
        }
        let Some(index) = self.free_buffer()? else {
            return Ok(());
        };
        let buffer = &self.buffers[index];
        let canvas = buffer
            .canvas(&mut self.pool)
            .ok_or_else(|| SurfaceError::Backend("buffer wl_shm del HUD ocupado".to_string()))?;
        // La opacidad ya viene aplicada en el render del HUD.
        rgba_premul_to_argb8888_with_alpha(frame.data, canvas, Opacity::OPAQUE)?;

        let surface = self.layer.wl_surface().clone();
        let width = i32::try_from(size.width).map_err(backend_error)?;
        let height = i32::try_from(size.height).map_err(backend_error)?;
        buffer.attach_to(&surface).map_err(backend_error)?;
        surface.damage_buffer(0, 0, width, height);
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        self.layer.commit();
        self.frame_pending = true;
        self.conn.flush().map_err(backend_error)?;
        Ok(())
    }

    fn set_visible(&mut self, visible: bool) {
        if visible == self.visible {
            return;
        }
        self.visible = visible;
        if visible {
            // Mapear exige un commit sin buffer y esperar el `configure`.
            self.configured = false;
            self.frame_pending = false;
            self.layer.set_size(self.size.width, self.size.height);
            self.reposition();
        } else {
            self.layer.wl_surface().attach(None, 0, 0);
        }
        self.layer.commit();
        let _ = self.conn.flush();
    }
}

/// Gesto del puntero en curso sobre la esfera.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct DragGesture {
    /// Posición local de la pulsación izquierda (si el botón sigue pulsado).
    press: Option<(f64, f64)>,
    /// Se superó el umbral: la pulsación ya es un arrastre.
    dragging: bool,
}

impl DragGesture {
    fn press(&mut self, position: (f64, f64)) {
        self.press = Some(position);
        self.dragging = false;
    }

    /// Desplazamiento entero a aplicar a la capa, o `None` si no hay arrastre.
    ///
    /// Tras mover la capa el compositor reporta coordenadas relativas a la
    /// nueva posición, por eso el origen de la pulsación no se actualiza.
    fn motion(&mut self, position: (f64, f64)) -> Option<(i32, i32)> {
        let (px, py) = self.press?;
        let (dx, dy) = (position.0 - px, position.1 - py);
        if !self.dragging && dx.hypot(dy) < DRAG_THRESHOLD {
            return None;
        }
        self.dragging = true;
        let delta = (dx.round() as i32, dy.round() as i32);
        (delta != (0, 0)).then_some(delta)
    }

    /// Suelta el botón; devuelve si la pulsación fue un arrastre.
    fn release(&mut self) -> bool {
        let dragged = self.dragging;
        *self = Self::default();
        dragged
    }
}

fn backend_error(e: impl std::fmt::Display) -> SurfaceError {
    SurfaceError::Backend(e.to_string())
}

/// Estado despachado por la cola de eventos Wayland.
struct LayerState {
    registry: RegistryState,
    /// Requerido por SCTK para seguir en qué salida está la superficie.
    output: OutputState,
    seat: SeatState,
    pointer: Option<wl_pointer::WlPointer>,
    gesture: DragGesture,
    /// Acciones del menú pendientes de entregar a la aplicación.
    commands: VecDeque<ShellCommand>,
    anchor: Anchor,
    /// Margen base de [`OverlayOptions`] (para volver a la posición inicial).
    #[allow(dead_code)] // reservado para "restablecer posición"
    base_margin: u32,
    margins: Margins,
    /// Tamaño lógico de la salida donde está la capa (acota el arrastre).
    output_size: Option<(i32, i32)>,
    /// Nombre de la última salida en la que entró la capa (HU-35). Se conserva
    /// al ocultarla para seguir comparando con la ventana a pantalla completa.
    output_name: Option<String>,
    compositor: CompositorState,
    shm: Shm,
    pool: SlotPool,
    buffers: Vec<Buffer>,
    layer: LayerSurface,
    size: SurfaceSize,
    /// Se recibió el `configure` vigente: ya se puede presentar.
    configured: bool,
    /// Hay un frame callback pedido y sin responder.
    frame_pending: bool,
    visible: bool,
    closed: bool,
    /// Ya se avisó de un tamaño propuesto distinto (se avisa una sola vez).
    size_mismatch_logged: bool,
    /// Opacidad global aplicada en la conversión a `Argb8888` (HU-34).
    opacity: Opacity,
    /// Ya se avisó de un `configure` que no llegó tras redimensionar.
    resize_timeout_logged: bool,
    /// Capa del HUD de burbujas (HU-36); `None` si está desactivado.
    hud: Option<HudLayer>,
}

impl LayerState {
    fn surface(&self) -> &wl_surface::WlSurface {
        self.layer.wl_surface()
    }

    /// `true` si `surface` es la del HUD.
    fn is_hud(&self, surface: &wl_surface::WlSurface) -> bool {
        self.hud
            .as_ref()
            .is_some_and(|hud| hud.layer.wl_surface() == surface)
    }

    fn hud_placement(&self) -> HudPlacement {
        HudPlacement {
            anchor: self.anchor,
            sphere_margins: self.margins,
            sphere: self.size,
            output: self.output_size,
        }
    }

    /// Recoloca el HUD tras mover, redimensionar o re-anclar la esfera.
    fn sync_hud(&mut self) {
        let placement = self.hud_placement();
        if let Some(hud) = &mut self.hud {
            if hud.placement == placement {
                return;
            }
            hud.placement = placement;
            hud.reposition();
            if hud.visible {
                hud.layer.commit();
            }
        }
    }

    /// Región de entrada circular: la esfera recibe los clics y las esquinas
    /// transparentes los dejan pasar a la ventana de abajo.
    fn apply_sphere_input_region(&self) -> Result<(), SurfaceError> {
        let region = Region::new(&self.compositor).map_err(backend_error)?;
        for (x, y, w, h) in circle_strips(self.size) {
            region.add(x, y, w, h);
        }
        self.layer.set_input_region(Some(region.wl_region()));
        Ok(())
    }

    /// Ancla la capa en `anchor` con el margen base (descarta arrastres previos).
    fn apply_anchor(&mut self, anchor: Anchor, margin: u32) {
        self.anchor = anchor;
        self.margins = margins_for(anchor, margin);
        self.layer.set_anchor(anchor_to_layer(anchor));
        self.apply_margins();
        self.sync_hud();
    }

    fn apply_margins(&self) {
        let (top, right, bottom, left) = self.margins;
        self.layer.set_margin(top, right, bottom, left);
    }

    /// Hueco libre `(x, y)` de la salida para acotar los márgenes.
    fn drag_bounds(&self) -> Option<(i32, i32)> {
        let (w, h) = self.output_size?;
        let sw = i32::try_from(self.size.width).ok()?;
        let sh = i32::try_from(self.size.height).ok()?;
        Some((w - sw, h - sh))
    }

    /// Re-satura los márgenes al hueco libre actual (tras crecer la esfera,
    /// para que no quede parcialmente fuera de la salida).
    fn clamp_margins(&mut self) {
        let next = drag_margins(self.anchor, self.margins, (0, 0), self.drag_bounds());
        if next != self.margins {
            self.margins = next;
            self.apply_margins();
        }
    }

    /// Aplica `size` a la capa: descarta los buffers del tamaño anterior,
    /// reserva memoria en el pool y recalcula región de entrada y márgenes.
    /// No hace commit.
    fn apply_size(&mut self, size: SurfaceSize) -> Result<(), SurfaceError> {
        self.size = size;
        // Los buffers retenidos por el compositor se destruyen al liberarse.
        self.buffers.clear();
        let needed = size.pixel_count() * 4 * BUFFER_COUNT;
        if self.pool.len() < needed {
            self.pool.resize(needed).map_err(backend_error)?;
        }
        self.layer.set_size(size.width, size.height);
        self.apply_sphere_input_region()?;
        self.clamp_margins();
        self.sync_hud();
        Ok(())
    }

    /// Desplaza la capa `delta` píxeles (arrastre con el botón izquierdo).
    fn drag_by(&mut self, delta: (i32, i32)) {
        let next = drag_margins(self.anchor, self.margins, delta, self.drag_bounds());
        if next != self.margins {
            self.margins = next;
            self.apply_margins();
            self.layer.commit();
            self.sync_hud();
        }
    }

    /// Devuelve el índice de un buffer libre (no retenido por el compositor),
    /// creándolo si aún no existen los [`BUFFER_COUNT`] del doble buffer.
    fn free_buffer(&mut self) -> Result<Option<usize>, SurfaceError> {
        for (i, buffer) in self.buffers.iter().enumerate() {
            if buffer.canvas(&mut self.pool).is_some() {
                return Ok(Some(i));
            }
        }
        if self.buffers.len() >= BUFFER_COUNT {
            return Ok(None);
        }
        let width = i32::try_from(self.size.width).map_err(backend_error)?;
        let height = i32::try_from(self.size.height).map_err(backend_error)?;
        let (buffer, _) = self
            .pool
            .create_buffer(width, height, width * 4, wl_shm::Format::Argb8888)
            .map_err(backend_error)?;
        self.buffers.push(buffer);
        Ok(Some(self.buffers.len() - 1))
    }
}

impl CompositorHandler for LayerState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if self.is_hud(surface) {
            if let Some(hud) = &mut self.hud {
                hud.frame_pending = false;
            }
        } else {
            self.frame_pending = false;
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        // Solo cuenta la salida de la esfera (el HUD la sigue).
        if surface != self.layer.wl_surface() {
            return;
        }
        let info = self.output.info(output);
        self.output_size = info.as_ref().and_then(|info| info.logical_size);
        if let Some(name) = info.and_then(|info| info.name) {
            self.output_name = Some(name);
        }
        self.sync_hud();
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for LayerState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if self.is_hud(layer.wl_surface()) {
            // Cerrar el HUD no cierra la esfera: se sigue sin burbujas.
            eprintln!("[shell] el compositor cerró la capa del HUD");
            self.hud = None;
            return;
        }
        self.closed = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if self.is_hud(layer.wl_surface()) {
            // El tamaño del HUD lo fija la app; solo se marca configurado.
            if let Some(hud) = &mut self.hud {
                hud.configured = true;
                hud.frame_pending = false;
            }
            return;
        }
        let (w, h) = configure.new_size;
        let proposed_other = w != 0 && h != 0 && (w, h) != (self.size.width, self.size.height);
        if proposed_other && !self.size_mismatch_logged {
            eprintln!(
                "[shell] el compositor propone {w}x{h}; se mantiene {}x{} (tamaño fijado por la app)",
                self.size.width, self.size.height
            );
            self.size_mismatch_logged = true;
        }
        self.configured = true;
        // Un configure nuevo invalida cualquier callback pendiente.
        self.frame_pending = false;
    }
}

impl OutputHandler for LayerState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for LayerState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat.get_pointer(qh, &seat) {
                Ok(pointer) => {
                    self.pointer = Some(pointer);
                }
                Err(e) => eprintln!("[shell] no se pudo obtener el puntero: {e}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
            self.gesture = DragGesture::default();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for LayerState {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            match event.kind {
                PointerEventKind::Press {
                    button: BTN_LEFT, ..
                } => self.gesture.press(event.position),
                PointerEventKind::Motion { .. } => {
                    if let Some(delta) = self.gesture.motion(event.position) {
                        self.drag_by(delta);
                    }
                }
                PointerEventKind::Release {
                    button: BTN_LEFT, ..
                } => {
                    self.gesture.release();
                }
                PointerEventKind::Press {
                    button: BTN_RIGHT, ..
                } => {
                    // Coordenada absoluta del clic en pantalla: posición local
                    // dentro de la esfera + esquina superior izquierda de la
                    // capa según el anclaje y los márgenes actuales.
                    let (top, right, bottom, left) = self.margins;
                    let (output_w, output_h) = self.output_size.unwrap_or((1920, 1080));
                    let sphere_w = self.size.width as i32;
                    let sphere_h = self.size.height as i32;
                    let sphere_x = match self.anchor {
                        Anchor::BottomRight | Anchor::TopRight => output_w - right - sphere_w,
                        Anchor::BottomLeft | Anchor::TopLeft => left,
                        Anchor::Center => (output_w - sphere_w) / 2,
                    };
                    let sphere_y = match self.anchor {
                        Anchor::BottomRight | Anchor::BottomLeft => output_h - bottom - sphere_h,
                        Anchor::TopLeft | Anchor::TopRight => top,
                        Anchor::Center => (output_h - sphere_h) / 2,
                    };
                    let click_x = sphere_x + event.position.0 as i32;
                    let click_y = sphere_y + event.position.1 as i32;
                    self.commands.push_back(ShellCommand::QuickPanel {
                        x: click_x,
                        y: click_y,
                    });
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for LayerState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for LayerState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(LayerState);
delegate_dispatch2!(LayerState);

/// Superficie overlay real sobre `zwlr_layer_shell_v1`.
pub struct WaylandLayerSurface {
    event_loop: EventLoop<'static, LayerState>,
    conn: Connection,
    qh: QueueHandle<LayerState>,
    state: LayerState,
    options: OverlayOptions,
    /// Para crear la capa del HUD bajo demanda (HU-36).
    layer_shell: LayerShell,
}

impl WaylandLayerSurface {
    /// Conecta con el compositor y crea la capa overlay (sin mapearla hasta el
    /// primer `configure`).
    pub fn open(options: OverlayOptions) -> Result<Self, SurfaceError> {
        let conn = Connection::connect_to_env()
            .map_err(|_| SurfaceError::BackendUnavailable("wayland"))?;
        let (globals, event_queue) = registry_queue_init::<LayerState>(&conn)
            .map_err(|_| SurfaceError::BackendUnavailable("wayland"))?;
        let qh = event_queue.handle();

        let compositor = CompositorState::bind(&globals, &qh)
            .map_err(|_| SurfaceError::BackendUnavailable("wl_compositor"))?;
        let layer_shell = LayerShell::bind(&globals, &qh)
            .map_err(|_| SurfaceError::BackendUnavailable("zwlr_layer_shell_v1"))?;
        let shm =
            Shm::bind(&globals, &qh).map_err(|_| SurfaceError::BackendUnavailable("wl_shm"))?;

        let size = options.size;
        let pool =
            SlotPool::new(size.pixel_count() * 4 * BUFFER_COUNT, &shm).map_err(backend_error)?;

        let surface = compositor.create_surface(&qh);
        let layer = layer_shell.create_layer_surface(
            &qh,
            surface,
            Layer::Overlay,
            Some(LAYER_NAMESPACE),
            None,
        );
        layer.set_size(size.width, size.height);
        layer.set_exclusive_zone(options.exclusive_zone);
        layer.set_keyboard_interactivity(keyboard_to_layer(options.keyboard_interactivity));

        let event_loop: EventLoop<'static, LayerState> =
            EventLoop::try_new().map_err(backend_error)?;
        WaylandSource::new(conn.clone(), event_queue)
            .insert(event_loop.handle())
            .map_err(|e| backend_error(e.error))?;

        let mut state = LayerState {
            registry: RegistryState::new(&globals),
            output: OutputState::new(&globals, &qh),
            seat: SeatState::new(&globals, &qh),
            pointer: None,
            gesture: DragGesture::default(),
            commands: VecDeque::new(),
            anchor: options.anchor,
            base_margin: options.margin,
            margins: margins_for(options.anchor, options.margin),
            output_size: None,
            output_name: None,
            compositor,
            shm,
            pool,
            buffers: Vec::with_capacity(BUFFER_COUNT),
            layer,
            size,
            configured: false,
            frame_pending: false,
            visible: true,
            closed: false,
            size_mismatch_logged: false,
            opacity: Opacity::OPAQUE,
            resize_timeout_logged: false,
            hud: None,
        };
        state.apply_anchor(options.anchor, options.margin);
        state.apply_sphere_input_region()?;
        // Commit inicial sin buffer: el compositor responde con `configure`.
        state.layer.commit();
        conn.flush().map_err(backend_error)?;

        Ok(Self {
            event_loop,
            conn,
            qh,
            state,
            options,
            layer_shell,
        })
    }

    /// Crea la capa del HUD sin mapearla (se mapea en `set_visible(true)`).
    fn create_hud(&mut self) -> Result<HudLayer, SurfaceError> {
        let size = SurfaceSize {
            width: 1,
            height: 1,
        };
        let pool = SlotPool::new(size.pixel_count() * 4 * BUFFER_COUNT, &self.state.shm)
            .map_err(backend_error)?;
        let surface = self.state.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some(HUD_NAMESPACE),
            None,
        );
        layer.set_size(size.width, size.height);
        layer.set_exclusive_zone(0);
        layer.set_keyboard_interactivity(wlr_layer::KeyboardInteractivity::None);
        // Región de entrada vacía: todos los clics pasan a la ventana de abajo.
        let region = Region::new(&self.state.compositor).map_err(backend_error)?;
        layer.set_input_region(Some(region.wl_region()));
        let hud = HudLayer {
            layer,
            pool,
            buffers: Vec::with_capacity(BUFFER_COUNT),
            size,
            placement: self.state.hud_placement(),
            configured: false,
            frame_pending: false,
            visible: false,
            conn: self.conn.clone(),
            qh: self.qh.clone(),
        };
        hud.reposition();
        Ok(hud)
    }

    /// Si ya se recibió el primer `configure`.
    pub fn is_configured(&self) -> bool {
        self.state.configured
    }
}

impl OverlaySurface for WaylandLayerSurface {
    fn backend(&self) -> SurfaceBackend {
        SurfaceBackend::WaylandLayer
    }

    fn pump_events(&mut self, timeout: Duration) -> Result<SurfaceStatus, SurfaceError> {
        if self.state.closed {
            return Ok(SurfaceStatus::Closed);
        }
        // Un error de despacho o de flush implica conexión rota (compositor caído).
        if self.event_loop.dispatch(timeout, &mut self.state).is_err() {
            return Ok(SurfaceStatus::Closed);
        }
        if self.conn.flush().is_err() || self.state.closed {
            return Ok(SurfaceStatus::Closed);
        }
        Ok(SurfaceStatus::Running)
    }

    fn wants_frame(&self) -> bool {
        self.state.configured && self.state.visible && !self.state.frame_pending
    }

    fn size(&self) -> SurfaceSize {
        self.state.size
    }

    fn present(&mut self, frame: &FrameBuffer<'_>) -> Result<(), SurfaceError> {
        let size = self.state.size;
        let expected = size.pixel_count() * 4;
        if frame.width != size.width || frame.height != size.height || frame.data.len() != expected
        {
            return Err(SurfaceError::InvalidFrame {
                expected,
                actual: frame.data.len(),
            });
        }
        if !self.wants_frame() {
            return Ok(());
        }
        // Ambos buffers retenidos por el compositor: se salta el frame en vez
        // de asignar uno nuevo.
        let Some(index) = self.state.free_buffer()? else {
            return Ok(());
        };

        let state = &mut self.state;
        let buffer = &state.buffers[index];
        let canvas = buffer
            .canvas(&mut state.pool)
            .ok_or_else(|| SurfaceError::Backend("buffer wl_shm ocupado".to_string()))?;
        rgba_premul_to_argb8888_with_alpha(frame.data, canvas, state.opacity)?;

        let surface = state.layer.wl_surface().clone();
        let width = i32::try_from(size.width).map_err(backend_error)?;
        let height = i32::try_from(size.height).map_err(backend_error)?;
        buffer.attach_to(&surface).map_err(backend_error)?;
        surface.damage_buffer(0, 0, width, height);
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        state.layer.commit();
        state.frame_pending = true;
        self.conn.flush().map_err(backend_error)?;
        Ok(())
    }

    fn set_visible(&mut self, visible: bool) {
        if visible == self.state.visible {
            return;
        }
        self.state.visible = visible;
        if visible {
            // Remapear exige un commit sin buffer y esperar un configure nuevo.
            self.state.configured = false;
            self.state.frame_pending = false;
            self.state
                .layer
                .set_size(self.state.size.width, self.state.size.height);
            // Se conserva la posición a la que el usuario arrastró la esfera.
            self.state
                .layer
                .set_anchor(anchor_to_layer(self.state.anchor));
            self.state.apply_margins();
            if let Err(e) = self.state.apply_sphere_input_region() {
                eprintln!("[shell] no se pudo restaurar la región de entrada: {e}");
            }
        } else {
            self.state.surface().attach(None, 0, 0);
        }
        self.state.layer.commit();
        // Un fallo de flush aflorará como `Closed` en el siguiente `pump_events`.
        let _ = self.conn.flush();
    }

    fn set_anchor(&mut self, anchor: Anchor) {
        self.options.anchor = anchor;
        self.state.apply_anchor(anchor, self.options.margin);
        self.state.layer.commit();
        let _ = self.conn.flush();
    }

    fn reset_position(&mut self) {
        self.state
            .apply_anchor(self.options.anchor, self.options.margin);
        self.state.layer.commit();
        let _ = self.conn.flush();
    }

    fn poll_command(&mut self) -> Option<ShellCommand> {
        self.state.commands.pop_front()
    }

    /// Redimensiona la capa y espera el `configure` correspondiente (como
    /// máximo [`RESIZE_CONFIGURE_TIMEOUT`]) para no presentar frames del
    /// tamaño anterior. Si el compositor no responde, se conserva el tamaño
    /// previo y se devuelve error.
    fn set_size(&mut self, size: SurfaceSize) -> Result<(), SurfaceError> {
        if size == self.state.size {
            return Ok(());
        }
        let previous = self.state.size;
        self.state.apply_size(size)?;
        if !self.state.visible {
            // Oculta: `set_visible(true)` remapea ya con el tamaño nuevo.
            return Ok(());
        }
        self.state.configured = false;
        self.state.frame_pending = false;
        self.state.layer.commit();
        self.conn.flush().map_err(backend_error)?;

        let deadline = Instant::now() + RESIZE_CONFIGURE_TIMEOUT;
        while !self.state.configured && !self.state.closed {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            if self.event_loop.dispatch(left, &mut self.state).is_err() {
                return Err(SurfaceError::Backend(
                    "conexión Wayland perdida".to_string(),
                ));
            }
        }
        if self.state.configured || self.state.closed {
            return Ok(());
        }

        // Sin `configure`: se restaura el tamaño anterior y se sigue dibujando.
        self.state.apply_size(previous)?;
        self.state.layer.commit();
        self.state.configured = true;
        let _ = self.conn.flush();
        let detail = format!(
            "el compositor no confirmó {}x{} en {} ms",
            size.width,
            size.height,
            RESIZE_CONFIGURE_TIMEOUT.as_millis()
        );
        if !self.state.resize_timeout_logged {
            eprintln!(
                "[shell] {detail}; se mantiene {}x{}",
                previous.width, previous.height
            );
            self.state.resize_timeout_logged = true;
        }
        Err(SurfaceError::Backend(detail))
    }

    fn set_opacity(&mut self, opacity: Opacity) {
        self.state.opacity = opacity;
    }

    fn current_output(&self) -> Option<String> {
        self.state.output_name.clone()
    }

    fn set_hud_enabled(&mut self, enabled: bool) {
        if !enabled {
            // Soltar la capa destruye la superficie y libera sus buffers.
            if self.state.hud.take().is_some() {
                let _ = self.conn.flush();
            }
            return;
        }
        if self.state.hud.is_some() {
            return;
        }
        match self.create_hud() {
            Ok(hud) => self.state.hud = Some(hud),
            Err(e) => eprintln!("[shell] no se pudo crear la capa del HUD: {e}"),
        }
    }

    fn hud(&mut self) -> Option<&mut dyn HudSurface> {
        self.state
            .hud
            .as_mut()
            .map(|hud| hud as &mut dyn HudSurface)
    }
}

/// Comprueba que el compositor ofrece los globals necesarios sin crear la capa.
pub(crate) fn probe() -> Result<(), SurfaceError> {
    let conn =
        Connection::connect_to_env().map_err(|_| SurfaceError::BackendUnavailable("wayland"))?;
    let (globals, event_queue) = registry_queue_init::<LayerState>(&conn)
        .map_err(|_| SurfaceError::BackendUnavailable("wayland"))?;
    let qh = event_queue.handle();
    CompositorState::bind(&globals, &qh)
        .map_err(|_| SurfaceError::BackendUnavailable("wl_compositor"))?;
    LayerShell::bind(&globals, &qh)
        .map_err(|_| SurfaceError::BackendUnavailable("zwlr_layer_shell_v1"))?;
    Shm::bind(&globals, &qh).map_err(|_| SurfaceError::BackendUnavailable("wl_shm"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_maps_to_layer_edges() {
        use wlr_layer::Anchor as L;
        assert_eq!(anchor_to_layer(Anchor::TopLeft), L::TOP | L::LEFT);
        assert_eq!(anchor_to_layer(Anchor::TopRight), L::TOP | L::RIGHT);
        assert_eq!(anchor_to_layer(Anchor::BottomLeft), L::BOTTOM | L::LEFT);
        assert_eq!(anchor_to_layer(Anchor::BottomRight), L::BOTTOM | L::RIGHT);
        assert_eq!(anchor_to_layer(Anchor::Center), L::empty());
    }

    #[test]
    fn margins_only_on_anchored_edges() {
        assert_eq!(margins_for(Anchor::BottomRight, 12), (0, 12, 12, 0));
        assert_eq!(margins_for(Anchor::TopLeft, 8), (8, 0, 0, 8));
        assert_eq!(margins_for(Anchor::Center, 12), (0, 0, 0, 0));
    }

    #[test]
    fn drag_moves_away_from_anchored_edges() {
        // BottomRight: mover a la derecha/abajo reduce los márgenes derecho/inferior.
        let base = margins_for(Anchor::BottomRight, 12);
        assert_eq!(
            drag_margins(Anchor::BottomRight, base, (-30, -20), None),
            (0, 42, 32, 0)
        );
        assert_eq!(
            drag_margins(Anchor::BottomRight, base, (5, 5), None),
            (0, 7, 7, 0)
        );
        // TopLeft: mover a la derecha/abajo aumenta los márgenes izquierdo/superior.
        let base = margins_for(Anchor::TopLeft, 8);
        assert_eq!(
            drag_margins(Anchor::TopLeft, base, (10, 4), None),
            (12, 0, 0, 18)
        );
        // Center no tiene bordes anclados: no se mueve.
        assert_eq!(
            drag_margins(Anchor::Center, (0, 0, 0, 0), (50, 50), None),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn drag_is_clamped_to_output() {
        let base = margins_for(Anchor::BottomRight, 12);
        // No sale por el borde anclado (margen negativo).
        assert_eq!(
            drag_margins(Anchor::BottomRight, base, (100, 100), None),
            (0, 0, 0, 0)
        );
        // Ni por el borde opuesto si se conoce la salida.
        assert_eq!(
            drag_margins(Anchor::BottomRight, base, (-5000, -5000), Some((1700, 860))),
            (0, 1700, 860, 0)
        );
    }

    #[test]
    fn gesture_needs_threshold_before_dragging() {
        let mut gesture = DragGesture::default();
        // Movimiento sin pulsación: no hay arrastre.
        assert_eq!(gesture.motion((50.0, 50.0)), None);

        gesture.press((100.0, 100.0));
        assert_eq!(gesture.motion((101.0, 102.0)), None);
        assert_eq!(gesture.motion((110.0, 95.0)), Some((10, -5)));
        // Ya en arrastre, cualquier desplazamiento cuenta.
        assert_eq!(gesture.motion((101.0, 100.0)), Some((1, 0)));
        assert!(gesture.release());
        assert_eq!(gesture.motion((150.0, 150.0)), None);

        // Pulsar y soltar sin moverse no es arrastre.
        gesture.press((10.0, 10.0));
        assert!(!gesture.release());
    }

    #[test]
    fn input_region_covers_the_sphere_but_not_the_corners() {
        let size = SurfaceSize::new(220, 220).unwrap();
        let strips = circle_strips(size);
        assert!(!strips.is_empty());
        let covers = |px: i32, py: i32| {
            strips
                .iter()
                .any(|&(x, y, w, h)| px >= x && px < x + w && py >= y && py < y + h)
        };
        // Centro y puntos cardinales cerca del borde del círculo.
        assert!(covers(110, 110));
        assert!(covers(110, 2));
        assert!(covers(2, 110));
        assert!(covers(217, 110));
        assert!(covers(110, 217));
        // Las esquinas transparentes dejan pasar los clics.
        assert!(!covers(0, 0));
        assert!(!covers(219, 0));
        assert!(!covers(0, 219));
        assert!(!covers(219, 219));
        // Todas las franjas caen dentro de la superficie.
        for (x, y, w, h) in strips {
            assert!(x >= 0 && y >= 0 && w > 0 && h > 0);
            assert!(x + w <= 220 && y + h <= 220);
        }
    }

    #[test]
    fn input_region_follows_the_new_size() {
        for side in [110_i32, 440] {
            let size = SurfaceSize::new(side as u32, side as u32).unwrap();
            let strips = circle_strips(size);
            let covers = |px: i32, py: i32| {
                strips
                    .iter()
                    .any(|&(x, y, w, h)| px >= x && px < x + w && py >= y && py < y + h)
            };
            assert!(covers(side / 2, side / 2));
            assert!(!covers(0, 0));
            assert!(!covers(side - 1, side - 1));
            for (x, y, w, h) in strips {
                assert!(x + w <= side && y + h <= side);
            }
        }
    }

    #[test]
    fn margins_are_resaturated_after_growing() {
        // Esfera de 220 px arrastrada casi al borde opuesto de una salida 1920x1080.
        let dragged = (0, 1690, 850, 0);
        // Al crecer a 440 px el hueco libre baja a (1480, 640): se satura.
        assert_eq!(
            drag_margins(Anchor::BottomRight, dragged, (0, 0), Some((1480, 640))),
            (0, 1480, 640, 0)
        );
        // Si cabe, no se mueve.
        assert_eq!(
            drag_margins(
                Anchor::BottomRight,
                (0, 12, 12, 0),
                (0, 0),
                Some((1480, 640))
            ),
            (0, 12, 12, 0)
        );
    }

    fn placement(anchor: Anchor, margins: Margins) -> HudPlacement {
        HudPlacement {
            anchor,
            sphere_margins: margins,
            sphere: SurfaceSize::new(220, 220).unwrap(),
            output: Some((1920, 1080)),
        }
    }

    #[test]
    fn hud_sits_above_a_bottom_anchored_sphere() {
        use wlr_layer::Anchor as L;
        let hud = SurfaceSize::new(320, 100).unwrap();
        let (anchor, margins) = hud_layout(&placement(Anchor::BottomRight, (0, 12, 12, 0)), hud);
        assert_eq!(anchor, L::BOTTOM | L::RIGHT);
        // 12 de margen + 220 de esfera + 8 de separación.
        assert_eq!(margins, (0, 12, 240, 0));
    }

    #[test]
    fn hud_sits_below_a_top_anchored_sphere() {
        let hud = SurfaceSize::new(320, 100).unwrap();
        let (_, margins) = hud_layout(&placement(Anchor::TopLeft, (8, 0, 0, 8)), hud);
        assert_eq!(margins, (236, 0, 0, 8));
    }

    #[test]
    fn hud_flips_side_when_it_does_not_fit() {
        // Esfera arrastrada arriba del todo estando anclada abajo: no cabe
        // encima, así que el HUD pasa debajo de la esfera.
        let hud = SurfaceSize::new(320, 200).unwrap();
        let (_, margins) = hud_layout(&placement(Anchor::BottomRight, (0, 12, 850, 0)), hud);
        assert_eq!(margins.2, 850 - HUD_GAP - 200);
    }

    #[test]
    fn hud_stays_inside_the_output_horizontally() {
        // Esfera de 220 px pegada al borde izquierdo con anclaje derecho: el
        // HUD de 320 px no puede usar el mismo margen derecho.
        let hud = SurfaceSize::new(320, 100).unwrap();
        let (_, margins) = hud_layout(&placement(Anchor::BottomRight, (0, 1700, 12, 0)), hud);
        assert_eq!(margins.1, 1920 - 320);
    }

    #[test]
    fn hud_over_a_centered_sphere_anchors_only_top() {
        use wlr_layer::Anchor as L;
        let hud = SurfaceSize::new(320, 100).unwrap();
        let (anchor, margins) = hud_layout(&placement(Anchor::Center, (0, 0, 0, 0)), hud);
        assert_eq!(anchor, L::TOP);
        assert_eq!(margins, ((1080 - 220) / 2 - HUD_GAP - 100, 0, 0, 0));
    }

    #[test]
    fn keyboard_is_never_captured_yet() {
        for value in [
            KeyboardInteractivity::None,
            KeyboardInteractivity::OnDemand,
            KeyboardInteractivity::Exclusive,
        ] {
            assert_eq!(
                keyboard_to_layer(value),
                wlr_layer::KeyboardInteractivity::None
            );
        }
    }
}
