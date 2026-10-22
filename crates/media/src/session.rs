//! [`PeerCall`] — one 1-to-1 WebRTC session.
//!
//! The calling app (`crates/network`) builds either the *offerer* or the
//! *answerer*, then drives signaling by sending the returned
//! [`SignalFrame`]s through a [`crate::SignalTransport`] and feeding inbound
//! frames into [`PeerCall::handle_inbound`].

use std::sync::{Arc, Mutex, OnceLock};

use webrtc_rs::api::APIBuilder;
use webrtc_rs::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc_rs::peer_connection::RTCPeerConnection;
use webrtc_rs::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc_rs::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc_rs::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc_rs::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc_rs::track::track_remote::TrackRemote;

use crate::config::Config;
use crate::error::Result;
use crate::media_source::{AudSource, VidSource, make_media_engine};
use crate::signal::{SignalFrame, SignalKind, VKind};
use crate::transport::SignalTransport;
use webrtc_rs::api::API;
use webrtc_rs::peer_connection::configuration::RTCConfiguration;

/// Lifecycle events the app can observe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerEvent {
    Connecting,
    Connected,
    Failed(String),
    Disconnected,
    Hangup,
}

impl PeerEvent {
    pub fn is_ended(self) -> bool {
        matches!(self, Self::Failed(_) | Self::Disconnected | Self::Hangup)
    }
}

type EventTx = async_broadcast::Sender<PeerEvent>;

#[derive(Clone)]
pub struct PeerCall {
    inner: Arc<Inner>,
}

struct Inner {
    call_id: String,
    /// This side's own identity (the libp2p base58 the app passes in). Used
    /// to stamp `SignalFrame.from` on every frame this peer sends, so the
    /// other side can display / correlate the sender.
    local_id: String,
    /// The *other* side's identity (peer's libp2p base58), learned from the
    /// `from` field of inbound signaling frames. Seeded on the answerer from
    /// the initial offer's `from`; updated as further stamped frames arrive.
    /// Empty for legacy peers that never stamp.
    remote_id: Mutex<String>,
    pc: Arc<RTCPeerConnection>,
    transport: Arc<dyn SignalTransport>,

    // Local outbound sources (owned for their lifetime).
    aud: Mutex<Option<Arc<AudSource>>>,
    cam: Mutex<Option<Arc<VidSource>>>,
    scr: Mutex<Option<Arc<VidSource>>>,

    event_tx: EventTx,
    // Remote (inbound) sinks.
    remote_audio: Mutex<Option<Box<dyn Fn(&[u8], u32, u32) + Send + Sync + 'static>>>,
    remote_cam: Mutex<Option<Box<dyn Fn(Vec<u8>, u32, u32) + Send + Sync + 'static>>>,
    remote_scr: Mutex<Option<Box<dyn Fn(Vec<u8>, u32, u32) + Send + Sync + 'static>>>,
}

impl Inner {
    pub fn upsert_video(&self, kind: VKind, src: Arc<VidSource>) {
        match kind {
            VKind::Cam => *self.cam.lock().unwrap() = Some(src),
            VKind::Scr => *self.scr.lock().unwrap() = Some(src),
        }
    }
    pub fn has_video(&self, kind: VKind) -> bool {
        match kind {
            VKind::Cam => self.cam.lock().unwrap().is_some(),
            VKind::Scr => self.scr.lock().unwrap().is_some(),
        }
    }
}

impl PeerCall {
    fn new(
        call_id: impl Into<String>,
        local_id: impl Into<String>,
        pc: Arc<RTCPeerConnection>,
        transport: Arc<dyn SignalTransport>,
    ) -> Self {
        let (event_tx, _rx) = async_broadcast::broadcast(64);
        let inner = Arc::new(Inner {
            call_id: call_id.into(),
            local_id: local_id.into(),
            remote_id: Mutex::new(String::new()),
            pc,
            transport,
            aud: Mutex::new(None),
            cam: Mutex::new(None),
            scr: Mutex::new(None),
            event_tx,
            remote_audio: Mutex::new(None),
            remote_cam: Mutex::new(None),
            remote_scr: Mutex::new(None),
        });
        Self { inner }
    }

    fn pc(&self) -> Arc<RTCPeerConnection> {
        self.inner.pc.clone()
    }

    /// Build a signaling frame for this call: pins `call_id` and stamps
    /// `from` with our local identity so the peer can display/correlate who
    /// sent it. Callers add any kind-specific metadata (upgrade fields,
    /// mode) afterwards.
    fn new_frame(&self, kind: SignalKind) -> SignalFrame {
        SignalFrame::new(self.call_id().to_string(), kind).with_from(&self.inner.local_id)
    }

    fn built_api(cfg: &Config) -> Result<(API, RTCConfiguration)> {
        let engine = make_media_engine()?;
        let mut ice = cfg.ice_servers();
        if ice.is_empty() {
            ice = vec![webrtc_rs::ice_transport::ice_server::RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".into()],
                ..Default::default()
            }];
        }
        let config = RTCConfiguration {
            ice_servers: ice,
            ..Default::default()
        };
        Ok((APIBuilder::new().with_media_engine(engine).build(), config))
    }

    /// Offerer: attach media, create + apply the local SDP, wait for ICE.
    ///
    /// `from` is *this* peer's identity (the app's libp2p base58); it is
    /// stamped onto the returned offer so the recipient learns who called.
    pub async fn offer(
        call_id: impl Into<String>,
        from: impl Into<String>,
        cfg: &Config,
        transport: Arc<dyn SignalTransport>,
        aud: Option<Arc<AudSource>>,
        cam: Option<Arc<VidSource>>,
        scr: Option<Arc<VidSource>>,
    ) -> Result<(PeerCall, SignalFrame)> {
        let (api, cfg_pc) = Self::built_api(cfg)?;
        let pc = Arc::new(api.new_peer_connection(cfg_pc).await?);
        let call = Self::new(call_id, from, pc.clone(), transport);
        call.set_local_sources(aud, cam, scr);
        call.add_local_tracks().await?;
        call.wire_event_handlers();

        let offer = pc.create_offer(None).await?;
        pc.set_local_description(offer.clone()).await?;
        let _ = pc.gathering_complete_promise().await;

        Ok((
            call.clone(),
            call.new_frame(SignalKind::Offer(offer.sdp.clone())),
        ))
    }

    /// Offerer side of a *mid-call* video upgrade. The caller pre-creates a
    /// [`VidSource`], hands it over; we attach it to a video transceiver and
    /// renegotiate on the same `RTCPeerConnection`, emitting the resulting
    /// `Offer` frame over the transport.
    ///
    /// We reuse the existing video-kind transceiver (via
    /// `sender.replace_track`) if one is already present — webrtc-rs `add_track`
    /// otherwise *adds* a brand-new transceiver every time the track id
    /// doesn't perfectly match an idle existing transceiver, which is what
    /// happens when alternating between cam (`chatx-cam`) and scr
    /// (`chatx-scr`). Unbounded transceiver growth would accumulate
    /// senders/SRTP writers per toggle.
    pub async fn upgrade_offer(
        &self,
        src: Arc<VidSource>,
        kind: VKind,
        both_sides: bool,
    ) -> Result<()> {
        // NOTE: we intentionally do NOT stop the other local source here. The
        // app reuses one `VidSource` per kind for the whole call (see
        // `local_source`), so stopping on every toggle would kill a source we
        // still want. Muting + stopping the capture device (done by the
        // caller) is enough to pause a kind.
        self.inner.upsert_video(kind, Arc::clone(&src));

        // Reuse the existing video transceiver if present; if not (first
        // upgrade on an audio-only call, or a call that started with the
        // *other* kind), fall back to a new transceiver.
        {
            let pc = self.pc();
            let video_track = src.track();
            let mut reused = false;
            let transceivers = pc.get_transceivers().await;
            for t in &transceivers {
                if t.kind() != RTPCodecType::Video {
                    continue;
                }
                let sender = t.sender().await;
                // It doesn't matter whether the sender currently has a track
                // attached; replace it. Restore sendrecv direction so the new
                // track actually transmits.
                if sender.replace_track(Some(video_track.clone())).await.is_ok() {
                    let _ = t.set_direction(RTCRtpTransceiverDirection::Sendrecv).await;
                    reused = true;
                    break;
                }
            }
            if !reused {
                pc.add_track(video_track).await?;
            }
        }

        let offer = self.pc().create_offer(None).await?;
        self.pc().set_local_description(offer.clone()).await?;
        let _ = self.pc().gathering_complete_promise().await;

        self.emit(
            self.new_frame(SignalKind::Offer(offer.sdp.clone())).with_upgrade(kind, both_sides),
        );
        Ok(())
    }

    /// Answerer side of a *mid-call* video upgrade. Applies the remote offer
    /// and (only if the peer asked us to also send our own track — i.e.
    /// `both_sides = true`) attaches the caller-supplied local track. The
    /// caller pre-creates the [`VidSource`] and registers it with their
    /// local capture dispatcher before calling this.
    pub async fn upgrade_answer(
        &self,
        offer: &SignalFrame,
        own_src: Option<Arc<VidSource>>,
    ) -> Result<()> {
        let (kind, both) = match (offer.upgrade_kind, offer.both_sides) {
            (Some(k), Some(b)) => (k, b),
            _ => {
                return Err(crate::error::Error::other(
                    "upgrade_answer: missing upgrade fields",
                ));
            }
        };
        self.learn_remote(&offer.from);
        if let Some(src) = own_src {
            // Only attach if we don't already have this kind (e.g. screen
            // recipient re-upgrading their camera is a no-op).
            if !self.inner.has_video(kind) {
                // Stop any *other* local video source so its encoder thread
                // doesn't outlive the toggle.
                if let Some(o) = self.inner.cam.lock().unwrap().clone() {
                    o.stop();
                }
                if let Some(o) = self.inner.scr.lock().unwrap().clone() {
                    o.stop();
                }

                self.inner.upsert_video(kind, Arc::clone(&src));

                // Reuse the existing video transceiver (via
                // `sender.replace_track`) if one is already present — avoids
                // accumulating transceivers across toggles.
                let pc = self.pc();
                let video_track = src.track();
                let mut reused = false;
                for t in pc.get_transceivers().await {
                    if t.kind() != RTPCodecType::Video {
                        continue;
                    }
                    let sender = t.sender().await;
                    if sender
                        .replace_track(Some(video_track.clone()))
                        .await
                        .is_ok()
                    {
                        let _ = t.set_direction(RTCRtpTransceiverDirection::Sendrecv).await;
                        reused = true;
                        break;
                    }
                }
                if !reused {
                    pc.add_track(video_track).await?;
                }
            }
        }
        let desc = RTCSessionDescription::offer(match &offer.kind {
            SignalKind::Offer(s) => s.clone(),
            _ => return Err(crate::error::Error::other("upgrade_answer: expected Offer")),
        })?;
        self.pc().set_remote_description(desc).await?;
        let answer = self.pc().create_answer(None).await?;
        self.pc().set_local_description(answer.clone()).await?;
        let _ = self.pc().gathering_complete_promise().await;

        self.emit(
            self.new_frame(SignalKind::Answer(answer.sdp.clone())).with_upgrade(kind, both),
        );
        Ok(())
    }

    /// Answerer: apply the `Offer`, create + apply the local SDP.
    ///
    /// `from` is *this* peer's identity (the app's libp2p base58). The
    /// remote peer's identity is learned from `offer.from` (if the sender
    /// stamped it) and exposed via [`PeerCall::remote_id`].
    pub async fn answer(
        offer: &SignalFrame,
        from: impl Into<String>,
        cfg: &Config,
        transport: Arc<dyn SignalTransport>,
        aud: Option<Arc<AudSource>>,
        cam: Option<Arc<VidSource>>,
        scr: Option<Arc<VidSource>>,
    ) -> Result<(PeerCall, SignalFrame)> {
        let sdp = match &offer.kind {
            SignalKind::Offer(s) => s.clone(),
            _ => return Err(crate::error::Error::other("answer: expected Offer")),
        };
        let (api, cfg_pc) = Self::built_api(cfg)?;
        let pc = Arc::new(api.new_peer_connection(cfg_pc).await?);
        let call = Self::new(
            offer.call_id.clone(),
            from,
            pc.clone(),
            transport,
        );
        call.learn_remote(&offer.from);
        call.set_local_sources(aud, cam, scr);
        call.add_local_tracks().await?;
        call.wire_event_handlers();

        pc.set_remote_description(RTCSessionDescription::offer(sdp)?)
            .await?;
        let answer = pc.create_answer(None).await?;
        pc.set_local_description(answer.clone()).await?;
        let _ = pc.gathering_complete_promise().await;

        Ok((
            call.clone(),
            call.new_frame(SignalKind::Answer(answer.sdp.clone())),
        ))
    }

    /// Update the cached *remote* identity when we receive a frame whose
    /// `from` is non-empty. We treat the most recent stamped value as
    /// authoritative because it is the only frame-level signal we have about
    /// "who the other side says they are."
    fn learn_remote(&self, from: &str) {
        if from.is_empty() {
            return;
        }
        let mut rx = self.inner.remote_id.lock().unwrap();
        if *rx != from {
            tracing::debug!(remote_id = %from, "learned remote identity");
            *rx = from.to_string();
        }
    }

    /// The other side's identity (empty until the first stamped frame
    /// arrives — see [`SignalFrame::from`] and [`Self::learn_remote`]).
    pub fn remote_id(&self) -> String {
        self.inner.remote_id.lock().unwrap().clone()
    }

    /// This side's identity as passed to `offer`/`answer`. Useful for
    /// tests and for the app to display "me" alongside the remote.
    pub fn local_id(&self) -> &str {
        &self.inner.local_id
    }

    // ── media ─────────────────────────────────────────────────────────

    fn set_local_sources(
        &self,
        aud: Option<Arc<AudSource>>,
        cam: Option<Arc<VidSource>>,
        scr: Option<Arc<VidSource>>,
    ) {
        *self.inner.aud.lock().unwrap() = aud;
        *self.inner.cam.lock().unwrap() = cam;
        *self.inner.scr.lock().unwrap() = scr;
    }

    /// Whether a local video track of this kind is currently attached.
    pub fn has_video(&self, kind: VKind) -> bool {
        self.inner.has_video(kind)
    }

    /// Mute / unmute a local video source (applied on this peer).
    pub fn set_video_mute(&self, kind: VKind, on: bool) {
        let src = match kind {
            VKind::Cam => self.inner.cam.lock().unwrap().clone(),
            VKind::Scr => self.inner.scr.lock().unwrap().clone(),
        };
        if let Some(src) = src {
            src.mute(on);
        }
    }

    /// Returns the [`VidSource`] the caller handed in for `kind` during
    /// `offer`/`answer`/`upgrade_offer` (or `None` if this call never
    /// attached a local video source of that kind). Use this instead of
    /// allocating a fresh source on every toggle — the source's `Arc` is
    /// pinned in `PeerCall.inner` for the call's life, creating new ones per
    /// toggle leaks the previous ones (encoder threads, last frames,
    /// webrtc-rs track bindings, especially when the peer never answers and
    /// `has_sent()==false` so `replace_track` skips `unbind`).
    pub fn local_source(&self, kind: VKind) -> Option<Arc<VidSource>> {
        match kind {
            VKind::Cam => self.inner.cam.lock().unwrap().clone(),
            VKind::Scr => self.inner.scr.lock().unwrap().clone(),
        }
    }

    /// Build + transmit a simple control signal (`Cam` / `Screen` on-off, or
    /// `MuteMic`) to the peer. The peer's `handle_inbound` observes it and
    /// updates its remote-video state. Non-Signaling: doesn't renegotiate.
    pub fn send_control(&self, kind: SignalKind) {
        self.emit(self.new_frame(kind));
    }

    async fn add_local_tracks(&self) -> Result<()> {
        if let Some(a) = self.inner.aud.lock().unwrap().clone() {
            self.pc().add_track(a.track()).await?;
        }
        if let Some(c) = self.inner.cam.lock().unwrap().clone() {
            self.pc().add_track(c.track()).await?;
        }
        if let Some(s) = self.inner.scr.lock().unwrap().clone() {
            self.pc().add_track(s.track()).await?;
        }
        Ok(())
    }

    pub fn set_remote_audio_sink(&self, cb: impl Fn(&[u8], u32, u32) + Send + Sync + 'static) {
        *self.inner.remote_audio.lock().unwrap() = Some(Box::new(cb));
    }
    pub fn set_remote_camera_sink(&self, cb: impl Fn(Vec<u8>, u32, u32) + Send + Sync + 'static) {
        *self.inner.remote_cam.lock().unwrap() = Some(Box::new(cb));
    }
    pub fn set_remote_screen_sink(&self, cb: impl Fn(Vec<u8>, u32, u32) + Send + Sync + 'static) {
        *self.inner.remote_scr.lock().unwrap() = Some(Box::new(cb));
    }

    // ── signaling ─────────────────────────────────────────────────────

    /// Apply an inbound signaling frame (sync, fire-and-forget for async ops).
    pub fn handle_inbound(&self, frame: &SignalFrame) -> Result<()> {
        // Learn / refresh our understanding of who the other side is, using
        // the sender-stamped identity on the frame. No-op for legacy peers
        // that never set `from`.
        self.learn_remote(&frame.from);
        let call = self.clone();
        match &frame.kind {
            SignalKind::Offer(sdp) => {
                let sdp = sdp.clone();
                let pc = self.pc();
                let desc = match RTCSessionDescription::offer(sdp) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(err = %e, "parse offer failed");
                        return Ok(());
                    }
                };
                runtime().spawn(async move {
                    if let Err(e) = pc.set_remote_description(desc).await {
                        tracing::warn!(err = %e, "set_remote(offer) failed");
                    }
                });
                Ok(())
            }
            SignalKind::Answer(sdp) => {
                let sdp = sdp.clone();
                let pc = self.pc();
                let desc = match RTCSessionDescription::answer(sdp) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(err = %e, "parse answer failed");
                        return Ok(());
                    }
                };
                runtime().spawn(async move {
                    if let Err(e) = pc.set_remote_description(desc).await {
                        tracing::warn!(err = %e, "set_remote(answer) failed");
                    }
                });
                Ok(())
            }
            SignalKind::Ice {
                candidate,
                sdp_mid,
                sdp_mline_index,
                ufrag,
            } => {
                let init = RTCIceCandidateInit {
                    candidate: candidate.clone(),
                    sdp_mid: Some(sdp_mid.clone()),
                    sdp_mline_index: Some(*sdp_mline_index),
                    username_fragment: Some(ufrag.clone()),
                };
                let pc = self.pc();
                runtime().spawn(async move {
                    if let Err(e) = pc.add_ice_candidate(init).await {
                        tracing::warn!(err = %e, "add_ice_candidate failed");
                    }
                });
                Ok(())
            }
            SignalKind::MuteMic(on) => {
                if let Some(a) = call.inner.aud.lock().unwrap().clone() {
                    a.mute(*on);
                }
                Ok(())
            }
            SignalKind::Cam(on) => {
                tracing::trace!(?on, "peer cam toggled");
                crate::fire_video_signal(VKind::Cam, *on);
                Ok(())
            }
            SignalKind::Screen(on) => {
                tracing::trace!(?on, "peer screen toggled");
                crate::fire_video_signal(VKind::Scr, *on);
                Ok(())
            }
            SignalKind::Bye => self.close(),
        }
    }

    fn emit(&self, frame: SignalFrame) {
        if let Err(e) = self.inner.transport.send(&frame) {
            tracing::warn!(err = %e, "signal send failed");
        }
    }

    // ── events / lifecycle ────────────────────────────────────────────

    pub fn events(&self) -> async_broadcast::Receiver<PeerEvent> {
        self.inner.event_tx.new_receiver()
    }

    pub fn peer_state(&self) -> RTCPeerConnectionState {
        self.pc().connection_state()
    }

    pub fn call_id(&self) -> &str {
        &self.inner.call_id
    }

    pub fn close(&self) -> Result<()> {
        // Connection gate off: nothing more may reach the webrtc track.
        crate::media_source::set_send_gate(false);
        // Stop the local video encoders (their dedicated `chatx-venc` threads
        // own an openh264 encoder + tokio runtime + last full-res frame).
        // Even if the caller has already stopped them (mid-call toggle),
        // `stop()` is idempotent.
        if let Some(s) = self.inner.cam.lock().unwrap().clone() {
            s.stop();
        }
        if let Some(s) = self.inner.scr.lock().unwrap().clone() {
            s.stop();
        }
        let pc = self.pc();
        runtime().spawn(async move {
            if let Err(e) = pc.close().await {
                tracing::warn!(err = %e, "close failed");
            }
        });
        let _ = self.inner.event_tx.try_broadcast(PeerEvent::Hangup);
        Ok(())
    }

    // ── wiring ────────────────────────────────────────────────────────

    fn wire_event_handlers(&self) {
        // State changes → event bus.
        {
            let pc = self.pc();
            let tx = self.inner.event_tx.clone();
            pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                let ev = match s {
                    RTCPeerConnectionState::New | RTCPeerConnectionState::Connecting => {
                        PeerEvent::Connecting
                    }
                    RTCPeerConnectionState::Connected => PeerEvent::Connected,
                    RTCPeerConnectionState::Failed => PeerEvent::Failed("state failed".into()),
                    RTCPeerConnectionState::Disconnected => PeerEvent::Disconnected,
                    _ => PeerEvent::Hangup,
                };
                // Connection gate: only let local media reach the webrtc track
                // once we're actually connected; stop again on any failure /
                // disconnect so nothing accumulates in the send pipeline.
                crate::media_source::set_send_gate(ev == PeerEvent::Connected);
                let _ = tx.try_broadcast(ev);
                Box::pin(async {})
            }));
        }

        // Local ICE candidates → out to the transport.
        {
            let call = self.clone();
            self.pc().on_ice_candidate(Box::new(move |cand| {
                if let Some(cand) = cand {
                    if let Ok(init) = cand.to_json() {
                        let frame = call.new_frame(SignalKind::Ice {
                            ufrag: init.username_fragment.unwrap_or_default(),
                            sdp_mid: init.sdp_mid.unwrap_or_default(),
                            sdp_mline_index: init.sdp_mline_index.unwrap_or(0),
                            candidate: init.candidate,
                        });
                        call.emit(frame);
                    }
                }
                Box::pin(async {})
            }));
        }

        // Inbound tracks → decode loop feeding the remote sinks.
        {
            let call = self.clone();
            self.pc()
                .on_track(Box::new(move |track: Arc<TrackRemote>, _, _| {
                    call.spawn_track_consumer(track);
                    Box::pin(async {})
                }));
        }
    }

    fn spawn_track_consumer(&self, track: Arc<TrackRemote>) {
        let is_audio = track.kind() == RTPCodecType::Audio;
        // Disambiguate camera vs screen on the video side by the local track's
        // stream id we chose when building it ("chatx-cam" vs "chatx-scr").
        let stream_id = track.stream_id();
        let remote_video = if stream_id.contains("scr") {
            RemoteVideo::Screen
        } else {
            RemoteVideo::Camera
        };
        let call = self.clone();
        runtime().spawn(async move {
            let mut acc = NalAcc::new();
            let mut buf: Vec<u8> = vec![0u8; 4096];
            loop {
                match track.read(&mut buf).await {
                    Ok((pkt, _)) => {
                        if is_audio {
                            let pcm = crate::media_source::mulaw_decode(&pkt.payload);
                            if let Some(cb) = call.inner.remote_audio.lock().unwrap().as_ref() {
                                cb(&pcm, 8_000, 1);
                            }
                        } else {
                            acc.push(&pkt.payload, pkt.header.timestamp);
                            if let Some((rgba, w, h)) = acc.drain_frame() {
                                match remote_video {
                                    RemoteVideo::Screen => {
                                        if let Some(cb) =
                                            call.inner.remote_scr.lock().unwrap().as_ref()
                                        {
                                            cb(rgba, w, h);
                                        }
                                    }
                                    RemoteVideo::Camera => {
                                        if let Some(cb) =
                                            call.inner.remote_cam.lock().unwrap().as_ref()
                                        {
                                            cb(rgba, w, h);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum RemoteVideo {
    Camera,
    Screen,
}

/// Minimal H.264 NAL frame accumulator (best-effort: coalesce payloads that
/// share an RTP timestamp, then hand the concatenated NALs to openh264).
struct NalAcc {
    ts: u32,
    acc: Vec<u8>,
    have: bool,
}

impl NalAcc {
    fn new() -> Self {
        Self {
            ts: 0,
            acc: Vec::new(),
            have: false,
        }
    }
    fn push(&mut self, payload: &[u8], ts: u32) {
        if self.have && ts != self.ts {
            self.acc.clear();
            self.have = false;
        }
        self.ts = ts;
        self.have = true;
        self.acc.extend_from_slice(payload);
    }
    fn drain_frame(&mut self) -> Option<(Vec<u8>, u32, u32)> {
        if self.acc.is_empty() {
            return None;
        }
        let data = std::mem::take(&mut self.acc);
        self.have = false;
        decode_h264(&data)
    }
}

fn decode_h264(nal: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    use openh264::decoder::Decoder;
    use openh264::nal_units;
    let mut dec = Decoder::new().ok()?;
    let mut last = None;
    for pkt in nal_units(nal) {
        // `decode` returns `Option<DecodedYUV>` — skip NALs that don't produce
        // a frame (e.g. SPS/PPS).
        if let Ok(Some(yuv)) = dec.decode(pkt) {
            let (uw, uh) = yuv.dimensions_uv();
            let (w, h) = (uw * 2, uh * 2);
            let mut rgba = vec![0u8; w * h * 4];
            yuv.write_rgba8(&mut rgba);
            last = Some((rgba, w as u32, h as u32));
        }
    }
    last
}

// ─── runtime ────────────────────────────────────────────────────────────────

static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Crate-shared tokio runtime. Reused by hot paths (e.g. `AudSource::push`)
/// instead of building a fresh runtime per call, which would burn CPU on the
/// audio thread (~48kHz).
pub(crate) fn shared_runtime() -> &'static tokio::runtime::Runtime {
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("media rt")
    })
}

fn runtime() -> &'static tokio::runtime::Runtime {
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio rt")
    })
}
