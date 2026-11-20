//! Native OS drag-and-drop receive (macOS / Windows / Linux).
//!
//! Slint's winit backend does NOT forward winit's `WindowEvent::DroppedFile`
//! to the app (its `WinitWindowEvent` match has no arm for it), and
//! Slint's `DropArea` only handles in-app `DragArea`→`DropArea` drags. So we
//! register a `CustomApplicationHandler` (via `slint::BackendSelector`) that
//! intercepts `DroppedFile` at the winit level.
//!
//! The handler runs on the winit event-loop thread, so it cannot touch the
//! Slint UI directly. It pushes the dropped path into a thread-safe queue
//! ([`take_pending`]); the UI thread drains that queue (see the 100 ms timer
//! in `lib.rs`) and sends each file (image vs. file by extension).
//!
//! [`install`] must be called BEFORE any Slint component is created
//! (i.e. before `MainWindow::new()`), because `BackendSelector::select()` is
//! the platform-install step. On mobile this is a no-op — the mobile backends
//! (android-activity / skia-ios) don't use winit.

use std::sync::Mutex;

/// Queue of dropped file paths awaiting UI-thread dispatch.
static PENDING: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Install the winit drag-and-drop handler. Must be called before any Slint
/// component is created. Idempotent (a second `select()` is a no-op once a
/// platform is active).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn install() {
    let _ = slint::BackendSelector::new()
        .with_winit_custom_application_handler(DropHandler)
        .select();
}

/// Mobile backends don't use winit — no-op.
#[cfg(any(target_os = "android", target_os = "ios"))]
pub fn install() {}

/// Take all queued dropped paths (in order) out of the queue. Empty if none.
pub fn take_pending() -> Vec<String> {
    let mut q = PENDING.lock().unwrap();
    std::mem::take(&mut *q)
}

struct DropHandler;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
impl slint::winit_030::CustomApplicationHandler for DropHandler {
    fn window_event(
        &mut self,
        _event_loop: &slint::winit_030::winit::event_loop::ActiveEventLoop,
        _window_id: slint::winit_030::winit::window::WindowId,
        _winit_window: Option<&slint::winit_030::winit::window::Window>,
        _slint_window: Option<&slint::Window>,
        event: &slint::winit_030::winit::event::WindowEvent,
    ) -> slint::winit_030::EventResult {
        if let slint::winit_030::winit::event::WindowEvent::DroppedFile(path) = event {
            let p = path.to_string_lossy().into_owned();
            tracing::warn!("[drag-drop] WINIT DroppedFile → {p}");
            PENDING.lock().unwrap().push(p);
        }
        slint::winit_030::EventResult::Propagate
    }
}
