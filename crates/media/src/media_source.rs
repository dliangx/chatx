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
    /// Diagnostic push counter (first + every 50th log line).
    push_n: std::sync::atomic::AtomicU64,
    /// Short log tag so live logs can tell the camera encoder apart from the
    /// screen one ("cam" vs "scr").
    tag: &'static str,
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
        // Screen capture runs at *full desktop resolution* (e.g. 2560x1440).
        // The default `Encoder::new()` target bitrate is only **120 kbps**,
        // far too low for full-res screen content — the rate controller drops
        // most frames and the ones kept are grainy, which is why the shared
        // screen was "unreadable". Clarity at full resolution is therefore
        // governed by the *target bitrate*, and by how much the network can
        // actually carry. So for the screen source:
        //   * keep the captured resolution UNCAPPED by default (no downscale),
        //     so the picture stays sharp — the user explicitly does not want
        //     the screen shrunk;
        //   * raise the target bitrate to a readable level (6 Mbps);
        //   * run at a modest frame rate (~10 fps) so the per-second volume
        //     stays within what a real 1:1 relay can deliver without heavy loss.
        // All three are overridable via env vars for tuning per network:
        //   CHATX_SCR_BITRATE  bits/s   (default 6_000_000)
        //   CHATX_SCR_MAX_W    px, 0=no cap (default 0 = full res)
        //   CHATX_SCR_FPS      fps    (default 10)
        let is_scr = matches!(kind, VKind::Scr);
        let bitrate_bps = if is_scr {
            std::env::var("CHATX_SCR_BITRATE")
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(6_000_000)
        } else {
            0
        };
        let max_w = if is_scr {
            std::env::var("CHATX_SCR_MAX_W")
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0)
        } else {
            0
        };
        let fps = if is_scr {
            std::env::var("CHATX_SCR_FPS")
                .ok()
                .and_then(|s| s.trim().parse::<f32>().ok())
                .filter(|f| *f > 0.0)
                .unwrap_or(10.0)
        } else {
            60.0
        };
        let enc = if is_scr {
            openh264::encoder::Encoder::with_api_config(
                openh264::OpenH264API::from_source(),
                openh264::encoder::EncoderConfig::new().bitrate(openh264::encoder::BitRate::from_bps(
                    bitrate_bps,
                )),
            )?
        } else {
            openh264::encoder::Encoder::new()?
        };
        // Encode-rate cap: 1/fps.
        let tick = Duration::from_millis(((1000.0 / fps).round().clamp(1.0, 1000.0)) as u64);
        let tag = tag_of_kind(kind);
        let track2 = track.clone();
        let latest2 = latest.clone();
        let stopped2 = stopped.clone();
        std::thread::Builder::new()
            .name("chatx-venc".into())
            .spawn(move || encode_loop(enc, track2, latest2, stopped2, tick, max_w, tag))
            .map_err(|e| crate::error::Error::other(format!("spawn venc: {e}")))?;
        Ok(Self {
            track,
            latest,
            muted: Arc::new(AtomicBool::new(false)),
            stopped,
            push_n: std::sync::atomic::AtomicU64::new(0),
            tag,
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
        // Diagnostic: prove the capture thread actually delivered a frame to
        // this source, and whether the connection gate let it through to
        // `latest`. Throttled (first + every 50th) so it never floods.
        if !self.muted.load(std::sync::atomic::Ordering::Relaxed) {
            let n = self.push_n.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n == 1 || n % 50 == 0 {
                tracing::debug!(
                    "[media] {} push #{} WxH={w}x{h} gate={}",
                    self.tag,
                    n,
                    should_send()
                );
            }
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

/// Short log tag for a video kind so live logs can tell the camera encoder
/// apart from the screen one.
fn tag_of_kind(kind: VKind) -> &'static str {
    match kind {
        VKind::Cam => "cam",
        VKind::Scr => "scr",
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
    tick: Duration,
    max_w: u32,
    tag: &'static str,
) {
    use openh264::formats::{RgbaSliceU8, YUVBuffer};
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("venc rt");
    // Throttled send diagnostic (first sample + every 50th) written to
    // stderr/chatx.log: proves the sender actually produced + handed the
    // encoded frame to the webrtc track, isolating "no frames produced"
    // from "frames dropped on the wire" in the field.
    let sent = std::sync::atomic::AtomicU64::new(0);
    // Diagnostic: count each encode_loop outcome so a live log can disambiguate
    // "no frame arrived" (latest empty) from "encode errored" / "encode empty"
    // from "encoded + written out". Throttled (first + every 50th).
    let enc_tick = std::sync::atomic::AtomicU64::new(0);
    loop {
        if stopped.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(tick); // frame-rate cap (see `VidSource::new`)
        let frame = latest.lock().unwrap().clone();
        if frame.is_none() {
            let n = enc_tick.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n == 1 || n % 50 == 0 {
                tracing::debug!("[media] {tag} venc #{}: no frame pending (latest empty)", n);
            }
            continue;
        }
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

        // Optional width cap (screen source): downscale to ≤ max_w so each
        // encoded frame stays small enough for a live 1:1 over a relay. The
        // cap is constant for the source's life, so the openh264 encoder always
        // sees the same resolution (it hard-errors if the size changes
        // mid-stream). `max_w == 0` means "no limit" (camera path).
        //
        // openh264 hard-limits the source to 3840×2160 (see `reinit` in
        // openh264::encoder::Encoder — anything greater refuses to encode),
        // so Retina / 5K displays MUST be downscaled before the call. This is
        // the LAST clamp: effective ceiling is `min(max_w, 3840)` × 2160,
        // preserving aspect ratio.
        const H264_MAX_W: u32 = 3840;
        const H264_MAX_H: u32 = 2160;
        let cap_w = if max_w > 0 { max_w.min(H264_MAX_W) } else { H264_MAX_W };
        let cap_h = H264_MAX_H;
        let (rgba, w, h) = if frame.w > cap_w || frame.h > cap_h {
            // Uniform scale: fit inside (cap_w, cap_h) while preserving aspect.
            let scale = (cap_w as f32 / frame.w as f32).min(cap_h as f32 / frame.h as f32);
            let dw = (frame.w as f32 * scale).round().max(1.0) as u32;
            let dh = (frame.h as f32 * scale).round().max(1.0) as u32;
            (downscale_rgba(&frame.rgba, frame.w, frame.h, dw, dh), dw, dh)
        } else {
            (frame.rgba, frame.w, frame.h)
        };

        let src = RgbaSliceU8::new(&rgba, (w as usize, h as usize));
        let yuv = YUVBuffer::from_rgb_source(src);
        let bs = match enc.encode(&yuv) {
            Ok(b) => b,
            Err(e) => {
                let n = enc_tick.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n == 1 || n % 10 == 0 {
                    tracing::warn!("[media] {tag} venc #{}: encode ERROR {e} ({}x{})", n, w, h);
                }
                tracing::warn!(err = %e, "{tag} h264 encode failed");
                continue;
            }
        };
        let data = bs.to_vec();
        if data.is_empty() {
            let n = enc_tick.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n == 1 || n % 10 == 0 {
                tracing::debug!("[media] {tag} venc #{}: encode EMPTY ({}x{})", n, w, h);
            }
            continue;
        }
        let data_len = data.len();
        let sample = webrtc_rs::media::Sample {
            data: Bytes::from(data),
            timestamp: SystemTime::now(),
            duration: tick,
            packet_timestamp: 0,
            prev_dropped_packets: 0,
            prev_padding_packets: 0,
        };
        let n_w = sent.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n_w == 1 || n_w % 50 == 0 {
            tracing::debug!(
                "[media] {tag} before write_sample #{} enc={}B ({}x{})",
                n_w, data_len, w, h
            );
        }
        if let Err(e) = rt.block_on(track.write_sample(&sample)) {
            tracing::warn!(err = %e, "{tag} write_sample video failed");
        }
        if n_w == 1 || n_w % 50 == 0 {
            tracing::debug!("[media] {tag} video sent #{} enc={}B ({}x{})", n_w, data_len, w, h);
        }
    }
}

/// Nearest-neighbour downscale of an RGBA8888 buffer to exact `dw`x`dh`.
/// Used to cap the screen-share encode width before the (fixed-resolution)
/// openh264 encoder so each frame stays small on the wire. `dw`/`dh` only
/// ever shrink (`dw < sw`, `dh < sh`).
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
