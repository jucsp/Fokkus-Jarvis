//! Servidor y cliente IPC de Fokkus-Jarvis (Issue #5).
//!
//! Expone el asistente a Fokkus IDE mediante un Unix Domain Socket
//! (`~/.fokkus/jarvis.sock` por defecto) hablando **JSON-RPC 2.0** sobre
//! framing **NDJSON** (un objeto por línea), según el SDD §3 y las enmiendas
//! del dictamen QA de Fase 1.
//!
//! # Arquitectura
//!
//! * [`protocol`] — mensajes, catálogo único de métodos y códigos de error.
//! * [`server`] — [`IpcServer`] (hilo aceptador + `ServerState`) y
//!   [`SpeechSink`] para conectar con `jarvis-audio`.
//! * [`client`] — [`IpcClient`] con métodos tipados (`ping`, `speak`,
//!   `dispatch_swarm`…).
//! * [`paths`] — resolución de la ruta del socket.
//!
//! # Enmiendas del dictamen aplicadas
//!
//! * **I1** framing NDJSON con límite de 1 MiB.
//! * **I2** los `notify*` son notificaciones (sin `id` → sin respuesta).
//! * **I3** un único catálogo de métodos; los parámetros aceptan `snake_case`
//!   (Issue #5) y `camelCase` (SDD §3.1).
//! * **I5** handshake `jarvis.hello` con versionado y `-32001` en conflicto.
//! * **I8** códigos de error estándar + `-32001`, `-32002`, `-32003`.
//!
//! # Pendiente
//!
//! * **I6** `SO_PEERCRED` no está disponible en `std` estable: ver
//!   [`PeerPolicy`]. Hoy se aplica endurecimiento por permisos (`0700`/`0600`).
//! * **I7** ruta en `$XDG_RUNTIME_DIR` calculada pero no activada por defecto
//!   (decisión del PO).

#[cfg(not(unix))]
compile_error!(
    "jarvis-ipc requiere Unix Domain Sockets (Linux/macOS); \
     el Named Pipe de Windows llegará en una fase posterior (SDD §3)."
);

pub mod client;
pub mod paths;
pub mod protocol;
pub mod server;

pub use client::{ClientError, IpcClient};
pub use paths::{
    default_socket_path, resolve_socket_path, runtime_socket_path, runtime_socket_path_from_env,
    SOCKET_FILE_NAME,
};
pub use protocol::{
    error_codes, methods, task_status, Ack, DispatchSwarmParams, HelloParams, HelloResult,
    HelloSession, Id, Incoming, JsonRpcError, Notification, NotifySwarmCompletedParams,
    NotifySwarmProgressParams, NotifyTriageAlertParams, PingResult, Request, Response,
    SetMutedParams, SetMutedResult, SpeakParams, SpeakResult, StateResult, SwarmStatusResult,
    SwarmTask, TriageAlert, Utterance, JSONRPC_VERSION, MAX_MESSAGE_BYTES, PROTOCOL_VERSION,
    SERVER_VERSION,
};
pub use server::{IpcError, IpcServer, PeerPolicy, ServerState, SpeechSink, MAX_CONNECTIONS};

pub use jarvis_core::{JarvisConfig, JarvisCore, JarvisState};

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_socket(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("jarvis-ipc-lib-{name}-{}", std::process::id()))
            .join("jarvis.sock")
    }

    /// Arranca un servidor en un socket temporal y devuelve el servidor junto
    /// con su ruta (el servidor debe mantenerse vivo durante el test).
    fn start_server(name: &str) -> (IpcServer, PathBuf) {
        let path = temp_socket(name);
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
        let state = std::sync::Arc::new(ServerState::with_default_config());
        let mut server = IpcServer::with_path(path.clone(), state);
        server.start().expect("el servidor debe arrancar");
        (server, path)
    }

    #[test]
    fn ping_and_get_state_over_ipc() {
        let (_server, path) = start_server("ping");
        let mut client = IpcClient::connect(&path).expect("debe conectar");

        let ping = client.ping().expect("ping");
        assert!(ping.pong);
        assert_eq!(ping.version, SERVER_VERSION);
        assert_eq!(ping.state, "idle");
        assert_eq!(ping.voice, "es-CL-LorenzoNeural");

        let state = client.get_state().expect("getState");
        assert_eq!(state.state, "idle");
        assert!(!state.muted);
        assert_eq!(state.voice_lang, "es-CL");
        assert_eq!(state.swarm_in_flight, 0);
        assert!(client.ping().expect("segundo ping").pong);
    }

    #[test]
    fn hello_registers_the_ide_session() {
        let (server, path) = start_server("hello");
        let mut client = IpcClient::connect(&path).expect("debe conectar");

        let hello = client.hello("fokkus-ide").expect("hello");
        assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
        assert!(hello.capabilities.contains(&methods::SPEAK.to_string()));

        let session = server.state().hello_session().expect("sesión registrada");
        assert_eq!(session.client, "fokkus-ide");
    }

    #[test]
    fn mute_blocks_speak_and_unmute_restores_it() {
        let (server, path) = start_server("mute");
        let mut client = IpcClient::connect(&path).expect("debe conectar");

        let muted = client.set_muted(true).expect("setMuted");
        assert!(muted.muted);
        assert_eq!(muted.state, "muted");

        let rejected = client.speak("hola", Some("general")).expect("speak");
        assert!(!rejected.spoken);
        assert_eq!(rejected.reason.as_deref(), Some("muted"));
        assert!(server.state().utterances().is_empty());

        let unmuted = client.set_muted(false).expect("setMuted");
        assert!(!unmuted.muted);

        let spoken = client
            .speak("Dame un segundo, lo estoy revisando...", Some("general"))
            .expect("speak");
        assert!(spoken.spoken);
        assert_eq!(server.state().utterances().len(), 1);
    }

    #[test]
    fn swarm_lifecycle_over_ipc() {
        let (server, path) = start_server("swarm");
        let mut client = IpcClient::connect(&path).expect("debe conectar");

        let task = client
            .dispatch_swarm("task-102", "dev", "Refactor de login")
            .expect("dispatchSwarm");
        assert_eq!(task.status, task_status::DISPATCHED);

        let status = client.get_swarm_status().expect("getSwarmStatus");
        assert_eq!(status.in_flight, 1);

        client
            .notify_swarm_progress("task-102", "dev", 60, "escribiendo tests")
            .expect("notifySwarmProgress");
        client
            .notify_swarm_completed("task-102", "Refactor de login completado", 43, 0)
            .expect("notifySwarmCompleted");

        // El servidor procesa las notificaciones de forma asíncrona respecto al
        // cliente: se sondea el estado hasta que la tarea deja de estar en vuelo.
        let mut in_flight = 1;
        for _ in 0..100 {
            if server.state().in_flight() == 0 {
                in_flight = 0;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(in_flight, 0, "la notificación debe registrarse");

        let tasks = server.state().swarm_tasks();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, task_status::COMPLETED);
        assert_eq!(
            tasks[0].summary.as_deref(),
            Some("Refactor de login completado")
        );
        assert_eq!(server.state().progress_events().len(), 1);
    }

    #[test]
    fn triage_alert_notification_is_registered() {
        let (server, path) = start_server("triage");
        let mut client = IpcClient::connect(&path).expect("debe conectar");

        client
            .notify_triage_alert("triage-4819", "Error en reporte PDF", "slack")
            .expect("notifyTriageAlert");

        let mut alerts = 0;
        for _ in 0..100 {
            alerts = server.state().alerts().len();
            if alerts > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(alerts, 1);
        let alert = &server.state().alerts()[0];
        assert_eq!(alert.item_id, "triage-4819");
        assert_eq!(alert.source, "slack");
    }

    #[test]
    fn unknown_method_returns_rpc_error() {
        let (_server, path) = start_server("unknown");
        let mut client = IpcClient::connect(&path).expect("debe conectar");

        let result = client.request("jarvis.noExiste", None);
        match result {
            Err(ClientError::Rpc(error)) => {
                assert_eq!(error.code, error_codes::METHOD_NOT_FOUND);
            }
            other => panic!("se esperaba ClientError::Rpc, obtenido {other:?}"),
        }
    }

    #[test]
    fn two_clients_are_served_concurrently() {
        let (_server, path) = start_server("concurrent");
        let mut first = IpcClient::connect(&path).expect("primer cliente");
        let mut second = IpcClient::connect(&path).expect("segundo cliente");

        first.set_muted(true).expect("mute");
        // El estado es compartido: el segundo cliente lo observa.
        assert!(second.get_state().expect("getState").muted);
        assert!(second.ping().expect("ping").pong);
        assert!(first.ping().expect("ping").pong);
    }

    #[test]
    fn second_server_on_same_socket_is_rejected() {
        let (_server, path) = start_server("exclusive");
        let state = std::sync::Arc::new(ServerState::with_default_config());
        let mut other = IpcServer::with_path(path, state);
        let error = other.start().expect_err("no debe poder arrancar dos veces");
        assert!(matches!(error, IpcError::AlreadyRunning(_)));
    }
}
