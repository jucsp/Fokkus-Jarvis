//! Cliente IPC para que Fokkus IDE —o cualquier proceso— hable con Jarvis.
//!
//! Mantiene una conexión NDJSON de larga vida: cada llamada envía una petición
//! con un `id` propio y espera la respuesta con el mismo `id`.

use std::fmt;
use std::path::Path;

use serde_json::Value;

use crate::protocol::{
    CancelTaskResult, DispatchTaskResult, HelloResult, Id, JsonRpcError, PingResult,
    SetMutedResult, SpeakResult, StateResult, SwarmStatusResult, SwarmTask, TasksResult,
};
use crate::server::IpcError;

#[cfg(unix)]
use crate::paths;
#[cfg(unix)]
use crate::protocol::{
    methods, DispatchSwarmParams, DispatchTaskParams, HelloParams, Notification,
    NotifySwarmCompletedParams, NotifySwarmProgressParams, NotifyTriageAlertParams, Request,
    Response, SetMutedParams, SpeakParams, TaskIdParams, PROTOCOL_VERSION,
};
#[cfg(unix)]
use serde::de::DeserializeOwned;
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

/// Error de una llamada al servidor.
#[derive(Debug)]
pub enum ClientError {
    /// No se pudo hablar con el servidor (socket ausente, cerrado, etc.).
    Transport(String),
    /// El resultado no se pudo deserializar al tipo esperado.
    Decode { method: String, source: String },
    /// El servidor respondió con un error JSON-RPC.
    Rpc(JsonRpcError),
    /// El servidor respondió sin `result` ni `error`.
    EmptyResponse,
    /// El `id` de la respuesta no coincide con el de la petición.
    UnexpectedId { expected: Id, received: Option<Id> },
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Transport(message) => write!(f, "fallo de transporte IPC: {message}"),
            ClientError::Decode { method, source } => {
                write!(f, "resultado ilegible de {method}: {source}")
            }
            ClientError::Rpc(error) => write!(f, "error JSON-RPC del servidor: {error}"),
            ClientError::EmptyResponse => {
                write!(f, "el servidor respondió sin \"result\" ni \"error\"")
            }
            ClientError::UnexpectedId { expected, received } => {
                let received = match received {
                    Some(id) => id.to_string(),
                    None => "null".to_string(),
                };
                write!(
                    f,
                    "id inesperado en la respuesta: {received} (esperado {expected})"
                )
            }
        }
    }
}

impl std::error::Error for ClientError {}

/// Cliente JSON-RPC sobre un Unix Domain Socket.
#[cfg(unix)]
pub struct IpcClient {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: i64,
}

#[cfg(unix)]
impl IpcClient {
    /// Conecta al socket indicado.
    pub fn connect(path: &Path) -> Result<Self, IpcError> {
        let writer = UnixStream::connect(path)?;
        let reader = BufReader::new(writer.try_clone()?);
        Ok(Self {
            writer,
            reader,
            next_id: 0,
        })
    }

    /// Conecta al socket por defecto ([`paths::default_socket_path`]).
    pub fn connect_default() -> Result<Self, IpcError> {
        Self::connect(&paths::default_socket_path())
    }

    /// Envía una petición y espera su respuesta.
    pub fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value, ClientError> {
        self.next_id += 1;
        let request = Request::new(self.next_id, method, params);
        self.write_message(&request)?;
        let response = self.read_response()?;

        if let Some(error) = response.error {
            return Err(ClientError::Rpc(error));
        }
        if response.id != Some(request.id.clone()) {
            return Err(ClientError::UnexpectedId {
                expected: request.id,
                received: response.id,
            });
        }
        match response.result {
            Some(result) => Ok(result),
            None => Err(ClientError::EmptyResponse),
        }
    }

    /// Envía una notificación (sin respuesta) y no espera nada.
    pub fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), ClientError> {
        let notification = Notification::new(method, params);
        self.write_message(&notification)
    }

    /// `jarvis.hello`: registra la sesión del IDE (I5).
    pub fn hello(&mut self, client: &str) -> Result<HelloResult, ClientError> {
        let params = HelloParams {
            client: client.to_string(),
            client_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            protocol_version: PROTOCOL_VERSION,
        };
        let value = self.request(methods::HELLO, Some(to_params(&params)?))?;
        decode(methods::HELLO, value)
    }

    /// `jarvis.ping`.
    pub fn ping(&mut self) -> Result<PingResult, ClientError> {
        let value = self.request(methods::PING, None)?;
        decode(methods::PING, value)
    }

    /// `jarvis.getState`.
    pub fn get_state(&mut self) -> Result<StateResult, ClientError> {
        let value = self.request(methods::GET_STATE, None)?;
        decode(methods::GET_STATE, value)
    }

    /// `jarvis.setMuted`.
    pub fn set_muted(&mut self, muted: bool) -> Result<SetMutedResult, ClientError> {
        let params = SetMutedParams { muted };
        let value = self.request(methods::SET_MUTED, Some(to_params(&params)?))?;
        decode(methods::SET_MUTED, value)
    }

    /// `jarvis.speak`.
    pub fn speak(
        &mut self,
        text: &str,
        category: Option<&str>,
    ) -> Result<SpeakResult, ClientError> {
        let params = SpeakParams {
            text: text.to_string(),
            category: category.map(str::to_string),
            queue: None,
            priority: None,
        };
        let value = self.request(methods::SPEAK, Some(to_params(&params)?))?;
        decode(methods::SPEAK, value)
    }

    /// `jarvis.speak` con `queue: true`: encola con la prioridad indicada
    /// (`notice` si `None`) y devuelve la posición en la cola (HU-37).
    pub fn speak_queued(
        &mut self,
        text: &str,
        priority: Option<&str>,
    ) -> Result<SpeakResult, ClientError> {
        let params = SpeakParams {
            text: text.to_string(),
            category: None,
            queue: Some(true),
            priority: priority.map(str::to_string),
        };
        let value = self.request(methods::SPEAK, Some(to_params(&params)?))?;
        decode(methods::SPEAK, value)
    }

    /// `jarvis.dispatchTask` (HU-37).
    pub fn dispatch_task(
        &mut self,
        title: &str,
        prompt: &str,
    ) -> Result<DispatchTaskResult, ClientError> {
        let params = DispatchTaskParams {
            title: title.to_string(),
            prompt: prompt.to_string(),
        };
        let value = self.request(methods::DISPATCH_TASK, Some(to_params(&params)?))?;
        decode(methods::DISPATCH_TASK, value)
    }

    /// `jarvis.getTasks` (HU-37).
    pub fn get_tasks(&mut self) -> Result<TasksResult, ClientError> {
        let value = self.request(methods::GET_TASKS, None)?;
        decode(methods::GET_TASKS, value)
    }

    /// `jarvis.getTask` (HU-37).
    pub fn get_task(&mut self, id: u64) -> Result<serde_json::Value, ClientError> {
        let value = self.request(methods::GET_TASK, Some(to_params(&TaskIdParams { id })?))?;
        decode(methods::GET_TASK, value)
    }

    /// `jarvis.cancelTask` (HU-37).
    pub fn cancel_task(&mut self, id: u64) -> Result<CancelTaskResult, ClientError> {
        let value = self.request(methods::CANCEL_TASK, Some(to_params(&TaskIdParams { id })?))?;
        decode(methods::CANCEL_TASK, value)
    }

    /// `jarvis.dispatchSwarm`.
    pub fn dispatch_swarm(
        &mut self,
        task_id: &str,
        agent: &str,
        goal: &str,
    ) -> Result<SwarmTask, ClientError> {
        let params = DispatchSwarmParams {
            task_id: task_id.to_string(),
            agent: agent.to_string(),
            goal: goal.to_string(),
        };
        let value = self.request(methods::DISPATCH_SWARM, Some(to_params(&params)?))?;
        decode(methods::DISPATCH_SWARM, value)
    }

    /// `jarvis.getSwarmStatus`.
    pub fn get_swarm_status(&mut self) -> Result<SwarmStatusResult, ClientError> {
        let value = self.request(methods::GET_SWARM_STATUS, None)?;
        decode(methods::GET_SWARM_STATUS, value)
    }

    /// `jarvis.notifySwarmProgress` (notificación).
    pub fn notify_swarm_progress(
        &mut self,
        task_id: &str,
        role: &str,
        percent: u8,
        message: &str,
    ) -> Result<(), ClientError> {
        let params = NotifySwarmProgressParams {
            task_id: task_id.to_string(),
            role: Some(role.to_string()),
            percent: Some(percent),
            message: Some(message.to_string()),
        };
        self.notify(methods::NOTIFY_SWARM_PROGRESS, Some(to_params(&params)?))
    }

    /// `jarvis.notifySwarmCompleted` (notificación).
    pub fn notify_swarm_completed(
        &mut self,
        task_id: &str,
        summary: &str,
        tests_passed: u32,
        tests_failed: u32,
    ) -> Result<(), ClientError> {
        let params = NotifySwarmCompletedParams {
            task_id: task_id.to_string(),
            status: None,
            summary: summary.to_string(),
            tests_passed: Some(tests_passed),
            tests_failed: Some(tests_failed),
        };
        self.notify(methods::NOTIFY_SWARM_COMPLETED, Some(to_params(&params)?))
    }

    /// `jarvis.notifyTriageAlert` (notificación).
    pub fn notify_triage_alert(
        &mut self,
        item_id: &str,
        title: &str,
        source: &str,
    ) -> Result<(), ClientError> {
        let params = NotifyTriageAlertParams {
            item_id: item_id.to_string(),
            title: title.to_string(),
            source: source.to_string(),
            priority: None,
            author: None,
        };
        self.notify(methods::NOTIFY_TRIAGE_ALERT, Some(to_params(&params)?))
    }

    fn write_message<T: serde::Serialize>(&mut self, message: &T) -> Result<(), ClientError> {
        let mut line = serde_json::to_string(message)
            .map_err(|e| ClientError::Transport(format!("no se pudo serializar: {e}")))?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        self.writer
            .flush()
            .map_err(|e| ClientError::Transport(e.to_string()))
    }

    fn read_response(&mut self) -> Result<Response, ClientError> {
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        if read == 0 {
            return Err(ClientError::Transport(
                "el servidor cerró la conexión".to_string(),
            ));
        }
        serde_json::from_str(line.trim())
            .map_err(|e| ClientError::Transport(format!("respuesta ilegible: {e}")))
    }
}

/// Serializa parámetros a `Value`.
#[cfg(unix)]
fn to_params<T: serde::Serialize>(params: &T) -> Result<Value, ClientError> {
    serde_json::to_value(params)
        .map_err(|e| ClientError::Transport(format!("parámetros no serializables: {e}")))
}

/// Deserializa un resultado al tipo esperado.
#[cfg(unix)]
fn decode<T: DeserializeOwned>(method: &str, value: Value) -> Result<T, ClientError> {
    serde_json::from_value(value).map_err(|e| ClientError::Decode {
        method: method.to_string(),
        source: e.to_string(),
    })
}

#[cfg(unix)]
impl fmt::Debug for IpcClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IpcClient")
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

/// Cliente IPC (stub para plataformas sin `UnixStream`, p. ej. Windows).
///
/// Mantiene la misma superficie pública que la implementación Unix para que el
/// workspace compile en cualquier plataforma: [`IpcClient::connect`] devuelve
/// [`IpcError::Unsupported`] y, por tanto, nunca existe una instancia real.
#[cfg(not(unix))]
#[derive(Debug)]
pub struct IpcClient {
    _private: (),
}

#[cfg(not(unix))]
impl IpcClient {
    /// Siempre falla: no hay Unix Domain Sockets en esta plataforma.
    pub fn connect(_path: &Path) -> Result<Self, IpcError> {
        Err(IpcError::Unsupported)
    }

    /// Siempre falla: no hay Unix Domain Sockets en esta plataforma.
    pub fn connect_default() -> Result<Self, IpcError> {
        Err(IpcError::Unsupported)
    }

    /// Siempre falla con [`ClientError::Transport`].
    pub fn request(&mut self, _method: &str, _params: Option<Value>) -> Result<Value, ClientError> {
        Err(unsupported())
    }

    /// Siempre falla con [`ClientError::Transport`].
    pub fn notify(&mut self, _method: &str, _params: Option<Value>) -> Result<(), ClientError> {
        Err(unsupported())
    }

    /// Siempre falla con [`ClientError::Transport`].
    pub fn hello(&mut self, _client: &str) -> Result<HelloResult, ClientError> {
        Err(unsupported())
    }

    /// Siempre falla con [`ClientError::Transport`].
    pub fn ping(&mut self) -> Result<PingResult, ClientError> {
        Err(unsupported())
    }

    /// Siempre falla con [`ClientError::Transport`].
    pub fn get_state(&mut self) -> Result<StateResult, ClientError> {
        Err(unsupported())
    }

    /// Siempre falla con [`ClientError::Transport`].
    pub fn set_muted(&mut self, _muted: bool) -> Result<SetMutedResult, ClientError> {
        Err(unsupported())
    }

    /// Siempre falla con [`ClientError::Transport`].
    pub fn speak(
        &mut self,
        _text: &str,
        _category: Option<&str>,
    ) -> Result<SpeakResult, ClientError> {
        Err(unsupported())
    }

    /// Siempre falla con [`ClientError::Transport`].
    pub fn dispatch_swarm(
        &mut self,
        _task_id: &str,
        _agent: &str,
        _goal: &str,
    ) -> Result<SwarmTask, ClientError> {
        Err(unsupported())
    }

    /// Siempre falla con [`ClientError::Transport`].
    pub fn get_swarm_status(&mut self) -> Result<SwarmStatusResult, ClientError> {
        Err(unsupported())
    }

    /// Siempre falla con [`ClientError::Transport`].
    pub fn notify_swarm_progress(
        &mut self,
        _task_id: &str,
        _role: &str,
        _percent: u8,
        _message: &str,
    ) -> Result<(), ClientError> {
        Err(unsupported())
    }

    /// Siempre falla con [`ClientError::Transport`].
    pub fn notify_swarm_completed(
        &mut self,
        _task_id: &str,
        _summary: &str,
        _tests_passed: u32,
        _tests_failed: u32,
    ) -> Result<(), ClientError> {
        Err(unsupported())
    }

    /// Siempre falla con [`ClientError::Transport`].
    pub fn notify_triage_alert(
        &mut self,
        _item_id: &str,
        _title: &str,
        _source: &str,
    ) -> Result<(), ClientError> {
        Err(unsupported())
    }
}

/// Error común del stub sin transporte.
#[cfg(not(unix))]
fn unsupported() -> ClientError {
    ClientError::Transport("IPC no soportado en esta plataforma".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::protocol::error_codes;

    fn missing_socket_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("jarvis-ipc-client-{name}-{}", std::process::id()))
            .join("ausente.sock")
    }

    #[test]
    fn connect_to_missing_socket_fails() {
        let result = IpcClient::connect(&missing_socket_path("missing"));
        assert!(matches!(result, Err(IpcError::Io(_))));
    }

    #[test]
    fn rpc_error_display_includes_code() {
        let error = ClientError::Rpc(JsonRpcError::new(
            error_codes::METHOD_NOT_FOUND,
            "método desconocido",
        ));
        let text = error.to_string();
        assert!(text.contains("-32601"), "texto: {text}");
        assert!(text.contains("método desconocido"));
    }

    #[test]
    fn unexpected_id_display_mentions_both_ids() {
        let error = ClientError::UnexpectedId {
            expected: Id::Number(1),
            received: Some(Id::Number(2)),
        };
        let text = error.to_string();
        assert!(text.contains('1') && text.contains('2'), "texto: {text}");

        let null = ClientError::UnexpectedId {
            expected: Id::Number(1),
            received: None,
        };
        assert!(null.to_string().contains("null"));
    }
}
