//! X mini-app (wasm sandbox) bridge for the chat UI.
//!
//! Owns at most one live `wasmhost::PluginInstance` at a time (P0: x.hello).
//! The page side (`ui/x.slint`) polls `pump-mini-app` on a 16 ms timer;
//! each pump pulls the latest committed frame out of the sandbox and sets it
//! onto the `AppHost` global. Popping the page drives `close()`.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use slint::SharedString;

use wasmhost::{InputEvent, PluginInstance, EV_KEY_DOWN};

static INST: LazyLock<Mutex<Option<Arc<PluginInstance>>>> =
    LazyLock::new(|| Mutex::new(None));

/// The live plugin's canvas scale factor (device px per logical px), set at
/// open time. The overlay forwards *logical* px from Slint, but the plugin's
/// canvas — and every gesture threshold keyed to it (edge bands, the HUD
/// hit-rect) — is in *device* px (`init` multiplies by dpr). Without this, a
/// right-edge tap on a 2× display lands in canvas *centre* and is ignored,
/// while a left-edge tap (still inside the band) works: the observed
/// "tap-left ok, tap-right dead" split.
static DPR: LazyLock<Mutex<f32>> = LazyLock::new(|| Mutex::new(1.0));

fn set_dpr(dpr_x10: u32) {
    let d = if dpr_x10 == 0 { 1.0 } else { dpr_x10 as f32 / 10.0 };
    *DPR.lock().unwrap() = d.max(1.0);
}

/// The one live mini-app instance, if any.
pub fn current() -> Option<Arc<PluginInstance>> {
    INST.lock().unwrap().clone()
}

/// Locate a built X-mini-app artifact by app id
/// (`x-t/<app_id>/0.1.0/app.wasm`, `scripts/build_x.sh` output).
///
/// Dev layout: the chat exe lives at `<root>/apps/chat` while the artifact
/// sits at `<root>/x-t/…`, so we probe the cwd first (running from the repo
/// root) and fall back to paths resolved from the executable location.
fn find_app_wasm(app_id: &str) -> Result<PathBuf, String> {
    let rel = PathBuf::from(format!("x-t/{app_id}/0.1.0/app.wasm"));
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
    Err(format!(
        "{} not found — run scripts/build_x.sh from the repo root",
        rel.display()
    ))
}

/// Start the x.hello plugin at the given canvas size (canvas px + dpr × 10).
/// No-op if an instance is already live (page is re-entered quickly).
pub fn open_hello(width: u32, height: u32, dpr_x10: u32) -> Result<(), String> {
    if current().is_some() {
        // Re-opened before the old one was released; keep the live one.
        return Ok(());
    }
    let path = find_app_wasm("x.hello")?;
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let inst = wasmhost::open("x.hello", bytes, width, height, dpr_x10)
        .map_err(|e| e.to_string())?;
    set_dpr(dpr_x10);
    *INST.lock().unwrap() = Some(inst);
    Ok(())
}

/// Start x.pdfview with the document at `file_path`. The host reads the file
/// bytes here (the sandbox has **no** pre-opens) and passes them to the
/// plugin through the `x_file_*` imports.
///
/// No-op if an instance is already live.
pub fn open_pdf_file(
    width: u32,
    height: u32,
    dpr_x10: u32,
    file_path: &std::path::Path,
) -> Result<(), String> {
    if current().is_some() {
        return Ok(());
    }
    let app_path = find_app_wasm("x.pdfview")?;
    let wasm = std::fs::read(&app_path).map_err(|e| e.to_string())?;
    let bytes = std::fs::read(file_path).map_err(|e| format!("read {}: {e}", file_path.display()))?;
    let name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("document.pdf")
        .to_string();
    let inst = wasmhost::open_with_file(
        "x.pdfview",
        wasm,
        width,
        height,
        dpr_x10,
        Some(name),
        Some(bytes),
    )
    .map_err(|e| e.to_string())?;
    set_dpr(dpr_x10);
    *INST.lock().unwrap() = Some(inst);
    Ok(())
}

/// Push a touch event (kind per `appsdk::EV_*`). The overlay hands us
/// *logical* px; the plugin's gesture space is *device* px, so scale by the
/// canvas dpr before forwarding (see `DPR`).
pub fn push_touch(kind: i32, x: i32, y: i32) {
    if let Some(inst) = current() {
        let s = *DPR.lock().unwrap();
        inst.push_input(InputEvent::new(kind, (x as f32 * s).round() as i32, (y as f32 * s).round() as i32, 0));
    }
}

/// Push a wheel / trackpad scroll delta (logical px, Slint `scroll-event`
/// convention: negative = the user scrolled down, toward later content).
/// Scaled to canvas device px and forwarded as an `EV_SCROLL` event whose
/// `y` field carries the vertical delta.
pub fn push_scroll(dy: f32) {
    if let Some(inst) = current() {
        let s = *DPR.lock().unwrap();
        inst.push_input(InputEvent::new(
            wasmhost::EV_SCROLL,
            0,
            (dy * s).round() as i32,
            0,
        ));
    }
}

/// Tell the live plugin which view mode to render in: `1` = width-fit
/// (宽度适配), `2` = height-fit (高度适配) — the `wasmhost::VIEW_MODE_*`
/// codes. Routed over the plugin's input channel as a key event
/// (`EV_KEY_DOWN` + mode code), which the plugin honours on its next tick.
pub fn push_mode(mode: i32) {
    if let Some(inst) = current() {
        inst.push_input(InputEvent::new(EV_KEY_DOWN, 0, 0, mode));
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
