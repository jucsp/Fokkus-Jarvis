# SDD HU-66 · IPC y CLI de sub-sesiones

**Épica 14** (multi-sesiones / sub-hilos) — fase C, superficie y operación.
Contrato JSON-RPC 2.0 sobre Unix Domain Socket (`~/.fokkus/jarvis.sock`),
framing NDJSON (un objeto JSON por línea, 1 MiB máximo).

Este documento es la fuente de verdad de la que se valida el catálogo de
`jarvis-ipc`: el test `catalog_is_documented_hu66` (en `protocol.rs`) falla si
algún método de `methods::ALL` o algún evento de `session_event::ALL` no
aparece aquí.

---

## 1. Catálogo completo de métodos

### 1.1 Núcleo (sin cambios)

| Método | Tipo | Resultado |
|---|---|---|
| `jarvis.hello` | request | `HelloResult` (handshake, versión de protocolo y `capabilities`) |
| `jarvis.ping` | request | `PingResult` |
| `jarvis.getState` | request | `StateResult` |
| `jarvis.setMuted` | request | `SetMutedResult` |
| `jarvis.speak` | request | `SpeakResult` |

### 1.2 Swarm de desarrollo (sin cambios)

| Método | Tipo | Resultado |
|---|---|---|
| `jarvis.dispatchSwarm` | request | `SwarmTask` |
| `jarvis.getSwarmStatus` | request | `SwarmStatusResult` |
| `jarvis.notifySwarmProgress` | notification | — |
| `jarvis.notifySwarmCompleted` | notification | — |
| `jarvis.notifyTriageAlert` | notification | — |

### 1.3 Tareas pesadas de fondo (HU-37, **alias compatibles**)

| Método | Tipo | Resultado |
|---|---|---|
| `jarvis.dispatchTask` | request | `DispatchTaskResult { id, status }` |
| `jarvis.getTasks` | request | `TasksResult { tasks: [registro…] }` |
| `jarvis.getTask` | request | registro de la tarea |
| `jarvis.cancelTask` | request | `CancelTaskResult { id, cancelled }` |

Estos cuatro métodos **no cambian**: `fokkus-jarvis dev run`, `task run` y
Fokkus IDE siguen usándolos igual que en v0.2.29 (HU-66 · CA-2).

### 1.4 Sub-sesiones (HU-66, nuevos)

| Método | Tipo | Parámetros | Resultado |
|---|---|---|---|
| `jarvis.sessions.list` | request | — | `SessionsResult { sessions: [registro…] }` |
| `jarvis.sessions.get` | request | `SessionIdParams` | registro de la sub-sesión |
| `jarvis.sessions.send` | request | `SessionSendParams` | `SessionSendResult { id, ok }` |
| `jarvis.sessions.cancel` | request | `SessionIdParams` | `SessionCancelResult { id, cancelled }` |
| `jarvis.sessions.read` | request | `SessionIdParams` | `SessionReadResult { id, detail }` |
| `jarvis.sessionEvent` | **notification** | `SessionEventParams` | — |

#### `jarvis.sessions.list`

Lista las sub-sesiones conocidas, ordenadas por id. Cada elemento es el
registro serializado del hilo con la misma forma que `jarvis.getTasks`
mientras las sub-sesiones sean tareas de fondo (`id`, `title`, `status`,
`origin`, `spoken_summary`, `detail_path`, `error`).

```json
{"jsonrpc":"2.0","method":"jarvis.sessions.list","id":1}
{"jsonrpc":"2.0","result":{"sessions":[{"id":3,"title":"revisión de PRs de Felipe","status":"running"}]},"id":1}
```

Errores: `-32603` si el gestor de tareas no está activo (falta cerebro o
configuración).

#### `jarvis.sessions.get`

Devuelve el registro de una sub-sesión. Misma carga útil que
`jarvis.getTask`, de modo que un cliente puede migrar método a método.

```json
{"jsonrpc":"2.0","method":"jarvis.sessions.get","params":{"id":3},"id":2}
```

Errores: `-32602` si la sub-sesión no existe.

#### `jarvis.sessions.send`

Envía una instrucción nueva al hilo (multi-turno). La instrucción no puede
venir vacía ni solo con espacios.

```json
{"jsonrpc":"2.0","method":"jarvis.sessions.send",
 "params":{"id":3,"instruction":"revisa también la PR 214"},"id":3}
{"jsonrpc":"2.0","result":{"id":3,"ok":true},"id":3}
```

Errores:

* `-32602` si `instruction` está vacía o la sub-sesión no existe.
* `-32603` si el puerto de tareas no soporta `send_session` o falla al
  aceptarla.

Al aceptarse se emite `jarvis.sessionEvent` con `event: "instruction"`.

#### `jarvis.sessions.cancel`

Cancela el turno en curso. `cancelled: false` no es un error: significa que la
sub-sesión existía pero ya había terminado y no había nada que cancelar.

```json
{"jsonrpc":"2.0","method":"jarvis.sessions.cancel","params":{"id":3},"id":4}
{"jsonrpc":"2.0","result":{"id":3,"cancelled":true},"id":4}
```

Errores: `-32602` si la sub-sesión no existe.

Al cancelarse se emite `jarvis.sessionEvent` con `event: "cancelled"`.

#### `jarvis.sessions.read`

Devuelve el detalle en markdown del último turno del hilo (`detail.md`), que
es lo mismo que imprime `fokkus-jarvis session show <id>` desde disco.

```json
{"jsonrpc":"2.0","method":"jarvis.sessions.read","params":{"id":3},"id":5}
{"jsonrpc":"2.0","result":{"id":3,"detail":"# PR 214\n\nSin hallazgos graves."},"id":5}
```

Errores: `-32602` si la sub-sesión no existe o no tiene detalle guardado.

#### `jarvis.sessionEvent` (notification)

El servidor emite este evento cuando cambia el estado de una sub-sesión. Como
toda notificación, no lleva `id` y no admite respuesta.

Parámetros (`SessionEventParams`):

| Campo | Tipo | Obligatorio | Significado |
|---|---|---|---|
| `id` | u64 | sí | id corto del hilo |
| `event` | string | sí | tipo de evento (ver abajo) |
| `title` | string | no | título del hilo |
| `status` | string | no | estado tras el evento (`queued`, `working`, `idle`, `awaiting_user`, `done`, `failed`, `cancelled`, `timed_out`) |
| `detail` | string | no | resumen hablado, avance o pregunta |

Los campos opcionales se omiten cuando no se conocen (nunca viajan como
`null`).

```json
{"jsonrpc":"2.0","method":"jarvis.sessionEvent",
 "params":{"id":3,"event":"replied","title":"revisión de PRs de Felipe",
           "status":"idle","detail":"dos PR listas, sin hallazgos graves"}}
```

Eventos del catálogo (`protocol::session_event::ALL`):

| Evento | Cuándo |
|---|---|
| `"created"` | se abrió la sub-sesión |
| `"started"` | el agente empezó un turno |
| `"progress"` | avance del turno (no se lee por voz) |
| `"instruction"` | llegó una instrucción nueva al hilo |
| `"replied"` | turno terminado con resumen hablado |
| `"question"` | el agente pide una decisión al usuario |
| `"finished"` | estado terminal (cerrada, fallida, interrumpida) |
| `"cancelled"` | el usuario canceló el hilo |

**Alcance actual:** `ServerState` registra cada evento (`session_events()`) y
expone `emit_session_event()` devolviendo la `Notification` lista para enviar.
El envío a los clientes conectados queda pendiente: el bucle de conexión es
bloqueante en lectura, así que el *push* necesita un canal por conexión y un
hilo escritor (ver HU-67, que ya consume estos eventos en el HUD).

---

## 2. CLI

```text
fokkus-jarvis session list                 # id, estado y título de cada hilo
fokkus-jarvis session show <id>            # detalle.md (lee del disco, sin daemon)
fokkus-jarvis session cancel <id>          # jarvis.sessions.cancel
fokkus-jarvis session send <id> <texto…>   # jarvis.sessions.send
```

* `session list`, `session cancel` y `session send` hablan por el socket: si
  Jarvis no está corriendo fallan con código 1.
* `session show` lee `state_dir()/sessions/<id>/detail.md` o, si no existe,
  `state_dir()/tasks/<id>.md` (o el `<ms>-<id>.md` más reciente). Acepta `#12`.
* El id se puede escribir con `#` (`session show #12`).

### Antirrecursión (HU-60 extendida)

Un subagente de fondo no puede abrir trabajo nuevo. Si `session send` corre
con `JARVIS_SUBSESSION_ID` presente en el entorno, se niega de inmediato:

```text
Error: fokkus-jarvis session send no está permitido dentro de una sub-sesión (antirrecursión)
```

y sale con código `1`, antes de tocar el socket. Igual que `dev run` (HU-60) y
`task run` (HU-61).

---

## 3. Compatibilidad

| Cliente | Antes | Ahora |
|---|---|---|
| `fokkus-jarvis dev run` | `jarvis.dispatchTask` + `DEV_TASK_TITLE` | igual |
| `fokkus-jarvis task run --profile …` | `jarvis.dispatchTask` | igual |
| Fokkus IDE (tareas) | `jarvis.getTasks` / `getTask` / `cancelTask` | igual, y además `jarvis.sessions.*` |
| Fokkus IDE (sub-sesiones) | — | `jarvis.sessions.*` + `jarvis.sessionEvent` |

`jarvis.hello` devuelve el catálogo de 20 métodos en `capabilities`, de modo
que un cliente puede descubrir en caliente si el servidor soporta
`jarvis.sessions.send` antes de usarlo.
