# Propuesta técnica — Épica 14: Hilo Central + Sub-sesiones Ramificadas

**Proyecto:** Fokkus-Jarvis · **Versión base analizada:** v0.2.29 (`422d77d`) · **Fecha:** 2026-10-27
**Autor:** Claude Code (PM/QA del Swarm) · **Destinatarios:** Antigravity (PO), DeepSeek V4.1 Flash (Dev Senior), Ollama (docs)

---

## 1. Contexto de negocio

En pruebas reales el usuario pidió por voz: *"revísame unas PR en segundo plano mientras me sintetizas un mensaje para enviar por chat"*. Jarvis a veces despacha bien y a veces no. Además el usuario quiere más que tareas sueltas: quiere trabajar con Jarvis como trabaja con varias sesiones abiertas en Antigravity (por ejemplo, "Jarvis pendientes" y "Revisión de PRs de Felipe" en paralelo). Cada sesión tiene su propio contexto, y una sesión central puede entrar a revisar otra.

El objetivo es un **embudo conversacional**:

- un **hilo central** (la voz de Jarvis) recibe todo lo que dice el usuario;
- **sub-sesiones** con historial, agente y estado propios trabajan en paralelo;
- el usuario puede crear, consultar, retomar, dar instrucciones y cancelar cada sub-sesión **por voz**: *"Jarvis, en la revisión de PRs, enfócate en el módulo de pagos"*, *"¿cómo va el hilo de los logs?"*.

Hoy ese modelo no existe. Lo que hay es un gestor de **tareas de un solo disparo** (HU-37/57), sin conversación propia, que se activa mediante una heurística de palabras clave poco fiable.

---

## 2. Diagnóstico de las limitaciones actuales

### 2.1 Flujo actual (as-is)

```
STT ──► BrainLoop::on_turn (orchestrator.rs:3135)
          ├─ local_command(query)            → hora, apps, cancelar/listar tareas, web…
          ├─ classify_workload(query)==Heavy → dispatch_task → TaskManager::submit (1 prompt, 1 disparo)
          └─ si no                           → converse() con el cerebro principal (síncrono, cli_timeout 120 s)
```

`TaskManager` (`tasks.rs:564`) ejecuta cada tarea en un hilo con un cerebro **nuevo**. Hace **una sola** llamada `stream_tokens`, guarda el `DETALLE` en `state_dir()/tasks/*.md` y encola un `RESUMEN_HABLADO`. Cuando termina, la tarea muere: no se le puede volver a hablar.

### 2.2 Evidencia empírica del clasificador

Para medir el comportamiento real se ejecutó contra `classify_workload` / `task_title` una prueba temporal (`cargo test -p jarvis-audio`; el archivo se eliminó después):

| Frase hablada | Resultado | Esperado | Problema |
|---|---|---|---|
| "revísame unas PR en segundo plano mientras me sintetizas un mensaje para Felipe" | Heavy, título "revísame unas PR en segundo plano" | Dividir en 2: PR→sub-sesión y mensaje→respuesta inmediata | Todo se va a una sola tarea. El mensaje nunca se responde en primer plano. |
| "revísame las PR de Felipe" | **Light** | Heavy | `"revísame"` no contiene `"revisa"` (tilde + enclítico) y `"PR"` singular no está en `HEAVY_NOUNS` |
| "revisa las PR de Felipe" | **Light** | Heavy | `"pr"` no existe; solo `"prs"` y `"pull request"` |
| "échale un ojo a los pull requests de Felipe" | **Light** | Heavy | Faltan verbos coloquiales ("échale un ojo", "chequea", "mira") |
| "chequea las pe erre del repo de melón" | **Light** | Heavy | Variante fonética de STT ("pe erre") y verbo ausente |
| "analízame los logs del servidor" | **Light** | Heavy | `"analízame"` ≠ `"analiza"` (tilde) |
| "revisa el reporte de ventas" | **Heavy** | Light/ambiguo | Falso positivo: `"repo"` ⊂ `"reporte"` (match por substring) |
| "resume el blog de Rust" | **Heavy**, título "los logs" | Light | Falso positivo: `"log"` ⊂ `"blog"`; además el título sale mal |
| "dale una revisión a los PRs y mientras tanto ayúdame con un correo" | **Light** | Compuesta | "revisión" no es verbo de la lista; nada detecta la conjunción |

**Conclusión:** el "a veces sí, a veces no" es determinista. Depende de la forma morfológica exacta que transcribe Whisper.

### 2.3 Causas raíz

| # | Limitación | Evidencia | Impacto |
|---|---|---|---|
| D1 | **Clasificación léxica rígida y por substring.** `HEAVY_*` usa `contains_any` sobre `to_lowercase()` sin plegar tildes ni separar palabras. `DEV_*`/`OPS_*` sí usan `normalize_words`: hay dos criterios distintos en el mismo módulo. | `tasks.rs:194-216` vs `tasks.rs:220-282` | Falsos negativos con enclíticos y tildes ("revísame", "analízame"). Falsos positivos ("reporte", "blog", "catálogo"). |
| D2 | **Sin noción de instrucción compuesta.** El turno se trata como una unidad: `Heavy` despacha *todo* el texto a background y `Light` lo procesa *todo* síncrono. | `orchestrator.rs:3224` | En "haz X en segundo plano mientras haces Y", la parte Y se pierde o la ejecuta el subagente (que no habla con el usuario). |
| D3 | **Falso negativo = ejecución síncrona con timeout.** Si una revisión de PRs cae en `Light`, la ejecuta `converse()` con el cerebro principal y `cli_timeout_secs=120`. | `config.rs:328`, `brain/cli.rs:27` | Bloquea la conversación. Si tarda más de 120 s se mata y responde `NETWORK_ERROR_PHRASE`. Para el usuario parece que Jarvis "no lo hizo". |
| D4 | **El LLM principal solo sabe derivar "desarrollos".** El `SYSTEM_PROMPT` le indica `fokkus-jarvis dev run` únicamente para desarrollo/pipelines, no para revisión, análisis o investigación. | `brain/client.rs:67-71` | El LLM, que es la segunda red de seguridad, no rescata los falsos negativos de D1. |
| D5 | **El subagente recibe la persona de voz.** `build_task_brain` crea `CliBrain::new(...)` con el `SYSTEM_PROMPT` de Jarvis ("una o dos frases", "despacha con dev run"). Además `CliBrain::build_prompt` agrega *"Responde únicamente … en una frase breve"*. | `settings.rs:118-121`, `brain/cli.rs:269` | Instrucciones contradictorias con `RESUMEN_HABLADO/DETALLE`, `DETALLE` pobre, y riesgo de **recursión**: un subagente de desarrollo puede ejecutar `fokkus-jarvis dev run` y crear otra tarea. |
| D6 | **Prompt de solo lectura para tareas que necesitan herramientas.** `task_prompt` dice *"Solo lees y resumes: no ejecutes acciones con efecto"*. Revisar PRs exige `gh pr diff`/`git`, y el modelo suele interpretarlo como prohibición de usar herramientas. | `tasks.rs:421-431` | Resultado "teórico", el mismo síntoma que motivó HU-57. |
| D7 | **Modo API sin herramientas.** `ClaudeClient` no declara `tools`. En `BrainMode::Api` un subagente no puede leer repos ni PRs. | `brain/client.rs` (no hay `tools`) | La tarea alucina o falla sin dar aviso. |
| D8 | **Tareas de un solo disparo, sin historial ni seguimiento.** Cada `TaskRecord` es *prompt → resultado*. No se le pueden dar instrucciones nuevas, no reporta progreso intermedio y el `DETALLE` solo existe en disco. | `tasks.rs:303-316`, `tasks.rs:784-910` | No hay "sub-hilos": no se puede entrar a una tarea, preguntarle ni redirigirla. |
| D9 | **El hilo central no conoce las tareas.** El cerebro conversacional no recibe el estado del `TaskManager`. "¿Cómo va lo de las PR?" solo funciona si coincide con `LIST_TASK_PHRASES`, y ese listado solo dice títulos. | `orchestrator.rs:3108-3133` | No responde preguntas de seguimiento ("¿encontró algo grave?"). |
| D10 | **Títulos y búsqueda por nombre frágiles.** `task_title` toma las primeras 6 palabras si no reconoce el tema. `find_by_title` hace `contains` bidireccional. | `tasks.rs:387-418`, `tasks.rs:683-699` | "Cancela la de Felipe" no encuentra "revísame unas PR en segundo plano". Las referencias por voz fallan. |
| D11 | **Capacidad limitada y silenciosa.** `max_parallel` es 2 por defecto (tope 4). A partir de la tercera, la tarea queda `Queued` sin aviso diferenciado. | `config.rs:318`, `tasks.rs:598` | Con varias sesiones en paralelo, el usuario no sabe cuáles están esperando. |
| D12 | **Volatilidad.** El registro vive en memoria y los ids reinician con el proceso. Solo sobrevive el `.md` del detalle. | `tasks.rs:551-560`, `tasks.rs:982-984` | Si Jarvis se reinicia, se pierden todas las sub-sesiones. |

---

## 3. Arquitectura propuesta (to-be)

### 3.1 Visión general

```
                         ┌───────────────────────────── HILO CENTRAL ─────────────────────────────┐
 STT ─► on_turn ─► local_command (instantáneo, sin LLM: hora, apps, kill switch, cancelar…)        │
                     │                                                                             │
                     ▼                                                                             │
               TurnRouter  ──(1) direccionamiento explícito ("en la revisión de PRs, …")           │
                 │         ──(2) heurística rápida normalizada (verbos/objetos/override)            │
                 │         ──(3) planificador LLM (JSON) para casos ambiguos o compuestos           │
                 ▼                                                                                 │
             TurnPlan { foreground, spawn[], route[], query[], control[] }                         │
                 │                                                                                 │
   ┌─────────────┼──────────────────────┬──────────────────────────┐                               │
   ▼             ▼                      ▼                          ▼                               │
 converse()  SessionManager.spawn   SessionManager.send       SessionManager.status/read           │
 (respuesta  (nueva sub-sesión)     (instrucción a sub-sesión) (consulta sin despertar al agente)  │
  inmediata)                                                                                       │
                 └──────────── eventos SessionEvent ──► SpeechQueue / HUD / IPC ◄──────────────────┘
                                    │
             ┌──────────────────────┼──────────────────────┐
             ▼                      ▼                      ▼
        SubSession #1          SubSession #2          SubSession #3
        "PRs de Felipe"        "Monitor ping"         "Logs de Melón"
        historial propio       proceso vivo           historial propio
        agente + perfil        perfil Operator        perfil ReadOnly
```

Hay tres ideas clave:

1. **Separar el enrutamiento de la ejecución.** `TurnRouter` decide *a dónde* va cada parte del turno, y `SessionManager` ejecuta.
2. **La sub-sesión es conversacional y persistente, no un *job*.** Tiene su propio historial, recibe instrucciones posteriores y conserva el cerebro (o su transcripción) entre turnos.
3. **El hilo central sabe qué sub-sesiones existen.** En cada turno recibe un *roster* compacto en su contexto, igual que una sesión de Antigravity puede "ir a mirar" otra.

### 3.2 Módulos nuevos y cambios

| Módulo | Crate | Responsabilidad |
|---|---|---|
| `text/normalize.rs` *(nuevo)* | `jarvis-audio` | Plegado de tildes, tokenización por palabra, lematización ligera de enclíticos (`revísame`→`revisa`, `analízalo`→`analiza`), alias fonéticos (`pe erre`/`pr`/`pull`→`pr`). Fuente única para `tasks.rs` y `orchestrator.rs`. |
| `router/mod.rs` *(nuevo)* | `jarvis-audio` | `TurnRouter`: produce `TurnPlan` combinando direccionamiento explícito, heurística y planificador LLM. |
| `router/planner.rs` *(nuevo)* | `jarvis-audio` | Llamada LLM corta (modelo rápido, `max_tokens` bajo, timeout de unos 2,5 s) con salida JSON validada. Si falla, se usa la heurística. |
| `sessions/mod.rs` *(nuevo; reemplaza progresivamente `tasks.rs`)* | `jarvis-audio` | `SessionManager`, `SubSession`, ciclo de vida, pool, eventos. |
| `sessions/store.rs` *(nuevo)* | `jarvis-audio` | Persistencia en `state_dir()/sessions/<id>/{meta.json,transcript.jsonl,detail.md}` (0700/0600) y rehidratación al arrancar. |
| `sessions/prompts.rs` *(nuevo)* | `jarvis-audio` | `SUBAGENT_SYSTEM_PROMPT` independiente de la persona de voz y prompts por perfil (ReadOnly / Operator / Developer). |
| `brain/cli.rs` | `jarvis-audio` | `CliBrain::with_mode(BrainMode::Voice \| BrainMode::Worker)`: en `Worker` no se agrega la coletilla "frase breve". Se inyecta `JARVIS_SUBSESSION_ID` en el entorno del proceso hijo. |
| `orchestrator.rs` | `jarvis-audio` | `on_turn` delega en `TurnRouter`. Se agrega el estado `focus: Option<SessionId>`. El roster se inyecta en el contexto del cerebro principal. |
| `jarvis-ipc` protocol/server | `jarvis-ipc` | Métodos `jarvis.sessions.{list,get,send,cancel,read}` y notificación `jarvis.sessionEvent`. `jarvis.dispatchTask` se conserva como alias de compatibilidad. |
| `main.rs` (CLI) | `jarvis-app` | `fokkus-jarvis session list\|show <id>\|send <id> "<texto>"\|cancel <id>`. `dev run` rechaza la ejecución si existe `JARVIS_SUBSESSION_ID` (antirecursión). |
| HUD / tray | `jarvis-render`, `jarvis-shell` | Chips de sub-sesiones (título, estado, tiempo) y submenú en la bandeja. |

### 3.3 Modelo de datos

```rust
pub type SessionId = u32;            // corto y pronunciable: "hilo 3"

pub enum SessionStatus {
    Queued,          // esperando slot del pool
    Working,         // el agente está procesando un turno
    Idle,            // terminó su último turno; acepta instrucciones nuevas
    AwaitingUser,    // el agente pidió una decisión (pregunta pendiente)
    Done,            // cerrada por el usuario o por política de expiración
    Failed, Cancelled, TimedOut,
}

pub enum SessionProfile {
    ReadOnly,        // leer/resumir; puede usar herramientas de lectura (gh pr view/diff, git log, cat)
    Operator,        // procesos del sistema (HU-57): ping, monitores, scripts; cancelables
    Developer,       // código, tests, pipelines (Issue #7)
}

pub struct SessionTurn {
    pub role: Role,                   // User | Agent | System
    pub text: String,
    pub at_ms: u64,
}

pub struct SubSession {
    pub id: SessionId,
    pub title: String,                // "revisión de PRs de Felipe" (lo genera el planner)
    pub aliases: Vec<String>,         // ["prs", "felipe", "pull requests"] para direccionamiento por voz
    pub profile: SessionProfile,
    pub status: SessionStatus,
    pub origin: TaskOrigin,           // Voice | Ipc
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub transcript: Vec<SessionTurn>, // historial propio (persistido en JSONL)
    pub last_summary: Option<String>, // último RESUMEN_HABLADO
    pub progress: Option<String>,     // última línea de PROGRESO: emitida en streaming
    pub pending_question: Option<String>,
    pub detail_path: Option<PathBuf>,
    pub unread: bool,                 // resultado aún no escuchado por el usuario
}

pub enum SessionEvent {
    Created { id, title },
    Started { id },
    Progress { id, text },            // no se habla por defecto; alimenta el estado y el HUD
    Replied { id, summary, detail_path },
    Question { id, text },            // se habla con prioridad TaskResult
    Finished { id, status },
}
```

El **agente asociado** a cada sub-sesión es un `Arc<dyn TokenStream>` creado por `BrainFactory::spawn_worker(profile, timeout)`. Se **mantiene vivo** mientras la sesión está `Idle`, para conservar el historial de `CliBrain` o `ClaudeClient`. Tras N minutos de inactividad se libera y, cuando llega una instrucción nueva, se **rehidrata** desde `transcript.jsonl`. Esto da continuidad sin depender de que `agy` soporte `--resume`.

Protocolo de salida del subagente: se extiende el actual con `PROGRESO:` (líneas opcionales durante el streaming), `PREGUNTA:` (pide una decisión al usuario y deja la sesión en `AwaitingUser`), `RESUMEN_HABLADO:` y `DETALLE:`.

### 3.4 `TurnRouter` y `TurnPlan`

```rust
pub struct TurnPlan {
    pub foreground: Option<String>,              // parte que responde el hilo central ahora
    pub spawn: Vec<SpawnSpec>,                   // sub-sesiones nuevas
    pub route: Vec<(SessionId, String)>,         // instrucciones a sub-sesiones existentes
    pub query: Vec<SessionQuery>,                // "¿cómo va X?", "léeme el detalle de X"
    pub control: Vec<SessionControl>,            // enfocar, salir de foco, cerrar, cancelar
}

pub struct SpawnSpec { pub title: String, pub aliases: Vec<String>, pub profile: SessionProfile, pub instruction: String }
pub enum SessionQuery { Status(SessionRef), ReadDetail(SessionRef), ListAll }
pub enum SessionControl { Focus(SessionRef), Unfocus, Close(SessionRef), Cancel(SessionRef) }
pub enum SessionRef { Id(SessionId), Alias(String), Latest, Focused }
```

**Etapas del router, en orden y con *short-circuit*:**

1. **Comandos locales** (sin cambios): hora, apps, kill switch, `cancela …`. Son instantáneos y no consumen LLM.
2. **Direccionamiento explícito** (determinista, sin LLM). Patrones sobre el texto normalizado:
   - `en (el|la) (hilo|sesión|tarea|revisión) (de|del)? <ref>, <instrucción>` → `route`
   - `(entra|ve|vamos|cambia|pasa) (al|a la) (hilo|sesión) <ref>` → `Focus`
   - `(vuelve|volvamos) (al hilo principal|a lo general|contigo)` / `sal del hilo` → `Unfocus`
   - `¿cómo va(n)? (el|la|lo) (hilo|tarea|sesión)? (de)? <ref>` → `query Status`
   - `léeme (el detalle|lo que encontró) (de)? <ref>` → `query ReadDetail`
   - `hilo <n>` / `el primero|segundo|último` → `SessionRef::Id` u ordinal

   `<ref>` se resuelve contra `title` + `aliases` con *score* por tokens normalizados (Jaccard ≥ 0,5, con desempate por la más reciente). Si hay ambigüedad, se pregunta: *"¿La de Felipe o la de Oscar?"*.
3. **Heurística rápida** (la de `classify_workload` reescrita sobre `text/normalize.rs`):
   - si hay *override* explícito ("en segundo plano", "avísame cuando…") **y** un conector de simultaneidad (`mientras`, `mientras tanto`, `y aparte`, `en paralelo`, `al mismo tiempo`) → turno **compuesto**, pasa a la etapa 4;
   - si es una única intención pesada clara → `spawn` directo sin LLM, para no sumar latencia en el caso fácil;
   - si es claramente ligera → `foreground`.
4. **Planificador LLM** (solo para casos compuestos o ambiguos). Prompt corto con el roster y el texto, y respuesta JSON con el esquema de `TurnPlan`. La validación es estricta: un JSON inválido o un timeout hacen caer en la heurística. Ejemplo:

```json
{
  "foreground": "Sintetiza el mensaje que el usuario quiere enviar por chat.",
  "spawn": [{"title": "revisión de PRs de Felipe", "aliases": ["prs","felipe","pull requests"],
             "profile": "read_only", "instruction": "Revisa los PR abiertos de Felipe…"}],
  "route": [], "query": [], "control": []
}
```

5. **Modo foco.** Si `focus = Some(id)` y el turno no tiene direccionamiento explícito ni es un comando local, el router **pregunta primero** al planner si el turno sigue el foco o es para el hilo central. Para no sumar latencia, la regla barata es esta: si el turno hace referencia anafórica ("y también…", "ahora revisa…", "agrega…"), va al foco; si es una pregunta general (hora, clima, "¿qué es…?"), va al hilo central. El foco caduca tras 5 minutos sin turnos dirigidos a él y se anuncia: *"Sigo en la revisión de PRs"* / *"Volvimos al hilo principal"*.

**Ejecución del plan:** primero los `control` y `spawn`, que son instantáneos y producen un acuse corto. Después `route`/`query`. Al final `foreground` mediante `converse()`. El acuse y la respuesta en primer plano se **funden en una sola locución**: *"Abrí la revisión de PRs de Felipe en paralelo. Sobre tu mensaje: …"*.

### 3.5 Contexto del hilo central (roster)

En cada `converse()` se antepone al turno un bloque compacto (≤ 400 tokens) que el modelo **no lee en voz alta**:

```
[Sub-sesiones activas]
#1 revisión de PRs de Felipe — trabajando (4 min) — progreso: "revisando PR #212, 2 de 5"
#2 monitor ping — corriendo (12 min)
#3 logs de Melón — lista, resultado sin escuchar
```

El `SYSTEM_PROMPT` gana una regla: *"Si el usuario pregunta por una sub-sesión, responde con el roster. Si necesitas su detalle, ejecuta `fokkus-jarvis session show <id>`. No inventes estados."*. Con esto, "¿encontró algo grave en las PR?" funciona aunque no coincida con un patrón.

### 3.6 Entrega de resultados por voz

- Se reutiliza `SpeechQueue` + `DeliveryGate` (HU-37). Las entregas llevan el prefijo de la sesión: *"Del hilo de PRs de Felipe: …"*.
- `Question` se entrega con prioridad `TaskResult` y deja la sesión en `AwaitingUser`. Lo siguiente que diga el usuario con referencia a esa sesión, o en foco, se enruta como respuesta.
- `Progress` **no se habla**. Alimenta el roster, el HUD y la respuesta a "¿cómo va…?".
- "Léeme el detalle" lee las primeras secciones de `detail.md` limpias de markdown y ofrece *"¿Sigo?"*. Ver el detalle completo abre el `.md` con `systemd-run --user --collect xdg-open`.

### 3.7 Seguridad y aislamiento

- `SUBAGENT_SYSTEM_PROMPT` hereda **íntegra** la constitución de privacidad del `SYSTEM_PROMPT` (carpetas bloqueadas, secretos), pero **no** la persona de voz ni la instrucción `dev run`.
- **Antirecursión:** `JARVIS_SUBSESSION_ID` en el entorno del subagente hace que `fokkus-jarvis dev run` y `session send` fallen con un mensaje claro. Se agrega un límite duro de profundidad igual a 1.
- **Perfiles:** `ReadOnly` permite comandos de lectura (`gh pr view/diff/list`, `git log/show/diff`, `rg`, `cat` fuera de rutas bloqueadas) y prohíbe escrituras o acciones remotas (`gh pr merge/comment`, `git push`). Las acciones con efecto pasan por `agent/confirm.rs` y se convierten en `PREGUNTA:` al usuario.
- **Contenido no confiable:** se mantiene la regla de no obedecer instrucciones que aparezcan dentro de PRs, correos o logs revisados (prompt injection).
- Se conserva el aislamiento MCP headless (HU-58).

### 3.8 Flujo conversacional de referencia

| # | Usuario dice | Router | Jarvis responde (voz) |
|---|---|---|---|
| 1 | "Jarvis, revísame las PR de Felipe en segundo plano mientras me ayudas a sintetizar un mensaje para Oscar" | compuesto → planner: `spawn` (#1 PRs Felipe, ReadOnly) + `foreground` | "Abrí la revisión de PRs de Felipe en paralelo. Sobre el mensaje para Oscar, ¿qué le quieres decir?" |
| 2 | "Que el deploy queda para mañana en la mañana" | `foreground` (hilo central) | "Te propongo: «don Oscar, el deploy queda para mañana AM, sin falta». ¿Lo dejo así?" |
| 3 | "¿Cómo va lo de las PR?" | `query Status(Alias "pr")` | "La revisión de PRs de Felipe va en la 2 de 5; por ahora nada bloqueante." |
| 4 | "En la revisión de PRs, enfócate en el módulo de pagos" | `route(#1, …)` | "Le pasé la indicación a la revisión de PRs." |
| 5 | "Abre otro hilo para revisar los logs de Melón" | `spawn` (#2) | "Abrí el hilo de logs de Melón." |
| 6 | *(silencio)* | `SessionEvent::Replied(#1)` | "Del hilo de PRs de Felipe: dos PR listas para aprobar; la 214 rompe un test de pagos." |
| 7 | "Entra al hilo de PRs" | `Focus(#1)` | "Estoy en la revisión de PRs." |
| 8 | "Léeme lo de la 214" | foco → `route(#1)` | (responde el agente de #1) |
| 9 | "Vuelve al hilo principal" | `Unfocus` | "Volvimos al hilo principal." |
| 10 | "Cierra el hilo de los logs" | `Close(Alias "logs")` | "Cerré el hilo de logs de Melón." |

---

## 4. Desglose de Historias de Usuario — Épica 14

Convención: **DeepSeek V4.1 Flash** implementa y **Claude Code** valida (`make test`, `make lint`, `make fmt-check`, pruebas dirigidas por voz). Se agrupan en tres fases desplegables de forma independiente.

### Fase A — Estabilizar lo existente (quick wins)

#### HU-59 · Normalización léxica unificada y clasificador robusto
- 📁 **Repositorio:** `Fokkus-Jarvis` — `crates/jarvis-audio/src/text/normalize.rs` (nuevo), `tasks.rs`, `orchestrator.rs`
- 🎯 **Capa:** Backend (NLU heurístico)
- 📌 **Objetivo técnico:** eliminar la doble lógica de comparación (substring vs palabra completa) y que todas las heurísticas de voz operen sobre un texto normalizado único que tolere tildes, enclíticos ("revísame", "analízalo", "échale"), variantes fonéticas de STT ("pe erre", "pull", "PR") y verbos coloquiales ("chequea", "mira", "échale un ojo", "dale una revisión"), sin introducir falsos positivos por substring ("reporte", "blog", "catálogo").
- ✅ **Criterios de aceptación:**
  1. Las 9 frases de la tabla §2.2 se clasifican según la columna "Esperado". En las compuestas, por ahora basta con `Heavy`; la división llega en HU-63.
  2. `task_title("resume el blog de Rust")` ≠ "los logs".
  3. Hay una tabla de pruebas con al menos 40 frases reales (positivas y negativas) y todos los tests existentes siguen en verde.

#### HU-60 · Cerebro de subagente desacoplado de la persona de voz
- 📁 **Repositorio:** `Fokkus-Jarvis` — `brain/cli.rs`, `brain/client.rs`, `jarvis-app/src/settings.rs`, `tasks.rs`
- 🎯 **Capa:** Backend (integración LLM)
- 📌 **Objetivo técnico:** que los subagentes no reciban la persona de voz ("una frase breve", "despacha con dev run"), que tengan un system prompt propio con la constitución de privacidad, que no puedan relanzar tareas recursivamente y que el prompt `ReadOnly` deje explícito que sí puede usar herramientas de lectura (`gh`, `git`) para cumplir.
- ✅ **Criterios de aceptación:**
  1. El prompt que recibe el proceso hijo del subagente no contiene "frase breve" ni "fokkus-jarvis dev run".
  2. `fokkus-jarvis dev run` sale con código ≠ 0 y el mensaje "no permitido dentro de una sub-sesión" cuando existe `JARVIS_SUBSESSION_ID`.
  3. En modo API sin herramientas, la tarea `ReadOnly` que requiera herramientas avisa por voz *"Necesito el modo CLI para revisar repositorios"* en lugar de inventar.

#### HU-61 · Ampliar la red de seguridad del LLM principal
- 📁 **Repositorio:** `Fokkus-Jarvis` — `brain/client.rs` (`SYSTEM_PROMPT`), `jarvis-app/src/main.rs`
- 🎯 **Capa:** Backend (prompting + CLI)
- 📌 **Objetivo técnico:** cuando la heurística no detecta una tarea larga, el LLM principal debe derivarla. Extender la regla de despacho (hoy limitada a "desarrollos") a revisión, análisis e investigación de larga duración, con un subcomando genérico (`fokkus-jarvis task run --profile read_only "<tarea>"`) que no fuerce el prompt de ingeniería.
- ✅ **Criterios de aceptación:**
  1. Con la heurística desactivada en test, "revisa las PR de Felipe" termina en el gestor de tareas por la vía del LLM.
  2. `dev run` sigue funcionando sin regresión.

### Fase B — Sub-sesiones conversacionales

#### HU-62 · `SessionManager` y sub-sesiones persistentes
- 📁 **Repositorio:** `Fokkus-Jarvis` — `crates/jarvis-audio/src/sessions/{mod,store,prompts}.rs` (nuevos); `tasks.rs` pasa a ser una fachada
- 🎯 **Capa:** Backend (concurrencia + persistencia)
- 📌 **Objetivo técnico:** evolucionar `TaskManager` (un disparo) a `SessionManager` (multiturno). Cada sub-sesión tiene id corto, título, alias, perfil, estado (§3.3), transcripción propia, agente reutilizable con rehidratación desde disco, protocolo `PROGRESO/PREGUNTA/RESUMEN_HABLADO/DETALLE` y emisión de `SessionEvent`. Se persiste en `state_dir()/sessions/` y se restaura al reiniciar (las sesiones `Working` quedan `Idle` con la nota "interrumpida por reinicio").
- ✅ **Criterios de aceptación:**
  1. `send(id, texto)` sobre una sesión `Idle` ejecuta un turno nuevo con el historial previo visible para el agente.
  2. Un `send` sobre una sesión `Working` se encola dentro de esa sesión y no se pierde.
  3. Se mantiene el pool configurable con el tope subido a 6, y el estado `Queued` se anuncia una vez.
  4. Persistencia: matar y reiniciar el proceso conserva título, transcripción y estado.
  5. Pruebas con `FakeFactory` para concurrencia, cancelación por sesión, timeout y rehidratación.

#### HU-63 · `TurnRouter` con planificador para instrucciones compuestas
- 📁 **Repositorio:** `Fokkus-Jarvis` — `crates/jarvis-audio/src/router/{mod,planner}.rs` (nuevos), `orchestrator.rs`
- 🎯 **Capa:** Backend (NLU + orquestación)
- 📌 **Objetivo técnico:** reemplazar la bifurcación binaria `Heavy/Light` de `on_turn` por un `TurnPlan` (§3.4) que combine direccionamiento explícito, heurística y un planificador LLM de baja latencia con salida JSON validada. Debe ejecutar el plan en orden (control → spawn → route/query → foreground) y fundir acuse y respuesta en una sola locución.
- ✅ **Criterios de aceptación:**
  1. "Revísame las PR de Felipe en segundo plano mientras me sintetizas un mensaje para Oscar" crea 1 sub-sesión **y** responde en primer plano sobre el mensaje.
  2. Si el planner no responde a tiempo o devuelve JSON inválido, el turno no se pierde: se usa la heurística (HU-59).
  3. En turnos simples la latencia no crece: el planner no se invoca y así lo comprueba un test con contador de llamadas.
  4. La salida del planner nunca se ejecuta como comando. Solo se mapea a acciones tipadas.

#### HU-64 · Direccionamiento verbal, foco y consultas de sub-sesión
- 📁 **Repositorio:** `Fokkus-Jarvis` — `router/mod.rs`, `orchestrator.rs`
- 🎯 **Capa:** Backend (NLU determinista + estado conversacional)
- 📌 **Objetivo técnico:** resolver por voz `SessionRef` (id hablado, ordinal, alias, "la última") y soportar los patrones de §3.4 etapa 2: hablarle a una sesión, entrar o salir de foco, preguntar cómo va, leer el detalle, cerrar o cancelar. Debe pedir desambiguación cuando haya más de una coincidencia y aplicar caducidad de foco.
- ✅ **Criterios de aceptación:**
  1. Los pasos 3, 4, 7, 8, 9 y 10 de la tabla §3.8 pasan como tests de integración del `BrainLoop` con cerebro falso.
  2. "Cancela la de Felipe" encuentra "revisión de PRs de Felipe".
  3. "¿Cómo va?" en foco responde sobre la sesión enfocada; sin foco y con una sola sesión activa, sobre esa; con varias, lista.

#### HU-65 · Roster de sub-sesiones en el contexto del hilo central
- 📁 **Repositorio:** `Fokkus-Jarvis` — `orchestrator.rs` (`converse`), `brain/cli.rs`, `brain/client.rs`
- 🎯 **Capa:** Backend (contexto LLM)
- 📌 **Objetivo técnico:** inyectar en cada turno del cerebro principal un resumen acotado de las sub-sesiones (§3.5), sin que pase al historial persistente ni se lea en voz alta, y exponer `fokkus-jarvis session show <id>` para que el LLM principal pueda "entrar a mirar" el detalle de un hilo.
- ✅ **Criterios de aceptación:**
  1. El roster nunca supera el presupuesto definido (tokens/caracteres) y está verificado con 20 sesiones.
  2. Una pregunta libre ("¿encontró algo grave en las PR?") se responde con datos del `last_summary` o del `progress` de la sesión.

### Fase C — Superficie y operación

#### HU-66 · IPC y CLI de sesiones
- 📁 **Repositorio:** `Fokkus-Jarvis` — `crates/jarvis-ipc/src/{protocol,server,client}.rs`, `crates/jarvis-app/src/main.rs`
- 🎯 **Capa:** Backend (protocolo JSON-RPC + CLI)
- 📌 **Objetivo técnico:** exponer `jarvis.sessions.{list,get,send,cancel,read}` y la notificación `jarvis.sessionEvent`, manteniendo `jarvis.dispatchTask/getTasks/getTask/cancelTask` como alias compatibles. Agregar los subcomandos `fokkus-jarvis session …`.
- ✅ **Criterios de aceptación:**
  1. Tests de *round-trip* del protocolo para cada método.
  2. Los clientes existentes (`dev run`) no cambian.
  3. La documentación de IPC cubre el 100 % de los métodos nuevos (validación código → doc).

#### HU-67 · Visualización de sub-sesiones en HUD y bandeja
- 📁 **Repositorio:** `Fokkus-Jarvis` — `crates/jarvis-render/src/hud.rs`, `crates/jarvis-shell/src/tray.rs`, `crates/jarvis-app/src/presence.rs`
- 🎯 **Capa:** Frontend nativo (render/shell)
- 📌 **Objetivo técnico:** mostrar chips de sub-sesiones (título corto, estado, indicador de "resultado sin escuchar" y de foco) junto a la esfera, que ya tiene el indicador de tareas activas de HU-55. Agregar un submenú en la bandeja para abrir el detalle o cancelar.
- ✅ **Criterios de aceptación:**
  1. Con 0 sesiones no hay cambio visual.
  2. El chip enfocado se distingue.
  3. No hay asignaciones en el hot path del render (se mantiene `tests/no_alloc.rs`).

#### HU-68 · QA de extremo a extremo y métricas de enrutamiento *(Claude)*
- 📁 **Repositorio:** `Fokkus-Jarvis` — `crates/jarvis-audio/src/orchestrator/tests.rs`, `tests/fixtures/`
- 🎯 **Capa:** QA
- 📌 **Objetivo técnico:** construir un corpus de al menos 100 transcripciones reales anotadas (`foreground/spawn/route/query/control`) y un test de regresión del router con umbral mínimo de acierto. Agregar a `VoiceStats` contadores de enrutamiento (`routed_spawn`, `routed_foreground`, `planner_fallbacks`, `planner_latency_ms`) para medir en uso real el "a veces sí, a veces no".
- ✅ **Criterios de aceptación:**
  1. El router alcanza ≥ 95 % de acierto en el corpus.
  2. La latencia p95 del planner es < 2,5 s en modo CLI.
  3. Las métricas son visibles en `jarvis.getState`.

### Dependencias y orden sugerido

```
HU-59 ─┬─► HU-61
HU-60 ─┘
HU-59 + HU-60 ─► HU-62 ─► HU-63 ─► HU-64 ─► HU-65
                     └──► HU-66 ─► HU-67
HU-68 transversal (corpus desde HU-59; umbral exigible desde HU-63)
```

La **Fase A** (HU-59/60/61) corrige el síntoma reportado y puede publicarse como v0.2.30. Las **Fases B y C** constituyen la Épica 14 propiamente dicha (v0.3.0).

---

## 5. Riesgos y decisiones abiertas

| Riesgo | Mitigación |
|---|---|
| Latencia extra del planner LLM en cada turno | Solo se invoca en compuestos o ambiguos (etapa 4) y tiene un timeout duro con *fallback* heurístico. |
| Coste/cuota con muchas sub-sesiones `agy` vivas | Liberar el agente tras inactividad y rehidratar desde el transcript. Tope del pool configurable. |
| Confusión del usuario sobre "dónde está hablando" | Anunciar al entrar o salir de foco, caducidad de 5 minutos, chip visual (HU-67) y prefijo "Del hilo de…" en cada entrega. |
| *Prompt injection* desde PRs/correos revisados | Mantener la regla de datos no confiables, el perfil `ReadOnly` sin acciones remotas y la confirmación (`agent/confirm.rs`) para efectos. |
| Recursión de subagentes | `JARVIS_SUBSESSION_ID` + profundidad máxima 1 (HU-60). |
| Soporte de herramientas en modo API | **Decisión para el PO:** declarar `tools` en `ClaudeClient` (alcance mayor) o restringir las sub-sesiones con herramientas al modo CLI y avisarlo (propuesto en HU-60). |
| ¿Sub-sesión = proceso `agy` persistente o un proceso por turno con transcript? | **Propuesta:** un proceso por turno con transcript inyectado (ya probado en `CliBrain`). Se reevaluará si `agy` ofrece `--resume` estable. |
