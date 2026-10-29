//! `appsdk` — the plugin side of the X ABI (x/README.md).
//!
//! Everything a mini-program needs to run inside `chatx`:
//!
//! * [`begin`] / [`with_ctx`] — the drawing context (RGBA8 buffer in the
//!   plugin's own linear memory);
//! * [`Ctx::commit`] — hand a frame to the host (`x_frame_commit`);
//! * [`poll_input`] — drain host-pushed input events (`x_input_poll`);
//! * [`log`] / [`now_ms`] — diagnostics + host clock;
//! * [`draw`] — 5×7 bitmap text (P0); P1 adds `fontdue`-backed CJK text.
//!
//! The plugin must still export the three lifecycle entry points
//! `init(width, height, dpr_x10)` / `tick(dt_ms)` / `destroy()` from its own
//! crate (see `x/hello`).

pub mod ctx;
pub mod draw;

pub use ctx::{Ctx, rgba};

// ---------------------------------------------------------------------------
// Host imports (namespace `x`)
// ---------------------------------------------------------------------------

#[link(wasm_import_module = "x")]
unsafe extern "C" {
    pub fn x_log(ptr: u32, len: u32) -> u32;
    pub fn x_now_ms() -> i64;
    pub fn x_frame_commit(ptr: u32, width: u32, height: u32, stride: u32) -> u32;
    pub fn x_input_poll(kind_out: u32, x_out: u32, y_out: u32, code_out: u32) -> i32;
}

// ---------------------------------------------------------------------------
// Input events (host → plugin)
// ---------------------------------------------------------------------------

pub const EV_NONE: i32 = 0;
pub const EV_TOUCH_DOWN: i32 = 1;
pub const EV_TOUCH_MOVE: i32 = 2;
pub const EV_TOUCH_UP: i32 = 3;
pub const EV_KEY_DOWN: i32 = 4;
pub const EV_KEY_UP: i32 = 5;
pub const EV_RESIZE: i32 = 16;

#[derive(Debug, Clone, Copy)]
pub struct InputEvent {
    pub kind: i32,
    pub x: i32,
    pub y: i32,
    pub code: i32,
}

/// Drain one pending input event (host-pushed) or `None`.
pub fn poll_input() -> Option<InputEvent> {
    let (mut kind, mut x, mut y, mut code) = (0i32, 0, 0, 0);
    let got = unsafe {
        x_input_poll(
            &mut kind as *mut i32 as u32,
            &mut x as *mut i32 as u32,
            &mut y as *mut i32 as u32,
            &mut code as *mut i32 as u32,
        )
    };
    (got == 1).then_some(InputEvent { kind, x, y, code })
}

// ---------------------------------------------------------------------------
// Diagnostics / clock
// ---------------------------------------------------------------------------

/// Log a line to the host (chatx.log).
pub fn log(line: &str) {
    let b = line.as_bytes();
    if b.is_empty() {
        return;
    }
    unsafe {
        x_log(b.as_ptr() as u32, b.len() as u32);
    }
}

/// Host monotonic clock, ms.
pub fn now_ms() -> i64 {
    unsafe { x_now_ms() }
}

// ---------------------------------------------------------------------------
// Drawing context (thread-local: one plugin instance, one thread)
// ---------------------------------------------------------------------------

thread_local! {
    static CTX: std::cell::RefCell<Option<Ctx>> = const { std::cell::RefCell::new(None) };
}

/// (Re)create the drawing context. Called by the plugin from `init`.
pub fn begin(width: u32, height: u32, dpr: f32) {
    let c = Ctx::new(width, height, dpr);
    CTX.with(|s| *s.borrow_mut() = Some(c));
}

fn with_ctx_inner<R>(f: impl FnOnce(&mut Ctx) -> R) -> Option<R> {
    CTX.with(|s| s.borrow_mut().as_mut().map(f))
}

/// Run `f` with the current [`Ctx`]; `None` if `begin` was never called.
pub fn with_ctx<R>(f: impl FnOnce(&mut Ctx) -> R) -> Option<R> {
    with_ctx_inner(f)
}
