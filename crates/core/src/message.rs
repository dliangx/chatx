use serde::{Deserialize, Serialize};

const _PROTOCOL: &str = "/chatx/text/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MsgKind {
    #[default]
    Dm,
    GroupKey,
    GroupMsg,
    Audio,
    Webrtc,
    Voice,
}

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
/// raw image bytes (JPEG/PNG)
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

/// A document file (pdf/docx/xlsx/pptx) sent over the binary CBOR file channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRequest {
    pub id: u64,
    pub from: String,
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAck {
    pub id: u64,
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
