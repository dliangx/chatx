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
    Reset,
}

/// The actor's command sender (created once by [`ensure_actor`]).
static CMD: std::sync::Mutex<Option<Sender<Cmd>>> = std::sync::Mutex::new(None);

/// The app-lifetime output channel from the actor to the UI. One pair for the
/// process lifetime. `OUT_TX` is moved into the actor at spawn time; `OUT_RX`
/// is owned by this crate and drained on each `pump`.
static OUT_TX: std::sync::Mutex<Option<Sender<UiEvent>>> = std::sync::Mutex::new(None);
static OUT_RX: std::sync::Mutex<Option<Receiver<UiEvent>>> = std::sync::Mutex::new(None);

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
            Cmd::Reset => {
                conv = chatx_agent::Conversation::new(SYSTEM_PROMPT);
            }
            Cmd::Send(text) => {
                let tx = OUT_TX.lock().unwrap().clone();
                let Some(tx) = tx else { continue };

                let ui_text = text.clone();
                conv.user(text);
                conv.trim_to_budget(2048);

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
                        conv.assistant(collected);
                        let _ = tx.send(UiEvent::Done);
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
