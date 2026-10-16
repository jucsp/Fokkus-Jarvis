# Contrato SDD — HU-38: Ojos y manos multiplataforma (Screen Inspector & Input Synthesizer)

**Proyecto:** Fokkus-Jarvis
**Épica:** 7 — Agente Visual Multiplataforma y Automatización Generalista
**Redactado por:** Claude (PM/QA)
**Destinatario:** Swarm de desarrollo (Dev Senior)
**Fecha:** 2026-10-16
**Versión base del código:** `v0.2.14`
**Estado:** Borrador para aprobación del PO — **requiere decisiones D-38.1 a D-38.3**
**Contratos relacionados:** [HU-35](sdd-hu35-deteccion-pantalla-completa.md) (monitor de ventana activa reutilizado), [HU-39](sdd-hu39-agente-visual-autoverificacion.md) (único consumidor de esta capa)

---

## 1. Contexto

HU-39 necesita **ver** la pantalla y **actuar** (clic, escribir, teclas) de forma consistente en Linux (Wayland y X11) y Windows. Esta HU entrega solo la capa de "ojos y manos" con salvaguardas; la inteligencia (qué mirar, dónde hacer clic) es HU-39.

### 1.1. Realidad del equipo de referencia (verificado 2026-10-16)

Fedora 44 · KDE Plasma (KWin **Wayland**) · binarios presentes: `grim`, `spectacle`, `qdbus-qt6`, `busctl`. Ausentes: `ydotool`, `wtype`, `xdotool`, `kdotool`.

| Herramienta | ¿Sirve en KWin Wayland? | Motivo |
|---|---|---|
| `grim` | **No** | Usa `wlr-screencopy`, que KWin no implementa. Solo para Hyprland/Sway. |
| `spectacle -b -n` | Sí | Spectacle es cliente privilegiado de KWin (`ScreenShot2`). |
| XDG Desktop Portal `Screenshot` | Sí (genérico) | Puede pedir permiso la primera vez; funciona en KDE y GNOME. |
| `wtype` | **No** | Requiere `virtual-keyboard-unstable-v1` (wlroots). |
| `ydotool` | Sí, con `ydotoold` + acceso a `/dev/uinput` | Requiere servicio con privilegios: superficie de ataque alta. |
| XDG Portal `RemoteDesktop` + **libei** | **Sí (recomendado)** | KDE 6.1+ lo soporta; consentimiento explícito del usuario y revocable. |

### 1.2. Principio de diseño

Ninguna acción de entrada se ejecuta sin pasar por `AutomationGuard` (kill switch, *lease* temporal, límites de tasa, lista de exclusión, auditoría). La automatización está **apagada por defecto**.

---

## 2. Criterios de aceptación (Given-When-Then)

**CA-38.1 — Sondeo de capacidades**
- *Given* cualquier plataforma
- *When* se ejecuta `fokkus-jarvis --check`
- *Then* se listan los backends de captura y de entrada disponibles, el elegido, y el motivo de los descartados (p. ej. `grim: descartado (KWin no expone wlr-screencopy)`).

**CA-38.2 — Captura**
- *Given* `automation_enabled = true` en KWin Wayland
- *When* HU-39 pide `capture(CaptureTarget::ActiveOutput)`
- *Then* se obtiene un `CapturedFrame` RGBA en ≤ 400 ms con geometría lógica, escala del output y marca de tiempo; la imagen **no se escribe a disco** salvo archivo temporal `0600` del backend, borrado en ≤ 1 s.

**CA-38.3 — Entrada**
- *Given* un *lease* de automatización activo
- *When* se pide `click(Point)`, `type_text(str)` o `key_combo([Ctrl, L])`
- *Then* la acción ocurre en la posición lógica correcta (multimonitor y escala 1.25 incluidas) con un error ≤ 2 px.

**CA-38.4 — Consentimiento**
- *Given* el primer uso de entrada en Wayland
- *When* se abre la sesión `RemoteDesktop`
- *Then* el diálogo del portal lo ve el usuario; Jarvis lo anuncia por voz ("Necesito permiso para controlar el teclado y el mouse"). Rechazo → `InputError::PermissionDenied` y explicación hablada. El *restore token* se guarda en el llavero para no volver a pedirlo.

**CA-38.5 — Kill switch**
- *Given* una secuencia de acciones en curso
- *When* el usuario dice "para", "detente", "cancela" o mueve el mouse físico > 50 px, o pulsa `Esc` dos veces
- *Then* toda acción pendiente se aborta en ≤ 100 ms, el *lease* se revoca y la esfera sale del estado visual "actuando".

**CA-38.6 — Indicador visible**
- *Given* un *lease* activo
- *Then* la esfera muestra un anillo ámbar pulsante (estado visual `Acting`, sin nuevo estado en la máquina de `JarvisState`) y el HUD (HU-36) muestra "Controlando: <app>".

**CA-38.7 — Exclusiones**
- *Given* la ventana activa pertenece a la lista de exclusión (gestores de contraseñas, `polkit`, diálogos de `sudo`/`pkexec`, banca configurada por el usuario)
- *When* se pide cualquier acción de entrada **o** captura
- *Then* se rechaza con `GuardError::ExcludedWindow`.

**CA-38.8 — Auditoría**
- *Then* cada acción queda en `~/.local/state/fokkus-jarvis/automation.jsonl` (tipo, coordenadas, app destino, resultado, id de *lease*); el texto tecleado se registra solo como longitud + hash salvo `audit_log_text = true`.

**CA-38.9 — Windows**
- *Given* Windows 11
- *Then* captura por `Windows.Graphics.Capture` (o GDI `BitBlt` de respaldo) y entrada por `SendInput`, con las mismas garantías; UAC y escritorios seguros quedan excluidos por diseño del SO.

---

## 3. Arquitectura técnica por crate

### 3.1. Crate nuevo `jarvis-automation`
Se crea un crate propio (no dentro de `jarvis-core`) para que el núcleo siga libre de dependencias de plataforma; `jarvis-core` solo recibe los campos de configuración.

```
jarvis-automation/
├── capture/   mod.rs (trait ScreenCapture) · portal.rs · spectacle.rs · grim.rs · x11.rs · windows.rs
├── input/     mod.rs (trait InputSynth)    · libei.rs · ydotool.rs · x11.rs · windows.rs
├── geometry.rs   (Point/Rect lógicos ↔ físicos, multimonitor, escalas)
├── guard.rs      (AutomationGuard: lease, rate limit, exclusiones, kill switch)
├── audit.rs      (JSONL)
└── probe.rs      (Capabilities)
```

**Orden de selección de captura (Linux):** Portal `Screenshot` (no interactivo si hay permiso) → `spectacle` (si `XDG_CURRENT_DESKTOP=KDE`) → `grim` (si el global `zwlr_screencopy_manager_v1` existe) → X11 `GetImage` (`x11rb`).
**Orden de selección de entrada (Linux):** Portal `RemoteDesktop` + libei → X11 XTest (solo sesión X11) → `ydotool` (solo si el usuario lo habilita explícitamente con `allow_uinput = true`).

### 3.2. `jarvis-core`
- `AutomationConfig` anidado (`[automation]` en TOML, `#[serde(default)]`): `enabled` (false), `allow_uinput` (false), `max_actions_per_sec` (10), `lease_secs` (120), `excluded_apps: Vec<String>`, `audit_log_text` (false).

### 3.3. `jarvis-shell`
- Exporta la ventana activa (de HU-35 `WindowWatcher`) para `AutomationGuard` y la geometría de outputs.
- Estado visual `Acting` en el renderer (anillo), controlado por un `AtomicBool` del guard.

### 3.4. `jarvis-audio`
- `LocalCommand::StopAutomation` ("para", "detente", "cancela") con **prioridad absoluta**: se evalúa en el hilo `jarvis-voice` antes de llegar al cerebro, para cumplir ≤ 100 ms.

---

## 4. Interfaces y structs Rust

```rust
// geometry.rs — coordenadas LÓGICAS del compositor salvo que se indique
#[derive(Debug, Clone, Copy, PartialEq)] pub struct Point { pub x: f64, pub y: f64 }
#[derive(Debug, Clone, Copy, PartialEq)] pub struct LRect { pub x: f64, pub y: f64, pub w: f64, pub h: f64 }

#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo { pub name: String, pub logical: LRect, pub scale: f64 }

pub struct DesktopLayout { pub outputs: Vec<OutputInfo> }
impl DesktopLayout {
    pub fn output_at(&self, p: Point) -> Option<&OutputInfo>;
    /// Píxel de una captura de `output` → punto lógico global.
    pub fn frame_to_logical(&self, frame: &CapturedFrame, px: (u32, u32)) -> Point;
}

// capture
#[derive(Debug, Clone)]
pub enum CaptureTarget { ActiveOutput, Output(String), ActiveWindow, Region(LRect) }

pub struct CapturedFrame {
    pub rgba: Vec<u8>,
    pub width: u32,            // píxeles físicos
    pub height: u32,
    pub logical: LRect,        // zona capturada en coordenadas lógicas
    pub scale: f64,
    pub taken_at: Instant,
    pub backend: CaptureBackendKind,
}

impl CapturedFrame {
    /// Reescala para modelos de visión (lado mayor ≤ `max_edge`, p. ej. 1568).
    pub fn downscaled(&self, max_edge: u32) -> CapturedFrame;
    pub fn encode_png(&self) -> Vec<u8>;
    /// Difuminado de regiones (campos de contraseña) antes de enviar fuera.
    pub fn redact(&mut self, regions: &[LRect]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBackendKind { Portal, Spectacle, Grim, X11, WindowsGraphicsCapture, Gdi }

pub trait ScreenCapture: Send {
    fn kind(&self) -> CaptureBackendKind;
    fn capture(&mut self, target: &CaptureTarget) -> Result<CapturedFrame, CaptureError>;
}

// input
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton { Left, Right, Middle }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key { Enter, Escape, Tab, Backspace, Ctrl, Shift, Alt, Super, Char(char), F(u8) /* … */ }

#[derive(Debug, Clone, PartialEq)]
pub enum InputAction {
    MoveTo(Point),
    Click { at: Point, button: MouseButton, count: u8 },
    Scroll { at: Point, dx: f64, dy: f64 },
    TypeText(String),
    KeyCombo(Vec<Key>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputBackendKind { PortalLibei, X11Xtest, Ydotool, WindowsSendInput }

pub trait InputSynth: Send {
    fn kind(&self) -> InputBackendKind;
    fn perform(&mut self, action: &InputAction) -> Result<(), InputError>;
}

// guard
pub struct LeaseId(pub u64);

pub struct AutomationGuard { /* cfg, active lease, token bucket, kill flag, audit */ }
impl AutomationGuard {
    pub fn acquire(&self, purpose: &str, target_app: &str) -> Result<Lease, GuardError>;
    pub fn kill(&self, reason: KillReason);            // idempotente, desde cualquier hilo
    pub fn is_killed(&self) -> bool;
}

pub struct Lease<'g> { /* guard, id, expires_at */ }
impl Lease<'_> {
    /// Única puerta de entrada a InputSynth: valida kill, expiración, tasa y exclusiones.
    pub fn perform(&mut self, synth: &mut dyn InputSynth, action: InputAction) -> Result<(), GuardError>;
    pub fn capture(&mut self, cap: &mut dyn ScreenCapture, target: &CaptureTarget) -> Result<CapturedFrame, GuardError>;
}
impl Drop for Lease<'_> { fn drop(&mut self) { /* apaga indicador, audita fin */ } }

pub enum KillReason { Voice, PhysicalMouse, DoubleEscape, Timeout, Shutdown }

pub enum GuardError {
    Disabled, Killed(KillReason), LeaseExpired, RateLimited,
    ExcludedWindow(String), Capture(CaptureError), Input(InputError),
}

// probe
pub struct Capabilities {
    pub capture: Vec<(CaptureBackendKind, Result<(), String>)>,
    pub input: Vec<(InputBackendKind, Result<(), String>)>,
}
pub fn probe() -> Capabilities;
```

---

## 5. Casos de borde

| Caso | Comportamiento |
|---|---|
| Portal `RemoteDesktop` no disponible (Plasma < 6.1) | Se informa por voz y `--check`; no se cae a `ydotool` salvo `allow_uinput = true`. |
| El usuario revoca el permiso en Ajustes del sistema | `perform` falla con `PermissionDenied` → `kill(Timeout)` + aviso; el *restore token* se borra del llavero. |
| Distribución de teclado (latam, es, us) | `TypeText` usa *keysyms* Unicode del backend (libei/XTest con mapeo), no códigos de tecla fijos; test con `ñ`, `¿`, `@`, tildes. |
| Escalas fraccionarias (1.25) y multimonitor con offsets negativos | Toda la API es lógica; la conversión vive solo en `geometry.rs` y está cubierta por tests. |
| Captura de la propia esfera/HUD | Antes de capturar, la esfera y el HUD se ocultan 1 frame (o se enmascaran) para no confundir a la visión. |
| Pantalla bloqueada / salvapantallas | Captura y entrada se rechazan (`ExcludedWindow("lockscreen")`). |
| Ventana activa cambia entre captura y acción | No se resuelve aquí: HU-39 re-verifica antes de actuar (TOCTOU). El guard sí rechaza si la app destino del *lease* ya no está activa. |
| Ráfaga de acciones (bug del planificador) | *Token bucket* de 10 acciones/s → `RateLimited`. |
| Captura en HDR / 10 bits | Se convierte a RGBA8 sRGB. |
| Windows con UAC elevado | `SendInput` no llega a procesos elevados (UIPI): se detecta y se informa. |

---

## 6. Decisiones que debe tomar el PO

- **D-38.1 — Entrada en Wayland.** Recomendación: crate `ashpd` (portal, D-Bus) + `reis` (libei) — ambos Rust; requieren análisis de seguridad. Alternativa: `enigo` con feature `libei` (aún experimental).
- **D-38.2 — D-Bus.** Reutilizar la misma decisión que D-35.1 (`zbus` blocking) para el portal: un solo stack D-Bus en el árbol.
- **D-38.3 — Windows.** Crate `windows` (oficial de Microsoft) con features mínimas (`Win32_UI_Input_KeyboardAndMouse`, `Graphics_Capture`).

---

## 7. Plan de tests

**Unitarios**
- `geometry`: dos outputs (1920×1080@1.0 en 0,0 y 2560×1440@1.25 en 1920,−200); `frame_to_logical` en esquinas y centro; `output_at` en bordes.
- `AutomationGuard`: deshabilitado → `Disabled`; *lease* expira; `kill` desde otro hilo aborta `perform`; *token bucket*; exclusión por `app_id`.
- `audit`: el texto no aparece en el JSONL con `audit_log_text = false`.
- `CapturedFrame::downscaled` mantiene proporción; `redact` deja la región uniforme.

**Integración (Linux, CI con sesión anidada)**
- `kwin_wayland --virtual` + portal: captura de un output virtual con un patrón conocido; comparación de píxeles.
- `InputSynth` contra una app de prueba (ventana GTK/Qt mínima que registra eventos): clic en coordenada lógica con escala 1.25, `TypeText("¿Añadir café? ñ@")`.
- X11 con `Xvfb` + XTest.

**Manual (equipo de referencia)**
- `--check` muestra `spectacle`/portal elegidos y `grim` descartado con motivo.
- Primer uso: aparece el diálogo del portal; segundo uso: no reaparece.
- Kill switch: decir "para" en medio de una secuencia de 20 teclas; mover el mouse físico.
