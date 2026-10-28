//! Protocolo JSON-RPC 2.0 de Fokkus-Jarvis sobre Unix Domain Socket.
//!
//! Implementa el contrato del Issue #5 con las enmiendas del dictamen QA:
//!
//! * **I1 (framing):** NDJSON — un objeto JSON por línea, terminador `\n`,
//!   tamaño máximo [`MAX_MESSAGE_BYTES`] (1 MiB) por mensaje.
//! * **I2:** los `jarvis.notify*` se clasifican como *notifications* (sin `id`,
//!   sin respuesta) cuando el mensaje no trae `id`.
//! * **I3:** catálogo único en [`methods`]. Los parámetros aceptan tanto
//!   `snake_case` (Issue #5) como `camelCase` (SDD §3.1) vía `#[serde(alias)]`.
//! * **I5:** handshake `jarvis.hello` con [`PROTOCOL_VERSION`].
//! * **I8:** códigos de error estándar + los específicos en [`error_codes`].

use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Versión del protocolo JSON-RPC soportada.
pub const JSONRPC_VERSION: &str = "2.0";

/// Versión del catálogo de métodos de Fokkus-Jarvis.
pub const PROTOCOL_VERSION: u32 = 1;

/// Versión del servidor (`jarvis-ipc`), reportada en `jarvis.ping`.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Tamaño máximo de un mensaje NDJSON (enmienda I1).
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Respuesta de último recurso si `serde_json` falla al serializar.
const FALLBACK_ERROR_LINE: &str =
    "{\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32603,\"message\":\"error interno\"},\"id\":null}";

/// Nombres de los métodos del catálogo único (enmienda I3).
pub mod methods {
    /// Handshake de sesión del IDE (I5).
    pub const HELLO: &str = "jarvis.hello";
    /// Verificación de presencia.
    pub const PING: &str = "jarvis.ping";
    /// Estado actual, voz, mute y tareas swarm en vuelo.
    pub const GET_STATE: &str = "jarvis.getState";
    /// Activa/desactiva el silencio global.
    pub const SET_MUTED: &str = "jarvis.setMuted";
    /// Ordena una locución.
    pub const SPEAK: &str = "jarvis.speak";
    /// Registra una tarea del Swarm de desarrollo.
    pub const DISPATCH_SWARM: &str = "jarvis.dispatchSwarm";
    /// Lista de tareas swarm activas.
    pub const GET_SWARM_STATUS: &str = "jarvis.getSwarmStatus";
    /// Progreso de una tarea swarm (notification).
    pub const NOTIFY_SWARM_PROGRESS: &str = "jarvis.notifySwarmProgress";
    /// Fin de una tarea swarm (notification).
    pub const NOTIFY_SWARM_COMPLETED: &str = "jarvis.notifySwarmCompleted";
    /// Alerta de triage (Slack/Gmail) (notification).
    pub const NOTIFY_TRIAGE_ALERT: &str = "jarvis.notifyTriageAlert";
    /// Despacha una tarea pesada en segundo plano (HU-37).
    pub const DISPATCH_TASK: &str = "jarvis.dispatchTask";
    /// Lista las tareas pesadas registradas (HU-37).
    pub const GET_TASKS: &str = "jarvis.getTasks";
    /// Una tarea pesada por id, con su resultado si ya terminó (HU-37).
    pub const GET_TASK: &str = "jarvis.getTask";
    /// Cancela una tarea pesada por id (HU-37).
    pub const CANCEL_TASK: &str = "jarvis.cancelTask";
    /// Lista las sub-sesiones con su estado (HU-66).
    pub const SESSIONS_LIST: &str = "jarvis.sessions.list";
    /// Una sub-sesión por id, con su resultado si ya terminó (HU-66).
    pub const SESSIONS_GET: &str = "jarvis.sessions.get";
    /// Envía una instrucción nueva a una sub-sesión (HU-66).
    pub const SESSIONS_SEND: &str = "jarvis.sessions.send";
    /// Cancela una sub-sesión por id (HU-66).
    pub const SESSIONS_CANCEL: &str = "jarvis.sessions.cancel";
    /// Detalle en markdown de una sub-sesión (HU-66).
    pub const SESSIONS_READ: &str = "jarvis.sessions.read";
    /// Evento de una sub-sesión (notification) (HU-66).
    pub const NOTIFY_SESSION_EVENT: &str = "jarvis.sessionEvent";

    /// Catálogo completo, para el handshake y para documentación.
    ///
    /// Debe coincidir con el catálogo documentado en
    /// `docs/specs/sdd-contracts/sdd-hu66-ipc-sesiones.md` (lo verifica
    /// `catalog_is_documented_hu66`).
    pub const ALL: [&str; 20] = [
        HELLO,
        PING,
        GET_STATE,
        SET_MUTED,
        SPEAK,
        DISPATCH_SWARM,
        GET_SWARM_STATUS,
        NOTIFY_SWARM_PROGRESS,
        NOTIFY_SWARM_COMPLETED,
        NOTIFY_TRIAGE_ALERT,
        DISPATCH_TASK,
        GET_TASKS,
        GET_TASK,
        CANCEL_TASK,
        SESSIONS_LIST,
        SESSIONS_GET,
        SESSIONS_SEND,
        SESSIONS_CANCEL,
        SESSIONS_READ,
        NOTIFY_SESSION_EVENT,
    ];
}

/// Nombres de los eventos de una sub-sesión en `jarvis.sessionEvent`
/// (HU-66), espejo de `jarvis_audio::sessions::SessionEvent`.
pub mod session_event {
    /// La sub-sesión acaba de abrirse.
    pub const CREATED: &str = "created";
    /// El agente empezó un turno.
    pub const STARTED: &str = "started";
    /// Avance del turno (no se lee por voz).
    pub const PROGRESS: &str = "progress";
    /// Llegó una instrucción nueva al hilo.
    pub const INSTRUCTION: &str = "instruction";
    /// Turno terminado con un resumen hablado.
    pub const REPLIED: &str = "replied";
    /// El agente pide una decisión al usuario.
    pub const QUESTION: &str = "question";
    /// La sub-sesión cambió de estado terminal (cerrada, cancelada, fallida).
    pub const FINISHED: &str = "finished";
    /// El usuario canceló la sub-sesión.
    pub const CANCELLED: &str = "cancelled";

    /// Catálogo completo de eventos.
    pub const ALL: [&str; 8] = [
        CREATED,
        STARTED,
        PROGRESS,
        INSTRUCTION,
        REPLIED,
        QUESTION,
        FINISHED,
        CANCELLED,
    ];
}

/// Códigos de error JSON-RPC (I8).
pub mod error_codes {
    /// JSON inválido.
    pub const PARSE_ERROR: i32 = -32700;
    /// Estructura JSON-RPC inválida.
    pub const INVALID_REQUEST: i32 = -32600;
    /// Método desconocido.
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// Parámetros inválidos.
    pub const INVALID_PARAMS: i32 = -32602;
    /// Error interno.
    pub const INTERNAL_ERROR: i32 = -32603;
    /// Versión mayor del protocolo no compatible (I5).
    pub const PROTOCOL_VERSION_MISMATCH: i32 = -32001;
    /// No hay sesión de Fokkus IDE registrada (I4).
    pub const IDE_NOT_CONNECTED: i32 = -32002;
    /// Demasiadas conexiones concurrentes (I8).
    pub const BUSY: i32 = -32003;
}

/// Identificador de una petición JSON-RPC (número o cadena, según el estándar).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    /// Identificador numérico.
    Number(i64),
    /// Identificador textual.
    String(String),
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Id::Number(n) => write!(f, "{n}"),
            Id::String(s) => write!(f, "{s}"),
        }
    }
}

impl From<i64> for Id {
    fn from(value: i64) -> Self {
        Id::Number(value)
    }
}

impl From<String> for Id {
    fn from(value: String) -> Self {
        Id::String(value)
    }
}

impl From<&str> for Id {
    fn from(value: &str) -> Self {
        Id::String(value.to_string())
    }
}

/// Error JSON-RPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Código numérico (ver [`error_codes`]).
    pub code: i32,
    /// Mensaje legible.
    pub message: String,
    /// Detalle adicional opcional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    /// Construye un error sin `data`.
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Construye un error con `data` adicional.
    pub fn with_data(code: i32, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }
}

impl fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for JsonRpcError {}

/// Petición JSON-RPC (requiere respuesta).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Siempre `"2.0"`.
    pub jsonrpc: String,
    /// Método invocado.
    pub method: String,
    /// Parámetros (objeto o array según el método).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Identificador de la petición.
    pub id: Id,
}

impl Request {
    /// Crea una petición `jsonrpc: "2.0"` con el `id` dado.
    pub fn new(id: impl Into<Id>, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
            id: id.into(),
        }
    }

    /// Parámetros como `Value` (`null` si no se enviaron).
    pub fn params_value(&self) -> Value {
        match &self.params {
            Some(params) => params.clone(),
            None => Value::Null,
        }
    }

    /// Deserializa los parámetros al tipo esperado.
    ///
    /// Devuelve [`error_codes::INVALID_PARAMS`] si falta algún campo requerido.
    pub fn parse_params<T: DeserializeOwned>(&self) -> Result<T, JsonRpcError> {
        serde_json::from_value(self.params_value()).map_err(|e| {
            JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                format!("parámetros inválidos para {}: {e}", self.method),
            )
        })
    }
}

/// Notificación JSON-RPC (sin `id`, sin respuesta).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    /// Siempre `"2.0"`.
    pub jsonrpc: String,
    /// Método notificado.
    pub method: String,
    /// Parámetros de la notificación.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    /// Crea una notificación `jsonrpc: "2.0"`.
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
        }
    }

    /// Parámetros como `Value` (`null` si no se enviaron).
    pub fn params_value(&self) -> Value {
        match &self.params {
            Some(params) => params.clone(),
            None => Value::Null,
        }
    }

    /// Deserializa los parámetros al tipo esperado.
    pub fn parse_params<T: DeserializeOwned>(&self) -> Result<T, JsonRpcError> {
        serde_json::from_value(self.params_value()).map_err(|e| {
            JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                format!("parámetros inválidos para {}: {e}", self.method),
            )
        })
    }
}

/// Respuesta JSON-RPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Siempre `"2.0"`.
    pub jsonrpc: String,
    /// Resultado en caso de éxito.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error en caso de fallo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    /// `id` de la petición respondida (`null` si no se pudo determinar).
    pub id: Option<Id>,
}

impl Response {
    /// Respuesta de éxito.
    pub fn success(id: impl Into<Id>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            result: Some(result),
            error: None,
            id: Some(id.into()),
        }
    }

    /// Respuesta de error; `id` es `None` cuando el mensaje no pudo parsearse.
    pub fn error(id: Option<Id>, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            result: None,
            error: Some(error),
            id,
        }
    }

    /// `true` si la respuesta contiene un error.
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }

    /// Serializa la respuesta como línea NDJSON (con `\n` final).
    ///
    /// La serialización de esta estructura no puede fallar (todos sus campos
    /// son tipos JSON); el `fallback` evita cualquier `unwrap` en producción.
    pub fn to_ndjson(&self) -> String {
        let mut line = match serde_json::to_string(self) {
            Ok(line) => line,
            Err(_) => FALLBACK_ERROR_LINE.to_string(),
        };
        line.push('\n');
        line
    }
}

/// Mensaje entrante ya clasificado.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Petición: requiere respuesta.
    Request(Request),
    /// Notificación: no admite respuesta.
    Notification(Notification),
    /// Respuesta a una petición emitida por nosotros (p. ej. `ide.*`).
    Response(Response),
}

impl Incoming {
    /// Clasifica una línea NDJSON.
    ///
    /// * Sin `method` pero con `result`/`error` → respuesta.
    /// * Con `method` y `id` no nulo → petición.
    /// * Con `method` y sin `id` (o `id: null`) → notificación.
    pub fn parse_line(line: &str) -> Result<Self, JsonRpcError> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Err(JsonRpcError::new(
                error_codes::PARSE_ERROR,
                "mensaje vacío: se esperaba un objeto JSON",
            ));
        }

        let value: Value = serde_json::from_str(trimmed).map_err(|e| {
            JsonRpcError::new(error_codes::PARSE_ERROR, format!("JSON inválido: {e}"))
        })?;

        let object = value.as_object().ok_or_else(|| {
            JsonRpcError::new(
                error_codes::INVALID_REQUEST,
                "el mensaje debe ser un objeto JSON",
            )
        })?;

        let jsonrpc = match object.get("jsonrpc") {
            Some(Value::String(v)) => v.as_str(),
            _ => {
                return Err(JsonRpcError::new(
                    error_codes::INVALID_REQUEST,
                    "falta el campo \"jsonrpc\"",
                ))
            }
        };
        if jsonrpc != JSONRPC_VERSION {
            return Err(JsonRpcError::new(
                error_codes::INVALID_REQUEST,
                format!("se esperaba jsonrpc \"2.0\", recibido \"{jsonrpc}\""),
            ));
        }

        let method = match object.get("method") {
            Some(Value::String(m)) => Some(m.clone()),
            Some(_) => {
                return Err(JsonRpcError::new(
                    error_codes::INVALID_REQUEST,
                    "\"method\" debe ser una cadena",
                ))
            }
            None => None,
        };

        // Un `id` ausente o `null` identifica una notificación.
        let id = match object.get("id") {
            None | Some(Value::Null) => None,
            Some(raw) => Some(serde_json::from_value::<Id>(raw.clone()).map_err(|e| {
                JsonRpcError::new(
                    error_codes::INVALID_REQUEST,
                    format!("\"id\" inválido: {e}"),
                )
            })?),
        };

        let params = object.get("params").cloned();

        match (method, id) {
            (Some(method), Some(id)) => Ok(Incoming::Request(Request {
                jsonrpc: JSONRPC_VERSION.to_string(),
                method,
                params,
                id,
            })),
            (Some(method), None) => Ok(Incoming::Notification(Notification {
                jsonrpc: JSONRPC_VERSION.to_string(),
                method,
                params,
            })),
            (None, Some(_)) => Ok(Incoming::Response(serde_json::from_value(value).map_err(
                |e| {
                    JsonRpcError::new(
                        error_codes::INVALID_REQUEST,
                        format!("respuesta inválida: {e}"),
                    )
                },
            )?)),
            (None, None) => Err(JsonRpcError::new(
                error_codes::INVALID_REQUEST,
                "el mensaje no tiene \"method\" ni \"id\": no es una petición válida",
            )),
        }
    }
}

/// Sesión del IDE registrada mediante `jarvis.hello` (I4, I5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloSession {
    /// Nombre del cliente (p. ej. `fokkus-ide`).
    pub client: String,
    /// Versión del cliente, si la informa.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    /// Versión del protocolo que anuncia el cliente.
    pub protocol_version: u32,
}

/// Parámetros de `jarvis.hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloParams {
    /// Nombre del cliente.
    pub client: String,
    /// Versión del cliente.
    #[serde(default, alias = "clientVersion")]
    pub client_version: Option<String>,
    /// Versión del protocolo del cliente.
    #[serde(default, alias = "protocolVersion")]
    pub protocol_version: u32,
}

/// Resultado de `jarvis.hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    /// Versión del protocolo del servidor.
    pub protocol_version: u32,
    /// Versión del servidor.
    pub version: String,
    /// Catálogo de métodos disponibles.
    pub capabilities: Vec<String>,
}

/// Resultado de `jarvis.ping`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingResult {
    /// Siempre `true` si el servidor está vivo.
    pub pong: bool,
    /// Estado textual (`"ok"`).
    pub status: String,
    /// Versión del servidor.
    pub version: String,
    /// Estado actual del asistente.
    pub state: String,
    /// Voz activa.
    pub voice: String,
}

/// Parámetros de `jarvis.setMuted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetMutedParams {
    /// `true` para silenciar, `false` para reactivar.
    pub muted: bool,
}

/// Resultado de `jarvis.setMuted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetMutedResult {
    /// Estado de silencio tras aplicar el cambio.
    pub muted: bool,
    /// Estado del asistente tras el cambio.
    pub state: String,
}

/// Parámetros de `jarvis.speak`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakParams {
    /// Texto a locutar.
    pub text: String,
    /// Categoría del texto (p. ej. `general`, `dev`, `inbox`).
    #[serde(default)]
    pub category: Option<String>,
    /// `true` = encolar en la cola hablada en vez de hablar/rechazar en el
    /// acto (HU-37). Por defecto `false` (compatibilidad).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<bool>,
    /// Prioridad en la cola: `notice` (defecto), `task_result`, `reply` o
    /// `critical` (ver [`speech_priority`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
}

/// Prioridades aceptadas por `jarvis.speak` con `queue: true`.
pub mod speech_priority {
    pub const NOTICE: &str = "notice";
    pub const TASK_RESULT: &str = "task_result";
    pub const REPLY: &str = "reply";
    pub const CRITICAL: &str = "critical";
    pub const ALL: [&str; 4] = [NOTICE, TASK_RESULT, REPLY, CRITICAL];
}

/// Resultado de `jarvis.speak`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakResult {
    /// `true` si la locución se aceptó.
    pub spoken: bool,
    /// Estado de silencio en el momento de la llamada.
    pub muted: bool,
    /// Texto solicitado.
    pub text: String,
    /// Categoría solicitada.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Motivo por el que se rechazó (`"muted"`), si aplica.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `true` si quedó en la cola hablada (`queue: true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<bool>,
    /// Posición en la cola (1 = la siguiente).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
}

/// Parámetros de `jarvis.dispatchTask` (HU-37).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchTaskParams {
    /// Título corto para el encabezado hablado ("tus correos"). Vacío =
    /// se deriva del prompt.
    #[serde(default)]
    pub title: String,
    /// Instrucción en lenguaje natural para el subagente de fondo.
    pub prompt: String,
}

/// Resultado de `jarvis.dispatchTask`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchTaskResult {
    /// Id estable de la tarea.
    pub id: u64,
    /// Estado inicial (`queued`).
    pub status: String,
}

/// Parámetros de `jarvis.getTask` y `jarvis.cancelTask`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskIdParams {
    pub id: u64,
}

/// Resultado de `jarvis.getTasks`: registros tal como los serializa el
/// gestor de tareas (`id`, `title`, `status`, `spoken_summary`…).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TasksResult {
    pub tasks: Vec<Value>,
}

/// Resultado de `jarvis.cancelTask`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelTaskResult {
    pub id: u64,
    pub cancelled: bool,
}

/// Parámetros de `jarvis.sessions.get`, `jarvis.sessions.cancel` y
/// `jarvis.sessions.read` (HU-66).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIdParams {
    /// Id corto de la sub-sesión.
    pub id: u64,
}

/// Parámetros de `jarvis.sessions.send` (HU-66).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSendParams {
    /// Id corto de la sub-sesión.
    pub id: u64,
    /// Instrucción nueva para ese hilo.
    pub instruction: String,
}

/// Resultado de `jarvis.sessions.list` (HU-66).
///
/// Cada elemento es el registro de la sub-sesión tal como lo serializa su
/// gestor (`id`, `title`, `status`, `spoken_summary`…), con la misma forma que
/// `jarvis.getTasks` mientras las sub-sesiones sean tareas de fondo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionsResult {
    pub sessions: Vec<Value>,
}

/// Resultado de `jarvis.sessions.send` (HU-66).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSendResult {
    pub id: u64,
    /// `true` si la instrucción quedó aceptada.
    pub ok: bool,
}

/// Resultado de `jarvis.sessions.cancel` (HU-66).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCancelResult {
    pub id: u64,
    /// `false` si la sub-sesión ya había terminado (no había nada que cancelar).
    pub cancelled: bool,
}

/// Resultado de `jarvis.sessions.read` (HU-66).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReadResult {
    pub id: u64,
    /// Detalle en markdown del último turno del hilo.
    pub detail: String,
}

/// Parámetros de la notificación `jarvis.sessionEvent` (HU-66).
///
/// Los nombres de `event` están en [`session_event`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEventParams {
    /// Id corto de la sub-sesión.
    pub id: u64,
    /// Tipo de evento (ver [`session_event`]).
    pub event: String,
    /// Título del hilo, si se conoce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Estado del hilo tras el evento, si se conoce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Resumen, avance o pregunta asociados al evento.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Resultado de `jarvis.getState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateResult {
    /// Estado actual del asistente.
    pub state: String,
    /// Silencio global activo.
    pub muted: bool,
    /// Voz activa.
    pub voice: String,
    /// Idioma de la voz.
    #[serde(alias = "voiceLang")]
    pub voice_lang: String,
    /// Número de tareas swarm en vuelo.
    #[serde(alias = "swarmInFlight")]
    pub swarm_in_flight: usize,
    /// Métricas de enrutamiento de turnos (HU-68). Ausente si no hay
    /// orquestador de voz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingMetrics>,
}

/// Contadores de enrutamiento de turnos de voz (HU-68), en `jarvis.getState`.
///
/// Miden en uso real el "a veces sí, a veces no" del router: cuántos turnos
/// abrieron sub-sesión, cuántos respondió el hilo central y cómo le fue al
/// planificador LLM.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingMetrics {
    /// Sub-sesiones abiertas por el router.
    #[serde(alias = "routedSpawn")]
    pub routed_spawn: u64,
    /// Turnos respondidos por el hilo central.
    #[serde(alias = "routedForeground")]
    pub routed_foreground: u64,
    /// Llamadas al planificador que acabaron en la heurística.
    #[serde(alias = "plannerFallbacks")]
    pub planner_fallbacks: u64,
    /// Latencia de la última llamada al planificador, en ms.
    #[serde(alias = "plannerLatencyMs")]
    pub planner_latency_ms: u64,
}

/// Parámetros de `jarvis.dispatchSwarm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchSwarmParams {
    /// Identificador de la tarea.
    #[serde(alias = "taskId")]
    pub task_id: String,
    /// Agente responsable (PO, PM, Dev…).
    pub agent: String,
    /// Objetivo en lenguaje natural.
    pub goal: String,
}

/// Estados posibles de una tarea swarm.
pub mod task_status {
    /// Despachada, aún sin resultado.
    pub const DISPATCHED: &str = "dispatched";
    /// Completada con éxito.
    pub const COMPLETED: &str = "completed";
    /// Completada con fallo.
    pub const FAILED: &str = "failed";
}

/// Tarea del Swarm de desarrollo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmTask {
    /// Identificador de la tarea.
    pub task_id: String,
    /// Agente responsable.
    pub agent: String,
    /// Objetivo de la tarea.
    pub goal: String,
    /// Estado (`dispatched`, `completed`, `failed`).
    pub status: String,
    /// Resumen final, si ya terminó.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Resultado de `jarvis.getSwarmStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmStatusResult {
    /// Tareas registradas (ordenadas por `task_id`).
    pub tasks: Vec<SwarmTask>,
    /// Número de tareas en vuelo.
    pub in_flight: usize,
}

/// Parámetros de `jarvis.notifySwarmProgress`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifySwarmProgressParams {
    /// Identificador de la tarea.
    #[serde(alias = "taskId")]
    pub task_id: String,
    /// Rol que reporta.
    #[serde(default)]
    pub role: Option<String>,
    /// Porcentaje de avance (0..=100).
    #[serde(default)]
    pub percent: Option<u8>,
    /// Mensaje de avance.
    #[serde(default)]
    pub message: Option<String>,
}

/// Parámetros de `jarvis.notifySwarmCompleted`.
///
/// Acepta las dos formas del contrato: la del Issue #5
/// (`{ task_id, status, summary }`) y la del SDD §3.1
/// (`{ taskId, summary, testsPassed, testsFailed }`). Enmienda I3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifySwarmCompletedParams {
    /// Identificador de la tarea.
    #[serde(alias = "taskId")]
    pub task_id: String,
    /// Estado textual (`completed`/`failed`). Opcional si se envían los tests.
    #[serde(default)]
    pub status: Option<String>,
    /// Resumen del resultado.
    #[serde(default, alias = "goal")]
    pub summary: String,
    /// Tests pasados (SDD §3.1).
    #[serde(default, alias = "testsPassed")]
    pub tests_passed: Option<u32>,
    /// Tests fallidos (SDD §3.1).
    #[serde(default, alias = "testsFailed")]
    pub tests_failed: Option<u32>,
}

impl NotifySwarmCompletedParams {
    /// Estado resuelto: usa `status` si viene; si no, lo deduce de los tests.
    pub fn resolved_status(&self) -> String {
        if let Some(status) = &self.status {
            return status.clone();
        }
        match self.tests_failed {
            Some(failed) if failed > 0 => task_status::FAILED.to_string(),
            Some(_) => task_status::COMPLETED.to_string(),
            None => task_status::COMPLETED.to_string(),
        }
    }

    /// Resumen resuelto: si no hay `summary`, se compone con los tests.
    pub fn resolved_summary(&self) -> String {
        if !self.summary.is_empty() {
            return self.summary.clone();
        }
        if self.tests_passed.is_some() || self.tests_failed.is_some() {
            let passed = self.tests_passed.unwrap_or(0);
            let failed = self.tests_failed.unwrap_or(0);
            return format!("tests: {passed} ok / {failed} fallidos");
        }
        String::new()
    }
}

/// Parámetros de `jarvis.notifyTriageAlert`.
///
/// Acepta `{ item_id, title, source }` (Issue #5) y
/// `{ ticketId, source, author, summary, priority }` (SDD §3.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyTriageAlertParams {
    /// Identificador del elemento en el triage inbox.
    #[serde(alias = "ticketId", alias = "itemId")]
    pub item_id: String,
    /// Título del elemento.
    #[serde(alias = "summary")]
    pub title: String,
    /// Origen (`slack`, `gmail`…).
    pub source: String,
    /// Prioridad declarada.
    #[serde(default)]
    pub priority: Option<String>,
    /// Autor del mensaje original.
    #[serde(default)]
    pub author: Option<String>,
}

/// Alerta de triage registrada en el servidor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriageAlert {
    /// Identificador del elemento.
    pub item_id: String,
    /// Título.
    pub title: String,
    /// Origen.
    pub source: String,
    /// Prioridad declarada.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// Autor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

impl From<NotifyTriageAlertParams> for TriageAlert {
    fn from(params: NotifyTriageAlertParams) -> Self {
        Self {
            item_id: params.item_id,
            title: params.title,
            source: params.source,
            priority: params.priority,
            author: params.author,
        }
    }
}

/// Locución aceptada por `jarvis.speak`, registrada para su verificación.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Utterance {
    /// Texto a locutar.
    pub text: String,
    /// Categoría declarada.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

/// Resultado de `jarvis.notify*`: acuse mínimo para clientes que los envían
/// como *request* (el contrato los define como *notification*).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    /// `true` si el evento se registró.
    pub accepted: bool,
    /// Método reconocido.
    pub method: String,
}

impl Ack {
    /// Acuse de un método registrado.
    pub fn new(method: impl Into<String>) -> Self {
        Self {
            accepted: true,
            method: method.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(line: &str) -> Incoming {
        Incoming::parse_line(line).expect("mensaje válido")
    }

    /// Serializa y vuelve a deserializar: *round-trip* de un tipo del protocolo.
    fn rt<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let json = serde_json::to_value(value).expect("serializable");
        serde_json::from_value(json).expect("re-deserializable")
    }

    #[test]
    fn request_roundtrip_with_numeric_id() {
        let line = r#"{"jsonrpc":"2.0","method":"jarvis.ping","id":1}"#;
        match parse(line) {
            Incoming::Request(req) => {
                assert_eq!(req.method, methods::PING);
                assert_eq!(req.id, Id::Number(1));
                assert!(req.params.is_none());
            }
            other => panic!("se esperaba Request, obtenido {other:?}"),
        }
    }

    #[test]
    fn request_roundtrip_with_string_id() {
        let line = r#"{"jsonrpc":"2.0","method":"jarvis.getState","id":"abc"}"#;
        match parse(line) {
            Incoming::Request(req) => assert_eq!(req.id, Id::String("abc".to_string())),
            other => panic!("se esperaba Request, obtenido {other:?}"),
        }
    }

    #[test]
    fn message_without_id_is_a_notification() {
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.notifyTriageAlert","#,
            r#""params":{"item_id":"t-1","title":"Fallo PDF","source":"slack"}}"#,
        );
        match parse(line) {
            Incoming::Notification(note) => {
                assert_eq!(note.method, methods::NOTIFY_TRIAGE_ALERT);
                let params: NotifyTriageAlertParams = note.parse_params().expect("params");
                assert_eq!(params.item_id, "t-1");
                assert_eq!(params.title, "Fallo PDF");
                assert_eq!(params.source, "slack");
            }
            other => panic!("se esperaba Notification, obtenido {other:?}"),
        }
    }

    #[test]
    fn explicit_null_id_is_a_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"jarvis.ping","id":null}"#;
        assert!(matches!(parse(line), Incoming::Notification(_)));
    }

    #[test]
    fn response_is_recognized() {
        let line = r#"{"jsonrpc":"2.0","result":{"ok":true},"id":7}"#;
        match parse(line) {
            Incoming::Response(resp) => {
                assert!(!resp.is_error());
                assert_eq!(resp.id, Some(Id::Number(7)));
                assert_eq!(resp.result, Some(json!({"ok": true})));
            }
            other => panic!("se esperaba Response, obtenido {other:?}"),
        }
    }

    #[test]
    fn error_response_is_recognized() {
        let line = r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"no existe"},"id":2}"#;
        match parse(line) {
            Incoming::Response(resp) => {
                assert!(resp.is_error());
                let error = resp.error.expect("error presente");
                assert_eq!(error.code, error_codes::METHOD_NOT_FOUND);
                assert_eq!(error.message, "no existe");
            }
            other => panic!("se esperaba Response, obtenido {other:?}"),
        }
    }

    #[test]
    fn invalid_json_yields_parse_error() {
        let error = Incoming::parse_line("{no es json").expect_err("debe fallar");
        assert_eq!(error.code, error_codes::PARSE_ERROR);
    }

    #[test]
    fn non_object_message_is_rejected() {
        for line in ["[]", "\"texto\"", "42"] {
            let error = Incoming::parse_line(line).expect_err("no es objeto");
            assert_eq!(error.code, error_codes::INVALID_REQUEST, "{line}");
        }
    }

    #[test]
    fn wrong_jsonrpc_version_is_rejected() {
        let error = Incoming::parse_line(r#"{"jsonrpc":"1.0","method":"jarvis.ping","id":1}"#)
            .expect_err("versión incorrecta");
        assert_eq!(error.code, error_codes::INVALID_REQUEST);
        assert!(error.message.contains("2.0"));
    }

    #[test]
    fn message_with_neither_method_nor_id_is_rejected() {
        let error = Incoming::parse_line(r#"{"jsonrpc":"2.0"}"#).expect_err("inválido");
        assert_eq!(error.code, error_codes::INVALID_REQUEST);
    }

    #[test]
    fn empty_line_yields_parse_error() {
        let error = Incoming::parse_line("   ").expect_err("vacío");
        assert_eq!(error.code, error_codes::PARSE_ERROR);
    }

    #[test]
    fn missing_required_params_yield_invalid_params() {
        let line = concat!(
            r#"{"jsonrpc":"2.0","method":"jarvis.dispatchSwarm","#,
            r#""params":{"agent":"dev"},"id":3}"#,
        );
        match parse(line) {
            Incoming::Request(req) => {
                let error = req
                    .parse_params::<DispatchSwarmParams>()
                    .expect_err("faltan campos");
                assert_eq!(error.code, error_codes::INVALID_PARAMS);
            }
            other => panic!("se esperaba Request, obtenido {other:?}"),
        }
    }

    #[test]
    fn params_accept_snake_and_camel_case() {
        let snake: DispatchSwarmParams =
            serde_json::from_value(json!({"task_id":"t-1","agent":"dev","goal":"g"})).unwrap();
        let camel: DispatchSwarmParams =
            serde_json::from_value(json!({"taskId":"t-1","agent":"dev","goal":"g"})).unwrap();
        assert_eq!(snake, camel);
    }

    #[test]
    fn triage_alert_accepts_both_contract_shapes() {
        let issue5: NotifyTriageAlertParams = serde_json::from_value(
            json!({"item_id":"triage-1","title":"Fallo PDF","source":"slack"}),
        )
        .unwrap();
        let sdd: NotifyTriageAlertParams = serde_json::from_value(json!({
            "ticketId":"triage-1",
            "source":"slack",
            "author":"María González",
            "summary":"Fallo PDF",
            "priority":"high"
        }))
        .unwrap();
        assert_eq!(issue5.item_id, sdd.item_id);
        assert_eq!(issue5.title, sdd.title);
        assert_eq!(issue5.source, sdd.source);
        assert_eq!(sdd.priority.as_deref(), Some("high"));
        assert_eq!(sdd.author.as_deref(), Some("María González"));
    }

    #[test]
    fn swarm_completed_resolves_status_and_summary() {
        let issue5: NotifySwarmCompletedParams = serde_json::from_value(json!({
            "task_id":"task-1","status":"completed","summary":"Refactor listo"
        }))
        .unwrap();
        assert_eq!(issue5.resolved_status(), task_status::COMPLETED);
        assert_eq!(issue5.resolved_summary(), "Refactor listo");

        let sdd: NotifySwarmCompletedParams = serde_json::from_value(json!({
            "taskId":"task-2","summary":"Refactor de login completado",
            "testsPassed":43,"testsFailed":0
        }))
        .unwrap();
        assert_eq!(sdd.resolved_status(), task_status::COMPLETED);
        assert_eq!(sdd.tests_passed, Some(43));

        let failing: NotifySwarmCompletedParams =
            serde_json::from_value(json!({"task_id":"t-3","testsFailed":2})).unwrap();
        assert_eq!(failing.resolved_status(), task_status::FAILED);
        assert_eq!(failing.resolved_summary(), "tests: 0 ok / 2 fallidos");
    }

    #[test]
    fn responses_serialize_as_ndjson_lines() {
        let response = Response::success(1i64, json!({"pong": true}));
        let line = response.to_ndjson();
        assert!(
            line.ends_with('\n'),
            "NDJSON debe terminar en salto de línea"
        );
        assert_eq!(line.matches('\n').count(), 1);
        let parsed: Response = serde_json::from_str(line.trim()).expect("re-parseable");
        assert_eq!(parsed, response);
    }

    #[test]
    fn error_response_without_id_serializes_null_id() {
        let error = JsonRpcError::new(error_codes::PARSE_ERROR, "boom");
        let line = Response::error(None, error).to_ndjson();
        assert!(line.contains("\"id\":null"), "línea: {line}");
    }

    #[test]
    fn error_codes_match_jsonrpc_standard() {
        assert_eq!(error_codes::PARSE_ERROR, -32700);
        assert_eq!(error_codes::INVALID_REQUEST, -32600);
        assert_eq!(error_codes::METHOD_NOT_FOUND, -32601);
        assert_eq!(error_codes::INVALID_PARAMS, -32602);
        assert_eq!(error_codes::INTERNAL_ERROR, -32603);
        assert_eq!(error_codes::PROTOCOL_VERSION_MISMATCH, -32001);
        assert_eq!(error_codes::IDE_NOT_CONNECTED, -32002);
        assert_eq!(error_codes::BUSY, -32003);
    }

    #[test]
    fn catalog_has_no_duplicates() {
        let mut sorted = methods::ALL.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(
            before,
            sorted.len(),
            "hay métodos duplicados en el catálogo"
        );
        assert!(methods::ALL.iter().all(|m| m.starts_with("jarvis.")));
    }

    #[test]
    fn id_conversions_are_consistent() {
        assert_eq!(Id::from(5i64), Id::Number(5));
        assert_eq!(Id::from("x"), Id::String("x".to_string()));
        assert_eq!(Id::from(5i64).to_string(), "5");
        assert_eq!(Id::from("x").to_string(), "x");
    }

    #[test]
    fn session_params_and_results_roundtrip_hu66() {
        let id = SessionIdParams { id: 7 };
        assert_eq!(rt(&id), id);

        let send = SessionSendParams {
            id: 7,
            instruction: "revisa también la PR 214".to_string(),
        };
        assert_eq!(rt(&send), send);

        let list = SessionsResult {
            sessions: vec![json!({"id": 7, "title": "PR de Felipe", "status": "running"})],
        };
        assert_eq!(rt(&list), list);

        let sent = SessionSendResult { id: 7, ok: true };
        assert_eq!(rt(&sent), sent);

        let cancelled = SessionCancelResult {
            id: 7,
            cancelled: true,
        };
        assert_eq!(rt(&cancelled), cancelled);

        let read = SessionReadResult {
            id: 7,
            detail: "# PR 214\n\nSin hallazgos graves.".to_string(),
        };
        assert_eq!(rt(&read), read);

        let event = SessionEventParams {
            id: 7,
            event: session_event::REPLIED.to_string(),
            title: Some("PR de Felipe".to_string()),
            status: Some("idle".to_string()),
            detail: Some("dos PR listas".to_string()),
        };
        assert_eq!(rt(&event), event);
    }

    /// Los campos opcionales del evento no viajan como `null`: el cliente que
    /// solo mira `id`/`event` no recibe ruido.
    #[test]
    fn session_event_omits_unknown_optional_fields_hu66() {
        let event = SessionEventParams {
            id: 3,
            event: session_event::STARTED.to_string(),
            title: None,
            status: None,
            detail: None,
        };
        let value = serde_json::to_value(&event).expect("serializable");
        assert_eq!(value, json!({"id": 3, "event": "started"}));
    }

    #[test]
    fn session_event_catalog_has_no_duplicates_hu66() {
        let mut sorted = session_event::ALL.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(
            before,
            sorted.len(),
            "hay eventos duplicados en el catálogo"
        );
    }

    /// HU-66 · CA-3 (validación código → doc): el 100 % de los métodos del
    /// catálogo aparece en la documentación de IPC.
    #[test]
    fn catalog_is_documented_hu66() {
        let doc = include_str!("../../../docs/specs/sdd-contracts/sdd-hu66-ipc-sesiones.md");
        for method in methods::ALL {
            assert!(
                doc.contains(method),
                "el método {method} no está documentado en sdd-hu66-ipc-sesiones.md"
            );
        }
        for event in session_event::ALL {
            assert!(
                doc.contains(&format!("\"{event}\"")) || doc.contains(&format!("`{event}`")),
                "el evento {event} no está documentado en sdd-hu66-ipc-sesiones.md"
            );
        }
    }
}
