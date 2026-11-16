//! Local GGUF backend via [`llama-cpp-2`](https://crates.io/crates/llama-cpp-2).
//!
//! Enabled with the `llama` feature. Runs the model fully on-device — no
//! network — which is what keeps chatx's "conversations never leave the
//! machine" property honest.
//!
//! This backend is **thread-confined**: it owns a `LlamaBackend` /
//! `LlamaModel` / context and must be used from one thread at a time. That
//! matches the [`crate::Llm`] contract (`generate` takes `&mut self`), and
//! the REPL in `apps/bot` runs it on a single dedicated thread.
//!
//! `generate` collects the full completion into a `String` and yields it in
//! small word-ish chunks so callers get the same token-stream experience as
//! a true tokenizer-stream, without exposing the non-`Send` model internals
//! across the iterator boundary.
//!
//! The conversation is converted to a prompt with the **model's own chat
//! template** (`llama_model_chat_template` + `llama_chat_apply_template`), so
//! the role framing (`<system>/<user>/<assistant>`) is whatever the GGUF was
//! trained with — not a hand-rolled `<<user>>` tag the model may not know.

use std::path::Path;

use crate::conversation::Conversation;
use crate::{Error, Llm, Result};

use llama_cpp_2::context::params::LlamaContextParams;
// Aliased so our own public `LlamaBackend` struct doesn't shadow the
// runtime type it must hold (a same-named field would become self-referential).
use llama_cpp_2::llama_backend::LlamaBackend as LlamaRuntime;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{LlamaChatMessage, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;

/// A local GGUF model loaded with `llama-cpp-2`.
pub struct LlamaBackend {
    _runtime: LlamaRuntime,
    model: LlamaModel,
    /// The model's chat template, fetched once at load.
    chat_template: LlamaChatTemplate,
    /// Context window (tokens) the model is asked to run with.
    n_ctx: u32,
    /// Hard cap on generated tokens per turn (guard against runaway output).
    max_tokens: u32,
}

impl LlamaBackend {
    /// Load a GGUF file. `n_ctx` is the context window (defaults to 4096);
    /// `max_tokens` caps the assistant's output length per turn (default 2048).
    pub fn load(
        path: impl AsRef<Path>,
        n_ctx: u32,
        max_tokens: u32,
    ) -> Result<Self> {
        let n_ctx = n_ctx.max(512);

        let backend = LlamaRuntime::init().map_err(|e| Error::Model(e.to_string()))?;
        let model = LlamaModel::load_from_file(
            &backend,
            path.as_ref(),
            &LlamaModelParams::default(),
        )
        .map_err(|e| Error::Model(format!("load model: {e}")))?;

        // Fetch the model's own chat template once; the prompt for every
        // completion is rendered through it (see `complete`).
        let chat_template = model
            .chat_template(None)
            .map_err(|e| Error::Model(format!("no chat template: {e}")))?;

        Ok(Self {
            _runtime: backend,
            model,
            chat_template,
            n_ctx,
            max_tokens,
        })
    }

    /// Load with default context (4096) and output cap (2048).
    pub fn load_default(path: impl AsRef<Path>) -> Result<Self> {
        Self::load(path, 4096, 2048)
    }

    /// Convert a [`Conversation`] into a prompt string using the model's own
    /// chat template (system + history, assistant slot appended).
    fn render_prompt(&self, conversation: &Conversation) -> Result<String> {
        let mut messages: Vec<LlamaChatMessage> = Vec::new();
        if !conversation.system().is_empty() {
            messages.push(LlamaChatMessage::new("system".into(), conversation.system().into())
                .map_err(|e| Error::Other(format!("system message: {e}")))?)
        }
        for m in conversation.history() {
            let role = match m.role {
                crate::Role::System => "system",
                crate::Role::User => "user",
                crate::Role::Assistant => "assistant",
            };
            messages.push(LlamaChatMessage::new(role.into(), m.text.clone())
                .map_err(|e| Error::Other(format!("message: {e}")))?)
        }

        self.model
            .apply_chat_template(&self.chat_template, &messages, true)
            .map_err(|e| Error::Model(format!("apply chat template: {e}")))
    }

    /// Run one completion over `conversation`, returning the assistant text.
    fn complete(&mut self, conversation: &Conversation) -> Result<String> {
        use std::num::NonZeroU32;

        let prompt = self.render_prompt(conversation)?;

        let backend = &self._runtime;
        let mut ctx = self
            .model
            .new_context(
                backend,
                LlamaContextParams::default()
                    .with_n_ctx(Some(NonZeroU32::new(self.n_ctx).unwrap_or(NonZeroU32::new(4096).unwrap()))),
            )
            .map_err(|e| Error::Model(format!("new context: {e}")))?;

        let tokens_list = self.model.vocab().tokenize(prompt.as_bytes(), true, true);
        let n_ctx = ctx.n_ctx() as i32;
        if tokens_list.len() as i32 >= n_ctx {
            return Err(Error::Model("prompt exceeds context window".into()));
        }

        // The batch must hold the whole prompt at once; size it to the prompt
        // (not a fixed 512) so longer first messages don't hit InsufficientSpace.
        let mut batch = LlamaBatch::new(tokens_list.len().max(1), 1);
        let last_index: i32 = (tokens_list.len() - 1) as i32;
        for (i, token) in (0_i32..).zip(tokens_list) {
            let is_last = i == last_index;
            batch
                .add(token, i, &[0], is_last)
                .map_err(|e| Error::Model(format!("batch add: {e}")))?;
        }
        ctx.decode(&mut batch)
            .map_err(|e| Error::Model(format!("decode prompt: {e}")))?;

        let mut sampler = LlamaSampler::chain_simple([LlamaSampler::dist(1234), LlamaSampler::greedy()]);
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut out = String::new();

        let mut n_cur = batch.n_tokens();
        for _ in 0..self.max_tokens {
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            sampler.accept(token);
            if self.model.vocab().is_eog(token) {
                break;
            }
            let piece = self.model.vocab().token_to_piece(token, true, None);
            let mut chunk = String::with_capacity(decoder.max_utf8_buffer_length(piece.len()).unwrap_or(8));
            let (_, _, _) = decoder.decode_to_string(&piece, &mut chunk, false);
            out.push_str(&chunk);

            batch.clear();
            batch
                .add(token, n_cur, &[0], true)
                .map_err(|e| Error::Model(format!("batch add gen: {e}")))?;
            n_cur += 1;
            ctx.decode(&mut batch)
                .map_err(|e| Error::Model(format!("decode gen: {e}")))?;
        }

        if out.is_empty() {
            return Err(Error::Empty);
        }
        Ok(out)
    }
}

impl Llm for LlamaBackend {
    fn generate(&mut self, conversation: &Conversation) -> Result<Box<dyn Iterator<Item = String>>> {
        let text = self.complete(conversation)?;
        // Yield in ~16-char chunks so the consumer gets word-ish streaming.
        // Chunk by CHAR (not byte): splitting UTF-8 mid-codepoint would turn
        // the broken fragment into U+FFFD (the "?" replacement char).
        let chars: Vec<char> = text.chars().collect();
        let mut chunks: Vec<String> = Vec::new();
        for group in chars.chunks(16) {
            chunks.push(group.iter().collect());
        }
        Ok(Box::new(chunks.into_iter()))
    }
}
