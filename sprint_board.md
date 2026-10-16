# Sprint Board — Fokkus-Jarvis

**Épica 7:** Agente Visual Multiplataforma y Automatización Generalista
**Sprint:** HU-37 (tareas pesadas asíncronas con notificación secuencial)
**Actualizado:** 2026-10-16 · **PM/QA:** Claude · **PO:** Antigravity
**Contratos:** [HU-37](docs/specs/sdd-contracts/sdd-hu37-tareas-pesadas-asincronas-ipc.md) · [HU-36](docs/specs/sdd-contracts/sdd-hu36-burbujas-chat-efimeras-hud.md) · [HU-35](docs/specs/sdd-contracts/sdd-hu35-deteccion-pantalla-completa.md)

Leyenda: `[x]` hecho y verificado con tests · `[ ]` pendiente · 🧪 requiere prueba manual

---

## HU-37 — Tareas pesadas en segundo plano con notificación vocal secuencial vía IPC

### jarvis-core
- [x] 37.1 `paths::state_dir()` (`$XDG_STATE_HOME/fokkus-jarvis`, default `~/.local/state/fokkus-jarvis`) y subdirectorio `tasks/` [Responsable: DeepSeek]
- [x] 37.2 Campos en `JarvisConfig`: `tasks_max_parallel` (1..=4, default 2), `tasks_timeout_secs` (60..=3600, default 900), `tasks_quiet_gap_ms` (500..=5000, default 1500) [Responsable: DeepSeek]
- [x] 37.3 Tests de config y paths para state_dir [Responsable: DeepSeek]

### jarvis-audio
- [x] 37.4 `classify_workload(query: &str) -> Workload` (heurística pesada vs conversacional) [Responsable: DeepSeek]
- [x] 37.5 `SpeechPriority`, `Delivery`, `DeliveryGate` y `SpeechQueue` con `BinaryHeap` (prioridad > FIFO, tope 32) [Responsable: DeepSeek]
- [x] 37.6 `TaskRecord`, `TaskStatus`, `TaskOrigin`, `BrainFactory` trait y `TaskManager` con pool desacoplado [Responsable: DeepSeek]
- [x] 37.7 Integración en `BrainLoop`: acuse rápido (< 1s) para tareas pesadas, despacho a worker independiente sin bloqueo, tick de 250ms para drenar `SpeechQueue` con `DeliveryGate` [Responsable: DeepSeek · cierre: Claude]
- [x] 37.8 Comandos de voz locales: cancelación ("cancela la tarea de los correos", "cancela todas las tareas"), estado y pendientes [Responsable: DeepSeek · cierre: Claude]
- [x] 37.9 Tests unitarios de `SpeechQueue`, `classify_workload` y `TaskManager` [Responsable: DeepSeek]

### jarvis-ipc
- [x] 37.10 Métodos JSON-RPC: `jarvis.dispatchTask`, `jarvis.getTasks`, `jarvis.getTask`, `jarvis.cancelTask` [Responsable: Claude]
- [ ] 37.10b Notificación servidor→cliente `jarvis.taskUpdated` (requiere canal de push por conexión en `IpcServer`) [Responsable: pendiente]
- [x] 37.11 Soporte de cola en `jarvis.speak`: params `queue`, `priority` y respuesta `queued`, `position` [Responsable: Claude]
- [x] 37.12 Implementación del trait `TaskPort` sobre `ServerState` [Responsable: Claude]

### jarvis-app
- [x] 37.13 Implementación de `BrainFactory` en `jarvis-app` para generar instancias limpias e independientes de CLI/Claude [Responsable: Claude]
- [x] 37.14 Cableado de `TaskManager` en `main.rs` y pasarela hacia `BrainLoop` y servidor IPC [Responsable: Claude]
- [x] 37.15 Tests de integración completos y suite de QA `cargo test --workspace` [Responsable: Claude]
- [ ] 37.16 🧪 Prueba manual: "Jarvis, revisa mis correos de hoy y dime si hay algo urgente" con `agy`/`claude` real, conversar mientras corre y verificar entrega + archivo en `~/.local/state/fokkus-jarvis/tasks/` [Responsable: PO]
- [ ] 37.17 Timeout por tarea en modo API (`ClaudeClient` no tiene timeout total; hoy solo lo aplica `CliBrain`) y política "máx. 60 s de espera del gate → siguiente Idle" (CA-37.5) [Responsable: pendiente]

**QA 2026-10-16 (Claude):** `make test` → 474 passed / 0 failed (audio 241, ipc 67, core 61, app 44, shell 42, render 13, no_alloc 6) · `make lint` (cargo-deny + clippy `-D warnings`) OK · `make fmt-check` OK.


---

## HU-35 — Detección universal de pantalla completa y modo ventana sin bordes (Auto-Dim / Auto-Hide)

### jarvis-core
- [x] 35.1 `FullscreenBehavior` enum (`Ignore`, `Dim`, `Hide`) con serde lowercase y default `Dim` [Responsable: DeepSeek]
- [x] 35.2 `Rect` struct (`x`, `y`, `w`, `h`) y tipos `WindowSnapshot`, `OutputGeometry` [Responsable: DeepSeek]
- [x] 35.3 Función pura `is_effectively_fullscreen(&WindowSnapshot, &OutputGeometry) -> bool` (fullscreen o borderless >= 98% con +/-2px) [Responsable: DeepSeek]
- [x] 35.4 Campos en `JarvisConfig`: `fullscreen_behavior`, `fullscreen_dim_pct` (5..=100, default 25), `fullscreen_active_pct` (5..=100, default 60) [Responsable: DeepSeek]
- [x] 35.5 Tests unitarios exhaustivos para `is_effectively_fullscreen` y config (55 tests pasando ✅) [Responsable: DeepSeek]

### jarvis-shell
- [x] 35.6 Módulo `window_watch` con `WindowEvent`, `WatcherKind` y trait `WindowWatcher` [Responsable: Claude]
- [x] 35.7 Script de KWin `assets/kwin_fullscreen_watch.js` con escucha de eventos de ventana y reporte DBus [Responsable: Claude]
- [x] 35.8 `KwinScriptWatcher`: servicio DBus `io.fokkus.Jarvis.WindowWatch`, carga/descarga con `org.kde.kwin.Scripting` y emisión a crossbeam channel [Responsable: Claude]
- [x] 35.9 `NullWatcher` y fábrica `detect_watcher(jarvis_pid: u32, behavior: FullscreenBehavior)` [Responsable: Claude]
- [x] 35.10 Tests unitarios e integración para `window_watch` [Responsable: Claude]

### jarvis-app
- [x] 35.11 `PresenceDirective` (`Normal`, `Dimmed(Opacity)`, `Hidden`) y máquina de estados `FullscreenPolicy` con histéresis 300 ms y override vocal [Responsable: Claude]
- [x] 35.12 Integración en `run_loop.rs`: consumo de eventos de ventana, modulación de `auto_factor` de HU-34 / visibilidad de superficie [Responsable: Claude]
- [x] 35.13 Selector "Pantalla completa" en Ajustes Generales (`unified_settings.py`) [Responsable: Claude]
- [x] 35.14 Verificación integral `cargo test --workspace`, clippy y formato [Responsable: Claude]


---

## HU-34 — Opacidad y transparencia variable de la esfera

### jarvis-core
- [x] 34.1 `Opacity` (newtype `u8`): `OPAQUE`, `from_pct` (piso 20 %), `as_u8`, `mul`, `is_opaque` [Responsable: DeepSeek]
- [x] 34.2 `sphere_opacity_pct` en `JarvisConfig` (default 100, saturado al cargar) + `sphere_opacity()` [Responsable: DeepSeek]
- [x] 34.3 Fix de visibilidad: `Opacity::from_raw(u8)` (`const fn`) para fundidos y atenuación automática [Responsable: Claude]
- [x] 34.4 Getter `sphere_opacity_pct()` saturado (log de `apply_config`) [Responsable: Claude]

### jarvis-render
- [x] 34.5 `apply_opacity(&mut Pixmap, Opacity)` con `(c*a+127)/255` y atajo para opaco [Responsable: DeepSeek]
- [x] 34.6 `OpacityFade` (`new`, `retarget`, `value_at`, `is_settled`) [Responsable: DeepSeek]
- [x] 34.7 Migración a `Opacity::from_raw` (el campo ya no se construye desde fuera de `jarvis-core`) [Responsable: Claude]

### jarvis-shell
- [x] 34.8 `rgba_premul_to_argb8888_with_alpha(src, dst, opacity)`; `rgba_premul_to_argb8888` delega con `Opacity::OPAQUE` [Responsable: Claude]
- [x] 34.9 `OverlaySurface::set_opacity` (default no-op) [Responsable: Claude]
- [x] 34.10 `WaylandLayerSurface::set_opacity`: la opacidad se aplica en el pase de conversión a `wl_shm` (un solo recorrido) [Responsable: Claude]
- [x] 34.11 `HeadlessSurface::set_opacity` + getter `opacity()` para tests [Responsable: Claude]
- [x] 34.12 Tests: equivalencia con la ruta opaca, blanco 50 % → `(128,128,128,128)`, invariante premultiplicado en 10⁴ píxeles [Responsable: Claude]

### jarvis-app
- [x] 34.13 Slider "Opacidad" 20–100 % (paso 5) en General → Esfera [Responsable: Claude]
- [x] 34.14 Vista previa `{"preview":{"sphere_opacity_pct":Y}}` ≤ 30 Hz, aplicada con fundido de 150 ms en `run_loop` [Responsable: Claude]
- [x] 34.15 Composición con la atenuación automática de HU-35 (`user × auto`) — depende de HU-35 [Responsable: DeepSeek]
- [ ] 🧪 34.16 Prueba manual: esfera al 30 % sobre un editor, colores de estado distinguibles [Responsable: Claude]
- [ ] 34.17 Benchmark de sobrecoste (≤ 0.3 ms a 220 px, ≤ 1.2 ms a 440 px) [Responsable: DeepSeek]

---

## HU-33 — Tamaño dinámico de la burbuja en tiempo real

### jarvis-core
- [x] 33.1 `DEFAULT_SPHERE_SIZE_PX`, `SPHERE_SCALE_RANGE`, `sphere_scale_pct` (default 100, saturado al cargar) y `sphere_size_px()` [Responsable: DeepSeek]
- [x] 33.2 `sphere_size_for_pct(pct)` compartido por config y vista previa [Responsable: Claude]

### jarvis-render
- [x] 33.3 `SphereRenderer::resize(size)` en O(1) [Responsable: DeepSeek]

### jarvis-shell
- [x] 33.4 `OverlaySurface::set_size` (default `Err(BackendUnavailable("resize"))`) [Responsable: Claude]
- [x] 33.5 `WaylandLayerSurface::set_size`: `layer.set_size` + commit, espera del `configure` (timeout 250 ms con log único), reutiliza/crece el `SlotPool`, recalcula región de entrada circular y re-satura los márgenes de drag; si el compositor no responde, restaura el tamaño anterior [Responsable: Claude]
- [x] 33.6 `HeadlessSurface::set_size` (revalida los frames contra el tamaño nuevo) [Responsable: Claude]
- [x] 33.7 Tests: `circle_strips` a 110/440 px y márgenes re-saturados tras crecer [Responsable: Claude]

### jarvis-app
- [x] 33.8 Slider "Tamaño de la esfera" 50–200 % (paso 5) en General → Esfera, valor cargado del TOML y botón "Guardar" [Responsable: Claude]
- [x] 33.9 Emisión `{"preview":{"sphere_scale_pct":X}}` limitada a 30 Hz (`PreviewThrottle` con `QTimer`, coalescencia y flush al soltar) [Responsable: Claude]
- [x] 33.10 `UnifiedSettings` con `stdout` piped + hilo `jarvis-settings-preview` → `bounded(16)` descartando el más viejo; `parse_preview_line` y `poll_preview` [Responsable: Claude]
- [x] 33.11 `SphereView` (renderer + pixmap, reasigna solo al cambiar) y `SphereLook` en `run_loop`: como máximo un cambio de tamaño por frame [Responsable: Claude]
- [x] 33.12 Arranque con el tamaño/opacidad persistidos; `Saved` relee el TOML, `Cancelled`/proceso muerto revierte al persistido [Responsable: Claude]
- [x] 33.13 Tests: parseo (válido, fuera de rango, basura), lector con canal lleno, `SphereView::resize`, bucle con último valor por frame y reversión [Responsable: Claude]
- [ ] 33.14 Cerrar el `xdg_popup` del menú antes del resize y re-posicionar el HUD (HU-36) [Responsable: DeepSeek]
- [ ] 33.15 HiDPI: buffer en píxeles físicos con `buffer_scale` (CA-33.7) [Responsable: DeepSeek]
- [ ] 33.16 Test de no-asignación del bucle (1 000 frames sin resize → 0 asignaciones) [Responsable: DeepSeek]
- [ ] 🧪 33.17 Prueba manual en KWin: slider 50→200→50 sin parpadeo, clic en esquinas *click-through*, `--diag` a 200 % [Responsable: Claude]

---

## Verificación (2026-10-16)
- `cargo test --workspace`: ✅ 440 passed, 0 failed (2 ignored preexistentes)
- `cargo clippy --workspace --all-targets -- -D warnings`: ✅ limpio
- `cargo fmt --all -- --check`: ✅ limpio
- `cargo deny check`: ✅ limpio (advisories ok, bans ok, licenses ok, sources ok)
- `unified_settings.py` en Qt offscreen: selector "Pantalla completa" (`dim`, `hide`, `ignore`); ráfaga de 151 cambios → 2 líneas JSON; valores saturados y alineados al paso; TOML guardado con `sphere_scale_pct`/`sphere_opacity_pct`/`fullscreen_behavior`
