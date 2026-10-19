# Contrato SDD — Issue #7: Soporte para Tareas Asíncronas en Segundo Plano y Desacoplamiento del Timeout CLI

**Proyecto:** Fokkus-Jarvis  
**Issue GitHub:** [#7 fix(cli): soporte para tareas asíncronas en segundo plano y desacoplamiento del timeout síncrono de 120s](https://github.com/jucsp/Fokkus-Jarvis/issues/7)  
**Épica Relacionada:** Épica 7 — Agente Visual Multiplataforma y Automatización Generalista / Tareas Pesadas (HU-37)  
**Redactado por:** Antigravity (PO)  
**Revisión y QA:** Claude Code (PM/QA)  
**Implementación Técnica:** DeepSeek (Senior Dev)  
**Fecha:** 2026-10-19  
**Versión Base:** `v0.2.25`  
**Versión Objetivo:** `v0.2.26`  

---

## 1. Contexto de Negocio y Problema Técnico

### 1.1. Contexto de Negocio
Cuando el usuario interactúa por voz con Jarvis y aprueba o solicita el inicio de tareas de desarrollo técnico u orquestaciones complejas del Swarm (ej. con el agente CLI `agy`), estas actividades toman habitualmente de 2 a 15 minutos en completarse (análisis de arquitectura, generación de código, ejecución de pruebas). 

Actualmente, el sistema trata estas instrucciones como turnos conversacionales síncronos regulares. Como consecuencia:
1. El hilo de conversación queda bloqueado por hasta 120 segundos en silencio.
2. Al superar los 120 segundos, la consulta aborta con el mensaje de error vocal:
   ```text
   [stt] escuchado: Perfecto. Me parece correcto. Lánselo no más. Sin problema.
   [cerebro] la respuesta falló: falló el cerebro CLI: `agy` no respondió en 120 s
   ```
3. El usuario pierde el control conversacional y si luego pregunta *"Jarvis, ¿en qué estado va el desarrollo?"*, la pregunta vuelve a enviarse al CLI o falla por timeout sin saber qué ocurrió con el subproceso.

### 1.2. Hallazgo en el Código
1. `classify_workload(query)` en `crates/jarvis-audio/src/tasks.rs` no incluye verbos ni sustantivos asociados a desarrollo de software (`"desarrollo"`, `"desarrollar"`, `"pipeline"`, `"swarm"`, `"construye"`, `"orquesta"`, etc.), ni modismos de aprobación como `"lánselo no más"`, `"lánzalo no más"` o `"procede con eso"`. Por tanto, se evalúan como `Workload::Light` y van al flujo bloqueante `converse()`.
2. `DEFAULT_CLI_TIMEOUT = Duration::from_secs(120)` está cableado rígidamente en `crates/jarvis-audio/src/brain/cli.rs`. En `crates/jarvis-core/src/config.rs` no existe el campo `cli_timeout_secs` para configurarlo en `config.toml`, y `settings::build_brain` en `jarvis-app` instancia `CliBrain::new()` sin configurar el timeout desde la configuración de usuario.
3. Las preguntas de seguimiento sobre el avance del trabajo (*"¿cómo va el desarrollo?"*, *"¿en qué estado va el desarrollo?"*, *"¿cómo va el swarm?"*) no forman parte de `LIST_TASK_PHRASES` en `crates/jarvis-audio/src/orchestrator.rs`, por lo que no consultan el estado del `TaskManager` ni de los subprocesos en segundo plano.

---

## 2. Criterios de Aceptación (Given-When-Then)

### CA-1: Detección y Clasificación Asíncrona de Tareas de Desarrollo (`Workload::Heavy`)
- **Given** una orden del usuario que solicite o apruebe tareas de desarrollo, pipelines o swarm (ej. *"Lanza el desarrollo"*, *"Perfecto, lánselo no más"*, *"Inicia el pipeline de pruebas"*, *"Desarrolla la nueva funcionalidad"*).
- **When** se evalúa la consulta en `classify_workload(&query)`.
- **Then** debe retornar `Workload::Heavy`.

### CA-2: Acuse Inmediato y No Bloqueo Conversacional
- **Given** una tarea clasificada como `Workload::Heavy`.
- **When** el orquestador ejecuta `dispatch_task(&query)`.
- **Then** Jarvis debe:
  1. Registrar la tarea en `TaskManager::submit` en segundo plano con `TaskOrigin::Voice`.
  2. Emitir un acuse de recibo vocal inmediato (< 1 s), por ejemplo: *"Iniciando el desarrollo en segundo plano, te aviso cuando termine."* (o el acuse contextual apropiado).
  3. Quedar inmediatamente en estado `Listening` / `Idle`, sin bloquear futuros turnos de voz.

### CA-3: Timeout de CLI Configurable (`cli_timeout_secs`)
- **Given** el archivo `config.toml` o la estructura `JarvisConfig`.
- **When** se define `cli_timeout_secs = 300` (o se omite, tomando por defecto `120`).
- **Then**:
  1. `JarvisConfig` debe parsear y exponer `cli_timeout_secs: u64` con sus métodos builder `with_cli_timeout_secs` y getter `cli_timeout_secs(&self)`.
  2. `settings::build_brain` debe construir el `CliBrain` configurándole `Duration::from_secs(config.cli_timeout_secs)`.

### CA-4: Consulta Verbal de Estado de Tareas en Curso
- **Given** una o más tareas en segundo plano ejecutándose o completadas.
- **When** el usuario pregunte: *"¿cómo va el desarrollo?"*, *"¿en qué estado va el desarrollo?"*, *"estado del desarrollo"*, *"¿cómo va la tarea?"* o *"¿cómo va el swarm?"*.
- **Then**:
  1. El orquestador debe reconocer la frase dentro de `LIST_TASK_PHRASES` y despachar `LocalCommand::ListTasks`.
  2. Jarvis debe locutar de forma clara el número de tareas activas y su estado sin invocar al cerebro CLI síncrono.

### CA-5: Integridad del Workspace y Notificación al Culminar
- **Given** la finalización de una tarea pesada en segundo plano.
- **When** termine el subproceso.
- **Then**:
  1. El resultado debe persistirse en `~/.local/state/fokkus-jarvis/tasks/<id>.md`.
  2. El resumen hablado debe entregarse a través de `SpeechQueue` en el siguiente hueco de silencio.
  3. Toda la suite de pruebas del proyecto (`make test`, `make lint`, `make fmt-check`) debe pasar al 100% con 0 fallos.

---

## 3. Plan de Implementación por Crate y Rol

### Capa 1: `crates/jarvis-core` [Responsable: DeepSeek Dev]
1. Modificar `JarvisConfig`:
   - Añadir campo público `cli_timeout_secs: u64` con valor por defecto `120`.
   - Implementar `pub fn with_cli_timeout_secs(mut self, secs: u64) -> Self`.
   - Implementar `pub fn cli_timeout_secs(&self) -> u64`.
2. Tests unitarios en `config.rs`:
   - Validar valor por defecto (120 s).
   - Validar roundtrip TOML (`cli_timeout_secs = 240`).

### Capa 2: `crates/jarvis-audio` [Responsable: DeepSeek Dev]
1. En `src/tasks.rs`:
   - Expandir `HEAVY_VERBS` con: `"lanza"`, `"lanzar"`, `"ejecuta"`, `"ejecutar"`, `"inicia"`, `"iniciar"`, `"desarrolla"`, `"desarrollar"`, `"construye"`, `"construir"`, `"implementa"`, `"implementar"`, `"orquesta"`, `"orquestar"`, `"compila"`, `"compilar"`, `"procede"`.
   - Expandir `HEAVY_NOUNS` con: `"desarrollo"`, `"swarm"`, `"pipeline"`, `"tarea"`, `"proyecto"`, `"script"`, `"suite"`, `"build"`, `"agente"`.
   - Expandir `HEAVY_OVERRIDES` con modismos de confirmación y aprobación de lanzamientos:
     - `"lánsalo"`, `"lansalo"`, `"lánselo"`, `"lanselo"`
     - `"lánzalo no más"`, `"lánselo no más"`, `"lansalo no mas"`, `"lanselo no mas"`
     - `"procede con eso"`, `"procede no más"`, `"procede no mas"`
     - `"adelante con el desarrollo"`, `"dale no más"`, `"dale nomas"`
2. En `src/orchestrator.rs`:
   - Expandir `LIST_TASK_PHRASES` con:
     - `"cómo va el desarrollo"`, `"como va el desarrollo"`
     - `"en qué estado va el desarrollo"`, `"en que estado va el desarrollo"`
     - `"estado del desarrollo"`, `"estado de las tareas"`
     - `"cómo va la tarea"`, `"como va la tarea"`
     - `"cómo va el swarm"`, `"como va el swarm"`
   - En `dispatch_task`: personalizar el mensaje de acuse para contexto de desarrollo (ej. si la consulta contiene "desarrollo", "swarm", "lánselo", locutar: *"Iniciando el desarrollo en segundo plano, te aviso cuando termine."*).
3. Tests unitarios en `tasks.rs` y `orchestrator/tests.rs`:
   - Test que verifique que `"Perfecto. Me parece correcto. Lánselo no más. Sin problema."` clasifica como `Workload::Heavy`.
   - Test que verifique que `"¿cómo va el desarrollo?"` se reconoce como `LocalCommand::ListTasks`.

### Capa 3: `crates/jarvis-app` [Responsable: DeepSeek Dev]
1. En `src/settings.rs`:
   - En `build_brain`, pasar el timeout configurado a `CliBrain`:
     ```rust
     CliBrain::new(command)?.with_timeout(Duration::from_secs(config.cli_timeout_secs))
     ```

### Capa 4: Validación, QA y Release [Responsable: Claude PM/QA]
1. Revisar código y diff de DeepSeek.
2. Ejecutar `make fmt-check`, `make lint` y `make test`.
3. Actualizar `Cargo.toml` a versión `0.2.26`.
4. Actualizar `backlog.md` y `sprint_board.md`.
5. Ejecutar commit, etiquetado (`git tag v0.2.26`) y push con autorización del PO.

---

## 4. Revisión QA (Claude PM/QA — 2026-10-19)

**Dictamen:** ⚠️ APROBADO CON CONDICIONES. Las capas 1 y 3 y la parte de `LIST_TASK_PHRASES` pueden pasar a DeepSeek ya. La Capa 2 (clasificación de aprobaciones como "lánselo no más") queda **BLOQUEADA** hasta que el PO resuelva B-1 y B-2.

### 4.1. Bloqueantes de arquitectura (los decide el PO)

- **B-1 — El trabajador de fondo no recibe contexto.** `build_task_brain` (`jarvis-app/src/settings.rs`) crea un `CliBrain` nuevo **sin historial**, y `task_prompt(query)` solo incluye la frase del usuario. Si se despacha *"Perfecto. Me parece correcto. Lánselo no más."*, el subagente recibe solo esa frase y no sabe qué tiene que lanzar. Hay que decidir cómo se le pasa el contexto: los últimos N turnos de `CliBrain::history()` del cerebro conversacional, o el último plan que propuso el asistente.
- **B-2 — Choca con el guardarraíl de HU-37.** `task_prompt` le dice explícitamente al subagente *"Solo lees y resumes: no ejecutes acciones con efecto"*. Una tarea de "desarrollo" (generar código o lanzar `agy`/el Swarm) es una acción con efecto, así que hoy el subagente la rechazaría o la degradaría a un resumen. Para hacerla posible haría falta un `TaskKind::Dev` con su propio prompt y su propia política de permisos. Eso afecta la seguridad (inyección de prompt desde los datos revisados) y tiene que quedar especificado en el SDD.

### 4.2. Correcciones obligatorias al plan

| # | Sección | Problema | Corrección |
|---|---------|----------|------------|
| C-1 | Capa 1 | Pide un "campo público `u64`". La convención de `JarvisConfig` para valores numéricos ajustables es: campo privado `u32` + `#[serde(default = "default_…")]` + getter con saturación + corrección en `sanitize` (ver `tasks_timeout_secs`). | `cli_timeout_secs: u32` privado, `default_cli_timeout_secs() -> 120`, `CLI_TIMEOUT_RANGE = 30..=1800`, getter `cli_timeout_secs(&self) -> u32` con saturación, aviso `[config] … se satura a …`, y tests de valor por defecto, roundtrip y saturación. |
| C-2 | Capa 3 | El fragmento `CliBrain::new(command)?.with_timeout(Duration::from_secs(config.cli_timeout_secs))` no compila: `build_brain` devuelve `Result<Brain, &'static str>` (el `?` no convierte `BrainError`), y el campo no es accesible. | Aplicar `.with_timeout(Duration::from_secs(u64::from(config.cli_timeout_secs())))` dentro del `match CliBrain::new(command)` que ya existe, sin cambiar los avisos actuales. |
| C-3 | Capa 2 / `HEAVY_OVERRIDES` | `"dale no más"`, `"dale nomas"` y `"procede no más"` son afirmaciones genéricas del español chileno. Cualquier "dale no más" en respuesta a una pregunta trivial del LLM se despacharía como tarea de fondo sin contexto. | Quitarlas de la lista, o exigir que el turno anterior del asistente haya propuesto una tarea de desarrollo (depende de B-1). |
| C-4 | Capa 2 / `HEAVY_OVERRIDES` | `classify_workload` no elimina acentos y las STT suelen transcribir "nomás" junto. | Agregar las variantes `"no más"`/`"nomás"`/`"nomas"`, o mejor normalizar con `fold_accents` antes de comparar. |
| C-5 | Capa 2 / `HEAVY_VERBS`+`HEAVY_NOUNS` | `contains_any` compara subcadenas: `"inicia"` coincide con "iniciativa", `"procede"` con "procedencia", `"suite"` con "suite del hotel", `"tarea"`+`"busca"` → "busca la tarea de matemáticas" se vuelve Heavy. | Agregar casos negativos al test de la tabla (`classify_workload_table`) con estas frases y, si fallan, comparar por palabra completa. |
| C-6 | CA-2 / timeout | Los desarrollos duran de 2 a 15 min y `tasks_timeout_secs` tiene 900 s (15 min) por defecto: las tareas largas quedarían en el límite (`TimedOut`). | Documentarlo en el SDD o subir el valor por defecto para las tareas `Dev`. |
| C-7 | Capa 4 | "commit, tag y push con autorización del PO". | Según la regla de protección de git, la autorización explícita la da **el usuario**, no el PO. |

### 4.3. Verificaciones contra el código

- ✅ `classify_workload` no tiene verbos ni sustantivos de desarrollo (`tasks.rs:38-93`).
- ✅ `DEFAULT_CLI_TIMEOUT = 120 s` está fijo en el código (`brain/cli.rs:28`); `CliBrain::with_timeout` ya existe (`cli.rs:106`).
- ✅ `JarvisConfig` no tiene `cli_timeout_secs`; `build_brain` no aplica ningún timeout (`settings.rs:41`).
- ✅ `LIST_TASK_PHRASES` no cubre las preguntas de seguimiento (`orchestrator.rs:541`).
- ✅ Orden de despacho: `local_command` se evalúa antes que `classify_workload` (`orchestrator.rs:2608/2665`). "lanza/ejecuta/inicia + app" sigue abriendo apps y "qué tareas tengo" sigue siendo ListTasks.
- ✅ CA-5.1/5.2 (`tasks/<id>.md` + `SpeechQueue`) ya los implementa HU-37: aquí solo se valida que no haya regresión.

---

## 5. Resolución del Product Owner (PO — Antigravity)

Habiendo revisado las observaciones de arquitectura de Claude PM/QA, se definen las siguientes soluciones definitivas:

### 5.1. Resolución de Bloqueante B-1 (Inyección de Contexto en Tarea de Fondo)
- `BrainLoop` en `crates/jarvis-audio/src/orchestrator.rs` mantendrá un campo `last_assistant_response: Option<String>` actualizado al finalizar cada turno de conversación exitoso.
- Cuando una tarea se despacha a través de `dispatch_task(&mut self, query: &str)` y se reconoce como una tarea de desarrollo o aprobación (`is_dev_task(&query)`), se construirá el prompt mediante `dev_task_prompt(query, self.last_assistant_response.as_deref())`.
- De este modo, ante frases de aprobación como *"Perfecto. Me parece correcto. Lánselo no más."*, el subproceso CLI (`agy`) recibe tanto la aprobación del usuario como el contexto técnico de la propuesta previa que Jarvis le presentó.

### 5.2. Resolución de Bloqueante B-2 (Diferenciación de Permisos de Subagente)
- Se introduce `dev_task_prompt(query: &str, proposal_context: Option<&str>) -> String` en `crates/jarvis-audio/src/tasks.rs`.
- A diferencia de `task_prompt` (que preserva el guardarraíl de lectura estricta para HU-37: correo, triage, slack), `dev_task_prompt` instruye al subagente:
  ```text
  Eres un subagente de ingeniería y desarrollo de Jarvis. Ejecuta la tarea técnica solicitada por el usuario (análisis, modificación de código, ejecución de pruebas o pipelines según corresponda).
  [Contexto de propuesta previa si existe]
  Instrucción del usuario: {query}
  
  Responde exactamente con dos secciones:
  RESUMEN_HABLADO: como mucho 60 palabras, en español, sin markdown, indicando qué se realizó y el resultado.
  DETALLE: el resultado completo, cambios o logs en markdown.
  ```
- Esto mantiene la seguridad: las tareas de lectura (HU-37) permanecen restringidas, mientras que las tareas explícitas de desarrollo técnico tienen permiso para operar con `agy`.

### 5.3. Adopción de Correcciones C-1 a C-7
- **C-1:** En `JarvisConfig` se define `cli_timeout_secs: u32` (privado), `default_cli_timeout_secs() -> 120`, rango `CLI_TIMEOUT_RANGE = 30..=1800`, getter saturado `cli_timeout_secs(&self) -> u32` y verificación en `sanitize()`.
- **C-2:** En `settings::build_brain`, se aplica `.with_timeout(Duration::from_secs(u64::from(config.cli_timeout_secs())))` dentro del bloque `match` existente.
- **C-3 & C-4:** Se descartan `"dale no más"` y `"procede no más"` aislados. Se admiten modismos directos de lanzamiento y aprobación técnica como `"lánsalo"`, `"lansalo"`, `"lánselo"`, `"lanselo"`, `"lánzalo no más"`, `"lánselo no más"`, `"lansalo nomas"`, `"lanselo nomas"`, `"procede con el desarrollo"`, `"adelante con el desarrollo"`, con normalización contra acentos.
- **C-5:** Casos negativos validados en tests para evitar falsos positivos con subcadenas ("iniciativa", "procedencia", "suite de hotel").
- **C-6:** Para tareas `Dev`, el timeout del worker de `TaskManager` se mantiene con el valor de `tasks_timeout_secs` (default 900 s / 15 min), suficiente para ejecuciones de desarrollo.
- **C-7:** La autorización de publicación y release ya fue concedida explícitamente por el usuario en su solicitud: *"Si esta todo ok estan autorizados a publicar en Github un nuevo release con tag"*.

**Con estas directrices, el contrato queda 100% DESBLOQUEADO para ejecución.**


---

## 6. Reporte QA de Implementación (Claude PM/QA — 2026-10-19)

**Dictamen:** ✅ APROBADO PARA RELEASE (condicionado a destrabar el gate `cargo deny` y a la autorización explícita del usuario para commit/tag/push).

### 6.1. Entrega de DeepSeek V4 Pro

| Capa | Archivo | Cambio | Estado |
|------|---------|--------|--------|
| 1 | `jarvis-core/src/config.rs` | `CLI_TIMEOUT_RANGE = 30..=1_800`, campo privado `cli_timeout_secs: u32` (default 120), builder, getter saturado, aviso en `sanitize` y 3 tests (C-1) | ✅ |
| 3 | `jarvis-app/src/settings.rs` | `build_brain` aplica `with_timeout(cli_timeout_secs())` dentro del `match` existente; avisos sin cambios (C-2) | ✅ |
| 2 | `jarvis-audio/src/tasks.rs` | `DEV_VERBS`/`DEV_NOUNS`/`DEV_OVERRIDES` en listas aparte, por palabra completa y sin tildes (C-3, C-4, C-5); `is_dev_task`, `dev_task_prompt` con contexto limitado a los últimos 4000 caracteres (B-2); `task_title` → "el desarrollo"; `task_prompt` de HU-37 sin cambios | ✅ |
| 2 | `jarvis-audio/src/orchestrator.rs` | +10 frases en `LIST_TASK_PHRASES`; `BrainLoop.last_assistant_response` se actualiza en cada turno de `converse` con respuesta no vacía (B-1); `dispatch_task` usa `dev_task_prompt` y el acuse "Iniciando el desarrollo en segundo plano, te aviso cuando termine." | ✅ |
| 2 | `jarvis-audio/src/orchestrator/tests.rs` | Test de las 10 frases → `ListTasks`, y de que las órdenes de desarrollo no abren apps | ✅ |

**Desvío consciente respecto a la Sección 3:** no se ampliaron `HEAVY_VERBS`/`HEAVY_NOUNS`/`HEAVY_OVERRIDES`, porque esas listas comparan subcadenas y "busca" + "tarea" habría marcado "busca la tarea de matemáticas" como Heavy (C-5). El vocabulario de desarrollo va en listas propias que se comparan por palabra completa.

### 6.2. Correcciones aplicadas por QA
- `tasks.rs`: el test `dev_task_prompt_truncates_long_utf8_context_without_panicking` contaba todas las "ñ" del prompt, incluida la de "español" del texto fijo (4001 ≠ 4000). Ahora resta la base sin contexto. El código de producción estaba bien.
- `make fmt`: formato de `DEV_VERBS` y un diff de formato que ya venía de v0.2.25 en `orchestrator/tests.rs:715`.

### 6.3. Resultados

| Verificación | Resultado |
|--------------|-----------|
| `make test` | ✅ 628 passed · 0 failed · 3 ignored |
| `make fmt-check` | ✅ |
| `make lint` → `cargo clippy --workspace --all-targets -D warnings` | ✅ 0 warnings en crates propios |
| `make lint` → `cargo deny check` | ❌ `error[yanked]`: `inout 0.2.3` (cadena `keyring` → `secret-service` → `aes`/`cbc` → `cipher`). Ya existía antes y no tiene relación con el issue. Hay que actualizar el lock (`cargo update -p inout`) en un cambio aparte de cadena de suministro. |
| Versión | ✅ `Cargo.toml` y `fokkus-jarvis.spec` en `0.2.26` (con entrada en `%changelog`) |
| Formularios Python | ✅ `unified_settings.py` vuelve a escribir todas las claves escalares que carga, así que `cli_timeout_secs` se conserva aunque no aparezca en la UI |

### 6.4. Pendientes y riesgos
- 🧪 **Prueba manual por voz:** proponer un plan con Jarvis → "Perfecto, lánselo no más" → acuse inmediato → "¿cómo va el desarrollo?" → resultado entregado por `SpeechQueue`.
- ⚠️ **Seguridad (B-2):** `dev_task_prompt` permite al subagente ejecutar acciones con efecto, y basta una frase de voz para dispararlo. La propuesta previa del asistente puede incluir contenido que vino de datos externos (inyección indirecta). El riesgo lo aceptó el PO; se recomienda una confirmación hablada o una lista blanca de repositorios en una iteración siguiente.
- ℹ️ Si el usuario interrumpe la respuesta (barge-in), `last_assistant_response` conserva la respuesta anterior completa y no la propuesta interrumpida.
- ⏳ **Commit, tag `v0.2.26` y release:** pendientes de autorización explícita del usuario (C-7, regla de protección de git).

---

## 7. Addendum: Fallback de Despacho Desacoplado vía Tool (`fokkus-jarvis dev run`)

### 7.1. Motivación y Alcance
Si el usuario emite una instrucción de desarrollo usando modismos o frases no contempladas en las listas de `is_dev_task` (por ejemplo: *"vamos con esa idea"*, *"arranca la refactorización acordada"*, *"haz los cambios que me propusiste"*), el turno no es interceptado por la heurística y llega al cerebro conversacional (`converse()`).

Para evitar que el agente ejecute la tarea de forma síncrona y sobrepase el timeout del turno conversacional, se introduce un mecanismo de **fallback cognitivo asistido por herramientas**:

1. **Tool / Subcomando CLI (`fokkus-jarvis dev run <tarea...>`):**
   - El binario `fokkus-jarvis` expone `dev run <tarea...>`.
   - Se conecta al servidor IPC local de Jarvis (`~/.fokkus/jarvis.sock`) vía `JarvisClient::connect_default()`.
   - Despacha la tarea mediante `client.dispatch_task("el desarrollo", &dev_prompt)` hacia el `TaskManager` en segundo plano.
   - Imprime la confirmación del despacho por stdout.

2. **Instrucción en el Prompt Base (`SYSTEM_PROMPT`):**
   - Se instruye al cerebro conversacional:
     > *"Pueden existir frases o indicaciones donde el usuario te solicite realizar desarrollos, construir software, ejecutar pipelines, refactorizaciones o tareas técnicas de programación (pueden ser breves o extensas). En dicho caso, NUNCA ejecutes el desarrollo de forma síncrona en este turno conversacional ni bloquees la respuesta: despáchalo de inmediato en segundo plano ejecutando la herramienta: `fokkus-jarvis dev run \"<descripción clara y completa de la tarea a desarrollar>\"`; y responde al usuario confirmando de inmediato en una frase breve que el desarrollo fue iniciado en segundo plano y que le avisarás apenas termine."*

3. **Cierre del Turno Conversacional:**
   - Al ejecutar `fokkus-jarvis dev run "..."`, el subproceso CLI o la tool delega la tarea a `TaskManager`.
   - El turno conversacional termina en 2–3 segundos confirmando al usuario verbalmente.
   - `TaskManager` ejecuta el desarrollo de fondo en su worker desacoplado (con límite de 15 min y permisos plenos de `dev_task_prompt`), notificando el resultado al concluir vía `SpeechQueue`.


## 8. Reporte QA del Addendum (Claude PM/QA — 2026-10-19)

**Implementación:** DeepSeek V4 Pro (`--model pro`), con corrección menor de QA.

| Punto | Estado |
|---|---|
| `fokkus-jarvis dev run <tarea...>` (`CliAction::DevRun`, `parse_dev`, `run_dev_task`, ayuda) | ✅ |
| Despacho vía `IpcClient::connect_default()` + `dispatch_task(DEV_TASK_TITLE, tarea)` | ✅ |
| `SYSTEM_PROMPT` instruye despacho en segundo plano con `fokkus-jarvis dev run` | ✅ |
| Re-export de `dev_task_prompt`, `task_title` y `DEV_TASK_TITLE` en `jarvis-audio/src/lib.rs` | ✅ |
| Tests unitarios (parseo, enrutamiento de prompt, SYSTEM_PROMPT, título) | ✅ 5 nuevos/ajustados |

**Hallazgo QA corregido en el briefing (bloqueante):** `VoiceTaskPort::dispatch` envolvía todo prompt IPC con `task_prompt` (solo lectura: "no ejecutes acciones con efecto"), así que un `dev run` nunca habría desarrollado nada. Se agregó `ipc_task_prompt()`: si el título es `DEV_TASK_TITLE` ("el desarrollo") usa `dev_task_prompt`, si no conserva `task_prompt`. El CLI envía la tarea cruda para no envolverla dos veces. No cambian el trait `TaskPort` ni el protocolo IPC.

**Corrección QA directa:** la entrega de DeepSeek no compilaba (E0382, `title` movido antes de `ipc_task_prompt(&title, …)`); también se corrigió el formato con `make fmt`.

**Resultados:** `make test` ✅ 632 pass / 0 fail / 3 ignored · `make lint` ✅ (cargo deny ok; clippy `-D warnings` ok, solo avisos del crate externo `whisper-rs-sys`) · `make fmt-check` ✅.

**Smoke CLI:** `dev run` sin tarea → `uso: dev run <tarea>` (exit 2) ✅ · con Jarvis apagado → "No se pudo conectar con Jarvis (¿está corriendo?)" (exit 1) ✅.

**Pendiente:** E2E con Jarvis corriendo (frase no heurística → el LLM ejecuta `dev run` → `TaskManager` → aviso por `SpeechQueue`). Queda abierto que el LLM obedezca la instrucción: depende del modelo y no se cubre con tests unitarios.
