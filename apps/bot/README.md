# chatx bot — Local AI Assistant

`chatx-agent` is the local LLM runtime for chatx's AI assistant. **On-device inference, no network access** — consistent with chatx's E2E privacy model. It powers two entry points:

- **`apps/bot`** — a standalone CLI copilot REPL (this directory)
- **`apps/chat`** — the "AI assistant" page inside the GUI (pinned row in the chat list)

Both share the same entry point `chatx_agent::default_backend()` (`crates/agent/src/lib.rs:68`) and behave identically.

## Features

- **Local-first** — the model runs on-device; conversation text never leaves the machine. The LLM is never handed E2E keys, group secrets, or keystore material.
- **Pluggable backends** — the `Llm` trait is backend-agnostic: `LlamaBackend` (GGUF, feature `llama`) or `MockBackend` (echo, for plumbing/tests).
- **Streaming output** — `Llm::generate` returns an `Iterator<Item = String>` over assistant tokens.
- **Budgeted context** — `Conversation` trims its own history to fit `n_ctx`, so long chats don't silently overflow a local model's context window.
- **Thread-confined** — the model context is thread-confined; `generate` is synchronous `&mut self`. The GUI calls it from a dedicated actor thread (`chatx-ai-actor`) and ships events back to the UI.
- **Platform-agnostic** — desktop, iOS, and Android share the same Rust code; mobile can later swap in a native engine (see below).

## Model configuration

The model is selected via the `CHATX_MODEL` environment variable pointing to a `.gguf` file. There are no cloud models and no hardcoded model names in the code.

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
