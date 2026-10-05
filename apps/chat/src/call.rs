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

use chatx_core::Client;
use chatx_core::signal::HttpDirectory;
use media::{
    AudSource, Config as MediaConfig, PeerCall, SignalFrame, SignalKind, SignalTransport,
    VidSource,
};
pub use crate::call_dispatcher;
use crate::{CallState, MainWindow};
use slint::ComponentHandle;

pub type ChatxClient = Client<HttpDirectory>;

// ── SignalTransport adapter (media crate ↔ libp2p) ────────────────────────

struct Lp2pSignal {
    client: Arc<ChatxClient>,
    peer: libp2p::PeerId,
}

impl SignalTransport for Lp2pSignal {
    fn send(&self, frame: &SignalFrame) -> media::Result<()> {
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

/// Weak reference to the Slint `MainWindow`. Set once at app startup.
static WINDOW: OnceLock<slint::Weak<MainWindow>> = OnceLock::new();

/// Install the `MainWindow` handle once; safe to call multiple times.
pub fn set_weak_window(weak: slint::Weak<MainWindow>) {
    let _ = WINDOW.set(weak);
}

fn weak_window() -> Option<slint::Weak<MainWindow>> {
    WINDOW.get().cloned()
}

/// Called (from the webrtc event thread) when the peer ends the call. The
/// teardown must run on the UI thread — the `CALL` thread_local + cpal
/// streams are owned there — so hop the whole thing over. Idempotent: safe
/// to run even if we are also tearing down from the "end call" button.
fn fire_ui_end() {
    if let Some(weak) = weak_window() {
        let _ = slint::invoke_from_event_loop(move || {
            stop_call();
            if let Some(ui) = weak.upgrade() {
                let cs = ui.global::<CallState>();
                cs.set_active(false);
                cs.set_muted(false);
                cs.set_has_local_camera(false);
                let state = ui.global::<crate::AppState>();
                let mut nav = state.get_nav_state();
                nav.global_overlay = crate::GlobalOverlayType::None;
                state.set_nav_state(nav);
            }
        });
    }
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
    let call_id = format!("call-{:x}", now_nanos());
    let cfg = MediaConfig::from_env();
    let call_id_c = call_id.clone();
    let aud2 = Arc::clone(&aud_src);
    let cam2 = cam_src.as_ref().cloned();
    let scr2 = scr_src.as_ref().cloned();

    let (call, offer) = {
        let t = Arc::clone(&transport);
        media_rt().block_on(async move {
            PeerCall::offer(
                call_id_c.clone(),
                String::new(),
                &cfg,
                as_dyn(t),
                Some(aud2),
                cam2,
                scr2,
            )
            .await
        })
    }
    .map_err(|e| anyhow::anyhow!("PeerCall::offer: {e}"))?;

    // `PeerCall::offer` returns the offer SDP frame; we are responsible for
    // sending it to the peer over the transport. (The media crate doesn't
    // auto-transmit; `Lp2pSignal::send` is invoked per frame.)
    transport.send(&offer).map_err(|e| anyhow::anyhow!("transport send: {e}"))?;

    wire_remote_sinks(&call, &audio, &queue);

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

    eprintln!("[call] offerer started: peer={peer} cam={} scr={}",
        opts.include_cam, opts.include_scr);
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
    let offer_c = offer.clone();
    let cfg = MediaConfig::from_env();
    let aud2 = Arc::clone(&aud_src);
    let cam2 = cam_src.as_ref().cloned();
    let scr2 = scr_src.as_ref().cloned();

    let (call, answer) = {
        let t = Arc::clone(&transport);
        media_rt().block_on(async move {
            PeerCall::answer(
                &offer_c,
                &cfg,
                as_dyn(t),
                Some(aud2),
                cam2,
                scr2,
            )
            .await
        })
    }
    .map_err(|e| anyhow::anyhow!("PeerCall::answer: {e}"))?;

    // Send our answer SDP back to the peer.
    transport.send(&answer).map_err(|e| anyhow::anyhow!("transport send: {e}"))?;

    wire_remote_sinks(&call, &audio, &queue);

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
    eprintln!("[call] answerer started");
    Ok(())
}

// ── public: inbound signaling ─────────────────────────────────────────────

/// Entry point for `ChatEvent::Webrtc`. `json` is the JSON-encoded
/// `media::SignalFrame` produced by the peer's `Lp2pSignal::send`.
pub fn handle_signal_offer(client: Arc<ChatxClient>, peer: libp2p::PeerId, json: &str) {
    let frame: SignalFrame = match serde_json::from_str(json) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[call] bad SignalFrame: {e}");
            return;
        }
    };
    match &frame.kind {
        SignalKind::Offer(_) => {
            if is_active() {
                tracing::warn!("[call] got Offer while active — dropping");
                return;
            }
            let opts = CallOptions { include_cam: true, include_scr: true };
            if let Err(e) = answer_offer(client, peer, &frame, opts) {
                eprintln!("[call] answer_offer: {e}");
            }
        }
        _ => {
            if let Some(call) = active_call() {
                if let Err(e) = call.handle_inbound(&frame) {
                    eprintln!("[call] handle_inbound: {e}");
                }
            }
        }
    }
}

// ── public: lifecycle ─────────────────────────────────────────────────────

pub fn stop_call() {
    if let Some(call) = active_call() {
        let _ = call.close();
    }
    CALL.with(|slot| {
        if let Some(inner) = slot.borrow_mut().take() {
            inner.audio.stop_input();
            inner.audio.stop_output();
        }
    });
    audio::clear_input_sink();
    call_dispatcher::teardown_sources();
    #[cfg(target_os = "android")]
    {
        // Release the shell's `AudioRecord`. The shell is already holding
        // the mic while we've been active; once this returns, the loop
        // terminates and the next `startMic()` call will start a fresh one.
        bridge::request_mic_stop();
    }
    // Clear the local camera preview on the UI thread.
    if let Some(weak) = weak_window() {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                let cs = ui.global::<CallState>();
                cs.set_local_camera(slint::Image::default());
                cs.set_has_local_camera(false);
            }
        });
    }
    eprintln!("[call] stopped");
}

pub fn mute_mic(on: bool) {
    CALL.with(|slot| {
        if let Some(inner) = slot.borrow().as_ref() {
            inner.aud_src.mute(on);
        }
    });
}

// ── media bootstrap (mic out, cam/scr out, speaker in) ────────────────────

fn bootstrap_media(opts: CallOptions) -> anyhow::Result<(
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
        audio::set_input_sink(move |bytes: &[u8], rate: u32, _ch: u32| {
            if !bytes.is_empty() {
                mic.push(bytes, rate);
            }
        });
        let (rate, ch) = audio
            .start_input()
            .map_err(|e| anyhow::anyhow!("start mic: {e}"))?;
        tracing::info!("[call] mic cpal @ {rate} Hz / {ch} ch");
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

    // Camera / screen local sources.
    let cam_src = if opts.include_cam {
        Some(Arc::new(
            VidSource::new().map_err(|e| anyhow::anyhow!("VidSource::cam: {e}"))?,
        ))
    } else {
        None
    };
    let scr_src = if opts.include_scr {
        Some(Arc::new(
            VidSource::new().map_err(|e| anyhow::anyhow!("VidSource::scr: {e}"))?,
        ))
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
    tracing::info!(
        "[call] wire_remote_sinks: in=8k/1ch  out={out_rate}Hz/{out_ch}ch"
    );
    let q = queue.clone();
    call.set_remote_audio_sink(move |s16le: &[u8], r: u32, c: u32| {
        if s16le.is_empty() {
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

    // Remote camera (webrtc read thread) → Slint event loop → CallState.remote-camera.
    call.set_remote_camera_sink(move |rgba: Vec<u8>, w: u32, h: u32| {
        if let Some(weak) = weak_window() {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    let img = rgba_to_slint(&rgba, w, h);
                    ui.global::<CallState>().set_remote_camera(img);
                }
            });
        }
    });

    // Remote screen (webrtc read thread) → Slint event loop → CallState.remote-screen.
    call.set_remote_screen_sink(move |rgba: Vec<u8>, w: u32, h: u32| {
        if let Some(weak) = weak_window() {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    let img = rgba_to_slint(&rgba, w, h);
                    ui.global::<CallState>().set_remote_screen(img);
                }
            });
        }
    });
}

/// Install the local camera preview sink so the UI shows a PIP of "me" in
/// parallel with the webrtc `VidSource` that the camera frames feed.
fn install_local_cam_preview() {
    call_dispatcher::set_cam_preview(Some(Box::new(move |rgba: &[u8], w: u32, h: u32| {
        if let Some(weak) = weak_window() {
            // Copy + horizontal mirror so the "me" preview reads as a mirror.
            let mut local = rgba.to_vec();
            call_dispatcher::flip_h(&mut local, w);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    let img = rgba_to_slint(&local, w, h);
                    let cs = ui.global::<CallState>();
                    cs.set_local_camera(img);
                    cs.set_has_local_camera(true);
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
    media_rt().spawn(async move {
        while let Ok(ev) = rx.recv().await {
            tracing::info!(?ev, "[call] event");
            if ev.is_ended() {
                fire_ui_end();
                break;
            }
        }
    });
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
