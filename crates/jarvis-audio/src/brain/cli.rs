//! Cerebro por CLI local (p. ej. Antigravity: `agy --effort low --dangerously-skip-permissions -p`), sin API key.
//!
//! Cada turno lanza el comando configurado con el *prompt* (persona Jarvis +
//! historial reciente + texto del usuario) como último argumento —o en el
//! lugar de `{prompt}` si el comando lo incluye— y entrega su salida estándar
//! a trozos según llega, igual que el streaming SSE de [`ClaudeClient`].
//!
//! El proceso se ejecuta sin shell (no hay interpolación de la voz del
//! usuario en un intérprete) y el barge-in lo mata en el acto.
//!
//! [`ClaudeClient`]: crate::brain::ClaudeClient

use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::brain::client::{BrainError, CancellationToken, Completion, Message, SYSTEM_PROMPT};
use crate::brain::DEFAULT_HISTORY_TURNS;

/// Comando por defecto: Antigravity en modo no interactivo y con esfuerzo
/// bajo para minimizar la latencia conversacional (HU-19).
pub const DEFAULT_CLI_COMMAND: &str = "agy --effort low --dangerously-skip-permissions -p";
/// Marcador que se reemplaza por el *prompt* dentro del comando.
pub const PROMPT_PLACEHOLDER: &str = "{prompt}";
/// Tiempo máximo de un turno antes de matar el proceso.
pub const DEFAULT_CLI_TIMEOUT: Duration = Duration::from_secs(120);
/// Bytes de stderr que se conservan para el mensaje de error.
const STDERR_TAIL: usize = 2_048;

/// Separa un comando en argumentos respetando comillas simples y dobles.
///
/// Devuelve `None` si hay comillas sin cerrar o el comando está vacío.
pub fn split_command(command: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut quote: Option<char> = None;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => current.push(chars.next()?),
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_arg = true;
            }
            (None, '\\') => {
                current.push(chars.next()?);
                in_arg = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            (None, c) => {
                current.push(c);
                in_arg = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_arg {
        args.push(current);
    }
    (!args.is_empty()).then_some(args)
}

/// Cerebro que delega cada turno en un CLI local.
pub struct CliBrain {
    program: String,
    args: Vec<String>,
    system_prompt: String,
    history_turns: usize,
    history: Mutex<VecDeque<Message>>,
    timeout: Duration,
    cancel: CancellationToken,
    /// Proceso en curso (para matarlo desde `cancel`).
    child: Arc<Mutex<Option<Child>>>,
}

impl CliBrain {
    /// Crea el cerebro a partir de un comando como `agy --effort low --dangerously-skip-permissions -p`.
    pub fn new(command: &str) -> Result<Self, BrainError> {
        let mut parts = split_command(command)
            .ok_or_else(|| BrainError::Cli(format!("comando CLI inválido: {command:?}")))?;
        let program = parts.remove(0);
        Ok(Self {
            program,
            args: parts,
            system_prompt: SYSTEM_PROMPT.to_string(),
            history_turns: DEFAULT_HISTORY_TURNS,
            history: Mutex::new(VecDeque::new()),
            timeout: DEFAULT_CLI_TIMEOUT,
            cancel: CancellationToken::new(),
            child: Arc::new(Mutex::new(None)),
        })
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_history_turns(mut self, turns: usize) -> Self {
        self.history_turns = turns;
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    /// Programa que se ejecuta (primer elemento del comando).
    pub fn program(&self) -> &str {
        &self.program
    }

    /// `true` si el programa existe (ruta explícita o en el `PATH`).
    pub fn is_available(&self) -> bool {
        resolve_program(&self.program).is_some()
    }

    /// Devuelve una copia del historial (para inspección/pruebas).
    pub fn history(&self) -> Vec<Message> {
        self.lock_history().iter().cloned().collect()
    }

    /// Cancela el turno en curso matando el proceso. Idempotente.
    pub fn cancel(&self) {
        self.cancel.cancel();
        if let Some(child) = self.lock_child().as_mut() {
            kill_tree(child);
        }
    }

    /// Ejecuta un turno y entrega la salida a trozos por `on_token`.
    pub fn stream(
        &self,
        user_text: &str,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError> {
        self.cancel.reset();
        let prompt = self.build_prompt(user_text);
        let mut child = self.spawn(&prompt)?;
        let mut stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *self.lock_child() = Some(child);
        // Barge-in llegado entre `reset` y el registro del proceso.
        if self.cancel.is_cancelled() {
            self.kill_and_reap();
            return Err(BrainError::Cancelled);
        }

        let stderr_reader = stderr.map(|mut err| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = err.read_to_end(&mut buf);
                let start = buf.len().saturating_sub(STDERR_TAIL);
                String::from_utf8_lossy(&buf[start..]).trim().to_string()
            })
        });
        let watchdog = self.spawn_watchdog();

        let mut text = String::new();
        let mut pending = Vec::new();
        let mut chunk = [0u8; 1_024];
        if let Some(out) = stdout.as_mut() {
            loop {
                let n = match out.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                pending.extend_from_slice(&chunk[..n]);
                let valid = utf8_prefix_len(&pending);
                if valid > 0 {
                    let piece = String::from_utf8_lossy(&pending[..valid]).into_owned();
                    pending.drain(..valid);
                    if !self.cancel.is_cancelled() {
                        on_token(&piece);
                    }
                    text.push_str(&piece);
                }
            }
        }
        if !pending.is_empty() {
            let piece = String::from_utf8_lossy(&pending).into_owned();
            if !self.cancel.is_cancelled() {
                on_token(&piece);
            }
            text.push_str(&piece);
        }

        let status = self.lock_child().take().map(|mut child| child.wait());
        let timed_out = watchdog.finish();
        let stderr_tail = stderr_reader
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();

        if self.cancel.is_cancelled() {
            return Err(BrainError::Cancelled);
        }
        if timed_out {
            return Err(BrainError::Cli(format!(
                "`{}` no respondió en {} s",
                self.program,
                self.timeout.as_secs()
            )));
        }
        match status {
            Some(Ok(status)) if status.success() => {}
            Some(Ok(status)) => {
                let detail = if stderr_tail.is_empty() {
                    status.to_string()
                } else {
                    stderr_tail
                };
                return Err(BrainError::Cli(format!(
                    "`{}` falló: {detail}",
                    self.program
                )));
            }
            Some(Err(e)) => return Err(BrainError::Cli(e.to_string())),
            None => return Err(BrainError::Cancelled),
        }

        let text = text.trim().to_string();
        // Respuestas vacías o errores del CLI (p. ej. permisos de jetski)
        // envenenarían el historial de los turnos siguientes.
        let cli_error = text
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("jetski:"));
        if !text.is_empty() && !cli_error {
            self.record_turn(user_text, &text);
        }
        Ok(Completion {
            text,
            stop_reason: Some("end_turn".to_string()),
            ..Completion::default()
        })
    }

    /// Persona + transcripción reciente + turno actual, en texto plano.
    fn build_prompt(&self, user_text: &str) -> String {
        let mut prompt = self.system_prompt.clone();
        let history = self.lock_history();
        if !history.is_empty() {
            prompt.push_str("\n\nConversación previa:\n");
            for message in history.iter() {
                let who = if message.role == "user" {
                    "Usuario"
                } else {
                    "Jarvis"
                };
                prompt.push_str(&format!("{who}: {}\n", message.content));
            }
        }
        drop(history);
        prompt.push_str(&format!(
            "\nUsa tus herramientas (run_command, herramientas MCP, etc.) para realizar las acciones solicitadas en el equipo cuando corresponda. Responde únicamente con lo que Jarvis debe decir en voz alta al usuario en una frase breve en español.\nUsuario: {user_text}"
        ));
        prompt
    }

    /// Argumentos finales: `{prompt}` se reemplaza; si no aparece, se añade.
    fn build_args(&self, prompt: &str) -> Vec<String> {
        let mut replaced = false;
        let mut args: Vec<String> = self
            .args
            .iter()
            .map(|arg| {
                if arg.contains(PROMPT_PLACEHOLDER) {
                    replaced = true;
                    arg.replace(PROMPT_PLACEHOLDER, prompt)
                } else {
                    arg.clone()
                }
            })
            .collect();
        if !replaced {
            args.push(prompt.to_string());
        }
        args
    }

    fn spawn(&self, prompt: &str) -> Result<Child, BrainError> {
        let program = resolve_program(&self.program).ok_or_else(|| {
            BrainError::Cli(format!("no se encontró `{}` en el PATH", self.program))
        })?;
        let mut command = Command::new(program);
        // Grupo de procesos propio: el barge-in mata también a los hijos
        // del CLI, que heredan la tubería de salida.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        command
            .args(self.build_args(prompt))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("NO_COLOR", "1")
            .spawn()
            .map_err(|e| BrainError::Cli(format!("no se pudo ejecutar `{}`: {e}", self.program)))
    }

    /// Mata el proceso si supera `timeout`; [`Watchdog::finish`] lo detiene.
    fn spawn_watchdog(&self) -> Watchdog {
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let child = Arc::clone(&self.child);
        let deadline = Instant::now() + self.timeout;
        let handle = std::thread::spawn(move || {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match done_rx.recv_timeout(remaining) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(child) = child
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .as_mut()
                    {
                        kill_tree(child);
                    }
                    true
                }
                _ => false,
            }
        });
        Watchdog { done_tx, handle }
    }

    fn kill_and_reap(&self) {
        if let Some(mut child) = self.lock_child().take() {
            kill_tree(&mut child);
            let _ = child.wait();
        }
    }

    fn record_turn(&self, user_text: &str, assistant_text: &str) {
        let mut history = self.lock_history();
        history.push_back(Message {
            role: "user".to_string(),
            content: user_text.to_string(),
        });
        history.push_back(Message {
            role: "assistant".to_string(),
            content: assistant_text.to_string(),
        });
        let max = self.history_turns.saturating_mul(2);
        while history.len() > max {
            history.pop_front();
        }
    }

    fn lock_history(&self) -> std::sync::MutexGuard<'_, VecDeque<Message>> {
        self.history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_child(&self) -> std::sync::MutexGuard<'_, Option<Child>> {
        self.child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Vigilante de *timeout* de un turno.
struct Watchdog {
    done_tx: std::sync::mpsc::Sender<()>,
    handle: std::thread::JoinHandle<bool>,
}

impl Watchdog {
    /// Detiene el vigilante; `true` si alcanzó a matar el proceso.
    fn finish(self) -> bool {
        let _ = self.done_tx.send(());
        self.handle.join().unwrap_or(false)
    }
}

/// Mata el proceso y su grupo (los hijos que lanzó el CLI).
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: `kill` solo envía una señal; el grupo es el del hijo
        // (creado con `process_group(0)`), nunca el de Jarvis.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

/// Largo del prefijo UTF-8 válido (deja fuera un carácter cortado al final).
fn utf8_prefix_len(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        // Bytes inválidos en medio: se entregan con reemplazo.
        Err(_) => bytes.len(),
    }
}

/// Ruta del programa: tal cual si tiene `/`, si no se busca en el `PATH`.
fn resolve_program(program: &str) -> Option<std::path::PathBuf> {
    let path = std::path::Path::new(program);
    if program.contains('/') {
        return path.is_file().then(|| path.to_path_buf());
    }
    let dirs = std::env::var_os("PATH")?;
    std::env::split_paths(&dirs)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_handles_quotes_and_escapes() {
        assert_eq!(
            split_command("agy -p"),
            Some(vec!["agy".to_string(), "-p".to_string()])
        );
        assert_eq!(
            split_command(r#"  cli --model "opus 5" 'a b' c\ d "" "#),
            Some(
                ["cli", "--model", "opus 5", "a b", "c d", ""]
                    .map(String::from)
                    .to_vec()
            )
        );
        assert_eq!(split_command("   "), None);
        assert_eq!(split_command("agy \"sin cerrar"), None);
    }

    #[test]
    fn prompt_is_appended_or_replaces_the_placeholder() {
        let brain = CliBrain::new("agy -p").unwrap();
        assert_eq!(brain.program(), "agy");
        assert_eq!(brain.build_args("hola"), vec!["-p", "hola"]);

        let brain = CliBrain::new("tool --q={prompt} --fast").unwrap();
        assert_eq!(brain.build_args("hola"), vec!["--q=hola", "--fast"]);
    }

    #[test]
    fn utf8_split_across_chunks_is_held_back() {
        let bytes = "añ".as_bytes();
        assert_eq!(utf8_prefix_len(&bytes[..2]), 1);
        assert_eq!(utf8_prefix_len(bytes), bytes.len());
    }

    #[test]
    fn streams_stdout_and_records_history() {
        let brain = CliBrain::new("echo").unwrap();
        let mut pieces = String::new();
        let completion = brain
            .stream("qué hora es", &mut |t| pieces.push_str(t))
            .expect("echo responde");
        assert!(completion.text.ends_with("Usuario: qué hora es"));
        assert_eq!(pieces.trim(), completion.text);
        let history = brain.history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "qué hora es");

        // El segundo turno incluye la conversación previa en el prompt.
        let second = brain.stream("y mañana", &mut |_| {}).unwrap();
        assert!(second.text.contains("Conversación previa:"));
        assert!(second.text.contains("Usuario: qué hora es"));
    }

    #[test]
    fn failing_command_reports_stderr() {
        let brain = CliBrain::new("sh -c {prompt}").unwrap();
        // El prompt no es un script válido: sh falla y lo dice por stderr.
        let err = brain.stream("(", &mut |_| {}).unwrap_err();
        assert!(matches!(err, BrainError::Cli(_)), "{err:?}");
        assert!(brain.history().is_empty());
    }

    #[test]
    fn missing_program_is_a_cli_error() {
        let brain = CliBrain::new("no-existe-este-cli-jarvis -p").unwrap();
        assert!(!brain.is_available());
        let err = brain.stream("hola", &mut |_| {}).unwrap_err();
        assert!(matches!(err, BrainError::Cli(_)));
    }

    #[test]
    fn timeout_kills_a_hung_command() {
        let brain = CliBrain::new("sh -c 'sleep 5' {prompt}")
            .unwrap()
            .with_timeout(Duration::from_millis(100));
        let started = Instant::now();
        let err = brain.stream("", &mut |_| {}).unwrap_err();
        assert!(matches!(err, BrainError::Cli(_)), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn cancel_kills_the_process_immediately() {
        let brain = Arc::new(CliBrain::new("sh -c 'sleep 5; true' {prompt}").unwrap());
        let worker = {
            let brain = Arc::clone(&brain);
            std::thread::spawn(move || brain.stream("", &mut |_| {}))
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while brain.lock_child().is_none() {
            assert!(Instant::now() < deadline, "el proceso no arrancó");
            std::thread::sleep(Duration::from_millis(5));
        }
        let started = Instant::now();
        brain.cancel();
        let result = worker.join().unwrap();
        assert!(matches!(result, Err(BrainError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
