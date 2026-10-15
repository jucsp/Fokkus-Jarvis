//! Cerebro conversacional (HU-6).
//!
//! Reúne el cliente síncrono de la API de Mensajes de Anthropic ([`client`]) y
//! el parser SSE incremental ([`sse`]) que alimenta el streaming de texto para
//! que el TTS pueda empezar a hablar antes de que termine la respuesta.

pub mod client;
pub mod sse;

pub use client::{
    resolve_api_key, BrainError, CancellationToken, ClaudeClient, Completion, Message,
    ANTHROPIC_BETA, ANTHROPIC_ENDPOINT, ANTHROPIC_VERSION, API_KEY_ENV, DEFAULT_EFFORT,
    DEFAULT_HISTORY_TURNS, DEFAULT_MAX_RETRIES, DEFAULT_MAX_TOKENS, DEFAULT_MODEL, KEYRING_SERVICE,
    KEYRING_USER, SYSTEM_PROMPT,
};
pub use sse::{SseEvent, SseParser};
