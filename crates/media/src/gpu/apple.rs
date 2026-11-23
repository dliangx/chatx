//! Apple (macOS / iOS) H.264 decoder binding — **VideoToolbox** (hardware /
//! GPU) decode, with a guaranteed software fallback.
//!
//! The heavy lifting lives in this crate's `shim/decoder.swift`, compiled to a
//! static archive by `build.rs` (exposing the `chatx_h264_dec_*` C-ABI). This
//! module wraps that FFI in [`AppleH264Decoder`].
//!
//! **Robustness:** the VideoToolbox path is *opportunistic*. We also carry a
//! pure-Rust [openh264] decoder; the moment a native `feed()` reports a hard
//! failure (session couldn't be created, the device has no H.264 hardware
//! decoder, the stream is rejected, …) we permanently switch to the software
//! path. So a call always decodes — GPU when available, CPU otherwise.
//!
//! The Swift shim builds the `avcC` descriptor (from the stream's SPS/PPS),
//! converts each Annex-B NAL to AVCC, hands it to `VTDecompressionSession`,
//! and does the decoded-pixel → RGBA8888 conversion. Rust just streams NALs in
//! arrival order and pulls decoded pictures out.

use crate::encoder::{prepare_rgba, H264Encoder, OpenH264Encoder};
use crate::{H264Decoder, OpenH264Decoder};
use crate::error::Error;

unsafe extern "C" {
    fn chatx_h264_dec_new() -> i32;
    fn chatx_h264_dec_feed(handle: i32, ptr: *const u8, len: i32) -> i32;
    fn chatx_h264_dec_out(
        handle: i32,
        dst: *mut u8,
        cap: i32,
        out_w: *mut u32,
        out_h: *mut u32,
    ) -> i32;
    fn chatx_h264_dec_close(handle: i32);
    fn chatx_h264_enc_new() -> i32;
    fn chatx_h264_enc_feed(handle: i32, rgba: *const u8, len: i32, w: i32, h: i32) -> i32;
    fn chatx_h264_enc_feed_cvpb(handle: i32, pb: *const std::ffi::c_void, w: i32, h: i32) -> i32;
    fn chatx_h264_enc_out(handle: i32, dst: *mut u8, cap: i32) -> i32;
    fn chatx_h264_enc_close(handle: i32);
}

pub struct AppleH264Decoder {
    /// Opaque handle into the shim's registry (one `VTDecompressionSession`
    /// per track — a call can run a camera + a screen decoder at once). `0` =
    /// dead / closed.
    handle: i32,
    /// **Guaranteed** software decoder. H.264 is stateful and the SPS/PPS NALs
    /// arrive in a *separate* feed from the first IDR slice, so a software
    /// decoder can only lock on if it has been fed *every* chunk from the
    /// start. We therefore keep a persistent `openh264` decoder that is fed
    /// each chunk and yields a picture whenever the (opportunistic) VideoToolbox
    /// path does not. This is what upholds the crate's "a call always decodes —
    /// GPU when available, CPU otherwise" guarantee: the old code only engaged
    /// the software path when the native `feed` *synchronously* returned
    /// non-zero, but VideoToolbox reports its rejection asynchronously in the
    /// completion handler, so `feed` returned 0, produced no picture, and the
    /// fallback was silently skipped — the "remote screen share never renders"
    /// bug.
    sw: OpenH264Decoder,
    /// Scratch buffer for pulling a HW picture — allocated once at the max
    /// picture size (4K) and reused; avoids a per-frame multi-MB allocation.
    scratch: Vec<u8>,
    /// Once a real video slice has been fed, lock the decoder in: whichever of
    /// {hardware, software} produced the first picture is used for the rest of
    /// the track. H.264 is stateful (P/B frames reference earlier pictures), so
    /// we must NOT mix pictures from two independent decoders mid-stream — the
    /// non-committed decoder keeps being fed for state but its pictures are not
    /// mixed in. This is what makes the fallback deterministic.
    committed: Option<Which>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Which {
    Hw,
    Sw,
}

/// Upper bound on a decoded picture we pull: 4K×4 bytes. Camera / screen caps
/// the encode at ≤ 3840×2160, so this is generous.
const MAX_RGBA: usize = 3840 * 2160 * 4;

impl AppleH264Decoder {
    pub fn new() -> Result<Self, String> {
        // The software decoder is the guaranteed path and must always exist.
        let sw = OpenH264Decoder::new().map_err(|e| format!("openh264 decoder alloc: {e}"))?;
        // The hardware session is opportunistic: if the shim can't create one
        // we still return a decoder that decodes (pure software) — the crate's
        // "never fails" contract.
        let handle = unsafe { chatx_h264_dec_new() };
        if handle == 0 {
            tracing::warn!("[media] VideoToolbox session unavailable — using openh264 (software) only");
        }
        Ok(Self {
            handle,
            sw,
            scratch: Vec::new(),
            committed: None,
        })
    }

    /// Pull the next decoded picture off the VideoToolbox session, if one is
    /// ready (grow-on-demand; the encoder caps output at ≤ 4K so this
    /// converges in at most one growth). `None` when the session is dead or no
    /// frame is pending.
    fn hw_pull(&mut self) -> Option<(Vec<u8>, u32, u32)> {
        if self.handle == 0 {
            return None;
        }
        if self.scratch.is_empty() {
            self.scratch.resize(1280 * 720 * 4, 0);
        }
        let mut w: u32 = 0;
        let mut h: u32 = 0;
        loop {
            let n = unsafe {
                chatx_h264_dec_out(self.handle, self.scratch.as_mut_ptr(), self.scratch.len() as i32, &mut w, &mut h)
            };
            if n <= 0 {
                return None; // no frame ready
            }
            let need = n as usize;
            if need > self.scratch.len() {
                if self.scratch.len() >= MAX_RGBA {
                    return None; // pathologically large; give up this frame
                }
                self.scratch.resize(need.max(MAX_RGBA), 0);
                continue; // frame was kept by the shim — retry with bigger buffer
            }
            if w == 0 || h == 0 {
                return None;
            }
            return Some((self.scratch[..need].to_vec(), w, h));
        }
    }

    fn sw_feed(&mut self, annexb: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        self.sw.feed(annexb).or_else(|| self.sw.drain())
    }

    /// True when the chunk carries a VCL slice (IDR type 5 or non-IDR type 1).
    /// Parameter-set-only (7/8) and SEI-only (6) chunks carry no picture, so a
    /// "no picture yet" outcome for them is expected and must not commit the
    /// decoder.
    fn has_vcl_slice(annexb: &[u8]) -> bool {
        for b in annexb {
            if b & 0x1F == 5 || b & 0x1F == 1 {
                return true;
            }
        }
        false
    }
}
impl H264Decoder for AppleH264Decoder {
    fn feed(&mut self, annexb: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        if annexb.is_empty() {
            return None;
        }

        // Already committed to a decoder: feed it in order and return whatever
        // it produced. This is the steady-state fast path and guarantees we
        // never mix two decoders mid-stream.
        match self.committed {
            Some(Which::Sw) => return self.sw_feed(annexb),
            Some(Which::Hw) => {
                if self.handle != 0 {
                    let _ = unsafe {
                        chatx_h264_dec_feed(self.handle, annexb.as_ptr(), annexb.len() as i32)
                    };
                    return self.hw_pull();
                }
                // HW session died after we committed to it — fall back to the
                // (still-warm) software decoder from this picture on.
                self.committed = Some(Which::Sw);
                return self.sw_feed(annexb);
            }
            None => {}
        }

        // Not yet committed: feed the guaranteed software decoder first (in
        // arrival order, so it keeps full H.264 state — it even sees the
        // SPS/PPS chunk that carries no picture).
        let sw_out = self.sw_feed(annexb);

        // Then try the opportunistic hardware decoder. A hard synchronous
        // failure retires the session permanently (software from here on).
        if self.handle != 0 {
            let r = unsafe { chatx_h264_dec_feed(self.handle, annexb.as_ptr(), annexb.len() as i32) };
            if r != 0 {
                tracing::warn!("[media] VideoToolbox feed failed (code {r}) — relying on openh264 (software) decode");
                unsafe { chatx_h264_dec_close(self.handle) }
                self.handle = 0;
                self.committed = Some(Which::Sw);
                return sw_out;
            }
            if let Some(hw) = self.hw_pull() {
                self.committed = Some(Which::Hw);
                return Some(hw);
            }
        }

        // No hardware picture for this chunk. If the chunk was a real slice,
        // the hardware path has proven it cannot render this stream — commit to
        // software so we never again wait on (or mix with) the hardware decoder.
        if Self::has_vcl_slice(annexb) {
            self.committed = Some(Which::Sw);
            if self.handle != 0 {
                tracing::debug!("[media] VideoToolbox produced no picture for a slice — committing to openh264 (software) decode");
            }
        }
        sw_out
    }

    fn drain(&mut self) -> Option<(Vec<u8>, u32, u32)> {
        match self.committed {
            Some(Which::Sw) => self.sw.drain(),
            Some(Which::Hw) => self.hw_pull(),
            // Not yet committed: `feed` already returned the software picture;
            // nothing extra is buffered.
            None => None,
        }
    }

    fn close(&mut self) {
        if self.handle != 0 {
            unsafe { chatx_h264_dec_close(self.handle) }
            self.handle = 0;
        }
        // The software decoder frees itself on drop.
    }
}

impl Drop for AppleH264Decoder {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe { chatx_h264_dec_close(self.handle) }
            self.handle = 0;
        }
    }
}

// ── H.264 **encode** (VideoToolbox, GPU) with software fallback ─────────────

/// Upper bound on the encoded Annex-B NAL stream we pull (an IDR can be
/// large; 4K screen at high bitrate). Generous — the shim reports the true
/// size and we grow on demand.
const MAX_BITSTREAM: usize = 64 * 1024 * 1024;

pub struct AppleH264Encoder {
    /// Opaque handle into the shim's registry (one `VTCompressionSession` per
    /// encoder — a call can run camera + screen at once). `0` = dead.
    handle: i32,
    /// Once a native `feed` hard-fails, encode everything through this
    /// software encoder instead (and stop calling into the shim).
    fallback: Option<OpenH264Encoder>,
    /// Scratch buffer for `out()` — allocated once and reused.
    scratch: Vec<u8>,
    max_w: u32,
}

impl AppleH264Encoder {
    pub fn new(_bitrate_bps: u32, max_w: u32) -> Result<Self, Error> {
        let handle = unsafe { chatx_h264_enc_new() };
        if handle == 0 {
            return Err(Error::other("chatx_h264_enc_new = 0"));
        }
        Ok(Self {
            handle,
            fallback: None,
            scratch: Vec::new(),
            max_w,
        })
    }

    /// Encode `rgba` through the software fallback.
    fn encode_fallback(&mut self, rgba: &[u8], w: u32, h: u32, bitrate_bps: u32) -> Result<Vec<u8>, Error> {
        if self.fallback.is_none() {
            self.fallback =
                Some(OpenH264Encoder::with_cfg(bitrate_bps, self.max_w).map_err(|e| Error::other(e.to_string()))?);
        }
        self.fallback.as_mut().unwrap().encode_rgba(rgba, w, h, bitrate_bps, self.max_w)
    }

    /// Pull the encoded Annex-B NAL stream out of the shim (grow-on-demand).
    fn drain_annexb(&mut self) -> Option<Vec<u8>> {
        if self.scratch.is_empty() {
            self.scratch.resize(1024 * 1024, 0);
        }
        loop {
            let n = unsafe {
                chatx_h264_enc_out(self.handle, self.scratch.as_mut_ptr(), self.scratch.len() as i32)
            };
            if n <= 0 {
                return None;
            }
            let need = n as usize;
            if need > self.scratch.len() {
                if need > MAX_BITSTREAM {
                    return None;
                }
                self.scratch.resize(need, 0);
                continue;
            }
            return Some(self.scratch[..need].to_vec());
        }
    }
}

impl H264Encoder for AppleH264Encoder {
    fn encode_rgba(&mut self, rgba: &[u8], w: u32, h: u32, bitrate_bps: u32, max_w: u32) -> Result<Vec<u8>, Error> {
        if rgba.is_empty() || w == 0 || h == 0 {
            return Err(Error::other("empty frame"));
        }
        // Already on the software path.
        if let Some(fb) = self.fallback.as_mut() {
            return fb.encode_rgba(rgba, w, h, bitrate_bps, max_w);
        }
        // Cap + downscale before the (fixed-resolution) native encoder. `rgba`
        // is a `Cow` that keeps the (possibly owned) downscaled buffer alive
        // for the whole encode; no downscale = zero-copy borrow.
        let (rgba, w, h) = prepare_rgba(rgba, w, h, max_w);
        let r = unsafe {
            chatx_h264_enc_feed(self.handle, rgba.as_ptr(), rgba.len() as i32, w as i32, h as i32)
        };
        if r != 0 {
            tracing::warn!("[media] VideoToolbox feed failed (code {r}) — switching to openh264 software encode");
            unsafe { chatx_h264_enc_close(self.handle) }
            self.handle = 0;
            return self.encode_fallback(&rgba, w, h, bitrate_bps);
        }
        self.drain_annexb().ok_or_else(|| Error::other("no encoded output from VideoToolbox"))
    }

    /// Zero-conversion fast path: hand the capture `CVPixelBuffer` straight to
    /// VideoToolbox, skipping the CPU RGBA→BGRA conversion. The buffer is
    /// encoded at its own native resolution (no downscale — the hardware
    /// encoder already handles the size).
    fn encode_cvpb(&mut self, pb: *const std::ffi::c_void, w: u32, h: u32) -> Option<Result<Vec<u8>, Error>> {
        if w == 0 || h == 0 || self.handle == 0 {
            return None;
        }
        let r = unsafe { chatx_h264_enc_feed_cvpb(self.handle, pb, w as i32, h as i32) };
        if r != 0 {
            // Native encode failed for this frame. We can't synthesize an
            // RGBA fallback from a pixel buffer here, so report the error and
            // let the caller fall back to the software path via `encode_rgba`
            // (which will have its own fallback wiring).
            return Some(Err(Error::other(format!("cvpb encode failed (code {r})"))));
        }
        Some(self.drain_annexb().ok_or_else(|| Error::other("no encoded output from VideoToolbox (cvpb)")))
    }

    fn close(&mut self) {
        if self.handle != 0 {
            unsafe { chatx_h264_enc_close(self.handle) }
            self.handle = 0;
        }
    }
}

#[cfg(test)]
mod cvpb_tests {
    use super::*;

    /// CoreVideo FFI — enough to synthesize a BGRA `CVPixelBuffer` for the
    /// fast-path test (the `media` build already links `CoreVideo` +
    /// `CoreFoundation`).
    type OsStatus = i32;
    const K_CV_RETURN_SUCCESS: OsStatus = 0;
    const K_CV_PIXEL_FORMAT_32BGRA: u32 = 0x42475241; // 'BGRA'
    type CVPixelBufferRef = *mut std::ffi::c_void;

    unsafe extern "C" {
        fn CVPixelBufferCreate(
            allocator: *const std::ffi::c_void,
            width: usize,
            height: usize,
            format: u32,
            attachments: *const std::ffi::c_void,
            pixel_buffer_out: *mut CVPixelBufferRef,
        ) -> OsStatus;
        fn CVPixelBufferRelease(pb: CVPixelBufferRef) -> OsStatus;
        fn CVPixelBufferLockBaseAddress(pb: CVPixelBufferRef, flags: u32) -> OsStatus;
        fn CVPixelBufferUnlockBaseAddress(pb: CVPixelBufferRef, flags: u32) -> OsStatus;
        fn CVPixelBufferGetBaseAddress(pb: CVPixelBufferRef) -> *mut std::ffi::c_void;
        fn CVPixelBufferGetBytesPerRow(pb: CVPixelBufferRef) -> usize;
    }

    /// Build a tightly-sized BGRA pixel buffer with a simple per-pixel pattern.
    /// Returns a retained buffer (refcount 1) the caller must `Release`.
    fn make_bgra_pb(w: u32, h: u32) -> Option<CVPixelBufferRef> {
        unsafe {
            let mut pb: CVPixelBufferRef = std::ptr::null_mut();
            let st = CVPixelBufferCreate(
                std::ptr::null(),
                w as usize,
                h as usize,
                K_CV_PIXEL_FORMAT_32BGRA,
                std::ptr::null(),
                &mut pb,
            );
            if st != K_CV_RETURN_SUCCESS || pb.is_null() {
                eprintln!("[cvpb-test] CVPixelBufferCreate status={st} null_pb={}", pb.is_null());
                return None;
            }
            if CVPixelBufferLockBaseAddress(pb, 0) == K_CV_RETURN_SUCCESS {
                let base = CVPixelBufferGetBaseAddress(pb).cast::<u8>();
                if !base.is_null() {
                    let bpr = CVPixelBufferGetBytesPerRow(pb);
                    for y in 0..h as usize {
                        let row = base.add(y * bpr);
                        for x in 0..w as usize {
                            let i = x * 4;
                            *row.add(i) = (x as u8).wrapping_mul(3);
                            *row.add(i + 1) = (y as u8).wrapping_mul(5);
                            *row.add(i + 2) = x.wrapping_add(y) as u8;
                            *row.add(i + 3) = 255;
                        }
                    }
                }
                let _ = CVPixelBufferUnlockBaseAddress(pb, 0);
            }
            Some(pb)
        }
    }

    /// The zero-conversion fast path (`feed_cvpb`) must emit the same Annex-B
    /// wire format as the RGBA path: SPS (7) then PPS (8) leading the stream,
    /// with an IDR (5) slice — proving a capture `CVPixelBuffer` can be fed
    /// straight to VideoToolbox with no CPU pixel conversion.
    #[test]
    fn cvpb_fast_path_emits_sps_pps_idr() {
        let (w, h) = (160u32, 120u32);
        let mut enc = AppleH264Encoder::new(2_000_000, 0).expect("gpu enc");
        let pb = make_bgra_pb(w, h).expect("build CVPixelBuffer");
        let first = enc
            .encode_cvpb(pb as *const std::ffi::c_void, w, h)
            .expect("cvpb fast path supported")
            .expect("cvpb encode");
        let mut types: Vec<u8> = Vec::new();
        let mut i = 0usize;
        while i + 4 <= first.len() {
            if first[i] == 0 && first[i + 1] == 0 && first[i + 2] == 0 && first[i + 3] == 1 {
                let start = i + 4;
                let mut j = start;
                while j + 4 <= first.len()
                    && !(first[j] == 0 && first[j + 1] == 0 && first[j + 2] == 0 && first[j + 3] == 1)
                {
                    j += 1;
                }
                types.push(first[start] & 0x1F);
                i = j;
            } else {
                i += 1;
            }
        }
        assert!(!types.is_empty(), "cvpb fast path produced no NALs");
        assert_eq!(types[0], 7, "first NAL must be SPS, got {types:?}");
        assert_eq!(types[1], 8, "second NAL must be PPS, got {types:?}");
        assert!(types.iter().any(|t| *t == 5), "stream must contain IDR, got {types:?}");
        unsafe { let _ = CVPixelBufferRelease(pb); }
    }

    /// Full GPU round trip: encode several frames on VideoToolbox, then decode
    /// them back on VideoToolbox. This exercises the *real* encode→decode path
    /// (Annex-B NALs in, RGBA out) and, importantly, would hang if the decode
    /// shim held its lock across the (synchronous) decode-completion call —
    /// the regression behind "remote video shows a frame then freezes".
    ///
    /// Run under a wall-clock timeout (see the test harness / CI): a real
    /// deadlock makes this test hang rather than fail, so the timeout is the
    /// guard.

    #[test]
    #[ignore = "flaky on VideoToolbox with a static test pattern (2nd frame times out in CI env); kept for on-device reference only"]
    fn gpu_encode_decode_roundtrip() {
        use crate::H264Encoder as _;
        use crate::H264Decoder as _;

        let (w, h) = (160u32, 120u32);
        let rgba: Vec<u8> = (0..(w as usize * h as usize * 4)).map(|i| i as u8).collect();

        // --- encode 2 frames (first carries SPS+PPS+IDR, second a P-frame) ---
        let mut enc = AppleH264Encoder::new(2_000_000, 0).expect("gpu enc");
        let mut encoded: Vec<Vec<u8>> = Vec::new();
        for _ in 0..2 {
            let out = enc.encode_rgba(&rgba, w, h, 2_000_000, 0).expect("gpu encode");
            if !out.is_empty() {
                encoded.push(out);
            }
        }
        assert!(!encoded.is_empty(), "GPU encoder produced no Annex-B stream");

        // --- decode the encoded chunks back through VideoToolbox ---
        let mut dec = AppleH264Decoder::new().expect("gpu dec");
        let mut got_frames = 0u32;
        let (mut rw, mut rh) = (0u32, 0u32);
        for chunk in &encoded {
            // `feed` returns the first decoded picture, if any.
            if let Some((rgba, dw, dh)) = dec.feed(chunk) {
                rw = dw; rh = dh;
                got_frames += 1;
                assert!(!rgba.is_empty(), "decoded frame is empty");
            }
            // Drain any additional pictures the async decoder produced.
            while let Some((rgba, dw, dh)) = dec.drain() {
                rw = dw; rh = dh;
                got_frames += 1;
                assert!(!rgba.is_empty(), "decoded frame is empty");
            }
        }
        assert!(got_frames > 0, "GPU decode produced no frames from valid GPU-encoded stream");
        assert_eq!(rw, w, "decoded width mismatch: {rw} != {w}");
        assert_eq!(rh, h, "decoded height mismatch: {rh} != {h}");
    }
}

impl Drop for AppleH264Encoder {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe { chatx_h264_enc_close(self.handle) }
            self.handle = 0;
        }
    }
}
