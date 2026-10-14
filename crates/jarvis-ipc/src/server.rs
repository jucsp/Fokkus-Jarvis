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

use jarvis_core::{JarvisConfig, JarvisCore, JarvisState};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use crate::paths;
#[cfg(unix)]
use crate::protocol::MAX_MESSAGE_BYTES;
use crate::protocol::{
    error_codes, methods, task_status, Ack, DispatchSwarmParams, HelloParams, HelloResult,
    HelloSession, Incoming, JsonRpcError, NotifySwarmCompletedParams, NotifySwarmProgressParams,
    NotifyTriageAlertParams, PingResult, Response, SetMutedParams, SetMutedResult, SpeakParams,
    SpeakResult, StateResult, SwarmStatusResult, SwarmTask, TriageAlert, Utterance,
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
}

/// Estado compartido que atiende las llamadas del protocolo.
///
/// Es seguro de compartir entre los hilos de conexión (`Arc`). Nunca hace
/// *panic* por envenenamiento de `Mutex`: los locks se recuperan del error de
/// veneno con [`std::sync::PoisonError::into_inner`].
pub struct ServerState {
    core: Mutex<JarvisCore>,
    swarm: Mutex<BTreeMap<String, SwarmTask>>,
    progress: Mutex<Vec<NotifySwarmProgressParams>>,
    alerts: Mutex<Vec<TriageAlert>>,
    utterances: Mutex<Vec<Utterance>>,
    sink: Mutex<Option<Arc<dyn SpeechSink>>>,
    hello: Mutex<Option<HelloSession>>,
}

impl ServerState {
    /// Crea el estado a partir de un núcleo ya construido.
    pub fn new(core: JarvisCore) -> Self {
        Self {
            core: Mutex::new(core),
            swarm: Mutex::new(BTreeMap::new()),
            progress: Mutex::new(Vec::new()),
            alerts: Mutex::new(Vec::new()),
            utterances: Mutex::new(Vec::new()),
            sink: Mutex::new(None),
            hello: Mutex::new(None),
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

    /// Estado actual del asistente.
    pub fn state(&self) -> JarvisState {
        lock(&self.core).state()
    }

    /// `true` si el asistente está silenciado.
    pub fn is_muted(&self) -> bool {
        lock(&self.core).is_muted()
    }

    /// Par voz activa (`voice_id`, `voice_lang`).
    pub fn voice(&self) -> (String, String) {
        let core = lock(&self.core);
        (
            core.config().voice_id.clone(),
            core.config().voice_lang.clone(),
        )
    }

    /// Cambia el silencio global; es idempotente.
    pub fn set_muted(&self, muted: bool) -> Result<SetMutedResult, JsonRpcError> {
        let mut core = lock(&self.core);
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
        let muted = self.is_muted();
        if muted {
            return Ok(SpeakResult {
                spoken: false,
                muted,
                text: params.text,
                category: params.category,
                reason: Some("muted".to_string()),
            });
        }
        let utterance = Utterance {
            text: params.text.clone(),
            category: params.category.clone(),
        };
        lock(&self.utterances).push(utterance.clone());
        // Se clona el Arc para no retener el lock durante la locución.
        let sink = lock(&self.sink).as_ref().map(Arc::clone);
        if let Some(sink) = sink {
            sink.speak(&utterance);
        }
        Ok(SpeakResult {
            spoken: true,
            muted,
            text: params.text,
            category: params.category,
            reason: None,
        })
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
            other => Err(JsonRpcError::new(
                error_codes::METHOD_NOT_FOUND,
                format!("método desconocido: {other}"),
            )),
        }
    }
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
