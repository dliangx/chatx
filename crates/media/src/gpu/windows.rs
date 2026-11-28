//! Windows H.264 codec backend.
//!
//! **Encode** and **decode** both use the pure-Rust [openh264] software path
//! today. This is a deliberate, safe default:
//!
//!   * The hardware path (Media Foundation H.264 decoder MFT via the
//!     `windows` crate) requires D3D11 device management and a non-trivial
//!     COM sample pipeline. It is best-effort by design, and shipping
//!     speculative COM code that cannot be compile-checked on this host would
//!     risk breaking the build for every Windows user.
//!   * openh264 is universally correct and fast enough for the resolutions we
//!     target, and it is the same backend the "Everywhere / on failure"
//!     fallback already uses on Apple / Android.
//!
//! **Future work:** wire the MF H.264 decoder MFT (`CLSID_CMSH264DecoderMFT`)
//! behind a `WinH264Decoder` that follows the exact same "opportunistic GPU,
//! guaranteed software fallback" contract as [`crate::gpu::apple::AppleH264Decoder`]
//! and [`crate::gpu::linux::VAAPIH264Decoder`]. The software fallback is already in
//! place, so dropping in the native decoder later is a pure win.
//!
//! This module exists so the `#[cfg(target_os = "windows")]` branches in
//! [`crate::encoder::create_encoder`] and [`crate::decoder::create`] have a
//! concrete named type to construct (mirroring the Apple / Android layout).

use crate::encoder::{H264Encoder, OpenH264Encoder};
use crate::{H264Decoder, OpenH264Decoder};
use crate::error::Error;

// ── H.264 **decode** — openh264 software (GPU MFT is future work) ──────────

pub struct WinH264Decoder {
    inner: OpenH264Decoder,
}

impl WinH264Decoder {
    pub fn new() -> Result<Self, String> {
        let inner = OpenH264Decoder::new().map_err(|e| e.to_string())?;
        tracing::debug!("[media] H.264 decoder: openh264 (software, Windows)");
        Ok(Self { inner })
    }
}

impl H264Decoder for WinH264Decoder {
    fn feed(&mut self, annexb: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        self.inner.feed(annexb).or_else(|| self.inner.drain())
    }

    fn drain(&mut self) -> Option<(Vec<u8>, u32, u32)> {
        self.inner.drain()
    }

    fn close(&mut self) {
        self.inner.close();
    }
}

// ── H.264 **encode** — openh264 software (GPU MFT is future work) ──────────

pub struct WinH264Encoder {
    inner: OpenH264Encoder,
}

impl WinH264Encoder {
    pub fn new(bitrate_bps: u32, max_w: u32) -> Result<Self, Error> {
        let inner = OpenH264Encoder::with_cfg(bitrate_bps, max_w)?;
        tracing::debug!("[media] H.264 encoder: openh264 (software, Windows)");
        Ok(Self { inner })
    }
}

impl H264Encoder for WinH264Encoder {
    fn submit(&mut self, rgba: &[u8], w: u32, h: u32) -> bool {
        self.inner.submit(rgba, w, h)
    }

    fn poll(&mut self) -> Option<Vec<u8>> {
        self.inner.poll()
    }

    fn close(&mut self) {
        self.inner.close();
    }
}
