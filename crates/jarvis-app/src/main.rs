//! Punto de entrada de Fokkus-Jarvis.
//!
//! Une los crates `jarvis-core`, `jarvis-render` y `jarvis-shell`. En Fase 1
//! arranca el núcleo, renderiza la esfera y la presenta en una superficie de
//! prueba headless; el backend real de ventana (wayland_layer / winit) llega
//! en las tareas 1.4 y 1.5.

use std::process::ExitCode;

use jarvis_core::{JarvisConfig, JarvisCore, JarvisState};
use jarvis_render::SphereRenderer;
use jarvis_shell::{
    Anchor, FrameBuffer, OverlaySurface, SurfaceBackend, SurfaceError, SurfaceSize,
};

/// Superficie de prueba que valida los frames sin abrir una ventana real.
struct HeadlessSurface {
    size: SurfaceSize,
}

impl OverlaySurface for HeadlessSurface {
    fn size(&self) -> SurfaceSize {
        self.size
    }

    fn present(&mut self, frame: &FrameBuffer<'_>) -> Result<(), SurfaceError> {
        let expected = self.size.width as usize * self.size.height as usize * 4;
        if frame.data.len() != expected {
            return Err(SurfaceError::InvalidFrame {
                expected,
                actual: frame.data.len(),
            });
        }
        Ok(())
    }

    fn set_visible(&mut self, _visible: bool) {}

    fn set_anchor(&mut self, _anchor: Anchor) {}
}

fn main() -> ExitCode {
    let mut core = JarvisCore::new(JarvisConfig::default());
    println!(
        "Fokkus-Jarvis {} — estado={:?} · voz={} ({}) · gaming={} · escucha_continua={}",
        env!("CARGO_PKG_VERSION"),
        core.state(),
        core.config().voice_id,
        core.config().voice_lang,
        core.config().gaming_mode,
        core.config().continuous_listening,
    );

    let renderer = SphereRenderer::new();
    let frame = renderer.render(0.0, core.state());

    let mut surface = HeadlessSurface {
        size: SurfaceSize::new(frame.width(), frame.height()).expect("tamaño de esfera válido"),
    };
    surface.set_visible(true);
    surface
        .present(
            &FrameBuffer::new(frame.data(), frame.width(), frame.height())
                .expect("frame RGBA válido"),
        )
        .expect("presentación del primer frame");

    println!(
        "Esfera renderizada: {}x{} px · {} bytes RGBA (backend detectado: {:?})",
        frame.width(),
        frame.height(),
        frame.data().len(),
        SurfaceBackend::detect(),
    );

    // Nivel de audio de demostración (el medidor real llega con cpal en Issue #2).
    core.set_audio_level(0.35);

    // Demo de la máquina de estados en el flujo normal de conversación.
    for target in [
        JarvisState::Listening,
        JarvisState::Thinking,
        JarvisState::Speaking,
        JarvisState::Idle,
    ] {
        core.transition(target)
            .expect("transición válida en el flujo de conversación");
        let f = renderer.render(0.6, core.state());
        println!(
            "  estado={:?} · frame={}x{} px",
            core.state(),
            f.width(),
            f.height()
        );
    }

    // Drenamos los eventos publicados por el bus (transiciones y audio).
    while let Ok(t) = core.bus().state_receiver().try_recv() {
        println!("  [evento] {:?} -> {:?}", t.from, t.to);
    }
    while let Ok(level) = core.bus().audio_level_receiver().try_recv() {
        println!("  [audio] level={:.3}", level);
    }

    ExitCode::SUCCESS
}
