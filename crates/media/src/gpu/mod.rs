//! GPU / hardware H.264 codec backends (per-platform).
//!
//! Each platform module in this folder implements the [`crate::H264Encoder`]
//! / [`crate::H264Decoder`] traits against that platform's native GPU video
//! pipeline, with a guaranteed [openh264] software fallback so a call always
//! works — GPU when available, CPU otherwise:
//!
//! | Platform  | Decode                          | Encode (today) |
//! |-----------|---------------------------------|----------------|
//! | **Apple** | VideoToolbox (GPU) — `apple`    | VideoToolbox   |
//! | **Android** | MediaCodec (GPU) — `android`   | MediaCodec     |
//! | **Linux** | VAAPI (GPU) — `linux`           | openh264 (sw)  |
//! | **Windows** | Media Foundation (GPU) — `windows` | openh264 (sw) |
//!
//! The shared [`nv12`] pixel-conversion helpers (RGBA ↔ NV12) live here too,
//! since every native backend needs them.
//!
//! The public *entry points* are the dispatchers [`crate::encoder::create_encoder`]
//! and [`crate::decoder::create`]; they pick the per-platform type declared
//! below (via `#[cfg]`) and fall back to [openh264] on any native failure.

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod apple;
#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(target_os = "linux")]
pub mod nv12;
