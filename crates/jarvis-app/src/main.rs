//! Punto de entrada de Fokkus-Jarvis.
//!
//! Une los crates `jarvis-core`, `jarvis-render` y `jarvis-shell`. En Fase 1
//! adquiere el lock de instancia única, carga la configuración, arranca el
//! núcleo y presenta la esfera en una superficie de prueba headless; el
//! backend real de ventana (wayland_layer / winit) llega en las tareas 1.4 y
//! 1.5.

use std::process::ExitCode;

use jarvis_core::single_instance::{self, InstanceError};
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
    // Instancia única: impide que corran dos procesos concurrentes (1.6).
    let _lock = match single_instance::acquire() {
        Ok(lock) => lock,
        Err(InstanceError::AlreadyRunning) => {
            eprintln!("Fokkus-Jarvis ya está en ejecución.");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("No se pudo adquirir el lock de instancia única: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Configuración persistente (1.6). Si el archivo está corrupto, se degrada
    // a los valores por defecto para no bloquear el arranque.
    let config = match JarvisConfig::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Configuración ilegible ({e}); usando valores por defecto.");
            JarvisConfig::default()
        }
    };

    let mut core = JarvisCore::new(config);
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
    let mut surface = match prepare_surface(&renderer) {
        Ok(surface) => surface,
        Err(e) => {
            eprintln!("No se pudo preparar la superficie headless: {e}");
            return ExitCode::FAILURE;
        }
    };
    surface.set_visible(true);

    if let Err(e) = present_frame(&renderer, &mut surface, &core, 0.0) {
        eprintln!("No se pudo presentar el primer frame: {e}");
        return ExitCode::FAILURE;
    }

    println!(
        "Esfera renderizada: {}x{} px (backend detectado: {:?})",
        renderer.size(),
        renderer.size(),
        SurfaceBackend::detect(),
    );

    // Nivel de audio de demostración (el medidor real llega con cpal en Issue #2).
    core.set_audio_level(0.35);

    // Demo de la máquina de estados en el flujo normal de conversación.
    let mut t = 0.0;
    for target in [
        JarvisState::Listening,
        JarvisState::Thinking,
        JarvisState::Speaking,
        JarvisState::Idle,
    ] {
        if let Err(e) = core.transition(target) {
            eprintln!("Transición inválida a {target:?} en la demo: {e}");
            return ExitCode::FAILURE;
        }
        if let Err(e) = present_frame(&renderer, &mut surface, &core, t) {
            eprintln!("No se pudo presentar el frame del estado {target:?}: {e}");
            return ExitCode::FAILURE;
        }
        println!("  estado={:?}", core.state());
        t += 1.0 / 60.0;
    }

    // Drenamos los eventos publicados por el bus (transiciones y audio).
    while let Ok(transition) = core.bus().state_receiver().try_recv() {
        println!("  [evento] {:?} -> {:?}", transition.from, transition.to);
    }
    println!("  [audio] level={:.3}", core.audio_level());

    ExitCode::SUCCESS
}

fn prepare_surface(renderer: &SphereRenderer) -> Result<HeadlessSurface, SurfaceError> {
    let size = SurfaceSize::new(renderer.size(), renderer.size())?;
    Ok(HeadlessSurface { size })
}

fn present_frame(
    renderer: &SphereRenderer,
    surface: &mut HeadlessSurface,
    core: &JarvisCore,
    t: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    let frame = renderer.render(core.audio_level(), core.state(), t)?;
    let frame_buffer = FrameBuffer::new(frame.data(), frame.width(), frame.height())?;
    surface.present(&frame_buffer)?;
    Ok(())
}
