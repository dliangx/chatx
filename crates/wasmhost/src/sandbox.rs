use std::sync::{Arc, Mutex};

/// One rendered frame handed back from a plugin: raw RGBA8, straight alpha,
/// row-major, `width * height * 4` bytes.
#[derive(Debug, Clone)]
pub struct Frame {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Host → plugin input event (the plugin drains the queue via `x_input_poll`).
///
/// `kind` is one of the [`EV_*` constants]; `x`/`y` are canvas pixel
/// coordinates; `code` is an app-defined code (key code, pointer id, …).
#[derive(Debug, Clone, Copy)]
pub struct InputEvent {
    pub kind: i32,
    pub x: i32,
    pub y: i32,
    pub code: i32,
}

impl InputEvent {
    pub fn new(kind: i32, x: i32, y: i32, code: i32) -> Self {
        Self { kind, x, y, code }
    }
}

pub const EV_NONE: i32 = 0;
pub const EV_TOUCH_DOWN: i32 = 1;
pub const EV_TOUCH_MOVE: i32 = 2;
pub const EV_TOUCH_UP: i32 = 3;
pub const EV_KEY_DOWN: i32 = 4;
pub const EV_KEY_UP: i32 = 5;
pub const EV_RESIZE: i32 = 16; // `x`/`y` = new width/height in canvas px

/// Runtime-agnostic per-instance state shared between the sandbox import
/// handlers (backend side) and the host UI (chat app side).
///
/// A WAMR backend (iOS) will bind the same `Sandbox` to its own native
/// function registration — the sandbox itself never mentions wasmer.
pub struct Sandbox {
    pub app_id: String,
    pub log_sink: Arc<dyn Fn(&str) + Send + Sync>,
    pub frame: Mutex<Option<Frame>>,
    pub input: Mutex<Vec<InputEvent>>,
    /// Set by the `wasi proc_exit` handler; the runner reports it on shutdown.
    pub exited_code: Mutex<Option<u32>>,
}

impl Sandbox {
    pub fn new(app_id: impl Into<String>, log_sink: Arc<dyn Fn(&str) + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            app_id: app_id.into(),
            log_sink,
            frame: Mutex::new(None),
            input: Mutex::new(Vec::new()),
            exited_code: Mutex::new(None),
        })
    }

    /// Default sandbox whose log sink writes through the host's `tracing`
    /// pipeline (i.e. into `chatx.log`).
    pub fn with_tracing_log(app_id: impl Into<String>) -> Arc<Self> {
        let app_id = app_id.into();
        let id_for_log = app_id.clone();
        let sink: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |line| {
            tracing::info!(target: "x", plugin = %id_for_log, "{}", line.trim_end());
        });
        Self::new(app_id, sink)
    }

    pub fn log(&self, line: &str) {
        if !line.is_empty() {
            (self.log_sink)(line);
        }
    }

    pub fn push_input(&self, ev: InputEvent) {
        let mut q = self.input.lock().unwrap();
        if q.len() < 256 {
            q.push(ev);
        }
    }

    pub fn pop_input(&self) -> Option<InputEvent> {
        let mut q = self.input.lock().unwrap();
        if q.is_empty() {
            None
        } else {
            Some(q.remove(0)) // FIFO
        }
    }

    /// Store the latest committed frame, dropping the previous one.
    pub fn store_frame(&self, frame: Frame) {
        *self.frame.lock().unwrap() = Some(frame);
    }

    /// Take the latest committed frame (if any), clearing the slot.
    pub fn take_frame(&self) -> Option<Frame> {
        self.frame.lock().unwrap().take()
    }

    pub fn exited_code(&self) -> Option<u32> {
        *self.exited_code.lock().unwrap()
    }
}
