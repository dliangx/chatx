
use serde::{Deserialize, Serialize};

const _PROTOCOL: &str = "/chatx/text/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MsgKind {
    #[default]
    Dm,
    GroupKey,
    GroupMsg,
    /// Realtime audio frame (s16le PCM, base64-transported inside the JSON
    /// envelope). Not persisted to the chat store — routed straight to the
    /// speaker-playback sink on the far end.
    Audio,
    /// WebRTC signaling frame (SDP offer/answer + ICE trickle) carried as an
    /// opaque JSON string. Not persisted.
    Webrtc,
    /// Persistent voice *message* — a recorded mic clip transmitted as s16le
    /// PCM (base64), stored locally on both sides and rendered as a bubble
    /// with a duration.
    Voice,
}

/// One realtime-audio frame carried inside a [`ChatRequest::audio`].
///
/// - `rate` — sample rate in Hz of `data`
/// - `ch`   — channel count (1 or 2)
/// - `data` — base64(s16le PCM); `decoded.len() == samples * 2 * ch`
///
/// Carried as base64 so it survives the request/response JSON transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioPayload {
    pub rate: u32,
    pub ch: u32,
    pub data: String,
}

/// A persistent voice message body carried inside a [`ChatRequest::voice`].
///
/// - `rate` — sample rate in Hz of `data`
/// - `ch`   — channel count (1 or 2)
/// - `duration_ms` — total capture duration in milliseconds (UI shows it)
/// - `data` — base64(s16le PCM); `decoded.len() == frames * 2 * ch`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoicePayload {
    pub rate: u32,
    pub ch: u32,
    #[serde(default)]
    pub duration_ms: u64,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub id: u64,
    pub from: String,
    pub e2e: String,
    pub text: Option<String>,
    pub sealed: Option<String>,
    #[serde(default)]
    pub kind: MsgKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioPayload>,
    /// Opaque WebRTC signaling payload (a `media::SignalFrame` serialized to
    /// JSON). Kept as a `String` so this crate does not depend on `media`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<VoicePayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: u64,
    pub e2e: String,
}

/// Voice-message request/response pair, carried on the binary CBOR channel
/// (`VOICE_PROTOCOL`). Unlike the JSON channel, `data` is a raw `Vec<u8>` —
/// `cbor4ii` serialises it as a byte-string with no base64 expansion, so a
/// 4.9 MB clip stays ~4.9 MB on the wire (JSON + string would have been
/// ~6.5 MB). The `request_size_maximum` is set on the codec at construction
/// time in `swarm.rs` to a value comfortably above any clip we produce.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceRequest {
    pub id: u64,
    /// Peer base58 of the recording sender (echoed in log/audit, not required
    /// for correctness — the transport already tells us who we are).
    pub from: String,
    pub rate: u32,
    pub ch: u32,
    #[serde(default)]
    pub duration_ms: u64,
    /// Raw s16le PCM. `len == frames * 2 * ch`.
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceAck {
    pub id: u64,
}

/// A photo message carried on the binary CBOR `image` channel. `data` is the
/// raw image bytes (JPEG/PNG) — `cbor4ii` serializes `Vec<u8>` as a compact
/// byte-string, so the wire size equals the image size. The sender writes the
/// same bytes to its own local disk *before* sending and records them by
/// path; the receiver writes the received bytes to disk and records them by
/// path (so both sides keep a self-contained local copy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageRequest {
    pub id: u64,
    pub from: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAck {
    pub id: u64,
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
