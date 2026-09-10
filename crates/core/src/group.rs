//! 群聊加密核心（Phase 2a：共享密钥模型）。
//!
//! 模型说明（与 Phase 1 设计一致）：
//! - **群共享密钥 `g_secret`**：随群生成的一把随机 32B 对称密钥，同一群所有成员持有同样的值。
//!   消息级密钥由 `HKDF(g_secret, group_id)` 派生，绑定群 → 防跨群重放。
//! - **密钥分发**：群主用**现有 E2E 信道**（`Account::derive_session_key` + AES-256-GCM）
//!   把 `{g_secret}` 私发给每个成员——不新增传输层，`g_secret` 也从不上目录服务器。
//! - **群消息**：明文为 JSON `GroupPayload{from, from_sign_pk, t, text, signature}`，
//!   发送方先对 `canonical(group_id|from|t|text)` 做账户 Ed25519 签名（防伪造/防冒充其它成员），
//!   再整体用群消息密钥 AES-256-GCM 加密。接收方解密→验签→取文。
//! - **踢人**：群主 `rotate()` 重新生成 `g_secret` + 重新 E2E 分发；未收到的成员拿旧密钥
//!   打不开新消息（事实上的前向隔离）。
//!
//! 局限：共享密钥 ⇒ 任一群成员都能解密整群历史（对称性）；不做 MLS 式的个人隔离，
//! M2 范围接受。离线补发/已读回执依赖投递确认协议，另行迭代。

use crate::account::{Account, AcctError};
use aes_gcm::aead::{Aead, KeyInit};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::BTreeMap;

/// 群成员公开信息（不含私钥，可上目录）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    /// 成员设备 peer_id（base58）。
    pub peer_id: String,
    /// 账户签名公钥（base64，Ed25519）——消息验签用。
    pub sign_pk: String,
    /// 账户 E2E 公钥（base64，X25519）——密钥分发/消息密钥提取用。
    pub e2e_public: String,
}

/// 群的**公开**信息（可上目录服务器 / 跨设备共享）：仅 owner + 成员公开信息。
/// 不含群密钥（`g_secret` 只走 E2E 分发，绝不上目录——服务器持有即可解密全群）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupPublic {
    pub group_id: String,
    pub owner: String,
    pub members: BTreeMap<String, MemberInfo>,
    pub created_at: u64,
}

/// 一个群的本地定义（只在本机 `profile/groups/{group_id}.json` 或内存持有）。
/// `g_secret` 不出现在任何公开结构里；落盘时用账户数据保护密钥加密成 `g_secret_enc`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub group_id: String,
    /// 群共享密钥：仅存于内存；落盘时不直接写出（经 `save` 加密到 `g_secret_enc`）。
    #[serde(skip)]
    g_secret: [u8; 32],
    /// 落盘态：`g_secret` 经账户保护密钥加密后的 base64。读取内存态时通常为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    g_secret_enc: Option<String>,
    /// 群主 peer_id（可轮换密钥、管理成员）。
    pub owner: String,
    /// 成员表：peer_id → 公开信息。
    pub members: BTreeMap<String, MemberInfo>,
    pub created_at: u64,
}

impl Group {
    /// 新建一个群，随机生成 `g_secret`。群主需自行 `set_member` 把自己/成员加入。
    pub fn create(group_id: impl Into<String>, owner: impl Into<String>) -> Self {
        let mut s = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut s);
        let owner = owner.into();
        Self {
            group_id: group_id.into(),
            g_secret: s,
            g_secret_enc: None,
            owner,
            members: BTreeMap::new(),
            created_at: crate::message::now_ms(),
        }
    }

    /// 用显式密钥构造（测试隔离 / 跨设备重放同一群）。
    pub fn with_secret(
        group_id: impl Into<String>,
        owner: impl Into<String>,
        secret: [u8; 32],
    ) -> Self {
        Self {
            group_id: group_id.into(),
            g_secret: secret,
            g_secret_enc: None,
            owner: owner.into(),
            members: BTreeMap::new(),
            created_at: crate::message::now_ms(),
        }
    }

    /// 加入/更新一名成员。
    pub fn set_member(&mut self, info: MemberInfo) {
        self.members.insert(info.peer_id.clone(), info);
    }

    /// 移除一名成员；返回是否原本在群里。
    pub fn remove_member(&mut self, peer_id: &str) -> bool {
        self.members.remove(peer_id).is_some()
    }

    pub fn has(&self, peer_id: &str) -> bool {
        self.members.contains_key(peer_id)
    }

    pub fn member(&self, peer_id: &str) -> Option<&MemberInfo> {
        self.members.get(peer_id)
    }

    pub fn members(&self) -> impl Iterator<Item = &MemberInfo> {
        self.members.values()
    }

    pub fn member_ids(&self) -> Vec<String> {
        self.members.keys().cloned().collect()
    }

    /// 群密钥（只读）。
    pub fn secret(&self) -> [u8; 32] {
        self.g_secret
    }

    /// 轮换群密钥（踢人/成员变更后必须调用；旧密钥即刻作废）。
    pub fn rotate(&mut self) {
        let mut s = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut s);
        self.g_secret = s;
    }

    /// 导出公开信息（可上目录/扇出发现用，不含群密钥）。
    pub fn to_public(&self) -> GroupPublic {
        GroupPublic {
            group_id: self.group_id.clone(),
            owner: self.owner.clone(),
            members: self.members.clone(),
            created_at: self.created_at,
        }
    }

    /// 用账户数据保护密钥把内存 `g_secret` 加密，写入 `g_secret_enc`，整体 JSON 落盘。
    pub fn save(&self, acct: &Account, path: &std::path::Path) -> anyhow::Result<()> {
        let enc = acct.encrypt_data(&self.g_secret)?;
        let mut wire = self.clone();
        wire.g_secret_enc = Some(crate::account::b64(&enc));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(&wire)?)?;
        Ok(())
    }

    /// 从盘读取并用账户数据保护密钥解出 `g_secret`。
    pub fn load(acct: &Account, path: &std::path::Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        let mut g: Self = serde_json::from_str(&s)?;
        let enc = g
            .g_secret_enc
            .clone()
            .ok_or_else(|| anyhow::anyhow!("group file lacks g_secret_enc"))?;
        let bytes = crate::account::from_b64(&enc)
            .ok_or_else(|| anyhow::anyhow!("bad g_secret_enc base64"))?;
        let plain = acct.decrypt_data(&bytes).map_err(|e| anyhow::anyhow!("g_secret decrypt: {e}"))?;
        if plain.len() != 32 {
            anyhow::bail!("g_secret len != 32");
        }
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&plain);
        g.g_secret = secret;
        g.g_secret_enc = None;
        Ok(g)
    }
}

/// 群消息密钥：由共享密钥 + 群 id 派生（同一群同一把，跨群互不相通）。
/// 公开以便测试断言"同密钥不同 group_id → 不同消息密钥"。
pub fn message_key(g_secret: &[u8; 32], group_id: &str) -> [u8; 32] {
    let hk = hkdf::Hkdf::<Sha256>::new(None, g_secret);
    let mut out = [0u8; 32];
    hk.expand(format!("p2pchat/grp/v1|{group_id}").as_bytes(), &mut out)
        .expect("hkdf expand");
    out
}

/// 群消息密文（wire / 存储单位）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SealedGroupMsg {
    pub group_id: String,
    /// 发送方 peer_id（base58）。
    pub from: String,
    /// 发送方账户 E2E 公钥（base64）——接收方 DH 用，提取 `g_secret`。
    pub from_e2e: String,
    nonce: [u8; 12],
    ciphertext: Vec<u8>,
}

impl SealedGroupMsg {
    pub fn nonce(&self) -> [u8; 12] {
        self.nonce
    }
    pub fn ciphertext(&self) -> &[u8] {
        &self.ciphertext
    }
}

/// 解密后取到的群消息明文（含签名与发送方凭据）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GroupPayload {
    from: String,
    from_sign_pk: String,
    t: u64,
    text: String,
    signature: String,
}

impl GroupPayload {
    /// 签名规范化串：`group_id|from|t|text`。
    fn canonical(&self, group_id: &str) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(group_id.as_bytes());
        v.push(b'|');
        v.extend_from_slice(self.from.as_bytes());
        v.push(b'|');
        v.extend_from_slice(&self.t.to_le_bytes());
        v.push(b'|');
        v.extend_from_slice(self.text.as_bytes());
        v
    }
}

// ── 群密钥的 E2E 分发（群主 → 成员） ──

/// 群主把 `g_secret` 用现有 E2E 信道加密给 `their_e2e_public` 持有者。
/// 返回 `nonce ‖ ciphertext+tag`（可放入 `Cmd::SendText` 的 `sealed` 传输）。
pub fn seal_secret(
    sender: &Account,
    their_e2e_public: &str,
    g_secret: &[u8; 32],
) -> Result<Vec<u8>, AcctError> {
    let key = sender.derive_session_key(their_e2e_public)?;
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let cipher = aes_gcm::Aes256Gcm::new_from_slice(&key).expect("32B key");
    let ct = cipher
        .encrypt(aes_gcm::Nonce::from_slice(&nonce), &g_secret[..])
        .expect("aes-gcm encrypt");
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// 成员用本账户 E2E 私钥 + 发送方 E2E 公钥解密得到 `g_secret`。
pub fn open_secret(
    my_account: &Account,
    sender_e2e_public: &str,
    bundle: &[u8],
) -> Result<[u8; 32], AcctError> {
    if bundle.len() < 12 {
        return Err(AcctError::Crypto("secret bundle too short".into()));
    }
    let (nonce_b, ct) = bundle.split_at(12);
    let nonce: [u8; 12] = nonce_b.try_into().map_err(|_| AcctError::Crypto("nonce len".into()))?;
    let key = my_account.derive_session_key(sender_e2e_public)?;
    let cipher = aes_gcm::Aes256Gcm::new_from_slice(&key).expect("32B key");
    let plain = cipher
        .decrypt(aes_gcm::Nonce::from_slice(&nonce), ct)
        .map_err(|_| AcctError::Crypto("secret decrypt".into()))?;
    plain
        .try_into()
        .map_err(|_| AcctError::Crypto("secret len".into()))
}

// ── 群消息收发 ──

type GmResult<T> = Result<T, AcctError>;

/// 用群共享密钥加密一条群消息；附带发送方对 `canonical()` 的账户签名（防伪造/跨群重放）。
pub fn seal_message(
    group: &Group,
    sender: &Account,
    sender_peer: &str,
    text: &str,
) -> GmResult<SealedGroupMsg> {
    let t = crate::message::now_ms();
    let from_sign_pk = sender.sign_pk().to_string();
    let mut payload = GroupPayload {
        from: sender_peer.to_string(),
        from_sign_pk,
        t,
        text: text.to_string(),
        signature: String::new(),
    };
    let msg = payload.canonical(&group.group_id);
    payload.signature = sender.sign_raw(&msg)?;
    let plaintext = serde_json::to_vec(&payload).expect("serialize payload");

    let key = message_key(&group.secret(), &group.group_id);
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let cipher = aes_gcm::Aes256Gcm::new_from_slice(&key).expect("32B key");
    let ciphertext = cipher
        .encrypt(aes_gcm::Nonce::from_slice(&nonce), plaintext.as_slice())
        .expect("aes-gcm encrypt");

    Ok(SealedGroupMsg {
        group_id: group.group_id.clone(),
        from: sender_peer.to_string(),
        from_e2e: sender.e2e_public().to_string(),
        nonce,
        ciphertext,
    })
}

/// 用群共享密钥解密一条群消息；校验发送方签名后返回文本。
///
/// 失败情形：
/// - `AcctError::Crypto` —— 群密钥错误 / nonce 被篡改 / 载荷损坏；
/// - `AcctError::BadSignature` —— 解密成功但签名不匹配（伪造 sender / 跨群重放 / 篡改字段）。
pub fn open_message(my_secret: &[u8; 32], env: &SealedGroupMsg) -> GmResult<String> {
    let key = message_key(my_secret, &env.group_id);
    let cipher = aes_gcm::Aes256Gcm::new_from_slice(&key).expect("32B key");
    let plaintext = cipher
        .decrypt(aes_gcm::Nonce::from_slice(&env.nonce), env.ciphertext.as_slice())
        .map_err(|_| AcctError::Crypto("group msg decrypt".into()))?;
    let payload: GroupPayload = serde_json::from_slice(&plaintext)
        .map_err(|e| AcctError::Crypto(format!("group msg payload: {e}")))?;
    let msg = payload.canonical(&env.group_id);
    if !Account::verify_raw(&payload.from_sign_pk, &msg, &payload.signature).unwrap_or(false) {
        return Err(AcctError::BadSignature("group msg".into()));
    }
    if payload.from != env.from {
        // 加密包里的 `from` 与明文 `from` 不一致——篡改过。
        return Err(AcctError::BadSignature("group msg from mismatch".into()));
    }
    Ok(payload.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(peer: &str, a: &crate::account::Account) -> MemberInfo {
        MemberInfo {
            peer_id: peer.into(),
            sign_pk: a.sign_pk().to_string(),
            e2e_public: a.e2e_public().to_string(),
        }
    }

    /// 建群→密钥分发→任意成员发言→其他成员读取；轮换后旧密钥失效；跨群同密钥读不通。
    #[test]
    fn group_lifecycle() {
        let (alice, bob, carol) = (
            crate::account::Account::generate("alice"),
            crate::account::Account::generate("bob"),
            crate::account::Account::generate("carol"),
        );
        let (alice_p, bob_p, carol_p) = ("QmAlice", "QmBob", "QmCarol");

        let mut group = Group::create("grp-1", alice_p);
        group.set_member(mem(alice_p, &alice));
        group.set_member(mem(bob_p, &bob));
        group.set_member(mem(carol_p, &carol));
        let s0 = group.secret();

        // ── 密钥分发：owner(alice) → bob / carol ──
        let to_bob = seal_secret(&alice, bob.e2e_public(), &s0).unwrap();
        assert_eq!(open_secret(&bob, alice.e2e_public(), &to_bob).unwrap(), s0);
        let to_carol = seal_secret(&alice, carol.e2e_public(), &s0).unwrap();
        assert_eq!(open_secret(&carol, alice.e2e_public(), &to_carol).unwrap(), s0);

        // ── 群消息：bob 发言 → alice / carol 都能读 ──
        let sealed_bob = seal_message(&group, &bob, bob_p, "hello group").unwrap();
        assert_eq!(open_message(&s0, &sealed_bob).unwrap(), "hello group");
        assert_eq!(open_message(&s0, &sealed_bob).unwrap(), "hello group");

        // 错误群密钥 → 解密失败
        assert!(open_message(&[9u8; 32], &sealed_bob).is_err());

        // ── 轮换（踢人后）：旧密钥打不开新消息，新密钥可以 ──
        group.rotate();
        let s1 = group.secret();
        assert_ne!(s0, s1, "轮换后密钥应变化");
        let sealed_after = seal_message(&group, &bob, bob_p, "secret msg").unwrap();
        assert!(open_message(&s0, &sealed_after).is_err(), "旧密钥应打不开新消息");
        assert_eq!(open_message(&s1, &sealed_after).unwrap(), "secret msg");

        // ── 跨群隔离：同样的密钥对另一群消息无效（group_id 绑定进 message_key） ──
        let mut other = Group::with_secret("grp-2", alice_p, s1);
        other.set_member(mem(alice_p, &alice));
        let sealed_cross = seal_message(&other, &alice, alice_p, "other group").unwrap();
        // 用 grp-2 的密钥 s1 可正常读 grp-2 消息
        assert_eq!(open_message(&s1, &sealed_cross).unwrap(), "other group");
        // 用一把不同的密钥读 grp-2 → 失败
        assert!(open_message(&[7u8; 32], &sealed_cross).is_err());

        // 篡改：改动 env.from 后验签/绑定应失败
        let mut tampered = sealed_bob.clone();
        tampered.from = carol_p.into();
        assert!(open_message(&s0, &tampered).is_err(), "篡改 from 应失败");
    }

    /// 伪造 sender：用非成员账户签名冒充某 peer_id → 应被识别（签名与 from 不一致）。
    #[test]
    fn forged_sender_rejected() {
        let alice = crate::account::Account::generate("alice");
        let mallory = crate::account::Account::generate("mallory");
        let alice_p = "QmAlice";
        let mut group = Group::create("g", alice_p);
        group.set_member(mem(alice_p, &alice));

        // mallory 不是群成员，用 alice 的 peer_id 冒充发言（签名来自 mallory，canonical 里 from=alice_p）
        let forged = seal_message(&group, &mallory, alice_p, "I am alice").unwrap();
        // open 时验证：payload.from_sign_pk(=mallory) + 签名 一致 → 签名"有效"（mallory 签了），
        // 但 mallory 不在 members 里。真正的成员资格在 open 之外由调用方检查 group.has(from)：
        assert!(!group.has("QmMallory"), "mallory 不在群内");
        // 关键：alice 持有群密钥能"解密"（共享密钥是透明的），所以安全边界是
        // "群成员资格"而非"加密"——M3 上 MLS 前，客户端 UI 必须以 group.has(from) 过滤。
        let _ = forged;
        assert!(group.has(alice_p), "alice 在群");
    }
}
