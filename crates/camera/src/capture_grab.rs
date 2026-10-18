//! Shared "grab the next preview frame → save as JPEG → fire the capture
//! sink" mechanism. Used by the `desktop` (nokhwa) and `android` (bridge)
//! backends, both of which already deliver every frame to the app's UI
//! preview in greyscale — so a still capture is just "remember to save the
//! next one". The saved still is visually equivalent to the preview (both
//! are the same greyscale frame) and Slint's image backend decodes the JPEG
//! identically.
//!
//! Apple uses the Swift `AVCapturePhotoOutput` path instead (a real
//! high-res photo rather than a preview frame); it has its own sink slot
//! in `appleshim.rs` and does **not** use this module.
//! (Hence the `dead_code` allowance below — on Apple none of it is called.)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::CaptureResult;

type CaptureSink = Box<dyn Fn(CaptureResult) + Send>;

static SINK: OnceLock<Mutex<CaptureSink>> = OnceLock::new();
/// `true` while a capture is pending — the next frame delivered via
/// [`grab`] (called from the per-frame callback) will be saved off-thread
/// and the sink fired exactly once.
static PENDING: AtomicBool = AtomicBool::new(false);

/// Install the capture-result sink. Must be in place before `capture()`.
pub(crate) fn set_sink<F: Fn(CaptureResult) + Send + 'static>(sink: F) {
    if let Some(slot) = SINK.get() {
        if let Ok(mut g) = slot.lock() {
            *g = Box::new(sink);
        }
    } else {
        let _ = SINK.set(Mutex::new(Box::new(sink)));
    }
}

/// Arm a capture: the next [`grab`] call saves that frame. `running` is
/// `Camera::is_running()` so callers can fail fast with a readable error.
pub(crate) fn arm(running: bool) -> Result<(), String> {
    if !running {
        return Err("Camera not started".to_string());
    }
    if SINK.get().is_none() {
        // Defensive: app didn't install a sink — install a no-op so the
        // result below isn't silently dropped.
        let _ = SINK.set(Mutex::new(Box::new(|_| {})));
    }
    PENDING.store(true, Ordering::SeqCst);
    Ok(())
}

/// Called from the per-frame callback with the *grey* preview frame.
/// If a capture is armed, saves it as a JPEG (off-thread so the frame
/// callback stays fast) and fires the sink. No-op when no capture pending.
pub(crate) fn grab(gray: &[u8], w: u32, h: u32) {
    if w == 0 || h == 0 {
        return;
    }
    if !PENDING.swap(false, Ordering::SeqCst) {
        return;
    }
    let buf = gray.to_vec();
    std::thread::spawn(move || {
        let res = save_gray_jpeg(&buf, w, h);
        let msg = res
            .as_ref()
            .map(|p| format!("saved → {p}"))
            .unwrap_or_else(|e| format!("failed: {e}"));
        eprintln!("[camera] still {msg}");
        let Some(slot) = SINK.get() else { return };
        let Ok(g) = slot.lock() else { return };
        (g)(res);
    });
}

fn save_gray_jpeg(buf: &[u8], w: u32, h: u32) -> Result<String, String> {
    let img = image::GrayImage::from_vec(w, h, buf.to_vec())
        .ok_or_else(|| "frame data / size mismatch".to_string())?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join("chatx_camera");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("still_{ts}.jpg"));
    img.save_with_format(&path, image::ImageFormat::Jpeg)
        .map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}
