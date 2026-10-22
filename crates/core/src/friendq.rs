//! Wire format for peer-to-peer friend requests, carried over the existing DM
//! request/response channel with a marker in `ChatRequest::text`.
//!
//! The two endpoints are libp2p peer ids (base58). The sender is identified by
//! `ChatRequest::from`; we re-state both peer ids in the payload so the frame
//! is self-contained, and we also carry the sender's `sender_name` (typically
//! the username) so the receiving side has a display hint right away even if
//! it has not previously resolved the sender.

use serde::{Deserialize, Serialize};

/// Prefix marking a friend-request frame inside the DM `text` field. Old peers
/// that predate this feature render it as an unreadable chat line but nothing
/// breaks — the frame is otherwise inert.
pub const TEXT_PREFIX: &str = "__chatx/friendq/__";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FriendQAction {
    /// `requester` asks `recipient` to add them as a friend.
    Inquire,
    /// `recipient` accepts the request from `requester`.
    Accept,
    /// `recipient` declines the request from `requester`.
    Reject,
}

/// Self-reported display profile carried by the *requester* in a FriendQ frame
/// so the receiver can persist rich contact info the first time they see the
/// peer. `nickname` and `bio` mirror what the sender's UI shows; `research` is
/// a short "what I work on" line; `public_key` is the requester's E2E public
/// key (X25519) so the receiver can immediately encrypt messages without an
/// extra directory round-trip; `avatar` is the requester's local 40×40 avatar
/// as a base64-encoded image (JPEG/PNG), kept compact over the P2P wire.
///
/// All fields are optional so old frames without a profile still decode, and so
/// a new sender with no profile set yet does not send a wall of `null`s.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research: Option<String>,
    /// E2E public key (X25519, base64 or raw hex — sender's choice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    /// base64-encoded avatar image (40×40, JPEG/PNG). Empty string means absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
}

/// True if the profile is carrying anything useful. Used to skip serializing
/// an all-`None` profile (keeps wire frames small and old-peer friendly).
impl UserProfile {
    /// True when no field carries any value (`None` or blank) — used to skip
    /// serializing an all-empty profile so wire frames stay compact.
    pub fn is_empty(&self) -> bool {
        [
            self.nickname.as_deref(),
            self.bio.as_deref(),
            self.research.as_deref(),
            self.public_key.as_deref(),
            self.avatar.as_deref(),
        ]
        .into_iter()
        .all(|v| v.map_or(true, |s| s.trim().is_empty()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FriendQ {
    pub action: FriendQAction,
    pub requester: String,
    pub recipient: String,
    /// The sender of this frame — typically their username — so the
    /// receiver has a display hint immediately. Optional / empty is fine.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sender_name: String,
    /// Rich display profile the sender wants the receiver to persist.
    /// `None` when the sender has no profile / no avatar to share (kept on
    /// the wire identical to pre-profile frames so old peers ignore it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<UserProfile>,
}

impl FriendQ {
    /// Build a frame for an action the local device (as `initiator`, peer
    /// `initiator_peer`, known username `initiator_name`) performs on `peer`.
    pub fn new(
        action: FriendQAction,
        initiator_peer: &str,
        initiator_name: &str,
        peer: &str,
    ) -> Self {
        let (requester, recipient) = match action {
            FriendQAction::Inquire => (
                initiator_peer.to_string(),
                peer.to_string(),
            ),
            FriendQAction::Accept | FriendQAction::Reject => (
                peer.to_string(),
                initiator_peer.to_string(),
            ),
        };
        Self {
            action,
            requester,
            recipient,
            sender_name: initiator_name.to_string(),
            profile: None,
        }
    }

    /// Attach a self-reported profile (e.g. the requester's nickname, bio,
    /// E2E public key, 40×40 avatar). `None` or an empty profile is dropped —
    /// wire frames stay identical to the pre-profile shape that old peers
    /// understand.
    pub fn with_profile(mut self, profile: UserProfile) -> Self {
        if profile.is_empty() {
            return self;
        }
        self.profile = Some(profile);
        self
    }

    pub fn encode(&self) -> Option<String> {
        Some(format!("{TEXT_PREFIX} {}", serde_json::to_string(self).ok()?))
    }

    pub fn decode(raw: &str) -> Option<Self> {
        let json = raw.strip_prefix(TEXT_PREFIX)?.trim_start();
        serde_json::from_str(json).ok()
    }

    pub fn is_frame(raw: &str) -> bool {
        raw.starts_with(TEXT_PREFIX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_roundtrip() {
        let f = FriendQ::new(FriendQAction::Inquire, "A", "alice", "B");
        let text = f.encode().unwrap();
        assert!(FriendQ::is_frame(&text));
        let d = FriendQ::decode(&text).unwrap();
        assert_eq!(d.action, FriendQAction::Inquire);
        assert_eq!(d.requester, "A");
        assert_eq!(d.recipient, "B");
        assert_eq!(d.sender_name, "alice");
    }

    #[test]
    fn accept_roles() {
        let f = FriendQ::new(FriendQAction::Accept, "B", "bob", "A");
        assert_eq!(f.requester, "A");
        assert_eq!(f.recipient, "B");
        assert_eq!(f.sender_name, "bob");
    }

    #[test]
    fn non_frame_decode_fails() {
        assert!(FriendQ::decode("hello").is_none());
        assert!(!FriendQ::is_frame("hello"));
    }

    #[test]
    fn profile_roundtrip() {
        let f = FriendQ::new(FriendQAction::Inquire, "A", "alice", "B").with_profile(UserProfile {
            nickname: Some("Alice W".into()),
            bio: Some("hi".into()),
            research: Some("p2p".into()),
            public_key: Some("pk123".into()),
            avatar: Some("aW1hZ2U=".into()),
        });
        let d = FriendQ::decode(&f.encode().unwrap()).unwrap();
        let p = d.profile.expect("profile present");
        assert_eq!(p.nickname.as_deref(), Some("Alice W"));
        assert_eq!(p.bio.as_deref(), Some("hi"));
        assert_eq!(p.research.as_deref(), Some("p2p"));
        assert_eq!(p.public_key.as_deref(), Some("pk123"));
        assert_eq!(p.avatar.as_deref(), Some("aW1hZ2U="));
    }

    #[test]
    fn empty_profile_dropped() {
        let f = FriendQ::new(
            FriendQAction::Inquire,
            "A",
            "alice",
            "B",
        )
        .with_profile(UserProfile::default());
        assert!(f.profile.is_none());
    }

    #[test]
    fn old_frame_without_profile_decodes() {
        // A frame produced by a pre-profile peer (no `profile` key).
        let json = serde_json::json!({
            "action": "inquire",
            "requester": "A",
            "recipient": "B",
            "sender_name": "alice"
        })
        .to_string();
        let raw = format!("{TEXT_PREFIX} {json}");
        let d = FriendQ::decode(&raw).unwrap();
        assert_eq!(d.requester, "A");
        assert!(d.profile.is_none());
    }
}
