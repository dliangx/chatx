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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FriendQ {
    pub action: FriendQAction,
    pub requester: String,
    pub recipient: String,
    /// The sender of this frame — typically their username — so the
    /// receiver has a display hint immediately. Optional / empty is fine.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sender_name: String,
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
        }
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
}
