//! Cliente síncrono de la API de Mensajes de Anthropic (HU-6).
//!
//! Usa `ureq` (rustls/ring, sin tokio ni reqwest) contra
//! `POST https://api.anthropic.com/v1/messages` con `"stream": true`. El flujo
//! SSE se parsea con [`crate::brain::sse::SseParser`] y cada *delta* de texto se
//! entrega por *callback* según llega, de modo que el TTS de HU-8 pueda empezar
//! a hablar antes de que termine la respuesta.
//!
//! Seguridad de la API key (CA-6.3): la key se resuelve **parámetro → variable
//! de entorno → llavero del sistema** (servicio `fokkus-jarvis`, usuario
//! `anthropic_api_key`) y jamás se persiste en texto plano ni se loguea.

use std::collections::VecDeque;
use std::fmt;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::brain::sse::{SseEvent, SseParser};

/// Endpoint de la API de Mensajes.
pub const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
/// Versión de la API.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Beta para `fallbacks: "default"` (CA-6.1, tolerancia a fallos del proveedor).
pub const ANTHROPIC_BETA: &str = "server-side-fallback-2026-07-01";
/// Modelo por defecto (D-4 del SDD).
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
/// *Effort* por defecto para minimizar latencia en voz.
pub const DEFAULT_EFFORT: &str = "low";
/// Número de turnos de historial por defecto (un turno = usuario + asistente).
pub const DEFAULT_HISTORY_TURNS: usize = 20;
/// Máximo de reintentos ante 429/5xx/errores de red.
pub const DEFAULT_MAX_RETRIES: u32 = 3;
/// Tokens máximos de salida.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;
/// Nombre del servicio en el llavero del sistema.
pub const KEYRING_SERVICE: &str = "fokkus-jarvis";
/// Usuario del llavero que guarda la API key.
pub const KEYRING_USER: &str = "anthropic_api_key";
/// Variable de entorno con la API key.
pub const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// *System prompt* fijo de la persona Jarvis. Se lee en voz alta, así que no
/// admite markdown, listas ni bloques de código.
pub const SYSTEM_PROMPT: &str = concat!(
    "Eres Jarvis, el asistente de desarrollo de Fokkus. ",
    "Respondes SIEMPRE en español de Chile, de forma breve, directa y conversacional; ",
    "NUNCA respondas en inglés, aunque la pregunta o el contexto estén en inglés. ",
    "Entrega solo la respuesta final: NUNCA incluyas etiquetas <thought> ni <thinking>, ",
    "ni expliques tu razonamiento, tus pasos ni tus reflexiones internas. ",
    "Puedes ejecutar comandos en el equipo del usuario para abrir o cerrar aplicaciones ",
    "y realizar acciones del sistema; solo di que lo hiciste si de verdad lo ejecutaste. ",
    "Al abrir aplicaciones gráficas, lánzalas SIEMPRE en segundo plano y desacopladas ",
    "(systemd-run --user --collect <app>, gtk-launch <app> o nohup <cmd> >/dev/null 2>&1 &) ",
    "para no bloquear la salida del proceso. ",
    "Cuentas con ojos y manos nativos en el sistema a través del comando fokkus-jarvis: ",
    "para escribir texto en la ventana o caja activa ejecuta fokkus-jarvis input type \"<texto>\"; ",
    "para pulsar teclas o atajos (Enter, Tab, Escape, Ctrl+l, etc.) ejecuta ",
    "fokkus-jarvis input key <tecla>; para hacer clic ejecuta fokkus-jarvis input click [x y]; ",
    "para capturar y ver la pantalla ejecuta fokkus-jarvis capture [ruta.png]. ",
    "Cuando el usuario te pida escribir en pantalla, interactuar con cajas de texto, ",
    "buscadores o hacer clic, USA estas herramientas para realizar la acción directamente. ",
    "Si el usuario te pide desarrollos, construir software, ejecutar pipelines, ",
    "refactorizaciones o tareas técnicas de programación, NUNCA las ejecutes de forma ",
    "síncrona en este turno; despáchalas de inmediato ejecutando fokkus-jarvis dev run ",
    "\"<descripción clara y completa de la tarea>\" y responde en una frase breve que el ",
    "desarrollo partió en segundo plano y que avisarás al terminar. ",
    "Constitución de privacidad, sin excepciones: tienes PROHIBIDO de forma ABSOLUTA leer, ",
    "listar, buscar, copiar, mover, modificar o borrar cualquier cosa dentro de ~/Imágenes/, ",
    "~/Images/, ~/Pictures/, ~/Documentos/, ~/Documents/, ~/Videos/, ~/Música/, ~/Music/, ",
    "~/fotos/, ~/foto/, ~/.ssh/ y ~/.gnupg/; también llaves privadas y secretos ",
    "(id_rsa, id_ed25519, *.key, *.pem, archivos .env) y las variables de entorno globales. ",
    "Si una petición requiere tocar algo de eso, niégate brevemente. ",
    "Nunca uses markdown, listas, negritas, viñetas ni bloques de código: ",
    "tu respuesta se lee en voz alta. Sé conciso: una o dos frases salvo que te ",
    "pidan más detalle. ",
    "Habla con calidez y naturalidad, apoyándote en la puntuación expresiva ",
    "(¡!, ¿?, …). Si aporta expresividad, puedes guiar tu voz con directivas entre ",
    "corchetes que nunca se leen en voz alta: [ánimo:alegre|serio|empático|entusiasta|",
    "calmo|curioso] antes de la frase, [ritmo:lento|normal|rápido], [pausa:300] ",
    "(milisegundos), [respira] antes de una frase emotiva o larga, ",
    "[énfasis]palabra[/énfasis] y [susurro]texto[/susurro]. Son opcionales: usa como ",
    "máximo una o dos por respuesta y ningún otro texto entre corchetes. ",
    "Ejemplo: [ánimo:entusiasta]¡Listo, ya quedó! [pausa:300]Te aviso cuando termine."
);

/// Error del cerebro conversacional.
#[derive(Debug)]
pub enum BrainError {
    /// No se pudo resolver una API key (parámetro, entorno ni llavero).
    NoApiKey,
    /// El llavero del sistema falló (no por ausencia de key).
    Keyring(String),
    /// Respuesta HTTP no 2xx (400/401 fallan de inmediato, 429/5xx se reintentan).
    Http { status: u16, message: String },
    /// Evento `error` recibido en mitad del flujo SSE.
    Api(String),
    /// JSON malformado al construir o interpretar la petición/respuesta.
    Json(String),
    /// Error de red/transporte (se reintenta).
    Network(String),
    /// Cancelado por barge-in.
    Cancelled,
    /// El cerebro por CLI local falló (no existe, timeout o salida ≠ 0).
    Cli(String),
    /// El modelo rechazó responder (`stop_reason == "refusal"`).
    Refusal,
    /// Se agotaron los reintentos; guarda el último error.
    RetriesExhausted {
        attempts: u32,
        last: Box<BrainError>,
    },
}

impl BrainError {
    /// `true` si este error merece reintento con *backoff*.
    pub fn is_retryable(&self) -> bool {
        match self {
            BrainError::Http { status, .. } => *status == 429 || (500..=599).contains(status),
            BrainError::Api(_) | BrainError::Network(_) => true,
            _ => false,
        }
    }
}

impl fmt::Display for BrainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BrainError::NoApiKey => write!(
                f,
                "no hay API key de Anthropic; usa el comando para configurarla"
            ),
            BrainError::Keyring(e) => write!(f, "falló el llavero del sistema: {e}"),
            BrainError::Http { status, message } => {
                write!(f, "la API respondió {status}: {message}")
            }
            BrainError::Api(e) => write!(f, "error de la API: {e}"),
            BrainError::Json(e) => write!(f, "JSON inválido: {e}"),
            BrainError::Network(e) => write!(f, "sin conexión con Anthropic: {e}"),
            BrainError::Cancelled => write!(f, "respuesta cancelada"),
            BrainError::Cli(e) => write!(f, "falló el cerebro CLI: {e}"),
            BrainError::Refusal => write!(f, "el asistente rechazó responder"),
            BrainError::RetriesExhausted { attempts, last } => {
                write!(f, "sin respuesta tras {attempts} intentos: {last}")
            }
        }
    }
}

impl std::error::Error for BrainError {}

/// Señal de cancelación compartida (barge-in).
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Un turno de la conversación en memoria.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: String,
    pub content: String,
}

/// Resultado completo de un turno.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Completion {
    pub text: String,
    pub stop_reason: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
}

/// Cliente de la API de Mensajes con streaming, memoria y cancelación.
pub struct ClaudeClient {
    agent: ureq::Agent,
    api_key: String,
    endpoint: String,
    model: String,
    effort: String,
    system_prompt: String,
    max_tokens: u32,
    history_turns: usize,
    history: Mutex<VecDeque<Message>>,
    max_retries: u32,
    cancel: CancellationToken,
}

impl ClaudeClient {
    /// Crea el cliente resolviendo la key desde el entorno o el llavero.
    pub fn new() -> Result<Self, BrainError> {
        let api_key = resolve_api_key(None)?;
        Ok(Self::with_api_key(api_key))
    }

    /// Crea el cliente con una key explícita (parámetro de máxima prioridad).
    pub fn with_api_key(api_key: impl Into<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .user_agent(concat!("fokkus-jarvis/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            api_key: api_key.into(),
            endpoint: ANTHROPIC_ENDPOINT.to_string(),
            model: DEFAULT_MODEL.to_string(),
            effort: DEFAULT_EFFORT.to_string(),
            system_prompt: SYSTEM_PROMPT.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            history_turns: DEFAULT_HISTORY_TURNS,
            history: Mutex::new(VecDeque::new()),
            max_retries: DEFAULT_MAX_RETRIES,
            cancel: CancellationToken::new(),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Base URL alternativa (proxy o gateway compatible con la API de
    /// Mensajes). Acepta la base (`https://host`) o la ruta completa
    /// (`https://host/v1/messages`); vacía mantiene la oficial.
    pub fn with_base_url(mut self, base_url: &str) -> Self {
        let base = base_url.trim().trim_end_matches('/');
        if !base.is_empty() {
            self.endpoint = if base.ends_with("/v1/messages") {
                base.to_string()
            } else if base.ends_with("/v1") {
                format!("{base}/messages")
            } else {
                format!("{base}/v1/messages")
            };
        }
        self
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = effort.into();
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_history_turns(mut self, turns: usize) -> Self {
        self.history_turns = turns;
        self
    }

    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Cancela la petición en curso (barge-in). Idempotente.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// `true` si hay una cancelación activa.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Devuelve una copia del historial (para inspección/pruebas).
    pub fn history(&self) -> Vec<Message> {
        self.history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    pub fn clear_history(&self) {
        self.history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Envía el turno del usuario y consume el flujo, llamando `on_token` con
    /// cada *delta* de texto. Devuelve el texto completo y el `usage`.
    ///
    /// Al terminar con éxito, el turno se agrega al historial en memoria.
    pub fn stream(
        &self,
        user_text: &str,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError> {
        let messages = self.build_messages(user_text);
        let body = self.build_body(messages);
        self.cancel.reset();

        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self.request_once(&body, on_token) {
                Ok(completion) => {
                    self.record_turn(user_text, &completion.text);
                    return Ok(completion);
                }
                Err(e) if e.is_retryable() && attempt <= self.max_retries => {
                    if self.cancel.is_cancelled() {
                        return Err(BrainError::Cancelled);
                    }
                    std::thread::sleep(backoff(attempt));
                }
                Err(e) if e.is_retryable() => {
                    return Err(BrainError::RetriesExhausted {
                        attempts: attempt,
                        last: Box::new(e),
                    });
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn request_once(
        &self,
        body: &Value,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError> {
        let response = self
            .agent
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("anthropic-beta", ANTHROPIC_BETA)
            .send_json(body)
            .map_err(|e| match e {
                ureq::Error::StatusCode(status) => BrainError::Http {
                    status,
                    message: String::new(),
                },
                other => BrainError::Network(other.to_string()),
            })?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let message = read_error_message(response);
            return Err(BrainError::Http { status, message });
        }

        let reader = response.into_body().into_reader();
        self.consume_stream(reader, on_token)
    }

    fn consume_stream<R: Read>(
        &self,
        mut reader: R,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<Completion, BrainError> {
        let mut parser = SseParser::new();
        let mut completion = Completion::default();
        let mut buf = [0u8; 4096];
        loop {
            if self.cancel.is_cancelled() {
                return Err(BrainError::Cancelled);
            }
            let n = reader
                .read(&mut buf)
                .map_err(|e| BrainError::Network(e.to_string()))?;
            if n == 0 {
                break;
            }
            for event in parser.feed(&buf[..n]) {
                self.apply_event(event, &mut completion, on_token)?;
            }
        }
        for event in parser.finish() {
            self.apply_event(event, &mut completion, on_token)?;
        }
        if completion.stop_reason.as_deref() == Some("refusal") {
            return Err(BrainError::Refusal);
        }
        Ok(completion)
    }

    fn apply_event(
        &self,
        event: SseEvent,
        completion: &mut Completion,
        on_token: &mut dyn FnMut(&str),
    ) -> Result<(), BrainError> {
        match event {
            SseEvent::MessageStart {
                input_tokens,
                cache_read_input_tokens,
                ..
            } => {
                completion.input_tokens = input_tokens;
                completion.cache_read_input_tokens = cache_read_input_tokens;
            }
            SseEvent::TextDelta { text, .. } => {
                on_token(&text);
                completion.text.push_str(&text);
            }
            SseEvent::MessageDelta {
                stop_reason,
                output_tokens,
            } => {
                if let Some(reason) = stop_reason {
                    completion.stop_reason = Some(reason);
                }
                completion.output_tokens = output_tokens;
            }
            SseEvent::Error { message, .. } => {
                return Err(BrainError::Api(message));
            }
            // `message_start` sin cache, `content_block_*`, `ping`,
            // `message_stop` y eventos desconocidos se toleran.
            _ => {}
        }
        Ok(())
    }

    fn build_messages(&self, user_text: &str) -> Vec<Value> {
        let mut messages = Vec::new();
        let history = self
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for message in history.iter() {
            messages.push(json!({"role": message.role, "content": message.content}));
        }
        drop(history);
        messages.push(json!({"role": "user", "content": user_text}));
        messages
    }

    fn build_body(&self, messages: Vec<Value>) -> Value {
        json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "output_config": {"effort": self.effort},
            "fallbacks": "default",
            "stream": true,
            "system": [{
                "type": "text",
                "text": self.system_prompt,
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": messages,
        })
    }

    fn record_turn(&self, user_text: &str, assistant_text: &str) {
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        history.push_back(Message {
            role: "user".to_string(),
            content: user_text.to_string(),
        });
        history.push_back(Message {
            role: "assistant".to_string(),
            content: assistant_text.to_string(),
        });
        let max_messages = self.history_turns.saturating_mul(2);
        while history.len() > max_messages {
            history.pop_front();
        }
    }
}

/// Resolución de la API key: parámetro → variable de entorno → llavero.
pub fn resolve_api_key(provided: Option<String>) -> Result<String, BrainError> {
    if let Some(key) = provided {
        let key = key.trim();
        if !key.is_empty() {
            return Ok(key.to_string());
        }
    }
    if let Ok(key) = std::env::var(API_KEY_ENV) {
        let key = key.trim();
        if !key.is_empty() {
            return Ok(key.to_string());
        }
    }
    match keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
        Ok(entry) => match entry.get_password() {
            Ok(key) if !key.is_empty() => Ok(key),
            Ok(_) => Err(BrainError::NoApiKey),
            Err(keyring::Error::NoEntry) => Err(BrainError::NoApiKey),
            Err(e) => Err(BrainError::Keyring(e.to_string())),
        },
        Err(keyring::Error::NoDefaultStore) => Err(BrainError::NoApiKey),
        Err(e) => Err(BrainError::Keyring(e.to_string())),
    }
}

/// Guarda la API key en el llavero del sistema (Secret Service/KWallet).
///
/// Es la vía para el binario lanzado desde el `.desktop`, que no hereda las
/// variables de entorno de la shell del usuario.
pub fn store_api_key(key: &str) -> Result<(), BrainError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(BrainError::NoApiKey);
    }
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| BrainError::Keyring(e.to_string()))?;
    entry
        .set_password(key)
        .map_err(|e| BrainError::Keyring(e.to_string()))
}

fn backoff(attempt: u32) -> Duration {
    // 250 ms, 500 ms, 1 s… con tope para los reintentos posteriores.
    Duration::from_millis(250u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(4)))
}

/// Lee el cuerpo de una respuesta de error y extrae `error.message` si es JSON.
fn read_error_message(response: ureq::http::Response<ureq::Body>) -> String {
    let reader = response.into_body().into_reader();
    let mut text = String::new();
    let _ = reader.take(16 * 1024).read_to_string(&mut text);
    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        if let Some(message) = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
        {
            return message.to_string();
        }
    }
    text.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn client() -> ClaudeClient {
        ClaudeClient::with_api_key("test-key")
    }

    fn text_delta(index: usize, text: &str) -> String {
        format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":{index},\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
        )
    }

    fn stream_bytes(deltas: &[&str], stop_reason: &str) -> Vec<u8> {
        let mut s = String::new();
        s.push_str("event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":6}}}\n\n");
        s.push_str("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n");
        for (i, d) in deltas.iter().enumerate() {
            s.push_str(&text_delta(i, d));
        }
        s.push_str(
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        s.push_str(&format!(
            "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop_reason}\"}},\"usage\":{{\"output_tokens\":5}}}}\n\n"
        ));
        s.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        s.into_bytes()
    }

    #[test]
    fn resolve_api_key_prefers_explicit_parameter() {
        assert_eq!(
            resolve_api_key(Some("param-key".to_string())).unwrap(),
            "param-key"
        );
    }

    #[test]
    fn resolve_api_key_reads_env_var() {
        // Serializa los tests que manipulan variables de entorno de proceso.
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap();
        // keyring no debe pisar la key de entorno.
        std::env::set_var(API_KEY_ENV, "env-key");
        assert_eq!(resolve_api_key(None).unwrap(), "env-key");
        std::env::remove_var(API_KEY_ENV);
    }

    #[test]
    fn system_prompt_exposes_eyes_and_hands() {
        for tool in [
            "fokkus-jarvis input type",
            "fokkus-jarvis input key",
            "fokkus-jarvis input click",
            "fokkus-jarvis capture",
        ] {
            assert!(SYSTEM_PROMPT.contains(tool), "{tool}");
        }
        // La prohibición vieja hacía que el LLM se negara a escribir.
        assert!(!SYSTEM_PROMPT.contains("NUNCA intentes ejecutar comandos de consola"));
    }

    #[test]
    fn system_prompt_exposes_background_dev_dispatch() {
        assert!(SYSTEM_PROMPT.contains("fokkus-jarvis dev run"));
        assert!(SYSTEM_PROMPT.contains("segundo plano"));
    }

    #[test]
    fn system_prompt_teaches_prosody_directives() {
        for directive in [
            "[ánimo:",
            "[ritmo:",
            "[pausa:300]",
            "[respira]",
            "[énfasis]",
            "[susurro]",
        ] {
            assert!(SYSTEM_PROMPT.contains(directive), "{directive}");
        }
        assert!(SYSTEM_PROMPT.contains("una o dos por respuesta"));
        // El ejemplo del prompt se limpia por completo antes de hablarse.
        let example = "[ánimo:entusiasta]¡Listo, ya quedó! [pausa:300]Te aviso cuando termine.";
        assert!(SYSTEM_PROMPT.contains(example));
        assert_eq!(
            crate::tts::strip_directives(example),
            "¡Listo, ya quedó! Te aviso cuando termine."
        );
    }

    #[test]
    fn client_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ClaudeClient>();
    }

    #[test]
    fn build_body_pins_model_effort_and_cache_control() {
        let c = client();
        let body = c.build_body(c.build_messages("hola"));
        assert_eq!(body["model"].as_str(), Some(DEFAULT_MODEL));
        assert_eq!(body["output_config"]["effort"].as_str(), Some("low"));
        assert_eq!(body["fallbacks"].as_str(), Some("default"));
        assert_eq!(body["stream"].as_bool(), Some(true));
        assert!(body.get("thinking").is_none());
        assert!(body.get("budget_tokens").is_none());
        let system = &body["system"][0];
        assert_eq!(system["type"].as_str(), Some("text"));
        assert_eq!(system["cache_control"]["type"].as_str(), Some("ephemeral"));
    }

    #[test]
    fn base_url_accepts_host_or_full_path() {
        assert_eq!(client().endpoint(), ANTHROPIC_ENDPOINT);
        assert_eq!(client().with_base_url("  ").endpoint(), ANTHROPIC_ENDPOINT);
        for base in [
            "https://gw.local/",
            "https://gw.local/v1",
            "https://gw.local/v1/messages",
        ] {
            assert_eq!(
                client().with_base_url(base).endpoint(),
                "https://gw.local/v1/messages"
            );
        }
    }

    #[test]
    fn build_messages_appends_history_and_current_turn() {
        let c = client();
        c.record_turn("hola", "¿qué necesitas?");
        let messages = c.build_messages("revisa el repo");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"].as_str(), Some("user"));
        assert_eq!(messages[0]["content"].as_str(), Some("hola"));
        assert_eq!(messages[1]["role"].as_str(), Some("assistant"));
        assert_eq!(messages[1]["content"].as_str(), Some("¿qué necesitas?"));
        assert_eq!(messages[2]["role"].as_str(), Some("user"));
        assert_eq!(messages[2]["content"].as_str(), Some("revisa el repo"));
    }

    #[test]
    fn consume_stream_emits_tokens_and_builds_completion() {
        let c = client();
        let bytes = stream_bytes(&["Hola", " mundo"], "end_turn");
        let mut tokens = Vec::new();
        let completion = c
            .consume_stream(Cursor::new(bytes), &mut |t| tokens.push(t.to_string()))
            .unwrap();
        assert_eq!(tokens, vec!["Hola", " mundo"]);
        assert_eq!(completion.text, "Hola mundo");
        assert_eq!(completion.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(completion.cache_read_input_tokens, 6);
        assert_eq!(completion.output_tokens, 5);
    }

    #[test]
    fn consume_stream_surfaces_refusal() {
        let c = client();
        let bytes = stream_bytes(&["no puedo"], "refusal");
        let result = c.consume_stream(Cursor::new(bytes), &mut |_| {});
        assert!(matches!(result, Err(BrainError::Refusal)));
    }

    #[test]
    fn consume_stream_surfaces_api_error() {
        let c = client();
        let mut s = String::new();
        s.push_str("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"parcial\"}}\n\n");
        s.push_str("event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n");
        let result = c.consume_stream(Cursor::new(s.into_bytes()), &mut |_| {});
        assert!(matches!(
            result,
            Err(BrainError::Api(m)) if m == "Overloaded"
        ));
    }

    #[test]
    fn consume_stream_stops_immediately_on_cancel() {
        let c = client();
        let bytes = stream_bytes(&["Hola"], "end_turn");
        c.cancel();
        let result = c.consume_stream(Cursor::new(bytes), &mut |_| {});
        assert!(matches!(result, Err(BrainError::Cancelled)));
    }

    #[test]
    fn record_turn_trims_history_to_configured_limit() {
        let c = client().with_history_turns(2);
        for i in 0..5 {
            c.record_turn(&format!("u{i}"), &format!("a{i}"));
        }
        let history = c.history();
        // 2 turnos = 4 mensajes: se conservan los últimos 2 turnos.
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].content, "u3");
        assert_eq!(history[3].content, "a4");
    }

    #[test]
    fn retryable_classification_matches_contract() {
        assert!(BrainError::Http {
            status: 429,
            message: String::new(),
        }
        .is_retryable());
        assert!(BrainError::Http {
            status: 503,
            message: String::new(),
        }
        .is_retryable());
        assert!(BrainError::Network("x".into()).is_retryable());
        assert!(BrainError::Api("overloaded".into()).is_retryable());
        assert!(!BrainError::Http {
            status: 400,
            message: String::new(),
        }
        .is_retryable());
        assert!(!BrainError::Http {
            status: 401,
            message: String::new(),
        }
        .is_retryable());
        assert!(!BrainError::NoApiKey.is_retryable());
        assert!(!BrainError::Cancelled.is_retryable());
    }

    #[test]
    fn read_error_message_extracts_json_message() {
        let body = ureq::Body::builder().mime_type("application/json").data(
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid key"}}"#,
        );
        let response = ureq::http::Response::builder()
            .status(401)
            .body(body)
            .unwrap();
        assert_eq!(read_error_message(response), "invalid key");
    }
}
