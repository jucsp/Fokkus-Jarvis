//! Parser SSE (Server-Sent Events) propio y robusto para el streaming de la
//! API de Mensajes de Anthropic (HU-6).
//!
//! A diferencia de un parser por línea ingenua, [`SseParser`] es *incremental*:
//! consume trozos arbitrarios de un `std::io::Read` y **nunca asume** que un
//! `feed` coincide con un evento completo ni con una línea completa. Acumula
//! los bytes pendientes y despacha un evento solo cuando llega la línea en
//! blanco que lo termina (o al cierre del flujo, con [`SseParser::finish`]).
//!
//! Es tolerante con todo lo que no sea texto hablado: eventos desconocidos,
//! bloques `thinking` y *deltas* no textuales se ignoran sin error.

use serde_json::Value;

/// Eventos SSE reconocidos de la API de Mensajes de Anthropic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseEvent {
    /// `message_start`: comienzo de la respuesta; trae el modelo y el `usage`
    /// inicial (para verificar `cache_read_input_tokens`).
    MessageStart {
        model: String,
        input_tokens: u64,
        cache_read_input_tokens: u64,
    },
    /// `content_block_start`: se abre un bloque de contenido (texto, thinking…).
    ContentBlockStart { index: usize, block_type: String },
    /// `content_block_delta` con `delta.type == "text_delta"`: texto incremental.
    TextDelta { index: usize, text: String },
    /// `content_block_stop`: se cierra un bloque.
    ContentBlockStop { index: usize },
    /// `message_delta`: razón de fin de turno y tokens de salida.
    MessageDelta {
        stop_reason: Option<String>,
        output_tokens: u64,
    },
    /// `message_stop`: fin normal del flujo.
    MessageStop,
    /// `ping`: latido del servidor.
    Ping,
    /// `error`: error reportado por la API en mitad del flujo.
    Error {
        message: String,
        error_type: Option<String>,
    },
    /// Evento cuyo tipo no está en la lista anterior (se tolera e ignora).
    Unknown { event_type: String },
}

/// Parser incremental de SSE.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Bytes acumulados que aún no forman una línea completa.
    pending: Vec<u8>,
    /// `event:` del evento en curso.
    event_type: Option<String>,
    /// Líneas `data:` del evento en curso (se unen con `\n`).
    data_lines: Vec<String>,
    /// `true` si el evento en curso ya acumuló algún campo.
    has_event: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume un trozo de bytes y devuelve los eventos completos que se hayan
    /// formado. Los eventos quedan en orden de aparición.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        self.pending.extend_from_slice(chunk);
        while let Some(nl) = self.pending.iter().position(|&b| b == b'\n') {
            let mut line = self.pending[..nl].to_vec();
            self.pending.drain(..=nl);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.consume_line(&line, &mut events);
        }
        events
    }

    /// Cierra el flujo: procesa una posible última línea sin `\n` final y
    /// despacha el evento que quedara pendiente.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.pending.is_empty() {
            let mut line = std::mem::take(&mut self.pending);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if !line.is_empty() {
                self.consume_line(&line, &mut events);
            }
        }
        if self.has_event {
            if let Some(event) = self.flush_event() {
                events.push(event);
            }
        }
        self.clear_event();
        events
    }

    fn consume_line(&mut self, line: &[u8], events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            // Línea en blanco: fin del evento en curso.
            if self.has_event {
                if let Some(event) = self.flush_event() {
                    events.push(event);
                }
            }
            self.clear_event();
            return;
        }
        // Comentario SSE (`:`).
        if line[0] == b':' {
            return;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            return;
        };
        let field = &line[..colon];
        let mut value = &line[colon + 1..];
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }
        match field {
            b"event" => {
                self.event_type = Some(String::from_utf8_lossy(value).into_owned());
                self.has_event = true;
            }
            b"data" => {
                self.data_lines
                    .push(String::from_utf8_lossy(value).into_owned());
                self.has_event = true;
            }
            // `id` y `retry` no se usan.
            _ => {}
        }
    }

    fn flush_event(&mut self) -> Option<SseEvent> {
        let event_type = self
            .event_type
            .take()
            .unwrap_or_else(|| "message".to_owned());
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        self.has_event = false;
        if data.is_empty() {
            return None;
        }
        parse_event(&event_type, &data)
    }

    fn clear_event(&mut self) {
        self.event_type = None;
        self.data_lines.clear();
        self.has_event = false;
    }
}

/// Lee `value[path[0]][path[1]]…` como cadena.
fn str_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = value;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_str()
}

/// Lee `value[path[0]][path[1]]…` como entero sin signo (0 si no existe).
fn u64_at(value: &Value, path: &[&str]) -> u64 {
    let mut cur = value;
    for key in path {
        match cur.get(*key) {
            Some(next) => cur = next,
            None => return 0,
        }
    }
    cur.as_u64().unwrap_or(0)
}

/// Interpreta un evento SSE ya delimitado. Devuelve `None` para los eventos que
/// se toleran y se ignoran (p. ej. *deltas* `thinking`).
fn parse_event(event_name: &str, data: &str) -> Option<SseEvent> {
    let value: Value = serde_json::from_str(data).ok()?;
    // Sin `event:` explícito, SSE usa `message`; en ese caso el tipo real está
    // en el campo JSON `type`.
    let kind = if event_name == "message" {
        value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
    } else {
        event_name
    };
    match kind {
        "message_start" => Some(SseEvent::MessageStart {
            model: str_at(&value, &["message", "model"])
                .unwrap_or("")
                .to_owned(),
            input_tokens: u64_at(&value, &["message", "usage", "input_tokens"]),
            cache_read_input_tokens: u64_at(
                &value,
                &["message", "usage", "cache_read_input_tokens"],
            ),
        }),
        "content_block_start" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let block_type = str_at(&value, &["content_block", "type"])
                .unwrap_or("unknown")
                .to_owned();
            Some(SseEvent::ContentBlockStart { index, block_type })
        }
        "content_block_delta" => {
            let delta = value.get("delta")?;
            let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");
            if delta_type == "text_delta" {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let text = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                Some(SseEvent::TextDelta { index, text })
            } else {
                // `thinking_delta`, `input_json_delta`, `signature_delta`…: tolerados.
                None
            }
        }
        "content_block_stop" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            Some(SseEvent::ContentBlockStop { index })
        }
        "message_delta" => {
            let stop_reason = str_at(&value, &["delta", "stop_reason"]).map(str::to_owned);
            let output_tokens = u64_at(&value, &["usage", "output_tokens"]);
            Some(SseEvent::MessageDelta {
                stop_reason,
                output_tokens,
            })
        }
        "message_stop" => Some(SseEvent::MessageStop),
        "ping" => Some(SseEvent::Ping),
        "error" => {
            let message = str_at(&value, &["error", "message"])
                .unwrap_or("")
                .to_owned();
            let error_type = str_at(&value, &["error", "type"]).map(str::to_owned);
            Some(SseEvent::Error {
                message,
                error_type,
            })
        }
        other => Some(SseEvent::Unknown {
            event_type: other.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: &str, data: &str) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    fn full_stream() -> String {
        let mut s = String::new();
        s.push_str(&ev(
            "message_start",
            r#"{"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":12,"cache_read_input_tokens":8}}}"#,
        ));
        s.push_str(&ev(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ));
        s.push_str(&ev(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hola"}}"#,
        ));
        s.push_str(&ev(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" mundo"}}"#,
        ));
        s.push_str(&ev(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ));
        s.push_str(&ev(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
        ));
        s.push_str(&ev("message_stop", r#"{"type":"message_stop"}"#));
        s
    }

    fn collect_events(bytes: &[u8]) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        let mut events = parser.feed(bytes);
        events.extend(parser.finish());
        events
    }

    #[test]
    fn parses_full_stream_with_all_known_events() {
        let events = collect_events(full_stream().as_bytes());
        assert_eq!(
            events,
            vec![
                SseEvent::MessageStart {
                    model: "claude-opus-5-5".to_string(),
                    input_tokens: 12,
                    cache_read_input_tokens: 8,
                },
                SseEvent::ContentBlockStart {
                    index: 0,
                    block_type: "text".to_string(),
                },
                SseEvent::TextDelta {
                    index: 0,
                    text: "Hola".to_string(),
                },
                SseEvent::TextDelta {
                    index: 0,
                    text: " mundo".to_string(),
                },
                SseEvent::ContentBlockStop { index: 0 },
                SseEvent::MessageDelta {
                    stop_reason: Some("end_turn".to_string()),
                    output_tokens: 7,
                },
                SseEvent::MessageStop,
            ]
        );
    }

    #[test]
    fn parses_byte_by_byte_identically_to_single_chunk() {
        let full = full_stream();
        let bytes = full.as_bytes();
        let expected = collect_events(bytes);

        let mut parser = SseParser::new();
        let mut events = Vec::new();
        for &b in bytes {
            events.extend(parser.feed(&[b]));
        }
        events.extend(parser.finish());
        assert_eq!(events, expected);
    }

    #[test]
    fn parses_multiple_events_in_one_chunk() {
        let chunk = full_stream();
        let events = collect_events(chunk.as_bytes());
        assert!(events.len() >= 7);
    }

    #[test]
    fn tolerates_unknown_event_and_unknown_block() {
        let mut s = String::new();
        s.push_str(&ev(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        ));
        s.push_str(&ev(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"razono"}}"#,
        ));
        s.push_str(&ev(
            "some_future_event",
            r#"{"type":"some_future_event","foo":1}"#,
        ));
        let events = collect_events(s.as_bytes());
        assert_eq!(
            events,
            vec![
                SseEvent::ContentBlockStart {
                    index: 0,
                    block_type: "thinking".to_string(),
                },
                // El `thinking_delta` se ignora (no produce evento).
                SseEvent::Unknown {
                    event_type: "some_future_event".to_string(),
                },
            ]
        );
    }

    #[test]
    fn surfaces_error_mid_stream() {
        let mut s = String::new();
        s.push_str(&ev(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"parcial"}}"#,
        ));
        s.push_str(&ev(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ));
        let events = collect_events(s.as_bytes());
        assert_eq!(
            events.last(),
            Some(&SseEvent::Error {
                message: "Overloaded".to_string(),
                error_type: Some("overloaded_error".to_string()),
            })
        );
    }

    #[test]
    fn surfaces_refusal_stop_reason() {
        let s = ev(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_sequence":null},"usage":{"output_tokens":3}}"#,
        );
        let events = collect_events(s.as_bytes());
        assert_eq!(
            events,
            vec![SseEvent::MessageDelta {
                stop_reason: Some("refusal".to_string()),
                output_tokens: 3,
            }]
        );
    }

    #[test]
    fn handles_ping_and_finishes_partial_final_line() {
        // `ping` completo + última línea sin `\n` final.
        let mut s = String::new();
        s.push_str(&ev("ping", r#"{"type":"ping"}"#));
        s.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}");
        let events = collect_events(s.as_bytes());
        assert_eq!(events, vec![SseEvent::Ping, SseEvent::MessageStop]);
    }

    #[test]
    fn ignores_comments_and_malformed_json() {
        let mut s = String::new();
        s.push_str(": keepalive comment\n\n");
        s.push_str(&ev("content_block_delta", "esto no es JSON válido"));
        s.push_str(&ev(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
        ));
        let events = collect_events(s.as_bytes());
        assert_eq!(
            events,
            vec![SseEvent::TextDelta {
                index: 0,
                text: "ok".to_string(),
            }]
        );
    }

    #[test]
    fn crlf_line_endings_are_supported() {
        let raw = "event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n";
        let events = collect_events(raw.as_bytes());
        assert_eq!(events, vec![SseEvent::MessageStop]);
    }
}
