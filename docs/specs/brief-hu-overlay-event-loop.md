# Brief técnico — Bucle principal + overlay Wayland real (Fokkus-Jarvis 0.1.2)

> Preparado por: Claude Code (PM/QA) · Destinatario: DeepSeek V4 Pro (Dev Senior)
> Fecha: 2026-10-15 · Repositorio: `/home/juancarlos/Proyectos/Personales/Fokkus-Jarvis`

## Contexto de negocio

El usuario instaló Fokkus-Jarvis en su equipo (Fedora, KDE Plasma con KWin Wayland) y
**no funciona**:

1. **El proceso termina en menos de 0,5 s.** `run()` (`crates/jarvis-app/src/main.rs`)
   arranca los subsistemas, imprime "iniciado correctamente" y devuelve
   `ExitCode::SUCCESS`. No hay bucle de eventos: al salir se liberan los recursos y el
   `Drop` de `IpcServer` borra `~/.fokkus/jarvis.sock`. Fokkus IDE no puede conectarse nunca.
2. **No hay ventana.** `prepare_surface()` devuelve un `HeadlessSurface` que solo valida el
   tamaño del buffer. La esfera se dibuja en memoria y nunca llega a la pantalla.
3. **Si falta whisper-cli/whisper-cpp**, el asistente debe seguir funcionando en modo
   degradado, sin terminar ni bloquear el bucle.

El objetivo es que, al ejecutar `fokkus-jarvis`, aparezca una esfera flotante transparente
siempre visible en la esquina inferior derecha, que el proceso siga activo como daemon con
el socket IPC disponible y que se cierre de forma limpia con Ctrl+C / `systemctl stop` / `kill`.

## Estado de partida (ya hecho por QA, no tocar)

| Ítem | Estado |
|---|---|
| `Cargo.toml` (workspace) | ✅ `smithay-client-toolkit = { version = "0.21", default-features = false, features = ["calloop"] }` y `signal-hook = "0.4"` |
| `crates/jarvis-shell/Cargo.toml` | ✅ SCTK solo bajo `[target.'cfg(target_os = "linux")'.dependencies]` |
| `crates/jarvis-app/Cargo.toml` | ✅ `signal-hook` solo bajo `[target.'cfg(unix)'.dependencies]` |
| `make check` / `make lint` / `make test` | ✅ 149 tests en verde, 0 warnings de clippy |
| `cargo check --target x86_64-pc-windows-gnu` | ✅ compila (las dependencias nuevas no afectan a Windows) |

Notas sobre las dependencias:
- SCTK se usa **sin features por defecto** para no enlazar `libxkbcommon` (todavía no se
  procesa el teclado). El backend de `wayland-client` es Rust puro: no hace falta `libwayland-dev`.
- **No añadir `calloop` como dependencia directa.** Usar el que reexporta SCTK
  (`smithay_client_toolkit::reexports::calloop` y `...::reexports::calloop_wayland_source`)
  para evitar dos versiones incompatibles.
- No se permite ninguna dependencia nueva sin aprobación (en particular `winit`, `ctrlc`,
  `tokio`, `libc`, `nix`). Si hiciera falta alguna, se justifica en el PR.

## Reglas obligatorias

- Rust 2021, MSRV 1.99, `#![deny(unsafe_code)]` implícito: **cero `unsafe`**.
- Sin `unwrap()`/`expect()` fuera de los tests. Errores tipados (`SurfaceError`, etc.).
- Comentarios y mensajes en **español**, con la misma densidad y estilo que el código actual.
- Todo lo de Wayland queda bajo `#[cfg(target_os = "linux")]`; Windows y macOS deben seguir
  compilando.
- Usar siempre el `Makefile`: `make check`, `make lint`, `make test`, `make fmt-check`.
- Sin commits ni push (los autoriza el usuario).

---

## HU-1 — Bucle principal con apagado limpio

- 📁 **Repositorio:** `Fokkus-Jarvis` → `crates/jarvis-app/src/main.rs` (y un módulo nuevo `crates/jarvis-app/src/runtime.rs` si conviene separar el bucle)
- 🎯 **Capa:** Backend / runtime del binario
- 📌 **Objetivo técnico:** convertir `run()` en un daemon que se mantenga vivo hasta recibir una señal o hasta que el compositor cierre la superficie, animando la esfera a unos 60 FPS.

### Arquitectura del bucle

```
arranque (lock → config → núcleo/TTS/coordinador → IPC → fillers → superficie → STT)
        │
        ▼
loop {
   1. ¿shutdown solicitado (AtomicBool)?           → salir
   2. surface.pump_events(timeout_hasta_próximo_frame)
        └─ Closed (el compositor cerró la capa)      → salir
   3. estado = server_state.state()                 (fuente de verdad, ver HU-4)
      nivel  = coordinator.core().audio_level()
   4. si surface.wants_frame():
        renderer.render_into(&mut pixmap, nivel, estado, t)   ← pixmap reutilizado
        surface.present(&FrameBuffer::new(pixmap.data(), …))
   5. FrameClock::next_deadline()                    (16,67 ms; 33 ms en Idle/Muted)
}
apagado ordenado
```

### Requisitos

1. **Señales:** registrar `SIGINT` y `SIGTERM` con `signal_hook::flag::register` sobre un
   `Arc<AtomicBool>` compartido (solo `cfg(unix)`; en Windows basta un stub que nunca se
   activa por ahora). Prohibido el `std::process::exit` dentro de los handlers.
2. **Ritmo de frames:** extraer un `FrameClock` puro (sin E/S) que reciba `Instant` y
   devuelva el siguiente deadline y el `t` de animación en segundos. Objetivo de 60 FPS
   (16,67 ms) cuando el estado es `Listening/Thinking/Speaking` y de 30 FPS en `Idle/Muted`
   para ahorrar CPU. Si un frame llega tarde, **no** recuperar los frames perdidos: se
   reprograma desde `now`.
3. **Sin bloqueos ni esperas activas:** el único punto de espera es
   `pump_events(timeout)`. Nada de `thread::sleep` en caliente salvo en la superficie
   headless, que sí debe dormir hasta el deadline para no consumir el 100 % de un núcleo.
4. **Cero asignaciones por frame:** un único `Pixmap` creado antes del bucle y
   `SphereRenderer::render_into`. El buffer `wl_shm` también se reutiliza (HU-2).
5. **Bucle testeable:** el bucle debe ser una función que reciba
   `&mut dyn OverlaySurface`, el flag de apagado y un `Option<u64>` de frames máximos
   (para tests), y que devuelva un `LoopExit { Signal, SurfaceClosed, MaxFrames }`.
6. **Apagado ordenado** (en este orden): ocultar/destruir la superficie → coordinador →
   `server.shutdown()` (debe borrar el socket) → soltar el lock de instancia única.
   Código de salida `0` en `Signal` y `SurfaceClosed`. El apagado tras Ctrl+C debe tardar
   **< 500 ms**.
7. Los fallos durante el bucle (un `present` que falla puntualmente) se registran **una vez**
   por tipo de error (sin inundar stderr a 60 Hz) y no detienen el bucle. Solo
   `SurfaceClosed` o la señal lo terminan.
8. El mensaje "iniciado correctamente" se imprime **después** de presentar el primer frame
   real y entrar en el bucle, no antes.

---

## HU-2 — Superficie Wayland real con `zwlr_layer_shell_v1`

- 📁 **Repositorio:** `Fokkus-Jarvis` → `crates/jarvis-shell/src/` (`lib.rs`, módulos nuevos `wayland_layer.rs` y `headless.rs`)
- 🎯 **Capa:** Shell / integración con el compositor
- 📌 **Objetivo técnico:** implementar `OverlaySurface` sobre una capa overlay de layer-shell que muestre el `FrameBuffer` RGBA premultiplicado con transparencia real, siempre encima y sin decoraciones.

### Cambios en el contrato (`lib.rs`)

Ampliar el trait `OverlaySurface` (con implementaciones por defecto cuando tenga sentido)
para que el bucle no dependa del backend:

| Método | Semántica |
|---|---|
| `fn pump_events(&mut self, timeout: Duration) -> Result<SurfaceStatus, SurfaceError>` | Despacha los eventos pendientes esperando como máximo `timeout`. `SurfaceStatus::{Running, Closed}` |
| `fn wants_frame(&self) -> bool` | `true` si el compositor pidió un frame nuevo (frame callback) o si el primer frame está pendiente |
| `fn backend(&self) -> SurfaceBackend` | Backend efectivo, para el log y `--check` |

Mover `HeadlessSurface` desde `main.rs` a `jarvis-shell/src/headless.rs` (pública), de modo
que `pump_events` duerma hasta `timeout` y `wants_frame` devuelva siempre `true`.

Añadir un constructor de fábrica: `pub fn open_overlay(opts: OverlayOptions) -> Result<Box<dyn OverlaySurface>, SurfaceError>`, que en Linux con `WAYLAND_DISPLAY` intente
layer-shell y, si no puede, devuelva `SurfaceError::BackendUnavailable(..)` para que la app
decida el fallback.

### Requisitos de `WaylandLayerSurface` (solo `cfg(target_os = "linux")`)

1. **Conexión:** `Connection::connect_to_env()` → `registry_queue_init` → enlazar
   `CompositorState`, `Shm` y `LayerShell`. Si `LayerShell::bind` falla (por ejemplo en GNOME
   Mutter), devolver `BackendUnavailable("zwlr_layer_shell_v1")`: **nunca** un pánico.
2. **Capa:** `Layer::Overlay`, namespace `"fokkus-jarvis"`, tamaño `OverlayOptions::size`
   (220×220 por defecto; el renderer usa `renderer.size()`, que debe coincidir: validarlo),
   `exclusive_zone = 0`, márgenes `OverlayOptions::margin` en los dos bordes anclados.
3. **Anclaje:** función pura `anchor_to_layer(Anchor) -> sctk::shell::wlr_layer::Anchor`
   (`BottomRight → BOTTOM | RIGHT`, `Center → vacío`, etc.) con test unitario.
   `set_anchor` reaplica el anclaje y los márgenes y hace commit.
4. **Teclado:** `KeyboardInteractivity::None` en esta HU (el `OnDemand` por defecto de
   `OverlayOptions` se mapea, pero no se procesa el teclado: no hay xkbcommon). Documentar el
   mapeo.
5. **Entrada de ratón:** región de entrada vacía (`wl_compositor.create_region()` sin
   rectángulos → `set_input_region`) para que la esfera **no capture clics** sobre el
   escritorio. El arrastre llega en otra tarea.
6. **Configure:** no presentar nada hasta recibir el primer `configure`. Si el compositor
   propone un tamaño distinto de 0 y del pedido, registrarlo y seguir usando el tamaño
   propio (la esfera tiene tamaño fijo).
7. **Buffer:** `SlotPool` de `wl_shm` con formato **`Argb8888`**. Ojo: tiny-skia entrega
   **RGBA premultiplicado** y `Argb8888` en little-endian es **BGRA en memoria**, también
   premultiplicado. Implementar una función pura
   `rgba_premul_to_argb8888(src: &[u8], dst: &mut [u8])` (intercambiar R↔B, conservar A) con
   tests, incluido un píxel totalmente transparente (`[0,0,0,0]`). No despremultiplicar.
   Reutilizar el buffer: si el compositor todavía lo tiene ocupado (sin `release`), usar
   un segundo slot (doble buffer) en lugar de asignar uno en cada frame.
8. **Presentación:** `attach` → `damage_buffer(0,0,w,h)` → `frame()` (pedir callback) →
   `commit`. `wants_frame()` pasa a `false` tras el commit y vuelve a `true` al llegar el
   callback `done`: así KWin regula el ritmo y no se dibuja cuando la superficie no es visible.
9. **Bucle de eventos:** dentro de la superficie, un `calloop::EventLoop` (el reexportado por
   SCTK) con `WaylandSource`; `pump_events(timeout)` = `event_loop.dispatch(timeout, &mut state)`
   y después `flush` de la conexión. Un error de conexión (compositor caído) devuelve `Closed`.
10. **Cierre:** el evento `LayerSurface::closed` → `SurfaceStatus::Closed`.
11. **Visibilidad:** `set_visible(false)` adjunta un buffer nulo y hace commit;
    `set_visible(true)` vuelve a presentar en el siguiente frame.
12. **Detección:** `SurfaceBackend::detect()` mantiene la heurística actual; la sonda real es
    el propio `open_overlay`.

### Fallback en la app

En `main.rs`, `prepare_surface()` deja de devolver error fatal:
- layer-shell OK → `WaylandLayerSurface`.
- `BackendUnavailable` o sin `WAYLAND_DISPLAY` → aviso por stderr
  (`"[shell] overlay no disponible (…); modo headless, solo IPC"`) y `HeadlessSurface`.
  **El proceso sigue vivo** como daemon IPC. (El backend `winit` para X11/Windows/macOS queda
  fuera del alcance de esta HU.)

---

## HU-3 — STT degradado sin whisper

- 📁 **Repositorio:** `Fokkus-Jarvis` → `crates/jarvis-app/src/main.rs` (el crate `jarvis-audio` no cambia de API)
- 🎯 **Capa:** Backend / audio
- 📌 **Objetivo técnico:** que la ausencia de `whisper-cli`/`whisper-cpp` o de su modelo se detecte una sola vez al arrancar y deje al asistente en modo degradado sin afectar al bucle.

Requisitos:
1. Sonda única al arrancar con `WhisperEngine::new()`: binario (`binary()`) y modelo
   (`model().is_some_and(Path::is_file)`). Resultado en un enum `SttAvailability { Ready, MissingBinary, MissingModel }`.
2. Si no está `Ready`: aviso **una sola vez**
   (`"[stt] deshabilitado: falta whisper-cli/whisper-cpp en PATH"` o `"… sin modelo local en <ruta>"`),
   **no** llamar a `coordinator.start_listening()` (la esfera queda en `Idle`) y **nunca**
   invocar `transcribe()` desde el bucle.
3. Si está `Ready`: `start_listening()` como hasta ahora, pero un error de transición se
   registra como aviso y **no** termina el proceso (hoy devuelve `ExitCode::FAILURE`).
4. La línea de arranque incluye `STT=<estado>`.

---

## HU-4 — Coherencia del estado entre IPC, coordinador y render (hallazgo de QA)

- 📁 **Repositorio:** `Fokkus-Jarvis` → `crates/jarvis-app/src/main.rs`
- 🎯 **Capa:** Backend / orquestación
- 📌 **Objetivo técnico:** que la esfera refleje el estado real del asistente.

Hoy `run()` crea **tres** `JarvisCore` independientes: `core` (el que se renderiza y que
nunca cambia), el interno de `ConversationCoordinator` y el de `ServerState` (el que muta
Fokkus IDE con `set_muted`, etc.). Así, un `jarvis.setMuted` desde el IDE nunca cambia el
color de la esfera.

Requisito mínimo de esta HU: eliminar el `core` suelto de `main.rs` y que el render lea el
estado desde `ServerState::state()` (fuente de verdad del IPC) y el nivel de audio desde
el coordinador. Unificar completamente el núcleo (un único `JarvisCore` compartido) queda
como deuda técnica registrada en `backlog.md`, salvo que DeepSeek lo resuelva sin cambiar
APIs públicas de `jarvis-ipc`/`jarvis-audio`.

---

## Extra opcional — `--check`

Añadir la sonda `"superficie"`: `OK` si layer-shell se enlaza (abrir la conexión y
cerrarla sin mapear la capa), `AVISO` si no hay layer-shell o no hay `WAYLAND_DISPLAY`.

---

## Criterios de aceptación (QA los validará)

### Automáticos (`make fmt-check && make lint && make test`)

| # | Prueba | Tipo |
|---|---|---|
| A1 | `rgba_premul_to_argb8888`: intercambio R↔B, alfa intacto, transparente → `[0,0,0,0]`, longitudes distintas → error | unit `jarvis-shell` |
| A2 | `anchor_to_layer` cubre las 5 variantes de `Anchor` | unit `jarvis-shell` |
| A3 | `FrameClock`: 60 FPS en estados activos, 30 FPS en `Idle/Muted`, sin recuperar frames tras un retraso | unit `jarvis-app` |
| A4 | Bucle con `HeadlessSurface` y `max_frames = N` → `LoopExit::MaxFrames`, N frames presentados | unit `jarvis-app` |
| A5 | Bucle con el flag de apagado activado desde otro hilo → `LoopExit::Signal` en < 500 ms | unit `jarvis-app` |
| A6 | `HeadlessSurface` (ya en `jarvis-shell`) mantiene la validación de longitud del frame | unit `jarvis-shell` |
| A7 | `SttAvailability` sin binario → no se entra en `Listening` | unit `jarvis-app` |
| A8 | `cargo check --workspace --target x86_64-pc-windows-gnu` sigue compilando | build |
| A9 | Los 149 tests existentes siguen en verde; clippy `-D warnings` sin avisos | regresión |

### Manuales en KDE Plasma / KWin Wayland

| # | Paso | Esperado |
|---|---|---|
| M1 | `fokkus-jarvis` | El proceso sigue vivo; la esfera aparece abajo a la derecha, con fondo transparente, sin bordes y encima de todas las ventanas |
| M2 | `ls -l ~/.fokkus/jarvis.sock` con el proceso vivo | El socket existe con permisos `srw-------` |
| M3 | Clic sobre la zona de la esfera | El clic llega a la ventana de debajo |
| M4 | `Ctrl+C` / `kill -TERM <pid>` | Sale con código 0 en < 500 ms y el socket desaparece |
| M5 | `top` en `Idle` | CPU < 5 % de un núcleo |
| M6 | `PATH` sin whisper | Un solo aviso `[stt] deshabilitado`, el proceso sigue vivo y la esfera en `Idle` |
| M7 | `env -u WAYLAND_DISPLAY fokkus-jarvis` | Aviso de modo headless, el proceso sigue vivo con IPC |
| M8 | Segunda instancia | `Fokkus-Jarvis ya está en ejecución.` y código 1 |

## Fuera de alcance

Backend `winit` (X11/Windows/macOS), arrastre de la esfera, teclado/atajos globales,
captura de micrófono (`cpal`) y wake-word.
