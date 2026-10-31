#![allow(deprecated, non_snake_case, unsafe_op_in_unsafe_fn)]

/// Outcome of a still-photo [`Camera::capture`]: either the saved file path or
/// an error message.
pub type CaptureResult = Result<String, String>;

pub mod album;
#[allow(dead_code)]
mod capture_grab;
pub mod filepick;
pub mod posts;

#[cfg(any(
    target_os = "windows",
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
pub mod scan;

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[path = "appleshim.rs"]
mod apple_core;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use apple_core::Camera;

#[cfg(target_os = "ios")]
#[path = "ios.rs"]
mod apple_ios;
#[cfg(target_os = "ios")]
pub use apple_ios::{CameraAuth, authorization_status};

#[cfg(any(target_os = "windows", target_os = "linux"))]
mod desktop;
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub use desktop::Camera;

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "android")]
pub use android::Camera;

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "linux",
    target_os = "android"
)))]
mod fallback;
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "linux",
    target_os = "android"
)))]
pub use fallback::Camera;

/// On non-Apple platforms there is no camera to authorise; report "granted".
#[cfg(not(target_os = "ios"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraAuth {
    Authorized,
}
#[cfg(not(target_os = "ios"))]
pub fn authorization_status() -> CameraAuth {
    CameraAuth::Authorized
}
