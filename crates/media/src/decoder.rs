//! Pluggable H.264 video **decode** backends.
//!
//! Every platform ends up here: the RTP read loop (`session.rs`) reassembles
//! raw payloads into Annex-B NAL units and feeds them to a single long-lived
//! [`H264Decoder`]. The *backend* is chosen once per track by [`create`]:
//!
//!   * **Apple** (macOS / iOS) — **VideoToolbox** hardware decode, exposed as
//!     a C-ABI surface compiled from `shim/decoder.swift` (see `build.rs`).
//!     This is the GPU path the task targets.
//!   * **Android** — **MediaCodec** via the JNI bridge (the Java shell owns
//!     the `MediaCodec`; see `bridge` / `apps/platform/android`).
//!   * **Everywhere / on failure** — pure-Rust [openh264] software decode.
//!
//! [`create`] always returns *something* that can decode: it never panics and
//! silently falls back to [openh264] if the native backend cannot be
//! instantiated (e.g. a host without the shim linked, a device whose
//! MediaCodec rejects the stream).
//!
//! **One decoder per track.** H.264 P/B frames depend on reference-frame
//! state, so the decoder is *stateful* and must outlive any single frame —
//! the same constraint the openh264 path already had.
//!
//! # Output contract
//!
//! [`H264Decoder::feed`] + [`H264Decoder::drain`] together yield zero or more
//! decoded pictures, each as a tightly-packed **RGBA8888** buffer
//! (`len == w * h * 4`). `feed` returns the *first* picture it produced (if
//! any); `drain` pops subsequent ones. Backends whose hardware pipeline is
//! strictly one-in-one-out leave `drain` at its `None` default (openh264,
//! MediaCodec); backends whose decoder emits asynchronously (VideoToolbox,
//! whose `outputCallback` can fire a picture *later* than the `feed` that
//! produced it) override `drain` to flush the queue.

/// A stateful H.264 decoder: feed Annex-B NALs in arrival order, read back
/// decoded RGBA8888 pictures.
///
/// Implementations are single-threaded (driven by one webrtc read loop per
/// track) but must be `Send` because the consumer task is spawned onto the
/// runtime thread pool.
pub trait H264Decoder: Send {
    /// Feed one Annex-B NAL unit (may carry SPS/PPS, an IDR slice, or P/B
    /// slices). Returns the first decoded picture, if this call produced one
    /// (parameter-only NALs return `None`, which is normal).
    fn feed(&mut self, annexb: &[u8]) -> Option<(Vec<u8>, u32, u32)>;

    /// Pop a picture that was produced but not yet returned. Backends with a
    /// strictly synchronous pipeline leave this at the default (`None`).
    fn drain(&mut self) -> Option<(Vec<u8>, u32, u32)> {
        None
    }

    /// Release any backend resources (native decoder / session). Default is a
    /// no-op; called once from [`Drop`] and when a track is torn down.
    fn close(&mut self) {}
}

/// Build the best available decoder for this target. Never fails — see the
/// module docs.
pub fn create() -> Box<dyn H264Decoder> {
    // Diagnostic / hard override: force the pure-Rust software decoder. Used
    // to isolate a broken native (VideoToolbox / MediaCodec) path from the
    // rest of the receive pipeline (depacketizer / routing / decode).
    if std::env::var("CHATX_FORCE_SW_DEC").ok().as_deref() == Some("1") {
        tracing::debug!("[media] H.264 decoder: openh264 (software, forced by env)");
        return Box::new(OpenH264Decoder::new().expect("openh264 decoder alloc"));
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        return match crate::apple::AppleH264Decoder::new() {
            Ok(d) => {
                tracing::debug!("[media] H.264 decoder: VideoToolbox (GPU)");
                Box::new(d)
            }
            Err(e) => {
                tracing::warn!("[media] VideoToolbox init failed ({e}) — falling back to openh264 (software)");
                Box::new(OpenH264Decoder::new().expect("openh264 decoder alloc"))
            }
        };
    }
    #[cfg(target_os = "android")]
    {
        // `AndroidH264Decoder::new` never hard-fails: if the MediaCodec can't
        // be opened it already wires in its openh264 software fallback.
        return Box::new(crate::android::AndroidH264Decoder::new().expect("android decoder alloc"));
    }
    // No native backend on this target (or the build disabled one).
    #[allow(unreachable_code)]
    {
        tracing::debug!("[media] H.264 decoder: openh264 (software)");
        Box::new(OpenH264Decoder::new().expect("openh264 decoder alloc"))
    }
}

// ── openh264 software fallback ─────────────────────────────────────────────

/// Pure-Rust H.264 decoder (the historical path). Kept as the universal
/// fallback so a call always works, even when the native/GPU backend is
/// unavailable.
pub struct OpenH264Decoder {
    inner: openh264::decoder::Decoder,
}

impl OpenH264Decoder {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            inner: openh264::decoder::Decoder::new().map_err(|e| e.to_string())?,
        })
    }
}

impl H264Decoder for OpenH264Decoder {
    fn feed(&mut self, annexb: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        let mut last = None;
        for nal in openh264::nal_units(annexb) {
            if let Ok(Some(yuv)) = self.inner.decode(nal) {
                let (uw, uh) = yuv.dimensions_uv();
                let (w, h) = (uw * 2, uh * 2);
                let mut rgba = vec![0u8; w * h * 4];
                yuv.write_rgba8(&mut rgba);
                last = Some((rgba, w as u32, h as u32));
            }
        }
        last
    }
}

/// Decode a single Annex-B NAL with a given decoder, draining any extra
/// pictures it buffered. Shared by [`session`] so the per-track loop stays
/// tiny.
pub fn decode_one(dec: &mut dyn H264Decoder, annexb: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    dec.feed(annexb).or_else(|| dec.drain())
}
