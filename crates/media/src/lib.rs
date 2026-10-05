//! WebRTC media transport for chatx.
//!
//! One [`PeerCall`] per 1-to-1 voice / video / screen-share call.
//!
//! - **Voice**: real RTP audio track, codec **G.711 PCMU** (pure Rust —
//!   `rtp::codecs::g7xx`). Callers push s16le PCM into [`AudSource`]::push.
//! - **Camera + Screen**: separate [`VidSource`]s, each on its own video track.
//!   Frames come in as RGBA8, are converted to YUV and encoded as H.264 with
//!   [openh264].
//!
//! This crate does **not** own the wire channel: callers inject a
//! [`SignalTransport`] (typically backed by libp2p in `crates/network`) and
//! feed it SDP + ICE `SignalFrame`s. It also does **not** own the media
//! sources: callers push frames from `crates/audio` / `crates/camera` /
//! `crates/screen` sinks into the `AudSource`/`VidSource` types.
//!
//! Remote frames are delivered via the sinks set with
//! [`PeerCall::set_remote_camera_sink`] / `set_remote_screen_sink` /
//! `set_remote_audio_sink`. Callbacks run on the tokio worker thread that
//! reads the RTP read stream — hop to your UI thread (e.g.
//! `slint::invoke_from_event_loop`) before touching UI widgets.

mod config;
mod error;
mod media_source;
mod session;
mod signal;
mod transport;

pub use config::Config;
pub use error::{Error, Result};
pub use media_source::{AudSource, VidSource};
pub use session::{PeerCall, PeerEvent};
pub use signal::{SignalFrame, SignalKind};
pub use transport::SignalTransport;

/// Re-export the webrtc types callers commonly touch so `media::webrtc::...`
/// reads naturally.
pub mod webrtc {
    pub use webrtc_rs::peer_connection::peer_connection_state::RTCPeerConnectionState;
    pub use webrtc_rs::peer_connection::sdp::session_description::RTCSessionDescription;
    pub use webrtc_rs::track::track_local::track_local_static_sample::TrackLocalStaticSample;
    pub use webrtc_rs::track::track_local::TrackLocal;
    pub use webrtc_rs::track::track_remote::TrackRemote;
}

/// Re-export openh264 encoder helpers.
pub mod encoder {
    pub use openh264::encoder::Encoder;
    pub use openh264::Timestamp;
}
