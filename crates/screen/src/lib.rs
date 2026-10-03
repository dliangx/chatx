//! Screen capture / sharing for chatx.
//!
//! One public [`Screen`] type across every target. On desktop (macOS, Windows,
//! Linux) it streams the primary display to a sink via [`xcap`]. On iOS /
//! Android it is a no-op handle — those platforms capture in the native shell
//! (ReplayKit / MediaProjection) and funnel frames through the bridge C-ABI,
//! which lands in the same consumer slot this crate feeds. So the application
//! registers ONE screen handler either way.

#![allow(deprecated)]

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
mod desktop;
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
pub use desktop::Screen;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod fallback;
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub use fallback::Screen;
