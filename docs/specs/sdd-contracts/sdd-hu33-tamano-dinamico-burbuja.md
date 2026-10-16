# Contrato SDD — HU-33: Tamaño dinámico de la burbuja en tiempo real

**Proyecto:** Fokkus-Jarvis
**Épica:** 6 — Experiencia Visual Adaptativa y Personalización de Interfaz
**Redactado por:** Claude (PM/QA)
**Destinatario:** Swarm de desarrollo (Dev Senior)
**Fecha:** 2026-10-16
**Versión base del código:** `v0.2.14` (workspace)
**Estado:** Borrador para aprobación del PO
**Documento padre:** [`../sdd-fokkus-jarvis.md`](../sdd-fokkus-jarvis.md)
**Contratos relacionados:** [HU-34](sdd-hu34-opacidad-transparencia-esfera.md) (comparte el canal de vista previa), [HU-36](sdd-hu36-burbujas-chat-efimeras-hud.md) (el HUD se posiciona según el tamaño)

---

## 1. Contexto

El usuario quiere ajustar el tamaño de la esfera según su monitor (1080p, 1440p, 4K con escalado) y ver el cambio **mientras arrastra el control**, sin reiniciar el daemon.

### 1.1. Estado actual verificado en el código

| Pieza | Hoy | Consecuencia |
|---|---|---|
| `jarvis_render::SphereRenderer` | `with_size(size)` fija `size`, `center` y `base_radius` al construir; `DEFAULT_SPHERE_SIZE = 220`. | Cambiar de tamaño exige reconstruir el renderer. |
| `jarvis_app::runtime::run_loop` | Recibe `&SphereRenderer` y `&mut Pixmap` inmutables en tamaño. | El bucle no puede redimensionar. |
| `jarvis_shell::OverlayOptions::size` | Fijo en 220×220 al abrir la capa. | No existe `OverlaySurface::set_size`. |
| `WaylandLayerSurface` | `layer.set_size()` solo en `open()` y en `set_visible(true)`; la región de entrada circular (`circle_strips`) se calcula con el tamaño de apertura. | Hay que recalcular región de entrada, márgenes de drag y el buffer `wl_shm`. |
| Ajustes Generales | **No es QML**: es `assets/unified_settings.py` (PyQt6) lanzado como subproceso `python3 -I`; el daemon solo conoce el **código de salida** (0 guardado, 1 cancelado, 3 reset). | No hay canal para eventos "en vivo"; hay que crearlo. |
| `JarvisConfig` | `#[derive(PartialEq, Eq)]`. | Un campo `f32` rompe `Eq`: el valor se guarda como entero en porcentaje. |

> Nota PM: el backlog menciona "QML/Qt"; el contrato se ajusta a la realidad del código (PyQt6 en subproceso). No se introduce QML.

---

## 2. Criterios de aceptación (Given-When-Then)

**CA-33.1 — Control deslizante**
- *Given* Ajustes Unificados abiertos en la pestaña **General**
- *When* el usuario ve la sección "Esfera"
- *Then* hay un `QSlider` "Tamaño de la esfera" de **50 % a 200 %** en pasos de 5 %, con etiqueta del valor en vivo y el valor actual cargado desde `config.toml`.

**CA-33.2 — Vista previa en vivo**
- *Given* el daemon corriendo con la esfera visible
- *When* el usuario arrastra el slider
- *Then* la esfera cambia de tamaño en **≤ 100 ms** desde el último movimiento, sin parpadeo de frame vacío, manteniendo la esquina de anclaje y la posición de drag.

**CA-33.3 — Guardar**
- *Given* un tamaño en vista previa
- *When* el usuario pulsa "Guardar"
- *Then* `sphere_scale_pct` queda escrito en `config.toml` y, al reiniciar el daemon, la esfera abre con ese tamaño.

**CA-33.4 — Cancelar revierte**
- *Given* un tamaño en vista previa distinto al guardado
- *When* el usuario cancela o cierra la ventana (o el proceso de ajustes muere)
- *Then* la esfera vuelve al tamaño persistido en ≤ 100 ms.

**CA-33.5 — Interacción coherente**
- *Given* la esfera redimensionada
- *When* el usuario hace clic, clic derecho o arrastra
- *Then* la región de entrada coincide con el círculo nuevo (las esquinas transparentes siguen siendo *click-through*) y el drag sigue limitado al output.

**CA-33.6 — Rendimiento**
- *Given* tamaño 200 % (440 px lógicos)
- *When* la esfera está en `Speaking`
- *Then* el render se mantiene en ≤ 6 ms/frame en el equipo de referencia y el bucle no asigna memoria por frame fuera del instante del cambio de tamaño.

**CA-33.7 — Escalado HiDPI**
- *Given* un output con escala 1.25 o 2.0
- *When* se aplica cualquier porcentaje
- *Then* el tamaño se interpreta en píxeles **lógicos** y el buffer se crea en píxeles físicos (`buffer_scale`), sin desenfoque.

---

## 3. Arquitectura técnica por crate

### 3.1. `jarvis-core`
- Nuevo campo `sphere_scale_pct: u16` en `JarvisConfig` (default `100`, rango válido `50..=200`).
- `JarvisConfig::sphere_size_px(&self) -> u32` = `DEFAULT_SPHERE_SIZE_PX * clamp(pct) / 100`, con la constante base (220) movida a `jarvis-core` para que `render` y `shell` la compartan sin dependencia circular.
- Un valor fuera de rango en el TOML se **satura** al cargar (no es error de parseo), con aviso en el log.

### 3.2. `jarvis-render`
- `SphereRenderer::resize(&mut self, size: u32)` recalcula `center` y `base_radius` en O(1).
- Sin cambios en la geometría: todo ya es proporcional a `size` (halo, blob, onda). Se verifica con snapshots a 110, 220 y 440 px.

### 3.3. `jarvis-shell`
- `OverlaySurface::set_size(&mut self, size: SurfaceSize) -> Result<(), SurfaceError>` (default: `Err(BackendUnavailable)` para backends que no lo soporten).
- `WaylandLayerSurface::set_size`:
  1. `layer.set_size(w, h)` + `wl_surface.commit()`.
  2. Espera el `configure` siguiente (no presenta frames con el tamaño viejo mientras tanto).
  3. Re-dimensiona el `SlotPool` de `wl_shm` (reutiliza si cabe, crece si no).
  4. Recalcula `apply_sphere_input_region()` y re-satura los márgenes de drag (`drag_bounds`) para que la esfera más grande no quede fuera del output.
- `HeadlessSurface::set_size` actualiza su tamaño (tests).

### 3.4. `jarvis-app`
- **Canal de vista previa** (compartido con HU-34): `unified_settings.py` escribe por **stdout** una línea JSON por cambio:
  `{"preview":{"sphere_scale_pct":135}}`. El daemon lanza el subproceso con `Stdio::piped()` y un hilo lector `jarvis-settings-preview` que parsea líneas y las envía a un `crossbeam_channel::bounded(16)` (si se llena, descarta la más vieja: solo importa el último valor).
- El script **limita** la emisión a una línea cada 33 ms (QTimer) y siempre emite el valor final al soltar el slider.
- `run_loop` pasa a recibir `&mut SphereView` (renderer + pixmap + tamaño). Antes de dibujar, aplica como máximo **un** cambio de tamaño por frame (el último recibido).
- Al cerrar los ajustes: `Saved` → `apply_config` relee el TOML (fuente de verdad); `Cancelled`/proceso muerto → se restaura `config.sphere_scale_pct` persistido.

```
unified_settings.py ──stdout JSON──▶ hilo preview ──chan──▶ run_loop
                                                         ├─ SphereRenderer::resize
                                                         ├─ Pixmap::new (solo en el cambio)
                                                         └─ OverlaySurface::set_size
```

---

## 4. Interfaces y structs Rust

```rust
// jarvis-core/src/config.rs
pub const DEFAULT_SPHERE_SIZE_PX: u32 = 220;
pub const SPHERE_SCALE_RANGE: std::ops::RangeInclusive<u16> = 50..=200;

pub struct JarvisConfig {
    // …campos existentes…
    /// Tamaño de la esfera en % del tamaño base (50–200). Entero para conservar `Eq`.
    pub sphere_scale_pct: u16,
}

impl JarvisConfig {
    /// Porcentaje saturado al rango válido.
    pub fn sphere_scale_pct(&self) -> u16;
    /// Lado de la superficie en píxeles lógicos.
    pub fn sphere_size_px(&self) -> u32;
}

// jarvis-render/src/lib.rs
impl SphereRenderer {
    /// Cambia el tamaño sin reasignar nada (el Pixmap lo gestiona el llamador).
    pub fn resize(&mut self, size: u32);
}

// jarvis-shell/src/lib.rs
pub trait OverlaySurface {
    // …
    fn set_size(&mut self, size: SurfaceSize) -> Result<(), SurfaceError> {
        let _ = size;
        Err(SurfaceError::BackendUnavailable("resize"))
    }
}

// jarvis-app/src/settings.rs (compartido con HU-34)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPreview {
    SphereScale(u16),
    SphereOpacity(u8), // HU-34
}

pub fn parse_preview_line(line: &str) -> Option<SettingsPreview>;

impl UnifiedSettings {
    /// Último valor de vista previa pendiente, si lo hay (no bloquea).
    pub fn poll_preview(&self) -> Option<SettingsPreview>;
}

// jarvis-app/src/runtime.rs
pub struct SphereView {
    pub renderer: SphereRenderer,
    pub pixmap: Pixmap,
}

impl SphereView {
    /// Reasigna el Pixmap solo si el tamaño cambió. Devuelve `true` si cambió.
    pub fn resize(&mut self, size: u32) -> Result<bool, RenderError>;
}
```

---

## 5. Casos de borde

| Caso | Comportamiento esperado |
|---|---|
| Slider movido muy rápido (cientos de eventos) | Coalescencia: el script limita a 30 Hz y el bucle aplica solo el último valor por frame. |
| Valor en TOML `sphere_scale_pct = 900` o `0` | Se satura a 200 / 50 y se registra `[config] sphere_scale_pct fuera de rango`. |
| Esfera arrastrada cerca del borde y se agranda | `drag_bounds` se recalcula y el margen se satura: la esfera nunca queda parcialmente fuera del output. |
| El compositor no responde el `configure` | Timeout de 250 ms: se mantiene el tamaño anterior, se registra el error una sola vez (`ErrorOnce`). |
| El proceso de ajustes muere (SIGKILL) | El hilo lector ve EOF → `Cancelled` → se restaura el tamaño persistido. |
| stdout del script con basura (prints de PyQt) | Las líneas que no son JSON `{"preview":…}` se ignoran sin log ruidoso. |
| Backend `Headless` | `set_size` actualiza el tamaño lógico; no falla. |
| Backend sin soporte (futuro winit) | `set_size` devuelve error: se registra una vez y se aplica en el próximo arranque. |
| Menú contextual abierto durante el resize | El `xdg_popup` se cierra antes del cambio (su posición dependía del tamaño anterior). |
| HUD de HU-36 visible | Se re-posiciona con el nuevo tamaño en el mismo frame. |

---

## 6. Plan de tests

**Unitarios**
- `jarvis-core`: `sphere_scale_pct` saturado (49→50, 201→200), `sphere_size_px` para 50/100/200 %, TOML parcial sin el campo → 100, round-trip.
- `jarvis-render`: `resize` recalcula `center`/`base_radius`; snapshot de píxeles a 110/220/440 px (mismo hash de forma normalizada); `render_into` con Pixmap del tamaño viejo devuelve `InvalidBuffer`.
- `jarvis-shell`: `circle_strips` para 110 y 440 px (esquinas fuera, centro dentro); `drag_margins` re-saturados tras crecer.
- `jarvis-app`: `parse_preview_line` (válido, fuera de rango, basura, JSON sin `preview`); `SphereView::resize` no reasigna si el tamaño no cambia.

**Integración**
- `run_loop` con `HeadlessSurface` + canal de preview inyectado: enviar 50 cambios en un frame aplica solo el último; tras `Cancelled` vuelve al persistido.
- Test de no-asignación (patrón de `tests/no_alloc.rs`): 1 000 frames sin cambio de tamaño → 0 asignaciones en el bucle.

**Manual (equipo de referencia: Fedora 44, KWin Wayland, Caelestia)**
- Arrastrar el slider 50→200→50: sin parpadeo, sin saltos de posición, clic en esquinas atraviesa a la ventana de abajo.
- Escala de output 1.25: la esfera no se ve borrosa.
- Medición de frame time a 200 % (`--diag`).
