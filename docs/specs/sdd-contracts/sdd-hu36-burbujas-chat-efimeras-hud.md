# Contrato SDD — HU-36: Mensajes efímeros flotantes en burbujas transparentes (Speech Popups HUD)

**Proyecto:** Fokkus-Jarvis
**Épica:** 6 — Experiencia Visual Adaptativa y Personalización de Interfaz
**Redactado por:** Claude (PM/QA)
**Destinatario:** Swarm de desarrollo (Dev Senior)
**Fecha:** 2026-10-16
**Versión base del código:** `v0.2.14`
**Estado:** Borrador para aprobación del PO — **requiere decisión D-36.1 (rasterizador de fuentes)**
**Contratos relacionados:** [HU-33](sdd-hu33-tamano-dinamico-burbuja.md), [HU-34](sdd-hu34-opacidad-transparencia-esfera.md), [HU-35](sdd-hu35-deteccion-pantalla-completa.md), [HU-37](sdd-hu37-tareas-pesadas-asincronas-ipc.md) (avisos de tareas), [HU-39](sdd-hu39-agente-visual-autoverificacion.md) (borrador a confirmar)

---

## 1. Contexto

El usuario quiere confirmar de un vistazo **qué entendió Jarvis** y **qué respondió**, sin una ventana de chat permanente. La solución es un HUD de 1 a 3 globos translúcidos sobre la esfera que se desvanecen solos.

### 1.1. Estado actual verificado

- La superficie de la esfera es un cuadrado de `size×size` con **región de entrada circular** (`circle_strips`). No hay espacio para texto encima.
- No existe rasterización de texto real: `jarvis-shell::menu` usa una fuente bitmap 5×7 **solo con mayúsculas parciales** (sin minúsculas, tildes ni `ñ`). No sirve para frases en español.
- El orquestador (`BrainLoop`) conoce la consulta aceptada (`on_turn(query)`), los tokens de la respuesta (`converse` → `stream_tokens(on_token)`) y las locuciones de `BrainJob::Say`, pero **no las publica** hacia el bucle de render.

### 1.2. Decisiones técnicas

1. **Superficie separada.** El HUD es una **segunda `zwlr_layer_surface_v1`** (capa `overlay`, mismo anclaje que la esfera) con **región de entrada vacía**: totalmente *click-through*. No se agranda la superficie de la esfera (rompería HU-33 y la región de entrada).
2. **Eventos, no polling de estado.** El orquestador publica `HudEvent` por un canal acotado; el bucle de render los consume sin bloquear.
3. **Rasterizado de texto en CPU** con una dependencia ligera y una fuente embebida (D-36.1). El glifo se cachea por (carácter, tamaño): el render por frame no rasteriza.

---

## 2. Criterios de aceptación (Given-When-Then)

**CA-36.1 — Lo que dijo el usuario**
- *Given* `hud_enabled = true`
- *When* un turno es aceptado por la activación
- *Then* en ≤ 150 ms aparece un globo "usuario" (alineado a la derecha, tono neutro) con la consulta **ya limpia de vocativo** (`"qué hora es"`, no `"Jarvis, qué hora es"`).

**CA-36.2 — Lo que responde Jarvis**
- *Given* un turno en curso
- *When* llegan tokens del cerebro
- *Then* aparece un globo "Jarvis" (alineado a la izquierda, color del estado `Speaking`) que se va completando por frase, sincronizado con el TTS (se muestra la frase cuando empieza a sonar, no antes).

**CA-36.3 — Máximo y desvanecimiento**
- *Given* globos visibles
- *When* llega un cuarto globo → *Then* el más antiguo sale con fundido de 250 ms (máximo 3).
- *When* pasa el TTL → *Then* el globo se desvanece en 400 ms. TTL = `max(4 s, 60 ms × caracteres)`, contado desde que **terminó de sonar** su locución (globos de Jarvis) o desde que apareció (globos del usuario), con tope de 12 s.

**CA-36.4 — Toggle**
- *Given* Ajustes → General → "Mostrar burbujas de conversación"
- *When* se desactiva y se guarda
- *Then* no se crea la superficie del HUD (0 bytes de buffer) y los globos visibles desaparecen en el siguiente frame.

**CA-36.5 — Click-through**
- *Given* globos visibles sobre una ventana
- *When* el usuario hace clic donde hay un globo
- *Then* el clic llega a la ventana de abajo.

**CA-36.6 — Legibilidad**
- *Given* cualquier opacidad de esfera (HU-34)
- *Then* el fondo del globo es negro al 55 % con texto blanco al `max(opacidad_efectiva, 60 %)`; contraste ≥ 4.5:1 sobre fondo blanco y negro.

**CA-36.7 — Pantalla completa y silencio**
- *Given* HU-35 en `Dimmed`/`Hidden` o Jarvis en `Muted`
- *Then* el HUD no muestra globos nuevos (salvo durante un turno activo según CA-35.4).

**CA-36.8 — Barge-in**
- *Given* Jarvis hablando con un globo a medio llenar
- *When* el usuario interrumpe
- *Then* el globo de Jarvis se cierra con "…" y empieza su TTL; el nuevo globo del usuario aparece normalmente.

**CA-36.9 — Rendimiento y memoria**
- Sin globos: el HUD no presenta frames (0 % CPU). Con 3 globos animando: ≤ 1.5 ms/frame y ≤ 3 MB de RSS adicional (atlas de glifos + buffers).

---

## 3. Arquitectura técnica por crate

### 3.1. `jarvis-core`
- Campos: `hud_enabled: bool` (default `true`), `hud_max_bubbles: u8` (1–3, default 3).

### 3.2. `jarvis-audio` (orquestador)
- `VoiceDeps` gana `hud: Option<Sender<HudEvent>>` (canal `bounded(64)`, `try_send`: si se llena se descarta; el HUD jamás frena la voz).
- Puntos de emisión en `BrainLoop`:
  - `on_turn(query)` → `HudEvent::UserSaid { turn, text }`.
  - Cuando el hilo TTS **empieza a reproducir** una frase de la generación `g` → `HudEvent::JarvisPhrase { turn, text }` (no en el token: el texto se muestra cuando suena).
  - Fin natural → `JarvisDone { turn }`; barge-in → `JarvisInterrupted { turn }`.
  - `BrainJob::Say` (IPC, avisos de HU-37) → `Notice { text }`.

### 3.3. `jarvis-render` — módulo nuevo `hud`
- `BubbleStack`: lógica **pura** (sin dibujo) de alta, fusión de frases, TTL, fundidos y descarte. Testeable con `Instant` inyectado.
- `TextRasterizer`: envuelve el rasterizador (D-36.1), caché de glifos `HashMap<(char, px), GlyphBitmap>` precalentada con ASCII + `áéíóúüñ¿¡ÁÉÍÓÚÑ`.
- `HudRenderer::render_into(&BubbleStack, &mut Pixmap, now)`: rectángulos redondeados (`tiny-skia`), *word-wrap* a 280 px lógicos, máximo 3 líneas por globo con elipsis "…".

### 3.4. `jarvis-shell`
- `WaylandLayerSurface` crea una segunda capa `HudLayer` en la misma conexión/cola de eventos: tamaño 320×(alto dinámico ≤ 260) px, anclada al mismo borde que la esfera, desplazada por encima (o debajo si la esfera está anclada arriba) según `size` de HU-33 y la posición de drag.
- `set_input_region(Some(empty_region))` en el HUD.
- API: `OverlaySurface::hud(&mut self) -> Option<&mut dyn HudSurface>`.

### 3.5. `jarvis-app`
- Hilo de render: drena `HudEvent` (`try_recv` en bucle), actualiza `BubbleStack`, y solo presenta el HUD si `stack.is_animating(now)` o cambió.

---

## 4. Interfaces y structs Rust

```rust
// jarvis-audio/src/events.rs
pub type TurnId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HudEvent {
    UserSaid { turn: TurnId, text: String },
    JarvisPhrase { turn: TurnId, text: String },
    JarvisDone { turn: TurnId },
    JarvisInterrupted { turn: TurnId },
    Notice { text: String },
    Clear,
}

// jarvis-render/src/hud.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker { User, Jarvis, System }

#[derive(Debug, Clone)]
pub struct Bubble {
    pub speaker: Speaker,
    pub turn: Option<TurnId>,
    pub text: String,
    pub born: Instant,
    pub sealed_at: Option<Instant>, // fin de locución: empieza el TTL
    pub interrupted: bool,
}

pub struct BubbleStack { /* VecDeque<Bubble>, max, ttl policy */ }

impl BubbleStack {
    pub fn new(max: usize) -> Self;
    pub fn apply(&mut self, ev: &HudEvent, now: Instant);
    /// Opacidad 0.0–1.0 de cada globo en `now` (fade in/out).
    pub fn visible(&self, now: Instant) -> impl Iterator<Item = (&Bubble, f32)>;
    pub fn gc(&mut self, now: Instant);
    pub fn is_animating(&self, now: Instant) -> bool;
    pub fn is_empty(&self) -> bool;
}

pub fn bubble_ttl(chars: usize) -> Duration; // max(4 s, 60 ms*chars).min(12 s)

pub struct HudRenderer { /* TextRasterizer, layout cfg */ }
impl HudRenderer {
    pub fn new(font_px: f32) -> Result<Self, RenderError>;
    /// Alto necesario para el stack actual (la capa se ajusta a esto).
    pub fn measure(&self, stack: &BubbleStack, now: Instant) -> u32;
    pub fn render_into(&mut self, stack: &BubbleStack, pixmap: &mut Pixmap, min_alpha: Opacity, now: Instant);
}

// jarvis-shell
pub trait HudSurface {
    fn set_size(&mut self, size: SurfaceSize) -> Result<(), SurfaceError>;
    fn present(&mut self, frame: &FrameBuffer<'_>) -> Result<(), SurfaceError>;
    fn set_visible(&mut self, visible: bool);
}
```

---

## 5. Casos de borde

| Caso | Comportamiento |
|---|---|
| Respuesta muy larga (> 3 líneas) | Se muestran las **últimas** 3 líneas de la frase en curso (efecto teleprompter); el globo nunca crece más de 3 líneas. |
| Emojis / caracteres fuera de la fuente | Se sustituyen por `□` sin pánico; la caché no crece sin límite (tope 512 glifos, LRU). |
| Markdown en la respuesta (`**`, `` ` ``, `#`) | Se limpia con el mismo saneado que usa el TTS antes de mostrar. |
| Texto sensible (el usuario dicta una clave) | El HUD es efímero y no persiste nada; no se agregan logs nuevos con el texto. |
| Esfera anclada arriba (TopLeft/TopRight) | El HUD se coloca **debajo** de la esfera. |
| Esfera arrastrada al borde | El HUD se desplaza horizontalmente para no salir del output. |
| HUD apagado mientras hay globos | `Clear` + destrucción de la capa en el siguiente frame. |
| Varios avisos IPC seguidos (HU-37) | Cada uno es `Notice`; se respeta el máximo de 3 con FIFO. |
| Locución sin texto (chime HU-18) | No genera globo. |
| Compositor sin layer-shell (headless) | `hud()` devuelve `None`; los eventos se drenan y descartan. |

---

## 6. Decisiones que debe tomar el PO

- **D-36.1 — Rasterizador y fuente.** Opciones: (a) `fontdue` (Rust puro, `no_std`-friendly, sin dependencias transitivas relevantes); (b) `ab_glyph`; (c) ampliar la fuente bitmap (descartado: calidad insuficiente para frases). Fuente embebida: **Inter** o **Noto Sans** subset Latin-1 (licencia OFL, ~80–120 KB). **Recomendación PM:** `fontdue` + Noto Sans subset, con análisis de seguridad en el formato de la §3 de `sdd-conversacion-nativa-v0.2.0.md` y registro en `deny.toml`.
- **D-36.2 — Default.** Recomendación: `hud_enabled = true`.

---

## 7. Plan de tests

**Unitarios (`jarvis-render::hud`)**
- `BubbleStack`: alta de usuario; fusión de `JarvisPhrase` del mismo `turn` en un solo globo; cuarto globo expulsa al más viejo; TTL desde `sealed_at`; `JarvisInterrupted` añade "…" y sella; `gc` elimina caducados; `is_animating` falso en reposo.
- `bubble_ttl`: 10 chars → 4 s, 100 → 6 s, 1 000 → 12 s.
- Word-wrap: palabras más largas que el ancho se cortan con guion; tildes y `ñ` miden lo correcto.
- Snapshot de píxeles de un globo de 2 líneas (hash estable).

**Unitarios (`jarvis-audio`)**
- Con el doble de `TokenStream` y sink de `orchestrator/tests.rs`: un turno emite `UserSaid` → `JarvisPhrase`×N → `JarvisDone` en ese orden; barge-in emite `JarvisInterrupted`; canal lleno no bloquea el turno.

**Integración**
- `run_loop` + `HeadlessSurface` con HUD simulado: sin eventos no se presentan frames de HUD.
- Medición de RSS antes/después de activar el HUD (≤ 3 MB).

**Manual**
- Conversación de 5 turnos: lo dicho y lo respondido se leen; los globos se van solos.
- Clic sobre un globo atraviesa a la ventana inferior.
- Fondo blanco (documento) y fondo negro (terminal): legible.
