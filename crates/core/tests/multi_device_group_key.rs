// Verifies the user's exact scenario via the *add_members* path (not
// create_group): alice owns an empty group, then adds bob, who has TWO
// APPROVED devices (B and C). Both devices must end up with the group
// and the same secret.

use chatx_core::Client;
use chatx_core::group::MemberInfo;
use chatx_core::identity::DeviceIdentity;
use chatx_core::signal::{DirectoryClient, InMemoryDirectory};

struct TmpBase(std::path::PathBuf);

impl TmpBase {
    fn new(tag: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("chatx-test-{tag}-{}", std::process::id()));
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
        if let Some(addr) = c.endpoints().into_iter().find(|a| a.starts_with("/ip4/")) {
            return (c.peer_id(), addr.parse().expect("addr should parse"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!(
        "client should have at least one ip4 listen addr; got {:?}",
        c.endpoints()
    );
}

/// Mirrors the app's `add_group_member` fan-out: resolve the user, enumerate
/// their APPROVED devices (mirroring apps/chat/src/lib.rs:6484-6505), then
/// call `client.add_members` once with all of them. Verifies BOTH of bob's
/// devices end up with the group secret.
#[tokio::test(start_paused = false)]
async fn add_members_fans_out_to_every_approved_device() {
    let base = TmpBase::new("add-mem-fanout");
    let (dir_shared, _) = InMemoryDirectory::pair();

    let (alice, _) = Client::bootstrap_in(
        base.path(),
        "alice-a",
        "alice",
        "p",
        dir_shared.clone(),
    )
    .await
    .expect("bootstrap alice");
    let alice = std::sync::Arc::new(alice);

    let (bob_b, _acct_b) = Client::bootstrap_in(
        base.path(),
        "bob-b",
        "bob",
        "p",
        dir_shared.clone(),
    )
    .await
    .expect("bootstrap bob-b");
    let from = DeviceIdentity::base_keystore_path(base.path(), "bob-b");
    let to = DeviceIdentity::base_keystore_path(base.path(), "bob-c");
    let bytes = std::fs::read(&from).unwrap();
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::write(&to, bytes).unwrap();
    let (bob_c, _acct_c) = Client::login_in(
        base.path(),
        "bob-c",
        "p",
        "bob-c-label",
        dir_shared.clone(),
    )
    .await
    .expect("login bob-c");
    let peer_c = bob_c.peer_base58();
    let att = bob_b.approve_device(&peer_c).expect("approve bob-c");
    assert_eq!(att.device, peer_c);

    // Directory should now list both bob devices as APPROVED. (This is the
    // input the app's fan-out loop consumes.)
    let devices = dir_shared.list_devices("bob");
    let approved: Vec<String> = devices
        .iter()
        .filter(|d| d.status == chatx_core::account::DeviceStatus::Approved)
        .map(|d| d.peer_id.clone())
        .collect();
    assert!(approved.iter().any(|p| p == &bob_b.peer_base58()));
    assert!(approved.iter().any(|p| p == &peer_c));

    // Connect both bob devices to alice (mirrors real-world where each
    // device was already connected to alice by the time the add happens).
    let (b_peer, b_addr) = peer_addr(&bob_b).await;
    let (c_peer, c_addr) = peer_addr(&bob_c).await;
    alice
        .running()
        .cmd_tx
        .send(chatx_core::swarm::Cmd::Connect {
            peer: b_peer,
            addr: b_addr,
        })
        .unwrap();
    alice
        .running()
        .cmd_tx
        .send(chatx_core::swarm::Cmd::Connect {
            peer: c_peer,
            addr: c_addr,
        })
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    // Create an empty group (owner = alice).
    let gid = "add-mem-fanout-team";
    alice.create_group(gid, Vec::new()).expect("create empty group");
    let alice_secret = alice.get_group(gid).unwrap().secret();
    alice.announce_group(gid).expect("announce");

    // ---- Mirror the APP's add_group_member fan-out (apps/chat/src/lib.rs
    // ---- :6472-6505) verbatim: filter Approved, drop self, push into a
    // ---- single `infos` vec, then call `client.add_members` ONCE. ----
    let my_peer = alice.peer_base58();
    let bob_acct = bob_b.account().clone();
    let devices = dir_shared
        .list_devices("bob")
        .into_iter()
        .filter(|d| d.status == chatx_core::account::DeviceStatus::Approved)
        .filter(|d| !d.peer_id.is_empty() && d.peer_id != my_peer)
        .collect::<Vec<_>>();
    assert_eq!(devices.len(), 2, "both bob devices should fan out");
    let infos: Vec<MemberInfo> = devices
        .into_iter()
        .map(|d| MemberInfo {
            peer_id: d.peer_id.clone(),
            sign_pk: bob_b.my_sign_pk().to_string(),
            e2e_public: bob_acct.e2e_public().to_string(),
        })
        .collect();
    alice.add_members(gid, infos).expect("add_members should add both bob devices");

    // Poll both bob devices for the group secret.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let _ = bob_b.drain_inbound().await;
        let _ = bob_c.drain_inbound().await;
        let b = bob_b
            .get_group(gid)
            .map(|g| g.secret() == alice_secret)
            .unwrap_or(false);
        let c = bob_c
            .get_group(gid)
            .map(|g| g.secret() == alice_secret)
            .unwrap_or(false);
        if b && c {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out: alice_secret={} alice_group_members={:?} \
                 bob_b_has_group={} bob_c_has_group={}",
                format!("{:?}", &alice_secret[..4]),
                alice.get_group(gid).unwrap().member_ids(),
                bob_b.get_group(gid).is_some(),
                bob_c.get_group(gid).is_some()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
