//! Desktop screen-capture streaming on top of [`xcap`].
//!
//! - macOS       — ScreenCaptureKit / `AVCaptureScreenInput` + `CGDisplayStream`
//!   (xcap handles the entitlement / TCC prompt automatically)
//! - Windows     — Windows Graphics Capture
//! - Linux       — PipeWire / Wayland / X11 (xcap picks the backend)
//!
//! Frames are delivered to a sink installed via [`Screen::set_sink`] as
//! **RGBA8888** (`width * height * 4` bytes), which is the exact convention the
//! bridge's `sinks::screen` consumer and the mobile shells (`bridge_screen_frame_in`)
//! use. So the app registers ONE handler and both the desktop `screen` crate
//! (this file) and the native iOS/Android shunts funnel into it.
//!
//! iOS / Android do NOT compile this file — those platforms capture in the
//! native shell (ReplayKit / MediaProjection) and deliver through the bridge
//! C-ABI. See `fallback.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, OnceLock};

use xcap::{Monitor, VideoRecorder};

/// Global sink. `bytes` is a freshly-allocated RGBA8888 buffer (`len == w*h*4`);
/// `w` / `h` are physical pixels. Called on the capture pump thread.
type FrameSink = Box<dyn Fn(Vec<u8>, u32, u32) + Send + 'static>;
static SINK: OnceLock<Mutex<Option<FrameSink>>> = OnceLock::new();

fn notify(bytes: Vec<u8>, w: u32, h: u32) {
    if let Some(slot) = SINK.get() {
        if let Ok(g) = slot.lock() {
            if let Some(cb) = g.as_ref() {
                cb(bytes, w, h);
            }
        }
    }
}

/// A desktop screen-capture session.
pub struct Screen {
    running: Arc<AtomicBool>,
    recorder: Option<VideoRecorder>,
    pump: Option<std::thread::JoinHandle<()>>,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    pub fn new() -> Self {
        Screen {
            running: Arc::new(AtomicBool::new(false)),
            recorder: None,
            pump: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Install the frame sink. Can be called any time; the next frame uses the
    /// newest closure. Frames are RGBA8888.
    pub fn set_sink<F: Fn(Vec<u8>, u32, u32) + Send + 'static>(&self, sink: F) {
        if let Some(slot) = SINK.get() {
            if let Ok(mut g) = slot.lock() {
                *g = Some(Box::new(sink));
            }
        } else {
            let _ = SINK.set(Mutex::new(Some(Box::new(sink))));
        }
    }

    /// Start streaming the primary display into the installed sink.
    /// Idempotent.
    pub fn start(&mut self) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        let monitors = Monitor::all().map_err(|e| format!("列出显示器: {e}"))?;
        let Some(primary) = monitors
            .iter()
            .find(|m| m.is_primary().unwrap_or(false))
            .or_else(|| monitors.first())
        else {
            return Err("未找到任何显示器".to_string());
        };
        let (w, h) = (
            primary
                .width()
                .map_err(|e| format!("主显示器宽度: {e}"))?,
            primary
                .height()
                .map_err(|e| format!("主显示器高度: {e}"))?,
        );
        let (recorder, rx) = primary
            .video_recorder()
            .map_err(|e| format!("创建屏幕录制流: {e}"))?;

        let pump = std::thread::Builder::new()
            .name("chatx.screen".into())
            .spawn(move || pump_frames(rx))
            .map_err(|e| format!("启动屏幕采集线程: {e}"))?;

        recorder
            .start()
            .map_err(|e| format!("启动屏幕采集: {e}"))?;

        self.recorder = Some(recorder);
        self.pump = Some(pump);
        self.running.store(true, Ordering::SeqCst);
        eprintln!("[screen] capture started {w}x{h}");
        Ok(())
    }

    /// Stop the stream and the pump thread; idempotent.
    pub fn stop(&mut self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Some(r) = self.recorder.take() {
            let _ = r.stop();
        }
        if let Some(p) = self.pump.take() {
            let _ = p.join();
        }
        eprintln!("[screen] capture stopped");
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.stop();
    }
}

fn pump_frames(rx: Receiver<xcap::Frame>) {
    while let Ok(frame) = rx.recv() {
        let w = frame.width;
        let h = frame.height;
        let bytes = frame.raw;
        // The pump thread owns the buffer; hand ownership to the sink.
        // `notify` may synchronously do non-trivial work (webrtc encode) — that
        // is the consumer's problem, not the capture loop's.
        notify(bytes, w, h);
    }
    // Receiver closed → recorder stopped; exit.
}
