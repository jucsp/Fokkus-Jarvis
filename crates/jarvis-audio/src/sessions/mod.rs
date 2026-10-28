//! Sub-sesiones conversacionales persistentes (HU-62).
//!
//! Una **sub-sesión** es un hilo de trabajo con identidad propia —id corto,
//! título, alias, perfil, estado e historial— que corre en paralelo al hilo
//! central de voz y **sobrevive a un reinicio** del proceso.
//!
//! El ciclo de vida típico es multiturno, no de un solo disparo:
//!
//! 1. [`SessionManager::spawn`] abre la sub-sesión con su primera instrucción.
//! 2. El agente trabaja ([`SessionStatus::Working`]) y entrega
//!    `PROGRESO:`, `RESUMEN_HABLADO:` y `DETALLE:` (o `PREGUNTA:` si necesita
//!    una decisión del usuario).
//! 3. Al terminar queda [`SessionStatus::Idle`] —**no** `Done`— y acepta más
//!    instrucciones con [`SessionManager::send`], que inyecta el historial
//!    previo en el *prompt* del turno nuevo (multi-turno sin `--resume`).
//! 4. Solo [`SessionManager::close`] la da por terminada
//!    ([`SessionStatus::Done`]) y [`SessionManager::cancel`] la detiene
//!    ([`SessionStatus::Cancelled`]).
//!
//! La concurrencia es un *pool* configurable (6 por defecto): una sub-sesión que
//! no encuentra hueco queda [`SessionStatus::Queued`] y se anuncia una vez.
//! Dentro de una misma sub-sesión los turnos son **secuenciales** y no se
//! pierden: lo que llega mientras trabaja se encola en su propia cola.
//!
//! Todo se persiste en [`SessionStore`]: `<root>/<id>/{meta.json,
//! transcript.jsonl,detail.md}` con permisos `0700`/`0600`.

pub mod prompts;
pub mod store;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use serde::{Deserialize, Serialize};

use jarvis_core::state_dir;

use crate::brain::{BrainError, CancellationToken};
use crate::conversation::TokenStream;
use crate::speech_queue::{Delivery, SpeechPriority};
use crate::tasks::{
    is_timeout, requires_repo_tools, summarize, BrainFactory, SpeechQueueHandle, TaskOrigin,
    REPO_TOOLS_NOTICE,
};
use crate::tts::strip_directives;

pub use prompts::{role_prompt, system_prompt, turn_prompt};
pub use store::{SessionMeta, SessionStore, DETAIL_FILE, META_FILE, TRANSCRIPT_FILE};

/// Tope del pool de sub-sesiones concurrentes (HU-62, CA-3).
pub const SESSION_POOL_LIMIT: usize = 6;

/// Nota que se añade al historial de una sub-sesión que quedó a medias cuando
/// el proceso se cerró de golpe.
pub const INTERRUPTED_NOTE: &str = "[Interrumpida por reinicio del sistema]";

/// Tope de caracteres de un turno guardado en la transcripción.
const TRANSCRIPT_TURN_MAX_CHARS: usize = 8_000;

/// Retención del registro de eventos en memoria.
const EVENTS_CAPACITY: usize = 256;

/// Tope de una línea mientras se busca el marcador `PROGRESO:`.
const SCAN_LINE_MAX_CHARS: usize = 4_096;

/// Marcadores del protocolo de salida del subagente.
const PROGRESS_MARKER: &str = "PROGRESO:";
const QUESTION_MARKER: &str = "PREGUNTA:";
const SUMMARY_MARKER: &str = "RESUMEN_HABLADO:";
const DETAIL_MARKER: &str = "DETALLE:";

/// Identificador corto y pronunciable de una sub-sesión ("hilo 3").
pub type SessionId = u32;

/// Directorio de las sub-sesiones en el estado del usuario
/// (`state_dir()/sessions`).
pub fn sessions_dir() -> PathBuf {
    state_dir().join("sessions")
}

/// Estado del ciclo de vida de una sub-sesión.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Esperando un hueco del *pool*.
    Queued,
    /// El agente está procesando un turno.
    Working,
    /// Terminó su último turno y acepta instrucciones nuevas.
    Idle,
    /// El agente pidió una decisión al usuario.
    AwaitingUser,
    /// Cerrada por el usuario.
    Done,
    Failed,
    Cancelled,
    TimedOut,
}

impl SessionStatus {
    /// `true` si la sub-sesión está en cola o trabajando.
    pub fn is_active(&self) -> bool {
        matches!(self, SessionStatus::Queued | SessionStatus::Working)
    }

    /// `true` si el estado admite instrucciones nuevas.
    ///
    /// Solo [`SessionStatus::Done`] cierra la sub-sesión: una sesión cancelada,
    /// fallida o interrumpida se puede retomar.
    pub fn accepts_instructions(&self) -> bool {
        !matches!(self, SessionStatus::Done)
    }
}

/// Perfil del agente de la sub-sesión.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionProfile {
    /// Leer, analizar y resumir (PRs, logs, documentos).
    ReadOnly,
    /// Procesos del sistema cancelables (monitores, pings, scripts).
    Operator,
    /// Código, pruebas y pipelines.
    Developer,
}

/// Autor de un turno del historial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRole {
    User,
    Agent,
    System,
}

/// Un turno del historial propio de la sub-sesión.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTurn {
    pub role: TurnRole,
    pub text: String,
    pub at_ms: u64,
}

/// Sub-sesión: hilo de trabajo conversacional con estado propio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubSession {
    pub id: SessionId,
    /// Título corto y hablable ("revisión de PRs de Felipe").
    pub title: String,
    /// Alias para el direccionamiento por voz ("prs", "felipe").
    #[serde(default)]
    pub aliases: Vec<String>,
    pub profile: SessionProfile,
    pub status: SessionStatus,
    /// De dónde salió la sub-sesión.
    pub origin: TaskOrigin,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Historial propio (también en `transcript.jsonl`).
    #[serde(default)]
    pub transcript: Vec<SessionTurn>,
    /// Último `RESUMEN_HABLADO`.
    #[serde(default)]
    pub last_summary: Option<String>,
    /// Última línea de `PROGRESO:` (alimenta el HUD, no se lee).
    #[serde(default)]
    pub progress: Option<String>,
    /// Pregunta pendiente de respuesta del usuario.
    #[serde(default)]
    pub pending_question: Option<String>,
    /// Detalle en markdown del último turno.
    #[serde(default)]
    pub detail_path: Option<PathBuf>,
    /// Resultado que el usuario todavía no ha escuchado.
    #[serde(default)]
    pub unread: bool,
}

impl SubSession {
    /// `true` si hay un resultado sin escuchar.
    pub fn has_news(&self) -> bool {
        self.unread && (self.last_summary.is_some() || self.pending_question.is_some())
    }
}

/// Evento observable de una sub-sesión.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SessionEvent {
    Created {
        id: SessionId,
        title: String,
    },
    Started {
        id: SessionId,
    },
    /// Avance del agente: alimenta el estado y el HUD, **no** se lee por voz.
    Progress {
        id: SessionId,
        text: String,
    },
    /// Turno terminado: hay resumen hablado y, si acaso, detalle en disco.
    Replied {
        id: SessionId,
        summary: String,
        detail_path: Option<PathBuf>,
    },
    /// El agente pide una decisión: se habla con prioridad de resultado.
    Question {
        id: SessionId,
        text: String,
    },
    Finished {
        id: SessionId,
        status: SessionStatus,
    },
}

/// Error de la API pública de [`SessionManager`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// No existe esa sub-sesión.
    NotFound(SessionId),
    /// La sub-sesión se cerró (`Done`) y no acepta más trabajo.
    Closed(SessionId),
    /// La instrucción llegó vacía.
    EmptyInstruction,
    /// No se pudo leer o escribir en disco.
    Store(String),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::NotFound(id) => write!(f, "no existe la sub-sesión {id}"),
            SessionError::Closed(id) => write!(f, "la sub-sesión {id} está cerrada"),
            SessionError::EmptyInstruction => write!(f, "la instrucción está vacía"),
            SessionError::Store(e) => write!(f, "falló el almacén de sub-sesiones: {e}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// Configuración del gestor de sub-sesiones.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Tope de sub-sesiones trabajando a la vez (se recorta a
    /// [`SESSION_POOL_LIMIT`]).
    pub max_parallel: usize,
    /// Tiempo máximo de un turno.
    pub timeout: Duration,
    /// Hueco de silencio antes de entregar resultados por voz.
    pub quiet_gap: Duration,
    /// Raíz del almacén (`state_dir()/sessions` por defecto).
    pub dir: PathBuf,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_parallel: SESSION_POOL_LIMIT,
            timeout: Duration::from_secs(900),
            quiet_gap: Duration::from_millis(1_500),
            dir: sessions_dir(),
        }
    }
}

/// Estado cancelable y cola de turnos de una sub-sesión.
struct SessionRuntime {
    /// Se reinicia al empezar cada turno y lo cancela `cancel`/`close`.
    token: CancellationToken,
    /// Cerebro del turno en curso (para poder cancelarlo desde fuera).
    brain: Mutex<Option<Arc<dyn TokenStream + Send + Sync>>>,
    /// Instrucciones pendientes cuando la sub-sesión ya está trabajando.
    queue: Mutex<VecDeque<String>>,
    /// `true` mientras hay un trabajador atendiendo esta sub-sesión.
    busy: AtomicBool,
    /// `true` si ya se anunció que espera un hueco del *pool* (CA-3).
    queued_announced: AtomicBool,
}

impl SessionRuntime {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            brain: Mutex::new(None),
            queue: Mutex::new(VecDeque::new()),
            busy: AtomicBool::new(false),
            queued_announced: AtomicBool::new(false),
        }
    }

    /// Suelta el cerebro del turno en curso.
    fn clear_brain(&self) {
        *lock(&self.brain) = None;
    }

    /// Corta el turno en curso y descarta lo que estuviera en cola.
    fn abort(&self) {
        self.token.cancel();
        if let Some(brain) = lock(&self.brain).as_ref() {
            brain.cancel();
        }
        lock(&self.queue).clear();
    }
}

/// Devuelve el hueco del *pool* al terminar el turno (o al abortar).
struct SlotGuard {
    tx: Sender<()>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(());
    }
}

struct Inner {
    registry: Mutex<HashMap<SessionId, SubSession>>,
    runtimes: Mutex<HashMap<SessionId, Arc<SessionRuntime>>>,
    next_id: AtomicU64,
    slots_tx: Sender<()>,
    slots_rx: Receiver<()>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    factory: Arc<dyn BrainFactory>,
    store: SessionStore,
    out: SpeechQueueHandle,
    cfg: SessionConfig,
    events: Mutex<Vec<SessionEvent>>,
}

/// Gestor de sub-sesiones conversacionales persistentes.
pub struct SessionManager {
    inner: Arc<Inner>,
}

impl Clone for SessionManager {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl SessionManager {
    /// Abre el gestor, rehidrata las sub-sesiones guardadas y levanta el *pool*.
    ///
    /// Las que quedaron `Queued`/`Working` por un cierre abrupto se rehidratan
    /// como `Idle` con [`INTERRUPTED_NOTE`] en su historial (CA-4).
    pub fn start(
        factory: Arc<dyn BrainFactory>,
        cfg: SessionConfig,
        out: SpeechQueueHandle,
    ) -> io::Result<Self> {
        let store = SessionStore::new(cfg.dir.clone());
        store.create_root()?;

        let max = cfg.max_parallel.clamp(1, SESSION_POOL_LIMIT);
        let (slots_tx, slots_rx) = crossbeam_channel::bounded::<()>(max);
        for _ in 0..max {
            let _ = slots_tx.send(());
        }

        let mut registry: HashMap<SessionId, SubSession> = HashMap::new();
        let mut highest_id = 0_u64;
        for mut session in store.load_all()? {
            if session.status.is_active() {
                session.status = SessionStatus::Idle;
                session.progress = None;
                session.updated_at_ms = now_ms();
                let note = SessionTurn {
                    role: TurnRole::System,
                    text: INTERRUPTED_NOTE.to_string(),
                    at_ms: now_ms(),
                };
                session.transcript.push(note.clone());
                if let Err(e) = store.append_turn(session.id, &note) {
                    eprintln!(
                        "[sesiones] #{}: no se pudo anotar la interrupción: {e}",
                        session.id
                    );
                }
                if let Err(e) = store.write_meta(&session) {
                    eprintln!(
                        "[sesiones] #{}: no se pudo guardar meta.json: {e}",
                        session.id
                    );
                }
            }
            highest_id = highest_id.max(u64::from(session.id));
            registry.insert(session.id, session);
        }

        Ok(Self {
            inner: Arc::new(Inner {
                registry: Mutex::new(registry),
                runtimes: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(highest_id),
                slots_tx,
                slots_rx,
                workers: Mutex::new(Vec::new()),
                factory,
                store,
                out,
                cfg,
                events: Mutex::new(Vec::new()),
            }),
        })
    }

    /// Abre una sub-sesión y encola su primera instrucción.
    ///
    /// Si `initial_instruction` viene vacía la sub-sesión nace `Idle`, lista
    /// para recibir instrucciones con [`send`](Self::send).
    pub fn spawn(
        &self,
        title: String,
        aliases: Vec<String>,
        profile: SessionProfile,
        origin: TaskOrigin,
        initial_instruction: String,
    ) -> SessionId {
        let next = self.inner.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let id = SessionId::try_from(next).unwrap_or(SessionId::MAX);
        let now = now_ms();
        let starts_idle = initial_instruction.trim().is_empty();
        let session = SubSession {
            id,
            title: title.clone(),
            aliases,
            profile,
            status: if starts_idle {
                SessionStatus::Idle
            } else {
                SessionStatus::Queued
            },
            origin,
            created_at_ms: now,
            updated_at_ms: now,
            transcript: Vec::new(),
            last_summary: None,
            progress: None,
            pending_question: None,
            detail_path: None,
            unread: false,
        };
        if let Err(e) = self.inner.store.write_meta(&session) {
            eprintln!("[sesiones] #{id}: no se pudo guardar meta.json: {e}");
        }
        lock(&self.inner.registry).insert(id, session);
        self.runtime(id);
        emit(
            &self.inner,
            SessionEvent::Created {
                id,
                title: title.clone(),
            },
        );
        if !starts_idle {
            if let Err(e) = self.send(id, initial_instruction) {
                eprintln!("[sesiones] #{id}: no se pudo arrancar el primer turno: {e}");
            }
        }
        id
    }

    /// Envía una instrucción a la sub-sesión.
    ///
    /// - `Idle` / `AwaitingUser` / cancelada / fallida: arranca un turno nuevo.
    /// - `Working` / `Queued`: la encola en la cola de esa sub-sesión y se
    ///   procesa en cuanto termine el turno en curso (CA-2).
    pub fn send(&self, id: SessionId, instruction: String) -> Result<(), SessionError> {
        let text = instruction.trim().to_string();
        if text.is_empty() {
            return Err(SessionError::EmptyInstruction);
        }
        let status = self
            .get(id)
            .map(|session| session.status)
            .ok_or(SessionError::NotFound(id))?;
        if !status.accepts_instructions() {
            return Err(SessionError::Closed(id));
        }
        let runtime = self.runtime(id).ok_or(SessionError::NotFound(id))?;
        update(&self.inner, id, |session| {
            // Una instrucción nueva responde la pregunta pendiente y, si la
            // sub-sesión estaba parada, la pone en cola.
            session.pending_question = None;
            if !session.status.is_active() {
                session.status = SessionStatus::Queued;
            }
        });
        lock(&runtime.queue).push_back(text);
        self.pump(id, runtime);
        Ok(())
    }

    /// Cancela el turno en curso (y lo que estuviera en cola).
    ///
    /// Devuelve `false` si la sub-sesión no existe.
    pub fn cancel(&self, id: SessionId) -> bool {
        let Some(runtime) = self.runtime(id) else {
            return false;
        };
        runtime.abort();
        update(&self.inner, id, |session| {
            session.status = SessionStatus::Cancelled;
            session.progress = None;
            session.pending_question = None;
        });
        emit(
            &self.inner,
            SessionEvent::Finished {
                id,
                status: SessionStatus::Cancelled,
            },
        );
        true
    }

    /// Cierra la sub-sesión: no acepta más instrucciones.
    ///
    /// Devuelve `false` si la sub-sesión no existe.
    pub fn close(&self, id: SessionId) -> bool {
        let Some(runtime) = self.runtime(id) else {
            return false;
        };
        runtime.abort();
        update(&self.inner, id, |session| {
            session.status = SessionStatus::Done;
            session.progress = None;
            session.pending_question = None;
        });
        emit(
            &self.inner,
            SessionEvent::Finished {
                id,
                status: SessionStatus::Done,
            },
        );
        true
    }

    /// Una sub-sesión por id.
    pub fn get(&self, id: SessionId) -> Option<SubSession> {
        snapshot(&self.inner, id)
    }

    /// Todas las sub-sesiones, ordenadas por id.
    pub fn list(&self) -> Vec<SubSession> {
        let mut sessions: Vec<SubSession> = lock(&self.inner.registry).values().cloned().collect();
        sessions.sort_by_key(|session| session.id);
        sessions
    }

    /// Cuántas sub-sesiones están en cola o trabajando.
    pub fn active_count(&self) -> usize {
        lock(&self.inner.registry)
            .values()
            .filter(|session| session.status.is_active())
            .count()
    }

    /// Retira y devuelve los eventos acumulados.
    pub fn drain_events(&self) -> Vec<SessionEvent> {
        std::mem::take(&mut *lock(&self.inner.events))
    }

    /// Detiene todas las sub-sesiones y espera a sus trabajadores.
    ///
    /// No cambia los estados guardados: el proceso está saliendo y al volver
    /// la rehidratación los dejará `Idle` con su nota de interrupción.
    pub fn shutdown(&self, deadline: Duration) {
        for runtime in lock(&self.inner.runtimes).values() {
            runtime.abort();
        }
        let mut pending = std::mem::take(&mut *lock(&self.inner.workers));
        let limit = Instant::now() + deadline;
        while !pending.is_empty() && Instant::now() < limit {
            let (done, busy): (Vec<_>, Vec<_>) = pending.into_iter().partition(|j| j.is_finished());
            for join in done {
                let _ = join.join();
            }
            pending = busy;
            if !pending.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// Devuelve (creando si hace falta) el estado en memoria de una sub-sesión.
    fn runtime(&self, id: SessionId) -> Option<Arc<SessionRuntime>> {
        {
            let runtimes = lock(&self.inner.runtimes);
            if let Some(runtime) = runtimes.get(&id) {
                return Some(Arc::clone(runtime));
            }
        }
        if !lock(&self.inner.registry).contains_key(&id) {
            return None;
        }
        let mut runtimes = lock(&self.inner.runtimes);
        let runtime = Arc::new(SessionRuntime::new());
        Some(Arc::clone(runtimes.entry(id).or_insert(runtime)))
    }

    /// Lanza (si no hay ya uno) el trabajador que atiende la cola de la
    /// sub-sesión.
    fn pump(&self, id: SessionId, runtime: Arc<SessionRuntime>) {
        if runtime.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let thread_runtime = Arc::clone(&runtime);
        let spawned = std::thread::Builder::new()
            .name(format!("jarvis-session-{id}"))
            .spawn(move || session_loop(&inner, id, &thread_runtime));
        match spawned {
            Ok(join) => {
                let mut workers = lock(&self.inner.workers);
                workers.retain(|j| !j.is_finished());
                workers.push(join);
            }
            Err(e) => {
                eprintln!("[sesiones] #{id}: no se pudo crear el hilo: {e}");
                runtime.busy.store(false, Ordering::SeqCst);
                update(&self.inner, id, |session| {
                    session.status = SessionStatus::Failed
                });
                emit(
                    &self.inner,
                    SessionEvent::Finished {
                        id,
                        status: SessionStatus::Failed,
                    },
                );
            }
        }
    }
}

/// Atiende la cola de una sub-sesión: un turno detrás de otro, sin perder nada.
fn session_loop(inner: &Arc<Inner>, id: SessionId, runtime: &Arc<SessionRuntime>) {
    loop {
        let next = lock(&runtime.queue).pop_front();
        let Some(instruction) = next else {
            runtime.busy.store(false, Ordering::SeqCst);
            // Carrera con `send`: si entró algo entre el `pop_front` y el
            // `store`, lo atendemos aquí mismo en vez de dejarlo colgado.
            if lock(&runtime.queue).is_empty() || runtime.busy.swap(true, Ordering::SeqCst) {
                return;
            }
            continue;
        };
        run_turn(inner, id, runtime, &instruction);
    }
}

/// Ejecuta un turno completo de la sub-sesión.
fn run_turn(inner: &Arc<Inner>, id: SessionId, runtime: &Arc<SessionRuntime>, instruction: &str) {
    let Some(_slot) = acquire_slot(inner, id, runtime) else {
        fail_turn(
            inner,
            id,
            "No hay hueco en el pool de sub-sesiones.",
            "el pool de sub-sesiones se cerró".to_string(),
        );
        return;
    };
    let Some(session) = snapshot(inner, id) else {
        return;
    };

    runtime.queued_announced.store(false, Ordering::SeqCst);
    runtime.token.reset();
    runtime.clear_brain();
    update(inner, id, |session| {
        session.status = SessionStatus::Working;
        session.progress = None;
    });
    emit(inner, SessionEvent::Started { id });

    // El historial previo se captura **antes** de añadir el turno nuevo: es lo
    // que se inyecta en el prompt (CA-1).
    let history = session.transcript;
    append_turn(
        inner,
        id,
        SessionTurn {
            role: TurnRole::User,
            text: instruction.to_string(),
            at_ms: now_ms(),
        },
    );

    let title = session.title;
    let profile = session.profile;
    let brain = match inner.factory.spawn_worker(profile, inner.cfg.timeout) {
        Ok(brain) => brain,
        Err(e) => {
            fail_turn(
                inner,
                id,
                "No tengo cerebro para esta sub-sesión.",
                format!("{e}"),
            );
            return;
        }
    };
    *lock(&runtime.brain) = Some(Arc::clone(&brain));

    if runtime.token.is_cancelled() {
        runtime.clear_brain();
        update(inner, id, |session| {
            session.status = SessionStatus::Cancelled;
        });
        emit(
            inner,
            SessionEvent::Finished {
                id,
                status: SessionStatus::Cancelled,
            },
        );
        return;
    }

    // HU-60 CA-3: sin herramientas de repositorio, una tarea que las necesita
    // avisa en vez de inventar el resultado.
    if requires_repo_tools(instruction) && !brain.has_repo_tools() {
        runtime.clear_brain();
        fail_turn(
            inner,
            id,
            REPO_TOOLS_NOTICE,
            REPO_TOOLS_NOTICE.trim_end_matches('.').to_string(),
        );
        return;
    }

    let prompt = turn_prompt(profile, &history, instruction);
    let mut text = String::new();
    let mut scanner = ProgressScanner::default();
    let mut lines: Vec<String> = Vec::new();
    let result = brain.stream_tokens(&prompt, &mut |token| {
        text.push_str(token);
        scanner.feed(token, &mut lines);
        for line in lines.drain(..) {
            record_progress(inner, id, line);
        }
    });
    scanner.flush(&mut lines);
    for line in lines.drain(..) {
        record_progress(inner, id, line);
    }
    runtime.clear_brain();

    match result {
        Ok(completion) => {
            // Con CLI el texto llega por `on_token`; con la API, en `text`.
            let raw = if completion.text.trim().is_empty() {
                text.as_str()
            } else {
                completion.text.as_str()
            };
            let full = strip_directives(raw).trim().to_string();
            if full.is_empty() {
                fail_turn(
                    inner,
                    id,
                    "El hilo no devolvió nada.",
                    "el cerebro devolvió una respuesta vacía".to_string(),
                );
            } else {
                append_turn(
                    inner,
                    id,
                    SessionTurn {
                        role: TurnRole::Agent,
                        text: full.clone(),
                        at_ms: now_ms(),
                    },
                );
                finish_turn(inner, id, &title, &full);
            }
        }
        Err(BrainError::Cancelled) => {
            update(inner, id, |session| {
                session.status = SessionStatus::Cancelled;
                session.progress = None;
            });
            emit(
                inner,
                SessionEvent::Finished {
                    id,
                    status: SessionStatus::Cancelled,
                },
            );
        }
        Err(e) if is_timeout(&e) => {
            let spoken = format!("El hilo {title} tardó demasiado y lo detuve.");
            fail_turn_with(inner, id, SessionStatus::TimedOut, spoken, format!("{e}"));
        }
        Err(e) => {
            eprintln!("[sesiones] #{id} falló: {e}");
            let spoken = format!("El hilo {title} falló.");
            fail_turn_with(inner, id, SessionStatus::Failed, spoken, format!("{e}"));
        }
    }
}

/// Toma un hueco del *pool*; mientras no haya, la sub-sesión queda `Queued` y
/// se anuncia una sola vez (CA-3).
fn acquire_slot(inner: &Arc<Inner>, id: SessionId, runtime: &SessionRuntime) -> Option<SlotGuard> {
    match inner.slots_rx.try_recv() {
        Ok(()) => Some(SlotGuard {
            tx: inner.slots_tx.clone(),
        }),
        Err(TryRecvError::Empty) => {
            update(inner, id, |session| session.status = SessionStatus::Queued);
            if !runtime.queued_announced.swap(true, Ordering::SeqCst) {
                let title = snapshot(inner, id)
                    .map(|session| session.title)
                    .unwrap_or_else(|| format!("el hilo {id}"));
                let max = inner.cfg.max_parallel.clamp(1, SESSION_POOL_LIMIT);
                speak(
                    inner,
                    id,
                    format!(
                        "{title} queda en espera: ya hay {max} sub-sesiones trabajando a la vez."
                    ),
                );
            }
            inner.slots_rx.recv().ok().map(|()| SlotGuard {
                tx: inner.slots_tx.clone(),
            })
        }
        Err(TryRecvError::Disconnected) => None,
    }
}

/// Cierra un turno con éxito: guarda el detalle y deja la sesión `Idle` (o
/// `AwaitingUser` si el agente preguntó algo).
///
/// Conserva el último `PROGRESO:` (se limpia al arrancar el turno siguiente)
/// para que el hilo central pueda responder con él (HU-63, CA-2).
fn finish_turn(inner: &Arc<Inner>, id: SessionId, title: &str, text: &str) {
    let parsed = parse_output(text);
    let detail_path = if parsed.detail.is_empty() {
        None
    } else {
        match inner.store.write_detail(id, &parsed.detail) {
            Ok(path) => Some(path),
            Err(e) => {
                eprintln!("[sesiones] #{id}: no se pudo guardar el detalle: {e}");
                None
            }
        }
    };
    let summary = parsed.summary;
    match parsed.question {
        Some(question) => {
            let spoken = question_notice(title, &question);
            update(inner, id, |session| {
                session.status = SessionStatus::AwaitingUser;
                session.pending_question = Some(question.clone());
                session.unread = true;
                session.last_summary = Some(summary.clone());
                if let Some(path) = &detail_path {
                    session.detail_path = Some(path.clone());
                }
            });
            emit(inner, SessionEvent::Question { id, text: question });
            speak(inner, id, spoken);
        }
        None => {
            let spoken = format!("Sobre {title}: {summary}");
            update(inner, id, |session| {
                session.status = SessionStatus::Idle;
                session.pending_question = None;
                session.unread = true;
                session.last_summary = Some(summary.clone());
                if let Some(path) = &detail_path {
                    session.detail_path = Some(path.clone());
                }
            });
            emit(
                inner,
                SessionEvent::Replied {
                    id,
                    summary,
                    detail_path,
                },
            );
            speak(inner, id, spoken);
        }
    }
}

/// Cierra un turno fallido con estado `Failed`.
fn fail_turn(inner: &Arc<Inner>, id: SessionId, spoken: &str, reason: String) {
    fail_turn_with(inner, id, SessionStatus::Failed, spoken.to_string(), reason);
}

/// Cierra un turno fallido con el estado indicado y un aviso hablado.
fn fail_turn_with(
    inner: &Arc<Inner>,
    id: SessionId,
    status: SessionStatus,
    spoken: String,
    reason: String,
) {
    append_turn(
        inner,
        id,
        SessionTurn {
            role: TurnRole::System,
            text: reason,
            at_ms: now_ms(),
        },
    );
    update(inner, id, |session| {
        session.status = status;
        session.progress = None;
        session.unread = true;
    });
    emit(inner, SessionEvent::Finished { id, status });
    speak(inner, id, spoken);
}

/// Frase con la que se pide una decisión al usuario.
fn question_notice(title: &str, question: &str) -> String {
    format!("En {title} necesito una decisión: {question}")
}

/// Añade un turno al historial en memoria y al `transcript.jsonl`.
fn append_turn(inner: &Arc<Inner>, id: SessionId, mut turn: SessionTurn) {
    turn.text = truncate_chars(&turn.text, TRANSCRIPT_TURN_MAX_CHARS);
    let stored = {
        let mut registry = lock(&inner.registry);
        match registry.get_mut(&id) {
            Some(session) => {
                session.transcript.push(turn.clone());
                session.updated_at_ms = now_ms();
                true
            }
            None => false,
        }
    };
    if !stored {
        return;
    }
    if let Err(e) = inner.store.append_turn(id, &turn) {
        eprintln!("[sesiones] #{id}: no se pudo guardar el turno: {e}");
    }
}

/// Registra un avance: estado e HUD, sin locución.
fn record_progress(inner: &Arc<Inner>, id: SessionId, text: String) {
    update(inner, id, |session| session.progress = Some(text.clone()));
    emit(inner, SessionEvent::Progress { id, text });
}

/// Aplica un cambio al estado de la sub-sesión y lo persiste.
fn update(inner: &Arc<Inner>, id: SessionId, change: impl FnOnce(&mut SubSession)) {
    let persisted = {
        let mut registry = lock(&inner.registry);
        let Some(session) = registry.get_mut(&id) else {
            return;
        };
        change(session);
        session.updated_at_ms = now_ms();
        session.clone()
    };
    if let Err(e) = inner.store.write_meta(&persisted) {
        eprintln!("[sesiones] #{id}: no se pudo guardar meta.json: {e}");
    }
}

/// Copia del estado actual de una sub-sesión.
fn snapshot(inner: &Arc<Inner>, id: SessionId) -> Option<SubSession> {
    lock(&inner.registry).get(&id).cloned()
}

/// Acumula el evento en el registro observable.
fn emit(inner: &Arc<Inner>, event: SessionEvent) {
    let mut events = lock(&inner.events);
    if events.len() >= EVENTS_CAPACITY {
        events.remove(0);
    }
    events.push(event);
}

/// Encola una locución de resultado de la sub-sesión.
fn speak(inner: &Arc<Inner>, id: SessionId, text: String) {
    inner.out.push(Delivery {
        text,
        priority: SpeechPriority::TaskResult,
        task: Some(u64::from(id)),
        enqueued_at: Instant::now(),
    });
}

/// Resultado interpretado de un turno del subagente.
struct ParsedOutput {
    summary: String,
    detail: String,
    question: Option<String>,
}

/// Interpreta el protocolo `PROGRESO/PREGUNTA/RESUMEN_HABLADO/DETALLE`.
fn parse_output(text: &str) -> ParsedOutput {
    let question = extract_question(text);
    let scrubbed = strip_progress_and_question_lines(text);
    let (summary, detail) = split_marked(&scrubbed);
    let summary = summary.trim();
    let summary = if summary.is_empty() {
        "No hubo resumen del agente.".to_string()
    } else {
        summary.to_string()
    };
    ParsedOutput {
        summary,
        detail,
        question,
    }
}

/// Separa `RESUMEN_HABLADO:` de `DETALLE:` dentro de un turno.
///
/// A diferencia de [`crate::tasks::split_task_output`] no exige las dos
/// secciones: un turno que solo trae resumen (o solo detalle) se interpreta
/// igual, en vez de colar los marcadores en el resumen que se lee en voz alta.
fn split_marked(text: &str) -> (String, String) {
    let summary = find_ci(text, SUMMARY_MARKER)
        .map(|at| (at, SUMMARY_MARKER.len()))
        .or_else(|| find_ci(text, "RESUMEN:").map(|at| (at, "RESUMEN:".len())));
    let detail = find_ci(text, DETAIL_MARKER);
    match (summary, detail) {
        (Some((start, len)), Some(detail_at)) if detail_at > start + len => (
            summarize(text[start + len..detail_at].trim(), 60),
            text[detail_at + DETAIL_MARKER.len()..].trim().to_string(),
        ),
        (Some((start, len)), _) => (summarize(text[start + len..].trim(), 60), String::new()),
        (None, Some(detail_at)) => (
            summarize(text[..detail_at].trim(), 60),
            text[detail_at + DETAIL_MARKER.len()..].trim().to_string(),
        ),
        (None, None) => (summarize(text, 60), text.to_string()),
    }
}

/// Busca `needle` (ASCII) sin distinguir mayúsculas.
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    hay.as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Extrae la primera `PREGUNTA:` del texto, con su continuación si el texto va
/// en las líneas siguientes.
fn extract_question(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|line| header_text(line, QUESTION_MARKER).is_some())?;
    let mut question = header_text(lines[start], QUESTION_MARKER).unwrap_or_default();
    if question.is_empty() {
        for next in &lines[start + 1..] {
            if next.trim().is_empty() || is_output_marker(next) {
                break;
            }
            if !question.is_empty() {
                question.push(' ');
            }
            question.push_str(next.trim());
        }
    }
    let question = question.trim().trim_matches('*').trim().to_string();
    (!question.is_empty()).then_some(question)
}

/// Quita del texto las líneas de `PROGRESO:` y `PREGUNTA:` para que no
/// contaminen el resumen hablado ni el detalle archivado.
fn strip_progress_and_question_lines(text: &str) -> String {
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            header_text(line, PROGRESS_MARKER).is_none()
                && header_text(line, QUESTION_MARKER).is_none()
        })
        .collect();
    kept.join("\n")
}

/// `true` si la línea es un marcador del protocolo de salida.
fn is_output_marker(line: &str) -> bool {
    header_text(line, PROGRESS_MARKER).is_some()
        || header_text(line, QUESTION_MARKER).is_some()
        || header_text(line, SUMMARY_MARKER).is_some()
        || header_text(line, "RESUMEN:").is_some()
        || header_text(line, DETAIL_MARKER).is_some()
}

/// Texto que sigue a un marcador del protocolo, si la línea lo contiene.
///
/// Acepta el marcador con viñeta, negritas o almohadilla delante
/// (`- PROGRESO: …`, `**PREGUNTA:** …`).
fn header_text(line: &str, marker: &str) -> Option<String> {
    let body = strip_markup_prefix(line);
    let body = body.trim_start_matches('*');
    let rest = strip_prefix_ci(body, marker)?;
    Some(rest.trim().trim_matches('*').trim().to_string())
}

/// Quita viñetas y almohadillas del principio de la línea.
fn strip_markup_prefix(line: &str) -> &str {
    let mut rest = line.trim();
    loop {
        let trimmed = rest
            .trim_start_matches(['-', '*', '#', '>', '•', '+'])
            .trim_start();
        if trimmed.len() == rest.len() {
            return trimmed;
        }
        rest = trimmed;
    }
}

/// `strip_prefix` sin distinguir mayúsculas.
fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

/// Busca líneas `PROGRESO:` en el flujo de tokens aunque lleguen partidas.
#[derive(Debug, Default)]
struct ProgressScanner {
    line: String,
}

impl ProgressScanner {
    /// Procesa un fragmento y acumula los avances completos en `out`.
    fn feed(&mut self, token: &str, out: &mut Vec<String>) {
        for ch in token.chars() {
            if ch == '\n' {
                if let Some(text) = progress_text(&self.line) {
                    out.push(text);
                }
                self.line.clear();
                continue;
            }
            self.line.push(ch);
            if self.line.len() >= SCAN_LINE_MAX_CHARS {
                // Línea anómalamente larga: no es un avance, no la acumulamos.
                self.line.clear();
            }
        }
    }

    /// Cierra la última línea si el turno terminó sin salto de línea.
    fn flush(&mut self, out: &mut Vec<String>) {
        if let Some(text) = progress_text(&self.line) {
            out.push(text);
        }
        self.line.clear();
    }
}

/// Texto de un avance pendiente en la línea, si la hubiera.
fn progress_text(line: &str) -> Option<String> {
    header_text(line, PROGRESS_MARKER).filter(|text| !text.is_empty())
}

/// Recorta un texto a `max_chars` caracteres sin romper UTF-8.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max_chars).collect();
    clipped.push_str("\n\n[truncado]");
    clipped
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Toma el mutex ignorando el envenenamiento (un pánico previo no debe dejar
/// las sub-sesiones inutilizables).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
