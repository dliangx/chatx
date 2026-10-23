//! Local media sources (outbound tracks).
//!
//! In webrtc-rs 0.17 a track *is* the source — there is no separate
//! `MediaSource` trait. `AudSource` / `VidSource` each wrap a
//! `TrackLocalStaticSample`: they feed `Sample`s into the track.
//!
//! Callers push raw frames (s16le PCM / RGBA8) from the `audio` / `camera` /
//! `screen` sink callbacks.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Connection gate. Set to `true` once the `PeerConnection` reaches the
/// `Connected` state (in [`crate::session`]); reset to `false` on any
/// `Failed` / `Disconnected` / `Hangup` / `close` event.
///
/// Local media encoders (audio `push`, video `encode_loop`) check this before
/// doing the (relatively) expensive encode + `write_sample` into the webrtc
/// track. Reason: before the peer answers, every encoded / packetized frame
/// just accumulates in webrtc-rs's send pipeline (or, worse, with
/// `has_sent()==false` keeps the previous `VidSource` track binding alive).
/// Gating means:
///   * before connect: audio/video frames are dropped locally (the UI still
///     previews via `install_local_cam_preview`, which is a different
///     sink) — no CPU, no webrtc send-queue growth;
///   * after connect: normal send;
///   * after disconnect/hangup: everything stops cleanly.
static SEND_GATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Enable / disable the send path for the active call. Called from
/// [`crate::session::PeerCall`] when the `PeerConnection` transitions into /
/// out of the connected state.
pub(crate) fn set_send_gate(on: bool) {
    SEND_GATE.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// True when local media should be encoded + written to the webrtc track.
pub(crate) fn should_send() -> bool {
    SEND_GATE.load(std::sync::atomic::Ordering::SeqCst)
}

use bytes::Bytes;
use webrtc_rs::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_PCMU};
use webrtc_rs::rtp_transceiver::rtp_codec::{
    RTPCodecType, RTCRtpCodecCapability, RTCRtpCodecParameters,
};
use webrtc_rs::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc_rs::track::track_local::TrackLocal;

use crate::error::Result;
use crate::signal::VKind;

pub type TrackArc = Arc<TrackLocalStaticSample>;

/// Local id / stream-id used in the SDP so the *remote* peer's `on_track`
/// handler can tell a camera track apart from a screen track.
fn vid_identity(kind: VKind) -> (String, String) {
    let s = match kind {
        VKind::Cam => "chatx-cam",
        VKind::Scr => "chatx-scr",
    };
    (s.to_owned(), s.to_owned())
}

fn audio_codec_cap() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_PCMU.to_owned(),
        clock_rate: 8_000,
        channels: 0,
        sdp_fmtp_line: String::new(),
        rtcp_feedback: Vec::new(),
    }
}

fn video_codec_cap() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=0".to_owned(),
        rtcp_feedback: Vec::new(),
    }
}

/// A `MediaEngine` registering only PCMU + H.264 (keeps the SDP small).
pub(crate) fn make_media_engine() -> Result<MediaEngine> {
    let mut e = MediaEngine::default();
    e.register_codec(
        RTCRtpCodecParameters {
            capability: audio_codec_cap(),
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;
    e.register_codec(
        RTCRtpCodecParameters {
            capability: video_codec_cap(),
            ..Default::default()
        },
        RTPCodecType::Video,
    )?;
    Ok(e)
}

fn as_track(t: &TrackArc) -> Arc<dyn TrackLocal + Send + Sync> {
    let t = Arc::clone(t);
    t as Arc<dyn TrackLocal + Send + Sync>
}

// ── AudSource ───────────────────────────────────────────────────────────────

/// Outbound G.711 PCMU audio. Push s16le PCM with [`AudSource::push`].
///
/// Audio arrives on the cpal audio thread at the device's native rate
/// (~48 kHz) → thousands of `push` calls per second. Each frame is encoded
/// to μ-law and handed to a **single** drain task that writes into the
/// webrtc track. The channel is **bounded** so that if the webrtc send
/// pipeline stalls (e.g. the peer never answers / is unreachable and the
/// SRTP writer isn't being drained), audio is dropped from the tail
/// instead of leaking memory on every push.
pub struct AudSource {
    muted: Arc<AtomicBool>,
    tx: tokio::sync::mpsc::Sender<webrtc_rs::media::Sample>,
    /// `track` is kept so callers can attach it to a `PeerCall`
    /// (via `track()`); the same `TrackArc` is also held by the drain task.
    track: TrackArc,
}

impl AudSource {
    pub fn new() -> Self {
        let track = TrackArc::new(TrackLocalStaticSample::new(
            audio_codec_cap(),
            "chatx-aud".into(),
            "chatx-aud".into(),
        ));

        // Bounded: a few seconds of 8 kHz μ-law audio (1 byte/sample).
        // `try_send` drops once full, so if the webrtc writer stalls (peer
        // unreachable / SRTP not drained) audio is dropped from the tail
        // instead of leaking memory per push. Normal calls drain well under
        // this ceiling.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<webrtc_rs::media::Sample>(8000);
        let drain_track = track.clone();
        let rt = crate::session::shared_runtime();
        let _ = rt.spawn(async move {
            while let Some(sample) = rx.recv().await {
                if let Err(e) = drain_track.write_sample(&sample).await {
                    tracing::warn!(err = %e, "aud write_sample");
                    break;
                }
            }
        });

        Self {
            muted: Arc::new(AtomicBool::new(false)),
            tx,
            track,
        }
    }

    /// Push one s16le PCM block. `rate` is in Hz (callers push at the
    /// negotiated device rate — we don't resample). Safe to call from the
    /// cpal audio thread: this is a non-blocking channel send (or a drop,
    /// once the drain is backlogged / stopped), so no per-call runtime build
    /// and no unbounded task accumulation.
    ///
    /// When [`AudSource::mute`](Self::mute) is `true`, the frame is dropped
    /// (no RTP packet is emitted). The remote peer hears silence.
    pub fn push(&self, s16le: &[u8], rate: u32) {
        if s16le.is_empty() || rate == 0 {
            return;
        }
        if self.muted.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        // Connection gate: don't mulaw-encode / packetize / enqueue RTP
        // before the peer is answering (peer may never arrive). The local
        // audio path is unaffected — the cpal callback already decodes /
        // resamples and forwards to `set_input_sink`, only the *outbound*
        // webrtc push is skipped.
        if !should_send() {
            return;
        }
        let data = mulaw_encode(s16le);
        let n_samples = (s16le.len() / 2) as u32;
        let dur = Duration::from_secs_f64(n_samples as f64 / rate as f64);
        let sample = webrtc_rs::media::Sample {
            data: Bytes::from(data),
            timestamp: SystemTime::now(),
            duration: dur.max(Duration::from_micros(1)),
            packet_timestamp: 0,
            prev_dropped_packets: 0,
            prev_padding_packets: 0,
        };
        // Non-blocking: if the drain task is backlogged (peer-side stall) we
        // drop the current sample rather than growing memory or blocking the
        // audio callback thread.
        let _ = self.tx.try_send(sample);
    }

    pub fn track(&self) -> Arc<dyn TrackLocal + Send + Sync> {
        as_track(&self.track)
    }

    pub fn mute(&self, on: bool) {
        self.muted.store(on, std::sync::atomic::Ordering::Relaxed);
        tracing::trace!(?on, "aud muted");
    }
}

// ── VidSource ───────────────────────────────────────────────────────────────

/// Outbound H.264 video. Push RGBA8 frames with [`VidSource::push`].
///
/// Encoding happens on a dedicated thread (openh264 is blocking). `push` is
/// non-blocking: the latest frame wins.
pub struct VidSource {
    track: TrackArc,
    latest: Arc<std::sync::Mutex<Option<Frame>>>,
    muted: Arc<AtomicBool>,
    /// Shared with the encode thread. Set on drop so the (detached) encoder
    /// thread wakes on its next tick and exits — releasing the openh264
    /// encoder, its tokio runtime, and the `latest` frame it otherwise pins.
    stopped: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct Frame {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

impl VidSource {
    pub fn new(kind: VKind) -> Result<Self> {
        let (id, stream_id) = vid_identity(kind);
        let track = TrackArc::new(TrackLocalStaticSample::new(
            video_codec_cap(),
            id,
            stream_id,
        ));
        let latest = Arc::new(std::sync::Mutex::new(None));
        let stopped = Arc::new(AtomicBool::new(false));
        let enc = openh264::encoder::Encoder::new()?;
        let track2 = track.clone();
        let latest2 = latest.clone();
        let stopped2 = stopped.clone();
        std::thread::Builder::new()
            .name("chatx-venc".into())
            .spawn(move || encode_loop(enc, track2, latest2, stopped2))
            .map_err(|e| crate::error::Error::other(format!("spawn venc: {e}")))?;
        Ok(Self {
            track,
            latest,
            muted: Arc::new(AtomicBool::new(false)),
            stopped,
        })
    }

    /// Explicitly stop the encoder thread. Also called from [`Drop`]; exposed
    /// so callers can tear down deterministically (e.g. on hangup / mid-call
    /// video-off) without waiting for the source to be dropped.
    pub fn stop(&self) {
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        *self.latest.lock().unwrap() = None;
    }

    /// Release the retained full-resolution frame WITHOUT stopping the encoder
    /// thread. Used when a video kind is switched OFF mid-call but the source
    /// is being REUSED: the device is stopped (no more `push`), so clearing
    /// `latest` both frees the last frame (screen 1440p ≈ 14MB, cam 8MP ≈
    /// 64MB) and lets the (still-alive) encoder idle — `encode_loop` skips
    /// when `latest` is `None`. Re-`push` on the next attach resumes it.
    /// Keeps the source's encoder + webrtc track binding alive for reuse,
    /// bounded to one per kind.
    pub fn clear(&self) {
        *self.latest.lock().unwrap() = None;
    }

    /// Push one RGBA8888 frame. When muted, the frame is dropped and no RTP
    /// packet is emitted (the peer freezes on the last frame).
    pub fn push(&self, rgba: &[u8], w: u32, h: u32) {
        if rgba.is_empty() || w == 0 || h == 0 {
            return;
        }
        if self.muted.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        // Connection gate: don't copy the full-res frame before the peer is
        // answering — the encoder loop skips it anyway, so holding it only
        // wastes RAM (up to ~14MB for a desktop screen).
        if !should_send() {
            return;
        }
        *self.latest.lock().unwrap() = Some(Frame { rgba: rgba.to_vec(), w, h });
    }

    pub fn mute(&self, on: bool) {
        self.muted.store(on, std::sync::atomic::Ordering::Relaxed);
        tracing::trace!(?on, "vid muted");
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn track(&self) -> Arc<dyn TrackLocal + Send + Sync> {
        as_track(&self.track)
    }
}

impl Drop for VidSource {
    fn drop(&mut self) {
        // Release the encoder (and its native buffers) + tokio runtime by
        // signalling the loop to exit. We do NOT join: the loop does
        // `rt.block_on(...)` on the track we also own, so a blocking join
        // here could self-deadlock. The stop flag alone is enough to reclaim
        // the openh264 encoder / frame / runtime (the thread exits on its
        // next ~16ms tick and drops everything it owned).
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn encode_loop(
    mut enc: openh264::encoder::Encoder,
    track: TrackArc,
    latest: Arc<std::sync::Mutex<Option<Frame>>>,
    stopped: Arc<AtomicBool>,
) {
    use openh264::formats::{RgbaSliceU8, YUVBuffer};
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("venc rt");
    loop {
        if stopped.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(16)); // ~60 fps cap
        let frame = latest.lock().unwrap().clone();
        let Some(frame) = frame else { continue };

        // Connection gate: before the peer answers (and after it hangs up /
        // disconnects) we must NOT feed the webrtc track — encoded frames only
        // pile up in the send pipeline. Skip the encode entirely (cheapest CPU
        // win: no openh264 work, no `write_sample`) and drop the retained
        // full-res frame so it isn't pinned while we're not sending.
        if !should_send() {
            *latest.lock().unwrap() = None;
            continue;
        }

        let src = RgbaSliceU8::new(&frame.rgba, (frame.w as usize, frame.h as usize));
        let yuv = YUVBuffer::from_rgb_source(src);
        let bs = match enc.encode(&yuv) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(err = %e, "h264 encode failed");
                continue;
            }
        };
        let data = bs.to_vec();
        if data.is_empty() {
            continue;
        }
        let sample = webrtc_rs::media::Sample {
            data: Bytes::from(data),
            timestamp: SystemTime::now(),
            duration: Duration::from_millis(33),
            packet_timestamp: 0,
            prev_dropped_packets: 0,
            prev_padding_packets: 0,
        };
        if let Err(e) = rt.block_on(track.write_sample(&sample)) {
            tracing::warn!(err = %e, "write_sample video failed");
        }
    }
}

// ── Audio payload (PCMU track slot) ────────────────────────────────────────
//
// webrtc-rs 0.17 is a **pure byte pipe** for the PCMU track: the sender's
// `Sample.data` is sliced by MTU and put verbatim into the RTP payload, and
// the receiver's `read()` hands back exactly those bytes (the rtp crate's
// `G7xxPayloader` is a bytes-chopper, NOT a codec — it contains no µ-law /
// A-law math; there is no G.711 codec anywhere in the library).
//
// Because **both endpoints are always our own app**, the wire bytes can be
// plain 16-bit little-endian PCM. The functions below are therefore
// *identity*: they pass the s16le buffer through untouched. The SDP still
// advertises `audio/PCMU 8000` (the only audio codec we register), but that
// is just a label — the actual payload is 16-bit PCM, consistent on both
// sides. This gives full dynamic range (no quantization hiss) at
// 8 kHz / 16-bit / mono = 128 kbps, plenty for the relay.
//
// (The earlier implementations tried to be µ-law and broke; 8-bit linear
// quantization produced audible hiss. Lossless PCMU-slot PCM avoids both
// problems and is trivially correct + testable.)

/// Pass 16-bit LE PCM bytes through to the webrtc `Sample.data`.
/// `pcm` is already at the codec's 8 kHz / 1-ch rate (resampled upstream).
pub fn mulaw_encode(pcm: &[u8]) -> Vec<u8> {
    pcm.to_vec()
}

/// The received `law` bytes are already 16-bit LE PCM (identity of
/// `mulaw_encode`). Pass through unchanged to the speaker queue.
pub fn mulaw_decode(law: &[u8]) -> Vec<u8> {
    law.to_vec()
}

#[cfg(test)]
mod mulaw_tests {
    use super::*;

    fn roundtrip(m: i16) -> i16 {
        let pcm = m.to_le_bytes();
        let enc = mulaw_encode(&pcm);
        let dec = mulaw_decode(&enc);
        i16::from_le_bytes([dec[0], dec[1]])
    }

    #[test]
    fn zero_maps_to_zero() {
        assert_eq!(roundtrip(0), 0);
    }

    #[test]
    fn roundtrip_is_lossless_for_every_sample() {
        // Identity codec must reconstruct every possible 16-bit value exactly.
        for m in std::i16::MIN..=std::i16::MAX {
            assert_eq!(roundtrip(m), m, "m={m}");
        }
    }

    #[test]
    fn byte_count_and_bits_preserved() {
        // 16 samples @ 16-bit = 32 bytes, preserved both directions.
        let pcm: Vec<u8> = (0..16).flat_map(|i| (i as i16 * 2000).to_le_bytes()).collect();
        assert_eq!(pcm.len(), 32);
        assert_eq!(mulaw_encode(&pcm).len(), 32);
        assert_eq!(mulaw_decode(&mulaw_encode(&pcm)).len(), 32);
        assert_eq!(mulaw_decode(&mulaw_encode(&pcm)), pcm);
    }
}
