//! Sesión persistente del CLI Antigravity (`agy`) por NDJSON (latencia E2E).
//!
//! Lanzar `agy -p "<prompt>"` en cada turno paga un arranque en frío de
//! ~3,5 s (binario, autenticación y los servidores MCP `uvx`/`npx`) antes de
//! que el modelo vea el prompt, y en modo `text` entrega la respuesta entera
//! al final, así que el TTS por frases no puede adelantarse.
//!
//! Con `--input-format stream-json --output-format stream-json` el mismo
//! proceso atiende un turno por cada línea NDJSON de stdin y emite los
//! `text_delta` según llegan. [`AgySession`] mantiene ese proceso vivo entre
//! turnos (se lanza por adelantado, fuera del camino crítico) y
//! [`parse_event`] traduce su salida a deltas y a un fin de turno.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Receiver;

/// Flags de modo *print* que aceptan el prompt como valor.
const PRINT_FLAGS: &[&str] = &["-p", "--print", "--prompt"];
/// Bytes de stderr que se conservan para el mensaje de error.
const STDERR_TAIL: usize = 2_048;

/// Argumentos de sesión persistente para un comando `agy … -p`, o `None` si
/// el comando no es Antigravity en modo *print* con el prompt al final (otro
/// CLI, `{prompt}` explícito, formatos ya fijados por el usuario…). En ese
/// caso el cerebro sigue lanzando un proceso por turno.
pub fn session_args(program: &str, args: &[String], placeholder: &str) -> Option<Vec<String>> {
    let name = Path::new(program).file_name()?.to_str()?;
    if name != "agy" {
        return None;
    }
    let (last, rest) = args.split_last()?;
    if !PRINT_FLAGS.contains(&last.as_str()) {
        return None;
    }
    let conflicting = rest.iter().any(|arg| {
        arg.contains(placeholder)
            || PRINT_FLAGS.contains(&arg.as_str())
            || arg.starts_with("--input-format")
            || arg.starts_with("--output-format")
            || matches!(arg.as_str(), "-i" | "--prompt-interactive")
    });
    if conflicting {
        return None;
    }
    let mut session = rest.to_vec();
    session.extend(
        [
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "-p",
            "",
        ]
        .map(String::from),
    );
    Some(session)
}

/// Línea NDJSON con un turno del usuario.
pub fn user_message(content: &str) -> String {
    serde_json::json!({
        "event": "user",
        "message": { "role": "user", "content": content },
    })
    .to_string()
}

/// Evento relevante de la salida `stream-json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Fragmento de texto de una respuesta del agente.
    Delta(String),
    /// Fin del turno: la respuesta completa o el error que informó el CLI.
    Done(Result<String, String>),
    /// Cualquier otro evento (init, pasos de herramientas, avisos).
    Other,
}

/// Interpreta una línea de la salida `stream-json` de `agy`.
pub fn parse_event(line: &str) -> SessionEvent {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return SessionEvent::Other;
    };
    match value.get("event").and_then(|e| e.as_str()) {
        Some("step_update") => {
            let step = &value["step_update"];
            if step["step_type"] != "agent_response" {
                return SessionEvent::Other;
            }
            match step["text_delta"].as_str() {
                Some(delta) if !delta.is_empty() => SessionEvent::Delta(delta.to_string()),
                _ => SessionEvent::Other,
            }
        }
        Some("result") => {
            let result = &value["result"];
            if result["status"] == "SUCCESS" {
                let response = result["response"].as_str().unwrap_or_default();
                SessionEvent::Done(Ok(response.to_string()))
            } else {
                let error = result["error"]
                    .as_str()
                    .filter(|e| !e.is_empty())
                    .unwrap_or("el CLI terminó el turno con error");
                SessionEvent::Done(Err(error.to_string()))
            }
        }
        _ => SessionEvent::Other,
    }
}

/// Proceso `agy` vivo que atiende turnos por stdin.
pub struct AgySession {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    stderr_tail: Arc<Mutex<String>>,
    /// Turnos ya enviados: el primero lleva la persona y el historial.
    turns: usize,
}

impl AgySession {
    /// Lanza el proceso con `command` ya configurado (entorno, grupo).
    pub fn spawn(mut command: Command) -> std::io::Result<Self> {
        use std::process::Stdio;
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other("el CLI no expuso sus tuberías"));
        };
        let (tx, lines) = crossbeam_channel::unbounded();
        std::thread::Builder::new()
            .name("jarvis-agy-out".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            })?;
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let tail = Arc::clone(&stderr_tail);
        std::thread::Builder::new()
            .name("jarvis-agy-err".into())
            .spawn(move || {
                let mut err = stderr;
                let mut chunk = [0u8; 1_024];
                while let Ok(n) = err.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    let mut tail = tail.lock().unwrap_or_else(|p| p.into_inner());
                    tail.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    if tail.len() > STDERR_TAIL * 2 {
                        let mut cut = tail.len() - STDERR_TAIL;
                        while !tail.is_char_boundary(cut) {
                            cut += 1;
                        }
                        tail.drain(..cut);
                    }
                }
            })?;
        Ok(Self {
            child,
            stdin,
            lines,
            stderr_tail,
            turns: 0,
        })
    }

    /// `true` si el proceso sigue corriendo.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Turnos ya enviados a este proceso.
    pub fn turns(&self) -> usize {
        self.turns
    }

    /// Envía un turno (una línea NDJSON).
    pub fn send(&mut self, content: &str) -> std::io::Result<()> {
        let mut line = user_message(content);
        line.push('\n');
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.flush()?;
        self.turns += 1;
        Ok(())
    }

    /// Líneas de stdout; se desconecta cuando el proceso cierra la salida.
    pub fn lines(&self) -> &Receiver<String> {
        &self.lines
    }

    /// Últimos bytes de stderr, para el mensaje de error.
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .trim()
            .to_string()
    }

    /// PID del proceso (pruebas de persistencia).
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Mata el proceso y su grupo y lo recoge.
    pub fn kill(mut self) {
        super::cli::kill_tree(&mut self.child);
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_agy_in_print_mode_with_the_prompt_last_gets_a_session() {
        let configured = args(&[
            "--model",
            "gemini-3.8-flash-medium",
            "--dangerously-skip-permissions",
            "-p",
        ]);
        let session = session_args("agy", &configured, "{prompt}").expect("agy -p");
        assert_eq!(
            session,
            args(&[
                "--model",
                "gemini-3.8-flash-medium",
                "--dangerously-skip-permissions",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "-p",
                "",
            ])
        );
        assert!(session_args("/home/u/.local/bin/agy", &args(&["--print"]), "{prompt}").is_some());

        // Otro CLI, prompt explícito, sin -p final o formatos ya elegidos.
        assert!(session_args("echo", &args(&["-p"]), "{prompt}").is_none());
        assert!(session_args("agy", &args(&["-p", "{prompt}"]), "{prompt}").is_none());
        assert!(session_args("agy", &args(&["-p", "--model", "x"]), "{prompt}").is_none());
        assert!(session_args("agy", &args(&[]), "{prompt}").is_none());
        assert!(
            session_args("agy", &args(&["--output-format", "json", "-p"]), "{prompt}").is_none()
        );
    }

    #[test]
    fn user_message_is_one_ndjson_line_with_the_turn() {
        let line = user_message("hola \"Jarvis\"\nsegunda línea");
        assert!(!line.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["event"], "user");
        assert_eq!(value["message"]["role"], "user");
        assert_eq!(
            value["message"]["content"],
            "hola \"Jarvis\"\nsegunda línea"
        );
    }

    #[test]
    fn parses_agent_deltas_results_and_ignores_the_rest() {
        let delta = r#"{"event":"step_update","step_update":{"step_index":3,"state":"ACTIVE","step_type":"agent_response","text_delta":"Son las "}}"#;
        assert_eq!(parse_event(delta), SessionEvent::Delta("Son las ".into()));
        let tool = r#"{"event":"step_update","step_update":{"step_type":"tool","tool_name":"run_command","text_delta":"x"}}"#;
        assert_eq!(parse_event(tool), SessionEvent::Other);
        let done = r#"{"event":"step_update","step_update":{"step_type":"agent_response","state":"DONE","usage":{}}}"#;
        assert_eq!(parse_event(done), SessionEvent::Other);
        let ok = r#"{"event":"result","result":{"status":"SUCCESS","response":"Son las diez.\n"}}"#;
        assert_eq!(
            parse_event(ok),
            SessionEvent::Done(Ok("Son las diez.\n".into()))
        );
        let err = r#"{"event":"result","result":{"status":"ERROR","response":"","error":"cuota agotada"}}"#;
        assert_eq!(
            parse_event(err),
            SessionEvent::Done(Err("cuota agotada".into()))
        );
        assert_eq!(parse_event("warning: algo"), SessionEvent::Other);
        assert_eq!(parse_event(r#"{"event":"init"}"#), SessionEvent::Other);
    }
}
