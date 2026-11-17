//! AI-assistant copilot (local LLM) glue for the Slint UI.
//!
//! A single **actor thread** owns the (thread-confined, `!Send`)
//! [`chatx_agent::Llm`] backend and the running [`chatx_agent::Conversation`].
//! The UI never touches the LLM directly: it sends a command to the actor and
//! a UI `Timer` "pumps" tokens out of a single shared output channel into the
//! message model — the `pump-mini-app` pattern from this codebase. The actor
//! thread blocks only on the (long) model inference; the UI thread is never
//! blocked.
//!
//! Backend: [`chatx_agent::default_backend`]
//! - `llama` feature + `CHATX_MODEL` set → the on-device GGUF model.
//! - otherwise → the echo `MockBackend` (page works end-to-end with no model).

use std::sync::mpsc::{channel, Receiver, Sender};

use slint::ComponentHandle;
use slint::SharedString;

use crate::{AiAssistantState, AiMessageRow, MainWindow};

const SYSTEM_PROMPT: &str = "You are ChatX's built-in assistant. Be concise and helpful. \
     You are running fully locally; you have no network access.";

/// A single item pushed from the actor to the UI.
#[derive(Clone)]
enum UiEvent {
    /// A new conversation row.
    Row(AiMessageRow),
    /// One assistant text chunk (append to last row).
    Text(String),
    /// Generation failed with `msg`.
    Err(String),
    /// Generation finished (success).
    Done,
}

/// Commands the actor understands.
enum Cmd {
    Send(String),
    /// Start a fresh, empty conversation (drop the current one). Sent by
    /// "new chat" / "clear conversation".
    Reset,
    /// Rebuild the backend from the (possibly new) selected model. Sent after a
    /// model finishes downloading or the user picks a different one.
    Reload,
    /// Replace the running conversation with the loaded history (user/assistant
    /// turns) so the next turn continues it. Sent when opening a past
    /// AI-assistant conversation.
    Load(Vec<(bool, String)>),
    /// One-shot question against a given conversation context (chat history).
    /// The answer is delivered to the provided oneshot channel and the running
    /// AI-assistant conversation is left untouched (this is the `@bot` chat
    /// reply path).
    Ask {
        /// Prior turns: `(is_assistant, text)` in chronological order.
        context: Vec<(bool, String)>,
        question: String,
        reply: std::sync::mpsc::Sender<String>,
    },
}

/// The actor's command sender (created once by [`ensure_actor`]).
static CMD: std::sync::Mutex<Option<Sender<Cmd>>> = std::sync::Mutex::new(None);

/// The app-lifetime output channel from the actor to the UI. One pair for the
/// process lifetime. `OUT_TX` is moved into the actor at spawn time; `OUT_RX`
/// is owned by this crate and drained on each `pump`.
static OUT_TX: std::sync::Mutex<Option<Sender<UiEvent>>> = std::sync::Mutex::new(None);
static OUT_RX: std::sync::Mutex<Option<Receiver<UiEvent>>> = std::sync::Mutex::new(None);

/// Called on the UI thread once a generation finishes, with the full assistant
/// text. Registered by `lib.rs` to persist the assistant turn to the AI
/// conversation (type-2) in SQLite.
static DONE_HOOK: std::sync::Mutex<Option<Box<dyn Fn(String) + Send>>> =
    std::sync::Mutex::new(None);

/// Register the assistant-turn completion hook (called with the final text).
pub fn set_done_hook(f: impl Fn(String) + Send + 'static) {
    *DONE_HOOK.lock().unwrap() = Some(Box::new(f));
}

fn ensure_actor() {
    if CMD.lock().unwrap().is_some() {
        return;
    }
    let (cmd_tx, cmd_rx) = channel::<Cmd>();
    let (out_tx, out_rx) = channel::<UiEvent>();
    *OUT_RX.lock().unwrap() = Some(out_rx);
    std::thread::Builder::new()
        .name("chatx-ai-actor".into())
        .spawn(move || {
            *OUT_TX.lock().unwrap() = Some(out_tx);
            actor_loop(cmd_rx);
        })
        .expect("spawn ai actor");
    *CMD.lock().unwrap() = Some(cmd_tx);
}

fn actor_loop(cmd_rx: Receiver<Cmd>) {
    let mut backend = chatx_agent::default_backend();
    let mut conv = chatx_agent::Conversation::new(SYSTEM_PROMPT);

    for cmd in cmd_rx {
        match cmd {
            Cmd::Reload => {
                // Re-read the (possibly newly-downloaded) model and continue
                // the same conversation with the new backend.
                backend = chatx_agent::default_backend();
            }
            Cmd::Reset => {
                conv = chatx_agent::Conversation::new(SYSTEM_PROMPT);
            }
            Cmd::Load(turns) => {
                let mut c = chatx_agent::Conversation::new(SYSTEM_PROMPT);
                for (is_assistant, text) in turns {
                    if is_assistant {
                        c.assistant(text);
                    } else {
                        c.user(text);
                    }
                }
                // Trim once on load (keeps the most recent turns) so a long
                // loaded history fits the local model's budget. Per-turn sends
                // no longer re-trim, which would otherwise erode the history.
                c.trim_to_budget(2048);
                conv = c;
            }
            Cmd::Ask { context, question, reply } => {
                // One-shot: build a fresh conversation from the supplied context
                // so the running AI-assistant conversation (`conv`) is left
                // untouched. The answer goes to the oneshot `reply` channel.
                let mut c = chatx_agent::Conversation::new(SYSTEM_PROMPT);
                for (is_assistant, text) in context {
                    if is_assistant {
                        c.assistant(text);
                    } else {
                        c.user(text);
                    }
                }
                c.user(question);
                c.trim_to_budget(2048);
                let mut collected = String::new();
                match backend.generate(&c) {
                    Ok(mut iter) => {
                        for tok in iter.by_ref() {
                            collected.push_str(&tok);
                        }
                    }
                    Err(e) => {
                        collected = format!("(AI error: {e})");
                    }
                }
                let _ = reply.send(collected);
            }
            Cmd::Send(text) => {
                let tx = OUT_TX.lock().unwrap().clone();
                let Some(tx) = tx else { continue };

                let ui_text = text.clone();
                conv.user(text);

                let _ = tx.send(UiEvent::Row(AiMessageRow {
                    side: 0, pending: false, text: SharedString::from(ui_text),
                }));
                let _ = tx.send(UiEvent::Row(AiMessageRow {
                    side: 1, pending: true, text: SharedString::new(),
                }));

                let mut collected = String::new();
                match backend.generate(&conv) {
                    Ok(mut iter) => {
                        for tok in iter.by_ref() {
                            collected.push_str(&tok);
                            if tx.send(UiEvent::Text(tok)).is_err() {
                                break;
                            }
                        }
                        conv.assistant(collected.clone());
                        let _ = tx.send(UiEvent::Done);
                        // Persist the assistant turn to the AI conversation (type-2)
                        // on the UI/runtime side — registered by lib.rs.
                        if let Some(h) = DONE_HOOK.lock().unwrap().as_ref() {
                            h(collected);
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(UiEvent::Err(e.to_string()));
                    }
                }
            }
        }
    }
}

pub fn send(text: &str) {
    ensure_actor();
    let Some(cmd_tx) = CMD.lock().unwrap().clone() else { return };
    let _ = cmd_tx.send(Cmd::Send(text.to_string()));
}

/// Rebuild the actor's backend from the currently-selected model. Called after
/// a model finishes downloading or the user picks a different one, so the next
/// turn uses the real LLM instead of the echo backend.
pub fn reload() {
    ensure_actor();
    let Some(cmd_tx) = CMD.lock().unwrap().clone() else { return };
    let _ = cmd_tx.send(Cmd::Reload);
}

/// Replace the running conversation with `turns` (each `(is_assistant, text)`),
/// so the next turn continues a past AI conversation. Called when opening a
/// type-2 conversation from the chat list.
pub fn load(turns: Vec<(bool, String)>) {
    ensure_actor();
    let Some(cmd_tx) = CMD.lock().unwrap().clone() else { return };
    let _ = cmd_tx.send(Cmd::Load(turns));
}

/// One-shot question (the `@bot` chat reply path). Asks the actor to answer
/// `question` given `context` (chronological `(is_assistant, text)` turns)
/// and blocks until the full answer is ready (or a 120s timeout). The actor's
/// running AI-assistant conversation is left untouched.
pub fn ask_on(
    context: Vec<(bool, String)>,
    question: String,
) -> Result<String, String> {
    ensure_actor();
    let Some(cmd_tx) = CMD.lock().unwrap().clone() else {
        return Err("AI actor unavailable".to_string());
    };
    let (tx, rx) = channel::<String>();
    cmd_tx
        .send(Cmd::Ask { context, question, reply: tx })
        .map_err(|e| e.to_string())?;
    rx.recv_timeout(std::time::Duration::from_secs(120))
        .map_err(|e| format!("AI reply timed out: {e}"))
}

pub fn reset(weak: slint::Weak<MainWindow>) {
    ensure_actor();
    let Some(cmd_tx) = CMD.lock().unwrap().clone() else { return };
    let _ = cmd_tx.send(Cmd::Reset);
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        let a = ui.global::<AiAssistantState>();
        a.set_messages(slint::ModelRc::new(slint::VecModel::from(Vec::<AiMessageRow>::new())));
        a.set_error(SharedString::new());
        a.set_busy(false);
    });
}

pub fn pump(weak: slint::Weak<MainWindow>) {
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        let a = ui.global::<AiAssistantState>();
        // Take the receiver out of the static (moves it, no Clone needed),
        // drain whatever is queued, and put it back.
        let rx = OUT_RX.lock().unwrap().take();
        let Some(rx) = rx else { return };

        let mut finished = false;
        while let Some(ev) = rx.try_recv().ok() {
            match ev {
                UiEvent::Row(row) => push_row(&a, row),
                UiEvent::Text(tok) => append_token(&a, tok),
                UiEvent::Err(msg) => {
                    a.set_error(SharedString::from(msg));
                    finalize(&a);
                    finished = true;
                    break;
                }
                UiEvent::Done => {
                    finalize(&a);
                    finished = true;
                    break;
                }
            }
        }
        *OUT_RX.lock().unwrap() = Some(rx);
        let _ = finished;
    });
}

fn push_row(a: &AiAssistantState, row: AiMessageRow) {
    use slint::Model as _;
    let mut rows: Vec<AiMessageRow> = (0..a.get_messages().row_count())
        .map(|i| a.get_messages().row_data(i).unwrap_or_default())
        .collect();
    rows.push(row);
    a.set_messages(slint::ModelRc::new(slint::VecModel::from(rows)));
}

fn append_token(a: &AiAssistantState, tok: String) {
    use slint::Model as _;
    let n = a.get_messages().row_count();
    if n == 0 { return; }
    let mut rows: Vec<AiMessageRow> = (0..n)
        .map(|i| a.get_messages().row_data(i).unwrap_or_default())
        .collect();
    let last = rows.last_mut().unwrap();
    last.text = format!("{}{}", last.text, tok).into();
    a.set_messages(slint::ModelRc::new(slint::VecModel::from(rows)));
}

fn finalize(a: &AiAssistantState) {
    use slint::Model as _;
    let n = a.get_messages().row_count();
    if n > 0 {
        let mut rows: Vec<AiMessageRow> = (0..n)
            .map(|i| a.get_messages().row_data(i).unwrap_or_default())
            .collect();
        if let Some(last) = rows.last_mut() {
            last.pending = false;
        }
        a.set_messages(slint::ModelRc::new(slint::VecModel::from(rows)));
    }
    a.set_busy(false);
}
