//! iOS camera permission helpers.
//!
//! The system TCC prompt fires automatically on first
//! `AVCaptureDeviceInput` creation (handled inside the Swift shim's
//! `start()`). This module exposes a *non-prompting* status query so the UI
//! can show a helpful "open Settings" hint when access has been denied.

/// Current camera authorization state (queried without prompting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraAuth {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
}

unsafe extern "C" {
    fn chatx_camera_auth_status() -> i32;
}

/// Query the current camera authorization without triggering the system
/// prompt. The prompt itself is triggered (as needed) by `Camera::start()`.
pub fn authorization_status() -> CameraAuth {
    let raw = unsafe { chatx_camera_auth_status() };
    match raw {
        3 => CameraAuth::Authorized,
        2 => CameraAuth::Denied,
        1 => CameraAuth::Restricted,
        _ => CameraAuth::NotDetermined,
    }
}
