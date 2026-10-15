use serde::{Deserialize, Serialize};

/// Which local video source is being (up)graded.
///
/// `Cam` = local camera, `Scr` = local screen capture. We keep the two on
/// *separate* tracks (distinct `stream_id`) so the peer's `on_track` handler
/// can route the remote camera vs. remote screen independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VKind {
    Cam,
    Scr,
}

/// The intent a call is started with (carried on the *initial* `Offer`).
///
/// Needed because a video-offer and a screen-offer look identical in the SDP
/// (audio + one H.264 video line), so the answerer cannot tell them apart
/// from the SDP alone. The answerer uses this to attach the matching local
/// track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallMode {
    Audio,
    Video,
    Screen,
}

/// WebRTC signaling message kind (serde over the wire transport).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "data")]
pub enum SignalKind {
    /// Offerer: local SDP.
    Offer(String),
    /// Answerer: local SDP in reply to an `Offer`.
    ///
    /// Carried on a frame whose `upgrade_kind` is `Some(..)` when it answers an
    /// *upgrade* offer (mid-call), `None` when it answers the initial call.
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
///
/// `upgrade_kind` / `both_sides` describe a *mid-call* renegotiation carried
/// on an `Offer`/`Answer` frame (e.g. audio → video, audio → screen, or a
/// screen-recipient opening their own camera). Both are `None` for the
/// initial call, so older peers keep deserializing fine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalFrame {
    pub call_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    pub kind: SignalKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_kind: Option<VKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub both_sides: Option<bool>,
    /// Initial call intent (only meaningful on the first `Offer`). `None`
    /// means "unknown / legacy audio call".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CallMode>,
}

impl SignalFrame {
    pub fn new(call_id: impl Into<String>, kind: SignalKind) -> Self {
        Self {
            call_id: call_id.into(),
            from: String::new(),
            kind,
            upgrade_kind: None,
            both_sides: None,
            mode: None,
        }
    }

    /// Convenience for building an upgrade `Offer`/`Answer` frame.
    pub fn with_upgrade(mut self, kind: VKind, both: bool) -> Self {
        self.upgrade_kind = Some(kind);
        self.both_sides = Some(both);
        self
    }

    /// Convenience for the *initial* offer/answer, tagging it with the
    /// caller's `CallMode` so the peer can mirror it.
    pub fn with_mode(mut self, mode: CallMode) -> Self {
        self.mode = Some(mode);
        self
    }
}
