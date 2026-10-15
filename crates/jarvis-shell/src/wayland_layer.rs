//! Backend Wayland: capa overlay vía `zwlr_layer_shell_v1` (KWin, Hyprland, Sway…).
//!
//! La superficie se presenta con buffers `wl_shm` en formato `Argb8888` (doble
//! buffer reutilizado) y el ritmo lo marca el compositor con frame callbacks:
//! si la capa no es visible, no se dibuja.

use std::time::Duration;

use smithay_client_toolkit::compositor::{
    CompositorHandler, CompositorState, FrameCallbackData, Region,
};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    self, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};

use crate::{
    rgba_premul_to_argb8888, Anchor, FrameBuffer, KeyboardInteractivity, OverlayOptions,
    OverlaySurface, SurfaceBackend, SurfaceError, SurfaceSize, SurfaceStatus,
};

/// Namespace de la capa (lo usan los compositores para reglas por app).
const LAYER_NAMESPACE: &str = "fokkus-jarvis";

/// Número de buffers `wl_shm` reutilizados (doble buffer).
const BUFFER_COUNT: usize = 2;

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
fn margins_for(anchor: Anchor, margin: u32) -> (i32, i32, i32, i32) {
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

fn backend_error(e: impl std::fmt::Display) -> SurfaceError {
    SurfaceError::Backend(e.to_string())
}

/// Estado despachado por la cola de eventos Wayland.
struct LayerState {
    registry: RegistryState,
    /// Requerido por SCTK para seguir en qué salida está la superficie.
    output: OutputState,
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
}

impl LayerState {
    fn surface(&self) -> &wl_surface::WlSurface {
        self.layer.wl_surface()
    }

    /// Región de entrada vacía: los clics atraviesan la esfera.
    fn apply_empty_input_region(&self) -> Result<(), SurfaceError> {
        let region = Region::new(&self.compositor).map_err(backend_error)?;
        self.layer.set_input_region(Some(region.wl_region()));
        Ok(())
    }

    fn apply_anchor(&self, anchor: Anchor, margin: u32) {
        let (top, right, bottom, left) = margins_for(anchor, margin);
        self.layer.set_anchor(anchor_to_layer(anchor));
        self.layer.set_margin(top, right, bottom, left);
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

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.frame_pending = false;
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
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
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.closed = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (w, h) = configure.new_size;
        let proposed_other = w != 0 && h != 0 && (w, h) != (self.size.width, self.size.height);
        if proposed_other && !self.size_mismatch_logged {
            eprintln!(
                "[shell] el compositor propone {w}x{h}; se mantiene {}x{} (esfera de tamaño fijo)",
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

impl ShmHandler for LayerState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for LayerState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
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

        let state = LayerState {
            registry: RegistryState::new(&globals),
            output: OutputState::new(&globals, &qh),
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
        };
        state.apply_anchor(options.anchor, options.margin);
        state.apply_empty_input_region()?;
        // Commit inicial sin buffer: el compositor responde con `configure`.
        state.layer.commit();
        conn.flush().map_err(backend_error)?;

        Ok(Self {
            event_loop,
            conn,
            qh,
            state,
            options,
        })
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
        rgba_premul_to_argb8888(frame.data, canvas)?;

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
            self.state
                .apply_anchor(self.options.anchor, self.options.margin);
            if let Err(e) = self.state.apply_empty_input_region() {
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
