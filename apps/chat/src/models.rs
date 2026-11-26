//! AI-assistant model manager: a built-in GGUF catalog (split by platform) and
//! a resumable download manager with pause / cancel + live progress.
//!
//! Each download runs on its own OS thread (ureq is synchronous), streams to a
//! `.part` file (so an interrupted download keeps its progress and can resume),
//! and publishes progress to the `AiAssistantState` global. A finished model is
//! registered as the active model via `chatx_agent::set_selected_model`, so the
//! next assistant turn uses the real LLM instead of the echo backend.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use slint::ComponentHandle;
use slint::SharedString;

use crate::AiAssistantState;

type Flag = Arc<AtomicBool>;

fn platform() -> &'static str {
    if cfg!(target_os = "ios") {
        "ios"
    } else if cfg!(target_os = "android") {
        "android"
    } else {
        "desktop"
    }
}

/// The built-in model catalog for the current platform, read from the
/// user-writable `config.toml` (see `chatx_core::config`). Returns an empty
/// list when the platform section is absent or malformed.
fn catalog() -> Vec<(String, String)> {
    chatx_core::config::model_catalog(platform())
        .into_iter()
        .map(|e| (e.name, e.url))
        .collect()
}

/// Resolve the download URL for `name` using the currently-selected source:
/// the huggingface.co URL from the catalog, remapped to hf-mirror.com when the
/// mirror is enabled.
fn url_for(name: &str) -> Option<String> {
    let mirror = STATE.lock().unwrap().mirror;
    let list = catalog();
    let (_, url) = list.iter().find(|(n, _)| *n == name)?;
    Some(if mirror {
        url.replacen("https://huggingface.co/", "https://hf-mirror.com/", 1)
    } else {
        url.to_string()
    })
}

/// Switch the download source (false = huggingface.co, true = hf-mirror.com).
pub fn set_source(mirror: bool) {
    STATE.lock().unwrap().mirror = mirror;
}

/// Whether the mirror source is currently selected.
pub fn is_mirror() -> bool {
    STATE.lock().unwrap().mirror
}

fn models_dir() -> PathBuf {
    let base = chatx_core::identity::DeviceIdentity::home_dir();
    let p = crate::profile().trim().to_lowercase();
    let dir = if p.is_empty() { base } else { base.join(p) };
    dir.join("models")
}

fn file_for(name: &str) -> PathBuf {
    models_dir().join(format!("{name}.gguf"))
}

fn part_for(name: &str) -> PathBuf {
    models_dir().join(format!("{name}.gguf.part"))
}

/// Where the active model's catalog name is persisted (same profile-scoped dir
/// as the `.gguf` files), so the next launch auto-selects it.
fn selected_model_file() -> PathBuf {
    models_dir().join("selected-model")
}

/// Persist the active model's catalog name to disk.
fn save_selected(name: &str) {
    let _ = std::fs::create_dir_all(models_dir());
    let _ = std::fs::write(selected_model_file(), name);
}

/// The persisted active model, if it is a valid catalog entry whose `.gguf` is
/// present on disk. Used to restore the selection on launch.
pub fn persisted_selected() -> Option<String> {
    let name = std::fs::read_to_string(selected_model_file()).ok()?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let list = catalog();
    if !list.iter().any(|(n, _)| *n == name) {
        return None;
    }
    if !file_for(&name).exists() {
        return None;
    }
    Some(name)
}

struct State {
    status: HashMap<String, String>,
    done: HashMap<String, u64>,
    total: HashMap<String, u64>,
    error: HashMap<String, String>,
    cancel: HashMap<String, Flag>,
    pause: HashMap<String, Flag>,
    selected: String,
    /// True when downloads go through the hf-mirror.com mirror instead of
    /// huggingface.co directly. Chosen by the user in the model picker.
    mirror: bool,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State {
    status: HashMap::new(),
    done: HashMap::new(),
    total: HashMap::new(),
    error: HashMap::new(),
    cancel: HashMap::new(),
    pause: HashMap::new(),
    selected: String::new(),
    mirror: false,
}));

static WEAK_UI: Mutex<Option<slint::Weak<crate::MainWindow>>> = Mutex::new(None);

pub fn set_ui(weak: slint::Weak<crate::MainWindow>) {
    *WEAK_UI.lock().unwrap() = Some(weak);
}

fn init(s: &mut State) {
    if !s.status.is_empty() { return; }
    // Persisted selection (survives restarts) wins over the CHATX_MODEL env
    // override; both fall back to the built-in default ("").
    s.selected = persisted_selected().unwrap_or_else(|| crate::selected_model_name());
    for (name, _) in catalog() {
        if file_for(&name).exists() {
            s.status.insert(name.to_string(), "ready".into());
            s.done.insert(name.to_string(), u64::MAX);
            s.total.insert(name.to_string(), u64::MAX);
        } else {
            s.status.insert(name.to_string(), "not-started".into());
            s.done.insert(name.to_string(), 0);
            s.total.insert(name.to_string(), 0);
        }
    }
}

fn frac(s: &State, name: &str) -> f32 {
    let total = s.total.get(name).copied().unwrap_or(0);
    let done = s.done.get(name).copied().unwrap_or(0);
    if total == 0 || total == u64::MAX {
        return if done == u64::MAX { 1.0 } else { 0.0 };
    }
    (done as f32 / total as f32).min(1.0)
}

fn to_row(name: &str, s: &State) -> crate::AiModelRow {
    crate::AiModelRow {
        name: SharedString::from(name),
        status: SharedString::from(s.status.get(name).map(|v| v.as_str()).unwrap_or("not-started")),
        progress: frac(s, name),
        error: SharedString::from(s.error.get(name).map(|v| v.as_str()).unwrap_or("")),
        selected: *name == s.selected,
    }
}

pub fn rows() -> Vec<crate::AiModelRow> {
    let mut s = STATE.lock().unwrap();
    init(&mut *s);
    catalog().iter().map(|(name, _)| to_row(name, &s)).collect()
}

fn publish_rows() {
    let rows = rows();
    let Some(weak) = WEAK_UI.lock().unwrap().clone() else { return };
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else { return };
        let a = ui.global::<AiAssistantState>();
        a.set_models(slint::ModelRc::new(slint::VecModel::from(rows)));
    });
}

pub fn select(name: &str) {
    let path = file_for(name);
    if !path.exists() { return; }
    STATE.lock().unwrap().selected = name.to_string();
    save_selected(name);
    chatx_agent::set_selected_model(Some(path));
    crate::ai::reload();
    publish_rows();
}

/// On launch, restore the persisted model selection: point the runtime override
/// at its `.gguf` so the actor's backend (and the first turn) uses the real LLM
/// instead of the echo backend. Safe no-op if nothing is persisted yet.
pub fn restore_on_launch() {
    let Some(name) = persisted_selected() else { return };
    let path = file_for(&name);
    chatx_agent::set_selected_model(Some(path));
}

pub fn download(name: &str) {
    let Some(url) = url_for(name) else { return };
    let name = name.to_string();
    let cancel: Flag = Arc::new(AtomicBool::new(false));
    let pause: Flag = Arc::new(AtomicBool::new(false));
    {
        let mut s = STATE.lock().unwrap();
        init(&mut *s);
        s.cancel.insert(name.clone(), cancel.clone());
        s.pause.insert(name.clone(), pause.clone());
        s.status.insert(name.clone(), "downloading".into());
        s.error.insert(name.clone(), String::new());
    }
    publish_rows();
    let nm = name.clone();
    std::thread::Builder::new()
        .name(format!("chatx-model-{nm}"))
        .spawn(move || download_worker(name, url, cancel, pause))
        .ok();
}

pub fn cancel(name: &str) {
    {
        let mut s = STATE.lock().unwrap();
        if let Some(flag) = s.cancel.get(name).cloned() {
            flag.store(true, Ordering::Relaxed);
        }
        s.cancel.remove(name);
        s.pause.remove(name);
        s.status.insert(name.to_string(), "not-started".into());
        s.done.insert(name.to_string(), 0);
        s.error.insert(name.to_string(), String::new());
    }
    // The lock is released before publishing: `publish_rows -> rows` re-locks
    // `STATE`, so it must not be held here (a non-reentrant deadlock otherwise).
    publish_rows();
}

pub fn pause_toggle(name: &str) {
    {
        let mut s = STATE.lock().unwrap();
        let Some(flag) = s.pause.get(name).cloned() else { return };
        let was_paused = flag.load(Ordering::Relaxed);
        flag.store(!was_paused, Ordering::Relaxed);
        s.status.insert(name.to_string(), if was_paused { "downloading".into() } else { "paused".into() });
    }
    publish_rows();
}

fn set_result(name: &str, status: &str, error: &str) {
    {
        let mut s = STATE.lock().unwrap();
        s.status.insert(name.to_string(), status.to_string());
        if !error.is_empty() { s.error.insert(name.to_string(), error.to_string()); }
        s.cancel.remove(name);
        s.pause.remove(name);
    }
    if !error.is_empty() {
        tracing::error!("[model] download failed: {name} — {error}");
    }
    publish_rows();
}

fn finalize(name: &str, file: &mut std::fs::File, part: &PathBuf, dst: &PathBuf) {
    let _ = file.flush();
    let _ = file.sync_all();
    if std::fs::rename(part, dst).is_err() {
        set_result(name, "error", "finalize failed");
        return;
    }
    chatx_agent::set_selected_model(Some(dst.clone()));
    save_selected(name);
    {
        let mut s = STATE.lock().unwrap();
        s.status.insert(name.to_string(), "ready".into());
        s.done.insert(name.to_string(), u64::MAX);
        s.total.insert(name.to_string(), u64::MAX);
        s.selected = name.to_string();
        s.error.insert(name.to_string(), String::new());
    }
    publish_rows();
    crate::ai::reload();
}

/// Outcome of a single download pass (there is one user-selected source).
enum Outcome {
    /// Body fully read — finalize the download.
    Finished,
    /// User cancelled.
    Cancelled,
    /// Network / IO failure.
    Failed(String),
}

/// Drive one `url` to the end of the file body, appending into `file`.
/// Resumes from `*done` via a `Range` request.
fn download_pass(
    name: &str,
    url: &str,
    file: &mut std::fs::File,
    done: &mut u64,
    agent: &ureq::Agent,
    cancel: &Flag,
    pause: &Flag,
) -> Outcome {
    let mut req = agent.request("GET", url);
    if *done > 0 {
        req = req.set("Range", &format!("bytes={}-", done));
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(416, _)) => return Outcome::Finished,
        Err(e) => return Outcome::Failed(format!("{e}")),
    };

    if *done > 0 && resp.header("content-range").is_none() {
        // Server ignored the Range header and is sending the whole file again.
        // Truncate the partial so appending this full body rebuilds it cleanly
        // instead of duplicating the already-downloaded bytes.
        let _ = file.set_len(0);
        *done = 0;
    }

    let total = resp
        .header("content-range")
        .and_then(|v| v.rsplit('/').next())
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| resp.header("content-length").and_then(|v| v.parse::<u64>().ok()).map(|cl| cl.max(*done)))
        .unwrap_or(0);
    STATE.lock().unwrap().total.insert(name.to_string(), total);

    let mut reader = std::io::BufReader::new(resp.into_reader());
    let mut buf = [0u8; 64 * 1024];

    loop {
        if cancel.load(Ordering::Relaxed) { return Outcome::Cancelled; }
        if pause.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(150));
            continue;
        }
        match reader.read(&mut buf) {
            Ok(0) => return Outcome::Finished,
            Ok(n) => {
                if file.write_all(&buf[..n]).is_err() {
                    return Outcome::Failed("write failed".into());
                }
                *done += n as u64;
                STATE.lock().unwrap().done.insert(name.to_string(), *done);
                if *done % (16 * 1024 * 1024) < (n as u64) {
                    publish_rows();
                }
            }
            Err(e) => return Outcome::Failed(format!("read: {e}")),
        }
    }
}

fn download_worker(name: String, url: String, cancel: Flag, pause: Flag) {
    let dst = file_for(&name);
    let part = part_for(&name);
    let _ = std::fs::create_dir_all(models_dir());

    let mut file = match std::fs::OpenOptions::new().create(true).append(true).open(&part) {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            set_result(&name, "error", &format!("open: {e}"));
            return;
        }
    };
    let mut done: u64 = file.metadata().map(|m| m.len()).unwrap_or(0);

    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .build();

    match download_pass(&name, &url, &mut file, &mut done, &agent, &cancel, &pause) {
        Outcome::Finished => finalize(&name, &mut file, &part, &dst),
        Outcome::Cancelled => set_result(&name, "paused", ""),
        Outcome::Failed(e) => set_result(&name, "error", &e),
    }
}
