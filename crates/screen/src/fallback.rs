//! Fallback for iOS / Android: screen capture is done in the native shell
//! (ReplayKit / Broadcast extension on iOS, MediaProjection on Android) and
//! delivered through the bridge C-ABI (`screen::bridge::screen_frame_in`),
//! which funnels into the SAME consumer slot the desktop crate feeds.
//!
//! This `Screen` therefore reports "unsupported / handled natively" — the app
//! should not call `start()` on mobile; it only needs to install the sink via
//! the bridge.

/// A no-op screen handle. `start()` errors out with a platform hint.
#[derive(Default)]
pub struct Screen {
    running: bool,
}

impl Screen {
    pub fn new() -> Self {
        Screen { running: false }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn set_sink<F: Fn(Vec<u8>, u32, u32) + Send + 'static>(&self, sink: F) {
        let _ = sink;
    }

    pub fn start(&mut self) -> Result<(), String> {
        Err("此平台屏幕共享由原生采集（iOS ReplayKit / Android MediaProjection）提供".to_string())
    }

    pub fn stop(&mut self) {
        self.running = false;
    }
}
