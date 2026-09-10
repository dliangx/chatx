//! 方案 4（Signal-like 设备审批）集成测试。
//!
//! 覆盖：
//! - 首台设备 `bootstrap` → 自签证明 → APPROVED
//! - 第二台设备：拷贝 keystore → `login` → PENDING
//! - 首台（APPROVED）账号键 `approve_device` → 第二台变 APPROVED（带签名证明 + 通过校验）
//! - 伪造/篡改的审批证明 `is_valid()` 失败
//! - 两方账户的 E2E 会话密钥派生一致
//! - 口令错误时 keystore 无法打开
//!
//! 说明：仅验证 "身份/账户/审批/E2E" 的密码学与状态机逻辑，不依赖真实网络端点。

use chatx_core::Client;
use chatx_core::account::{Account, Attestation, AttestationAction, DeviceStatus, KDF_ITERATIONS};
use chatx_core::group::MemberInfo;
use chatx_core::identity::DeviceIdentity;
use chatx_core::signal::{DirectoryClient, InMemoryDirectory};

/// 隔离的临时 base 目录（避免污染 `~/.config/p2pchat`，也不依赖环境变量）。
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
        "root device 应自批为 APPROVED"
    );
    let alice_e2e = c1.e2e_public().to_string();
    let alice_sign_pk = c1.my_sign_pk().to_string();
    assert!(!alice_e2e.is_empty() && !alice_sign_pk.is_empty());
    assert!(acct1.has_secret());

    // ── ② 第二台设备：拷贝 keystore 到 deviceB → login → PENDING ──
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
    assert_eq!(c2.user_id(), "alice", "两设备同账户");
    assert_eq!(c2.e2e_public(), alice_e2e, "账户级 E2E 公钥应一致");
    assert_eq!(
        c2.status().expect("status"),
        DeviceStatus::Pending,
        "新登录设备应为 PENDING"
    );
    assert!(acct2.has_secret());

    // ── ③ 首台（APPROVED）批准第二台（PENDING） ──
    let peer_b = c2.peer_base58();
    let att = c1.approve_device(&peer_b).expect("approve deviceB");
    assert_eq!(att.device, peer_b);
    assert!(att.is_valid(), "批准证明应通过签名校验");

    // 目录里 deviceB 应变为 APPROVED（c1 与 c2 共享同一张表）
    let rec_b = c1.dir().resolve_device(&peer_b).expect("deviceB in dir");
    assert_eq!(rec_b.status, DeviceStatus::Approved, "批准后应 APPROVED");
    assert!(
        rec_b.attestation.as_ref().unwrap().is_valid(),
        "目录中的证明应有效"
    );

    // ── ④ 跨账户 E2E 会话密钥派生一致 ──
    let bob = Account::generate("bob");
    let key_a = c1.shared_key(bob.e2e_public()).unwrap();
    let key_b = bob.derive_session_key(&alice_e2e).unwrap();
    assert_eq!(key_a, key_b, "两方应派生同一把 AES-256 密钥");

    // ── ⑤ pending 列表：批准后 deviceB 不应再出现 ──
    assert!(
        c1.pending_devices().is_empty(),
        "批准后的账户不应有 pending 设备"
    );
}

/// 篡改/伪造审批证明应无法通过校验。
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
    assert!(good.is_valid(), "未篡改证明应通过");

    let mut bad_action = good.clone();
    bad_action.action = AttestationAction::Revoke;
    assert!(!bad_action.is_valid(), "篡改 action 应失败");

    let mut bad_device = good.clone();
    bad_device.device = "QmOther".into();
    assert!(!bad_device.is_valid(), "篡改 device 应失败");

    // 用另一个账户的公钥冒充 → 失败
    let evil = Account::generate("eve");
    let mut forged = good.clone();
    forged.approver_sign_pk = evil.sign_pk().to_string();
    assert!(!forged.is_valid(), "用别的账户公钥应失败");
}

/// 口令加密 keystore 的往返 + 错误口令拒绝。
#[tokio::test]
async fn keystore_password_roundtrip() {
    let acct = Account::generate("kara");
    let ks = acct
        .to_keystore("s3cret!", KDF_ITERATIONS)
        .expect("to_keystore");
    let opened = ks.open("s3cret!").expect("open with correct pass");
    assert_eq!(opened.e2e_public(), acct.e2e_public());
    assert_eq!(opened.sign_pk(), acct.sign_pk());
    assert!(ks.open("wrong").is_err(), "错误口令应失败");
}

/// 审批前提：本设备必须是 APPROVED，否则不能签发证明（防伪造）。
#[tokio::test]
async fn only_approved_can_attest() {
    let base = TmpBase::new("only-approved");
    let base_p = base.path().to_path_buf();
    let (dir1, dir2) = InMemoryDirectory::pair();

    // 首台设备 bootstrap（APPROVED）
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

    // deviceC（PENDING）尝试批准自己以外的设备应失败：它自己不是 APPROVED
    let peer_c = c3.peer_base58();
    let err = c2.approve_device(&peer_c);
    assert!(err.is_err(), "PENDING 设备不能签发审批证明");
    let msg = format!("{:?}", err.unwrap_err());
    assert!(
        msg.contains("APPROVED") || msg.contains("无权"),
        "错误信息应说明原因: {msg}"
    );
    let _ = c1;
}

/// 成员端入站处理：群主 announce+seal 密钥 → 成员 apply_inbound 落地本群 →
/// 成员解密群消息；轮换后重新分发密钥，成员更新本地密钥。
#[tokio::test]
async fn member_receives_group_key_and_msg() {
    let base = TmpBase::new("member-in");
    let (dir_a, dir_b) = InMemoryDirectory::pair();

    // alice = 群主；bob = 成员（独立 profile，避免同 peer）
    let (alice_c, _) = Client::bootstrap_in(base.path(), "mow", "alice", "p", dir_a.clone())
        .await
        .unwrap();
    let (bob_c, _) = Client::bootstrap_in(base.path(), "memb", "bob", "p", dir_b.clone())
        .await
        .unwrap();
    let (alice, bob) = (alice_c.account().clone(), bob_c.account().clone());

    const GID: &str = "team-in";
    // alice 建群（把 bob 作为成员）
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
    // 公布到目录（成员端建群要从这里拿成员表）
    alice_c.announce_group(GID).unwrap();

    // 群主把密钥 seal 给 bob（用 bob 的 E2E 公钥封装；DH 交换对称）
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

    // bob 处理入站群密钥 → 本地落地该群
    let handled = bob_c.apply_inbound(&key_req).unwrap().expect("GroupKey 应被处理");
    let bob_group = bob_c.get_group(GID).expect("bob 应已建出本地群");
    assert_eq!(bob_group.secret(), secret0, "bob 本地群密钥应与群主一致");
    assert!(matches!(handled, chatx_core::InboundGroup::Key { .. }));
    // bob 成员应已把群主/自己都补进成员表
    assert!(bob_group.has(&alice_c.peer_base58()), "群主应在 bob 的成员表");
    assert!(bob_group.has(&bob_c.peer_base58()), "bob 应在自己的成员表");

    // 群消息：alice 发言 → bob 接收解密
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
    let m = bob_c.apply_inbound(&req).unwrap().expect("GroupMsg 应被处理");
    match m {
        chatx_core::InboundGroup::Message { text, from, .. } => {
            assert_eq!(text, "hi bob");
            assert_eq!(from, alice_c.peer_base58());
        }
        _ => panic!("应返回 Message"),
    }
    // bob 入站已落盘
    let buf = bob_c.store().all();
    assert!(buf.iter().any(|s| s.chat_id == GID && !s.outgoing), "bob 入站群消息应落盘");

    // 轮换：群主换新密钥 → 旧密钥下 bob 打不开新消息（模拟被踢），
    // 重新分发密钥后 bob 更新本地密钥 → 能读
    alice_c.rotate_group(GID).unwrap();
    let post = alice_c.get_group(GID).unwrap();
    assert_ne!(post.secret(), secret0);
    let secret_new = post.secret();
    let sealed_post = alice_c.seal_group_message(GID, "after rotate").unwrap();
    assert!(
        chatx_core::group::open_message(&secret0, &sealed_post).is_err(),
        "旧密钥打不开新消息"
    );
    // 重新分发（用 bob 新密钥封）
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
    assert_eq!(now_group.secret(), secret_new, "bob 应更新为新密钥");
    assert_eq!(
        chatx_core::group::open_message(&now_group.secret(), &sealed_post).unwrap(),
        "after rotate"
    );
}

/// 群目录 API：发布群公开信息 → 其他成员解析 → 列表（InMemory 后端往返）。
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
    // 发布（绝不含群密钥）
    let pubinfo = g.to_public();
    assert_eq!(pubinfo.group_id, gid);
    dir.upsert_group(&pubinfo);
    // 解析
    let got = dir.resolve_group(gid).expect("resolve group");
    assert_eq!(got.group_id, gid);
    assert!(got.members.contains_key("QmCarol"), "群应含成员");
    // 列表
    assert_eq!(dir.list_groups().len(), 1);
    // 不存在的群 → Err
    assert!(dir.resolve_group("nope").is_err());
}

/// 客户端群聊完整流程：建群→加密落盘→重启载入→密钥 E2E 分发→群消息收发→轮换踢人后旧密钥失效。
#[tokio::test]
async fn client_group_flow() {
    let base = TmpBase::new("group");
    let (dir1, _dir2) = InMemoryDirectory::pair();

    // c1 = alice（群主，全新 bootstrap）
    let (c1, _) = Client::bootstrap_in(base.path(), "gowner", "alice", "pass", dir1.clone())
        .await
        .expect("bootstrap owner");
    assert_eq!(c1.user_id(), "alice");

    // c2 = bob：拷 keystore 到新 profile 后登录（同账户设备或另一账户均可，此处用另一账户以测跨账户分发）
    let (dirb, _) = InMemoryDirectory::pair();
    let (c2, acct_b) = Client::bootstrap_in(base.path(), "gmemb", "bob", "pass", dirb.clone())
        .await
        .expect("bootstrap bob");
    assert_eq!(c2.user_id(), "bob");
    assert!(acct_b.has_secret());

    // 建群：alice 为群主，把 bob 作为成员加入
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
    assert!(g.has(&c1.peer_base58()), "群主应在群内");
    assert!(g.has(&c2.peer_base58()), "bob 应在群内");
    // 群定义已加密落盘
    let gf = base.path().join("gowner").join("groups").join(format!("{gid}.json"));
    assert!(gf.exists(), "群定义应落盘");

    // 重启载入：同 profile 重新 bootstrap 应读回已有群（g_secret 解密成功）
    let (c1b, _) = Client::bootstrap_in(base.path(), "gowner", "alice", "pass", dir1.clone())
        .await
        .expect("bootstrap owner again");
    let g2 = c1b.get_group(gid).expect("重启后应载入同一群");
    assert_eq!(g2.secret(), g.secret(), "重启后群密钥应一致");

    // 密钥分发：alice →（E2E 信道）→ bob
    let s0 = g2.secret();
    let alice_acct = c1b.account().clone();
    let bob_acct = c2.account().clone();
    let bundle = chatx_core::group::seal_secret(&alice_acct, &bob_acct.e2e_public(), &s0)
        .expect("seal secret");
    let bob_secret = chatx_core::group::open_secret(&bob_acct, &alice_acct.e2e_public(), &bundle)
        .expect("open secret");
    assert_eq!(bob_secret, s0, "bob 应解出同一群密钥");

    // 群消息：alice 封印 → bob 解封（验签通过）
    let env = c1b.seal_group_message(gid, "hello team").expect("seal msg");
    let text = chatx_core::group::open_message(&bob_secret, &env).expect("bob open msg");
    assert_eq!(text, "hello team");
    // 入站落盘（bob 侧）
    c2.store_group_inbound(&env).expect("store inbound");
    // 出站落盘（alice 侧）
    c1b.store_group_outbound(&env).expect("store outbound");

    // bob 回发一条（bob 侧用群密钥重建 Group 对象以 seal）
    let mut bobg = chatx_core::group::Group::with_secret(gid, c2.peer_base58(), bob_secret);
    bobg.set_member(MemberInfo {
        peer_id: c2.peer_base58().into(),
        sign_pk: c2.my_sign_pk().into(),
        e2e_public: c2.e2e_public().into(),
    });
    let reply = chatx_core::group::seal_message(&bobg, &bob_acct, &c2.peer_base58(), "got it").expect("bob seal");
    let rt = chatx_core::group::open_message(&s0, &reply).expect("alice open reply");
    assert_eq!(rt, "got it");

    // 轮换（踢 bob）：旧密钥打不开新消息
    let g3 = c1b.rotate_group(gid).expect("rotate");
    assert_ne!(g3.secret(), s0, "轮换后密钥应变");
    let secret_env = c1b
        .seal_group_message(gid, "post-rot")
        .expect("seal post-rot");
    assert!(
        chatx_core::group::open_message(&s0, &secret_env).is_err(),
        "轮换后旧密钥应打不开新消息（kick）"
    );
    assert_eq!(
        chatx_core::group::open_message(&g3.secret(), &secret_env).unwrap(),
        "post-rot"
    );

    // 跨群重放隔离：grp-two 与 team-alpha **用同一把群密钥**，但因 group_id 不同，
    // message_key = HKDF(secret, group_id) 不同 → team-alpha 侧打不开 grp-two 的消息
    let same_secret = g3.secret();
    let mut other = chatx_core::group::Group::with_secret("grp-two", c1.peer_base58(), same_secret);
    other.set_member(MemberInfo {
        peer_id: c1.peer_base58().into(),
        sign_pk: c1.my_sign_pk().into(),
        e2e_public: c1.e2e_public().into(),
    });
    let cross = chatx_core::group::seal_message(&other, c1b.account(), &c1.peer_base58(), "x").expect("cross seal");
    // 用 grp-two 自己的同密钥可读
    assert_eq!(chatx_core::group::open_message(&same_secret, &cross).unwrap(), "x");
    // team-alpha(gid) 的 message_key（同密钥不同的 group_id）打不开
    let team_msg_key = chatx_core::group::message_key(&same_secret, gid);
    let cross_msg_key = chatx_core::group::message_key(&same_secret, "grp-two");
    assert_ne!(team_msg_key, cross_msg_key, "不同 group_id 派生出不同消息密钥");

    let _ = c1;
    let _ = c2;
}
