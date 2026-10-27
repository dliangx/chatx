//! Realtime call session over WebRTC (voice / camera / screen share).
//!
//! One active call at a time. The `media` crate owns the webrtc-rs
//! `PeerConnection` + RTP pipeline; `chatx-core` owns the libp2p channel.
//! This module sits between them:
//!
//! 1. Implements `media::SignalTransport` by serializing each `SignalFrame`
//!    to JSON and calling `Client::send_webrtc_signal`.
//! 2. Drives `PeerCall::offer` / `PeerCall::answer` on a dedicated tokio
//!    runtime (the `media` crate's `OnceLock` global), with a local `Audio`
//!    (cpal) + `AudSource` / `VidSource`(×2) attached.
//! 3. Wires the **mic** (cpal input thread) → `AudSource` and
//!    **remote-audio** (webrtc read thread) → cpal speaker queue, so the
//!    audio pipeline is closed.
//! 4. Installs `PeerCall::set_remote_camera_sink` / `set_remote_screen_sink`
//!    that push each decoded frame to the Slint event loop, which routes it
//!    to the `CallState.remote-camera` / `AppState.remote-screen` images.
//!
//! The local camera / screen capture sinks are installed globally at
//! app-start (`call_dispatcher::on_camera_frame` / `on_screen_frame`).
//! They forward each frame both to the active call's `VidSource` and to the
//! QR scanner hook (in parallel with an active call).

use std::cell::RefCell;
use std::sync::{Arc, Mutex, OnceLock};

pub use crate::call_dispatcher;
use crate::{AppState, CallState, GlobalOverlayType, MainWindow};
use chatx_core::Client;
use chatx_core::signal::HttpDirectory;
use media::{
    AudSource, CallMode, Config as MediaConfig, PeerCall, SignalFrame, SignalKind, SignalTransport,
    VidSource,
};
use slint::ComponentHandle;
use slint::SharedString;

pub type ChatxClient = Client<HttpDirectory>;

// ── SignalTransport adapter (media crate ↔ libp2p) ────────────────────────

struct Lp2pSignal {
    client: Arc<ChatxClient>,
    peer: libp2p::PeerId,
}

impl SignalTransport for Lp2pSignal {
    fn send(&self, frame: &SignalFrame) -> media::Result<()> {
        // A `Bye` is the one signaling frame that must *reliably* reach the
        // peer. During an active call the media rides WebRTC's own DTLS socket,
        // so the libp2p relay circuit that carries signaling idles and is
        // typically reclaimed by the time we hang up — and a `send_request`
        // with no live route (no fresh address) silently fails. That is why
        // offer/answer/ICE got through (each was preceded by a `dial_peer` at
        // call setup) but the hang-up `Bye` did not. Re-dial first to refresh
        // the peer's address + circuit, so the `Bye` actually has a route.
        if matches!(frame.kind, SignalKind::Bye) {
            let base58 = self.peer.to_base58();
            tracing::debug!("[media] Bye → {base58}; re-dialing to refresh route");
            if let Err(e) = self.client.dial_peer(&base58) {
                tracing::warn!("[media] pre-Bye dial failed (still sending): {e}");
            }
        }
        let json = serde_json::to_string(frame)
            .map_err(|e| media::Error::other(format!("serialize SignalFrame: {e}")))?;
        self.client
            .send_webrtc_signal(self.peer.clone(), &json)
            .map_err(|e| media::Error::other(format!("send_webrtc: {e}")))?;
        Ok(())
    }
}

fn as_dyn(t: Arc<Lp2pSignal>) -> Arc<dyn SignalTransport> {
    t as Arc<dyn SignalTransport>
}

// ── call state ────────────────────────────────────────────────────────────

struct CallInner {
    peer: libp2p::PeerId,
    call: PeerCall,
    /// Lives so the cpal input/output streams stay open. Dropped on stop.
    audio: audio::Audio,
    #[allow(dead_code)]
    aud_src: Arc<AudSource>,
    #[allow(dead_code)]
    cam_src: Option<Arc<VidSource>>,
    #[allow(dead_code)]
    scr_src: Option<Arc<VidSource>>,
    #[allow(dead_code)]
    call_id: String,
}

thread_local! {
    static CALL: RefCell<Option<CallInner>> = RefCell::new(None);
}

/// One inbound offer waiting for the user's decision. Stashed in the
/// process-global `RINGING` slot when it arrives; consumed either by `accept`
/// (user tapped ACCEPT) or `decline` (user tapped DECLINE) / `timeout`.
/// Only one at a time — the second offer replaces the first.
struct PendingInbound {
    client: Arc<ChatxClient>,
    peer: libp2p::PeerId,
    offer: SignalFrame,
    opts: CallOptions,
    call_id: String,
}

#[derive(Default)]
struct RingingSlot {
    pending: Option<PendingInbound>,
    /// Bumped on every new inbound offer. A timeout task only acts if the
    /// slot's generation still matches the one it captured when it started —
    /// this avoids a stale timeout (from a previous, already-acted-upon
    /// offer) clobbering a newer one.
    generation: u64,
}

static RINGING: std::sync::LazyLock<std::sync::Mutex<RingingSlot>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(RingingSlot::default()));

/// Weak reference to the Slint `MainWindow`. Set once at app startup.
static WINDOW: OnceLock<slint::Weak<MainWindow>> = OnceLock::new();

/// Install the `MainWindow` handle once; safe to call multiple times.
pub fn set_weak_window(weak: slint::Weak<MainWindow>) {
    let _ = WINDOW.set(weak);
}

/// Return the installed weak window handle (or `None` if not yet set).
pub fn weak_window() -> Option<slint::Weak<MainWindow>> {
    WINDOW.get().cloned()
}

/// Called (from the webrtc event thread) when the peer ends the call. The
/// teardown must run on the UI thread — the `CALL` thread_local + cpal
/// streams are owned there — so hop the whole thing over. Idempotent: safe
/// to run even if we are also tearing down from the "end call" button.
fn fire_ui_end() {
    if let Some(weak) = weak_window() {
        let _ = slint::invoke_from_event_loop(move || {
            // Passive side: clean up locally, do NOT echo a `Bye` back.
            stop_call_no_notify();
            if let Some(ui) = weak.upgrade() {
                let cs = ui.global::<CallState>();
                cs.set_active(false);
                cs.set_ringing(false);
                cs.set_connected(false);
                cs.set_muted(false);
                cs.set_audio_muted(false);
                cs.set_cam_on(false);
                cs.set_scr_on(false);
                cs.set_my_cam(slint::Image::default());
                cs.set_my_scr(slint::Image::default());
                cs.set_remote_cam(slint::Image::default());
                cs.set_remote_scr(slint::Image::default());
                cs.set_has_my_cam(false);
                cs.set_has_my_scr(false);
                cs.set_has_remote_cam(false);
                cs.set_has_remote_scr(false);
                cs.set_swapped(false);
                cs.set_minimized(false);
                let state = ui.global::<crate::AppState>();
                let mut nav = state.get_nav_state();
                nav.global_overlay = crate::GlobalOverlayType::None;
                state.set_nav_state(nav);
            }
        });
    }
}

/// Per-call generation. Bumped on every teardown (`stop_common`). Remote-
/// camera / remote-screen / local-preview sink closures capture the value
/// at install time; if it no longer matches, the frame is dropped. This
/// kills the race where a frame already in-flight on the webrtc read thread
/// (or camera / screen capture thread) lands on the Slint event loop AFTER
/// our teardown reset — re-populating the `CallState` image slots with the
/// peer's full-resolution pixel buffer (and a freshly-allocated local
/// preview copy) even though the call is over.
static CALL_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `g` captured at install time is still current?
fn gen_live(g: u64) -> bool {
    CALL_GEN.load(std::sync::atomic::Ordering::Acquire) == g
}

// ── public: active / peer ─────────────────────────────────────────────────

pub fn is_active() -> bool {
    CALL.with(|slot| slot.borrow().is_some())
}

pub fn active_peer_base58() -> Option<String> {
    CALL.with(|slot| slot.borrow().as_ref().map(|c| c.peer.to_base58()))
}

pub fn active_call() -> Option<PeerCall> {
    CALL.with(|slot| slot.borrow().as_ref().map(|c| c.call.clone()))
}

// ── public: start (offerer) ───────────────────────────────────────────────

/// The optional local video sources we want in this call.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct CallOptions {
    pub include_cam: bool,
    pub include_scr: bool,
}

/// Open a call (offerer side). `opts` decides which local media to attach.
/// The offer SDP is sent through the transport immediately; the peer responds
/// via [`handle_signal_offer`](Self::handle_signal_offer) (inbound
/// `ChatEvent::Webrtc`).
pub fn start_call(
    client: Arc<ChatxClient>,
    peer_base58: &str,
    opts: CallOptions,
) -> anyhow::Result<()> {
    if is_active() {
        stop_call();
    }

    let peer = client
        .dial_peer(peer_base58)
        .map_err(|e| anyhow::anyhow!("dial peer '{peer_base58}': {e}"))?;

    let (audio, queue, aud_src, cam_src, scr_src) = bootstrap_media(opts)?;

    let transport = Arc::new(Lp2pSignal {
        client: Arc::clone(&client),
        peer: peer.clone(),
    });
    let release_on_err = |e: anyhow::Error| -> anyhow::Error {
        release_bootstrap(&audio, cam_src.as_ref(), scr_src.as_ref());
        e
    };
    let call_id = format!("call-{:x}", now_nanos());
    let cfg = MediaConfig::from_env();
    let call_id_c = call_id.clone();
    let aud2 = Arc::clone(&aud_src);
    let cam2 = cam_src.as_ref().cloned();
    let scr2 = scr_src.as_ref().cloned();
    let me58 = client.peer_base58();
    let me58c = me58.clone();

    let (call, offer) = match {
        let t = Arc::clone(&transport);
        media_rt().block_on(async move {
            PeerCall::offer(
                call_id_c.clone(),
                me58c,
                &cfg,
                as_dyn(t),
                Some(aud2),
                cam2,
                scr2,
            )
            .await
        })
    } {
        Ok(ot) => ot,
        Err(e) => return Err(release_on_err(anyhow::anyhow!("PeerCall::offer: {e}"))),
    };

    // Tag the initial offer with our call intent so the answerer can mirror
    // it (a video-offer and screen-offer are otherwise identical in SDP).
    let mode = if opts.include_cam {
        CallMode::Video
    } else if opts.include_scr {
        CallMode::Screen
    } else {
        CallMode::Audio
    };
    let offer = offer.with_mode(mode);

    // `PeerCall::offer` returns the offer SDP frame; we are responsible for
    // sending it to the peer over the transport. (The media crate doesn't
    // auto-transmit; `Lp2pSignal::send` is invoked per frame.)
    if let Err(e) = transport.send(&offer) {
        return Err(release_on_err(anyhow::anyhow!("transport send: {e}")));
    }

    wire_remote_sinks(&call, &audio, &queue);

    // Local camera preview (offerer started a video call). Screen share does
    // NOT install a local `my-scr` preview — the UI shows only the remote
    // peer; see `bootstrap_media` caller comments. The outgoing screen encode
    // to the peer runs through the separate `VidSource` slot.
    if opts.include_cam {
        install_local_cam_preview();
    }
    spawn_event_drain(call.clone());

    CALL.with(|slot| {
        *slot.borrow_mut() = Some(CallInner {
            peer: peer.clone(),
            call: call.clone(),
            audio,
            aud_src,
            cam_src,
            scr_src,
            call_id,
        });
    });

    tracing::debug!(
        "[call] offerer started: peer={peer} cam={} scr={}",
        opts.include_cam, opts.include_scr
    );
    Ok(())
}

// ── public: answer (inbound offer) ────────────────────────────────────────

/// Answer an inbound `Offer`. Called from
/// [`handle_signal_offer`](Self::handle_signal_offer).
pub fn answer_offer(
    client: Arc<ChatxClient>,
    peer: libp2p::PeerId,
    offer: &SignalFrame,
    opts: CallOptions,
) -> anyhow::Result<()> {
    if is_active() {
        stop_call();
    }
    let (audio, queue, aud_src, cam_src, scr_src) = bootstrap_media(opts)?;

    let transport = Arc::new(Lp2pSignal {
        client: Arc::clone(&client),
        peer: peer.clone(),
    });
    let release_on_err = |e: anyhow::Error| -> anyhow::Error {
        release_bootstrap(&audio, cam_src.as_ref(), scr_src.as_ref());
        e
    };
    let offer_c = offer.clone();
    let cfg = MediaConfig::from_env();
    let aud2 = Arc::clone(&aud_src);
    let cam2 = cam_src.as_ref().cloned();
    let scr2 = scr_src.as_ref().cloned();
    let me58 = client.peer_base58();
    let me58c = me58.clone();

    let (call, answer) = match {
        let t = Arc::clone(&transport);
        media_rt().block_on(async move {
            PeerCall::answer(&offer_c, me58c, &cfg, as_dyn(t), Some(aud2), cam2, scr2).await
        })
    } {
        Ok(ot) => ot,
        Err(e) => return Err(release_on_err(anyhow::anyhow!("PeerCall::answer: {e}"))), // keep (answerer path)
    };

    // Send our answer SDP back to the peer.
    if let Err(e) = transport.send(&answer) {
        return Err(release_on_err(anyhow::anyhow!("transport send: {e}")));
    }

    wire_remote_sinks(&call, &audio, &queue);

    // Local camera preview (offerer started a video call). Screen share does
    // NOT install a local `my-scr` preview — the UI shows only the remote
    // peer; the outgoing encode goes through the separate `VidSource` slot.
    if opts.include_cam {
        install_local_cam_preview();
    }
    spawn_event_drain(call.clone());

    let call_id = offer.call_id.clone();
    CALL.with(|slot| {
        *slot.borrow_mut() = Some(CallInner {
            peer,
            call,
            audio,
            aud_src,
            cam_src,
            scr_src,
            call_id,
        });
    });

    // ── push the call UI to the user ────────────────────────────────────
    // The outbound path (`begin_voice_call` / `begin_video_call` /
    // `begin_screen_call` in lib.rs) does this right after `start_call`
    // returns OK. The inbound path was missing this — the call was
    // running with no UI overlay, so the user could not tell a call was
    // in progress. Mirror the mapping: offer mode → overlay type.
    if let Some(weak) = weak_window() {
        let peer_b58 = peer.to_base58();
        let (peer_b58, peer_name) = crate::resolve_peer_display(&peer_b58);
        let overlay = match offer.mode {
            Some(CallMode::Video) => crate::GlobalOverlayType::VideoCall,
            Some(CallMode::Screen) => crate::GlobalOverlayType::ScreenShare,
            Some(CallMode::Audio) | None => crate::GlobalOverlayType::AudioCall,
        };
        crate::publish_call_state(weak.clone(), Some(peer_b58), peer_name, true);
        if opts.include_cam {
            let wk = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = wk.upgrade() {
                    ui.global::<CallState>().set_cam_on(true);
                }
            });
        }
        crate::show_call_overlay(weak, overlay);
    }
    tracing::debug!("[call] answerer started (ui=on)");
    Ok(())
}

// ── public: inbound signaling ─────────────────────────────────────────────

/// Entry point for `ChatEvent::Webrtc`. `json` is the JSON-encoded
/// `media::SignalFrame` produced by the peer's `Lp2pSignal::send`.
pub fn handle_signal_offer(client: Arc<ChatxClient>, peer: libp2p::PeerId, json: &str) {
    // tracing::debug!("[call] inbound webrtc signal: len={}", json.len());
    let frame: SignalFrame = match serde_json::from_str(json) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("[call] bad SignalFrame: {e}");
            return;
        }
    };
    tracing::debug!("[call] inbound {:?} call_id={}", frame.kind, frame.call_id);
    match &frame.kind {
        SignalKind::Offer(_) => {
            if is_active() {
                if frame.upgrade_kind.is_some() {
                    // Mid-call upgrade (e.g. an audio → video / audio → screen
                    // answerer receiving the offer, or a screen recipient that
                    // has just decided to also open their own camera). Route
                    // to the answerer upgrade path.
                    apply_incoming_upgrade(&frame);
                    return;
                }
                tracing::warn!("[call] got Offer while active — dropping (not an upgrade)");
                return;
            }
            // Stash the offer in the ringing slot and surface the UI.
            // `accept_inbound` / `decline_inbound` (called from the Slint
            // callbacks) actually drive the WebRTC answer.
            let opts = match frame.mode {
                Some(CallMode::Video) => CallOptions {
                    include_cam: true,
                    include_scr: false,
                },
                Some(CallMode::Screen) => CallOptions {
                    include_cam: false,
                    include_scr: false,
                }, // recipient doesn't share; sharer is the offerer
                Some(CallMode::Audio) | None => CallOptions {
                    include_cam: false,
                    include_scr: false,
                },
            };
            ring_inbound(client, peer, frame, opts);
        }
        _ => {
            if matches!(frame.kind, SignalKind::Bye) {
                // Peer hung up. `handle_signal_offer` already runs on the UI
                // thread (it is dispatched via `slint::invoke_from_event_loop`),
                // so we can tear down *synchronously* right here. This removes
                // the dependency on the fragile async chain that was silently
                // losing the passive-side teardown:
                //   handle_inbound → close → try_broadcast(Hangup) → drain task
                //   (on media_rt) → fire_ui_end → stop_call
                // — any link dying (drain task not alive, broadcast dropped)
                // left B's UI frozen exactly as observed. The synchronous path
                // below is airtight. We still keep `handle_inbound` available for
                // other kinds (MuteMic, Cam, Screen) — for those the state is
                // applied on the local PeerCall, no UI mutation is required.
                tracing::debug!(
                    "[call] Bye from peer: active={} → tearing down synchronously",
                    is_active()
                );
                if is_active() {
                    fire_ui_end();
                } else {
                    // We were waiting on an inbound offer (ringing) and the peer
                    // hung up / cancelled before we answered. Dismiss the panel
                    // without sending a `Bye` back (the call was theirs).
                    dismiss_inbound();
                }
                return;
            }
            if let Some(call) = active_call() {
                if let Err(e) = call.handle_inbound(&frame) {
                    tracing::debug!("[call] handle_inbound: {e}");
                }
            }
        }
    }
}

/// Clear a pending inbound offer + its ringing UI WITHOUT notifying the peer.
/// Used when the offer's originating side ends the call first (e.g. the
/// caller hung up while we were still at the Accept/Decline screen).
fn dismiss_inbound() {
    let was_pending = {
        let mut slot = RINGING.lock().unwrap();
        slot.generation += 1;
        slot.pending.take().is_some()
    };
    if !was_pending {
        return;
    }
    if let Some(weak) = weak_window() {
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let st = ui.global::<CallState>();
            st.set_ringing(false);
            st.set_active(false);
            let state = ui.global::<AppState>();
            let mut nav = state.get_nav_state();
            nav.global_overlay = GlobalOverlayType::None;
            state.set_nav_state(nav);
        });
    }
}

/// Stash an inbound offer and present the ringing UI. Replaces any
/// existing pending inbound (only one at a time).
fn ring_inbound(
    client: Arc<ChatxClient>,
    peer: libp2p::PeerId,
    offer: SignalFrame,
    opts: CallOptions,
) {
    let call_id = offer.call_id.clone();
    let mode = offer.mode; // copy out before moving into the slot

    {
        let mut slot = RINGING.lock().unwrap();
        slot.pending = Some(PendingInbound {
            client: Arc::clone(&client),
            peer: peer.clone(),
            offer,
            opts,
            call_id: call_id.clone(),
        });
        slot.generation += 1;
        let g = slot.generation;
        ring_inbound_spawn_timeout(g);
    }

    // Surface the UI. `publish_call_state_with(..., active=false, ringing=true)`
    // resets the controls + presents the Accept/Decline panel.
    if let Some(weak) = weak_window() {
        let peer_b58 = peer.to_base58();
        let (peer_b58, peer_name) = crate::resolve_peer_display(&peer_b58);
        let overlay = match mode {
            Some(CallMode::Video) => crate::GlobalOverlayType::VideoCall,
            Some(CallMode::Screen) => crate::GlobalOverlayType::ScreenShare,
            Some(CallMode::Audio) | None => crate::GlobalOverlayType::AudioCall,
        };
        crate::publish_call_state_with(weak.clone(), Some(peer_b58), peer_name, false, true);
        crate::show_call_overlay(weak, overlay);
    }
    tracing::debug!("[call] inbound offer: {call_id}");
}

/// Spawn a 30-second timeout task that declines the offer if the user
/// hasn't acted. Only one is live; a newer offer (with a higher generation)
/// supersedes an older one via the generation guard.
fn ring_inbound_spawn_timeout(g: u64) {
    let rt = crate::runtime();
    rt.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let still_current = {
            let slot = RINGING.lock().unwrap();
            slot.generation == g && slot.pending.is_some()
        };
        if !still_current {
            return;
        }
        tracing::warn!("[call] inbound offer timed out");
        let _ = slint::invoke_from_event_loop(move || decline_inbound());
    });
}

/// Consume the pending inbound (if any) and drive the WebRTC answer.
/// Called from the ACCEPT button (`CallState.accept-call`) or from the
/// timeout task.
pub fn accept_inbound() {
    let taken: Option<PendingInbound> = RINGING.lock().unwrap().pending.take();
    // Bump generation so a live timeout task (for this offer) becomes stale.
    RINGING.lock().unwrap().generation += 1;

    let Some(p) = taken else {
        tracing::warn!("[call] accept_inbound: no pending offer");
        return;
    };

    match answer_offer(p.client, p.peer, &p.offer, p.opts) {
        Ok(()) => {
            if let Some(weak) = weak_window() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        let st = ui.global::<CallState>();
                        st.set_ringing(false);
                        st.set_active(true);
                    }
                });
            }
        }
        Err(e) => {
            tracing::debug!("[call] answer_offer: {e}");
            if let Some(weak) = weak_window() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        let st = ui.global::<CallState>();
                        st.set_ringing(false);
                        st.set_error_message(SharedString::from(format!("Call failed: {e}")));
                    }
                });
            }
        }
    }
}

/// Consume the pending inbound (if any) and send a `Bye` to the peer.
/// Called from the DECLINE button or the timeout task.
pub fn decline_inbound() {
    let taken: Option<PendingInbound> = RINGING.lock().unwrap().pending.take();
    RINGING.lock().unwrap().generation += 1;

    let Some(p) = taken else {
        tracing::warn!("[call] decline_inbound: no pending offer");
        return;
    };

    let bye = SignalFrame::new(p.call_id.clone(), media::SignalKind::Bye)
        .with_from(p.client.peer_base58());
    let json = serde_json::to_string(&bye);
    if let Ok(json) = json {
        if let Err(e) = p.client.send_webrtc_signal(p.peer.clone(), &json) {
            tracing::debug!("[call] decline: send_webrtc: {e}");
        }
    }
    tracing::debug!(
        "[call] inbound offer declined: {call_id}",
        call_id = p.call_id
    );

    if let Some(weak) = weak_window() {
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let st = ui.global::<CallState>();
            st.set_ringing(false);
            st.set_active(false);
            st.set_error_message(SharedString::new());
            let state = ui.global::<AppState>();
            let mut nav = state.get_nav_state();
            nav.global_overlay = GlobalOverlayType::None;
            state.set_nav_state(nav);
        });
    }
}

// ── public: lifecycle ─────────────────────────────────────────────────────

pub fn stop_call() {
    // Local user hung up (active side) → tell the peer with a `Bye` so it
    // tears down now, instead of waiting for its ICE/DTLS to time out.
    stop_common(true);
}

/// Teardown used when *we* are the passive side (the peer already sent a
/// `Bye`, or a new call supersedes this one), or from the async event-drain's
/// `ended` branch. Cleans everything local but does NOT send a `Bye` —
/// echoing one back is at best noise and risks a signaling loop.
pub fn stop_call_no_notify() {
    stop_common(false);
}

fn stop_common(send_bye: bool) {
    // Invalidate every sink closure installed by the call we are tearing
    // down (remote cam / remote scr / local cam preview / local scr preview).
    // Frames already queued on their source threads will check this when
    // they next enter the SLint event loop and bail — so they can NOT
    // re-populate the `CallState` image slots with the last full-res pixel
    // buffer we just reset. Bump FIRST so the invalidation is visible to
    // any thread that hasn't yet captured a new value.
    CALL_GEN.fetch_add(1, std::sync::atomic::Ordering::Release);
    if let Some(call) = active_call() {
        if send_bye {
            let _ = call.hangup();
        } else {
            let _ = call.close();
        }
    }
    CALL.with(|slot| {
        if let Some(inner) = slot.borrow_mut().take() {
            // Stop the video encoders NOW (deterministically). Their threads
            // own an openh264 encoder + tokio runtime + a full-res frame
            // buffer; signalling stop here means teardown doesn't depend on
            // the async `pc.close()` finishing and the per-track consumer
            // tasks exiting before `Stop` would otherwise fire via `Drop`.
            if let Some(src) = inner.cam_src.as_ref() {
                src.stop();
            }
            if let Some(src) = inner.scr_src.as_ref() {
                src.stop();
            }
            inner.audio.stop_input();
            inner.audio.stop_output();
        }
    });
    audio::clear_input_sink();
    call_dispatcher::teardown_sources();
    // Close the local capture device(s). `Camera` is a process-wide singleton
    // so we release it here regardless of which path started it (`start_call`
    // → `bootstrap_media`, a mid-call upgrade, or the in-chat scanner).
    // `stop_screen_capture` below only tears down the screen capture when we
    // are in the event loop (where the dispatcher is registered from the UI
    // side). For Android / iOS the bridge requests the shell to stop.
    crate::stop_local_media_capture();
    // Reset per-call UI flags so the next call starts clean.
    REMOTE_MUTE.store(false, std::sync::atomic::Ordering::Relaxed);
    #[cfg(target_os = "android")]
    {
        // Release the shell's `AudioRecord`. The shell is already holding
        // the mic while we've been active; once this returns, the loop
        // terminates and the next `startMic()` call will start a fresh one.
        bridge::request_mic_stop();
    }
    tracing::debug!("[call] stopped");
}

pub fn mute_mic(on: bool) {
    CALL.with(|slot| {
        if let Some(inner) = slot.borrow().as_ref() {
            inner.aud_src.mute(on);
        }
    });
}

/// Mute / unmute one of the caller's local video sources (peer side freezes
/// on the last frame). The kind is `Cam` for the local camera, `Scr` for the
/// local screen share. No-ops if that source wasn't in this call.
pub fn set_video_mute(kind: media::VKind, on: bool) {
    if let Some(call) = active_call() {
        call.set_video_mute(kind, on);
    }
}

/// Process-global "remote audio muted" gate. The UI's SPEAKER ON/OFF button
/// toggles this. When set, `wire_remote_sinks` drops decoded remote audio
/// frames (so nothing is written to the speaker), but we still receive them
/// over the wire — the peer is unaffected.
pub(crate) static REMOTE_MUTE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_remote_mute(on: bool) {
    REMOTE_MUTE.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn is_remote_muted() -> bool {
    REMOTE_MUTE.load(std::sync::atomic::Ordering::Relaxed)
}

// ── mid-call video upgrades ────────────────────────────────────────────────

/// Offerer path: I want to add a local video track (`kind`) to the current
/// call (audio → video, audio → screen, or — less common — me adding a cam
/// during a screen share). The caller is responsible for:
///   1. Starting the local capture device (camera / screen) — see
///      `crate::start_local_cam` / `crate::start_local_screen` in `lib.rs`.
///   2. Building a `VidSource::new(kind)`.
///   3. Registering it in `call_dispatcher` via `set_cam_source` /
///      `set_scr_source` so camera frames flow in.
/// Then call this to attach the track, re-offer, and emit the upgrade offer
/// over the transport (peer will answer automatically).
pub fn upgrade_outgoing(
    kind: media::VKind,
    both_sides: bool,
    src: Arc<VidSource>,
) -> anyhow::Result<()> {
    let call = active_call().ok_or_else(|| anyhow::anyhow!("no active call"))?;
    media_rt()
        .block_on(call.upgrade_offer(src, kind, both_sides))
        .map_err(|e| anyhow::anyhow!("upgrade_offer: {e}"))?;
    Ok(())
}

/// Answerer path: the peer has sent an upgrade offer. If their
/// `both_sides` is `true` (e.g. audio→video, both sides send cam), we attach
/// our own local source too (caller pre-owns it via `start_local_capture`
/// in `lib.rs`); otherwise we just apply their remote track and answer.
pub fn apply_incoming_upgrade(frame: &SignalFrame) {
    let kind = frame.upgrade_kind.unwrap_or(media::VKind::Cam);
    let is_scr = matches!(kind, media::VKind::Scr);
    let own_src = if frame.both_sides == Some(true) {
        let kind = match frame.upgrade_kind {
            Some(k) => k,
            None => {
                tracing::warn!("[call] upgrade frame missing kind");
                return;
            }
        };
        match crate::start_local_capture(kind) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::debug!("[call] start_local_capture: {e}");
                None
            }
        }
    } else {
        None
    };
    let call = match active_call() {
        Some(c) => c,
        None => {
            tracing::warn!("[call] apply_incoming_upgrade: no active call");
            return;
        }
    };
    if let Err(e) = media_rt().block_on(call.upgrade_answer(frame, own_src)) {
        tracing::debug!("[call] upgrade_answer: {e}");
    }
    // Surface the new remote video presence on the UI (B image slot starts
    // rendering + the has-remote-* flag flips green on the peer-side state).
    if let Some(weak) = weak_window() {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                let cs = ui.global::<CallState>();
                if is_scr {
                    cs.set_has_remote_scr(true);
                } else {
                    cs.set_has_remote_cam(true);
                }
            }
        });
    }
}

// ── media bootstrap (mic out, cam/scr out, speaker in) ────────────────────
//
// `opts` gates which local sources to attach. When a cam/scr source is
// needed, we call `crate::start_local_capture(kind)` ourselves — it
// ensures the device is running, the dispatcher is wired, and a fresh
// `VidSource` is registered into the dispatcher slot. That way there's a
// single source that both receives camera frames AND is attached to the
// webrtc track.
fn bootstrap_media(
    opts: CallOptions,
) -> anyhow::Result<(
    audio::Audio,
    Arc<Mutex<std::collections::VecDeque<u8>>>,
    Arc<AudSource>,
    Option<Arc<VidSource>>,
    Option<Arc<VidSource>>,
)> {
    let audio = audio::Audio::new();

    let aud_src = Arc::new(AudSource::new());

    // ── local mic → this call's `AudSource` ─────────────────────────────
    // iOS + desktop: cpal (`crates/audio`) is the mic — cpal 0.16 uses Core
    // Audio on macOS/iOS, so no AVAudioEngine shim is needed. The callback
    // runs on cpal's audio thread; `AudSource::push` is non-blocking and
    // drops frames while muted.
    // Android: do NOT open a cpal input stream — the shell already runs a
    // native `MicLoop` (AudioRecord 16 kHz mono) that delivers s16le via
    // `bridge::set_audio_consumer` → `call_dispatcher::on_audio_frame`.
    // Opening a second recorder would fight the shell's AudioRecord for the
    // microphone, so we only register this call's `AudSource` in that slot.
    #[cfg(not(target_os = "android"))]
    {
        let mic = Arc::clone(&aud_src);
        // The PCMU track is fixed at 8 kHz mono, but cpal delivers the mic at
        // the device's native rate (typically 48 kHz stereo). Resample here
        // (audio thread; cheap O(n) linear) before pushing into the track, or
        // the peer hears 6× pitch / wrong-duration garbage.
        audio::set_input_sink(move |bytes: &[u8], rate: u32, ch: u32| {
            if bytes.is_empty() {
                return;
            }
            let pcm8 = audio::resample_s16le(bytes, rate, ch, 8_000, 1);
            if !pcm8.is_empty() {
                mic.push(&pcm8, 8_000);
            }
        });
        let (rate, ch) = audio
            .start_input()
            .map_err(|e| anyhow::anyhow!("start mic: {e}"))?;
        tracing::info!("[call] mic cpal @ {rate} Hz / {ch} ch (→ 8 kHz PCMU)");
    }
    #[cfg(target_os = "android")]
    {
        // Start the shell's `MicLoop` (AudioRecord 16 kHz mono) so it can
        // begin streaming into the slot we register below. This is a static
        // JNI call into `com.chatx.NativeBridge.startMicCapture()` →
        // `Capture.startMic()`; the shell spawns a HandlerThread and is
        // expected to deliver `audioPcmIn` frames once recording starts.
        // No-op if the permission isn't granted (Shell requests it in
        // `onCreate`, so by the time the app is interactive it should be).
        bridge::request_mic_start();
        // Point the shell's mic at this call's `AudSource`. After this
        // returns, `audioPcmIn` → `sinks::audio_in::call` →
        // `call_dispatcher::on_audio_frame` → `src.push(bytes, rate)` lands
        // on our webrtc track.
        call_dispatcher::set_aud_source(Some(Arc::clone(&aud_src)));
        tracing::info!("[call] mic android @ 16000 Hz / 1 ch (shell-driven)");
    }

    // ── speaker (remote → local) ────────────────────────────────────────
    let _ = audio
        .start_output()
        .map_err(|e| anyhow::anyhow!("start speaker: {e}"))?;
    let queue = audio.output_queue().clone();

    // Camera / screen local sources. `start_local_capture` guarantees a
    // single registered `VidSource` (which the camera / screen sinks push
    // into) AND starts the underlying device — no need to duplicate that
    // work on the caller side.
    //
    // IMPORTANT: these must stay LAZY `if`s. `match (flag, expr)` builds the
    // tuple eagerly — `expr` runs even when `flag` is `false` — which would
    // boot the Retina screen recorder (xcap/ScreenCaptureKit + TCC prompt,
    // on the UI thread) for EVERY call and freeze the UI on accept.
    let cam_src: Option<Arc<VidSource>> = if opts.include_cam {
        Some(crate::start_local_capture(media::VKind::Cam)?)
    } else {
        None
    };
    let scr_src: Option<Arc<VidSource>> = if opts.include_scr {
        match crate::start_local_capture(media::VKind::Scr) {
            Ok(src) => Some(src),
            Err(e) => {
                // Screen bootstrap failed after camera succeeded: roll back
                // the camera side so no encoder thread / capture device is
                // left half-alive on a call that is about to error out.
                release_bootstrap(&audio, cam_src.as_ref(), None);
                return Err(e);
            }
        }
    } else {
        None
    };

    // Register in the dispatcher so camera/screen frames get to us.
    if let Some(src) = &cam_src {
        call_dispatcher::set_cam_source(Some(Arc::clone(src)));
    }
    if let Some(src) = &scr_src {
        call_dispatcher::set_scr_source(Some(Arc::clone(src)));
    }

    Ok((audio, queue, aud_src, cam_src, scr_src))
}

/// Tear everything [`bootstrap_media`] started, for the setup-failure paths
/// (`PeerCall::offer` / `answer` error, transport send failure) where a
/// `CallInner` was **never** stored, so `stop_common` cannot clean up for us.
/// Without this, the video encoder threads (openh264 encoders + last full-res
/// frame), the cpal input/output streams, and the camera / screen capture
/// devices all stay alive after a failed call — the reported "memory left
/// over after a call" on the error paths.
fn release_bootstrap(
    audio: &audio::Audio,
    cam: Option<&Arc<VidSource>>,
    scr: Option<&Arc<VidSource>>,
) {
    if let Some(src) = cam {
        src.stop();
    }
    if let Some(src) = scr {
        src.stop();
    }
    audio.stop_input();
    audio.stop_output();
    audio::clear_input_sink();
    call_dispatcher::teardown_sources();
    crate::stop_local_media_capture();
    #[cfg(target_os = "android")]
    bridge::request_mic_stop();
}

/// Install remote-side sinks on the `PeerCall`.
fn wire_remote_sinks(
    call: &PeerCall,
    audio: &audio::Audio,
    queue: &Arc<Mutex<std::collections::VecDeque<u8>>>,
) {
    // Remote audio (webrtc read thread) → cpal speaker queue.
    // `start_output` negotiated (out_rate, out_ch) with the device; remote
    // PCMU arrives as 8 kHz mono. Resample + upmix to the speaker layout so
    // the render callback consumes a correct frame stride.
    let out_rate = audio.out_rate();
    let out_ch = audio.out_channels();
    tracing::info!("[call] wire_remote_sinks: in=8k/1ch  out={out_rate}Hz/{out_ch}ch");
    let q = queue.clone();
    call.set_remote_audio_sink(move |s16le: &[u8], r: u32, c: u32| {
        if s16le.is_empty() {
            return;
        }
        // SPEAKER OFF: drop the frame locally (peer is unaffected). Keeps the
        // pipeline alive (decoding still happens) so flipping the toggle back
        // ON is instant — no re-negotiation needed.
        if REMOTE_MUTE.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let pcm = audio::resample_s16le(s16le, r, c, out_rate, out_ch);
        let mut q = q.lock().unwrap();
        q.extend(pcm.iter().copied());
        const CAP: usize = 5 * 48_000 * 2 * 2; // ~5s @48kHz stereo
        while q.len() > CAP {
            q.pop_front();
        }
    });

    // Remote camera (webrtc read thread) → Slint event loop → CallState.remote-cam.
    // Gen guard: if the call has been torn down by the time this frame reaches
    // the event loop, drop it — see `CALL_GEN`.
    {
        let g = CALL_GEN.load(std::sync::atomic::Ordering::Acquire);
        call.set_remote_camera_sink(move |rgba: Vec<u8>, w: u32, h: u32| {
            if !gen_live(g) {
                return;
            }
            if let Some(weak) = weak_window() {
                let _ = slint::invoke_from_event_loop(move || {
                    if !gen_live(g) {
                        return;
                    }
                    if let Some(ui) = weak.upgrade() {
                        let img = rgba_to_slint(&rgba, w, h);
                        let cs = ui.global::<CallState>();
                        cs.set_remote_cam(img);
                        cs.set_has_remote_cam(true);
                    }
                });
            }
        });
    }

    // Remote screen (webrtc read thread) → Slint event loop → CallState.remote-scr.
    {
        let g = CALL_GEN.load(std::sync::atomic::Ordering::Acquire);
        call.set_remote_screen_sink(move |rgba: Vec<u8>, w: u32, h: u32| {
            if !gen_live(g) {
                return;
            }
            if let Some(weak) = weak_window() {
                let _ = slint::invoke_from_event_loop(move || {
                    if !gen_live(g) {
                        return;
                    }
                    if let Some(ui) = weak.upgrade() {
                        let img = rgba_to_slint(&rgba, w, h);
                        let cs = ui.global::<CallState>();
                        cs.set_remote_scr(img);
                        cs.set_has_remote_scr(true);
                    }
                });
            }
        });
    }
}

/// Install the local camera preview sink so the UI shows a PIP of "me" in
/// parallel with the webrtc `VidSource` that the camera frames feed. Marks
/// `has-my-cam` so the A image slot renders my camera immediately.
pub(crate) fn install_local_cam_preview() {
    let g = CALL_GEN.load(std::sync::atomic::Ordering::Acquire);
    call_dispatcher::set_cam_preview(Some(Box::new(move |rgba: &[u8], w: u32, h: u32| {
        if !gen_live(g) {
            return;
        }
        if let Some(weak) = weak_window() {
            // Copy + horizontal mirror so the "me" preview reads as a mirror.
            let mut local = rgba.to_vec();
            call_dispatcher::flip_h(&mut local, w);
            let _ = slint::invoke_from_event_loop(move || {
                if !gen_live(g) {
                    return;
                }
                if let Some(ui) = weak.upgrade() {
                    let img = rgba_to_slint(&local, w, h);
                    let cs = ui.global::<CallState>();
                    cs.set_my_cam(img);
                    cs.set_has_my_cam(true);
                }
            });
        }
    })));
}

/// Watch the `PeerCall` lifecycle events. When the peer ends the call
/// (hangup / failure / disconnect), tear the call down on the UI thread.
/// Idempotent: safe even when we initiated the teardown ourselves
/// (`close()` also emits `Hangup`).
fn spawn_event_drain(call: PeerCall) {
    let mut rx = call.events();

    // Ended detection still goes via the tokio event loop (receiving `Bye` /
    // `Hangup` / `Failed` / `Disconnected` on the webrtc state machine). We
    // no longer try to observe `Connected` here, because in practice the
    // broadcast is dropped — the webrtc thread fires it before the receiver
    // in this task is registered — so we just log every event we do see and
    // act on `is_ended` only.
    media_rt().spawn(async move {
        while let Ok(ev) = rx.recv().await {
            tracing::info!(?ev, "[call] event");
            if ev.is_ended() {
                fire_ui_end();
                break;
            }
        }
    });

    // ── connected detection: plain OS-thread poller ──────────────────
    // The `Connected` broadcast is dropped in practice (no live receiver
    // when the webrtc thread fires it), and a tokio task did not reliably
    // get scheduled. A plain `std::thread` always runs and `peer_state()`
    // is synchronous, so this is the robust path: poll until Connected,
    // then flip `CallState.connected` from a detached thread (which is safe
    // via `slint::invoke_from_event_loop`).
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let s = call.peer_state();
            if call.is_connected() {
                mark_connected();
                break;
            }
            if matches!(
                s,
                media::webrtc::RTCPeerConnectionState::Failed
                    | media::webrtc::RTCPeerConnectionState::Disconnected
            ) {
                break;
            }
        }
    });
}

/// Set `CallState.connected = true` from the event thread. The hop to the
/// UI thread + the window lookup are the part that can silently no-op if the
/// window handle is missing or dead — log which case happened.
fn mark_connected() {
    if let Some(weak) = weak_window() {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<CallState>().set_connected(true);
            }
        });
    }
}

/// Build a `slint::Image` from an `rgba8` buffer. (Matches
/// `make_preview_image` in lib.rs for the `fmt == 1 (RGBA8888)` case.)
fn rgba_to_slint(rgba: &[u8], w: u32, h: u32) -> slint::Image {
    if w == 0 || h == 0 || rgba.is_empty() {
        return slint::Image::default();
    }
    let n = (w as usize).saturating_mul(h as usize);
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
    let dst = buf.make_mut_bytes();
    let l = (n * 4).min(rgba.len()).min(dst.len());
    dst[..l].copy_from_slice(&rgba[..l]);
    slint::Image::from_rgba8(buf)
}

// ── media runtime (shared with the media crate) ──────────────────────────

/// The `media` crate owns its own `OnceLock<tokio::runtime>`. We create it
/// here once, on the same `media`-module path, so both sides use the same
/// runtime. In practice `PeerCall::offer/answer/handle_inbound` already
/// call `media::runtime()` internally — we just need a runtime to
/// `block_on` the offer/answer calls from non-tokio threads.
fn media_rt() -> &'static tokio::runtime::Runtime {
    // We create a thread-local-ish global runtime, but `OnceLock` is the
    // standard pattern. We do NOT reuse the chat app's `runtime()` (lib.rs)
    // because that one is also `OnceLock` — and we don't want to couple to
    // it. A dedicated runtime here keeps the media crate's internal
    // `rt.spawn` calls independent of the app's rt.
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("media rt")
    })
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
