//! Android H.264 decoder — **MediaCodec** (hardware / GPU) decode, with a
//! guaranteed software fallback.
//!
//! The `MediaCodec` and its YUV→RGBA conversion live in the Java shell
//! (`com.chatx.MediaCodecDecoder`, invoked via the `bridge` JNI surface
//! `bridge::h264_*`). This module wraps that in [`AndroidH264Decoder`],
//! carrying a pure-Rust [openh264] fallback: the moment a native call fails
//! (MediaCodec unavailable, no H.264 decoder, stream rejected, …) we
//! permanently switch to software decode. So a call always decodes — GPU when
//! available, CPU otherwise.
//!
//! The Rust side still owns the Annex-B → NAL framing contract: each NAL chunk
//! is fed whole to MediaCodec, which handles the codec state (SPS/PPS/IDR,
//! P/B references) internally.

use super::encoder::{prepare_rgba, H264Encoder, OpenH264Encoder};
use super::{H264Decoder, OpenH264Decoder};
use crate::error::Error;

/// Upper bound on a decoded picture we pull (4K). The encoder caps output at
/// ≤ 3840×2160, so this is generous and the scratch buffer is allocated once.
const MAX_RGBA: usize = 3840 * 2160 * 4;

pub struct AndroidH264Decoder {
    fallback: Option<OpenH264Decoder>,
    /// Native path is dead (a call failed) — decode through `fallback`.
    native_dead: bool,
    scratch: Vec<u8>,
}

impl AndroidH264Decoder {
    /// Ask the Java shell to open its MediaCodec decoder. If the JVM isn't
    /// ready or the call fails, we fall back to software immediately.
    pub fn new() -> Result<Self, String> {
        let ok = bridge::h264_init();
        if !ok {
            tracing::warn!("[media] Android MediaCodec init failed — using openh264 software decode");
        }
        Ok(Self {
            fallback: if !ok {
                Some(OpenH264Decoder::new().expect("openh264 fallback alloc"))
            } else {
                None
            },
            native_dead: !ok,
            scratch: Vec::new(),
        })
    }

    /// Switch to the software decoder and decode `annexb` through it.
    fn fall_back(&mut self, annexb: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        if self.fallback.is_none() {
            self.fallback = Some(OpenH264Decoder::new().expect("openh264 fallback alloc"));
        }
        self.native_dead = true;
        let fb = self.fallback.as_mut().unwrap();
        fb.feed(annexb).or_else(|| fb.drain())
    }
}

impl H264Decoder for AndroidH264Decoder {
    fn feed(&mut self, annexb: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        if annexb.is_empty() {
            return None;
        }
        if self.native_dead {
            let fb = self.fallback.as_mut().unwrap();
            return fb.feed(annexb).or_else(|| fb.drain());
        }
        if !bridge::h264_feed(annexb) {
            tracing::warn!("[media] Android MediaCodec feed failed — switching to openh264 software decode");
            return self.fall_back(annexb);
        }
        self.drain()
    }

    fn drain(&mut self) -> Option<(Vec<u8>, u32, u32)> {
        if self.native_dead {
            return self.fallback.as_mut().unwrap().drain();
        }
        if self.scratch.len() < MAX_RGBA {
            self.scratch.resize(MAX_RGBA, 0);
        }
        let mut dims: [u32; 2] = [0; 2];
        let n = match bridge::h264_out(self.scratch.as_mut_slice(), &mut dims) {
            Some(n) => n,
            None => return None,
        };
        if dims[0] == 0 || dims[1] == 0 {
            return None;
        }
        Some((self.scratch[..n].to_vec(), dims[0], dims[1]))
    }

    fn close(&mut self) {
        bridge::h264_close();
        // The software fallback (if present) frees itself on drop.
    }
}

// ── H.264 **encode** (MediaCodec, GPU) with software fallback ──────────────

pub struct AndroidH264Encoder {
    /// Opaque handle into the Java `MediaCodecEncoder` registry. `0` = dead.
    handle: i32,
    /// Once a native `feed` fails, encode everything through this software
    /// encoder instead (and stop calling into the Java side).
    fallback: Option<OpenH264Encoder>,
    /// Scratch buffer for `out()` — allocated once and reused.
    scratch: Vec<u8>,
    max_w: u32,
}

impl AndroidH264Encoder {
    pub fn new(_bitrate_bps: u32, max_w: u32) -> Result<Self, Error> {
        let handle = bridge::h264enc_new();
        if handle == 0 {
            return Err(Error::other("MediaCodec encoder unavailable (handle 0)"));
        }
        Ok(Self {
            handle,
            fallback: None,
            scratch: Vec::new(),
            max_w,
        })
    }

    fn encode_fallback(&mut self, rgba: &[u8], w: u32, h: u32, bitrate_bps: u32) -> crate::error::Result<Vec<u8>> {
        if self.fallback.is_none() {
            self.fallback = Some(
                OpenH264Encoder::with_cfg(bitrate_bps, self.max_w)
                    .map_err(|e| Error::other(e.to_string()))?,
            );
        }
        self.fallback.as_mut().unwrap().encode_rgba(rgba, w, h, bitrate_bps, self.max_w)
    }

    fn drain_annexb(&mut self) -> Option<Vec<u8>> {
        // Pre-allocate a generous scratch buffer (like the decode side does
        // with MAX_RGBA) so the Java `out()` — which copies `min(n, dst.len)`
        // and then consumes the frame — never truncates a real IDR.
        if self.scratch.len() < 8 * 1024 * 1024 {
            self.scratch.resize(8 * 1024 * 1024, 0);
        }
        let n = bridge::h264enc_out(self.handle, self.scratch.as_mut_slice())?;
        Some(self.scratch[..n].to_vec())
    }
}

impl H264Encoder for AndroidH264Encoder {
    fn encode_rgba(&mut self, rgba: &[u8], w: u32, h: u32, bitrate_bps: u32, max_w: u32) -> crate::error::Result<Vec<u8>> {
        if rgba.is_empty() || w == 0 || h == 0 {
            return Err(Error::other("empty frame"));
        }
        if let Some(fb) = self.fallback.as_mut() {
            return fb.encode_rgba(rgba, w, h, bitrate_bps, max_w);
        }
        let (rgba, w, h) = prepare_rgba(rgba, w, h, max_w);
        if !bridge::h264enc_feed(self.handle, &rgba, w, h) {
            tracing::warn!("[media] MediaCodec feed failed — switching to openh264 software encode");
            bridge::h264enc_close(self.handle);
            self.handle = 0;
            return self.encode_fallback(&rgba, w, h, bitrate_bps);
        }
        self.drain_annexb().ok_or_else(|| Error::other("no encoded output from MediaCodec"))
    }

    fn close(&mut self) {
        if self.handle != 0 {
            bridge::h264enc_close(self.handle);
            self.handle = 0;
        }
    }
}

impl Drop for AndroidH264Encoder {
    fn drop(&mut self) {
        if self.handle != 0 {
            bridge::h264enc_close(self.handle);
            self.handle = 0;
        }
    }
}
