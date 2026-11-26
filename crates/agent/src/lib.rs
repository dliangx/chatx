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
//!   Callers that need cross-thread delivery (the Slint UI) run `generate`
//!   on one dedicated worker thread; every text piece is pushed out
//!   immediately through the `on_token` callback as it is decoded, so the
//!   UI can paint each piece the moment it exists.
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
/// `generate` invokes `on_token` with each decoded text piece **as it is
/// generated** (true streaming — no buffering until the turn ends) and
/// returns the full reply text.
pub trait Llm {
    /// Produce the assistant's reply to `conversation`.
    ///
    /// `on_token` is called for every text piece the moment it is decoded,
    /// so callers can render or print it immediately. Returns the complete
    /// reply text on success.
    fn generate(
        &mut self,
        conversation: &Conversation,
        on_token: &mut dyn FnMut(String),
    ) -> Result<String>;
}

/// A process-local override of the active model, set at runtime (e.g. after a
/// model finishes downloading). Takes precedence over the `CHATX_MODEL` env
/// var; `None` means "use whatever the environment says".
static SELECTED: std::sync::Mutex<Option<std::path::PathBuf>> =
    std::sync::Mutex::new(None);

/// Set the active model path for this process (used by the in-app download
/// manager once a model is fully downloaded).
pub fn set_selected_model(path: Option<std::path::PathBuf>) {
    *SELECTED.lock().unwrap() = path;
}

/// Resolve the model path to use: the runtime override first, else the
/// `CHATX_MODEL` environment variable.
fn selected_model_path() -> Option<std::path::PathBuf> {
    SELECTED.lock().unwrap().clone().or_else(|| {
        std::env::var_os("CHATX_MODEL").map(std::path::PathBuf::from)
    })
}

/// The currently-resolved model path (if any), so a caller can remember which
/// GGUF is actually in use (for reverting back to it after a failed switch).
pub fn resolved_selected_model() -> Option<std::path::PathBuf> {
    selected_model_path()
}

/// Build the default backend from the environment:
///
/// - `llama` feature enabled **and** a model path is resolvable (the runtime
///   override from [`set_selected_model`] or the `CHATX_MODEL` env var) →
///   the on-device [`llama::LlamaBackend`].
/// - otherwise → the [`MockBackend`] (echo), so the conversation loop, REPL,
///   and UI plumbing all run end-to-end without a model or the feature.
///
/// The chosen GGUF is also **self-tested** (see [`try_load_selected`]): a
/// corrupt-but-loadable file falls through to the mock rather than producing
/// garbage or "empty response" output. Used by both `apps/bot` and the chat UI
/// so there is one entry point.
pub fn default_backend() -> Box<dyn Llm> {
    #[cfg(feature = "llama")]
    {
        if let Some(path) = selected_model_path() {
            if let Ok(be) = try_load_selected(path) {
                return be;
            }
            // Unloadable or self-test failed (corrupt file) → fall through.
        }
    }
    Box::new(MockBackend::new())
}

/// Load the GGUF at `path` and verify it actually produces a valid logit
/// distribution before trusting it.
///
/// A corrupt-but-loadable GGUF (truncated weights, a bad quant) passes
/// `LlamaBackend::load` but yields all-NaN / all-zero logits on inference. This
/// runs one tiny decode and rejects the model unless the first-position logits
/// form a real distribution, so the caller can mark a broken file as unusable
/// instead of surfacing "empty response" mid-conversation.
///
/// Returns `Ok(backend)` when the model loads and self-tests clean; `Err` when
/// the file is unloadable or fails the self-test.
pub fn try_load_selected(path: std::path::PathBuf) -> Result<Box<dyn Llm>> {
    #[cfg(feature = "llama")]
    {
        let be = llama::LlamaBackend::load_default(path)?;
        be.self_test()?;
        Ok(Box::new(be))
    }
    #[cfg(not(feature = "llama"))]
    {
        let _ = path;
        Err(Error::Model("llama feature disabled".into()))
    }
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
    fn generate(
        &mut self,
        conversation: &Conversation,
        on_token: &mut dyn FnMut(String),
    ) -> Result<String> {
        let last = conversation
            .history()
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.text.clone())
            .unwrap_or_default();
        let out = format!("(mock) heard: {last}");
        // Stream word-by-word (each chunk is "word ") so the callback sees
        // natural streaming; the final chunk drops the trailing space so the
        // streamed text equals the returned text exactly.
        let words: Vec<&str> = out.split_whitespace().collect();
        for (i, w) in words.iter().enumerate() {
            let sep = if i + 1 == words.len() { "" } else { " " };
            on_token(format!("{w}{sep}"));
        }
        Ok(out)
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
        let mut streamed = String::new();
        let collected = b
            .generate(&conv, &mut |tok| streamed.push_str(&tok))
            .unwrap();
        assert!(collected.contains("hello"));
        assert_eq!(streamed, collected);
    }
}
