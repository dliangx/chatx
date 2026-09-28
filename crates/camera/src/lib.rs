//! Camera capture + native QR-code recognition.
//!
//! On macOS this uses AVFoundation (`AVCaptureMetadataOutput`) together with
//! Apple's built-in QR decoder — no external barcode library required.
//! Other platforms provide a no-op fallback that reports "unsupported".

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(target_os = "macos"))]
mod fallback;

#[cfg(target_os = "macos")]
pub use macos::Camera;
#[cfg(not(target_os = "macos"))]
pub use fallback::Camera;
