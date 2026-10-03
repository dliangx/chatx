//! Realtime voice-call session (M7).
//!
//! One active call at a time, driven entirely by:
//! - **mic → peer**: cpal's input callback (audio thread) forwards every frame
//!   to [`ChatxClient::send_audio`], which enqueues a `Cmd::SendAudio` on the
//!   swarm loop.
//! - **peer → speaker**: the inbound pump delivers a `ChatEvent::Audio` on the
//!   tokio thread; we hop to the UI thread and push the bytes into the cpal
//!   output queue. The render callback drains it as it runs.
//!
//! cpal normalises to s16le and handles resampling against the OS's default
//! input/output rates, so both halves stay transparently at the device rates —
//! no manual conversion here.
//!
//! On every platform (macOS / iOS / Android / Windows / Linux) the same code
//! path runs: cpal talks to Core Audio / Oboe / WASAPI / ALSA underneath.

use std::cell::RefCell;
use std::sync::Arc;

use chatx_core::Client;
use chatx_core::signal::HttpDirectory;

pub type ChatxClient = Client<HttpDirectory>;

struct CallInner {
    #[allow(dead_code)]
    peer: libp2p::PeerId,
    /// Lives so its cpal input / output streams stay open; dropped on
    /// [`stop_call`].
    audio: audio::Audio,
    #[allow(dead_code)]
    rate: u32,
    #[allow(dead_code)]
    ch: u32,
}

thread_local! {
    static CALL: RefCell<Option<CallInner>> = RefCell::new(None);
}

/// Whether a call is currently open on this thread.
pub fn is_active() -> bool {
    CALL.with(|slot| slot.borrow().is_some())
}

/// Base-58 peer id of the active call, if any.
pub fn active_peer_base58() -> Option<String> {
    CALL.with(|slot| slot.borrow().as_ref().map(|c| c.peer.to_base58()))
}

/// Open a voice call to `peer_base58` (a directory username, base-58 peer id,
/// or the other side of a DM chat id). Idempotent: any existing call is
/// dropped first.
///
/// Requires that `client` is already logged in and that `peer_base58` is
/// resolvable in the directory (online + approved). Returns any startup error
/// (no mic / no speaker / peer offline).
pub fn start_call(client: Arc<ChatxClient>, peer_base58: &str) -> anyhow::Result<()> {
    if is_active() {
        stop_call();
    }
    let peer = client
        .dial_peer(peer_base58)
        .map_err(|e| anyhow::anyhow!("dial peer '{peer_base58}': {e}"))?;

    let audio = audio::Audio::new();

    // Mic → peer. The closure runs every capture tick on the audio thread;
    // `Client::send_audio` only enqueues one cmd on the swarm loop, so it is
    // safe to call here.
    let up = Arc::clone(&client);
    let up_peer = peer;
    audio::set_input_sink(move |bytes: &[u8], rate: u32, ch: u32| {
        if bytes.is_empty() {
            return;
        }
        if let Err(e) = up.send_audio(up_peer, rate, ch, bytes.to_vec()) {
            eprintln!("[call] send_audio: {e}");
        }
    });

    let (rate, ch) = audio
        .start_input()
        .map_err(|e| anyhow::anyhow!("start mic: {e}"))?;
    audio
        .start_output()
        .map_err(|e| anyhow::anyhow!("start speaker: {e}"))?;

    eprintln!("[call] open: peer={peer} mic={rate}Hz/{ch}ch");
    CALL.with(|slot| *slot.borrow_mut() = Some(CallInner { peer, audio, rate, ch }));
    Ok(())
}

/// Inject a peer audio frame (s16le PCM) into the speaker queue.
///
/// `rate` / `ch` are informational — the cpal render callback consumes from
/// the shared queue at the OS output rate; cross-rate mismatches are handled
/// by cpal internally (frame alignment). Kept for logs / debugging.
///
/// MUST be called on the thread that owns the cpal streams — the Slint UI
/// thread. The inbound pump hops here via `slint::invoke_from_event_loop`.
pub fn play_incoming(s16le: Vec<u8>, rate: u32, ch: u32) {
    if s16le.is_empty() {
        return;
    }
    CALL.with(|slot| {
        if let Some(c) = slot.borrow().as_ref() {
            c.audio.queue_output(s16le);
        } else {
            eprintln!("[call] play_incoming: no active call, dropping {rate}Hz/{ch}ch");
        }
    });
}

/// Close the active call (if any), stopping the mic and speaker and clearing
/// the global input sink.
pub fn stop_call() {
    eprintln!("[call] close");
    CALL.with(|slot| {
        let mut guard = slot.borrow_mut();
        if let Some(inner) = guard.take() {
            inner.audio.stop_input();
            inner.audio.stop_output();
        }
    });
    audio::clear_input_sink();
}
