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

/// View-state a viewer plugin pushes to the host (via `x_view_state`) so the
/// app UI can render its own chrome: the active page, the page count, and the
/// active view mode. `mode` is one of the `VIEW_MODE_*` codes below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewState {
    /// 1-based index of the currently visible page.
    pub page: i32,
    /// Total page count of the open document.
    pub page_count: u32,
    /// Active view mode (`VIEW_MODE_WIDTH` / `VIEW_MODE_HEIGHT`).
    pub mode: u32,
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
/// Wheel / trackpad scroll delta. `y` carries the vertical delta in canvas
/// (device) px: negative = the user scrolled down (toward later content),
/// positive = scrolled back up — the same sign convention Slint's own
/// `scroll-event` / ScrollView / TabWidget use. Mirrored in `appsdk`.
pub const EV_SCROLL: i32 = 6;
pub const EV_RESIZE: i32 = 16; // `x`/`y` = new canvas size in device px (host
                               // scales window-logical → device, same as touch)

/// View-mode codes shared by the host (command → plugin via `EV_KEY_DOWN.code`,
/// and out of the plugin via `ViewState.mode`). The plugin (in the `x/`
/// workspace) mirrors these literals — kept numeric so the two workspaces stay
/// decoupled.
pub const VIEW_MODE_WIDTH: i32 = 1; // 宽度适配 (fit width)
pub const VIEW_MODE_HEIGHT: i32 = 2; // 高度适配 (fit height)

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
    /// Latest view-state reported by a viewer plugin (page / count / mode);
    /// the app UI reads it non-destructively every pump so it always mirrors
    /// the plugin's current state.
    pub view_state: Mutex<Option<ViewState>>,
    /// Host-supplied document for this instance (P1): `None` for apps that
    /// open without a file. The plugin reads it through the `x_file_*`
    /// imports — there are no WASI pre-opens on either backend.
    pub file_name: Option<String>,
    pub file_bytes: Option<Vec<u8>>,
}

impl Sandbox {
    pub fn new(app_id: impl Into<String>, log_sink: Arc<dyn Fn(&str) + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            app_id: app_id.into(),
            log_sink,
            frame: Mutex::new(None),
            input: Mutex::new(Vec::new()),
            exited_code: Mutex::new(None),
            view_state: Mutex::new(None),
            file_name: None,
            file_bytes: None,
        })
    }

    /// Like [`Self::new`] but with one host-supplied document attached
    /// (P1 file viewers).
    pub fn with_file(
        app_id: impl Into<String>,
        log_sink: Arc<dyn Fn(&str) + Send + Sync>,
        file_name: Option<String>,
        file_bytes: Option<Vec<u8>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            app_id: app_id.into(),
            log_sink,
            frame: Mutex::new(None),
            input: Mutex::new(Vec::new()),
            exited_code: Mutex::new(None),
            view_state: Mutex::new(None),
            file_name,
            file_bytes,
        })
    }

    /// Default sandbox whose log sink writes through the host's `tracing`
    /// pipeline (i.e. into `chatx.log`).
    pub fn with_tracing_log(app_id: impl Into<String>) -> Arc<Self> {
        let app_id = app_id.into();
        Self::new(app_id.clone(), Self::tracing_log_sink(app_id))
    }

    /// The tracing log sink on its own (compose with [`Self::with_file`]).
    pub fn tracing_log_sink(app_id: impl Into<String>) -> Arc<dyn Fn(&str) + Send + Sync> {
        let id_for_log = app_id.into();
        Arc::new(move |line| {
            tracing::info!(target: "x", plugin = %id_for_log, "{}", line.trim_end());
        })
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

    /// Record a view-state report from the plugin (overwrites the previous).
    pub fn store_view_state(&self, vs: ViewState) {
        *self.view_state.lock().unwrap() = Some(vs);
    }

    /// Read the latest view-state without consuming it (the app UI pumps this
    /// every tick so it always mirrors the plugin's freshest report).
    pub fn view_state(&self) -> Option<ViewState> {
        self.view_state.lock().unwrap().clone()
    }

    pub fn exited_code(&self) -> Option<u32> {
        *self.exited_code.lock().unwrap()
    }
}
