//! X mini-app (wasm sandbox) bridge for the chat UI.
//!
//! Owns at most one live `wasmhost::PluginInstance` at a time (P0: x.hello).
//! The page side (`ui/x.slint`) polls `pump-mini-app` on a 16 ms timer;
//! each pump pulls the latest committed frame out of the sandbox and sets it
//! onto the `AppHost` global. Popping the page drives `close()`.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use slint::SharedString;

use wasmhost::{InputEvent, PluginInstance};

static INST: LazyLock<Mutex<Option<Arc<PluginInstance>>>> =
    LazyLock::new(|| Mutex::new(None));

/// The one live mini-app instance, if any.
pub fn current() -> Option<Arc<PluginInstance>> {
    INST.lock().unwrap().clone()
}

/// Locate the built `x.hello` artifact (`scripts/build_x.sh` output).
///
/// Dev layout: the chat exe lives at `<root>/apps/chat` while the artifact
/// sits at `<root>/x-t/x.hello/0.1.0/app.wasm`, so we probe the cwd first
/// (running from the repo root) and fall back to paths resolved from the
/// executable location.
fn find_hello_wasm() -> Result<PathBuf, String> {
    let rel = PathBuf::from("x-t/x.hello/0.1.0/app.wasm");
    let mut cands: Vec<PathBuf> = Vec::new();
    cands.push(std::env::current_dir().ok().map(|d| d.join(&rel)).unwrap_or_default());
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for up in 0..4 {
                let mut p = dir.to_path_buf();
                for _ in 0..up {
                    p = p.parent().unwrap_or(dir).to_path_buf();
                }
                cands.push(p.join(&rel));
            }
        }
    }
    for c in cands {
        if c.is_file() {
            return Ok(c);
        }
    }
    Err(
        "x-t/x.hello/0.1.0/app.wasm not found — run scripts/build_x.sh from the repo root"
            .into(),
    )
}

/// Start the x.hello plugin at the given canvas size (canvas px + dpr × 10).
/// No-op if an instance is already live (page is re-entered quickly).
pub fn open_hello(width: u32, height: u32, dpr_x10: u32) -> Result<(), String> {
    if current().is_some() {
        // Re-opened before the old one was released; keep the live one.
        return Ok(());
    }
    let path = find_hello_wasm()?;
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let inst = wasmhost::open("x.hello", bytes, width, height, dpr_x10)
        .map_err(|e| e.to_string())?;
    *INST.lock().unwrap() = Some(inst);
    Ok(())
}

/// Push a touch event (kind per `appsdk::EV_*`, coords in canvas px).
pub fn push_touch(kind: i32, x: i32, y: i32) {
    if let Some(inst) = current() {
        inst.push_input(InputEvent::new(kind, x, y, 0));
    }
}

/// Shut the plugin down (idempotent) and drop the handle.
pub fn close() {
    if let Some(inst) = INST.lock().unwrap().take() {
        let _ = inst.close();
    }
}

/// Convert a committed frame into a Slint `image` property value.
pub fn frame_image(f: &wasmhost::Frame) -> slint::Image {
    if f.width == 0 || f.height == 0 || f.rgba.is_empty() {
        return slint::Image::default();
    }
    let n = (f.width as usize).saturating_mul(f.height as usize);
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(f.width, f.height);
    let dst = buf.make_mut_bytes();
    let l = (n * 4).min(f.rgba.len()).min(dst.len());
    dst[..l].copy_from_slice(&f.rgba[..l]);
    slint::Image::from_rgba8(buf)
}

/// Refresh status line / error state after a settle (called from the pump).
pub fn error_status(inst: &PluginInstance) -> SharedString {
    match inst.settled_error() {
        Some(msg) => SharedString::from(msg),
        None => SharedString::from("plugin exited"),
    }
}
