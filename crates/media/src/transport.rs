//! Outbound signaling transport.
//!
//! `media` never owns a wire channel — apps inject one. The chat app
//! (`crates/network` eventually, or `apps/chat` in the short term) bridges
//! this trait into libp2p request-response and serializes [`SignalFrame`]s
//! with `serde_json` (already `Serialize` + `Deserialize`).
//!
//! Implementations should be `Send + Sync`: `send` may be called from
//! `on_ice_candidate` (any tokio worker) and from user actions.
//!
//! `send` is intentionally *not* `async fn` so implementors can choose their
//! back-pressure (sync sender channels, `tokio::sync::mpsc` + fire-and-forget
//! task, etc.). A failed send is treated as fatal by the caller (typically
//! logged + call ended by the app's event loop).

use crate::error::Result;
use crate::signal::SignalFrame;

pub trait SignalTransport: Send + Sync {
    fn send(&self, frame: &SignalFrame) -> Result<()>;
}
