//! chatx local AI-assistant copilot REPL.
//!
//! A minimal, runnable proof of the "local LLM + assistant conversation" loop,
//! deliberately decoupled from the Slint UI so it builds fast and is easy to
//! exercise by hand:
//!
//! ```text
//!   cargo run -p bot
//!   ```
//!
//! Without a model it runs the [`chatx_agent::MockBackend`] (echo) so the
//! token-stream + conversation loop is testable end to end. With a GGUF model
//! path in `CHATX_MODEL` and the `llama` feature enabled, it loads the real
//! model on-device:
//!
//! ```text
//!   CHATX_MODEL=/path/to/model.gguf cargo run -p bot --features llama
//! ```
//!
//! The model never leaves the machine; the conversation is held in a
//! [`chatx_agent::Conversation`] that the REPL owns.

use std::io::{self, Write};

use chatx_agent::{Conversation, Llm, MockBackend};

/// Cap on how many tokens of context the assistant keeps. Keeps a local model
/// from overflowing `n_ctx` on a long REPL session.
const MAX_CONTEXT_TOKENS: usize = 2048;

const SYSTEM_PROMPT: &str = "You are ChatX's built-in assistant. Be concise and helpful. \
     You are running fully locally; you have no network access.";

type Backend = Box<dyn Llm>;

fn build_backend() -> Result<Backend, String> {
    #[cfg(feature = "llama")]
    {
        if let Some(path) = std::env::var_os("CHATX_MODEL") {
            eprintln!("[bot] loading GGUF model from {path:?} ...");
            let be = chatx_agent::llama::LlamaBackend::load_default(path)
                .map_err(|e| e.to_string())?;
            return Ok(Box::new(be));
        }
        eprintln!("[bot] `llama` feature enabled but CHATX_MODEL unset — using mock backend");
    }

    #[cfg(not(feature = "llama"))]
    {
        eprintln!("[bot] built without `llama` feature — using mock backend (echo).");
        eprintln!("[bot] for a real model: CHATX_MODEL=/path/model.gguf cargo run -p bot --features llama");
    }

    Ok(Box::new(MockBackend::new()))
}

fn read_line() -> Option<String> {
    print!("you> ");
    io::stdout().flush().ok();
    let mut s = String::new();
    io::stdin().read_line(&mut s).ok()?;
    let t = s.trim().to_string();
    if t.is_empty() {
        return None;
    }
    Some(t)
}

fn main() -> std::process::ExitCode {
    let mut backend = match build_backend() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[bot] failed to start backend: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let mut conv = Conversation::new(SYSTEM_PROMPT);
    eprintln!("[bot] ready. Type a message. `/reset` clears, `/quit` exits.\n");

    loop {
        let Some(input) = read_line() else {
            break; // EOF (Ctrl-D)
        };
        match input.as_str() {
            "/quit" | "/exit" => break,
            "/reset" => {
                // Conversation has no `clear`; rebuild it with the same system prompt.
                let sys = conv.system().to_string();
                conv = Conversation::new(sys);
                eprintln!("[bot] conversation reset.\n");
                continue;
            }
            _ => {}
        }

        conv.user(input);
        conv.trim_to_budget(MAX_CONTEXT_TOKENS);

        print!("chatx> ");
        io::stdout().flush().ok();

        match backend.generate(&conv) {
            Ok(stream) => {
                let mut reply = String::new();
                for tok in stream {
                    reply.push_str(&tok);
                    print!("{tok}");
                    io::stdout().flush().ok();
                }
                println!();
                // Keep continuity: record the assistant's own turn.
                conv.assistant(reply);
            }
            Err(e) => {
                println!();
                eprintln!("[bot] generation failed: {e}");
            }
        }
        println!();
    }

    eprintln!("[bot] bye.");
    std::process::ExitCode::SUCCESS
}
