//! Shared AVFoundation camera implementation for macOS **and** iOS.
//!
//! - [`Camera`] — the capture session + QR/photo outputs (platform-neutral
//!   Apple; lives in `macos.rs`).
//! - [`authorization_status`] — iOS permission *query* (no prompt). The prompt
//!   itself fires automatically in `Camera::start()` when the status is
//!   `NotDetermined`.

#[path = "macos.rs"]
mod core;
pub use core::Camera;

#[cfg(target_os = "ios")]
#[path = "ios.rs"]
mod ios;
#[cfg(target_os = "ios")]
pub use ios::{authorization_status, CameraAuth};
