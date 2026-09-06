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

use p2pchat_core::account::{Account, Attestation, AttestationAction, DeviceStatus, KDF_ITERATIONS};
use p2pchat_core::identity::DeviceIdentity;
use p2pchat_core::signal::{DirectoryClient, InMemoryDirectory};
use p2pchat_core::Client;

/// 隔离的临时 base 目录（避免污染 `~/.config/p2pchat`，也不依赖环境变量）。
struct TmpBase(std::path::PathBuf);

impl TmpBase {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "p2pchat-test-{}-{tag}",
            std::process::id()
        ));
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
    assert!(rec_b.attestation.as_ref().unwrap().is_valid(), "目录中的证明应有效");

    // ── ④ 跨账户 E2E 会话密钥派生一致 ──
    let bob = Account::generate("bob");
    let key_a = c1.shared_key(bob.e2e_public()).unwrap();
    let key_b = bob.derive_session_key(&alice_e2e).unwrap();
    assert_eq!(key_a, key_b, "两方应派生同一把 AES-256 密钥");

    // ── ⑤ pending 列表：批准后 deviceB 不应再出现 ──
    assert!(c1.pending_devices().is_empty(), "批准后的账户不应有 pending 设备");
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
    let ks = acct.to_keystore("s3cret!", KDF_ITERATIONS).expect("to_keystore");
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
    let (c1, _) = Client::bootstrap_in(&base_p, "deviceA", "alice", "p", dir1.clone()).await.unwrap();

    let copy_to = |profile: &str| {
        let from = DeviceIdentity::base_keystore_path(&base_p, "deviceA");
        let to = DeviceIdentity::base_keystore_path(&base_p, profile);
        let bytes = std::fs::read(&from).unwrap();
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::write(&to, bytes).unwrap();
    };

    copy_to("deviceB");
    let (c2, _) = Client::login_in(&base_p, "deviceB", "p", "b", dir2.clone()).await.unwrap();

    copy_to("deviceC");
    let (c3, _) = Client::login_in(&base_p, "deviceC", "p", "c", dir2.clone()).await.unwrap();

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
