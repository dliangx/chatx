//! Shared Apple (macOS + iOS) camera implementation — driven by a Swift
//! AVFoundation shim compiled from `shim/shim.swift` and linked into the
//! final `chatx` binary (see `build.rs` of this crate). No `objc2` deps:
//! the Swift side owns all Foundation/AVFoundation types, and only a small
//! C-ABI surface crosses the language boundary (the 5 `chatx_camera_*`
//! entry points defined in Swift, plus 3 Rust → Swift trampolines defined
//! here).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Global sink, installed via [`Camera::set_sink`], called from the
/// `_chatx_rust_qr` trampoline (which runs on the Swift metadata delegate
/// queue) with every decoded QR payload.
static SINK: OnceLock<Mutex<Box<dyn Fn(String) + Send>>> = OnceLock::new();

/// Global preview-frame sink, installed via [`Camera::set_frame_sink`],
/// called from `_chatx_rust_frame` with each preview frame as
/// `(bytes, w, h, fmt)`. `fmt` is a `crates/bridge::types::PixelFormat`
/// value (1 = RGBA8888, 8 = Gray8).
type FrameSinkFn = Box<dyn Fn(&[u8], u32, u32, u32) + Send>;
static FRAME_SINK: OnceLock<Mutex<FrameSinkFn>> = OnceLock::new();

/// Global sink, installed via [`Camera::set_capture_sink`], called from
/// `_chatx_rust_capture_done` with the saved file path (or error) when a
/// `Camera::capture` finishes.
pub type CaptureResult = Result<String, String>;
static CAPTURE_SINK: OnceLock<Mutex<Box<dyn Fn(CaptureResult) + Send>>> = OnceLock::new();

/// `1` if the camera session is currently running (per the Swift shim);
/// tracked on the Rust side in case the shim is not yet running.
///
/// The Swift shim exposes `chatx_camera_is_running()` — we mirror it for
/// our own bookkeeping so `start()` is idempotent from the Rust viewpoint
/// even when called repeatedly.
static LOCAL_RUNNING: AtomicBool = AtomicBool::new(false);

// ── Swift → Rust callbacks ──────────────────────────────────────────────────
// The Swift shim declares these via `@_silgen_name` — they are C-ABI symbols
// that the Swift object file references and our static library must supply.

/// Decoded QR payload (`payload` is UTF-8 of length `len`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _chatx_rust_qr(payload: *const u8, len: i32) {
    if payload.is_null() || len <= 0 {
        return;
    }
    let s = unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(payload, len as usize)) };
    let s = s.to_string();
    if s.trim().is_empty() {
        return;
    }
    if let Some(slot) = SINK.get() {
        if let Ok(g) = slot.lock() {
            (g)(s);
        }
    }
}

/// One preview frame. `data` is `len` bytes in the layout implied by `fmt`
/// (4 B/px for RGBA8888, 1 B/px for Gray8). `w` / `h` are the image
/// dimensions; `fmt` is one of the `bridge::types::PixelFormat` values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _chatx_rust_frame(data: *const u8, len: i32, w: u32, h: u32, fmt: u32) {
    if data.is_null() || len <= 0 || w == 0 || h == 0 {
        return;
    }
    let slice = unsafe { std::slice::from_raw_parts(data, len as usize) };
    if let Some(slot) = FRAME_SINK.get() {
        if let Ok(g) = slot.lock() {
            (g)(slice, w, h, fmt);
        }
    }
}

/// Result of a `Camera::capture`: either a saved file path or an error
/// message. Exactly one of `path` / `err` is non-empty, signalled by `ok`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _chatx_rust_capture_done(
    ok: i32,
    path: *const u8,
    path_len: i32,
    err: *const u8,
    err_len: i32,
) {
    let read = |p: *const u8, l: i32| -> String {
        if p.is_null() || l <= 0 {
            return String::new();
        }
        unsafe { String::from_utf8_lossy(std::slice::from_raw_parts(p, l as usize)).to_string() }
    };
    let res = if ok != 0 {
        Ok(read(path, path_len))
    } else {
        Err(read(err, err_len))
    };
    if let Some(slot) = CAPTURE_SINK.get() {
        if let Ok(g) = slot.lock() {
            (g)(res);
        }
    }
}

// ── Rust → Swift (the shim) ─────────────────────────────────────────────────

unsafe extern "C" {
    fn chatx_camera_start(err: *mut u8, err_len: u32) -> i32;
    fn chatx_camera_stop();
    fn chatx_camera_is_running() -> i32;
    fn chatx_camera_capture() -> i32;
}

/// A (possibly running) AVFoundation capture session with QR recognition +
/// live video preview + still photo. Backed by a Swift AVFoundation shim
/// (see `shim/shim.swift`) — all Apple-side state lives in Swift.
pub struct Camera;

impl Default for Camera {
    fn default() -> Self {
        Self::new()
    }
}

impl Camera {
    pub fn new() -> Self {
        Camera
    }

    /// `true` if the shim reports the session as currently running.
    pub fn is_running(&self) -> bool {
        LOCAL_RUNNING.load(Ordering::SeqCst)
            && unsafe { chatx_camera_is_running() } != 0
    }

    /// Install the sink used for decoded QR payloads. Can be called any time;
    /// the next decode event uses the newest closure.
    pub fn set_sink<F: Fn(String) + Send + 'static>(&self, sink: F) {
        if let Some(slot) = SINK.get() {
            if let Ok(mut g) = slot.lock() {
                *g = Box::new(sink);
            }
        } else {
            let _ = SINK.set(std::sync::Mutex::new(Box::new(sink)));
        }
    }

    /// Install the preview-frame sink. Called on the shim's video delegate
    /// queue with every captured frame as `(bytes, w, h, fmt)`.
    pub fn set_frame_sink<F: Fn(&[u8], u32, u32, u32) + Send + 'static>(&self, sink: F) {
        if let Some(slot) = FRAME_SINK.get() {
            if let Ok(mut g) = slot.lock() {
                *g = Box::new(sink);
            }
        } else {
            let _ = FRAME_SINK.set(std::sync::Mutex::new(Box::new(sink)));
        }
    }

    /// Install the sink used for photo-capture results.
    pub fn set_capture_sink<F: Fn(CaptureResult) + Send + 'static>(&self, sink: F) {
        if let Some(slot) = CAPTURE_SINK.get() {
            if let Ok(mut g) = slot.lock() {
                *g = Box::new(sink);
            }
        } else {
            let _ = CAPTURE_SINK.set(std::sync::Mutex::new(Box::new(sink)));
        }
    }

    /// Open the default camera and start the metadata (QR) + video (preview)
    /// + photo outputs (delegates to the Swift shim).
    pub fn start(&mut self) -> Result<(), String> {
        if self.is_running() {
            return Ok(());
        }
        let mut buf = [0u8; 1024];
        let r = unsafe { chatx_camera_start(buf.as_mut_ptr(), buf.len() as u32) };
        if r == 0 {
            LOCAL_RUNNING.store(true, Ordering::SeqCst);
            Ok(())
        } else {
            let len = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            let msg = String::from_utf8_lossy(&buf[..len]).to_string();
            Err(if msg.is_empty() {
                "启动摄像头失败（原因未知）".to_string()
            } else {
                msg
            })
        }
    }

    /// Trigger a still-photo capture. The saved path (or error) is delivered
    /// asynchronously to the sink installed via
    /// [`Camera::set_capture_sink`].
    pub fn capture(&mut self) -> Result<(), String> {
        if !self.is_running() {
            return Err("摄像头未启动".to_string());
        }
        if unsafe { chatx_camera_capture() } != 0 {
            return Err("触发拍摄失败（会话未就绪）".to_string());
        }
        Ok(())
    }

    /// Stop the capture session; the instance remains reusable.
    pub fn stop(&mut self) {
        LOCAL_RUNNING.store(false, Ordering::SeqCst);
        unsafe {
            chatx_camera_stop();
        }
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop();
    }
}
