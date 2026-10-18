//! Windows + Linux camera capture via nokhwa, QR decoding via `scan::scan_gray`.
//!
//! Architecture:
//! - `CallbackCamera` (nokhwa's threaded output) keeps a camera open on a
//!   dedicated thread and delivers every raw frame to our `|Buffer|` callback.
//! - We ask for **YUYV** at 640×480 — universally available on Media
//!   Foundation and V4L2 webcams, and the `Y` planes of YUYV are exactly the
//!   greyscale data the QR decoder expects, so per-frame cost is
//!   one `Vec` + a stride loop (no colour-space library pulled in).
//! - A single global [`SINK`] slot (same pattern Apple uses in `macos.rs`)
//!   receives decoded payloads; the app installs its UI hop there.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::capture_grab;
use crate::scan;
use nokhwa::Buffer;
use nokhwa::threaded::CallbackCamera;
use nokhwa::utils::{
    CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};

type QrSinkFn = Box<dyn Fn(String) + Send + 'static>;
type FrameSinkFn = Box<dyn Fn(&[u8], u32, u32, u32) + Send + 'static>;

static SINK: OnceLock<Mutex<QrSinkFn>> = OnceLock::new();
/// Installed by `Camera::set_frame_sink`. Receives raw YUYV frames before
/// they pass through the QR pipeline (i.e. the full-resolution buffer,
/// not the greyscaled subset).
static FRAME_SINK: OnceLock<Mutex<Option<FrameSinkFn>>> = OnceLock::new();
static RUNNING: AtomicBool = AtomicBool::new(false);

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

fn handle_frame(buf: Buffer) {
    let res = buf.resolution();
    let w = res.width();
    let h = res.height();
    let raw = buf.buffer().to_vec();
    // YUYV is interleaved (Y0 U Y1 V). The preview sink receives the raw
    // bytes so the consumer can decode whatever it likes; for the QR
    // pipeline below we already know the format — pull the Y planes and
    // push a greyscale `w` × `h` buffer to the `FRAME_SINK` for convenience.
    let gray = yuyv_to_gray(&raw, w);

    // Maybe-save the captured still (next frame) *after* the preview has
    // been delivered, so the user sees the preview even mid-capture.
    notify_frame(&gray, w, h, 8); // PixelFormat::Gray8
    capture_grab::grab(&gray, w, h);

    scan::scan_gray(&gray, w, h, &|text| notify(text));
}

/// YUYV = `Y0 U Y1 V` (2 bytes per pixel). Return only the Y planes — the
/// greyscale format the QR decoder consumes.
fn yuyv_to_gray(data: &[u8], w: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w as usize) * (data.len() / 2));
    let mut i = 0usize;
    while i + 1 < data.len() {
        out.push(data[i]);
        i += 2;
    }
    out
}

/// The public `Camera` for Windows + Linux. Same surface as the Apple impl:
/// `new`, `set_sink`, `is_running`, `start`, `stop`.
pub struct Camera {
    stream: Option<CallbackCamera>,
}

impl Default for Camera {
    fn default() -> Self {
        Self::new()
    }
}

impl Camera {
    pub fn new() -> Self {
        Camera { stream: None }
    }

    pub fn is_running(&self) -> bool {
        self.stream.is_some()
    }

    /// Install the sink that receives decoded QR payloads. Can be called at
    /// any time; the next frame event delivers to the newest closure.
    pub fn set_sink<F: Fn(String) + Send + 'static>(&self, sink: F) {
        if let Some(slot) = SINK.get() {
            if let Ok(mut g) = slot.lock() {
                *g = Box::new(sink);
            }
        } else {
            let _ = SINK.set(Mutex::new(Box::new(sink)));
        }
    }

    /// Install the preview-frame sink. Called from the nokhwa callback
    /// thread with the greyscale Y-plane view of each YUYV frame (i.e.
    /// `fmt = 8` (Gray8), `w` × `h` bytes). No-op with a warning if the
    /// camera is not running.
    pub fn set_frame_sink<F: Fn(&[u8], u32, u32, u32) + Send + 'static>(&self, sink: F) {
        if let Some(slot) = FRAME_SINK.get() {
            if let Ok(mut g) = slot.lock() {
                *g = Some(Box::new(sink));
            }
        } else {
            let _ = FRAME_SINK.set(Mutex::new(Some(Box::new(sink))));
        }
    }

    /// Install the sink used for photo-capture results. On this target a
    /// "capture" saves the *next* preview frame as a JPEG; the sink fires
    /// with the on-disk path (or an error message).
    pub fn set_capture_sink<F: Fn(crate::CaptureResult) + Send + 'static>(&self, sink: F) {
        capture_grab::set_sink(sink);
    }

    /// Trigger a still-photo capture: the next preview frame is saved as a
    /// JPEG and the path delivered to the sink installed via
    /// [`Camera::set_capture_sink`].
    pub fn capture(&mut self) -> Result<(), String> {
        capture_grab::arm(self.is_running())
    }

    /// Open the default camera (index 0) and pump frames through the QR
    /// pipeline. Idempotent: returns `Ok(())` if already running.
    pub fn start(&mut self) -> Result<(), String> {
        if self.stream.is_some() {
            return Ok(());
        }
        let res = Resolution::new(640, 480);
        let camfmt = CameraFormat::new(res, FrameFormat::YUYV, 30);
        let requested = RequestedFormat::with_formats(
            RequestedFormatType::Closest(camfmt),
            &[FrameFormat::YUYV],
        );
        let cb = move |buf: Buffer| {
            if RUNNING.load(Ordering::SeqCst) {
                handle_frame(buf);
            }
        };
        let cam =
            CallbackCamera::new(CameraIndex::Index(0), requested, cb).map_err(|e| {
                format!("Failed to start camera: {e} (please confirm camera permission is granted)")
            })?;
        self.stream = Some(cam);
        RUNNING.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Stop the capture session; the instance can be re-used via `start()`.
    pub fn stop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
        self.stream = None;
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop();
    }
}
