//! [`PeerCall`] — one 1-to-1 WebRTC session.
//!
//! The calling app (`crates/network`) builds either the *offerer* or the
//! *answerer*, then drives signaling by sending the returned
//! [`SignalFrame`]s through a [`crate::SignalTransport`] and feeding inbound
//! frames into [`PeerCall::handle_inbound`].

use std::sync::{Arc, Mutex, OnceLock};

use webrtc_rs::api::APIBuilder;
use webrtc_rs::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc_rs::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc_rs::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc_rs::peer_connection::RTCPeerConnection;
use webrtc_rs::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc_rs::track::track_remote::TrackRemote;

use crate::config::Config;
use crate::error::Result;
use crate::media_source::{make_media_engine, AudSource, VidSource};
use crate::signal::{SignalFrame, SignalKind};
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
    from: String,
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

impl PeerCall {
    fn new(
        call_id: impl Into<String>,
        from: impl Into<String>,
        pc: Arc<RTCPeerConnection>,
        transport: Arc<dyn SignalTransport>,
    ) -> Self {
        let (event_tx, _rx) = async_broadcast::broadcast(64);
        let inner = Arc::new(Inner {
            call_id: call_id.into(),
            from: from.into(),
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

        let cid = call.call_id().to_string();
        Ok((
            call,
            SignalFrame::new(cid, SignalKind::Offer(offer.sdp.clone())),
        ))
    }

    /// Answerer: apply the `Offer`, create + apply the local SDP.
    pub async fn answer(
        offer: &SignalFrame,
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
        let call = Self::new(offer.call_id.clone(), offer.from.clone(), pc.clone(), transport);
        call.set_local_sources(aud, cam, scr);
        call.add_local_tracks().await?;
        call.wire_event_handlers();

        pc.set_remote_description(RTCSessionDescription::offer(sdp)?)
        .await?;
        let answer = pc.create_answer(None).await?;
        pc.set_local_description(answer.clone()).await?;
        let _ = pc.gathering_complete_promise().await;

        Ok((
            call,
            SignalFrame::new(
                offer.call_id.clone(),
                SignalKind::Answer(answer.sdp.clone()),
            ),
        ))
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
            SignalKind::Cam(on) | SignalKind::Screen(on) => {
                tracing::trace!(?on, "video toggled");
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
                        let frame = SignalFrame::new(
                            call.call_id().to_string(),
                            SignalKind::Ice {
                                ufrag: init.username_fragment.unwrap_or_default(),
                                sdp_mid: init.sdp_mid.unwrap_or_default(),
                                sdp_mline_index: init.sdp_mline_index.unwrap_or(0),
                                candidate: init.candidate,
                            },
                        );
                        call.emit(frame);
                    }
                }
                Box::pin(async {})
            }));
        }

        // Inbound tracks → decode loop feeding the remote sinks.
        {
            let call = self.clone();
            self.pc().on_track(Box::new(move |track: Arc<TrackRemote>, _, _| {
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
        Self { ts: 0, acc: Vec::new(), have: false }
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

fn runtime() -> &'static tokio::runtime::Runtime {
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio rt")
    })
}
