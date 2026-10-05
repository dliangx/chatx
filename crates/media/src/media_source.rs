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

use bytes::Bytes;
use webrtc_rs::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_PCMU};
use webrtc_rs::rtp_transceiver::rtp_codec::{
    RTPCodecType, RTCRtpCodecCapability, RTCRtpCodecParameters,
};
use webrtc_rs::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc_rs::track::track_local::TrackLocal;

use crate::error::Result;

pub type TrackArc = Arc<TrackLocalStaticSample>;

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
pub struct AudSource {
    track: TrackArc,
    muted: Arc<AtomicBool>,
}

impl AudSource {
    pub fn new() -> Self {
        Self {
            track: TrackArc::new(TrackLocalStaticSample::new(
                audio_codec_cap(),
                "chatx-aud".into(),
                "chatx-aud".into(),
            )),
            muted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Push one s16le PCM block. `rate` is in Hz (callers push at the
    /// negotiated device rate — we don't resample). Safe to call from the
    /// cpal audio thread (it spawns on tokio or spins a brief runtime).
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
        let track = self.track.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                let _ = h.spawn(async move { let _ = track.write_sample(&sample).await; });
            }
            Err(_) => {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build();
                if let Ok(rt) = rt {
                    let _ = rt.block_on(track.write_sample(&sample));
                }
            }
        }
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
}

#[derive(Clone)]
pub struct Frame {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

impl VidSource {
    pub fn new() -> Result<Self> {
        let track = TrackArc::new(TrackLocalStaticSample::new(
            video_codec_cap(),
            "chatx-vid".into(),
            "chatx-vid".into(),
        ));
        let latest = Arc::new(std::sync::Mutex::new(None));
        let enc = openh264::encoder::Encoder::new()?;
        let track2 = track.clone();
        let latest2 = latest.clone();
        std::thread::Builder::new()
            .name("chatx-venc".into())
            .spawn(move || encode_loop(enc, track2, latest2))
            .map_err(|e| crate::error::Error::other(format!("spawn venc: {e}")))?;
        Ok(Self { track, latest })
    }

    pub fn push(&self, rgba: &[u8], w: u32, h: u32) {
        if rgba.is_empty() || w == 0 || h == 0 {
            return;
        }
        *self.latest.lock().unwrap() = Some(Frame { rgba: rgba.to_vec(), w, h });
    }

    pub fn track(&self) -> Arc<dyn TrackLocal + Send + Sync> {
        as_track(&self.track)
    }
}

fn encode_loop(
    mut enc: openh264::encoder::Encoder,
    track: TrackArc,
    latest: Arc<std::sync::Mutex<Option<Frame>>>,
) {
    use openh264::formats::{RgbaSliceU8, YUVBuffer};
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("venc rt");
    loop {
        std::thread::sleep(std::time::Duration::from_millis(16)); // ~60 fps cap
        let frame = latest.lock().unwrap().clone();
        let Some(frame) = frame else { continue };

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

// ── μ-law (RFC 3551) ───────────────────────────────────────────────────────

pub fn mulaw_encode(pcm: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() / 2);
    let mut i = 0;
    while i + 1 < pcm.len() {
        let v = i16::from_le_bytes([pcm[i], pcm[i + 1]]);
        out.push(le_mulaw(v as u16));
        i += 2;
    }
    out
}

pub fn mulaw_decode(law: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(law.len() * 2);
    for &b in law {
        let v = ule_mulaw(b) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn le_mulaw(v: u16) -> u8 {
    const BIAS: i32 = 0x84;
    let sign = ((v >> 15) & 1) as i32;
    let mut x = (v & 0x7fff) as i32 + BIAS;
    let mut exp = 0;
    let mut shift = 1 << 13;
    while x & (shift as u32 as i32) == 0 && exp < 6 {
        x = (x << 1) as i32;
        exp += 1;
        shift >>= 1;
    }
    let mant = (x >> (1 + exp)) & 0x7f;
    ((sign & 1) << 7 | (exp & 7) << 4 | mant) as u8
}

fn ule_mulaw(law: u8) -> u32 {
    const BIAS: i32 = 0x84;
    let sign = ((law >> 7) & 1) as i32;
    let exp = ((law >> 4) & 7) as i32;
    let mant = (law & 0x0f) as i32;
    let x = ((mant << 4) + (1 << 3) + (1 << exp)) << (exp + 1);
    ((1i32 - 2 * sign) * (x - BIAS)) as u32
}
