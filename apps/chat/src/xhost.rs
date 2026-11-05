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
/// Probe order:
///   1. The cwd — covers `cargo run` from the repo root (dev layout, where
///      `x-t/` sits beside the source tree).
///   2. The `.app` bundle's `Resources/x-t` — the artifact is shipped inside
///      `Chatx.app/Contents/Resources/x-t` (see `scripts/build_osx.sh`), so a
///      released bundle is self-contained and works even after being copied
///      to `/Applications` or another machine.
///   3. Walking up from the executable's directory — covers any other host
///      layout where the exe is somewhere under the repo.
fn find_hello_wasm() -> Result<PathBuf, String> {
    let rel = PathBuf::from("x-t/x.hello/0.1.0/app.wasm");
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Ok(d) = std::env::current_dir() {
        bases.push(d);
    }
    // Android / app-private data dir: `home_dir()` (== the Java `getFilesDir()`
    // where `installAssets` copies the asset at startup).
    let home = chatx_core::identity::DeviceIdentity::home_dir();
    bases.push(home);
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Bundle: dir is `Chatx.app/Contents/MacOS`; resources live at
            // `Chatx.app/Contents/Resources`.
            if let Some(contents) = dir.parent() {
                bases.push(contents.join("Resources"));
            }
            for up in 0..6 {
                let mut p = dir.to_path_buf();
                for _ in 0..up {
                    p = p.parent().unwrap_or(dir).to_path_buf();
                }
                bases.push(p);
            }
        }
    }
    for b in bases {
        let c = b.join(&rel);
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
    const ASSET: &str = "x-t/x.hello/0.1.0/app.wasm";
    // Primary path: read via the JNI asset bridge (APK `assets/`).
    // Fallback: a plain filesystem probe (covers the Java `installAssets`
    // copy into the app-private files dir, and desktop dev layouts).
    let bytes = if let Some(b) = bridge::read_asset(ASSET) {
        tracing::info!("[x] read_asset hit: {} bytes", b.len());
        b
    } else {
        let path = match find_hello_wasm() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("[x] filesystem probe failed: {e}");
                return Err(e);
            }
        };
        tracing::info!("[x] using filesystem path: {}", path.display());
        let b = std::fs::read(&path).map_err(|e| e.to_string())?;
        tracing::info!("[x] loaded via fs: {} bytes", b.len());
        b
    };
    let inst = wasmhost::open("x.hello", bytes, width, height, dpr_x10)
        .map_err(|e| {
            tracing::warn!("[x] wasmhost::open failed: {e}");
            e.to_string()
        })?;
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
