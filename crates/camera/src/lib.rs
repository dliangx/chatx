//! Camera capture + native QR-code recognition.
//!
//! On macOS and iOS this uses AVFoundation (`AVCaptureMetadataOutput`) together
//! with Apple's built-in QR decoder — no external barcode library required.
//! The implementation lives in `macos.rs` (the filename predates iOS support;
//! the code is platform-neutral Apple). iOS additionally exposes
//! [`authorization_status`] to query (without prompting) the camera grant
//! state.
//!
//! Other platforms provide a no-op fallback that reports "unsupported".

#![allow(deprecated, non_snake_case, unsafe_op_in_unsafe_fn)]

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod apple;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use apple::Camera;

#[cfg(target_os = "ios")]
pub use apple::{authorization_status, CameraAuth};

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
mod fallback;
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
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
