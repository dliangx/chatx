//! Chat image send/receive: size ceiling + per-row send state.
//!
//! Design (agreed 2026-11-02, revised): chat photos travel over the wire as
//! the **ORIGINAL (HD) bytes** — sent raw, with no lossless-compression
//! envelope. gzip buys almost nothing for JPEG (the dominant photo format) and
//! the complexity was not worth it, so both DM and group images just carry the
//! original. The `chatx-core` transport stays byte-agnostic; it simply carries
//! `data`, which for a photo is the original image bytes.
//!
//! The on-list **thumbnail** (a ≤300px downscale, see `image.rs`) is a separate
//! concern: it is what the bubble *draws*; the full original is only decoded
//! in the full-screen viewer on tap.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Outbound size ceiling for a chat image; mirrors the file cap so both share
/// one "too big" rule (50 MiB).
pub const CHAT_IMAGE_MAX_BYTES: u64 = 50 * 1024 * 1024;

// ── Per-row media send state -----------------------------------------------
//
// A media row you sent (image / voice / file) shows a badge (sending / sent /
// failed) instead of the old shared spinner. The row's identity is its store
// timestamp (`t`), so the in-flight state is keyed by `(chat_id, t)` in this
// process-local registry.
//
// A row that is NOT in the registry is considered settled: a self media row
// that made it into the chat store reads as "sent". The badge therefore only
// needs to override the two transient cases — "sending" and "failed".

/// `MessageData.send_state` values (mirrored in `chat.slint`).
pub const SEND_STATE_NONE: i32 = 0;
/// The media row is on disk and the wire send is still in flight.
pub const SEND_STATE_SENDING: i32 = 1;
/// The receiver acknowledged (or the row is a settled local/remote media).
pub const SEND_STATE_SENT: i32 = 2;
/// The wire send timed out / errored.
pub const SEND_STATE_FAILED: i32 = 3;

/// `(chat_id, row_t) -> send_state` for self media rows, process-local.
static SEND_REGISTRY: OnceLock<Mutex<HashMap<(String, i64), i32>>> =
    OnceLock::new();

fn registry() -> &'static Mutex<HashMap<(String, i64), i32>> {
    SEND_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a self media row (keyed by its store `t`) as "sending".
pub fn note_media_sending(chat_id: &str, t: i64) {
    registry()
        .lock()
        .unwrap()
        .insert((chat_id.to_string(), t), SEND_STATE_SENDING);
}

/// Set the send state of a self voice row (sent / failed).
pub fn set_media_state(chat_id: &str, t: i64, state: i32) {
    registry()
        .lock()
        .unwrap()
        .insert((chat_id.to_string(), t), state);
}

/// Look up the send state of a self voice row; `SEND_STATE_NONE` if unset.
pub fn media_state(chat_id: &str, t: i64) -> i32 {
    registry()
        .lock()
        .unwrap()
        .get(&(chat_id.to_string(), t))
        .copied()
        .unwrap_or(SEND_STATE_NONE)
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_state_registry_round_trips() {
        let chat = "test-chat";
        // Not yet present -> NONE.
        assert_eq!(media_state(chat, 42), SEND_STATE_NONE);
        note_media_sending(chat, 42);
        assert_eq!(media_state(chat, 42), SEND_STATE_SENDING);
        set_media_state(chat, 42, SEND_STATE_SENT);
        assert_eq!(media_state(chat, 42), SEND_STATE_SENT);
        // A different row in the same chat is unaffected.
        assert_eq!(media_state(chat, 43), SEND_STATE_NONE);
    }
}
