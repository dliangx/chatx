
use serde::{Deserialize, Serialize};

const _PROTOCOL: &str = "/p2pchat/text/1";

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: u64,
    pub e2e: String,
}

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
