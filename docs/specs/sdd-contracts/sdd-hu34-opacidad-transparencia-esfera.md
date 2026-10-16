# Contrato SDD — HU-34: Opacidad y transparencia variable de la esfera líquida

**Proyecto:** Fokkus-Jarvis
**Épica:** 6 — Experiencia Visual Adaptativa y Personalización de Interfaz
**Redactado por:** Claude (PM/QA)
**Destinatario:** Swarm de desarrollo (Dev Senior)
**Fecha:** 2026-10-16
**Versión base del código:** `v0.2.14`
**Estado:** Borrador para aprobación del PO
**Contratos relacionados:** [HU-33](sdd-hu33-tamano-dinamico-burbuja.md) (canal de vista previa), [HU-35](sdd-hu35-deteccion-pantalla-completa.md) (atenuación automática que se compone con esta opacidad), [HU-36](sdd-hu36-burbujas-chat-efimeras-hud.md)

---

## 1. Contexto

El usuario trabaja sobre código y documentos; la esfera, opaca, tapa información. Necesita un control de opacidad global que haga a Jarvis más o menos translúcido sin perder la lectura de su estado (colores por estado).

### 1.1. Estado actual verificado

- `SphereRenderer::render_into` dibuja sobre un `Pixmap` RGBA **premultiplicado** (halo, blob con shader radial, onda interior). No existe parámetro de opacidad.
- `jarvis_shell::rgba_premul_to_argb8888` ya recorre **cada píxel** de cada frame para pasar de RGBA a BGRA (`wl_shm Argb8888`). Ese recorrido es el punto natural para modular el alfa sin un segundo pase.
- `JarvisConfig` deriva `Eq`: la opacidad se persiste como entero.

### 1.2. Decisión técnica

La opacidad se aplica **en el pase de conversión** (un solo recorrido, coste marginal) y no dentro de cada `fill_path` de tiny-skia. Motivo: con premultiplicado, multiplicar los cuatro canales por `α` equivale a componer la capa con opacidad `α`, sin artefactos en los bordes antialias. Para snapshots, headless y backends futuros se expone además `jarvis_render::apply_opacity` sobre el `Pixmap`.

Optimización opcional (fuera del alcance obligatorio): si el compositor anuncia `wp_alpha_modifier_v1` (KWin 6 lo implementa), el backend Wayland puede delegar el alfa al compositor y saltarse la multiplicación. Debe dar el mismo resultado visual que la ruta CPU.

---

## 2. Criterios de aceptación (Given-When-Then)

**CA-34.1 — Control**
- *Given* Ajustes Unificados → General → Esfera
- *When* el usuario abre la sección
- *Then* hay un slider "Opacidad" de **20 % a 100 %** (pasos de 5 %) con el valor actual de `config.toml`.

**CA-34.2 — Vista previa en vivo**
- *Given* la esfera visible
- *When* el usuario mueve el slider
- *Then* la opacidad cambia en ≤ 100 ms usando el mismo canal JSON por stdout de HU-33 (`{"preview":{"sphere_opacity_pct":60}}`).

**CA-34.3 — Persistencia y cancelación**
- *Given* un valor en vista previa
- *When* guarda → *Then* `sphere_opacity_pct` queda en `config.toml` y se aplica en el arranque.
- *When* cancela → *Then* vuelve al valor persistido.

**CA-34.4 — Legibilidad del estado**
- *Given* opacidad 20 %
- *When* Jarvis pasa a `Listening`, `Thinking` o `Speaking`
- *Then* el color de estado sigue siendo distinguible: el piso de 20 % es inviolable (ni el TOML ni la vista previa pueden bajar de ahí).

**CA-34.5 — Interacción**
- *Given* cualquier opacidad ≥ 20 %
- *When* el usuario hace clic o arrastra la esfera
- *Then* la región de entrada no cambia (la opacidad es solo visual).

**CA-34.6 — Composición con HU-35**
- *Given* opacidad de usuario 70 % y atenuación por pantalla completa al 30 %
- *When* hay una ventana a pantalla completa
- *Then* la opacidad efectiva es `0.70 × 0.30 = 21 %` y al salir vuelve a 70 % con un fundido de 150 ms.

**CA-34.7 — Rendimiento**
- *Given* opacidad < 100 %
- *When* se mide el frame
- *Then* el sobrecoste es ≤ 0.3 ms/frame a 220 px y no hay asignaciones nuevas. Con 100 % se usa la ruta actual sin multiplicar (atajo).

---

## 3. Arquitectura técnica por crate

### 3.1. `jarvis-core`
- `sphere_opacity_pct: u8` (default 100, rango `20..=100`, saturado al cargar).
- `Opacity` (newtype `u8` 0–255) con `from_pct`, `mul(Opacity)` para componer usuario × automático (HU-35) y `is_opaque()`.

### 3.2. `jarvis-render`
- `pub fn apply_opacity(pixmap: &mut Pixmap, opacity: Opacity)`: multiplica los 4 canales premultiplicados por `opacity/255` con aritmética entera (`(c * a + 127) / 255`), SIMD-friendly (`chunks_exact(4)`).
- `OpacityFade`: interpolador temporal lineal (150 ms) entre opacidad actual y objetivo; lo consulta el bucle cada frame.

### 3.3. `jarvis-shell`
- Nueva función `rgba_premul_to_argb8888_with_alpha(src, dst, opacity)`; la actual queda como atajo para `Opacity::OPAQUE`.
- `OverlaySurface::set_opacity(&mut self, opacity: Opacity)` (default: guarda el valor y lo aplica en `present`). `WaylandLayerSurface` la usa en su conversión; si existe `wp_alpha_modifier_v1` y se habilita el feature opcional, la delega.

### 3.4. `jarvis-app`
- El bucle mantiene `user_opacity` (config/preview) y `auto_factor` (HU-35, por defecto `OPAQUE`); en cada frame calcula `target = user × auto`, avanza `OpacityFade` y llama `surface.set_opacity`.
- `unified_settings.py`: slider en la pestaña General, emisión limitada a 30 Hz, mismo protocolo de preview que HU-33.

---

## 4. Interfaces y structs Rust

```rust
// jarvis-core
pub const SPHERE_OPACITY_RANGE: std::ops::RangeInclusive<u8> = 20..=100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Opacity(u8); // 0..=255

impl Opacity {
    pub const OPAQUE: Opacity = Opacity(255);
    /// 20–100 % → 51–255 (satura al rango permitido).
    pub fn from_pct(pct: u8) -> Self;
    pub fn as_u8(self) -> u8;
    /// Composición multiplicativa (usuario × atenuación automática).
    pub fn mul(self, other: Opacity) -> Opacity;
    pub fn is_opaque(self) -> bool;
}

impl JarvisConfig {
    pub fn sphere_opacity(&self) -> Opacity;
}

// jarvis-render
pub fn apply_opacity(pixmap: &mut Pixmap, opacity: Opacity);

pub struct OpacityFade { /* from, to, started_at, duration */ }
impl OpacityFade {
    pub fn new(initial: Opacity, duration: Duration) -> Self;
    pub fn retarget(&mut self, to: Opacity, now: Instant);
    pub fn value_at(&self, now: Instant) -> Opacity;
    pub fn is_settled(&self, now: Instant) -> bool;
}

// jarvis-shell
pub fn rgba_premul_to_argb8888_with_alpha(
    src: &[u8],
    dst: &mut [u8],
    opacity: Opacity,
) -> Result<(), SurfaceError>;

pub trait OverlaySurface {
    // …
    fn set_opacity(&mut self, _opacity: Opacity) {}
}
```

---

## 5. Casos de borde

| Caso | Comportamiento |
|---|---|
| TOML con `sphere_opacity_pct = 0` | Satura a 20 %. La esfera nunca es invisible por opacidad (para ocultarla está HU-35 *auto-hide* o "Silenciar"). |
| Opacidad 100 % | Atajo: se usa la conversión actual (cero coste extra). |
| Fundido en curso y llega otro objetivo | `retarget` parte desde el valor actual interpolado, sin saltos. |
| Menú contextual (`xdg_popup`) | No hereda la opacidad: el menú siempre es 100 % para legibilidad. |
| HUD (HU-36) | Usa `max(opacidad_efectiva, 60 %)` para el texto: los globos no deben volverse ilegibles. |
| Redondeo entero | `(c*a+127)/255` garantiza que `c ≤ a` se mantiene (premultiplicado válido) para todo `a`. |
| Compositor sin `wp_alpha_modifier_v1` | Ruta CPU, sin aviso (es lo normal). |

---

## 6. Plan de tests

**Unitarios**
- `Opacity::from_pct` (0→20 %, 100→255, 50→128), `mul` (255×x = x, conmutativa, monótona).
- `apply_opacity`: pixel opaco blanco a 50 % → `(128,128,128,128)`; invariante `r,g,b ≤ a` en 10⁵ píxeles aleatorios (proptest o generador determinista).
- `rgba_premul_to_argb8888_with_alpha` == `apply_opacity` + conversión actual (equivalencia bit a bit).
- `OpacityFade`: valores en t=0, t=75 ms, t=150 ms; `retarget` a mitad del fundido.

**Integración**
- `run_loop` con `HeadlessSurface` y preview inyectada: el frame presentado tiene el alfa esperado.
- Benchmark (criterion o `--diag`): sobrecoste ≤ 0.3 ms a 220 px y ≤ 1.2 ms a 440 px (HU-33).

**Manual**
- Esfera al 30 % sobre un editor con texto: el texto debajo se lee; los colores de estado se distinguen.
- Composición con HU-35: abrir un video a pantalla completa y salir; verificar fundido sin parpadeo.
