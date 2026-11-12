// Reproduces the user-reported bug at the core level:
//
//   "dliang adds bob (2 approved devices B, C); only 1 peer request is sent.
//    Expected: every APPROVED device of bob receives the group key."
//
// The fan-out code in `deliver_group_key_all` must be per-peer tolerant: a
// single dial/ack failure on one device must NOT abort the rest of the
// batch. This test puts one peer into a state where `deliver_group_key`
// will fail (its directory record has only a loopback endpoint, which is
// classified as unroutable), and asserts that the *other* peer still
// receives the secret. Under the previous single-`?`-abort design, the
// first failure would have stopped the loop and the other peer would not
// have received anything.

use chatx_core::Client;
use chatx_core::group::MemberInfo;
use chatx_core::identity::DeviceIdentity;
use chatx_core::signal::{DirectoryClient, InMemoryDirectory};

struct TmpBase(std::path::PathBuf);

impl TmpBase {
    fn new(tag: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("chatx-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&p);
        Self(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TmpBase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn peer_addr(c: &Client<InMemoryDirectory>) -> (libp2p::PeerId, libp2p::Multiaddr) {
    for _ in 0..100 {
        if let Some(addr) = c.endpoints().into_iter().find(|a| a.starts_with("/ip4/192")) {
            return (c.peer_id(), addr.parse().expect("addr should parse"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    panic!(
        "client should have a LAN ip4 listen addr; got {:?}",
        c.endpoints()
    );
}

/// Replace a device's directory `endpoints` with just a loopback/unroutable
/// list so that `deliver_group_key`'s dial path classifies every candidate
/// as unroutable and `dial_and_wait` will time out (returning `Ok(false)`).
/// This is the "one device unreachable" scenario we want to harden against.
fn clobber_endpoints(dir: &InMemoryDirectory, peer: &str) {
    let mut rec = dir
        .resolve_device(peer)
        .unwrap_or_else(|_| panic!("device {peer} should be in the directory"));
    rec.endpoints = vec!["/ip4/127.0.0.1/tcp/9/tcp/9".into()]; // loopback + duplicate port (unroutable)
    dir.upsert_device(&rec);
}

#[tokio::test(start_paused = false)]
async fn one_unreachable_peer_does_not_block_the_others() {
    let base = TmpBase::new("one-bad-peer");
    let (dir, _) = InMemoryDirectory::pair();

    // dliang (alice) — the owner.
    let (alice, _) = Client::bootstrap_in(base.path(), "alice", "alice", "p", dir.clone())
        .await
        .expect("bootstrap alice");
    let alice = std::sync::Arc::new(alice);

    // bob — two devices, one healthy, one with a broken directory record.
    let (bob_good, _acct) = Client::bootstrap_in(base.path(), "bob-g", "bob", "p", dir.clone())
        .await
        .expect("bootstrap bob (good)");
    let (c_keystore, to_path) =
        (DeviceIdentity::base_keystore_path(base.path(), "bob-g"),
         DeviceIdentity::base_keystore_path(base.path(), "bob-b"));
    std::fs::create_dir_all(to_path.parent().unwrap()).unwrap();
    std::fs::copy(&c_keystore, &to_path).unwrap();
    let (bob_bad, _) = Client::login_in(base.path(), "bob-b", "p", "bob-b", dir.clone())
        .await
        .expect("login bob (bad endpoint)");

    let peer_good = bob_good.peer_base58();
    let peer_bad = bob_bad.peer_base58();

    // Approve the second device from the first so both are Approved in the
    // directory (mirrors dliang approving bob's two devices).
    let att = bob_good.approve_device(&peer_bad).expect("approve bob-bad");
    assert_eq!(att.device, peer_bad);
    let good_rec = dir.resolve_device(&peer_good).unwrap();
    let bad_rec = dir.resolve_device(&peer_bad).unwrap();
    assert_eq!(
        good_rec.status,
        chatx_core::account::DeviceStatus::Approved,
        "bob-good should be Approved"
    );
    assert_eq!(
        bad_rec.status,
        chatx_core::account::DeviceStatus::Approved,
        "bob-bad should be Approved"
    );

    // Now break bob-bad's directory endpoints. `deliver_group_key` will try
    // each endpoint, classify it as unroutable, and fall off into the
    // "no dialable endpoint" branch — returning `Ok(false)`. This is the
    // "one device of a user unreachable" failure mode the user is hitting.
    clobber_endpoints(&dir, &peer_bad);

    // Connect alice → bob-good up front so the round-1 dial is a no-op (fast
    // path). bob-bad has no valid endpoint to dial, so it can only fail.
    let (good_peer_id, good_addr) = peer_addr(&bob_good).await;
    alice
        .running()
        .cmd_tx
        .send(chatx_core::swarm::Cmd::Connect {
            peer: good_peer_id,
            addr: good_addr,
        })
        .expect("connect alice -> bob-good");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Create the group with BOTH bob devices as members.
    let gid = "one-bad-peer-team";
    let bob_acct = bob_good.account().clone();
    alice
        .create_group(
            gid,
            vec![
                MemberInfo {
                    peer_id: peer_good.clone(),
                    sign_pk: bob_good.my_sign_pk().to_string(),
                    e2e_public: bob_acct.e2e_public().to_string(),
                },
                MemberInfo {
                    peer_id: peer_bad.clone(),
                    sign_pk: bob_good.my_sign_pk().to_string(),
                    e2e_public: bob_acct.e2e_public().to_string(),
                },
            ],
        )
        .expect("create group with both bob devices");
    let alice_secret = alice.get_group(gid).unwrap().secret();
    alice.announce_group(gid).expect("announce");

    // This is the call under test: dliang's group-key fan-out to every
    // approved member. bob-bad will fail (no dialable endpoint), but the
    // loop must still continue to bob-good. Use the synchronous
    // `deliver_group_key_all` so the returned count is the *real* delivery
    // tally (publish_group_key spawns a worker and returns immediately).
    let members = alice
        .get_group(gid)
        .unwrap()
        .member_ids()
        .into_iter()
        .filter(|p| *p != alice.peer_base58())
        .collect::<Vec<_>>();
    let n = alice
        .deliver_group_key_all(gid, &members)
        .await
        .expect("deliver_group_key_all should not hard-error on a partial failure");
    // Exactly one (bob-good) can deliver; bob-bad has no dialable endpoint.
    tracing::info!(delivered = n, "deliver_group_key_all returned");
    assert_eq!(
        n, 1,
        "exactly bob-good should have received the key; got {n}"
    );

    // Poll bob-good for the group secret within a bounded window. bob-bad
    // can never receive it (no dialable endpoint), but bob-good must.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let _ = bob_good.drain_inbound().await;
        let good_got = bob_good
            .get_group(gid)
            .map(|g| g.secret() == alice_secret)
            .unwrap_or(false);
        if good_got {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "bob-good never received the group key even though bob-bad is \
                 the unreachable one — the batch must have been aborted by the \
                 first failure"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // bob-bad should NOT have the group (it has no dialable endpoint), which
    // confirms the failure was isolated to that single peer rather than
    // aborting the whole batch.
    let bad_has = bob_bad.get_group(gid).is_some();
    assert!(!bad_has, "bob-bad (no dialable endpoint) should NOT have the group");
}
