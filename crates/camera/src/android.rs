//! Android camera capture — consumes frames from the Java shell via the
//! stable `bridge` C-ABI (JNI) and QR-decodes each frame with `rxing`.
//!
//! The Java side (Camera2 + `ImageReader`) is started by the Activity host
//! on `onCreate` and never stops — it is the persistent capture surface for
//! the whole app. `Camera::start()` installs our QR sink into the bridge;
//! `stop()` clears it. The frame thread keeps running; we just ignore frames
//! while stopped.
//!
//! Pixel-format dispatch:
//! - [`bridge::types::PixelFormat::Gray8`]   — Java extracts the Y plane of
//!   a `YUV_420_888` frame and sends it as an 8-bit-plane. Fastest path.
//! - `RGB_565` (0x13) — legacy `Capture.java` from before the Camera2
//!   upgrade; still handled so we don't break older builds.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::capture_grab;
use crate::scan;
use bridge::types::PixelFormat;

type QrSinkFn = Box<dyn Fn(String) + Send + 'static>;
type FrameSinkFn = Box<dyn Fn(&[u8], u32, u32, u32) + Send + 'static>;

static SINK: OnceLock<Mutex<QrSinkFn>> = OnceLock::new();
/// Installed by `Camera::set_frame_sink`. Called from the bridge consumer
/// with every Y-plane (GRAY8) camera frame, in addition to the QR decoder
/// pipeline which runs first. `None` when no preview was requested.
static FRAME_SINK: OnceLock<Mutex<Option<FrameSinkFn>>> = OnceLock::new();

fn notify(payload: String) {
    if let Some(slot) = SINK.get() {
        if let Ok(g) = slot.lock() {
            (g)(payload);
        }
    }
}

fn notify_frame(bytes: &[u8], w: u32, h: u32, fmt: u32) {
    if let Some(slot) = FRAME_SINK.get() {
        if let Ok(g) = slot.lock() {
            if let Some(cb) = g.as_ref() {
                cb(bytes, w, h, fmt);
            }
        }
    }
}

/// Log once to distinguish "no frames" from "decode failing".
static FRAME_SEEN: AtomicBool = AtomicBool::new(false);

/// Callback installed into the bridge. Runs on the JNI caller thread.
fn on_bridge_frame(bytes: &[u8], w: u32, h: u32, fmt: u32) {
    if !FRAME_SEEN.swap(true, Ordering::SeqCst) {
        tracing::debug!("[camera] android: first frame {}x{} fmt={} {}B", w, h, fmt, bytes.len());
    }
    // Reduce every frame to the grey Y-plane the QR decoder, preview sink,
    // and still-capture grabber all consume. GRAY8 is already that plane
    // (no copy); RGB565 is converted to an owned buffer.
    let gray: std::borrow::Cow<'_, [u8]> = match PixelFormat::from_i32(fmt as i32) {
        PixelFormat::Gray8 => std::borrow::Cow::Borrowed(bytes),
        PixelFormat::Rgb565 => match rgb565_to_greyscale(bytes, w, h) {
            Some(v) => std::borrow::Cow::Owned(v),
            None => return,
        },
        _ => {
            tracing::warn!(
                "[camera] android: unsupported pixel format {} ({}x{} {}B) — dropped",
                fmt, w, h, bytes.len()
            );
            return;
        }
    };
    let g = gray.as_ref();

    // QR decode first (gives the decoder priority over the preview copy if
    // the consumer is slow), then the preview sink (GRAY8 — Slint
    // `make_preview_image` decodes it), then the still-capture grabber
    // (saves this frame if a capture is armed).
    scan::scan_gray(g, w, h, &|text| notify(text));
    notify_frame(g, w, h, PixelFormat::Gray8.as_i32() as u32);
    capture_grab::grab(g, w, h);
}

/// `bytes` is a `w*h` u16-le (MSB-high) RGB565 stream. Produce `w*h` Y8.
fn rgb565_to_greyscale(bytes: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let n = (w as usize) * (h as usize);
    if bytes.len() < n * 2 {
        return None;
    }
    let mut out = Vec::with_capacity(n);
    let mut ptr = 0usize;
    for _ in 0..n {
        let v = (bytes[ptr] as u32) | ((bytes[ptr + 1] as u32) << 8);
        ptr += 2;
        let r = ((v >> 11) & 0x1F) as u32 * (255 / 31);
        let g = ((v >> 5) & 0x3F) as u32 * (255 / 63);
        let b = (v & 0x1F) as u32 * (255 / 31);
        let y = (77 * r + 150 * g + 29 * b) / 256;
        out.push(y as u8);
    }
    Some(out)
}

/// The public `Camera` for Android. Same surface as Apple / desktop:
/// `new`, `set_sink`, `is_running`, `start`, `stop`.
pub struct Camera {
    attached: bool,
}

impl Default for Camera {
    fn default() -> Self {
        Self::new()
    }
}

impl Camera {
    pub fn new() -> Self {
        Camera { attached: false }
    }

    pub fn is_running(&self) -> bool {
        self.attached
    }

    pub fn set_sink<F: Fn(String) + Send + 'static>(&self, sink: F) {
        if let Some(slot) = SINK.get() {
            if let Ok(mut g) = slot.lock() {
                *g = Box::new(sink);
            }
        } else {
            let _ = SINK.set(Mutex::new(Box::new(sink)));
        }
    }

    /// Install the preview-frame sink. Called with the same raw bytes that
    /// the QR decoder consumes (GRAY8 for YUV_420_888 shells, RGB565 for
    /// legacy shells). No-op when the camera is stopped — the bridge
    /// consumer is already detached by `stop()`.
    pub fn set_frame_sink<F: Fn(&[u8], u32, u32, u32) + Send + 'static>(&self, sink: F) {
        if let Some(slot) = FRAME_SINK.get() {
            if let Ok(mut g) = slot.lock() {
                *g = Some(Box::new(sink));
            }
        } else {
            let _ = FRAME_SINK.set(Mutex::new(Some(Box::new(sink))));
        }
    }

    /// Install the QR-decoding sink into the `bridge` camera consumer slot.
    /// Idempotent.
    pub fn start(&mut self) -> Result<(), String> {
        if self.attached {
            return Ok(());
        }
        bridge::set_camera_consumer(Box::new(on_bridge_frame));
        self.attached = true;
        Ok(())
    }

    /// Install the sink used for photo-capture results. A "capture" saves the
    /// *next* preview frame as a JPEG and fires the sink with the on-disk
    /// path (or an error). Shared with the desktop implementation via
    /// [`crate::capture_grab`].
    pub fn set_capture_sink<F: Fn(crate::CaptureResult) + Send + 'static>(&self, sink: F) {
        capture_grab::set_sink(sink);
    }

    /// Trigger a still-photo capture. The next preview frame is saved and the
    /// path delivered to the sink installed via [`Camera::set_capture_sink`].
    pub fn capture(&mut self) -> Result<(), String> {
        capture_grab::arm(self.is_running())
    }

    /// Detach our QR sink (other consumers, e.g. audio, keep working).
    pub fn stop(&mut self) {
        bridge::set_camera_consumer(Box::new(|_: &[u8], _: u32, _: u32, _: u32| {}));
        self.attached = false;
    }
}
