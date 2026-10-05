//! Smoke test: verify the offerer-side offer SDP contains media lines for
//! audio + video. No real network I/O.

use std::sync::Arc;

use media::{Config, PeerCall, SignalTransport, SignalFrame, AudSource, VidSource};

#[derive(Default, Clone)]
struct Null;
impl SignalTransport for Null {
    fn send(&self, _frame: &SignalFrame) -> media::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn offer_includes_audio_and_video_media_lines() {
    let cfg = Config::default();
    let (call, frame) = PeerCall::offer(
        "smoke",
        "alice",
        &cfg,
        Arc::new(Null),
        Some(Arc::new(AudSource::new())),
        Some(Arc::new(VidSource::new().expect("vsrc"))),
        None,
    )
    .await
    .expect("offer");

    let sdp = match &frame.kind {
        media::SignalKind::Offer(s) => s.clone(),
        other => panic!("expected Offer, got {other:?}"),
    };
    // m= lines: one for audio, one for video.
    let m_lines: Vec<&str> = sdp.lines().filter(|l| l.starts_with("m=")).collect();
    assert_eq!(m_lines.len(), 2, "expected 2 media lines, got {m_lines:?}");
    assert!(sdp.contains("audio"), "no audio m-line");
    assert!(sdp.contains("video"), "no video m-line");
    drop(call);
}
