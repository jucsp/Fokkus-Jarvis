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
//! Con Antigravity (`agy … -p`) en modo voz, el cerebro mantiene además una
//! [`AgySession`] persistente: el proceso se lanza por adelantado
//! ([`CliBrain::warm_up`]) y atiende cada turno por NDJSON, de modo que el
//! arranque en frío (~3,5 s) sale del camino crítico y los `text_delta` llegan
//! al TTS por frases en cuanto el modelo los emite.
//!
//! [`ClaudeClient`]: crate::brain::ClaudeClient

use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::brain::client::{
    BrainError, CancellationToken, Completion, Message, SUBAGENT_SYSTEM_PROMPT, SYSTEM_PROMPT,
};
use crate::brain::session::{parse_event, session_args, AgySession, SessionEvent};
use crate::brain::DEFAULT_HISTORY_TURNS;

/// Comando por defecto: Antigravity en modo no interactivo y con esfuerzo
/// bajo para minimizar la latencia conversacional (HU-19).
pub const DEFAULT_CLI_COMMAND: &str = "agy --effort low --dangerously-skip-permissions -p";
/// Marcador que se reemplaza por el *prompt* dentro del comando.
pub const PROMPT_PLACEHOLDER: &str = "{prompt}";
/// Tiempo máximo de un turno antes de matar el proceso.
pub const DEFAULT_CLI_TIMEOUT: Duration = Duration::from_secs(120);
/// Variable de entorno que marca al proceso hijo como subagente de fondo
/// (HU-60). `fokkus-jarvis dev run` la usa para negarse a crear otra tarea.
pub const SUBSESSION_ENV: &str = "JARVIS_SUBSESSION_ID";
/// Valor por defecto cuando el llamador no da un identificador de sub-sesión.
pub const DEFAULT_SUBSESSION_ID: &str = "1";
/// Bytes de stderr que se conservan para el mensaje de error.
const STDERR_TAIL: usize = 2_048;
/// Cada cuánto revisa la sesión persistente el barge-in y el *timeout*.
const SESSION_POLL: Duration = Duration::from_millis(25);

/// Papel del cerebro CLI dentro de Jarvis (HU-60).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CliBrainMode {
    /// Turno hablado: persona de voz y respuesta de una frase para el TTS.
    #[default]
    Voice,
    /// Subagente de fondo: prompt propio, sin coletilla de voz y con
    /// [`SUBSESSION_ENV`] en el entorno del proceso hijo.
    Worker,
}

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
    mode: CliBrainMode,
    /// Identificador de sub-sesión que se exporta al proceso hijo (HU-60).
    subsession_id: Option<String>,
    cancel: CancellationToken,
    /// Proceso en curso (para matarlo desde `cancel`).
    child: Arc<Mutex<Option<Child>>>,
    /// Argumentos de la sesión persistente si el comando es `agy … -p`.
    session_args: Option<Vec<String>>,
    /// Proceso `agy` caliente entre turnos (solo en modo voz).
    session: Mutex<Option<AgySession>>,
}

impl CliBrain {
    /// Crea el cerebro a partir de un comando como `agy --effort low --dangerously-skip-permissions -p`.
    pub fn new(command: &str) -> Result<Self, BrainError> {
        let mut parts = split_command(command)
            .ok_or_else(|| BrainError::Cli(format!("comando CLI inválido: {command:?}")))?;
        let program = parts.remove(0);
        let session_args = session_args(&program, &parts, PROMPT_PLACEHOLDER);
        Ok(Self {
            program,
            args: parts,
            system_prompt: SYSTEM_PROMPT.to_string(),
            history_turns: DEFAULT_HISTORY_TURNS,
            history: Mutex::new(VecDeque::new()),
            timeout: DEFAULT_CLI_TIMEOUT,
            mode: CliBrainMode::Voice,
            subsession_id: None,
            cancel: CancellationToken::new(),
            child: Arc::new(Mutex::new(None)),
            session_args,
            session: Mutex::new(None),
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

    /// Cambia el papel del cerebro (HU-60). En [`CliBrainMode::Worker`] se
    /// sustituye la persona de voz por [`SUBAGENT_SYSTEM_PROMPT`]; si el
    /// llamador quiere otro prompt, encadena [`with_system_prompt`] después.
    ///
    /// [`with_system_prompt`]: Self::with_system_prompt
    pub fn with_mode(mut self, mode: CliBrainMode) -> Self {
        self.mode = mode;
        if mode == CliBrainMode::Worker {
            self.system_prompt = SUBAGENT_SYSTEM_PROMPT.to_string();
        }
        self
    }

    /// Atajo de [`with_mode`](Self::with_mode) para subagentes de fondo.
    pub fn with_worker_mode(self, worker: bool) -> Self {
        let mode = if worker {
            CliBrainMode::Worker
        } else {
            CliBrainMode::Voice
        };
        self.with_mode(mode)
    }

    /// Identificador de sub-sesión que verá el proceso hijo en
    /// [`SUBSESSION_ENV`] (por defecto [`DEFAULT_SUBSESSION_ID`]).
    pub fn with_subsession_id(mut self, id: impl Into<String>) -> Self {
        self.subsession_id = Some(id.into());
        self
    }

    /// Papel actual del cerebro.
    pub fn mode(&self) -> CliBrainMode {
        self.mode
    }

    /// Programa que se ejecuta (primer elemento del comando).
    pub fn program(&self) -> &str {
        &self.program
    }

    /// `true` si el programa existe (ruta explícita o en el `PATH`).
    pub fn is_available(&self) -> bool {
        resolve_program(&self.program).is_some()
    }

    /// `true` si los turnos de voz van por una [`AgySession`] persistente.
    pub fn is_persistent(&self) -> bool {
        self.mode == CliBrainMode::Voice && self.session_args.is_some()
    }

    /// Lanza por adelantado el proceso de la sesión persistente para que el
    /// primer turno no pague el arranque en frío. Sin sesión, no hace nada.
    pub fn warm_up(&self) {
        if !self.is_persistent() {
            return;
        }
        let mut slot = self.lock_session();
        if slot.as_mut().is_some_and(AgySession::is_alive) {
            return;
        }
        if let Some(dead) = slot.take() {
            dead.kill();
        }
        *slot = self.spawn_session().ok();
    }

    /// Devuelve una copia del historial (para inspección/pruebas).
    pub fn history(&self) -> Vec<Message> {
        self.lock_history().iter().cloned().collect()
    }

    /// Cancela el turno en curso matando el proceso. Idempotente.
    ///
    /// Con sesión persistente, el bucle del turno ve el token en menos de
    /// [`SESSION_POLL`], mata el proceso y deja otro calentando.
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
        self.stream_with_context(None, user_text, on_token)
    }

    /// Como [`stream`](Self::stream) con un bloque de contexto efímero en el
    /// prompt de este turno (HU-65). El historial guarda solo `user_text`.
    pub fn stream_with_context(
        &self,
        context: Option<&str>,
        user_text: &str,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError> {
        self.cancel.reset();
        if self.is_persistent() {
            return self.stream_session(context, user_text, on_token);
        }
        let prompt = self.build_prompt(context, user_text);
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

        Ok(self.complete(user_text, &text))
    }

    /// Cierra un turno exitoso: recorta y lo guarda en el historial.
    fn complete(&self, user_text: &str, text: &str) -> Completion {
        let text = text.trim().to_string();
        // Respuestas vacías o errores del CLI (p. ej. permisos de jetski)
        // envenenarían el historial de los turnos siguientes.
        let cli_error = text
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("jetski:"));
        if !text.is_empty() && !cli_error {
            self.record_turn(user_text, &text);
        }
        Completion {
            text,
            stop_reason: Some("end_turn".to_string()),
            ..Completion::default()
        }
    }

    /// Turno por la sesión persistente: el primer mensaje de cada proceso
    /// lleva persona e historial; los siguientes solo el turno, porque `agy`
    /// ya conserva la conversación.
    fn stream_session(
        &self,
        context: Option<&str>,
        user_text: &str,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError> {
        let warm = self.lock_session().take();
        let mut session = match warm {
            Some(mut session) => {
                if session.is_alive() {
                    session
                } else {
                    session.kill();
                    self.spawn_session()?
                }
            }
            None => self.spawn_session()?,
        };
        let message = if session.turns() == 0 {
            self.build_prompt(context, user_text)
        } else {
            self.build_turn(context, user_text).trim_start().to_string()
        };
        if let Err(e) = session.send(&message) {
            let detail = session.stderr_tail();
            session.kill();
            self.warm_up();
            return Err(BrainError::Cli(format!(
                "`{}` cerró la sesión: {}",
                self.program,
                if detail.is_empty() {
                    e.to_string()
                } else {
                    detail
                }
            )));
        }

        let deadline = Instant::now() + self.timeout;
        let mut text = String::new();
        let outcome = loop {
            if self.cancel.is_cancelled() {
                break Err(BrainError::Cancelled);
            }
            if Instant::now() >= deadline {
                break Err(BrainError::Cli(format!(
                    "`{}` no respondió en {} s",
                    self.program,
                    self.timeout.as_secs()
                )));
            }
            let line = match session.lines().recv_timeout(SESSION_POLL) {
                Ok(line) => line,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    let detail = session.stderr_tail();
                    break Err(BrainError::Cli(format!(
                        "`{}` terminó a mitad del turno{}",
                        self.program,
                        if detail.is_empty() {
                            String::new()
                        } else {
                            format!(": {detail}")
                        }
                    )));
                }
            };
            match parse_event(&line) {
                SessionEvent::Delta(piece) => {
                    on_token(&piece);
                    text.push_str(&piece);
                }
                SessionEvent::Done(Ok(response)) => {
                    // Sin deltas (respuesta de un solo bloque), se entrega entera.
                    if text.trim().is_empty() && !response.trim().is_empty() {
                        on_token(&response);
                        text = response;
                    }
                    break Ok(());
                }
                SessionEvent::Done(Err(error)) => {
                    break Err(BrainError::Cli(format!(
                        "`{}` falló: {error}",
                        self.program
                    )))
                }
                SessionEvent::Other => {}
            }
        };

        // Cancelado, vencido o caído: el proceso queda en un estado
        // desconocido. Se descarta y otro empieza a calentar ya.
        // Tras `history_turns` turnos se recicla para acotar su contexto.
        let recycle = session.turns() >= self.history_turns.max(1);
        if outcome.is_err() || recycle {
            session.kill();
            self.warm_up();
        } else {
            *self.lock_session() = Some(session);
        }
        outcome.map(|()| self.complete(user_text, &text))
    }

    /// Persona + transcripción reciente + turno actual, en texto plano.
    ///
    /// En [`CliBrainMode::Worker`] no se añade la coletilla de voz
    /// ("responde únicamente … en una frase breve"): el subagente recibe su
    /// propio prompt y el formato pactado con el gestor de tareas (HU-60).
    fn build_prompt(&self, context: Option<&str>, user_text: &str) -> String {
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
        prompt.push_str(&self.build_turn(context, user_text));
        prompt
    }

    /// Contexto efímero + coletilla de modo + turno actual.
    fn build_turn(&self, context: Option<&str>, user_text: &str) -> String {
        let mut prompt = String::new();
        if let Some(ctx) = context.filter(|ctx| !ctx.trim().is_empty()) {
            prompt.push_str(&format!("\n\n{ctx}\n"));
        }
        match self.mode {
            CliBrainMode::Voice => prompt.push_str(&format!(
                "\nUsa tus herramientas (run_command, herramientas MCP, etc.) para realizar \
                 las acciones solicitadas en el equipo cuando corresponda. Usa puntuación \
                 expresiva (¡!, ¿?, …). Tu tono por defecto es normal y neutro: usa \
                 directivas de prosodia ([ánimo:normal|alegre|serio|empático|entusiasta|\
                 calmo|curioso|asustado] [ritmo:lento|normal|rápido] [pausa:ms]) solo \
                 cuando el usuario pida una emoción concreta o el contenido lo justifique, \
                 y vuelve a [ánimo:normal] después. Nómbralas entre corchetes y nunca las \
                 leas en voz alta. Responde únicamente con lo que Jarvis debe decir en voz \
                 alta al usuario en una frase breve en español.\nUsuario: {user_text}"
            )),
            CliBrainMode::Worker => prompt.push_str(&format!(
                "\nTarea encomendada a esta sub-sesión:\n{user_text}"
            )),
        }
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
        let mut command = self.command(self.build_args(prompt))?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
            .spawn()
            .map_err(|e| BrainError::Cli(format!("no se pudo ejecutar `{}`: {e}", self.program)))
    }

    /// Lanza el proceso de la sesión persistente.
    fn spawn_session(&self) -> Result<AgySession, BrainError> {
        let args = self.session_args.clone().unwrap_or_default();
        AgySession::spawn(self.command(args)?)
            .map_err(|e| BrainError::Cli(format!("no se pudo ejecutar `{}`: {e}", self.program)))
    }

    /// Comando sin shell, con su grupo de procesos y el entorno *headless*.
    fn command(&self, args: Vec<String>) -> Result<Command, BrainError> {
        let program = resolve_program(&self.program).ok_or_else(|| {
            BrainError::Cli(format!("no se encontró `{}` en el PATH", self.program))
        })?;
        let mut command = Command::new(program);
        // Grupo de procesos propio: el barge-in mata también a los hijos
        // del CLI, que heredan la tubería de salida.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        command
            .args(args)
            .env("NO_COLOR", "1")
            // HU-58: el subagente corre sin interfaz. Ningún MCP (p. ej. el de
            // Google Workspace) ni script hijo debe abrir un navegador
            // (`xdg-open`, `webbrowser.open`) en medio de la pantalla.
            .env("BROWSER", "/bin/true")
            .env("NO_BROWSER", "1");
        // HU-60: el subagente sabe que corre dentro de una sub-sesión, de modo
        // que `fokkus-jarvis dev run` se niega a crear otra tarea (antirrecursión).
        if self.mode == CliBrainMode::Worker {
            command.env(
                SUBSESSION_ENV,
                self.subsession_id
                    .as_deref()
                    .unwrap_or(DEFAULT_SUBSESSION_ID),
            );
        }
        Ok(command)
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

    fn lock_session(&self) -> std::sync::MutexGuard<'_, Option<AgySession>> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for CliBrain {
    /// Un cerebro reemplazado (cambio de ajustes, salida) no deja un `agy`
    /// huérfano esperando turnos.
    fn drop(&mut self) {
        if let Some(session) = self.lock_session().take() {
            session.kill();
        }
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
pub(crate) fn kill_tree(child: &mut Child) {
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
    fn ephemeral_context_goes_in_the_prompt_but_not_the_history_hu65() {
        let roster = "[Sub-sesiones activas]\n#1 la revisión de PR de Felipe — terminada (4 min)";
        let brain = CliBrain::new("echo").unwrap();
        let prompt = brain.build_prompt(Some(roster), "¿encontró algo grave?");
        assert!(prompt.contains(roster), "{prompt}");
        assert!(
            prompt.ends_with("Usuario: ¿encontró algo grave?"),
            "{prompt}"
        );
        assert!(!brain.build_prompt(None, "hola").contains("#1 la revisión"));

        // El prompt va como `$0`: el script responde fijo.
        let brain = CliBrain::new("sh -c 'echo Nada grave.' {prompt}").unwrap();
        brain
            .stream_with_context(Some(roster), "¿encontró algo grave?", &mut |_| {})
            .expect("sh responde");
        let history = brain.history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "¿encontró algo grave?");
        assert_eq!(history[1].content, "Nada grave.");
        let next = brain.build_prompt(None, "gracias");
        assert!(!next.contains("#1 la revisión"), "{next}");
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
    fn child_runs_headless_without_browser_hu58() {
        // El prompt va como `$0`: el script solo imprime el entorno del hijo.
        let brain = CliBrain::new("sh -c 'echo \"$BROWSER|$NO_BROWSER\"' {prompt}").unwrap();
        let completion = brain.stream("hola", &mut |_| {}).expect("sh responde");
        assert_eq!(completion.text.trim(), "/bin/true|1");
    }

    /// HU-73: en la sesión persistente el primer mensaje lleva la persona, pero
    /// los turnos siguientes solo llevan [`CliBrain::build_turn`]. Si el
    /// recordatorio no repite las directivas de prosodia, `agy` deja de
    /// emitirlas a partir del segundo turno y la voz vuelve a sonar plana; y si
    /// no fija el tono normal por defecto, el modelo se queda con la última
    /// emoción del turno anterior.
    #[test]
    fn every_voice_turn_reminds_the_model_about_prosody_hu71() {
        let brain = CliBrain::new("agy -p").unwrap();
        for text in ["hola", "cuéntame algo asustado", "y ahora enojado"] {
            let turn = brain.build_turn(None, text);
            for needle in ["ánimo:", "asustado", "ritmo:", "pausa:", "expresiva"] {
                assert!(turn.contains(needle), "falta {needle:?} en {turn}");
            }
            // HU-73: el recordatorio también fija el ánimo por defecto.
            for needle in [
                "normal y neutro",
                "[ánimo:normal|",
                "vuelve a [ánimo:normal]",
            ] {
                assert!(turn.contains(needle), "falta {needle:?} en {turn}");
            }
            // La coletilla de voz sigue cerrando el turno con el texto del
            // usuario: los tests de historial y de contexto dependen de ello.
            assert!(turn.ends_with(&format!("Usuario: {text}")), "{turn}");
        }
        // El subagente de fondo no recibe prosodia: su salida no se habla.
        let worker = CliBrain::new("agy -p").unwrap().with_worker_mode(true);
        let turn = worker.build_turn(None, "revisa las PR");
        assert!(!turn.contains("ánimo:"), "{turn}");
        assert!(!turn.contains("asustado"), "{turn}");
    }

    #[test]
    fn worker_mode_drops_the_voice_persona_and_marks_the_sub_session_hu60() {
        let brain = CliBrain::new("agy -p").unwrap();
        assert_eq!(brain.mode(), CliBrainMode::Voice);
        let voice = brain.build_prompt(None, "revisa las PR de Felipe");
        assert!(voice.contains("una frase breve"), "modo voz intacto");
        assert!(voice.contains("fokkus-jarvis dev run"));

        let worker = CliBrain::new("agy -p").unwrap().with_worker_mode(true);
        assert_eq!(worker.mode(), CliBrainMode::Worker);
        let prompt = worker.build_prompt(None, "revisa las PR de Felipe");
        assert!(!prompt.contains("frase breve"), "{prompt}");
        assert!(!prompt.contains("fokkus-jarvis dev run"), "{prompt}");
        assert!(prompt.contains("subagente de fondo"), "{prompt}");
        assert!(prompt.contains("gh pr view"), "{prompt}");
        // La constitución de privacidad se conserva íntegra.
        assert!(prompt.contains("~/.ssh/"), "{prompt}");
    }

    #[test]
    fn worker_child_sees_the_sub_session_id_hu60() {
        let script = "sh -c 'echo \"$JARVIS_SUBSESSION_ID\"' {prompt}";
        let voice = CliBrain::new(script).unwrap();
        let completion = voice.stream("hola", &mut |_| {}).expect("sh responde");
        // El turno de voz no marca nada: el hijo hereda el entorno del proceso.
        let inherited = std::env::var(SUBSESSION_ENV).unwrap_or_default();
        assert_eq!(completion.text.trim(), inherited);

        let worker = CliBrain::new(script)
            .unwrap()
            .with_worker_mode(true)
            .with_subsession_id("7");
        let completion = worker.stream("hola", &mut |_| {}).expect("sh responde");
        assert_eq!(completion.text.trim(), "7");

        // Sin identificador explícito se usa el valor por defecto.
        let worker = CliBrain::new(script)
            .unwrap()
            .with_mode(CliBrainMode::Worker);
        let completion = worker.stream("hola", &mut |_| {}).expect("sh responde");
        assert_eq!(completion.text.trim(), DEFAULT_SUBSESSION_ID);
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

    /// `agy` de mentira: atiende turnos NDJSON por stdin y responde con el
    /// número de turno, su PID y si el mensaje traía la persona Jarvis.
    fn fake_agy(name: &str, reply: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("jarvis-agy-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agy");
        let script = format!(
            "#!/bin/sh\nn=0\nwhile IFS= read -r line; do\n  n=$((n+1))\n  \
             case \"$line\" in *'Eres Jarvis'*) who=persona;; *) who=corto;; esac\n  {reply}\ndone\n"
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Otro test que hace `fork` en paralelo puede heredar el descriptor de
        // escritura hasta su `exec` (ETXTBSY): se espera a que se libere.
        for _ in 0..100 {
            match Command::new(&path).stdin(Stdio::null()).status() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => break,
            }
        }
        path
    }

    const ECHO_TURN: &str = r#"printf '{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"turno %s "}}\n' "$n"
  printf '{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"%s %s."}}\n' "$who" "$$"
  printf '{"event":"result","result":{"status":"SUCCESS","response":"x"}}\n'"#;

    #[test]
    fn voice_turns_reuse_one_warm_agy_process() {
        let agy = fake_agy("persist", ECHO_TURN);
        let brain = CliBrain::new(&format!("{} --model m -p", agy.display())).unwrap();
        assert!(brain.is_persistent());
        brain.warm_up();
        let mut pieces = Vec::new();
        let first = brain
            .stream("hola", &mut |t| pieces.push(t.to_string()))
            .expect("primer turno");
        // Streaming real: dos deltas, no un bloque al final.
        assert_eq!(pieces.len(), 2, "{pieces:?}");
        let second = brain
            .stream("y mañana", &mut |_| {})
            .expect("segundo turno");
        let pid = |text: &str| text.rsplit(' ').next().unwrap().to_string();
        assert!(first.text.starts_with("turno 1 persona"), "{}", first.text);
        assert!(second.text.starts_with("turno 2 corto"), "{}", second.text);
        assert_eq!(pid(&first.text), pid(&second.text), "mismo proceso");
        assert_eq!(brain.history().len(), 4);

        let alive = pid(&second.text)
            .trim_end_matches('.')
            .parse::<i32>()
            .unwrap();
        drop(brain);
        std::thread::sleep(Duration::from_millis(50));
        // SAFETY: señal 0 solo consulta si el PID existe.
        let exists = unsafe { libc::kill(alive, 0) } == 0;
        assert!(!exists, "el agy caliente no debe quedar huérfano");
    }

    #[test]
    fn session_is_recycled_and_primed_again_after_history_turns() {
        let agy = fake_agy("recycle", ECHO_TURN);
        let brain = CliBrain::new(&format!("{} -p", agy.display()))
            .unwrap()
            .with_history_turns(2);
        let texts: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|t| brain.stream(t, &mut |_| {}).unwrap().text)
            .collect();
        assert!(texts[1].starts_with("turno 2 corto"), "{texts:?}");
        // Proceso nuevo: vuelve a recibir persona + historial.
        assert!(texts[2].starts_with("turno 1 persona"), "{texts:?}");
    }

    #[test]
    fn session_error_result_is_a_cli_error_and_keeps_history_clean() {
        let agy = fake_agy(
            "error",
            r#"printf '{"event":"result","result":{"status":"ERROR","error":"cuota agotada"}}\n'"#,
        );
        let brain = CliBrain::new(&format!("{} -p", agy.display())).unwrap();
        let err = brain.stream("hola", &mut |_| {}).unwrap_err();
        assert!(
            matches!(&err, BrainError::Cli(m) if m.contains("cuota agotada")),
            "{err:?}"
        );
        assert!(brain.history().is_empty());
    }

    #[test]
    fn barge_in_kills_the_session_turn_and_leaves_another_warming() {
        let agy = fake_agy("cancel", "sleep 5");
        let brain = Arc::new(CliBrain::new(&format!("{} -p", agy.display())).unwrap());
        brain.warm_up();
        let worker = {
            let brain = Arc::clone(&brain);
            std::thread::spawn(move || brain.stream("hola", &mut |_| {}))
        };
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        brain.cancel();
        let result = worker.join().unwrap();
        assert!(matches!(result, Err(BrainError::Cancelled)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(brain
            .lock_session()
            .as_mut()
            .is_some_and(AgySession::is_alive));
    }

    #[test]
    fn worker_mode_and_other_clis_stay_one_shot() {
        let agy = fake_agy("worker", ECHO_TURN);
        let worker = CliBrain::new(&format!("{} -p", agy.display()))
            .unwrap()
            .with_worker_mode(true);
        assert!(!worker.is_persistent());
        worker.warm_up();
        assert!(worker.lock_session().is_none());
        assert!(!CliBrain::new("echo -p").unwrap().is_persistent());
    }

    /// Perfil de latencia con el CLI real (`JARVIS_BENCH_CLI`, p. ej.
    /// `agy --model gemini-3.8-flash-medium --dangerously-skip-permissions -p`):
    /// compara un proceso por turno con la sesión persistente caliente.
    #[test]
    #[ignore = "requiere el CLI real y red (JARVIS_BENCH_CLI)"]
    fn bench_cli_time_to_first_token() {
        let command = std::env::var("JARVIS_BENCH_CLI").expect("JARVIS_BENCH_CLI");
        let turns = [
            "Jarvis, ¿qué día es hoy?",
            "Dime en dos frases qué es un agujero negro.",
            "¿Y quién lo predijo?",
        ];
        let measure = |brain: &CliBrain, label: &str| {
            for turn in turns {
                let started = Instant::now();
                let mut first = None;
                let done = brain
                    .stream(turn, &mut |_| {
                        first.get_or_insert_with(|| started.elapsed());
                    })
                    .expect("turno");
                eprintln!(
                    "[bench-cli] {label}: primer token {} ms, total {} ms → {:?}",
                    first.unwrap_or_default().as_millis(),
                    started.elapsed().as_millis(),
                    done.text
                );
            }
        };
        let one_shot = CliBrain::new(&command.replace(" -p", " --output-format text -p")).unwrap();
        assert!(!one_shot.is_persistent());
        measure(&one_shot, "proceso por turno");

        let persistent = CliBrain::new(&command).unwrap();
        assert!(persistent.is_persistent());
        persistent.warm_up();
        // El arranque en frío ocurre mientras el usuario aún habla.
        std::thread::sleep(Duration::from_secs(5));
        measure(&persistent, "sesión persistente");
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
