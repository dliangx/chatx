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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};

use slint::ComponentHandle;
use slint::SharedString;

use crate::{AiAssistantState, AiMessageRow, MainWindow};

const SYSTEM_PROMPT: &str = "You are ChatX's built-in assistant. Be concise and helpful. \
     You are running fully locally; you have no network access.";

const TRANSLATE_SYSTEM_PROMPT: &str =
    "You are a precise translator. You translate the user's text into the requested \
     target language and reply with the translation only — no preamble, no quotation, \
     no notes. Preserve line breaks, links and emoji.";

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
    /// One-shot translation: translate `text` into the `target` language.
    /// The answer is delivered to the provided oneshot channel. A separate
    /// actor path (no running AI-assistant conversation is touched) with a
    /// translation-dedicated system prompt.
    Translate {
        text: String,
        target: String,
        reply: std::sync::mpsc::Sender<String>,
    },
}

/// The actor's command sender (created once by [`ensure_actor`]).
static CMD: std::sync::Mutex<Option<Sender<Cmd>>> = std::sync::Mutex::new(None);

/// The actor thread's `JoinHandle`. `None` once [`shutdown`] has joined it.
static ACTOR_THREAD: std::sync::Mutex<Option<std::thread::JoinHandle<()>>> =
    std::sync::Mutex::new(None);

/// The app-lifetime output channel from the actor to the UI. One pair for the
/// process lifetime. `OUT_TX` is moved into the actor at spawn time; `OUT_RX`
/// is owned by this crate and drained on each `pump`.
static OUT_TX: std::sync::Mutex<Option<Sender<UiEvent>>> = std::sync::Mutex::new(None);
static OUT_RX: std::sync::Mutex<Option<Receiver<UiEvent>>> = std::sync::Mutex::new(None);

/// Guards against [`shutdown`] running twice. AppKit's `⌘Q` / red-X path calls
/// `exit()`, which runs C++ static destructors *after* `atexit` handlers — so
/// the Metal residency sets must be emptied on the actor thread **before**
/// `ggml_metal_device_free` runs, or `GGML_ASSERT([rsets->data count] == 0)`
/// aborts the process. The `atexit` handler (see [`install_exit_guard`]) and the
/// normal `run_app` teardown both funnel through here.
static SHUTDOWN_CALLED: AtomicBool = AtomicBool::new(false);

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
    let handle = std::thread::Builder::new()
        .name("chatx-ai-actor".into())
        .spawn(move || {
            *OUT_TX.lock().unwrap() = Some(out_tx);
            actor_loop(cmd_rx);
        })
        .expect("spawn ai actor");
    *CMD.lock().unwrap() = Some(cmd_tx);
    *ACTOR_THREAD.lock().unwrap() = Some(handle);
}

/// Shut the actor down cleanly: end its command stream so the loop exits and
/// the (thread-confined) LLM backend drops **on the actor thread**, then join
/// the thread before returning.
///
/// The local GGUF backend owns Metal resources (GGUF residency sets) that
/// must be released before llama.cpp's C++ static destructors run at process
/// exit; otherwise `ggml_metal_rsets_free` hits `GGML_ASSERT([data count] ==
/// 0)` and aborts the process. Joining the actor first guarantees the backend
/// (and its Metal buffers) are freed before `main` returns, so the static
/// destructors find empty residency sets.
pub fn shutdown() {
    if SHUTDOWN_CALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    // Drop the only sender so the actor's `for cmd in rx` iterator ends (the
    // receiver becomes broken) and the loop returns, dropping the backend.
    *CMD.lock().unwrap() = None;
    let Some(handle) = ACTOR_THREAD.lock().unwrap().take() else { return };
    let _ = handle.join();
}

/// Install an `atexit` handler that tears down the LLM actor **before**
/// llama.cpp's C++ static destructors run.
///
/// Why this is needed: the normal teardown (`run_app` returning from
/// `ui.run()`) already calls [`shutdown`]. But macOS's ⌘Q / red-X path is
/// different — AppKit's `NSApplication terminate:` calls `exit(0)`, which runs
/// C++ static destructors (`__cxa_finalize`) *directly*, never unwinding the
/// Rust stack, so `ai::shutdown()` is never reached. The Metal residency sets
/// still hold the `LlamaContext` buffers, and `ggml_metal_device_free` then
/// trips `GGML_ASSERT([rsets->data count] == 0)` and aborts.
///
/// `atexit` handlers run *before* C++ static destructors (the C standard
/// guarantees `exit()` calls `atexit` handlers first, then runs the
/// static-destruction phase). This handler drops the actor — and with it the
/// `LlamaModel` and the Metal residency-set buffers it owns — on the actor
/// thread, so the static destructors find empty residency sets.
///
/// Safe to call more than once: only the first call registers (the rest are
/// no-ops), and the handler itself is idempotent via [`SHUTDOWN_CALLED`].
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn install_exit_guard() {
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    if REGISTERED.swap(true, Ordering::SeqCst) {
        return;
    }
    // `atexit` requires `extern "C" fn()`, so wrap the idempotent [`shutdown`]
    // (guarded by `SHUTDOWN_CALLED`) in an extern-C trampoline.
    extern "C" fn atexit_shutdown() {
        shutdown();
    }
    unsafe {
        let _ = libc::atexit(atexit_shutdown);
    }
}

/// Build a validated backend for the currently-selected model, running on the
/// actor thread.
///
/// A corrupt-but-loadable GGUF passes `load` but produces garbage (NaN / all-0
///) logits on inference — which is exactly what surfaced as "empty response".
/// So we self-test on a tiny decode. If the pick fails we fall back to the
/// last known-good model (or the built-in mock) and mark the corrupt file as an
/// error so the picker shows it as a re-download target, NOT a selectable model.
fn build_backend() -> Box<dyn chatx_agent::Llm> {
    let (name, path) = crate::models::selected_name_and_path();
    if let Some(ref p) = path {
        match chatx_agent::try_load_selected(p.clone()) {
            Ok(be) => {
                crate::models::report_model_ok(&name);
                return be;
            }
            Err(e) => {
                let detail = e.to_string();
                if let Some(fallback) = crate::models::report_model_corrupt(&name, &detail) {
                    return match chatx_agent::try_load_selected(fallback) {
                        Ok(be) => be,
                        Err(_) => Box::new(chatx_agent::MockBackend::new()),
                    };
                }
                return Box::new(chatx_agent::MockBackend::new());
            }
        }
    }
    Box::new(chatx_agent::MockBackend::new())
}

fn actor_loop(cmd_rx: Receiver<Cmd>) {
    let mut backend = build_backend();
    let mut conv = chatx_agent::Conversation::new(SYSTEM_PROMPT);

    for cmd in cmd_rx {
        match cmd {
            Cmd::Reload => {
                // Drop the previous backend FIRST. The llama runtime is a
                // process-wide singleton (llama-cpp-2's `LLAMA_BACKEND_INITIALIZED`
                // static), so a second `LlamaBackend` cannot be built while the
                // first is still alive — `LlamaRuntime::init` would fail with
                // `BackendAlreadyInitialized` (which is why a model switch only
                // took effect after a restart). Dropping the old backend resets
                // that flag so the new GGUF actually loads.
                drop(backend);
                backend = build_backend();
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
                let mut collected = String::new();
                match backend.generate(&c, &mut |tok| collected.push_str(&tok)) {
                    Ok(full) => {
                        collected = full;
                    }
                    Err(e) => {
                        collected = format!("(AI error: {e})");
                    }
                }
                let _ = reply.send(collected);
            }
            Cmd::Translate { text, target, reply } => {
                // One-shot translation. The conversation is seeded with a
                // translation-dedicated system prompt so the model answers
                // with the translation (and nothing else).
                let mut c =
                    chatx_agent::Conversation::new(TRANSLATE_SYSTEM_PROMPT);
                c.user(format!(
                    "Translate the following text into {target}. Reply with the translation only.\n\n{text}"
                ));
                let mut collected = String::new();
                match backend.generate(&c, &mut |tok| collected.push_str(&tok)) {
                    Ok(full) => {
                        collected = full;
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
                let result = backend.generate(&conv, &mut |tok: String| {
                    // Stream each piece to the UI the moment it is decoded.
                    collected.push_str(&tok);
                    let _ = tx.send(UiEvent::Text(tok));
                });
                match result {
                    Ok(full) => {
                        collected = full;
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

/// One-shot translation (the chat "translate" header button path). Asks the
/// actor to translate `text` into `target` and blocks until the full answer is
/// ready (or a 120s timeout). The actor's running AI-assistant conversation is
/// left untouched — the translation uses its own dedicated system prompt.
pub fn translate(text: &str, target: &str) -> Result<String, String> {
    ensure_actor();
    let Some(cmd_tx) = CMD.lock().unwrap().clone() else {
        return Err("AI actor unavailable".to_string());
    };
    let (tx, rx) = channel::<String>();
    cmd_tx
        .send(Cmd::Translate {
            text: text.to_string(),
            target: target.to_string(),
            reply: tx,
        })
        .map_err(|e| e.to_string())?;
    rx.recv_timeout(std::time::Duration::from_secs(120))
        .map_err(|e| format!("AI translate timed out: {e}"))
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
