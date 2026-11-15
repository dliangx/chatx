//! `chatx-agent` — local LLM runtime for the chatx "AI assistant" copilot.
//!
//! Design goals, shaped by chatx's privacy model:
//!
//! * **Local-first**: the `Llm` trait is backend-agnostic. The default
//!   [`MockBackend`] needs no model and no network (for tests / plumbing);
//!   the [`LlamaBackend`](llama::LlamaBackend) runs a GGUF model on-device
//!   (feature `llama`) so conversation text never leaves the machine.
//! * **Sync + thread-confined**: a local model owns a thread-confined
//!   context, so `Llm::generate` is synchronous and takes `&mut self`.
//!   Callers that need cross-thread delivery (the Slint UI) run
//!   `generate` + iteration on one dedicated worker thread and ship the
//!   yielded `String`s across — the same pattern chatx uses for group-key
//!   delivery and DM redelivery.
//! * **Budgeted context**: [`Conversation`] renders a single prompt and can
//!   trim its own history to fit a context window, so long chats don't
//!   silently overflow a local model's `n_ctx`.
//!
//! The LLM is a **plain assistant**: it must never be handed E2E keys,
//! group secrets, or `keystore` material — only plaintext the user chose to
//! put in a conversation. That boundary is enforced by what the caller puts
//! into [`Conversation`], not by anything in this crate.

pub mod conversation;

#[cfg(feature = "llama")]
pub mod llama;

use thiserror::Error;

pub use conversation::{Conversation, Message, Role};

/// Errors surfaced by the agent runtime.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("model error: {0}")]
    Model(String),

    #[error("empty response from model")]
    Empty,

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A local language model.
///
/// `generate` returns an iterator over the assistant's tokens; consume it to
/// completion (or drop it to abort). The iterator is `'static` so it can be
/// boxed and moved; backends that stream from an owning buffer satisfy this
/// naturally.
pub trait Llm {
    /// Produce the assistant's reply to `conversation`, streaming tokens.
    fn generate(&mut self, conversation: &Conversation) -> Result<Box<dyn Iterator<Item = String>>>;
}

/// Build the default backend from the environment:
///
/// - `llama` feature enabled **and** `CHATX_MODEL` set to a `.gguf` path →
///   the on-device [`llama::LlamaBackend`].
/// - otherwise → the [`MockBackend`] (echo), so the conversation loop, REPL,
///   and UI plumbing all run end-to-end without a model or the feature.
///
/// Used by both `apps/bot` and the chat UI so there is one entry point.
pub fn default_backend() -> Box<dyn Llm> {
    #[cfg(feature = "llama")]
    {
        if let Some(path) = std::env::var_os("CHATX_MODEL") {
            if let Ok(be) = llama::LlamaBackend::load_default(path) {
                return Box::new(be);
            }
        }
    }
    Box::new(MockBackend::new())
}

/// A no-op stand-in backend that echoes the last user message.
///
/// Exists so the conversation loop, REPL, and UI plumbing can be built and
/// tested **without** a model, network, or the `llama` feature. It is NOT a
/// real assistant — it just proves the token-stream contract end to end.
#[derive(Debug, Default)]
pub struct MockBackend;

impl MockBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Llm for MockBackend {
    fn generate(&mut self, conversation: &Conversation) -> Result<Box<dyn Iterator<Item = String>>> {
        let last = conversation
            .history()
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.text.clone())
            .unwrap_or_default();
        let out = format!("(mock) heard: {last}");
        // Yield word-by-word (each chunk is "word ") so streaming looks natural.
        let chunks: Vec<String> = out.split_whitespace().map(|w| format!("{w} ")).collect();
        Ok(Box::new(chunks.into_iter()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_streams_tokens() {
        let mut conv = Conversation::new("You are helpful.");
        conv.user("hello");
        let mut b = MockBackend::new();
        let collected: String = b
            .generate(&conv)
            .unwrap()
            .collect::<Vec<_>>()
            .concat();
        assert!(collected.contains("hello"));
    }
}
