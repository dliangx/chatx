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

        let offer = self.pc().create_offer(None).await?;
        self.pc().set_local_description(offer.clone()).await?;
        let _ = self.pc().gathering_complete_promise().await;

        self.emit(
            self.new_frame(SignalKind::Offer(offer.sdp.clone()))
                .with_upgrade(kind, both_sides),
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
            self.new_frame(SignalKind::Answer(answer.sdp.clone()))
                .with_upgrade(kind, both),
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
        let call = Self::new(offer.call_id.clone(), from, pc.clone(), transport);
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

    /// True if the peer connection is currently `Connected`. Used by the app
    /// to compensate for the `async_broadcast` race where the `Connected`
    /// event is emitted (and dropped, no receiver yet) before the drain's
    /// receiver is created — common on the answerer side, since ICE can
    /// complete while the user is still looking at the Accept/Decline panel.
    pub fn is_connected(&self) -> bool {
        self.pc().connection_state() == RTCPeerConnectionState::Connected
    }

    pub fn call_id(&self) -> &str {
        &self.inner.call_id
    }

    /// Actively end the call from *this* side: notify the peer with a `Bye`
    /// frame over the transport, then tear down locally via [`Self::close`].
    ///
    /// Use this when the local user hangs up — the peer's `handle_inbound`
    /// observes the `Bye` and ends *its* side immediately. Relying on the
    /// raw `pc.close()` / ICE teardown alone leaves the peer hanging, because
    /// webrtc-rs only surfaces `Disconnected`/`Failed` well after the DTLS
    /// keepalive expires (tens of seconds, sometimes never).
    pub fn hangup(&self) -> Result<()> {
        // Emit the `Bye` *before* the local teardown so the transport and
        // PC are still alive to carry it.
        eprintln!("[call] hangup(): sending Bye to peer");
        match self.inner.transport.send(&self.new_frame(SignalKind::Bye)) {
            Ok(()) => eprintln!("[call] hangup(): Bye accepted by transport"),
            Err(e) => eprintln!("[call] hangup(): Bye send FAILED: {e}"),
        }
        self.close()
    }

    /// Local-only teardown: gate off outbound media, stop encoders, close the
    /// `RTCPeerConnection`, and broadcast `Hangup`. Does NOT send a `Bye` —
    /// the receiver of a remote `Bye` (and any idempotent re-invocation)
    /// calls this so it doesn't echo a `Bye` back into a signaling loop.
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
                tracing::trace!(?s, "peer-connection state change");
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
        eprintln!(
            "[media] on_track: kind={:?} stream_id={stream_id}",
            track.kind()
        );
        runtime().spawn(async move {
            // One LONG-LIVED H.264 stateful decoder per track. `openh264::Decoder`
            // keeps motion/reference state across `decode` calls, which is
            // required for P/B frames (which are >99% of a typical stream).
            // Creating a fresh decoder per frame — the old approach — silently
            // dropped every non-IDR frame so only keyframes rendered.
            //
            // We feed each RTP packet's NALs straight through the same decoder
            // in arrival order. SPS/PPS arrive ahead of their IDR frame and are
            // consumed (they return `None`); subsequent P/B NALs decode to
            // pictures that we forward to the remote sink.
            let mut decoder = openh264::decoder::Decoder::new().expect("h264 decoder alloc");
            let mut depack = H264DePacketizer::new();
            let mut buf: Vec<u8> = vec![0u8; 8192];
            // let mut frames_decoded = 0u64;
            loop {
                match track.read(&mut buf).await {
                    Ok((pkt, _)) => {
                        if is_audio {
                            let pcm = crate::media_source::mulaw_decode(&pkt.payload);
                            if let Some(cb) = call.inner.remote_audio.lock().unwrap().as_ref() {
                                cb(&pcm, 8_000, 1);
                            }
                        } else {
                            // Reassemble the raw RTP payload into full Annex-B
                            // NAL unit(s), then feed the stateful decoder.
                            let annexb = depack.depacketize(&pkt.payload);
                            if !annexb.is_empty() {
                                if let Some((rgba, w, h)) = decode_h264_seq(&mut decoder, &annexb) {
                                    // frames_decoded += 1;
                                    // if frames_decoded <= 3 || frames_decoded % 60 == 0 {
                                    //     eprintln!(
                                    //         "[media] video decoded #{frames_decoded} {w}x{h} ({remote_video:?})",
                                    //     );
                                    // }
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
                    }
                    Err(_) => break,
                }
            }
        });
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum RemoteVideo {
    Camera,
    Screen,
}

// ── H.264 over RTP (RFC 6184) depacketization + stateful decode ────────────

/// Reassembles H.264 NAL units from raw RTP video payloads (RFC 6184).
///
/// webrtc-rs hands us the *raw* RTP payload (no depacketizer in its receive
/// path), so a video frame is split across packets depending on size:
///   * Single NAL Unit (type 1..=23) — whole NAL in one packet;
///   * STAP-A (type 24) — several small NALs (e.g. SPS+PPS) in one packet;
///   * FU-A (type 28) — one large NAL split across many packets.
///
/// We rebuild full Annex-B NAL units (with `00 00 00 01` start codes) so a
/// single *long-lived* [`openh264::Decoder`] (which is stateful and thus
/// retains the motion reference data P/B frames need) can decode them in
/// order. This is what finally makes remote video appear — the earlier code
/// fed raw payloads straight to `openh264::nal_units`, which requires Annex-B
/// start codes that RTP payloads don't carry, so it decoded *zero* frames.
struct H264DePacketizer {
    /// In-flight FU-A fragment buffer (accumulated until the FU-A End flag).
    fua: Vec<u8>,
    fua_active: bool,
}

impl H264DePacketizer {
    const NALU_TYPE_BITMASK: u8 = 0x1F;
    const NALU_REF_IDC_BITMASK: u8 = 0x60;
    const FU_START_BITMASK: u8 = 0x80;
    const FU_END_BITMASK: u8 = 0x40;
    const STAPA_NALU_TYPE: u8 = 24;
    const FUA_NALU_TYPE: u8 = 28;
    const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

    fn new() -> Self {
        Self {
            fua: Vec::new(),
            fua_active: false,
        }
    }

    /// Feed one RTP video payload. Returns the rebuilt Annex-B NAL unit(s)
    /// carried by this packet (empty for non-terminal FU-A fragments or
    /// NAL types we drop, e.g. AUD/filler).
    fn depacketize(&mut self, packet: &[u8]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        if packet.len() < 1 {
            return out;
        }
        let b0 = packet[0];
        let nalu_type = b0 & Self::NALU_TYPE_BITMASK;

        match nalu_type {
            // Single NAL Unit — already a complete NAL.
            1..=23 => {
                out.extend_from_slice(&Self::START_CODE);
                out.extend_from_slice(packet);
            }
            // STAP-A — multiple NAL Units concatenated.
            Self::STAPA_NALU_TYPE => {
                let mut off = 1; // skip STAP-A type byte
                while off + 2 <= packet.len() {
                    let nalu_size = ((packet[off] as usize) << 8) | packet[off + 1] as usize;
                    off += 2;
                    if off + nalu_size > packet.len() {
                        break; // malformed / truncated
                    }
                    out.extend_from_slice(&Self::START_CODE);
                    out.extend_from_slice(&packet[off..off + nalu_size]);
                    off += nalu_size;
                }
            }
            // FU-A — one NAL split across packets.
            Self::FUA_NALU_TYPE => {
                if packet.len() < 2 {
                    return out;
                }
                let b1 = packet[1];
                if (b1 & Self::FU_START_BITMASK) != 0 {
                    // Start flag: reset any stale fragment, begin a new one.
                    self.fua.clear();
                    self.fua_active = true;
                }
                if !self.fua_active {
                    // Middle fragment with no start — drop it (desync).
                    return out;
                }
                // Rebuild the NAL header: ref_idc | type.
                if self.fua.is_empty() {
                    let nalu_header =
                        (b0 & Self::NALU_REF_IDC_BITMASK) | (b1 & Self::NALU_TYPE_BITMASK);
                    self.fua.push(nalu_header);
                }
                // Append this fragment's payload (after the 2-byte FU header).
                self.fua.extend_from_slice(&packet[2..]);

                if (b1 & Self::FU_END_BITMASK) != 0 {
                    // End fragment — emit the full NAL.
                    out.extend_from_slice(&Self::START_CODE);
                    out.append(&mut self.fua);
                    self.fua_active = false;
                }
            }
            // AUD (9), filler (12), STAP-B (25), FU-B (29), reserved — drop.
            _ => {}
        }
        out
    }
}

/// Split an Annex-B stream into its NAL units and feed each — in order —
/// through a long-lived stateful [`openh264::Decoder`]. Returns the last
/// picture produced (RGBA8, w, h), if any (SPS/PPS-only NAL sets return
/// `None`, which is normal).
fn decode_h264_seq(
    dec: &mut openh264::decoder::Decoder,
    annexb: &[u8],
) -> Option<(Vec<u8>, u32, u32)> {
    let mut last = None;
    for nal in openh264::nal_units(annexb) {
        if let Ok(Some(yuv)) = dec.decode(nal) {
            let (uw, uh) = yuv.dimensions_uv();
            let (w, h) = (uw * 2, uh * 2);
            let mut rgba = vec![0u8; w * h * 4];
            yuv.write_rgba8(&mut rgba);
            last = Some((rgba, w as u32, h as u32));
        }
    }
    last
}

#[cfg(test)]
mod h264_depacketize_tests {
    use super::H264DePacketizer;

    const START: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

    /// A single-NAL packet (e.g. an IDR slice) must pass through verbatim
    /// with an Annex-B start code prepended.
    #[test]
    fn single_nal() {
        let mut d = H264DePacketizer::new();
        let nalu = [0x25u8, 0xAA, 0xBB, 0xCC]; // NRI 0b01 | type 5 (IDR)
        let out = d.depacketize(&nalu);
        assert_eq!(out[..4], START);
        assert_eq!(&out[4..], &nalu[..]);
    }

    /// STAP-A carrying two NALs must yield two independent start-coded units.
    #[test]
    fn stap_a() {
        let mut d = H264DePacketizer::new();
        let stap = [
            0x18u8, // type 24 (STAP-A), lower 5 bits = 11000
            0x00, 0x02, // NALU1 size = 2
            0x88, 0x88, // NALU1 body
            0x00, 0x03, // NALU2 size = 3
            0xC8, 0xC8, 0xC8, // NALU2 body
        ];
        let out = d.depacketize(&stap);
        assert_eq!(out.len(), 4 + 2 + 4 + 3);
        assert_eq!(out[..4], START);
        assert_eq!(&out[4..6], &[0x88, 0x88]);
        assert_eq!(out[6..10], START);
        assert_eq!(&out[10..13], &[0xC8, 0xC8, 0xC8]);
    }

    /// A NAL fragmented across three FU-A packets must reassemble into one
    /// complete unit with a reconstructed header (NRI from FU header, type
    /// from FU indicator).
    #[test]
    fn fu_a_roundtrip() {
        let mut d = H264DePacketizer::new();
        // b0 = FU-A(28) | NRI; b1 holds the F(0x80) / E(0x40) flags + type(5).
        let s1 = [0x3C, 0x85, 0xAA]; // start only (0x80) | type 5
        let s2 = [0x3C, 0x05, 0xBB]; // middle (no flags)
        let s3 = [0x3C, 0x45, 0xCC, 0xDD]; // end (0x40) | type 5
        assert!(d.depacketize(&s1).is_empty());
        assert!(d.depacketize(&s2).is_empty());
        let out = d.depacketize(&s3);
        // start(4) + header(1) + body(4) = 9 bytes.
        assert_eq!(out.len(), 9);
        assert_eq!(out[..4], START);
        // Reconstructed NAL header: NRI(0x20) | type(5) = 0x25.
        assert_eq!(out[4], 0x25);
        assert_eq!(&out[5..], &[0xAA, 0xBB, 0xCC, 0xDD]);
    }

    /// A middle fragment arriving with no start in flight is dropped
    /// (desync recovery) rather than corrupting the buffer.
    #[test]
    fn fu_a_stray_middle_dropped() {
        let mut d = H264DePacketizer::new();
        let stray = [0x3C, 0x05, 0xFF];
        assert!(d.depacketize(&stray).is_empty());
    }
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
