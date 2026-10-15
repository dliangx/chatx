//! Smoke test: offerer-side SDP contains the expected media lines, and the
//! new mid-call upgrade fields round-trip through serde.

use std::sync::Arc;

use media::{AudSource, Config, PeerCall, SignalFrame, SignalKind, SignalTransport, VKind, VidSource};

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
        Some(Arc::new(VidSource::new(VKind::Cam).expect("vsrc"))),
        None,
    )
    .await
    .expect("offer");

    let sdp = match &frame.kind {
        SignalKind::Offer(s) => s.clone(),
        other => panic!("expected Offer, got {other:?}"),
    };
    let m_lines: Vec<&str> = sdp.lines().filter(|l| l.starts_with("m=")).collect();
    assert_eq!(m_lines.len(), 2, "expected 2 media lines, got {m_lines:?}");
    assert!(sdp.contains("audio"), "no audio m-line");
    assert!(sdp.contains("video"), "no video m-line");
    drop(call);
}

#[test]
fn signal_frame_upgrade_fields_roundtrip() {
    // No upgrade — legacy shape.
    let f = SignalFrame::new("c1", SignalKind::Offer("sdp".into()));
    let j = serde_json::to_string(&f).unwrap();
    let b: SignalFrame = serde_json::from_str(&j).unwrap();
    assert!(b.upgrade_kind.is_none());
    assert!(b.both_sides.is_none());

    // With upgrade (Cam, both_sides = true) — e.g. audio → video.
    let f = SignalFrame::new("c2", SignalKind::Offer("sdp".into()))
        .with_upgrade(VKind::Cam, true);
    let j = serde_json::to_string(&f).unwrap();
    let b: SignalFrame = serde_json::from_str(&j).unwrap();
    assert_eq!(b.upgrade_kind, Some(VKind::Cam));
    assert_eq!(b.both_sides, Some(true));

    // With upgrade (Scr, both_sides = false) — e.g. screen-recipient opening
    // their own camera.
    let f = SignalFrame::new("c3", SignalKind::Answer("sdp".into()))
        .with_upgrade(VKind::Scr, false);
    let b: SignalFrame = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
    assert_eq!(b.upgrade_kind, Some(VKind::Scr));
    assert_eq!(b.both_sides, Some(false));
}
