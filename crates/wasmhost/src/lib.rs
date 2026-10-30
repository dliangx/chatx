//! `wasmhost` — ChatX "X" mini-program host runtime.
//!
//! Runs plugin WebAssembly modules (built to `wasm32-wasip1`, see the `x/`
//! workspace in this repo) inside a sandbox:
//!
//! * **one dedicated thread per live instance**, owning its wasmer
//!   `Store`/`Instance` (wasmer objects are thread-confined — that is what
//!   keeps the runtime lock-free from the plugin's point of view);
//! * **shared `Engine`** across instances (JIT artifact reuse);
//! * **imports**: the `x` namespace (canvas / input / log / clock) plus a
//!   minimal `wasi_snapshot_preview1` surface for Rust `std` (stdout →
//!   `chatx.log`, clocks, termination);
//! * **no WASI file/socket pre-opens** — all real I/O goes through host
//!   imports gated by the plugin manifest (P4).
//!
//! The backend is wasmer today; the plugin ABI (exports `init`/`tick`/`destroy`
//! + `x.*` imports) is what a future WAMR backend (iOS) must match, so all of
//! `sandbox.rs` stays backend-neutral.

mod host;
pub(crate) mod wasi;
pub mod sandbox;

pub use host::{monotonic_ms, open, open_with_file, MAX_FILE_BYTES, PluginInstance};
pub use sandbox::{Frame, InputEvent, Sandbox, ViewState};

pub const EV_NONE: i32 = sandbox::EV_NONE;
pub const EV_TOUCH_DOWN: i32 = sandbox::EV_TOUCH_DOWN;
pub const EV_TOUCH_MOVE: i32 = sandbox::EV_TOUCH_MOVE;
pub const EV_TOUCH_UP: i32 = sandbox::EV_TOUCH_UP;
pub const EV_KEY_DOWN: i32 = sandbox::EV_KEY_DOWN;
pub const EV_KEY_UP: i32 = sandbox::EV_KEY_UP;
pub const EV_SCROLL: i32 = sandbox::EV_SCROLL;
pub const EV_RESIZE: i32 = sandbox::EV_RESIZE;
pub const VIEW_MODE_WIDTH: i32 = sandbox::VIEW_MODE_WIDTH;
pub const VIEW_MODE_HEIGHT: i32 = sandbox::VIEW_MODE_HEIGHT;

/// Errors surfaced by the host runtime.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("wasm runtime error: {0}")]
    Wasm(String),

    #[error("missing plugin export: {0}")]
    MissingExport(String),

    #[error("plugin init/tick declined with code {code}")]
    PluginDeclined { code: u32 },

    #[error("plugin exited with code {code}")]
    PluginExited { code: u32 },

    #[error("plugin thread panicked")]
    JoinPanic,

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

use std::collections::HashMap;

/// Host-side registry of live + known plugin instances.
///
/// P0: one `hello` instance at a time (the UI pumps whichever instance holds
/// a fresh frame). The registry shape supports P4's multi-instance Discover
/// flow without rework.
pub struct XHost {
    instances: std::sync::RwLock<HashMap<i32, std::sync::Arc<PluginInstance>>>,
    next_id: std::sync::atomic::AtomicI32,
}

impl Default for XHost {
    fn default() -> Self {
        Self::new()
    }
}

impl XHost {
    pub fn new() -> Self {
        Self {
            instances: std::sync::RwLock::new(HashMap::new()),
            next_id: std::sync::atomic::AtomicI32::new(1),
        }
    }

    /// Live instance ids (for the UI registry map on the app side).
    pub fn instance_ids(&self) -> Vec<i32> {
        self.instances.read().unwrap().keys().copied().collect()
    }

    pub fn instance(&self, id: i32) -> Option<std::sync::Arc<PluginInstance>> {
        self.instances.read().unwrap().get(&id).cloned()
    }

    /// Load `wasm_bytes` (plugin of `app_id`) and start it at the given
    /// canvas size. Returns the instance plus its registry id.
    pub fn start(
        &self,
        app_id: &str,
        wasm_bytes: Vec<u8>,
        width: u32,
        height: u32,
        dpr_x10: u32,
    ) -> Result<(std::sync::Arc<PluginInstance>, i32)> {
        let inst = open(app_id, wasm_bytes, width, height, dpr_x10)?;
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.instances.write().unwrap().insert(id, inst.clone());
        Ok((inst, id))
    }
}
