# Contrato SDD — HU-35: Detección universal de pantalla completa y modo ventana sin bordes (Auto-Dim / Auto-Hide)

**Proyecto:** Fokkus-Jarvis
**Épica:** 6 — Experiencia Visual Adaptativa y Personalización de Interfaz
**Redactado por:** Claude (PM/QA)
**Destinatario:** Swarm de desarrollo (Dev Senior)
**Fecha:** 2026-10-16
**Versión base del código:** `v0.2.14`
**Estado:** Borrador para aprobación del PO — **requiere decisión D-35.1 (dependencia D-Bus)**
**Contratos relacionados:** [HU-34](sdd-hu34-opacidad-transparencia-esfera.md) (factor de atenuación), [HU-36](sdd-hu36-burbujas-chat-efimeras-hud.md) (el HUD se oculta), [HU-38](sdd-hu38-ojos-manos-multiplataforma.md) (reutiliza el monitor de ventana activa)

---

## 1. Contexto

La esfera vive en la capa `overlay` de `zwlr_layer_shell_v1`, que por diseño queda **encima incluso de las ventanas a pantalla completa**. Al ver un video en YouTube, jugar en Steam (pantalla completa exclusiva o *borderless*) o presentar, Jarvis estorba. Debe detectarlo de forma genérica y atenuarse u ocultarse sin dejar de escuchar.

### 1.1. Restricción clave de Wayland

Un cliente Wayland **no puede** consultar el estado de ventanas ajenas: el protocolo lo impide por seguridad. Cada familia de compositor ofrece una vía distinta:

| Entorno | Fuente fiable | Privilegio |
|---|---|---|
| **KDE Plasma 6 / KWin Wayland** (equipo de referencia) | **Script de KWin** cargado por D-Bus (`org.kde.kwin.Scripting.loadScript`) que escucha `workspace.windowActivated`, `window.fullScreenChanged` y `frameGeometryChanged`, y reporta por `callDBus` a un servicio de sesión de Jarvis. | Ninguno extra: cualquier proceso de la sesión puede cargar scripts. |
| wlroots (Hyprland, Sway, river) | `zwlr_foreign_toplevel_management_v1` (estado `fullscreen` + `activated`). | Protocolo público del compositor. |
| GNOME Mutter | Sin vía pública; la extensión Shell queda fuera de alcance. Degrada a "sin detección". | — |
| X11 | `_NET_ACTIVE_WINDOW` + `_NET_WM_STATE_FULLSCREEN` + geometría vs. monitor (`x11rb`). | Ninguno. |
| Windows | `SHQueryUserNotificationState` (`QUNS_BUSY`, `QUNS_RUNNING_D3D_FULL_SCREEN`, `QUNS_PRESENTATION_MODE`) + `GetForegroundWindow`/`MonitorFromWindow` para *borderless*. | Ninguno. |

### 1.2. Detección de "ventana sin bordes" (borderless)

Muchos juegos y reproductores no activan el estado `fullscreen`: crean una ventana sin decoraciones del tamaño exacto del monitor. Regla común a todos los backends:

> Una ventana activa es **efectivamente pantalla completa** si (a) declara estado fullscreen, **o** (b) no tiene decoraciones y su geometría cubre ≥ 98 % del área del output donde está, con tolerancia de ±2 px por borde.

---

## 2. Criterios de aceptación (Given-When-Then)

**CA-35.1 — Video a pantalla completa**
- *Given* `fullscreen_behavior = "dim"` y la esfera en el monitor 1
- *When* el usuario pone un video de YouTube a pantalla completa en el monitor 1
- *Then* en ≤ 500 ms la esfera baja a `fullscreen_dim_pct` (default 25 %) con fundido de 150 ms; al salir, recupera la opacidad de HU-34.

**CA-35.2 — Juego borderless**
- *Given* un juego de Steam en modo ventana sin bordes que cubre el monitor
- *When* la ventana está activa
- *Then* se aplica la misma acción que en CA-35.1.

**CA-35.3 — Auto-Hide**
- *Given* `fullscreen_behavior = "hide"`
- *When* se detecta pantalla completa en el monitor de la esfera
- *Then* la superficie se oculta (`set_visible(false)`), **la escucha sigue activa** y el log registra `[pantalla] oculta por pantalla completa (<app_id>)`.

**CA-35.4 — Interacción por voz estando oculta/atenuada**
- *Given* la esfera oculta o atenuada por pantalla completa
- *When* el usuario dice el wake-word y Jarvis pasa a `Listening`/`Thinking`/`Speaking`
- *Then* la esfera se muestra a `fullscreen_active_pct` (default 60 %) mientras dura el turno y vuelve a ocultarse/atenuarse 2 s después de volver a `Idle`.

**CA-35.5 — Multimonitor**
- *Given* la esfera en el monitor 1
- *When* la ventana a pantalla completa está en el monitor 2
- *Then* no se aplica ninguna acción.

**CA-35.6 — Histéresis**
- *Given* el usuario hace Alt+Tab rápido entre un juego y el escritorio
- *When* el estado cambia varias veces en < 300 ms
- *Then* solo se aplica el estado estable tras 300 ms (sin parpadeo).

**CA-35.7 — Degradación honesta**
- *Given* un entorno sin fuente de detección (GNOME Wayland)
- *When* arranca el daemon
- *Then* `--check` reporta `detección de pantalla completa: no disponible (GNOME/Mutter)` y no se aplica nada; nunca se finge detección.

**CA-35.8 — Apagable**
- *Given* `fullscreen_behavior = "ignore"`
- *Then* no se carga el script de KWin ni se crea ningún monitor.

**CA-35.9 — Limpieza**
- *Given* el daemon se cierra (señal, menú "Salir" o *crash* recuperado en el siguiente arranque)
- *Then* el script de KWin se descarga (`unloadScript`) y no quedan scripts huérfanos `fokkus-jarvis-*` en KWin.

---

## 3. Arquitectura técnica por crate

### 3.1. `jarvis-core`
- `FullscreenBehavior { Ignore, Dim, Hide }` (serde lowercase, default `Dim`).
- Campos: `fullscreen_behavior`, `fullscreen_dim_pct: u8` (5–100, default 25), `fullscreen_active_pct: u8` (default 60).
- Tipo de dominio `WindowSnapshot` y la función **pura** `is_effectively_fullscreen(&WindowSnapshot, &OutputGeometry) -> bool` (regla de §1.2): testeable sin compositor.

### 3.2. `jarvis-shell` — módulo nuevo `window_watch`
- Trait `WindowWatcher` con backends:
  - `KwinScriptWatcher` (Linux, KDE): escribe un script JS embebido (`include_str!("../assets/kwin_fullscreen_watch.js")`) en `$XDG_RUNTIME_DIR/fokkus-jarvis/`, lo carga con `loadScript(path, "fokkus-jarvis-fswatch")`, lo arranca con `run`, y expone el servicio D-Bus de sesión `io.fokkus.Jarvis.WindowWatch` con el método `Report(a{sv})` que el script invoca vía `callDBus`.
  - `WlrToplevelWatcher` (wlroots): enlaza `zwlr_foreign_toplevel_manager_v1` en la **misma conexión** Wayland de la capa (sin hilos extra).
  - `X11Watcher`, `WindowsWatcher` (cfg por plataforma; Windows con polling de 500 ms).
  - `NullWatcher` (sin detección).
- `WindowWatcher::detect()` elige el backend por `XDG_CURRENT_DESKTOP`, globals Wayland anunciados y plataforma.
- El watcher emite `WindowEvent` por `crossbeam_channel`; nunca bloquea el bucle de render.
- La salida (`wl_output`) de la esfera se conoce por `surface_enter`/`surface_leave` (ya implementados en `wayland_layer.rs`).

### 3.3. `jarvis-app`
- `FullscreenPolicy` (máquina de estados pura): entra `WindowEvent` + estado de Jarvis + tiempo; sale `PresenceDirective { Normal, Dimmed(Opacity), Hidden }`. Implementa histéresis (300 ms) y la ventana de "turno activo" (CA-35.4).
- El bucle traduce la directiva a `auto_factor` de HU-34 o a `set_visible`.
- Se expone `fullscreen: bool` en `jarvis.getState` (campo opcional, compatibilidad hacia atrás).

### 3.4. Script de KWin (contrato)
```js
// assets/kwin_fullscreen_watch.js (resumen del contrato, no implementación final)
function report(w) {
  if (!w) return;
  const out = w.output.geometry;
  callDBus("io.fokkus.Jarvis.WindowWatch", "/WindowWatch",
           "io.fokkus.Jarvis.WindowWatch", "Report",
           { app_id: w.resourceClass, caption_hash: hash(w.caption),
             fullscreen: w.fullScreen, decorated: !w.noBorder,
             x: w.frameGeometry.x, y: w.frameGeometry.y,
             w: w.frameGeometry.width, h: w.frameGeometry.height,
             output: w.output.name, ox: out.x, oy: out.y, ow: out.width, oh: out.height,
             own: w.pid === JARVIS_PID });
}
workspace.windowActivated.connect(report);
// + fullScreenChanged / frameGeometryChanged del activo
```
El script **no envía el título** de la ventana (privacidad), solo un hash para diagnóstico.

---

## 4. Interfaces y structs Rust

```rust
// jarvis-core
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FullscreenBehavior { Ignore, #[default] Dim, Hide }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rect { pub x: i32, pub y: i32, pub w: u32, pub h: u32 }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowSnapshot {
    pub app_id: String,
    pub fullscreen: bool,
    pub decorated: bool,
    pub geometry: Rect,
    pub output: String,
    /// La ventana pertenece a Fokkus-Jarvis (ajustes, panel): se ignora.
    pub own: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputGeometry { pub name: String, pub rect: Rect }

pub fn is_effectively_fullscreen(w: &WindowSnapshot, out: &OutputGeometry) -> bool;

// jarvis-shell::window_watch
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowEvent {
    Active(WindowSnapshot, OutputGeometry),
    /// Sin ventana activa (escritorio vacío).
    None,
    /// La fuente se cayó (KWin reiniciado, D-Bus perdido).
    SourceLost(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherKind { KwinScript, WlrToplevel, X11, Windows, Unavailable }

pub trait WindowWatcher: Send {
    fn kind(&self) -> WatcherKind;
    fn events(&self) -> crossbeam_channel::Receiver<WindowEvent>;
    fn shutdown(&mut self);
}

pub fn detect_watcher(jarvis_pid: u32) -> Box<dyn WindowWatcher>;

// jarvis-app
pub enum PresenceDirective { Normal, Dimmed(Opacity), Hidden }

pub struct FullscreenPolicy { /* behavior, pcts, hysteresis, pending, active_turn_until */ }
impl FullscreenPolicy {
    pub fn new(cfg: &JarvisConfig) -> Self;
    pub fn on_window(&mut self, ev: &WindowEvent, sphere_output: Option<&str>, now: Instant);
    pub fn on_state(&mut self, state: JarvisState, now: Instant);
    pub fn directive(&self, now: Instant) -> PresenceDirective;
}
```

---

## 5. Casos de borde

| Caso | Comportamiento |
|---|---|
| La ventana activa es la propia de Jarvis (Ajustes PyQt, Panel Rápido) | `own = true` → se ignora (los ajustes no deben ocultar la esfera que se está configurando). |
| KWin se reinicia (`kwin_wayland --replace`) | `SourceLost` → `Normal`; reintento de carga del script con backoff (1 s, 5 s, 30 s). |
| Script huérfano de una ejecución anterior que crasheó | Al arrancar se hace `unloadScript("fokkus-jarvis-fswatch")` antes de cargar. |
| Juego en pantalla completa **exclusiva** con *direct scanout* | La capa overlay puede no componerse; igual se aplica la directiva para que al salir el estado sea coherente. |
| Ventana maximizada con decoraciones (no fullscreen) | No cuenta: `decorated = true` y no declara fullscreen. |
| Presentación de LibreOffice / visor PDF en modo presentación | Cuenta como fullscreen (declara el estado). |
| Llamada de Meet/Zoom compartiendo pantalla | Fuera de alcance de esta HU (se anota para HU futura "modo compartir pantalla"). |
| `gaming_mode = true` | Independiente: no se activa automáticamente. Se deja como mejora futura (decisión del PO). |
| HUD (HU-36) | Con `Dimmed` o `Hidden` el HUD se oculta; reaparece con CA-35.4. |
| Dos monitores con escala distinta | La comparación se hace en coordenadas lógicas del compositor (las que reporta KWin). |

---

## 6. Decisiones que debe tomar el PO

- **D-35.1 — Servidor D-Bus de sesión.** El backend KDE necesita *exponer* un objeto D-Bus para recibir `callDBus`. Opciones: (a) `zbus` con API `blocking` (Rust puro; arrastra `async-io`/`event-listener`, sin tokio) — requiere análisis de seguridad con el formato de la §3 de `sdd-conversacion-nativa-v0.2.0.md`; (b) `dbus` (bindings a `libdbus-1`, ya presente en Fedora). **Recomendación PM:** (a) `zbus` con `default-features = false, features = ["blocking-api"]`, sujeto al gate `cargo deny`.
- **D-35.2 — Default de comportamiento.** Recomendación: `dim` al 25 % (menos sorpresivo que `hide`).

---

## 7. Plan de tests

**Unitarios**
- `is_effectively_fullscreen`: fullscreen declarado; borderless exacto; borderless con ±2 px; maximizada decorada (false); ventana 1919×1080 en output 1920×1080 sin decoración (true por 98 %); ventana en otro output.
- `FullscreenPolicy`: histéresis de 300 ms con secuencia alternante; `Hidden` → turno activo → `Dimmed(60 %)` → 2 s tras `Idle` → `Hidden`; `own = true` ignorado; `SourceLost` → `Normal`.
- Parseo del diccionario `a{sv}` del script a `WindowSnapshot` (campos faltantes → evento descartado con log único).

**Integración**
- `KwinScriptWatcher` con un bus D-Bus privado (`dbus-daemon --session --print-address`) y un "KWin falso" que llama `Report`: verifica el flujo hasta `WindowEvent`.
- `run_loop` con `HeadlessSurface` + watcher simulado: la opacidad presentada sigue la directiva.

**Manual (equipo de referencia)**
- YouTube a pantalla completa (Firefox/Chromium), juego Steam borderless, `mpv --fs`, presentación de LibreOffice.
- Multimonitor: fullscreen en el monitor sin esfera no hace nada.
- `kwin_wayland --replace` con el daemon vivo: se recupera sin reiniciar Jarvis.
- `qdbus-qt6 org.kde.KWin /Scripting` tras cerrar Jarvis: no quedan scripts `fokkus-jarvis-*`.
