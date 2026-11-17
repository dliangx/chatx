# chatx bot — Local AI Assistant

`chatx-agent` is the local LLM runtime for chatx's AI assistant. **On-device inference, no network access** — consistent with chatx's E2E privacy model. It powers three surfaces:

- **`apps/bot`** — a standalone CLI copilot REPL (this directory)
- **The AI assistant page** inside the GUI — a pinned "AI assistant" row in the chat list, with **persistent conversation history**
- **`@bot` in any DM / group** — type `@bot <question>` to get a local-LLM answer that reads the chat's recent context and is posted back into the conversation as a normal message

All three share the same backend `chatx_agent::default_backend()` (`crates/agent/src/lib.rs:68`).

## Features

- **Local-first** — the model runs on-device; conversation text never leaves the machine. The LLM is never handed E2E keys, group secrets, or keystore material.
- **Pluggable backends** — the `Llm` trait is backend-agnostic: `LlamaBackend` (GGUF, feature `llama`) or `MockBackend` (echo, for plumbing/tests).
- **Streaming output** — `Llm::generate` returns an `Iterator<Item = String>` over assistant tokens.
- **Budgeted context** — `Conversation` trims its own history to fit `n_ctx`, so long chats don't silently overflow a local model's context window.
- **Thread-confined** — the model context is thread-confined; `generate` is synchronous `&mut self`. The GUI calls it from a dedicated actor thread (`chatx-ai-actor`) and ships events back to the UI.
- **Platform-agnostic** — desktop, iOS, and Android share the same Rust code; mobile can later swap in a native engine (see below).

---

## AI assistant page (GUI)

The pinned "AI assistant" row opens a chat-style copilot (`apps/chat/ui/ai_assistant.slint`). Beyond a plain REPL:

- **Persistent conversation history.** Each assistant chat is stored in SQLite as a first-class conversation (`conversations.type = 2`, `crates/sqlite/src/conversations.rs`). Turns are recorded to `messages` (user turn on **send**, assistant turn on **completion** — `apps/chat/src/lib.rs`, `apps/chat/src/ai.rs`). The conversation **title is its first user message**. Reopening a past chat from the list reloads its history into the model (`Cmd::Load`) so you can **continue** where you left off; the **new-conversation** button in the header archives the current one and starts fresh. The pinned top row always opens the most-recent assistant chat.
- **Model picker + download manager.** The header's download button opens a model catalog (`apps/chat/src/models.rs`) split by platform. Downloads are **resumable** (stream to a `.part`, survive interruptions) with **pause / resume / cancel** and live progress. A **source toggle** switches between `huggingface.co` and the `hf-mirror.com` mirror. The selected model is **persisted** and auto-restored on launch (`restore_on_launch`).
- **Streaming UI.** Tokens are pumped from the actor thread into the message list (`pump-mini-app` pattern) — the GUI thread is never blocked by inference.

## `@bot` inline reply (DM & group)

Type `@bot <question>` in any 1:1 or group chat. The app:

1. **Sends your line normally** (the peer sees `@bot <question>` as a normal message), then
2. builds the **chat context** — the most recent ~20 text messages (group rows are decrypted the same way the chat window does), your messages on the "user" side, everyone else on the "assistant" side (`build_bot_context`, `apps/chat/src/lib.rs`),
3. asks the local LLM a **one-shot question** that does **not** touch the assistant page's conversation (`Cmd::Ask` / `ai::ask_on`, `apps/chat/src/ai.rs`), and
4. **posts the answer as a fresh outgoing message** through the regular send path.

Trigger matching (`parse_bot_trigger`): leading whole word `bot`, case-insensitive, ASCII or full-width `@`, optional `:`/`,` boundary (e.g. `@bot 你好` or `@bot: 你好`). Bot-generated answers never re-trigger `@bot`, so there is no recursion.

| Item | Value |
|---|---|
| Trigger | leading `@bot` word |
| Context window | most recent ~20 text messages |
| One-shot budget | 2048 tokens (`apps/chat/src/ai.rs`) |
| Reply delivery | normal outgoing message (DM or group) |
| Failure | in-app notice, your original line already sent |

---

## Model configuration

A `.gguf` file is required for real inference. There are two ways to provide one:

- **CLI (`apps/bot`)** — point the `CHATX_MODEL` environment variable at a `.gguf` file. Unset → `MockBackend`.
- **GUI (assistant page + `@bot`)** — use the built-in **model download manager** (see above). The downloaded file is registered as the active model via `chatx_agent::set_selected_model` and the choice is **persisted**, so the next launch (and the `@bot` path) reuse it without re-downloading.

There are no cloud models and no hardcoded model names in the inference path — the model is always a local file.

| Item | Value | Where |
|---|---|---|
| Model path | env `CHATX_MODEL` (`.gguf`) | `crates/agent/src/lib.rs:71` |
| Feature gate | `llama` | `crates/agent/Cargo.toml:14` |
| Context window `n_ctx` | 4096 (min 512) | `crates/agent/src/llama.rs:81,96` |
| Max output per turn | 2048 tokens | `crates/agent/src/llama.rs` (`load_default`) |
| Conversation budget | 2048 tokens | `apps/bot/src/main.rs:29`, `apps/chat/src/ai.rs:85` |
| Sampling | `dist(1234)` + greedy | `crates/agent/src/llama.rs:144` |
| System prompt | "You are ChatX's built-in assistant…" | `apps/bot/src/main.rs:31`, `apps/chat/src/ai.rs:22` |
| Fallback backend | `MockBackend` (echo) | `crates/agent/src/lib.rs:86` |

### Which model per platform

| Platform | Current implementation | Planned production path (see `crates/bridge/src/lib.rs:157`) |
|---|---|---|
| Desktop (macOS/Win/Linux) | GGUF via `llama-cpp-2`, needs `llama` feature | — |
| iOS | Same | CoreML / llama.cpp-Metal, tokens streamed back via `bridge_llm_token_in` FFI |
| Android | Same | MLC-LLM / llama.cpp-NDK, tokens streamed back via `NativeBridge.llmToken` JNI |

## Startup parameters

| Env var | Purpose | Default |
|---|---|---|
| `CHATX_MODEL` | Path to a `.gguf` model; if unset, falls back to `MockBackend` | unset → mock |
| `RUST_LOG` | Log verbosity (tracing env-filter) | `warn` |

REPL commands (type while running):

| Command | Action |
|---|---|
| `/reset` | Clear the conversation (keeps the system prompt) |
| `/quit`, `/exit` | Exit (equivalent to `Ctrl-D`) |

## Quick start

```bash
# 1) Mock mode (no model needed, exercises the loop)
cargo run -p bot

# 2) Real model (requires a llama.cpp toolchain and a .gguf file)
CHATX_MODEL=/path/to/model.gguf cargo run -p bot --features llama

# The chat GUI binary is named `chatx` (package `apps/chat`); its `llama`
# feature is not forwarded, so enable the dependency's feature directly:
CHATX_MODEL=/path/to/model.gguf cargo run -p chatx --features chatx-agent/llama
