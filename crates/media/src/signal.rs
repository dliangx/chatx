use serde::{Deserialize, Serialize};

/// WebRTC signaling message kind (serde over the wire transport).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "data")]
pub enum SignalKind {
    /// Offerer: local SDP.
    Offer(String),
    /// Answerer: local SDP in reply to an `Offer`.
    Answer(String),
    /// ICE candidate (ufrag, sdpMid, sdpMlineIndex, candidate).
    Ice {
        ufrag: String,
        sdp_mid: String,
        sdp_mline_index: u16,
        candidate: String,
    },
    /// Mute / unmute the local microphone (applies on the *local* peer).
    MuteMic(bool),
    /// Start / stop local camera (applies on the *local* peer).
    Cam(bool),
    /// Start / stop local screen (applies on the *local* peer).
    Screen(bool),
    /// Hang up.
    Bye,
}

/// One signaling frame. `call_id` correlates a whole call session; both
/// peers use the same id (offerer picks it, answerer echoes it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalFrame {
    pub call_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    pub kind: SignalKind,
}

impl SignalFrame {
    pub fn new(call_id: impl Into<String>, kind: SignalKind) -> Self {
        Self { call_id: call_id.into(), from: String::new(), kind }
    }
}
