//! Android camera capture — consumes frames from the Java shell via the
//! stable `bridge` C-ABI (JNI) and QR-decodes each frame with `rqrr`.
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

use std::sync::{Mutex, OnceLock};

use crate::scan;
use bridge::types::PixelFormat;

type QrSinkFn = Box<dyn Fn(String) + Send + 'static>;

static SINK: OnceLock<Mutex<QrSinkFn>> = OnceLock::new();

fn notify(payload: String) {
    if let Some(slot) = SINK.get() {
        if let Ok(g) = slot.lock() {
            (g)(payload);
        }
    }
}

/// Callback installed into the bridge. Runs on the JNI caller thread.
fn on_bridge_frame(bytes: &[u8], w: u32, h: u32, fmt: u32) {
    match PixelFormat::from_i32(fmt as i32) {
        PixelFormat::Gray8 => scan::scan_gray(bytes, w, h, &|text| notify(text)),
        // Legacy Camera1 (RGB_565) path. Kept so old builds keep working
        // during the Camera2 migration.
        PixelFormat::Rgb565 => {
            let Some(gray) = rgb565_to_greyscale(bytes, w, h) else {
                return;
            };
            scan::scan_gray(&gray, w, h, &|text| notify(text));
        }
        // All other formats (NV12, I420, RGBA8888...) not yet supported in
        // this build. We could add a Y-plane extractor per format here, but
        // the shell contract today is: *either* send GRAY8 with a Y-plane
        // payload, *or* keep the legacy RGB565. Adding more means more
        // per-format code on the Rust side — punt until a real need.
        _ => {}
    }
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

    /// Detach our QR sink (other consumers, e.g. audio, keep working).
    pub fn stop(&mut self) {
        bridge::set_camera_consumer(Box::new(|_: &[u8], _: u32, _: u32, _: u32| {}));
        self.attached = false;
    }
}
