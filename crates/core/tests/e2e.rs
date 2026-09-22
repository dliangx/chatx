
use chatx_core::Client;
use chatx_core::account::{Account, Attestation, AttestationAction, DeviceStatus, KDF_ITERATIONS};
use chatx_core::group::MemberInfo;
use chatx_core::identity::DeviceIdentity;
use chatx_core::signal::{DirectoryClient, InMemoryDirectory};

struct TmpBase(std::path::PathBuf);

impl TmpBase {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("p2pchat-test-{}-{tag}", std::process::id()));
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

#[tokio::test]
async fn full_scheme4_flow() {
    let base = TmpBase::new("flow");
    let (dir1, dir2) = InMemoryDirectory::pair();

    let (c1, acct1) = Client::bootstrap_in(base.path(), "deviceA", "alice", "pass-A", dir1.clone())
        .await
        .expect("bootstrap deviceA");
    assert_eq!(c1.user_id(), "alice");
    assert_eq!(
        c1.status().expect("status"),
        DeviceStatus::Approved,
        "root device should self-approve as APPROVED"
    );
    let alice_e2e = c1.e2e_public().to_string();
    let alice_sign_pk = c1.my_sign_pk().to_string();
    assert!(!alice_e2e.is_empty() && !alice_sign_pk.is_empty());
    assert!(acct1.has_secret());

    {
        let from = DeviceIdentity::base_keystore_path(base.path(), "deviceA");
        let to = DeviceIdentity::base_keystore_path(base.path(), "deviceB");
        let bytes = std::fs::read(&from).expect("read keystore");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::write(&to, bytes).expect("write keystore");
    }
    let (c2, acct2) = Client::login_in(base.path(), "deviceB", "pass-A", "deviceB", dir2.clone())
        .await
        .expect("login deviceB");
    assert_eq!(c2.user_id(), "alice", "both devices share one account");
    assert_eq!(c2.e2e_public(), alice_e2e, "account-level E2E key must match");
    assert_eq!(
        c2.status().expect("status"),
        DeviceStatus::Pending,
        "newly logged-in device should be PENDING"
    );
    assert!(acct2.has_secret());

    let peer_b = c2.peer_base58();
    let att = c1.approve_device(&peer_b).expect("approve deviceB");
    assert_eq!(att.device, peer_b);
    assert!(att.is_valid(), "attestation must pass signature check");

    let rec_b = c1.dir().resolve_device(&peer_b).expect("deviceB in dir");
    assert_eq!(rec_b.status, DeviceStatus::Approved, "deviceB should be APPROVED after approval");
    assert!(
        rec_b.attestation.as_ref().unwrap().is_valid(),
        "attestation in directory should be valid"
    );

    let bob = Account::generate("bob");
    let key_a = c1.shared_key(bob.e2e_public()).unwrap();
    let key_b = bob.derive_session_key(&alice_e2e).unwrap();
    assert_eq!(key_a, key_b, "both sides must derive the same AES-256 key");

    assert!(
        c1.pending_devices().is_empty(),
        "account should have no pending devices after approval"
    );
}

#[tokio::test]
async fn attestation_tamper_detection() {
    let acct = Account::generate("alice");
    let base = Attestation {
        user_id: "alice".into(),
        device: "QmFakeDevice".into(),
        device_pk: "AAAA".into(),
        action: AttestationAction::Approve,
        approver: "QmApprover".into(),
        approver_sign_pk: acct.sign_pk().to_string(),
        approved_at: 12345,
        signature: String::new(),
    };
    let sig = acct.sign_attestation(&base).expect("sign");
    let good = Attestation {
        signature: sig,
        ..base
    };
    assert!(good.is_valid(), "unmodified attestation should pass");

    let mut bad_action = good.clone();
    bad_action.action = AttestationAction::Revoke;
    assert!(!bad_action.is_valid(), "tampered action should fail");

    let mut bad_device = good.clone();
    bad_device.device = "QmOther".into();
    assert!(!bad_device.is_valid(), "tampered device should fail");

    let evil = Account::generate("eve");
    let mut forged = good.clone();
    forged.approver_sign_pk = evil.sign_pk().to_string();
    assert!(!forged.is_valid(), "forged with another account's key should fail");
}

#[tokio::test]
async fn keystore_password_roundtrip() {
    let acct = Account::generate("kara");
    let ks = acct
        .to_keystore("s3cret!", KDF_ITERATIONS)
        .expect("to_keystore");
    let opened = ks.open("s3cret!").expect("open with correct pass");
    assert_eq!(opened.e2e_public(), acct.e2e_public());
    assert_eq!(opened.sign_pk(), acct.sign_pk());
    assert!(ks.open("wrong").is_err(), "wrong passphrase should fail");
}

#[tokio::test]
async fn only_approved_can_attest() {
    let base = TmpBase::new("only-approved");
    let base_p = base.path().to_path_buf();
    let (dir1, dir2) = InMemoryDirectory::pair();

    let (c1, _) = Client::bootstrap_in(&base_p, "deviceA", "alice", "p", dir1.clone())
        .await
        .unwrap();

    let copy_to = |profile: &str| {
        let from = DeviceIdentity::base_keystore_path(&base_p, "deviceA");
        let to = DeviceIdentity::base_keystore_path(&base_p, profile);
        let bytes = std::fs::read(&from).unwrap();
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::write(&to, bytes).unwrap();
    };

    copy_to("deviceB");
    let (c2, _) = Client::login_in(&base_p, "deviceB", "p", "b", dir2.clone())
        .await
        .unwrap();

    copy_to("deviceC");
    let (c3, _) = Client::login_in(&base_p, "deviceC", "p", "c", dir2.clone())
        .await
        .unwrap();

    assert_eq!(c2.status().unwrap(), DeviceStatus::Pending);
    assert_eq!(c3.status().unwrap(), DeviceStatus::Pending);

    let peer_c = c3.peer_base58();
    let err = c2.approve_device(&peer_c);
    assert!(err.is_err(), "PENDING device cannot issue attestations");
    let msg = format!("{:?}", err.unwrap_err());
    assert!(
        msg.contains("APPROVED") || msg.contains("no permission"),
        "error message should explain the reason: {msg}"
    );
    let _ = c1;
}

#[tokio::test]
async fn member_receives_group_key_and_msg() {
    let base = TmpBase::new("member-in");
    let (dir_a, dir_b) = InMemoryDirectory::pair();

    let (alice_c, _) = Client::bootstrap_in(base.path(), "mow", "alice", "p", dir_a.clone())
        .await
        .unwrap();
    let (bob_c, _) = Client::bootstrap_in(base.path(), "memb", "bob", "p", dir_b.clone())
        .await
        .unwrap();
    let (alice, bob) = (alice_c.account().clone(), bob_c.account().clone());

    const GID: &str = "team-in";
    alice_c
        .create_group(
            GID,
            vec![MemberInfo {
                peer_id: bob_c.peer_base58().into(),
                sign_pk: bob_c.my_sign_pk().into(),
                e2e_public: bob_c.e2e_public().into(),
            }],
        )
        .unwrap();
    let secret0 = alice_c.get_group(GID).unwrap().secret();
    alice_c.announce_group(GID).unwrap();

    let bundle = chatx_core::group::seal_secret(&alice, &bob.e2e_public(), &secret0).unwrap();
    let key_req = chatx_core::message::ChatRequest {
        id: 1,
        from: alice_c.peer_base58(),
        e2e: alice.e2e_public().to_string(),
        text: None,
        sealed: Some(chatx_core::account::b64(&bundle)),
        kind: chatx_core::message::MsgKind::GroupKey,
        group_id: Some(GID.into()),
    };

    let handled = bob_c.apply_inbound(&key_req).unwrap().expect("GroupKey should be handled");
    let bob_group = bob_c.get_group(GID).expect("bob should have a local group");
    assert_eq!(bob_group.secret(), secret0, "bob's local group secret must match the owner's");
    assert!(matches!(handled, chatx_core::InboundGroup::Key { .. }));
    assert!(bob_group.has(&alice_c.peer_base58()), "owner should be in bob's member list");
    assert!(bob_group.has(&bob_c.peer_base58()), "bob should be in his own member list");

    let env = alice_c.seal_group_message(GID, "hi bob").unwrap();
    let req = chatx_core::message::ChatRequest {
        id: 2,
        from: alice_c.peer_base58(),
        e2e: alice.e2e_public().to_string(),
        text: None,
        sealed: Some(chatx_core::account::b64(
            &serde_json::to_vec(&env).unwrap(),
        )),
        kind: chatx_core::message::MsgKind::GroupMsg,
        group_id: Some(GID.into()),
    };
    let m = bob_c.apply_inbound(&req).unwrap().expect("GroupMsg should be handled");
    match m {
        chatx_core::InboundGroup::Message { text, from, .. } => {
            assert_eq!(text, "hi bob");
            assert_eq!(from, alice_c.peer_base58());
        }
        _ => panic!("should return Message"),
    }
    let buf = bob_c.store().all();
    let me = bob_c.peer_base58();
    assert!(buf.iter().any(|s| s.chat_id == GID && !s.is_outgoing(&me)), "bob's inbound group message should be stored");

    alice_c.rotate_group(GID).unwrap();
    let post = alice_c.get_group(GID).unwrap();
    assert_ne!(post.secret(), secret0);
    let secret_new = post.secret();
    let sealed_post = alice_c.seal_group_message(GID, "after rotate").unwrap();
    assert!(
        chatx_core::group::open_message(&secret0, &sealed_post).is_err(),
        "old secret must not open new message"
    );
    let bundle2 = chatx_core::group::seal_secret(&alice, &bob.e2e_public(), &secret_new).unwrap();
    let key_req2 = chatx_core::message::ChatRequest {
        id: 3,
        from: alice_c.peer_base58(),
        e2e: alice.e2e_public().to_string(),
        text: None,
        sealed: Some(chatx_core::account::b64(&bundle2)),
        kind: chatx_core::message::MsgKind::GroupKey,
        group_id: Some(GID.into()),
    };
    bob_c.apply_inbound(&key_req2).unwrap();
    let now_group = bob_c.get_group(GID).unwrap();
    assert_eq!(now_group.secret(), secret_new, "bob should have the new secret");
    assert_eq!(
        chatx_core::group::open_message(&now_group.secret(), &sealed_post).unwrap(),
        "after rotate"
    );
}

#[test]
fn group_directory_roundtrip() {
    let dir = InMemoryDirectory::new();
    let acct = Account::generate("carol");
    let gid = "team-dir";
    let mut g = chatx_core::group::Group::create(gid, "QmCarol");
    g.set_member(MemberInfo {
        peer_id: "QmCarol".into(),
        sign_pk: acct.sign_pk().into(),
        e2e_public: acct.e2e_public().into(),
    });
    let pubinfo = g.to_public();
    assert_eq!(pubinfo.group_id, gid);
    dir.upsert_group(&pubinfo);
    let got = dir.resolve_group(gid).expect("resolve group");
    assert_eq!(got.group_id, gid);
    assert!(got.members.contains_key("QmCarol"), "group should contain members");
    assert_eq!(dir.list_groups().len(), 1);
    assert!(dir.resolve_group("nope").is_err());
}

#[tokio::test]
async fn client_group_flow() {
    let base = TmpBase::new("group");
    let (dir1, _dir2) = InMemoryDirectory::pair();

    let (c1, _) = Client::bootstrap_in(base.path(), "gowner", "alice", "pass", dir1.clone())
        .await
        .expect("bootstrap owner");
    assert_eq!(c1.user_id(), "alice");

    let (dirb, _) = InMemoryDirectory::pair();
    let (c2, acct_b) = Client::bootstrap_in(base.path(), "gmemb", "bob", "pass", dirb.clone())
        .await
        .expect("bootstrap bob");
    assert_eq!(c2.user_id(), "bob");
    assert!(acct_b.has_secret());

    let gid = "team-alpha";
    let g = c1
        .create_group(
            gid,
            vec![MemberInfo {
                peer_id: c2.peer_base58().into(),
                sign_pk: c2.my_sign_pk().into(),
                e2e_public: c2.e2e_public().into(),
            }],
        )
        .expect("create group");
    assert!(g.has(&c1.peer_base58()), "owner should be in the group");
    assert!(g.has(&c2.peer_base58()), "bob should be in the group");
    let gf = base.path().join("gowner").join("groups").join(format!("{gid}.json"));
    assert!(gf.exists(), "group definition must be persisted");

    let (c1b, _) = Client::bootstrap_in(base.path(), "gowner", "alice", "pass", dir1.clone())
        .await
        .expect("bootstrap owner again");
    let g2 = c1b.get_group(gid).expect("same group should be loaded after restart");
    assert_eq!(g2.secret(), g.secret(), "group secret must survive restart");

    let s0 = g2.secret();
    let alice_acct = c1b.account().clone();
    let bob_acct = c2.account().clone();
    let bundle = chatx_core::group::seal_secret(&alice_acct, &bob_acct.e2e_public(), &s0)
        .expect("seal secret");
    let bob_secret = chatx_core::group::open_secret(&bob_acct, &alice_acct.e2e_public(), &bundle)
        .expect("open secret");
    assert_eq!(bob_secret, s0, "bob should open the same group secret");

    let env = c1b.seal_group_message(gid, "hello team").expect("seal msg");
    let text = chatx_core::group::open_message(&bob_secret, &env).expect("bob open msg");
    assert_eq!(text, "hello team");
    c2.store_group_inbound(&env).expect("store inbound");
    c1b.store_group_outbound(&env).expect("store outbound");

    let mut bobg = chatx_core::group::Group::with_secret(gid, c2.peer_base58(), bob_secret);
    bobg.set_member(MemberInfo {
        peer_id: c2.peer_base58().into(),
        sign_pk: c2.my_sign_pk().into(),
        e2e_public: c2.e2e_public().into(),
    });
    let reply = chatx_core::group::seal_message(&bobg, &bob_acct, &c2.peer_base58(), "got it").expect("bob seal");
    let rt = chatx_core::group::open_message(&s0, &reply).expect("alice open reply");
    assert_eq!(rt, "got it");

    let g3 = c1b.rotate_group(gid).expect("rotate");
    assert_ne!(g3.secret(), s0, "secret must change after rotation");
    let secret_env = c1b
        .seal_group_message(gid, "post-rot")
        .expect("seal post-rot");
    assert!(
        chatx_core::group::open_message(&s0, &secret_env).is_err(),
        "old secret must not open new message after rotation (kick)"
    );
    assert_eq!(
        chatx_core::group::open_message(&g3.secret(), &secret_env).unwrap(),
        "post-rot"
    );

    let same_secret = g3.secret();
    let mut other = chatx_core::group::Group::with_secret("grp-two", c1.peer_base58(), same_secret);
    other.set_member(MemberInfo {
        peer_id: c1.peer_base58().into(),
        sign_pk: c1.my_sign_pk().into(),
        e2e_public: c1.e2e_public().into(),
    });
    let cross = chatx_core::group::seal_message(&other, c1b.account(), &c1.peer_base58(), "x").expect("cross seal");
    assert_eq!(chatx_core::group::open_message(&same_secret, &cross).unwrap(), "x");
    let team_msg_key = chatx_core::group::message_key(&same_secret, gid);
    let cross_msg_key = chatx_core::group::message_key(&same_secret, "grp-two");
    assert_ne!(team_msg_key, cross_msg_key, "different group_id must derive different message keys");

    let _ = c1;
    let _ = c2;
}

async fn peer_addr(
    c: &Client<InMemoryDirectory>,
) -> (libp2p::PeerId, libp2p::Multiaddr) {
    for _ in 0..100 {
        if let Some(addr) = c
            .endpoints()
            .into_iter()
            .find(|a| a.starts_with("/ip4/"))
        {
            return (
                c.peer_id(),
                addr.parse().expect("addr should parse"),
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("client should have at least one ip4 listen addr; got {:?}", c.endpoints())
}

#[tokio::test(start_paused = false)]
async fn group_messages_flow_over_gossipsub() {
    let base = TmpBase::new("gossipsub");
    let (dir_a, dir_b) = InMemoryDirectory::pair();

    let (alice, _) = Client::bootstrap_in(base.path(), "fa", "alice", "p", dir_a.clone())
        .await
        .expect("bootstrap alice");

    let (mut bob, _) = Client::bootstrap_in(base.path(), "fb", "bob", "p", dir_b.clone())
        .await
        .expect("bootstrap bob");

    let gid = "gossipsub-team";
    alice
        .create_group(
            gid,
            vec![MemberInfo {
                peer_id: bob.peer_base58(),
                sign_pk: bob.my_sign_pk().to_string(),
                e2e_public: bob.e2e_public().to_string(),
            }],
        )
        .expect("create group");
    alice.announce_group(gid).expect("announce");

    // Connect the two peers
    let (bob_peer, bob_addr) = peer_addr(&bob).await;
    alice
        .running()
        .cmd_tx
        .send(chatx_core::swarm::Cmd::Connect { peer: bob_peer, addr: bob_addr })
        .expect("connect cmd");

    // Give the swarm a moment to establish the gossipsub substream.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Bob subscribes to the group topic so he can receive the key packet.
    bob.join_group(gid).expect("bob joins topic");

    // Now distribute the group key to bob (one sealed bundle per member,
    // each individually E2E-sealed under the member's own account key).
    alice.publish_group_key(gid).expect("publish key to bob");

    // Poll drain_inbound until bob receives the key.
    for _ in 0..200 {
        let touched = bob.drain_inbound();
        if touched.iter().any(|g| g == gid) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let bob_g = bob.get_group(gid).expect("bob should have the group after key pub");
    let alice_secret = alice.get_group(gid).expect("alice should have the group").secret();
    assert_eq!(bob_g.secret(), alice_secret, "bob must have the same secret after key delivery");
    assert!(bob_g.has(&bob.peer_base58()), "bob is member of own group");
    assert!(bob_g.has(&alice.peer_base58()), "owner is member of group");

    // Now send a group message from alice to bob via gossipsub.
    alice.send_group_message(gid, "hi over gossipsub").expect("send group msg");

    let bob_seen = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let b = &mut bob;
        loop {
            let touched = b.drain_inbound();
            if touched.iter().any(|g| g == gid) {
                // Pull the stored message
                let all = b.store().all();
                if let Some(msg) = all.iter().find(|m| m.chat_id == gid && m.sender == alice.peer_base58()) {
                    return msg.text.clone();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("bob should receive the group message over gossipsub");

    // The stored text is the sealed envelope (base64) — decrypt via open_group_message
    // and verify it round-trips back to the original plaintext.
    let secret = alice.get_group(gid).unwrap().secret();
    use base64::Engine;
    let env_bytes = base64::engine::general_purpose::STANDARD.decode(&bob_seen).expect("b64");
    let env: chatx_core::group::SealedGroupMsg = serde_json::from_slice(&env_bytes).expect("env");
    assert_eq!(
        chatx_core::group::open_message(&secret, &env).unwrap(),
        "hi over gossipsub"
    );
}

#[tokio::test(start_paused = false)]
async fn add_member_receives_group_key() {
    let base = TmpBase::new("addmem");
    let (dir_a, dir_b) = InMemoryDirectory::pair();

    let (mut alice, _) = Client::bootstrap_in(base.path(), "am-a", "alice", "p", dir_a.clone())
        .await
        .expect("bootstrap alice");
    let (mut bob, _) = Client::bootstrap_in(base.path(), "am-b", "bob", "p", dir_b.clone())
        .await
        .expect("bootstrap bob");

    let gid = "addmem-team";
    // alice creates the group WITHOUT bob; bob has no key at this point.
    alice.create_group(gid, Vec::new()).expect("create group (no members)");
    alice.announce_group(gid).expect("announce");

    let (bob_peer, bob_addr) = peer_addr(&bob).await;
    alice
        .running()
        .cmd_tx
        .send(chatx_core::swarm::Cmd::Connect { peer: bob_peer, addr: bob_addr })
        .expect("connect cmd");
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    // Adding bob to the group must redistribute the secret to bob.
    alice
        .add_member(
            gid,
            MemberInfo {
                peer_id: bob.peer_base58(),
                sign_pk: bob.my_sign_pk().to_string(),
                e2e_public: bob.e2e_public().to_string(),
            },
        )
        .expect("add bob as member (triggers key redistribution)");

    // Poll (draining inbound events) until bob has the group with a matching secret.
    for _ in 0..200 {
        let _ = bob.drain_inbound();
        if bob
            .get_group(gid)
            .map(|g| g.secret() == alice.get_group(gid).unwrap().secret())
            .unwrap_or(false)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let bob_g = bob.get_group(gid).expect("bob should have the group after add_member");
    assert!(bob_g.has(&bob.peer_base58()), "bob is a member");
    assert_eq!(
        bob_g.secret(),
        alice.get_group(gid).unwrap().secret(),
        "bob's secret must match alice's after add_member"
    );

    // Bob can now send a group message that alice (already a member) receives.
    bob.send_group_message(gid, "bob joined via add_member").expect("bob sends");
    for _ in 0..200 {
        let _ = alice.drain_inbound();
        if alice
            .store()
            .all()
            .iter()
            .any(|m| m.chat_id == gid && m.sender == bob.peer_base58())
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let all = alice.store().all();
    let stored = all
        .iter()
        .find(|m| m.chat_id == gid && m.sender == bob.peer_base58())
        .expect("alice should have stored bob's group message");
    let secret = alice.get_group(gid).unwrap().secret();
    use base64::Engine;
    let env_bytes = base64::engine::general_purpose::STANDARD.decode(&stored.text).unwrap();
    let env: chatx_core::group::SealedGroupMsg = serde_json::from_slice(&env_bytes).unwrap();
    assert_eq!(
        chatx_core::group::open_message(&secret, &env).unwrap(),
        "bob joined via add_member"
    );
}
