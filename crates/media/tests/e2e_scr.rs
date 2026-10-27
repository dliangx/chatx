//! End-to-end loopback test: A offerer (audio + screen) ↔ B answerer.
//! Verifies the full screen-share pipeline works at full desktop resolution.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use media::{
    AudSource, Config, PeerCall, SignalFrame, SignalTransport, VKind, VidSource,
};

#[derive(Clone)]
struct ToA {
    to_b: Arc<Mutex<Vec<SignalFrame>>>,
}
impl SignalTransport for ToA {
    fn send(&self, frame: &SignalFrame) -> media::Result<()> {
        self.to_b.lock().unwrap().push(frame.clone());
        Ok(())
    }
}
#[derive(Clone)]
struct ToB {
    to_a: Arc<Mutex<Vec<SignalFrame>>>,
}
impl SignalTransport for ToB {
    fn send(&self, frame: &SignalFrame) -> media::Result<()> {
        self.to_a.lock().unwrap().push(frame.clone());
        Ok(())
    }
}

fn pump_once(from: &Arc<Mutex<Vec<SignalFrame>>>, call: &PeerCall) {
    let frames = std::mem::take(&mut *from.lock().unwrap());
    for f in frames {
        let _ = call.handle_inbound(&f);
    }
}

fn is_connected(call: &PeerCall) -> bool {
    format!("{:?}", call.peer_state()).eq_ignore_ascii_case("connected")
}

fn make_frame(w: u32, h: u32, color: u8) -> Vec<u8> {
    let mut v = vec![0u8; (w as usize) * (h as usize) * 4];
    for p in v.chunks_mut(4) {
        p[0] = color;
        p[1] = 100;
        p[2] = 200;
        p[3] = 255;
    }
    v
}

/// Regression: screen capture at 5K (5120×2880) Retina — openh264 hard-limits
/// the source to 3840×2160, so `VidSource` must downscale before encoding.
/// The loopback (loopback transport, no network) still has to deliver frames
/// to the remote sink for the test to pass.
#[tokio::test]
async fn e2e_screen_share_5k_loopback() {
    let (w, h) = (5120u32, 2880u32);
    let in_a = Arc::new(Mutex::new(Vec::new()));
    let in_b = Arc::new(Mutex::new(Vec::new()));
    let ta: Arc<dyn SignalTransport> = Arc::new(ToA { to_b: Arc::clone(&in_b) });
    let tb: Arc<dyn SignalTransport> = Arc::new(ToB { to_a: Arc::clone(&in_a) });
    let cfg = Config::default();

    let aud_a = Arc::new(AudSource::new());
    let scr_a = Arc::new(VidSource::new(VKind::Scr).expect("vsrc"));
    let (call_a, offer) =
        PeerCall::offer("e2e-5k", "alice", &cfg, ta.clone(), Some(aud_a), None, Some(Arc::clone(&scr_a))).await.expect("A offer");

    let aud_b = Arc::new(AudSource::new());
    let (call_b, answer) = PeerCall::answer(&offer, "bob", &cfg, tb.clone(), Some(aud_b), None, None).await.expect("B answer");
    tb.send(&answer).expect("send answer");

    let b_video = Arc::new(AtomicUsize::new(0));
    let b_video_dim = Arc::new(Mutex::new((0u32, 0u32)));
    {
        let bv = Arc::clone(&b_video);
        let bvd = Arc::clone(&b_video_dim);
        call_b.set_remote_screen_sink(move |rgba, w, h| {
            bv.fetch_add(rgba.len(), Ordering::Relaxed);
            *bvd.lock().unwrap() = (w, h);
        });
    }

    let connected_at = tokio::time::Instant::now() + tokio::time::Duration::from_secs(15);
    let mut pumping = true;
    while pumping && tokio::time::Instant::now() < connected_at {
        pump_once(&in_a, &call_a);
        pump_once(&in_b, &call_b);
        pumping = !is_connected(&call_a) || !is_connected(&call_b);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // Push a synthetic 5K frame repeatedly.
    let frame = make_frame(w, h, 255);
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        scr_a.push(&frame, w, h);
        if b_video.load(Ordering::Relaxed) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }

    let bv = b_video.load(Ordering::Relaxed);
    let (rw, rh) = *b_video_dim.lock().unwrap();
    tracing::debug!("5K RESULT: B received remote-screen bytes={bv} last_dim={}x{}", rw, rh);
    assert!(bv > 0, "BUG: 5120x2880 screen frames must still be delivered");
    // The encoder must have downscaled ≤ 3840×2160 (openh264's hard limit).
    assert!(rw <= 3840 && rh <= 2160, "downscaled dim {}x{} exceeds 3840x2160", rw, rh);
}

#[tokio::test]
async fn e2e_screen_share_loopback() {
    // Full desktop resolution: the pipeline must work at this size.
    let (w, h) = (2560u32, 1440u32);
    let in_a = Arc::new(Mutex::new(Vec::new()));
    let in_b = Arc::new(Mutex::new(Vec::new()));
    let ta: Arc<dyn SignalTransport> = Arc::new(ToA { to_b: Arc::clone(&in_b) });
    let tb: Arc<dyn SignalTransport> = Arc::new(ToB { to_a: Arc::clone(&in_a) });
    let cfg = Config::default();

    let aud_a = Arc::new(AudSource::new());
    let scr_a = Arc::new(VidSource::new(VKind::Scr).expect("vsrc"));
    let (call_a, offer) =
        PeerCall::offer(
            "e2e",
            "alice",
            &cfg,
            Arc::clone(&ta),
            Some(aud_a),
            None,
            Some(Arc::clone(&scr_a)),
        )
        .await
        .expect("A offer");

    let aud_b = Arc::new(AudSource::new());
    let (call_b, answer) = PeerCall::answer(
        &offer,
        "bob",
        &cfg,
        Arc::clone(&tb),
        Some(aud_b),
        None,
        None,
    )
    .await
    .expect("B answer");
    // The answerer explicitly transmits the answer (media crate does not auto-send).
    tb.send(&answer).expect("send answer");

    let b_video = Arc::new(AtomicUsize::new(0));
    let b_video_dim = Arc::new(Mutex::new((0u32, 0u32)));
    {
        let bv = Arc::clone(&b_video);
        let bvd = Arc::clone(&b_video_dim);
        call_b.set_remote_screen_sink(move |rgba, w, h| {
            bv.fetch_add(rgba.len(), Ordering::Relaxed);
            *bvd.lock().unwrap() = (w, h);
        });
    }

    // Pump both directions until connected (or 15s).
    let connected_at = tokio::time::Instant::now() + tokio::time::Duration::from_secs(15);
    let mut pumping = true;
    while pumping && tokio::time::Instant::now() < connected_at {
        pump_once(&in_a, &call_a); // A reads what B sent
        pump_once(&in_b, &call_b); // B reads what A sent
        pumping = !is_connected(&call_a) || !is_connected(&call_b);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    tracing::debug!(
        "after ICE: A state={:?} B state={:?}",
        call_a.peer_state(),
        call_b.peer_state()
    );

    let frame = make_frame(w, h, 255);
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        scr_a.push(&frame, w, h);
        if b_video.load(Ordering::Relaxed) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }

    let bv = b_video.load(Ordering::Relaxed);
    let (rw, rh) = *b_video_dim.lock().unwrap();
    tracing::debug!(
        "RESULT: B received remote-screen bytes={bv} last_dim={}x{}",
        rw, rh
    );
    assert!(bv > 0, "BUG REPRODUCED: remote_screen_sink never called");
}
