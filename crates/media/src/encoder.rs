//! Pluggable H.264 video **encode** backends — the outbound (local → peer) side.
//!
//! **Asynchronous submit / poll contract.** Every backend exposes two
//! non-blocking operations driven by the single `chatx-venc` thread per
//! [`crate::media_source::VidSource`]:
//!
//!   * [`H264Encoder::submit`] — hand in one RGBA8888 picture. For a hardware
//!     backend this just *enqueues* the frame into the native encoder and
//!     returns immediately; the encoded output is produced **asynchronously**
//!     (VideoToolbox / MediaCodec run on their own worker thread) and is later
//!     collected via [`H264Encoder::poll`]. For the software backend it encodes
//!     the frame inline and buffers the result for `poll`.
//!   * [`H264Encoder::poll`] — drain one completed **Annex-B** NAL stream (SPS /
//!     PPS / IDR / P in order), if one is ready. Returns `None` when nothing has
//!     been produced yet.
//!
//! This split is the whole point: the old `encode_rgba` *blocked on the
//! encoder's per-frame output callback*, which deadlocked against the
//! asynchronous VideoToolbox worker (the "GPU feed failed → switch to software
//! every time" field bug). Never blocking on a single frame's output is what
//! lets the hardware path actually be used.
//!
//! **Frame dropping.** `submit` returns `false` (or `poll` keeps returning
//! `None`) when the backend is saturated or still warming up. The encode loop
//! treats that as *drop this frame and keep going* — acceptable for live video,
//! and it is what keeps the pipeline from ever wedging a thread.
//!
//! The wire format is **Annex-B** (start-code prefixed): webrtc-rs's
//! `H264Payloader` splits it on `00 00 01` into RFC 6184 packets, and the
//! peer's [`crate::decoder::H264DePacketizer`] reassembles the NALs — so every
//! backend MUST emit Annex-B.
//!
//! [`create_encoder`] picks the best backend:
//!
//!   * **Apple** (macOS / iOS) — **VideoToolbox** hardware (GPU) encode, a
//!     C-ABI surface compiled from `shim/encoder.swift` (see `build.rs`),
//!     wrapped in [`crate::gpu::apple::AppleH264Encoder`].
//!   * **Android** — **MediaCodec** via the JNI bridge, wrapped in
//!     [`crate::gpu::android::AndroidH264Encoder`].
//!   * **Everywhere / on failure** — pure-Rust [openh264] software encode.
//!
//! The native/GPU path is *opportunistic*: a backend that hard-fails on a
//! frame permanently switches to [openh264], so a call always encodes — GPU
//! when available, CPU otherwise. [`OpenH264Encoder`] is that universal
//! fallback and is used directly on targets with no native backend.
//!
//! [`media_source::VidSource`]: crate::media_source::VidSource

use crate::error::Error;

/// A stateful, **asynchronous** H.264 encoder: submit RGBA8888 frames, poll for
/// the Annex-B NAL stream of each completed picture.
///
/// Both operations are non-blocking. Implementations are driven by a single
/// (detached) encode thread per [`crate::media_source::VidSource`] and only
/// need to be `Send` so that object can be handed to the spawned thread.
pub trait H264Encoder: Send {
    /// Submit one RGBA8888 picture (`rgba.len() == w * h * 4`) for encoding.
    ///
    /// Returns `true` if the frame was accepted (its output will be collected
    /// later via [`Self::poll`]), or `false` if the backend could not accept
    /// it right now (queue full / not ready) — the caller then drops the frame.
    /// **This must not block** waiting for the frame to be encoded.
    ///
    /// Resolution policy (`max_w` cap, H.264 4K ceiling) is fixed at
    /// construction time and applied here by the backend; `bitrate` was likewise
    /// baked in when the encoder was created.
    fn submit(&mut self, rgba: &[u8], w: u32, h: u32) -> bool;

    /// Collect one completed **Annex-B** NAL stream (SPS / PPS / IDR / P, in
    /// order), if one is ready; `None` when nothing has been produced yet.
    ///
    /// Non-blocking. A backend emits at most one picture per call; the encode
    /// loop calls this in a `while let` to drain any backlog before submitting
    /// the next frame.
    fn poll(&mut self) -> Option<Vec<u8>>;

    /// Release backend resources (native encoder / session). Default is a
    /// no-op; called once when a track is torn down and on [`Drop`].
    fn close(&mut self) {}
}

/// Pure-Rust H.264 encoder — the historical path and the universal fallback,
/// so a call always encodes even when the native/GPU backend is unavailable.
pub struct OpenH264Encoder {
    inner: openh264::encoder::Encoder,
    /// Bounded (<=1) output queue of completed Annex-B NAL streams, filled by
    /// `submit` and drained by `poll`.
    pending: std::collections::VecDeque<Vec<u8>>,
    /// Width cap for the `max_w` downscale, fixed at construction (0 = none
    /// beyond the 4K ceiling).
    max_w: u32,
}

impl OpenH264Encoder {
    /// Build an encoder with an explicit target bitrate (`0` = the openh264
    /// default, ~120 kbps). Only resolution is taken from the first frame.
    pub fn new(bitrate_bps: u32) -> crate::error::Result<Self> {
        let inner = if bitrate_bps > 0 {
            openh264::encoder::Encoder::with_api_config(
                openh264::OpenH264API::from_source(),
                openh264::encoder::EncoderConfig::new()
                    .bitrate(openh264::encoder::BitRate::from_bps(bitrate_bps)),
            )
            .map_err(|e| Error::other(format!("openh264 encoder: {e}")))?
        } else {
            openh264::encoder::Encoder::new()
                .map_err(|e| Error::other(format!("openh264 encoder: {e}")))?
        };
        Ok(Self { inner, pending: Default::default(), max_w: 0 })
    }

    /// Reusable from the native backends' software fallback: build an
    /// [`OpenH264Encoder`] with the same `bitrate_bps` / `max_w` a caller had
    /// asked the native path to use.
    pub fn with_cfg(bitrate_bps: u32, max_w: u32) -> crate::error::Result<Self> {
        let mut e = Self::new(bitrate_bps)?;
        e.max_w = max_w;
        Ok(e)
    }

    /// Encode `rgba` (applying the width cap `max_w` + the 4K ceiling) through
    /// openh264 and return the Annex-B NAL stream. Synchronous inherent helper
    /// shared by the trait impl and by the native backends' software fallback.
    pub fn encode_rgba(&mut self, rgba: &[u8], w: u32, h: u32, max_w: u32) -> crate::error::Result<Vec<u8>> {
        // Cap + downscale before the (fixed-resolution) backend (shared helper).
        // When no downscale is needed the input is borrowed (zero-copy);
        // `rgba` is a `Cow` that keeps the (possibly owned) downscaled buffer
        // alive for the duration of this call.
        let (rgba, w, h) = prepare_rgba(rgba, w, h, max_w);
        let src = openh264::formats::RgbaSliceU8::new(&rgba, (w as usize, h as usize));
        let yuv = openh264::formats::YUVBuffer::from_rgb_source(src);
        let bs = self
            .inner
            .encode(&yuv)
            .map_err(|e| Error::other(format!("openh264 encode: {e}")))?;
        Ok(bs.to_vec())
    }
}

impl H264Encoder for OpenH264Encoder {
    fn submit(&mut self, rgba: &[u8], w: u32, h: u32) -> bool {
        if rgba.is_empty() || w == 0 || h == 0 {
            return false;
        }
        // Bounded: if the previous output hasn't been polled yet we are behind —
        // drop this frame rather than grow an unbounded queue (the "drop, don't
        // block" contract).
        if !self.pending.is_empty() {
            return false;
        }
        match self.encode_rgba(rgba, w, h, self.max_w) {
            Ok(bs) => {
                self.pending.push_back(bs);
                true
            }
            Err(e) => {
                tracing::warn!(err = %e, "openh264 encode failed");
                false
            }
        }
    }

    fn poll(&mut self) -> Option<Vec<u8>> {
        self.pending.pop_front()
    }
}

/// Build the best available encoder for this target. Never fails — see the
/// module docs. `bitrate_bps == 0` means "use the openh264 default".
pub fn create_encoder(bitrate_bps: u32, max_w: u32) -> Box<dyn H264Encoder> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        return match crate::gpu::apple::AppleH264Encoder::new(bitrate_bps, max_w) {
            Ok(d) => {
                tracing::debug!("[media] H.264 encoder: VideoToolbox (GPU)");
                Box::new(d)
            }
            Err(e) => {
                tracing::warn!("[media] VideoToolbox encode init failed ({e}) — falling back to openh264 (software)");
                Box::new(
                    OpenH264Encoder::with_cfg(bitrate_bps, max_w)
                        .expect("openh264 encoder alloc"),
                )
            }
        };
    }
    #[cfg(target_os = "android")]
    {
        // `AndroidH264Encoder::new` never hard-fails: if MediaCodec can't be
        // opened it already wires in its openh264 software fallback.
        return Box::new(
            crate::gpu::android::AndroidH264Encoder::new(bitrate_bps, max_w)
                .expect("android encoder alloc"),
        );
    }
    #[cfg(target_os = "linux")]
    {
        // Software encode (openh264) for now — see the module note in
        // `crate::gpu::linux` for why VAAPI hardware encode is deferred.
        return match crate::gpu::linux::LinuxH264Encoder::new(bitrate_bps, max_w) {
            Ok(d) => Box::new(d),
            Err(e) => {
                tracing::warn!("[media] Linux encoder init failed ({e}) — falling back to openh264 (software)");
                Box::new(
                    OpenH264Encoder::with_cfg(bitrate_bps, max_w)
                        .expect("openh264 encoder alloc"),
                )
            }
        };
    }
    #[cfg(target_os = "windows")]
    {
        // Software encode (openh264) for now — see the module note in
        // `crate::gpu::windows` for why Media Foundation hardware encode is deferred.
        return match crate::gpu::windows::WinH264Encoder::new(bitrate_bps, max_w) {
            Ok(d) => Box::new(d),
            Err(e) => {
                tracing::warn!("[media] Windows encoder init failed ({e}) — falling back to openh264 (software)");
                Box::new(
                    OpenH264Encoder::with_cfg(bitrate_bps, max_w)
                        .expect("openh264 encoder alloc"),
                )
            }
        };
    }
    // No native backend on this target (or the build disabled one).
    #[allow(unreachable_code)]
    {
        tracing::debug!("[media] H.264 encoder: openh264 (software)");
        Box::new(OpenH264Encoder::with_cfg(bitrate_bps, max_w).expect("openh264 encoder alloc"))
    }
}

/// Apply the H.264 ceiling clamp + the per-source width cap (`max_w`) and
/// downscale the result (aspect preserved). Shared by every backend so the
/// resolution handed to a (fixed-resolution) hardware encoder is always
/// in-bounds. `max_w == 0` means "no width cap" (camera path); the ceiling is
/// 3840×2160 (openh264 hard-limits there, and it keeps a 4K desktop screen
/// inside a live 1:1 relay).
///
/// Returns a tight RGBA8888 buffer (`Cow`) at the (possibly reduced) size.
/// When no downscale is needed the input is **borrowed** (zero-copy
/// `Cow::Borrowed`) — a screen frame can be ~14 MB and the only real pixel
/// work is the backend's own conversion, so cloning it here was pure waste on
/// the dominant stream. When a downscale *is* needed the new buffer is owned
/// (`Cow::Owned`) and the caller keeps it alive via the returned `Cow`.
pub(crate) fn prepare_rgba<'a>(
    rgba: &'a [u8],
    w: u32,
    h: u32,
    max_w: u32,
) -> (std::borrow::Cow<'a, [u8]>, u32, u32) {
    const H264_MAX_W: u32 = 3840;
    const H264_MAX_H: u32 = 2160;
    let cap_w = if max_w > 0 { max_w.min(H264_MAX_W) } else { H264_MAX_W };
    if w > cap_w || h > H264_MAX_H {
        let scale = (cap_w as f32 / w as f32).min(H264_MAX_H as f32 / h as f32);
        let dw = (w as f32 * scale).round().max(1.0) as u32;
        let dh = (h as f32 * scale).round().max(1.0) as u32;
        (std::borrow::Cow::Owned(downscale_rgba(rgba, w, h, dw, dh)), dw, dh)
    } else {
        (std::borrow::Cow::Borrowed(rgba), w, h)
    }
}

/// Nearest-neighbour downscale of an RGBA8888 buffer to exact `dw`x`dh`.
fn downscale_rgba(rgba: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    if dw == 0 || dh == 0 || sw == 0 || sh == 0 || rgba.is_empty() {
        return Vec::new();
    }
    let src_w = sw as usize;
    let mut out = vec![0u8; (dw as usize) * (dh as usize) * 4];
    let mut j = 0usize;
    while (j as u32) < dw {
        let sx = ((j as u32) * sw / dw).min(sw - 1) as usize;
        let mut i = 0usize;
        while (i as u32) < dh {
            let sy = ((i as u32) * sh / dh).min(sh - 1) as usize;
            let s = sy * src_w + sx;
            let d = (i * (dw as usize) + j) * 4;
            out[d] = rgba[s * 4];
            out[d + 1] = rgba[s * 4 + 1];
            out[d + 2] = rgba[s * 4 + 2];
            out[d + 3] = rgba[s * 4 + 3];
            i += 1;
        }
        j += 1;
    }
    out
}

#[cfg(test)]
#[cfg(any(target_os = "macos", target_os = "ios"))]
mod apple_enc_tests {
    use super::*;

    /// The VideoToolbox GPU encoder must emit the Annex-B wire format the
    /// peer's decoder expects: SPS (type 7) then PPS (type 8) leading the
    /// stream, followed by the IDR slice (type 5) — each start-code prefixed.
    #[test]
    fn gpu_encode_emits_sps_pps_idr() {
        let mut enc = crate::gpu::apple::AppleH264Encoder::new(2_000_000, 0).expect("gpu enc");
        let (w, h): (u32, u32) = (320, 240);
        let rgba: Vec<u8> = (0..(w * h * 4)).map(|i| i as u8).collect();
        // Async contract: submit the frame, then poll for its Annex-B output.
        // (The first picture may be empty while the encoder warms up — retry.)
        let out = loop {
            enc.submit(&rgba, w, h);
            if let Some(o) = enc.poll() {
                if !o.is_empty() {
                    break o;
                }
            }
        };
        let mut types: Vec<u8> = Vec::new();
        let mut i = 0usize;
        while i + 4 <= out.len() {
            if out[i] == 0 && out[i + 1] == 0 && out[i + 2] == 0 && out[i + 3] == 1 {
                let start = i + 4;
                let mut j = start;
                while j + 4 <= out.len()
                    && !(out[j] == 0 && out[j + 1] == 0 && out[j + 2] == 0 && out[j + 3] == 1)
                {
                    j += 1;
                }
                types.push(out[start] & 0x1F);
                i = j;
            } else {
                i += 1;
            }
        }
        assert!(!types.is_empty(), "GPU encoder produced no NALs");
        assert_eq!(types[0], 7, "first NAL must be SPS, got {types:?}");
        assert_eq!(types[1], 8, "second NAL must be PPS, got {types:?}");
        assert!(
            types.iter().any(|t| *t == 5),
            "stream must contain an IDR slice, got {types:?}"
        );
    }

    /// Regression test for the SIGSEGV in `VTEncoderSessionEmitEncodedFrame` /
    /// `FigSimpleMutexLock` that the field hit during screen-share: a
    /// `VTCompressionSessionEncodeFrame` is **asynchronous** — its output
    /// callback fires on a VideoToolbox worker thread, and may still be in
    /// flight when we drop the encoder (e.g. the moment `feed` returns a `-2`
    /// timeout and Rust switches to the software fallback).
    ///
    /// The old shim called `VTCompressionSessionInvalidate` synchronously in
    /// `close()`, while the worker was still inside `EmitEncodedFrame` —
    /// freeing the session's internal mutex out from under it (SIGSEGV).
    ///
    /// This test feeds a frame, does NOT drain the output, and immediately
    /// drops the encoder — forcing `deinit` to run while a VideoToolbox worker
    /// is (or may still be) mid-emit. If the fix regresses, this test segfaults
    /// the test process instead of returning.
    #[test]
    fn drop_while_in_flight_does_not_crash() {
        let (w, h): (u32, u32) = (320, 240);
        let rgba: Vec<u8> = (0..(w * h * 4)).map(|i| i as u8).collect();
        for _ in 0..8 {
            let mut enc = crate::gpu::apple::AppleH264Encoder::new(2_000_000, 0).expect("gpu enc");
            // Fire off an encode; do NOT drain. The output may still be
            // in flight on a VideoToolbox worker when we drop the encoder
            // below.
            let _ = enc.submit(&rgba, w, h);
            // Drop the encoder while a VideoToolbox worker may still be
            // emitting. Must not crash the process.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A solid-green 16×16 frame must encode to a non-empty Annex-B stream
    /// (i.e. at least one `00 00 00 01` start code), proving the software
    /// backend produces the wire format the payloader expects.
    #[test]
    fn software_encode_produces_annexb() {
        let mut enc = OpenH264Encoder::new(0).unwrap();
        let rgba: Vec<u8> = vec![0u8; 16 * 16 * 4];
        let mut out = Vec::new();
        // First frame is typically empty (the openh264 rate controller warms
        // up); loop submit→poll until a non-empty Annex-B NAL stream is produced.
        for _ in 0..8 {
            enc.submit(&rgba, 16, 16);
            if let Some(o) = enc.poll() {
                if !o.is_empty() {
                    out = o;
                    break;
                }
            }
        }
        assert!(!out.is_empty(), "openh264 produced no NALs over 8 frames");
        // Annex-B must start with a 3- or 4-byte zero-start-code (NAL header
        // 0x67 = SPS, 0x68 = PPS, 0x65 = IDR).
        let mut i = 0;
        while i + 4 <= out.len() {
            if out[i] == 0 && out[i + 1] == 0 && out[i + 2] == 0 && out[i + 3] == 1 {
                let nal_type = out[i + 4] & 0x1F;
                assert!(
                    nal_type == 5 || nal_type == 7 || nal_type == 8,
                    "unexpected first NAL type {nal_type:#x} (want IDR/SPS/PPS)"
                );
                break;
            }
            i += 1;
        }
    }
}
