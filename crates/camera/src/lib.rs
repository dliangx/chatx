//! Camera capture + native QR-code recognition.
//!
//! One public [`Camera`] type with the same surface on every target —
//! `new`, `set_sink`, `is_running`, `start`, `stop` — so the app drives it
//! identically no matter where it runs.
//!
//! - **macOS / iOS**: AVFoundation (`AVCaptureMetadataOutput`) + Apple's
//!   built-in QR decoder (no external library). See `apple.rs` → `macos.rs`.
//!   iOS additionally exposes [`authorization_status`] to query the camera
//!   grant state without prompting.
//! - **Windows / Linux**: [`nokhwa`](https://docs.rs/nokhwa) opens the camera
//!   and streams frames; each frame is greyscaled and handed to `zbar` for
//!   QR decode. See `desktop.rs`.
//! - **Android**: the Java shell captures via Camera1 and delivers frames
//!   through the `bridge` JNI consumer; we decode them with `zbar`. See
//!   `android.rs`.
//!
//! Anything else (e.g. wasm) gets a no-op [`Camera`] whose `start()` reports
//! "unsupported" (`fallback.rs`).

#![allow(deprecated, non_snake_case, unsafe_op_in_unsafe_fn)]

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "android"))]
pub mod scan;

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod apple;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use apple::Camera;

#[cfg(target_os = "ios")]
pub use apple::{authorization_status, CameraAuth};

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
