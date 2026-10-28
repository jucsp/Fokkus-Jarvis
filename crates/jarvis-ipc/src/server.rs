//! Servidor IPC: escucha en un Unix Domain Socket y responde JSON-RPC 2.0.
//!
//! Responsabilidades:
//!
//! * **Ciclo de vida del socket:** bind, limpieza de sockets *stale* (I6) y
//!   borrado en `Drop`.
//! * **Framing NDJSON** con límite de [`MAX_MESSAGE_BYTES`] (I1).
//! * **Concurrencia:** un hilo aceptador + un hilo por conexión (tope
//!   [`MAX_CONNECTIONS`]); el estado se comparte con `Arc<Mutex<…>>`.
//! * **Endurecimiento (I6):** directorio `0700` y socket `0600`.
//!
//! Sobre `SO_PEERCRED`: `std` **no** lo expone de forma estable
//! (`UCred`/`peer_cred()` siguen siendo API experimental,
//! `peer_credentials_unix_socket` · rust-lang/rust#42839). La verificación por
//! credenciales requiere la dependencia `libc`, pendiente de aprobación del PO
//! (ver [`PeerPolicy`]).

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(unix)]
use std::thread::{self, JoinHandle};
#[cfg(unix)]
use std::time::Duration;

use jarvis_core::{JarvisConfig, JarvisCore, JarvisState, SharedCore};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use crate::paths;
#[cfg(unix)]
use crate::protocol::MAX_MESSAGE_BYTES;
use crate::protocol::{
    error_codes, methods, session_event, speech_priority, task_status, Ack, CancelTaskResult,
    DispatchSwarmParams, DispatchTaskParams, DispatchTaskResult, HelloParams, HelloResult,
    HelloSession, Incoming, JsonRpcError, Notification, NotifySwarmCompletedParams,
    NotifySwarmProgressParams, NotifyTriageAlertParams, PingResult, Response, RoutingMetrics,
    SessionCancelResult, SessionEventParams, SessionIdParams, SessionReadResult, SessionSendParams,
    SessionSendResult, SessionsResult, SetMutedParams, SetMutedResult, SpeakParams, SpeakResult,
    StateResult, SwarmStatusResult, SwarmTask, TaskIdParams, TasksResult, TriageAlert, Utterance,
    PROTOCOL_VERSION, SERVER_VERSION,
};

/// Número máximo de conexiones concurrentes antes de responder `-32003`.
pub const MAX_CONNECTIONS: usize = 32;

/// Longitud máxima de la ruta de un socket en Linux (`sun_path` de 108 bytes,
/// incluido el NUL final).
pub const MAX_SOCKET_PATH_BYTES: usize = 107;

/// Política de verificación de identidad del cliente que se conecta (I6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PeerPolicy {
    /// Permisos de sistema de archivos: directorio `0700` + socket `0600`.
    ///
    /// Es lo único que `std` permite en Rust *stable*; suficiente para que
    /// ningún otro usuario del sistema pueda conectarse.
    #[default]
    SocketPermissions,
    /// Comparación de credenciales con `SO_PEERCRED`.
    ///
    /// **No disponible todavía:** requiere la dependencia `libc`
    /// (`UCred`/`peer_cred` son inestables en `std`,
    /// rust-lang/rust#42839). Solicitar esta política hace que
    /// [`IpcServer::bind`] falle con [`IpcError::PeerCredentialsUnavailable`]
    /// en lugar de degradar en silencio.
    PeerCredentials,
}

/// Contrato de locución: permite conectar el servidor con `jarvis-audio`.
pub trait SpeechSink: Send + Sync {
    /// Notifica una locución aceptada por el servidor.
    fn speak(&self, utterance: &Utterance);

    /// Encola la locución en la cola hablada priorizada (HU-37) con una de
    /// las prioridades de [`speech_priority`]. Devuelve la posición
    /// (1 = la siguiente; 0 = rechazada por cola llena) o `None` si el
    /// sumidero no tiene cola.
    fn enqueue(&self, _utterance: &Utterance, _priority: &str) -> Option<u32> {
        None
    }
}

/// Puerto de tareas pesadas (HU-37): lo implementa `jarvis-app` sobre el
/// `TaskManager` de `jarvis-audio`, sin que este crate dependa de él.
///
/// Desde HU-66 el mismo puerto expone las sub-sesiones (`jarvis.sessions.*`):
/// mientras el `SessionManager` de HU-62 no esté conectado al orquestador, una
/// sub-sesión es una tarea de fondo y estos métodos por defecto fallan de forma
/// explícita en vez de fingir que funcionan.
pub trait TaskPort: Send + Sync {
    /// Despacha una tarea y devuelve su id estable.
    fn dispatch(&self, title: String, prompt: String) -> u64;
    /// Registros de todas las tareas, ordenados por id.
    fn list(&self) -> Vec<Value>;
    /// Registro de una tarea, si existe.
    fn get(&self, id: u64) -> Option<Value>;
    /// Cancela una tarea en cola o en curso; `false` si no existe o ya terminó.
    fn cancel(&self, id: u64) -> bool;

    /// Envía una instrucción nueva a una sub-sesión (HU-66).
    ///
    /// La implementación por defecto rechaza el envío: un puerto que no sepa
    /// continuar un hilo no debe aceptar la instrucción y perderla.
    fn send_session(&self, _id: u64, _instruction: String) -> Result<(), String> {
        Err(SEND_SESSION_UNSUPPORTED.to_string())
    }

    /// Detalle en markdown de una sub-sesión (`None` si no hay) (HU-66).
    fn read_detail(&self, _id: u64) -> Option<String> {
        None
    }
}

/// Motivo con el que un [`TaskPort`] sin sub-sesiones rechaza `send_session`.
pub const SEND_SESSION_UNSUPPORTED: &str = "send_session no soportado";

/// Tope del registro de eventos de sub-sesión (`jarvis.sessionEvent`).
const SESSION_EVENTS_CAPACITY: usize = 256;

/// Estado compartido que atiende las llamadas del protocolo.
///
/// Es seguro de compartir entre los hilos de conexión (`Arc`). Nunca hace
/// *panic* por envenenamiento de `Mutex`: los locks se recuperan del error de
/// veneno con [`std::sync::PoisonError::into_inner`].
pub struct ServerState {
    core: SharedCore,
    swarm: Mutex<BTreeMap<String, SwarmTask>>,
    progress: Mutex<Vec<NotifySwarmProgressParams>>,
    alerts: Mutex<Vec<TriageAlert>>,
    utterances: Mutex<Vec<Utterance>>,
    sink: Mutex<Option<Arc<dyn SpeechSink>>>,
    tasks: Mutex<Option<Arc<dyn TaskPort>>>,
    hello: Mutex<Option<HelloSession>>,
    /// Eventos de sub-sesión emitidos (HU-66), acotados a
    /// [`SESSION_EVENTS_CAPACITY`].
    session_events: Mutex<Vec<SessionEventParams>>,
    /// Fuente de las métricas de enrutamiento (HU-68); `None` sin voz.
    routing: Mutex<Option<RoutingSource>>,
}

/// Lee las métricas de enrutamiento del orquestador en el momento (HU-68).
pub type RoutingSource = Arc<dyn Fn() -> RoutingMetrics + Send + Sync>;

impl ServerState {
    /// Crea el estado a partir de un núcleo ya construido.
    pub fn new(core: JarvisCore) -> Self {
        Self::with_shared_core(SharedCore::new(core))
    }

    /// Crea el estado sobre un núcleo compartido con la esfera y el
    /// orquestador de voz (un único `JarvisCore`, SDD §5).
    pub fn with_shared_core(core: SharedCore) -> Self {
        Self {
            core,
            swarm: Mutex::new(BTreeMap::new()),
            progress: Mutex::new(Vec::new()),
            alerts: Mutex::new(Vec::new()),
            utterances: Mutex::new(Vec::new()),
            sink: Mutex::new(None),
            tasks: Mutex::new(None),
            hello: Mutex::new(None),
            session_events: Mutex::new(Vec::new()),
            routing: Mutex::new(None),
        }
    }

    /// Estado con la configuración por defecto.
    pub fn with_default_config() -> Self {
        Self::new(JarvisCore::new(JarvisConfig::default()))
    }

    /// Registra un sumidero de locuciones (p. ej. el motor de audio).
    pub fn with_speech_sink(self, sink: Arc<dyn SpeechSink>) -> Self {
        *lock(&self.sink) = Some(sink);
        self
    }

    /// Registra el puerto de tareas pesadas (HU-37).
    pub fn with_task_port(self, tasks: Arc<dyn TaskPort>) -> Self {
        self.set_task_port(tasks);
        self
    }

    /// Instala el puerto de tareas con el servidor ya en marcha (el
    /// orquestador de voz arranca después del IPC).
    pub fn set_task_port(&self, tasks: Arc<dyn TaskPort>) {
        *lock(&self.tasks) = Some(tasks);
    }

    /// Instala la fuente de métricas de enrutamiento de `jarvis.getState`
    /// (HU-68). Como el puerto de tareas, llega con el servidor ya en marcha.
    pub fn set_routing_metrics(&self, source: RoutingSource) {
        *lock(&self.routing) = Some(source);
    }

    /// Núcleo compartido (el mismo que ven la esfera y el orquestador).
    pub fn shared_core(&self) -> &SharedCore {
        &self.core
    }

    /// Estado actual del asistente.
    pub fn state(&self) -> JarvisState {
        self.core.state()
    }

    /// `true` si el asistente está silenciado.
    pub fn is_muted(&self) -> bool {
        self.core.is_muted()
    }

    /// Par voz activa (`voice_id`, `voice_lang`).
    pub fn voice(&self) -> (String, String) {
        let core = self.core.lock();
        (
            core.config().voice_id.clone(),
            core.config().voice_lang.clone(),
        )
    }

    /// Cambia el silencio global; es idempotente.
    pub fn set_muted(&self, muted: bool) -> Result<SetMutedResult, JsonRpcError> {
        let mut core = self.core.lock();
        if core.is_muted() != muted {
            let target = if muted {
                JarvisState::Muted
            } else {
                JarvisState::Idle
            };
            if let Err(e) = core.transition(target) {
                return Err(JsonRpcError::new(
                    error_codes::INTERNAL_ERROR,
                    format!("no se pudo cambiar el estado de silencio: {e}"),
                ));
            }
        }
        Ok(SetMutedResult {
            muted: core.is_muted(),
            state: state_name(core.state()).to_string(),
        })
    }

    /// Tareas swarm registradas, ordenadas por `task_id`.
    pub fn swarm_tasks(&self) -> Vec<SwarmTask> {
        lock(&self.swarm).values().cloned().collect()
    }

    /// Número de tareas swarm en vuelo.
    pub fn in_flight(&self) -> usize {
        lock(&self.swarm)
            .values()
            .filter(|task| task.status == task_status::DISPATCHED)
            .count()
    }

    /// Alertas de triage recibidas.
    pub fn alerts(&self) -> Vec<TriageAlert> {
        lock(&self.alerts).clone()
    }

    /// Locuciones aceptadas por el servidor.
    pub fn utterances(&self) -> Vec<Utterance> {
        lock(&self.utterances).clone()
    }

    /// Eventos de progreso recibidos.
    pub fn progress_events(&self) -> Vec<NotifySwarmProgressParams> {
        lock(&self.progress).clone()
    }

    /// Sesión del IDE registrada con `jarvis.hello`, si existe.
    pub fn hello_session(&self) -> Option<HelloSession> {
        lock(&self.hello).clone()
    }

    /// Procesa una línea entrante y devuelve la respuesta NDJSON.
    ///
    /// Devuelve `None` para notificaciones y respuestas entrantes (no admiten
    /// respuesta) y una respuesta de error con `id: null` si el mensaje no se
    /// puede parsear.
    pub fn handle_line(&self, line: &str) -> Option<String> {
        match Incoming::parse_line(line) {
            Ok(message) => self.dispatch(message).map(|response| response.to_ndjson()),
            Err(error) => Some(Response::error(None, error).to_ndjson()),
        }
    }

    /// Aplica un mensaje ya clasificado.
    pub fn dispatch(&self, message: Incoming) -> Option<Response> {
        match message {
            Incoming::Request(request) => {
                let response = match self.handle_call(&request.method, request.params) {
                    Ok(result) => Response::success(request.id, result),
                    Err(error) => Response::error(Some(request.id), error),
                };
                Some(response)
            }
            Incoming::Notification(notification) => {
                // I2: las notificaciones no admiten respuesta. Un error de
                // parámetros se descarta: reportarlo violaría el estándar.
                let _ = self.handle_call(&notification.method, notification.params);
                None
            }
            Incoming::Response(_) => None,
        }
    }

    fn ping(&self) -> PingResult {
        let (voice, _) = self.voice();
        PingResult {
            pong: true,
            status: "ok".to_string(),
            version: SERVER_VERSION.to_string(),
            state: state_name(self.state()).to_string(),
            voice,
        }
    }

    fn get_state(&self) -> StateResult {
        let (voice, voice_lang) = self.voice();
        StateResult {
            state: state_name(self.state()).to_string(),
            muted: self.is_muted(),
            voice,
            voice_lang,
            swarm_in_flight: self.in_flight(),
            routing: lock(&self.routing).as_ref().map(|source| source()),
        }
    }

    fn hello(&self, params: HelloParams) -> Result<HelloResult, JsonRpcError> {
        if params.client.trim().is_empty() {
            return Err(JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                "el campo \"client\" no puede estar vacío",
            ));
        }
        if params.protocol_version != PROTOCOL_VERSION {
            return Err(JsonRpcError::with_data(
                error_codes::PROTOCOL_VERSION_MISMATCH,
                format!(
                    "versión de protocolo incompatible: cliente {} · servidor {PROTOCOL_VERSION}",
                    params.protocol_version
                ),
                json!({ "serverProtocolVersion": PROTOCOL_VERSION }),
            ));
        }
        let session = HelloSession {
            client: params.client,
            client_version: params.client_version,
            protocol_version: params.protocol_version,
        };
        *lock(&self.hello) = Some(session);
        Ok(HelloResult {
            protocol_version: PROTOCOL_VERSION,
            version: SERVER_VERSION.to_string(),
            capabilities: methods::ALL.iter().map(|name| name.to_string()).collect(),
        })
    }

    fn speak(&self, params: SpeakParams) -> Result<SpeakResult, JsonRpcError> {
        if params.text.trim().is_empty() {
            return Err(JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                "el campo \"text\" no puede estar vacío",
            ));
        }
        let priority = params
            .priority
            .as_deref()
            .unwrap_or(speech_priority::NOTICE);
        if !speech_priority::ALL.contains(&priority) {
            return Err(JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                format!(
                    "prioridad desconocida: {priority} (válidas: {})",
                    speech_priority::ALL.join(", ")
                ),
            ));
        }
        let muted = self.is_muted();
        let utterance = Utterance {
            text: params.text.clone(),
            category: params.category.clone(),
        };
        // Se clona el Arc para no retener el lock durante la locución.
        let sink = lock(&self.sink).as_ref().map(Arc::clone);

        // Con `queue: true` se encola aunque esté silenciado: la cola retiene
        // la locución hasta que el *gate* lo permita (CA-37.8/37.9).
        if params.queue == Some(true) {
            if let Some(position) = sink.as_ref().and_then(|s| s.enqueue(&utterance, priority)) {
                if position > 0 {
                    lock(&self.utterances).push(utterance);
                }
                return Ok(SpeakResult {
                    spoken: false,
                    muted,
                    text: params.text,
                    category: params.category,
                    reason: (position == 0).then(|| "queue_full".to_string()),
                    queued: Some(position > 0),
                    position: (position > 0).then_some(position),
                });
            }
        }

        if muted {
            return Ok(SpeakResult {
                spoken: false,
                muted,
                text: params.text,
                category: params.category,
                reason: Some("muted".to_string()),
                queued: None,
                position: None,
            });
        }
        lock(&self.utterances).push(utterance.clone());
        if let Some(sink) = sink {
            sink.speak(&utterance);
        }
        Ok(SpeakResult {
            spoken: true,
            muted,
            text: params.text,
            category: params.category,
            reason: None,
            queued: None,
            position: None,
        })
    }

    fn task_port(&self) -> Result<Arc<dyn TaskPort>, JsonRpcError> {
        lock(&self.tasks).as_ref().map(Arc::clone).ok_or_else(|| {
            JsonRpcError::new(
                error_codes::INTERNAL_ERROR,
                "las tareas de fondo no están activas (falta cerebro o configuración)",
            )
        })
    }

    fn dispatch_task(
        &self,
        params: DispatchTaskParams,
    ) -> Result<DispatchTaskResult, JsonRpcError> {
        if params.prompt.trim().is_empty() {
            return Err(JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                "el campo \"prompt\" no puede estar vacío",
            ));
        }
        let port = self.task_port()?;
        let id = port.dispatch(params.title.trim().to_string(), params.prompt);
        Ok(DispatchTaskResult {
            id,
            status: "queued".to_string(),
        })
    }

    fn get_task(&self, id: u64) -> Result<Value, JsonRpcError> {
        self.task_port()?.get(id).ok_or_else(|| unknown_task(id))
    }

    fn cancel_task(&self, id: u64) -> Result<CancelTaskResult, JsonRpcError> {
        let port = self.task_port()?;
        if port.get(id).is_none() {
            return Err(unknown_task(id));
        }
        Ok(CancelTaskResult {
            id,
            cancelled: port.cancel(id),
        })
    }

    /// `jarvis.sessions.list` (HU-66): las sub-sesiones conocidas.
    fn sessions_list(&self) -> Result<SessionsResult, JsonRpcError> {
        Ok(SessionsResult {
            sessions: self.task_port()?.list(),
        })
    }

    /// `jarvis.sessions.get` (HU-66): una sub-sesión, con la misma carga útil
    /// que `jarvis.getTask`.
    fn sessions_get(&self, id: u64) -> Result<Value, JsonRpcError> {
        self.task_port()?.get(id).ok_or_else(|| unknown_session(id))
    }

    /// `jarvis.sessions.send` (HU-66): instrucción nueva al hilo.
    fn sessions_send(&self, params: SessionSendParams) -> Result<SessionSendResult, JsonRpcError> {
        let instruction = params.instruction.trim();
        if instruction.is_empty() {
            return Err(JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                "el campo \"instruction\" no puede estar vacío",
            ));
        }
        let port = self.task_port()?;
        if port.get(params.id).is_none() {
            return Err(unknown_session(params.id));
        }
        port.send_session(params.id, instruction.to_string())
            .map_err(|e| {
                JsonRpcError::new(
                    error_codes::INTERNAL_ERROR,
                    format!(
                        "no se pudo enviar la instrucción a la sesión {}: {e}",
                        params.id
                    ),
                )
            })?;
        self.emit_session_event(SessionEventParams {
            id: params.id,
            event: session_event::INSTRUCTION.to_string(),
            title: session_title(port.as_ref(), params.id),
            status: session_status(port.as_ref(), params.id),
            detail: Some(instruction.to_string()),
        });
        Ok(SessionSendResult {
            id: params.id,
            ok: true,
        })
    }

    /// `jarvis.sessions.cancel` (HU-66): detiene el turno en curso.
    fn sessions_cancel(&self, id: u64) -> Result<SessionCancelResult, JsonRpcError> {
        let port = self.task_port()?;
        if port.get(id).is_none() {
            return Err(unknown_session(id));
        }
        let cancelled = port.cancel(id);
        if cancelled {
            self.emit_session_event(SessionEventParams {
                id,
                event: session_event::CANCELLED.to_string(),
                title: session_title(port.as_ref(), id),
                status: session_status(port.as_ref(), id),
                detail: None,
            });
        }
        Ok(SessionCancelResult { id, cancelled })
    }

    /// `jarvis.sessions.read` (HU-66): detalle en markdown del último turno.
    fn sessions_read(&self, id: u64) -> Result<SessionReadResult, JsonRpcError> {
        let port = self.task_port()?;
        if port.get(id).is_none() {
            return Err(unknown_session(id));
        }
        let detail = port.read_detail(id).ok_or_else(|| {
            JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                format!("la sesión {id} no tiene detalle guardado"),
            )
        })?;
        Ok(SessionReadResult { id, detail })
    }

    /// Registra un evento de sub-sesión y devuelve la notificación
    /// `jarvis.sessionEvent` lista para enviar (HU-66).
    ///
    /// # Alcance
    ///
    /// El evento queda en [`ServerState::session_events`], que es lo que hoy
    /// pueden consumir el HUD y los tests. El envío *push* a las conexiones
    /// abiertas necesita un canal por conexión y un hilo escritor (el bucle de
    /// conexión es bloqueante en lectura); queda para HU-67.
    pub fn emit_session_event(&self, params: SessionEventParams) -> Notification {
        let payload = serde_json::to_value(&params).unwrap_or(Value::Null);
        {
            let mut events = lock(&self.session_events);
            if events.len() >= SESSION_EVENTS_CAPACITY {
                events.remove(0);
            }
            events.push(params);
        }
        Notification::new(methods::NOTIFY_SESSION_EVENT, Some(payload))
    }

    /// Eventos de sub-sesión emitidos hasta ahora, del más viejo al más nuevo.
    pub fn session_events(&self) -> Vec<SessionEventParams> {
        lock(&self.session_events).clone()
    }

    fn dispatch_swarm(&self, params: DispatchSwarmParams) -> Result<SwarmTask, JsonRpcError> {
        if params.task_id.trim().is_empty() {
            return Err(JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                "el campo \"task_id\" no puede estar vacío",
            ));
        }
        let task = SwarmTask {
            task_id: params.task_id.clone(),
            agent: params.agent,
            goal: params.goal,
            status: task_status::DISPATCHED.to_string(),
            summary: None,
        };
        lock(&self.swarm).insert(task.task_id.clone(), task.clone());
        Ok(task)
    }

    fn complete_swarm(
        &self,
        params: NotifySwarmCompletedParams,
    ) -> Result<SwarmTask, JsonRpcError> {
        if params.task_id.trim().is_empty() {
            return Err(JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                "el campo \"task_id\" no puede estar vacío",
            ));
        }
        let status = params.resolved_status();
        let summary = params.resolved_summary();
        let mut swarm = lock(&self.swarm);
        let task = swarm
            .entry(params.task_id.clone())
            .or_insert_with(|| SwarmTask {
                task_id: params.task_id.clone(),
                agent: "swarm".to_string(),
                goal: summary.clone(),
                status: task_status::DISPATCHED.to_string(),
                summary: None,
            });
        task.status = status;
        if !summary.is_empty() {
            task.summary = Some(summary);
        }
        Ok(task.clone())
    }

    fn record_progress(&self, params: NotifySwarmProgressParams) -> Result<Ack, JsonRpcError> {
        if params.task_id.trim().is_empty() {
            return Err(JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                "el campo \"task_id\" no puede estar vacío",
            ));
        }
        lock(&self.progress).push(params);
        Ok(Ack::new(methods::NOTIFY_SWARM_PROGRESS))
    }

    fn record_alert(&self, params: NotifyTriageAlertParams) -> Result<Ack, JsonRpcError> {
        if params.item_id.trim().is_empty() {
            return Err(JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                "el campo \"item_id\" no puede estar vacío",
            ));
        }
        lock(&self.alerts).push(TriageAlert::from(params));
        Ok(Ack::new(methods::NOTIFY_TRIAGE_ALERT))
    }

    fn swarm_status(&self) -> SwarmStatusResult {
        SwarmStatusResult {
            tasks: self.swarm_tasks(),
            in_flight: self.in_flight(),
        }
    }

    fn handle_call(&self, method: &str, params: Option<Value>) -> Result<Value, JsonRpcError> {
        match method {
            methods::HELLO => {
                let params: HelloParams = params_as(method, params)?;
                to_value(self.hello(params)?)
            }
            methods::PING => to_value(self.ping()),
            methods::GET_STATE => to_value(self.get_state()),
            methods::SET_MUTED => {
                let params: SetMutedParams = params_as(method, params)?;
                to_value(self.set_muted(params.muted)?)
            }
            methods::SPEAK => {
                let params: SpeakParams = params_as(method, params)?;
                to_value(self.speak(params)?)
            }
            methods::DISPATCH_SWARM => {
                let params: DispatchSwarmParams = params_as(method, params)?;
                to_value(self.dispatch_swarm(params)?)
            }
            methods::GET_SWARM_STATUS => to_value(self.swarm_status()),
            methods::NOTIFY_SWARM_PROGRESS => {
                let params: NotifySwarmProgressParams = params_as(method, params)?;
                to_value(self.record_progress(params)?)
            }
            methods::NOTIFY_SWARM_COMPLETED => {
                let params: NotifySwarmCompletedParams = params_as(method, params)?;
                to_value(self.complete_swarm(params)?)
            }
            methods::NOTIFY_TRIAGE_ALERT => {
                let params: NotifyTriageAlertParams = params_as(method, params)?;
                to_value(self.record_alert(params)?)
            }
            methods::DISPATCH_TASK => {
                let params: DispatchTaskParams = params_as(method, params)?;
                to_value(self.dispatch_task(params)?)
            }
            methods::GET_TASKS => to_value(TasksResult {
                tasks: self.task_port()?.list(),
            }),
            methods::GET_TASK => {
                let params: TaskIdParams = params_as(method, params)?;
                self.get_task(params.id)
            }
            methods::CANCEL_TASK => {
                let params: TaskIdParams = params_as(method, params)?;
                to_value(self.cancel_task(params.id)?)
            }
            methods::SESSIONS_LIST => to_value(self.sessions_list()?),
            methods::SESSIONS_GET => {
                let params: SessionIdParams = params_as(method, params)?;
                self.sessions_get(params.id)
            }
            methods::SESSIONS_SEND => {
                let params: SessionSendParams = params_as(method, params)?;
                to_value(self.sessions_send(params)?)
            }
            methods::SESSIONS_CANCEL => {
                let params: SessionIdParams = params_as(method, params)?;
                to_value(self.sessions_cancel(params.id)?)
            }
            methods::SESSIONS_READ => {
                let params: SessionIdParams = params_as(method, params)?;
                to_value(self.sessions_read(params.id)?)
            }
            other => Err(JsonRpcError::new(
                error_codes::METHOD_NOT_FOUND,
                format!("método desconocido: {other}"),
            )),
        }
    }
}

fn unknown_task(id: u64) -> JsonRpcError {
    JsonRpcError::new(
        error_codes::INVALID_PARAMS,
        format!("no existe la tarea {id}"),
    )
}

/// Error de una sub-sesión inexistente (HU-66).
fn unknown_session(id: u64) -> JsonRpcError {
    JsonRpcError::new(
        error_codes::INVALID_PARAMS,
        format!("no existe la sesión {id}"),
    )
}

/// Título del hilo según su registro, si lo informa (HU-66).
fn session_title(port: &dyn TaskPort, id: u64) -> Option<String> {
    port.get(id)?
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Estado del hilo según su registro, si lo informa (HU-66).
fn session_status(port: &dyn TaskPort, id: u64) -> Option<String> {
    port.get(id)?
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_string)
}

impl fmt::Debug for ServerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerState")
            .field("state", &self.state())
            .field("muted", &self.is_muted())
            .field("in_flight", &self.in_flight())
            .finish()
    }
}

/// Error del servidor IPC.
#[derive(Debug)]
pub enum IpcError {
    /// Error de entrada/salida (bind, permisos, hilos).
    Io(std::io::Error),
    /// Ya hay un servidor escuchando en esa ruta.
    AlreadyRunning(PathBuf),
    /// La ruta del socket no tiene directorio padre.
    InvalidPath(PathBuf),
    /// La ruta excede el límite de `sun_path`.
    SocketPathTooLong { path: PathBuf, len: usize },
    /// El transporte IPC (Unix Domain Socket) no está soportado en esta
    /// plataforma (p. ej. Windows hasta que llegue el Named Pipe del SDD §3).
    Unsupported,
    /// Se pidió [`PeerPolicy::PeerCredentials`], no disponible en `std` estable.
    PeerCredentialsUnavailable,
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpcError::Io(e) => write!(f, "error de I/O en el servidor IPC: {e}"),
            IpcError::AlreadyRunning(path) => {
                write!(f, "ya hay un servidor Jarvis en {}", path.display())
            }
            IpcError::InvalidPath(path) => {
                write!(f, "ruta de socket sin directorio padre: {}", path.display())
            }
            IpcError::SocketPathTooLong { path, len } => write!(
                f,
                "ruta de socket de {len} bytes (máximo {MAX_SOCKET_PATH_BYTES}): {}",
                path.display()
            ),
            IpcError::Unsupported => write!(
                f,
                "transporte IPC (Unix Domain Socket) no soportado en esta plataforma"
            ),
            IpcError::PeerCredentialsUnavailable => write!(
                f,
                "SO_PEERCRED requiere la dependencia `libc`: UCred/peer_cred son inestables en std"
            ),
        }
    }
}

impl std::error::Error for IpcError {}

impl From<std::io::Error> for IpcError {
    fn from(error: std::io::Error) -> Self {
        IpcError::Io(error)
    }
}

/// Servidor IPC con hilo aceptador propio (Unix Domain Sockets).
#[cfg(unix)]
pub struct IpcServer {
    socket_path: PathBuf,
    state: Arc<ServerState>,
    listener: Option<UnixListener>,
    shutdown_flag: Arc<AtomicBool>,
    accept_handle: Option<JoinHandle<()>>,
    connections: Arc<AtomicUsize>,
    peer_policy: PeerPolicy,
    bound: bool,
}

#[cfg(unix)]
impl IpcServer {
    /// Crea el servidor en la ruta por defecto ([`paths::default_socket_path`]).
    pub fn new(state: Arc<ServerState>) -> Self {
        Self::with_path(paths::default_socket_path(), state)
    }

    /// Crea el servidor en una ruta explícita (tests, perfiles múltiples).
    pub fn with_path(path: impl Into<PathBuf>, state: Arc<ServerState>) -> Self {
        Self {
            socket_path: path.into(),
            state,
            listener: None,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            accept_handle: None,
            connections: Arc::new(AtomicUsize::new(0)),
            peer_policy: PeerPolicy::default(),
            bound: false,
        }
    }

    /// Ruta del socket.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Estado compartido del servidor.
    pub fn state(&self) -> &Arc<ServerState> {
        &self.state
    }

    /// Política de verificación del cliente.
    pub fn peer_policy(&self) -> PeerPolicy {
        self.peer_policy
    }

    /// Cambia la política de verificación del cliente (antes de `bind`).
    pub fn set_peer_policy(&mut self, policy: PeerPolicy) {
        self.peer_policy = policy;
    }

    /// Conexiones activas en este momento.
    pub fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// `true` si el socket está creado y el hilo aceptador está corriendo.
    pub fn is_running(&self) -> bool {
        self.accept_handle.is_some()
    }

    /// Crea el socket, limpiando uno *stale* si existe.
    ///
    /// Si ya hay un servidor vivo en la ruta devuelve
    /// [`IpcError::AlreadyRunning`] (I6: probar `connect` antes de borrar).
    pub fn bind(&mut self) -> Result<(), IpcError> {
        if self.bound {
            return Ok(());
        }
        if self.peer_policy == PeerPolicy::PeerCredentials {
            return Err(IpcError::PeerCredentialsUnavailable);
        }

        let path = self.socket_path.clone();
        let len = path.as_os_str().len();
        if len > MAX_SOCKET_PATH_BYTES {
            return Err(IpcError::SocketPathTooLong { path, len });
        }
        let parent = path
            .parent()
            .ok_or_else(|| IpcError::InvalidPath(path.clone()))?;
        fs::create_dir_all(parent)?;
        // I6: el directorio debe ser privado para que nadie ajeno entre.
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;

        if path.exists() {
            cleanup_stale(&path)?;
        }

        let listener = UnixListener::bind(&path)?;
        // I6: el socket solo lo usa el propietario.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        self.listener = Some(listener);
        self.bound = true;
        Ok(())
    }

    /// Añade el socket con [`IpcServer::bind`] y lanza el hilo aceptador.
    ///
    /// Es idempotente: si ya está sirviendo, no hace nada. Devuelve la ruta del
    /// socket activo (`Some`) o `None` si la plataforma no soporta socket Unix.
    pub fn start(&mut self) -> Result<Option<PathBuf>, IpcError> {
        if self.accept_handle.is_some() {
            return Ok(Some(self.socket_path.clone()));
        }
        self.bind()?;
        let Some(listener) = self.listener.take() else {
            return Ok(Some(self.socket_path.clone()));
        };

        self.shutdown_flag.store(false, Ordering::SeqCst);
        let state = Arc::clone(&self.state);
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let connections = Arc::clone(&self.connections);
        let handle = thread::Builder::new()
            .name("jarvis-ipc-accept".to_string())
            .spawn(move || {
                accept_loop(listener, state, shutdown_flag, connections, MAX_CONNECTIONS);
            })?;
        self.accept_handle = Some(handle);
        Ok(Some(self.socket_path.clone()))
    }

    /// Detiene el hilo aceptador y espera a que termine.
    pub fn shutdown(&mut self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept_handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(unix)]
impl Drop for IpcServer {
    fn drop(&mut self) {
        self.shutdown();
        // Solo se borra el socket si lo creamos nosotros: si el bind falló con
        // "ya en ejecución", el archivo pertenece a otro proceso.
        if self.bound {
            let _ = fs::remove_file(&self.socket_path);
        }
    }
}

#[cfg(unix)]
impl fmt::Debug for IpcServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IpcServer")
            .field("socket_path", &self.socket_path)
            .field("running", &self.is_running())
            .field("connections", &self.connection_count())
            .field("peer_policy", &self.peer_policy)
            .finish()
    }
}

/// Servidor IPC (stub para plataformas sin `UnixListener`, p. ej. Windows).
///
/// La implementación real usa Unix Domain Sockets; el Named Pipe de Windows
/// llegará en una fase posterior (SDD §3). Este stub mantiene la misma
/// superficie pública para que el workspace compile en cualquier plataforma:
/// no enlaza ningún socket, no lanza hilos y [`IpcServer::start`] devuelve
/// `Ok(None)`.
#[cfg(not(unix))]
pub struct IpcServer {
    socket_path: PathBuf,
    state: Arc<ServerState>,
    peer_policy: PeerPolicy,
}

#[cfg(not(unix))]
impl IpcServer {
    /// Crea el servidor en la ruta por defecto ([`paths::default_socket_path`]).
    pub fn new(state: Arc<ServerState>) -> Self {
        Self::with_path(paths::default_socket_path(), state)
    }

    /// Crea el servidor en una ruta explícita (tests, perfiles múltiples).
    pub fn with_path(path: impl Into<PathBuf>, state: Arc<ServerState>) -> Self {
        Self {
            socket_path: path.into(),
            state,
            peer_policy: PeerPolicy::default(),
        }
    }

    /// Ruta configurada del socket (nunca enlazada en esta plataforma).
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Estado compartido del servidor.
    pub fn state(&self) -> &Arc<ServerState> {
        &self.state
    }

    /// Política de verificación del cliente.
    pub fn peer_policy(&self) -> PeerPolicy {
        self.peer_policy
    }

    /// Cambia la política de verificación del cliente.
    pub fn set_peer_policy(&mut self, policy: PeerPolicy) {
        self.peer_policy = policy;
    }

    /// Conexiones activas: siempre 0 (sin socket real en esta plataforma).
    pub fn connection_count(&self) -> usize {
        0
    }

    /// `true` si el socket está enlazado: nunca en esta plataforma.
    pub fn is_running(&self) -> bool {
        false
    }

    /// No-op: no hay Unix Domain Sockets que enlazar en esta plataforma.
    pub fn bind(&mut self) -> Result<(), IpcError> {
        Ok(())
    }

    /// No-op: no arranca ningún hilo aceptador y devuelve `Ok(None)`.
    pub fn start(&mut self) -> Result<Option<PathBuf>, IpcError> {
        Ok(None)
    }

    /// No-op: no hay hilo aceptador que detener.
    pub fn shutdown(&mut self) {}
}

/// Recupera un lock ignorando el envenenamiento (nunca hace *panic*).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Serializa un resultado de dominio a `Value`.
fn to_value<T: Serialize>(value: T) -> Result<Value, JsonRpcError> {
    serde_json::to_value(value).map_err(|e| {
        JsonRpcError::new(
            error_codes::INTERNAL_ERROR,
            format!("no se pudo serializar el resultado: {e}"),
        )
    })
}

/// Deserializa los parámetros de una llamada.
fn params_as<T: DeserializeOwned>(method: &str, params: Option<Value>) -> Result<T, JsonRpcError> {
    let value = match params {
        Some(value) => value,
        None => Value::Null,
    };
    serde_json::from_value(value).map_err(|e| {
        JsonRpcError::new(
            error_codes::INVALID_PARAMS,
            format!("parámetros inválidos para {method}: {e}"),
        )
    })
}

/// Nombre estable en el protocolo para cada estado.
fn state_name(state: JarvisState) -> &'static str {
    match state {
        JarvisState::Idle => "idle",
        JarvisState::Listening => "listening",
        JarvisState::Thinking => "thinking",
        JarvisState::Speaking => "speaking",
        JarvisState::Muted => "muted",
    }
}

/// Borra un socket *stale*; si hay un servidor vivo responde `AlreadyRunning`.
#[cfg(unix)]
fn cleanup_stale(path: &Path) -> Result<(), IpcError> {
    match UnixStream::connect(path) {
        Ok(_) => Err(IpcError::AlreadyRunning(path.to_path_buf())),
        Err(_) => fs::remove_file(path).map_err(IpcError::Io),
    }
}

/// Resultado de leer una unidad NDJSON del stream.
#[cfg(unix)]
enum ReadStatus {
    /// El peer cerró la conexión.
    Eof,
    /// El mensaje supera [`MAX_MESSAGE_BYTES`]: hay que cerrar la conexión (I1).
    TooLarge,
    /// Se leyó una línea completa en el buffer.
    Complete,
}

/// Lee una línea NDJSON acotada, sin crecer sin control.
///
/// Acumula desde el `BufReader` sin superar `MAX_MESSAGE_BYTES + 1` y deja la
/// línea cruda (incluido el `\n`) en `buffer`.
#[cfg(unix)]
fn read_message(
    reader: &mut BufReader<UnixStream>,
    buffer: &mut Vec<u8>,
) -> std::io::Result<ReadStatus> {
    buffer.clear();
    loop {
        // El préstamo de `reader` termina al cerrar el bloque: así se puede
        // llamar a `consume` después.
        let (chunk, found) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(if buffer.is_empty() {
                    ReadStatus::Eof
                } else {
                    ReadStatus::Complete
                });
            }
            match available.iter().position(|byte| *byte == b'\n') {
                Some(position) => (available[..=position].to_vec(), true),
                None => (available.to_vec(), false),
            }
        };
        if buffer.len() + chunk.len() > MAX_MESSAGE_BYTES + 1 {
            // I1: el mensaje excede el límite → se cierra la conexión.
            return Ok(ReadStatus::TooLarge);
        }
        let consumed = chunk.len();
        buffer.extend_from_slice(&chunk);
        reader.consume(consumed);
        if found {
            return Ok(ReadStatus::Complete);
        }
    }
}

/// Atiende una conexión hasta EOF o error.
#[cfg(unix)]
fn handle_connection(stream: UnixStream, state: Arc<ServerState>) {
    let mut writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    let mut buffer: Vec<u8> = Vec::new();

    loop {
        match read_message(&mut reader, &mut buffer) {
            Ok(ReadStatus::Eof) => break,
            Ok(ReadStatus::TooLarge) => {
                let error = JsonRpcError::new(
                    error_codes::INVALID_REQUEST,
                    format!("mensaje mayor a {MAX_MESSAGE_BYTES} bytes"),
                );
                let _ = write_line(&mut writer, &Response::error(None, error).to_ndjson());
                break;
            }
            Ok(ReadStatus::Complete) => {
                let text = String::from_utf8_lossy(&buffer);
                let line = text.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(response) = state.handle_line(line) {
                    if write_line(&mut writer, &response).is_err() {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }

    // Cierre ordenado: el peer puede estar esperando el EOF.
    let _ = writer.shutdown(std::net::Shutdown::Both);
}

/// Escribe una línea NDJSON y la vacía al socket.
#[cfg(unix)]
fn write_line(writer: &mut UnixStream, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes())?;
    writer.flush()
}

/// Hilo aceptador: un hilo por conexión, con tope `limit`.
#[cfg(unix)]
fn accept_loop(
    listener: UnixListener,
    state: Arc<ServerState>,
    shutdown_flag: Arc<AtomicBool>,
    connections: Arc<AtomicUsize>,
    limit: usize,
) {
    while !shutdown_flag.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                if connections.load(Ordering::SeqCst) >= limit {
                    // I8: -32003 Busy cuando se satura el tope de conexiones.
                    let error = JsonRpcError::new(
                        error_codes::BUSY,
                        format!("demasiadas conexiones concurrentes (máximo {limit})"),
                    );
                    let _ = write_line(&mut stream, &Response::error(None, error).to_ndjson());
                    continue;
                }
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                connections.fetch_add(1, Ordering::SeqCst);
                let state = Arc::clone(&state);
                let counter = Arc::clone(&connections);
                let spawned = thread::Builder::new()
                    .name("jarvis-ipc-conn".to_string())
                    .spawn(move || {
                        handle_connection(stream, state);
                        counter.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    connections.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// Sumidero de prueba que cuenta las locuciones recibidas.
    struct CountingSink {
        calls: AtomicUsize,
    }

    impl SpeechSink for CountingSink {
        fn speak(&self, _utterance: &Utterance) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn temp_socket_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("jarvis-ipc-test-{name}-{}", std::process::id()))
            .join("jarvis.sock")
    }

    fn call(state: &ServerState, line: &str) -> Response {
        let response_line = state.handle_line(line).expect("se esperaba respuesta");
        serde_json::from_str(response_line.trim()).expect("respuesta JSON válida")
    }

    fn result_of(response: &Response) -> Value {
        assert!(
            !response.is_error(),
            "respuesta con error inesperado: {:?}",
            response.error
        );
        response.result.clone().expect("result presente")
    }

    #[test]
    fn ping_returns_pong_version_and_state() {
        let state = ServerState::with_default_config();
        let response = call(&state, r#"{"jsonrpc":"2.0","method":"jarvis.ping","id":1}"#);
        assert_eq!(response.id, Some(crate::protocol::Id::Number(1)));
        let ping: PingResult = serde_json::from_value(result_of(&response)).expect("result válido");
        assert!(ping.pong);
        assert_eq!(ping.status, "ok");
        assert_eq!(ping.version, SERVER_VERSION);
        assert_eq!(ping.state, "idle");
        assert_eq!(ping.voice, "es-CL-LorenzoNeural");
    }

    #[test]
    fn get_state_reports_voice_mute_and_swarm() {
        let state = ServerState::with_default_config();
        let response = call(
            &state,
            r#"{"jsonrpc":"2.0","method":"jarvis.getState","id":2}"#,
        );
        let result: StateResult =
            serde_json::from_value(result_of(&response)).expect("result válido");
        assert_eq!(result.state, "idle");
        assert!(!result.muted);
        assert_eq!(result.voice_lang, "es-CL");
        assert_eq!(result.swarm_in_flight, 0);
        assert_eq!(result.routing, None, "sin orquestador no hay métricas");
    }

    #[test]
    fn get_state_exposes_routing_metrics() {
        let state = ServerState::with_default_config();
        let metrics = RoutingMetrics {
            routed_spawn: 3,
            routed_foreground: 7,
            planner_fallbacks: 1,
            planner_latency_ms: 840,
        };
        state.set_routing_metrics(Arc::new(move || metrics));
        let response = call(
            &state,
            r#"{"jsonrpc":"2.0","method":"jarvis.getState","id":2}"#,
        );
        let raw = result_of(&response);
        assert_eq!(raw["routing"]["routed_spawn"], 3);
        assert_eq!(raw["routing"]["planner_latency_ms"], 840);
        let result: StateResult = serde_json::from_value(raw).expect("result válido");
        assert_eq!(result.routing, Some(metrics));
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let state = ServerState::with_default_config();
        let response = call(
            &state,
            r#"{"jsonrpc":"2.0","method":"jarvis.noExiste","id":3}"#,
        );
        let error = response.error.expect("debe haber error");
        assert_eq!(error.code, error_codes::METHOD_NOT_FOUND);
        assert!(error.message.contains("jarvis.noExiste"));
    }

    #[test]
    fn parse_error_returns_null_id() {
        let state = ServerState::with_default_config();
        let response = call(&state, "{esto no es json");
        assert_eq!(response.id, None);
        let error = response.error.expect("debe haber error");
        assert_eq!(error.code, error_codes::PARSE_ERROR);
    }

    #[test]
    fn invalid_params_are_rejected() {
        let state = ServerState::with_default_config();
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.setMuted","#,
            r#""params":{"muted":"sí"},"id":4}"#,
        );
        let error = call(&state, line).error.expect("debe haber error");
        assert_eq!(error.code, error_codes::INVALID_PARAMS);
    }

    #[test]
    fn notification_yields_no_response() {
        let state = ServerState::with_default_config();
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.notifyTriageAlert","#,
            r#""params":{"item_id":"triage-1","title":"Fallo PDF","#,
            r#""source":"slack","priority":"high"}}"#,
        );
        assert!(
            state.handle_line(line).is_none(),
            "una notificación no debe producir respuesta (I2)"
        );
        let alerts = state.alerts();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].item_id, "triage-1");
        assert_eq!(alerts[0].priority.as_deref(), Some("high"));
    }

    #[test]
    fn notify_as_request_is_acknowledged() {
        let state = ServerState::with_default_config();
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.notifyTriageAlert","#,
            r#""params":{"item_id":"t-9","title":"X","source":"gmail"},"#,
            r#""id":8}"#,
        );
        let ack: Ack = serde_json::from_value(result_of(&call(&state, line))).expect("ack");
        assert!(ack.accepted);
        assert_eq!(ack.method, methods::NOTIFY_TRIAGE_ALERT);
    }

    #[test]
    fn set_muted_toggles_state_and_is_idempotent() {
        let state = ServerState::with_default_config();
        let mute = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.setMuted","#,
            r#""params":{"muted":true},"id":5}"#,
        );
        let first: SetMutedResult =
            serde_json::from_value(result_of(&call(&state, mute))).expect("result válido");
        assert!(first.muted);
        assert_eq!(first.state, "muted");
        assert!(state.is_muted());

        let second: SetMutedResult =
            serde_json::from_value(result_of(&call(&state, mute))).expect("idempotente");
        assert!(second.muted, "repetir el mute no debe fallar");

        let unmute = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.setMuted","#,
            r#""params":{"muted":false},"id":6}"#,
        );
        let third: SetMutedResult =
            serde_json::from_value(result_of(&call(&state, unmute))).expect("unmute");
        assert!(!third.muted);
        assert_eq!(third.state, "idle");
    }

    #[test]
    fn speak_is_rejected_while_muted() {
        let state = ServerState::with_default_config();
        state.set_muted(true).expect("mute");
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.speak","#,
            r#""params":{"text":"hola"},"id":7}"#,
        );
        let result: SpeakResult =
            serde_json::from_value(result_of(&call(&state, line))).expect("result válido");
        assert!(!result.spoken);
        assert!(result.muted);
        assert_eq!(result.reason.as_deref(), Some("muted"));
        assert!(state.utterances().is_empty());
    }

    #[test]
    fn speak_records_utterance_and_calls_sink() {
        let sink = Arc::new(CountingSink {
            calls: AtomicUsize::new(0),
        });
        let state = ServerState::with_default_config().with_speech_sink(sink.clone());
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.speak","#,
            r#""params":{"text":"Dame un segundo","category":"dev"},"id":9}"#,
        );
        let result: SpeakResult =
            serde_json::from_value(result_of(&call(&state, line))).expect("result válido");
        assert!(result.spoken);
        assert_eq!(result.category.as_deref(), Some("dev"));
        assert_eq!(state.utterances().len(), 1);
        assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
    }

    /// Sumidero con cola: devuelve posiciones crecientes.
    struct QueueSink {
        queued: Mutex<Vec<(String, String)>>,
    }

    impl SpeechSink for QueueSink {
        fn speak(&self, _utterance: &Utterance) {}

        fn enqueue(&self, utterance: &Utterance, priority: &str) -> Option<u32> {
            let mut queued = lock(&self.queued);
            queued.push((utterance.text.clone(), priority.to_string()));
            Some(queued.len() as u32)
        }
    }

    /// Puerto de tareas en memoria: `dispatch` deja la tarea `running` y
    /// `finish` la marca `done`.
    #[derive(Default)]
    struct MemoryTasks {
        tasks: Mutex<BTreeMap<u64, Value>>,
        /// Instrucciones recibidas por `send_session` (HU-66).
        sent: Mutex<Vec<(u64, String)>>,
        /// Detalle en markdown por id (HU-66).
        details: Mutex<BTreeMap<u64, String>>,
    }

    impl MemoryTasks {
        fn finish(&self, id: u64) {
            if let Some(task) = lock(&self.tasks).get_mut(&id) {
                task["status"] = json!("done");
            }
        }
    }

    /// Puerto que delega en [`MemoryTasks`] sin sub-sesiones: usa las
    /// implementaciones por defecto de `send_session` y `read_detail`.
    #[derive(Default)]
    struct TasksOnly(MemoryTasks);

    impl TaskPort for TasksOnly {
        fn dispatch(&self, title: String, prompt: String) -> u64 {
            self.0.dispatch(title, prompt)
        }

        fn list(&self) -> Vec<Value> {
            self.0.list()
        }

        fn get(&self, id: u64) -> Option<Value> {
            self.0.get(id)
        }

        fn cancel(&self, id: u64) -> bool {
            self.0.cancel(id)
        }
    }

    impl TaskPort for MemoryTasks {
        fn dispatch(&self, title: String, prompt: String) -> u64 {
            let mut tasks = lock(&self.tasks);
            let id = tasks.len() as u64 + 1;
            tasks.insert(
                id,
                json!({"id": id, "title": title, "prompt": prompt, "status": "running"}),
            );
            id
        }

        fn list(&self) -> Vec<Value> {
            lock(&self.tasks).values().cloned().collect()
        }

        fn get(&self, id: u64) -> Option<Value> {
            lock(&self.tasks).get(&id).cloned()
        }

        fn cancel(&self, id: u64) -> bool {
            match lock(&self.tasks).get_mut(&id) {
                Some(task) if task["status"] == "running" => {
                    task["status"] = json!("cancelled");
                    true
                }
                _ => false,
            }
        }

        fn send_session(&self, id: u64, instruction: String) -> Result<(), String> {
            lock(&self.sent).push((id, instruction));
            Ok(())
        }

        fn read_detail(&self, id: u64) -> Option<String> {
            lock(&self.details).get(&id).cloned()
        }
    }

    #[test]
    fn sessions_round_trip_over_socket_hu66() {
        let port = Arc::new(MemoryTasks::default());
        let state = Arc::new(ServerState::with_default_config().with_task_port(port.clone()));
        let mut server = IpcServer::with_path(temp_socket_path("hu66"), Arc::clone(&state));
        server.start().expect("start");
        let mut client = crate::client::IpcClient::connect(server.socket_path()).expect("connect");

        // Compatibilidad (CA-2): los clientes de HU-37 abren el hilo igual que antes.
        let dispatched = client
            .dispatch_task("revisión de PRs", "revisa las PR de Felipe")
            .expect("dispatchTask");
        let id = dispatched.id;
        assert_eq!(client.get_tasks().expect("getTasks").tasks.len(), 1);

        let sessions = client.list_sessions().expect("sessions.list");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["title"], "revisión de PRs");
        assert_eq!(
            client.get_session(id).expect("sessions.get"),
            client.get_task(id).expect("getTask"),
            "sessions.get y getTask comparten carga útil"
        );

        assert!(client
            .send_session(id, "  revisa también la PR 214  ")
            .expect("sessions.send"));
        assert_eq!(
            lock(&port.sent).as_slice(),
            &[(id, "revisa también la PR 214".to_string())],
            "la instrucción llega recortada al puerto"
        );

        lock(&port.details).insert(id, "# PR 214\n\nSin hallazgos graves.".into());
        assert_eq!(
            client.read_session(id).expect("sessions.read"),
            "# PR 214\n\nSin hallazgos graves."
        );

        assert!(client.cancel_session(id).expect("sessions.cancel"));
        assert!(
            !client.cancel_session(id).expect("cancel repetido"),
            "cancelar un hilo terminado no es error"
        );

        let events = state.session_events();
        let kinds: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(
            kinds,
            [session_event::INSTRUCTION, session_event::CANCELLED],
            "un evento por envío aceptado y por cancelación efectiva"
        );
        assert_eq!(events[0].title.as_deref(), Some("revisión de PRs"));
        assert_eq!(events[1].status.as_deref(), Some("cancelled"));
        server.shutdown();
    }

    #[test]
    fn sessions_reject_unknown_ids_and_empty_instructions_hu66() {
        let port = Arc::new(MemoryTasks::default());
        let state = ServerState::with_default_config().with_task_port(port.clone());
        port.dispatch("x".into(), "y".into());
        let cases = [
            r#"{"jsonrpc":"2.0","method":"jarvis.sessions.get","params":{"id":99},"id":1}"#,
            r#"{"jsonrpc":"2.0","method":"jarvis.sessions.cancel","params":{"id":99},"id":2}"#,
            r#"{"jsonrpc":"2.0","method":"jarvis.sessions.read","params":{"id":99},"id":3}"#,
            r#"{"jsonrpc":"2.0","method":"jarvis.sessions.send","params":{"id":99,"instruction":"a"},"id":4}"#,
            r#"{"jsonrpc":"2.0","method":"jarvis.sessions.send","params":{"id":1,"instruction":"   "},"id":5}"#,
            // Existe pero aún no tiene detalle.
            r#"{"jsonrpc":"2.0","method":"jarvis.sessions.read","params":{"id":1},"id":6}"#,
            r#"{"jsonrpc":"2.0","method":"jarvis.sessions.get","id":7}"#,
        ];
        for line in cases {
            let error = call(&state, line).error.expect(line);
            assert_eq!(error.code, error_codes::INVALID_PARAMS, "{line}");
        }
        let missing = call(&state, cases[1]).error.expect("error");
        assert!(missing.message.contains("sesión 99"), "{}", missing.message);
        assert!(lock(&port.sent).is_empty(), "nada llegó al puerto");
        assert!(state.session_events().is_empty(), "sin eventos por errores");
    }

    #[test]
    fn sessions_fail_explicitly_without_port_or_support_hu66() {
        let bare = ServerState::with_default_config();
        let list = r#"{"jsonrpc":"2.0","method":"jarvis.sessions.list","id":1}"#;
        assert_eq!(
            call(&bare, list).error.expect("sin puerto").code,
            error_codes::INTERNAL_ERROR
        );

        let port = Arc::new(TasksOnly::default());
        let state = ServerState::with_default_config().with_task_port(port.clone());
        port.dispatch("x".into(), "y".into());
        let send = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.sessions.send","#,
            r#""params":{"id":1,"instruction":"sigue"},"id":2}"#,
        );
        let error = call(&state, send).error.expect("send no soportado");
        assert_eq!(error.code, error_codes::INTERNAL_ERROR);
        assert!(
            error.message.contains(SEND_SESSION_UNSUPPORTED),
            "{}",
            error.message
        );
        assert!(state.session_events().is_empty(), "no se finge el envío");

        let read = r#"{"jsonrpc":"2.0","method":"jarvis.sessions.read","params":{"id":1},"id":3}"#;
        assert_eq!(
            call(&state, read).error.expect("sin detalle").code,
            error_codes::INVALID_PARAMS
        );
    }

    #[test]
    fn session_event_notification_omits_unknown_fields_and_is_bounded_hu66() {
        let state = ServerState::with_default_config();
        let notification = state.emit_session_event(SessionEventParams {
            id: 3,
            event: session_event::REPLIED.to_string(),
            title: None,
            status: Some("idle".into()),
            detail: None,
        });
        assert_eq!(notification.method, methods::NOTIFY_SESSION_EVENT);
        let line = serde_json::to_string(&notification).expect("serializa");
        assert!(!line.contains("null"), "{line}");
        let wire: Value = serde_json::from_str(&line).expect("JSON válido");
        assert!(
            wire.get("id").is_none(),
            "una notificación no lleva id: {line}"
        );
        assert_eq!(
            notification.params,
            Some(json!({"id": 3, "event": "replied", "status": "idle"}))
        );

        for id in 0..(SESSION_EVENTS_CAPACITY as u64 + 10) {
            state.emit_session_event(SessionEventParams {
                id,
                event: session_event::PROGRESS.to_string(),
                title: None,
                status: None,
                detail: None,
            });
        }
        let events = state.session_events();
        assert_eq!(events.len(), SESSION_EVENTS_CAPACITY);
        assert_eq!(
            events.last().map(|e| e.id),
            Some(SESSION_EVENTS_CAPACITY as u64 + 9)
        );
    }

    #[test]
    fn dispatch_get_and_cancel_tasks() {
        let port = Arc::new(MemoryTasks::default());
        let state = ServerState::with_default_config().with_task_port(port.clone());
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.dispatchTask","#,
            r#""params":{"title":"tus correos","prompt":"revisa mis correos"},"id":1}"#,
        );
        let dispatched: DispatchTaskResult =
            serde_json::from_value(result_of(&call(&state, line))).expect("dispatch");
        assert_eq!(dispatched.id, 1);
        assert_eq!(dispatched.status, "queued");

        let list = r#"{"jsonrpc":"2.0","method":"jarvis.getTasks","id":2}"#;
        let tasks: TasksResult = serde_json::from_value(result_of(&call(&state, list))).unwrap();
        assert_eq!(tasks.tasks.len(), 1);
        assert_eq!(tasks.tasks[0]["status"], "running");

        port.finish(1);
        let get = r#"{"jsonrpc":"2.0","method":"jarvis.getTask","params":{"id":1},"id":3}"#;
        assert_eq!(result_of(&call(&state, get))["status"], "done");

        // Cancelar una tarea terminada: existe pero ya no se cancela.
        let cancel = r#"{"jsonrpc":"2.0","method":"jarvis.cancelTask","params":{"id":1},"id":4}"#;
        let result: CancelTaskResult =
            serde_json::from_value(result_of(&call(&state, cancel))).unwrap();
        assert!(!result.cancelled);

        let missing = r#"{"jsonrpc":"2.0","method":"jarvis.cancelTask","params":{"id":99},"id":5}"#;
        assert_eq!(
            call(&state, missing).error.expect("error").code,
            error_codes::INVALID_PARAMS
        );
        let missing = r#"{"jsonrpc":"2.0","method":"jarvis.getTask","params":{"id":99},"id":6}"#;
        assert_eq!(
            call(&state, missing).error.expect("error").code,
            error_codes::INVALID_PARAMS
        );
    }

    #[test]
    fn dispatch_task_validates_prompt_and_requires_a_port() {
        let state = ServerState::with_default_config();
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.dispatchTask","#,
            r#""params":{"title":"x","prompt":"hola"},"id":1}"#,
        );
        assert_eq!(
            call(&state, line).error.expect("sin puerto").code,
            error_codes::INTERNAL_ERROR
        );
        let state = state.with_task_port(Arc::new(MemoryTasks::default()));
        let empty = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.dispatchTask","#,
            r#""params":{"title":"x","prompt":"  "},"id":2}"#,
        );
        assert_eq!(
            call(&state, empty).error.expect("prompt vacío").code,
            error_codes::INVALID_PARAMS
        );
    }

    #[test]
    fn speak_with_queue_returns_position_even_while_muted() {
        let sink = Arc::new(QueueSink {
            queued: Mutex::new(Vec::new()),
        });
        let state = ServerState::with_default_config().with_speech_sink(sink.clone());
        state.set_muted(true).expect("mute");
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.speak","#,
            r#""params":{"text":"build listo","queue":true,"priority":"task_result"},"id":1}"#,
        );
        let result: SpeakResult = serde_json::from_value(result_of(&call(&state, line))).unwrap();
        assert_eq!(result.queued, Some(true));
        assert_eq!(result.position, Some(1));
        assert!(!result.spoken);
        assert_eq!(
            lock(&sink.queued).clone(),
            vec![("build listo".to_string(), "task_result".to_string())]
        );

        let bad = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.speak","#,
            r#""params":{"text":"x","queue":true,"priority":"urgente"},"id":2}"#,
        );
        assert_eq!(
            call(&state, bad).error.expect("prioridad inválida").code,
            error_codes::INVALID_PARAMS
        );
    }

    #[test]
    fn speak_without_queue_keeps_legacy_fields_absent() {
        let state = ServerState::with_default_config();
        let line = r#"{"jsonrpc":"2.0","method":"jarvis.speak","params":{"text":"hola"},"id":1}"#;
        let value = result_of(&call(&state, line));
        assert!(value.get("queued").is_none() && value.get("position").is_none());
    }

    #[test]
    fn speak_with_empty_text_is_rejected() {
        let state = ServerState::with_default_config();
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.speak","#,
            r#""params":{"text":"   "},"id":10}"#,
        );
        let error = call(&state, line).error.expect("debe haber error");
        assert_eq!(error.code, error_codes::INVALID_PARAMS);
    }

    #[test]
    fn dispatch_and_complete_swarm_task() {
        let state = ServerState::with_default_config();
        let dispatch = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.dispatchSwarm","#,
            r#""params":{"task_id":"task-102","agent":"dev","#,
            r#""goal":"Refactor de login"},"id":11}"#,
        );
        let task: SwarmTask =
            serde_json::from_value(result_of(&call(&state, dispatch))).expect("result válido");
        assert_eq!(task.status, task_status::DISPATCHED);
        assert_eq!(state.in_flight(), 1);

        let status_line = r#"{"jsonrpc":"2.0","method":"jarvis.getSwarmStatus","id":12}"#;
        let status: SwarmStatusResult =
            serde_json::from_value(result_of(&call(&state, status_line))).expect("result válido");
        assert_eq!(status.in_flight, 1);
        assert_eq!(status.tasks.len(), 1);
        assert_eq!(status.tasks[0].task_id, "task-102");

        // Forma del SDD §3.1 (camelCase + tests) enviada como notificación.
        let completed = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.notifySwarmCompleted","#,
            r#""params":{"taskId":"task-102","#,
            r#""summary":"Refactor completado","#,
            r#""testsPassed":43,"testsFailed":0}}"#,
        );
        assert!(state.handle_line(completed).is_none());
        assert_eq!(state.in_flight(), 0, "la tarea ya no está en vuelo");

        let tasks = state.swarm_tasks();
        assert_eq!(tasks[0].status, task_status::COMPLETED);
        assert_eq!(tasks[0].summary.as_deref(), Some("Refactor completado"));
    }

    #[test]
    fn completing_unknown_task_registers_it() {
        let state = ServerState::with_default_config();
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.notifySwarmCompleted","#,
            r#""params":{"task_id":"task-404","status":"failed","#,
            r#""summary":"boom"}}"#,
        );
        assert!(state.handle_line(line).is_none());
        let tasks = state.swarm_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, task_status::FAILED);
        assert_eq!(tasks[0].goal, "boom");
    }

    #[test]
    fn dispatch_swarm_requires_task_id() {
        let state = ServerState::with_default_config();
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.dispatchSwarm","#,
            r#""params":{"task_id":"  ","agent":"dev","goal":"x"},"#,
            r#""id":14}"#,
        );
        let error = call(&state, line).error.expect("debe haber error");
        assert_eq!(error.code, error_codes::INVALID_PARAMS);
    }

    #[test]
    fn swarm_progress_is_recorded() {
        let state = ServerState::with_default_config();
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.notifySwarmProgress","#,
            r#""params":{"taskId":"task-1","role":"dev","percent":40,"#,
            r#""message":"escribiendo tests"}}"#,
        );
        assert!(state.handle_line(line).is_none());
        let events = state.progress_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].percent, Some(40));
        assert_eq!(events[0].role.as_deref(), Some("dev"));
    }

    #[test]
    fn hello_registers_session_and_lists_capabilities() {
        let state = ServerState::with_default_config();
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.hello","#,
            r#""params":{"client":"fokkus-ide","clientVersion":"2026.10","#,
            r#""protocolVersion":1},"id":15}"#,
        );
        let result: HelloResult =
            serde_json::from_value(result_of(&call(&state, line))).expect("result válido");
        assert_eq!(result.protocol_version, PROTOCOL_VERSION);
        assert_eq!(result.capabilities.len(), methods::ALL.len());
        let session = state.hello_session().expect("sesión registrada");
        assert_eq!(session.client, "fokkus-ide");
        assert_eq!(session.client_version.as_deref(), Some("2026.10"));
    }

    #[test]
    fn hello_with_wrong_protocol_version_is_rejected() {
        let state = ServerState::with_default_config();
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.hello","#,
            r#""params":{"client":"fokkus-ide","protocolVersion":99},"#,
            r#""id":16}"#,
        );
        let error = call(&state, line).error.expect("debe haber error");
        assert_eq!(error.code, error_codes::PROTOCOL_VERSION_MISMATCH);
        assert_eq!(error.data, Some(json!({ "serverProtocolVersion": 1 })));
        assert!(state.hello_session().is_none());
    }

    #[test]
    fn state_name_covers_every_state() {
        let names: Vec<&str> = JarvisState::ALL.iter().copied().map(state_name).collect();
        assert_eq!(
            names,
            vec!["idle", "listening", "thinking", "speaking", "muted"]
        );
    }

    #[test]
    fn default_peer_policy_is_socket_permissions() {
        let server = IpcServer::new(Arc::new(ServerState::with_default_config()));
        assert_eq!(server.peer_policy(), PeerPolicy::SocketPermissions);
    }

    #[test]
    fn peer_credentials_policy_is_reported_as_unavailable() {
        let mut server = IpcServer::with_path(
            temp_socket_path("peercred"),
            Arc::new(ServerState::with_default_config()),
        );
        server.set_peer_policy(PeerPolicy::PeerCredentials);
        let error = server.bind().expect_err("no disponible en stable");
        assert!(matches!(error, IpcError::PeerCredentialsUnavailable));
        assert!(error.to_string().contains("libc"));
    }

    #[test]
    fn bind_creates_private_socket_and_drop_removes_it() {
        let path = temp_socket_path("bind");
        let dir = path.parent().expect("padre").to_path_buf();
        let _ = fs::remove_dir_all(&dir);

        let mut server =
            IpcServer::with_path(path.clone(), Arc::new(ServerState::with_default_config()));
        server.bind().expect("bind");
        assert!(path.exists());

        let socket_mode = fs::metadata(&path).expect("metadata").mode() & 0o777;
        assert_eq!(socket_mode, 0o600, "I6: el socket debe ser 0600");
        let dir_mode = fs::metadata(&dir).expect("metadata dir").mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "I6: el directorio debe ser 0700");

        drop(server);
        assert!(!path.exists(), "Drop debe borrar el socket");
    }

    #[test]
    fn second_bind_reports_already_running() {
        let path = temp_socket_path("running");
        let _ = fs::remove_dir_all(path.parent().expect("padre"));

        let mut first =
            IpcServer::with_path(path.clone(), Arc::new(ServerState::with_default_config()));
        first.bind().expect("primer bind");

        let mut second =
            IpcServer::with_path(path.clone(), Arc::new(ServerState::with_default_config()));
        let error = second.bind().expect_err("debe rechazarse");
        assert!(matches!(error, IpcError::AlreadyRunning(_)));

        // El socket del primero sigue intacto tras el intento fallido.
        assert!(path.exists());
        drop(first);
        assert!(!path.exists());
    }

    #[test]
    fn stale_socket_is_removed_and_rebound() {
        let path = temp_socket_path("stale");
        let dir = path.parent().expect("padre").to_path_buf();
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        // Un archivo normal (no un socket) simula un socket huérfano.
        fs::write(&path, b"stale").expect("archivo stale");

        let mut server =
            IpcServer::with_path(path.clone(), Arc::new(ServerState::with_default_config()));
        server.bind().expect("bind tras limpiar el stale");
        assert!(path.exists());
        drop(server);
    }

    #[test]
    fn start_is_idempotent_and_shutdown_joins() {
        let mut server = IpcServer::with_path(
            temp_socket_path("start"),
            Arc::new(ServerState::with_default_config()),
        );
        server.start().expect("start");
        server.start().expect("start repetido es no-op");
        assert!(server.is_running());
        assert!(server.socket_path().exists());
        server.shutdown();
        assert!(!server.is_running());
        server.shutdown();
    }

    #[test]
    fn connection_answers_two_requests_on_one_socket() {
        let mut server = IpcServer::with_path(
            temp_socket_path("e2e"),
            Arc::new(ServerState::with_default_config()),
        );
        server.start().expect("start");

        let mut client = UnixStream::connect(server.socket_path()).expect("connect");
        let request = "{\"jsonrpc\":\"2.0\",\"method\":\"jarvis.ping\",\"id\":1}\n";
        client.write_all(request.as_bytes()).expect("write");
        client.flush().expect("flush");

        let mut reader = BufReader::new(client.try_clone().expect("clone"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        let response: Response = serde_json::from_str(line.trim()).expect("JSON válido");
        let ping: PingResult =
            serde_json::from_value(response.result.expect("result")).expect("PingResult válido");
        assert!(ping.pong);

        // Segunda petición por la misma conexión (keep-alive NDJSON).
        let second = "{\"jsonrpc\":\"2.0\",\"method\":\"jarvis.getState\",\"id\":2}\n";
        client.write_all(second.as_bytes()).expect("write");
        line.clear();
        reader.read_line(&mut line).expect("read");
        let response: Response = serde_json::from_str(line.trim()).expect("JSON válido");
        assert_eq!(response.id, Some(crate::protocol::Id::Number(2)));
    }

    #[test]
    fn oversized_message_is_reported_or_connection_closed() {
        let mut server = IpcServer::with_path(
            temp_socket_path("oversize"),
            Arc::new(ServerState::with_default_config()),
        );
        server.start().expect("start");

        let mut client = UnixStream::connect(server.socket_path()).expect("connect");
        let giant = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"jarvis.ping\",\"id\":1,\"x\":\"{}\"}}\n",
            "a".repeat(MAX_MESSAGE_BYTES + 16)
        );
        // El servidor puede cerrar antes de terminar de escribir: se ignora el
        // error de escritura truncada.
        let _ = client.write_all(giant.as_bytes());
        let _ = client.flush();

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        if !line.trim().is_empty() {
            let response: Response = serde_json::from_str(line.trim()).expect("JSON válido");
            let error = response.error.expect("debe reportar el exceso de tamaño");
            assert_eq!(error.code, error_codes::INVALID_REQUEST);
            assert_eq!(response.id, None);
        }
    }
}
