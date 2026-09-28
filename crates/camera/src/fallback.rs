//! Fallback for non-macOS targets: no camera/QR support.

/// A no-op camera that reports "unsupported" on start.
#[derive(Default)]
pub struct Camera {
    running: bool,
}

impl Camera {
    pub fn new() -> Self {
        Camera { running: false }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn set_sink<F: Fn(String) + Send + 'static>(&self, sink: F) {
        let _ = sink;
    }

    pub fn start(&mut self) -> Result<(), String> {
        Err("当前平台不支持摄像头扫描".to_string())
    }

    pub fn stop(&mut self) {
        self.running = false;
    }
}
