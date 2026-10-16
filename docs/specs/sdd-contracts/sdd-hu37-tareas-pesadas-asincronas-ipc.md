# Contrato SDD — HU-37: Tareas pesadas en segundo plano con notificación vocal secuencial vía IPC

**Proyecto:** Fokkus-Jarvis
**Épica:** 7 — Agente Visual Multiplataforma y Automatización Generalista
**Redactado por:** Claude (PM/QA)
**Destinatario:** Swarm de desarrollo (Dev Senior)
**Fecha:** 2026-10-16
**Versión base del código:** `v0.2.14`
**Estado:** Borrador para aprobación del PO
**Contratos relacionados:** [HU-36](sdd-hu36-burbujas-chat-efimeras-hud.md) (avisos visibles), [HU-39](sdd-hu39-agente-visual-autoverificacion.md) (el agente visual corre como tarea de este gestor)

---

## 1. Contexto

"Jarvis, revisa mis correos y dime si hay algo urgente" puede tardar minutos (cerebro CLI con herramientas MCP, Slack, análisis de datos). Hoy eso **congela la conversación** y un barge-in o un turno nuevo **lo mata**.

### 1.1. Estado actual verificado

| Pieza | Comportamiento hoy | Problema |
|---|---|---|
| `BrainLoop::run` (hilo `jarvis-brain`) | Procesa `BrainJob` **secuencialmente**; `on_turn` → `converse` llama `stream_tokens` de forma **bloqueante**. | Mientras una consulta larga corre, ningún otro turno se atiende. |
| `CliBrain::stream` | `DEFAULT_CLI_TIMEOUT = 120 s`; `cancel()` mata el árbol de procesos (`kill_tree`). | Una tarea de 5 min se corta a los 120 s; el barge-in la cancela. |
| `barge_in(core, playback, client)` | Llama `client.cancel()` sobre el **cerebro compartido**. | Cancela cualquier stream activo de ese cerebro, aunque sea una tarea de fondo. |
| `BrainLoop.deferred: VecDeque<String>` | Cola de `BrainJob::Say` mientras otra locución suena; se drena en `on_speech_done`. | Base correcta, pero sin prioridad, sin tope, sin política de silencio/gaming. |
| `jarvis.speak` (IPC) | Rechaza si `muted`; si no, `SpeechSink::speak` → `VoiceSpeaker::say`. | No informa si quedó en cola ni en qué posición. |
| `CliBrain` | Mantiene **historial** propio (`history()`). | Una tarea de fondo que usa el mismo `CliBrain` contamina el historial conversacional. |

---

## 2. Criterios de aceptación (Given-When-Then)

**CA-37.1 — Acuse inmediato**
- *Given* una consulta clasificada como pesada
- *When* el turno es aceptado
- *Then* Jarvis dice un acuse en **< 1 s** (frase pre-sintetizada de la caché de fillers, p. ej. "Voy con eso, te aviso cuando termine") y vuelve a `Listening` con la ventana de seguimiento abierta.

**CA-37.2 — Desacople**
- *Given* una tarea pesada en curso
- *When* el usuario hace otra pregunta ("¿qué hora es?", "¿quién ganó el partido?")
- *Then* se responde con normalidad y la tarea **sigue corriendo** (verificable en `jarvis.getTasks`).

**CA-37.3 — Inmunidad al barge-in**
- *Given* una tarea pesada en curso y Jarvis hablando de otro tema
- *When* el usuario interrumpe (barge-in)
- *Then* se corta la locución y el stream conversacional, pero **la tarea no se cancela** (CA-9.4 del SDD v0.2.0 extendido a tareas).

**CA-37.4 — Entrega secuencial sin colisiones**
- *Given* dos tareas que terminan casi a la vez mientras Jarvis está hablando
- *When* termina la locución actual
- *Then* los resultados se dicen **uno tras otro en orden FIFO de finalización**, cada uno precedido por su encabezado ("Sobre tus correos: …"), nunca superpuestos, y nunca interrumpiendo una locución en curso.

**CA-37.5 — Momento oportuno**
- *Given* un resultado listo
- *When* el usuario está hablando o el VAD detectó voz en los últimos 1.5 s
- *Then* la entrega espera a un hueco de silencio de 1.5 s (máx. 60 s; pasado eso, se entrega al siguiente `Idle`).

**CA-37.6 — Resultado largo**
- *Given* un resultado de más de 60 palabras
- *Then* se locuta un resumen hablado ≤ 60 palabras y el resultado completo queda en `~/.local/state/fokkus-jarvis/tasks/<id>.md` y en `jarvis.getTask`. Jarvis ofrece: "¿Quieres el detalle?" (ventana de seguimiento).

**CA-37.7 — Cancelación explícita**
- *Given* tareas en curso
- *When* el usuario dice "cancela la tarea de los correos" o "cancela todas las tareas"
- *Then* se cancela la indicada (o todas) y Jarvis lo confirma. Ambigüedad → pregunta cuál.

**CA-37.8 — Silencio y modo juego**
- *Given* `Muted` o `gaming_mode = true`
- *When* termina una tarea
- *Then* el resultado se retiene; al salir del silencio/modo juego Jarvis dice "Tienes N resultados pendientes" y los entrega a petición o en orden.

**CA-37.9 — IPC**
- *Given* un cliente (Fokkus IDE)
- *When* llama `jarvis.dispatchTask`, `jarvis.getTasks`, `jarvis.cancelTask`
- *Then* obtiene ids estables, estado y resultados; además, `jarvis.speak` con `"queue": true` devuelve `queued: true` y `position` en vez de rechazar por estar hablando.

**CA-37.10 — Límites**
- Máximo **2** tareas en paralelo (configurable 1–4); la 3.ª queda `Queued`. Timeout por tarea: 15 min (configurable). Al expirar: "La tarea X tardó demasiado y la detuve."

---

## 3. Arquitectura técnica por crate

### 3.1. `jarvis-audio` — módulo nuevo `tasks`

```
                  ┌───────────── jarvis-brain (BrainLoop) ─────────────┐
on_turn(query) ──▶│ classify_workload(query) ─┬─ Light ─▶ converse()  │
                  │                           └─ Heavy ─▶ ack + submit │
                  └───────────────────────────────────────┬────────────┘
                                                          ▼
                         TaskManager (hilo jarvis-tasks, pool de N workers)
                         ├─ worker: BrainFactory::spawn() → cerebro AISLADO
                         ├─ CancellationToken propio (no el del barge-in)
                         └─ al terminar ─▶ SpeechQueue.push(Delivery)
                                                          │
             BrainLoop.on_speech_done / tick 250 ms ◀─────┘ (drena si hay hueco)
```

- **Clasificación** `classify_workload(query) -> Workload` (función pura): heurística por verbos/objetos ("revisa", "analiza", "resume mis", "busca en", correos/Slack/Drive/PRs/logs/datos) + palabras temporales ("en segundo plano", "avísame cuando"). Se apoya en el `classify_intent` existente (Dev/Inbox/General). Override explícito: "hazlo en segundo plano" fuerza `Heavy`; "dime ya" fuerza `Light`.
- **Cerebro aislado por tarea.** Nuevo trait `BrainFactory` (lo implementa `jarvis-app` con la config actual): cada tarea obtiene **su propia instancia** de `CliBrain`/`ClaudeClient` → historial propio, `CancellationToken` propio, timeout propio (`with_timeout(task_timeout)`). El barge-in sigue llamando `cancel()` solo sobre el cerebro conversacional.
- **Prompt de tarea**: persona + "Eres un subagente de fondo. Devuelve: 1) RESUMEN_HABLADO (≤ 60 palabras, sin markdown) 2) DETALLE (markdown)". El parser separa ambas secciones; si el modelo no respeta el formato, se pide un resumen en una segunda llamada corta.
- **`SpeechQueue`** reemplaza a `deferred: VecDeque<String>`:
  - Prioridades: `Critical` (errores de seguridad, confirmaciones de HU-39) > `Reply` (respuesta del turno actual) > `TaskResult` > `Notice` (IPC/avisos).
  - FIFO dentro de cada prioridad; tope 32 elementos (al exceder, se descartan `Notice` antiguos, nunca `TaskResult`).
  - Política de entrega `DeliveryGate`: hablar solo si `state ∈ {Idle, Listening}` **y** no hay `speaking` **y** `last_voice_activity` > 1.5 s **y** no `Muted` **y** no `gaming_mode` (salvo `Critical`).
- **Tick**: el `select!` del `BrainLoop` agrega un `tick(250 ms)` para evaluar el gate sin depender de que termine otra locución.

### 3.2. `jarvis-ipc`
- Métodos nuevos en `protocol::methods`: `jarvis.dispatchTask`, `jarvis.getTasks`, `jarvis.getTask`, `jarvis.cancelTask`.
- Notificaciones servidor→cliente (a conexiones con `jarvis.hello` y capacidad `tasks`): `jarvis.taskUpdated`.
- `SpeakParams` gana `queue: Option<bool>` (default `false` = compatibilidad) y `priority: Option<String>`; `SpeakResult` gana `queued: Option<bool>`, `position: Option<u32>`.
- `ServerState` recibe un `Arc<dyn TaskPort>` (trait implementado por el `TaskManager`), igual que hoy recibe `SpeechSink`.

### 3.3. `jarvis-core`
- Campos: `tasks_max_parallel: u8` (default 2), `tasks_timeout_secs: u32` (default 900), `tasks_quiet_gap_ms: u32` (default 1500).
- Nuevo `paths::state_dir()` (`$XDG_STATE_HOME/fokkus-jarvis`, por defecto `~/.local/state/fokkus-jarvis`; hoy `paths.rs` solo expone `config_dir`, `config_file` y `lock_file`). Resultados en `state_dir().join("tasks")`, directorio `0700`, archivos `0600`, retención 7 días (limpieza al arrancar).
- `classify_intent` (hoy en `conversation.rs`, devuelve `FillerCategory`) se reutiliza para elegir el acuse adecuado (Inbox → "Reviso tu bandeja y te aviso").

### 3.4. `jarvis-app`
- Implementa `BrainFactory` a partir de `settings::build_brain` (misma lógica, instancia nueva).
- En `apply_config` actualiza la fábrica: las tareas en curso **terminan con el cerebro con que empezaron**.

---

## 4. Interfaces y structs Rust

```rust
// jarvis-audio/src/tasks.rs
pub type TaskId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload { Light, Heavy }

pub fn classify_workload(query: &str) -> Workload;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus { Queued, Running, Done, Failed, Cancelled, TimedOut }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOrigin { Voice, Ipc }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: TaskId,
    pub title: String,          // "tus correos" (para el encabezado hablado)
    pub prompt: String,
    pub origin: TaskOrigin,
    pub status: TaskStatus,
    pub created_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub spoken_summary: Option<String>,
    pub detail_path: Option<PathBuf>,
    pub error: Option<String>,
}

pub trait BrainFactory: Send + Sync {
    /// Cerebro nuevo e independiente (historial y cancelación propios).
    fn spawn(&self, timeout: Duration) -> Result<Box<dyn TokenStream + Send>, BrainError>;
}

pub struct TaskManager { /* workers, registry: Arc<Mutex<Vec<TaskRecord>>>, queue tx */ }

impl TaskManager {
    pub fn start(factory: Arc<dyn BrainFactory>, cfg: TaskConfig, out: SpeechQueueHandle) -> std::io::Result<Self>;
    pub fn submit(&self, title: String, prompt: String, origin: TaskOrigin) -> TaskId;
    pub fn cancel(&self, id: TaskId) -> bool;
    pub fn cancel_all(&self) -> usize;
    pub fn find_by_title(&self, spoken: &str) -> Vec<TaskId>;
    pub fn list(&self) -> Vec<TaskRecord>;
    pub fn shutdown(&mut self, deadline: Duration);
}

// jarvis-audio/src/orchestrator/speech_queue.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SpeechPriority { Notice, TaskResult, Reply, Critical }

#[derive(Debug, Clone)]
pub struct Delivery {
    pub text: String,
    pub priority: SpeechPriority,
    pub task: Option<TaskId>,
    pub enqueued_at: Instant,
}

pub struct SpeechQueue { /* BinaryHeap por (prioridad, secuencia) */ }
impl SpeechQueue {
    pub fn push(&mut self, d: Delivery) -> u32;          // devuelve posición
    pub fn pop_ready(&mut self, gate: &DeliveryGate, now: Instant) -> Option<Delivery>;
    pub fn pending(&self) -> usize;
}

pub struct DeliveryGate {
    pub state: JarvisState,
    pub speaking: bool,
    pub last_voice_activity: Option<Instant>,
    pub quiet_gap: Duration,
    pub muted: bool,
    pub gaming_mode: bool,
}

// LocalCommand (orchestrator.rs) — variantes nuevas
pub enum LocalCommand {
    // …existentes…
    CancelTask { spoken_target: Option<String> }, // None = todas
    ListTasks,
    PendingResults,
}

// jarvis-ipc/src/protocol.rs
pub struct DispatchTaskParams { pub title: String, pub prompt: String }
pub struct DispatchTaskResult { pub id: u64, pub status: String }
pub struct CancelTaskParams { pub id: u64 }
pub trait TaskPort: Send + Sync {
    fn dispatch(&self, title: String, prompt: String) -> u64;
    fn list(&self) -> Vec<serde_json::Value>;
    fn get(&self, id: u64) -> Option<serde_json::Value>;
    fn cancel(&self, id: u64) -> bool;
}
```

---

## 5. Casos de borde

| Caso | Comportamiento |
|---|---|
| Cerebro CLI ausente al despachar | La tarea pasa a `Failed` de inmediato y se dice "No tengo cerebro para tareas de fondo". |
| El usuario cambia de cerebro en Ajustes con tareas corriendo | Las tareas en curso siguen con el cerebro original; las nuevas usan el nuevo. |
| Apagado del daemon con tareas `Running` | `shutdown` cancela (mata árboles de procesos), marca `Cancelled` y persiste el registro; al volver a arrancar no se re-ejecutan (se informa si el usuario pregunta). |
| Dos tareas con el mismo título ("correos") | `find_by_title` devuelve ambas → Jarvis pregunta "¿La primera o la segunda?". |
| Resultado vacío o error del modelo | `Failed` con mensaje hablado corto; detalle del error solo en log (sin datos del prompt). |
| La salida del CLI excede 1 MB | Se trunca el detalle en disco con marca `[truncado]`; el resumen se pide sobre los primeros 64 KB. |
| `jarvis.speak` con `queue:false` mientras habla | Comportamiento actual (compatibilidad): se encola internamente como `Notice` y responde `spoken:true`. |
| Turno del usuario empieza justo cuando el gate abre | El gate exige `speaking == false` y `last_voice_activity` > gap **bajo el mismo lock** del `SharedCore`; si el VAD dispara, la entrega se aborta antes de sintetizar. |
| Resultado retenido > 24 h | Se descarta de la cola hablada (queda en disco). |
| Prompt injection en datos (correo que dice "borra todo") | Las tareas de fondo de esta HU son **solo lectura/resumen**; acciones con efecto pasan por HU-39 con confirmación vocal. |

---

## 6. Plan de tests

**Unitarios**
- `classify_workload`: tabla de 30 frases (pesadas, ligeras, overrides).
- `SpeechQueue`: prioridad > FIFO; tope 32 descarta `Notice`; posición devuelta; `pop_ready` respeta cada condición de `DeliveryGate` por separado.
- Parser de salida de tarea (`RESUMEN_HABLADO`/`DETALLE`, salida sin formato → fallback).
- `LocalCommand` nuevos: "cancela la tarea de los correos", "cancela todas las tareas", "¿qué tareas tengo?".

**Integración (`orchestrator/tests.rs`, con dobles)**
- `BrainFactory` falso con latencia de 2 s: el turno pesado da acuse < 1 s; un turno ligero posterior se responde mientras la tarea corre.
- Barge-in durante una respuesta ligera con tarea activa: la tarea termina `Done`.
- Dos tareas que terminan durante una locución: se entregan en orden y sin solaparse (orden de `PlayOutcome` en el sink falso).
- `Muted` durante la finalización: nada se locuta; al desmutear se anuncia "Tienes 1 resultado pendiente".
- Timeout configurado en 1 s → `TimedOut` + aviso.

**IPC**
- `dispatchTask` → `getTasks` muestra `running` → `done`; `cancelTask` de id inexistente → error `INVALID_PARAMS`; `speak` con `queue:true` devuelve `position`.

**Manual**
- "Jarvis, revisa mis correos de hoy y dime si hay algo urgente" con `agy`/`claude` real; mientras tanto, conversar; verificar entrega al final y archivo en `tasks/`.
