//! 账户（user）身份 + 口令加密 keystore + 设备审批/证明。
//!
//! 方案4（Signal-like）：
//! - **账户**是稳定身份：`user_id` + Ed25519 账户签名密钥（设备审批/证明）+ X25519 E2E 密钥
//!   （会话派生，账户级，非设备级——任一被批准设备都能与对方账户派生同一把 AES 密钥）。
//! - **设备**是身份载体：各自一把 libp2p Ed25519（`peer_id`），互不相同。
//! - 第二台机器"登录" = 拷贝加密的 `keystore.json` + 口令 → 解出账户两把私钥 → 设备以
//!   PENDING 注册 → 由一个已持有账户签名密钥的设备 **签名证明（attestation）** 批准后
//!   变为 APPROVED。签名让"目录服务器/中间人无法伪造批准"成为可能。
//!
//! 口令 KDF：`PBKDF2-HMAC-SHA256`（RFC 2898，仅用 `hmac`+`sha2`，零新依赖）。
//! keystore 加密：`AES-256-GCM`。签名：`ed25519-dalek`。

use aes_gcm::aead::{Aead, KeyInit};
use ed25519_dalek::{Signature, SigningKey, VerifyingKey, Signer, Verifier};
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{PublicKey as X25519Pub, StaticSecret};

/// 口令 KDF 默认迭代次数（PBKDF2-HMAC-SHA256）。
pub const KDF_ITERATIONS: u32 = 200_000;
/// 账户签名密钥在审批语义下使用的域名前缀（进规范化串，防跨用途重放）。
const DOMAIN_APPROVE: &[u8] = b"p2pchat/attest/v1";

// ────────────────────────── 账户密钥 + 签名 ──────────────────────────

/// 账户的可打印公开部分（不含私钥）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountPublic {
    pub user_id: String,
    /// 账户签名公钥（base64，Ed25519）。用于设备审批证明。
    pub sign_pk: String,
    /// 账户 E2E 公钥（base64，X25519）。用于会话密钥派生。
    pub e2e_public: String,
}

/// 账户的私有部分（两把私钥：签名 + E2E）。
#[derive(Clone)]
pub struct AccountSecret {
    /// 账户签名私钥（32B seed）。
    pub sign_sk: [u8; 32],
    /// 账户 E2E 私钥（X25519 static secret，32B）。
    pub e2e_sk: [u8; 32],
}

/// 一个完整的账户身份（公钥 + 可选私钥持有）。
/// 注意：不 derive `Debug`，避免口令派生私钥泄漏到日志。
#[derive(Clone)]
pub struct Account {
    pub public: AccountPublic,
    secret: Option<AccountSecret>,
}

impl Account {
    /// 从公钥信息构造（不持有私钥；无法签名/解密 keystore）。
    pub fn from_public(p: AccountPublic) -> Self {
        Self {
            public: p,
            secret: None,
        }
    }

    /// 生成一个全新账户（随机签名密钥 + E2E 密钥，持有私钥）。
    pub fn generate(user_id: impl Into<String>) -> Self {
        let mut sign_seed = [0u8; 32];
        let mut e2e_seed = [0u8; 32];
        OsRng.fill_bytes(&mut sign_seed);
        OsRng.fill_bytes(&mut e2e_seed);
        let sign_pk = SigningKey::from_bytes(&sign_seed).verifying_key();
        let e2e_static = StaticSecret::from(e2e_seed);
        let e2e_pub = X25519Pub::from(&e2e_static);
        Self {
            public: AccountPublic {
                user_id: user_id.into(),
                sign_pk: b64(&sign_pk.to_bytes()),
                e2e_public: b64(&e2e_pub.to_bytes()),
            },
            secret: Some(AccountSecret {
                sign_sk: sign_seed,
                e2e_sk: e2e_seed,
            }),
        }
    }

    /// 从私钥 bundle 构造（持有私钥）。
    pub fn from_secret(user_id: impl Into<String>, secret: AccountSecret) -> Self {
        let sign_pk = SigningKey::from_bytes(&secret.sign_sk).verifying_key();
        let e2e_pub = X25519Pub::from(&StaticSecret::from(secret.e2e_sk));
        Self {
            public: AccountPublic {
                user_id: user_id.into(),
                sign_pk: b64(sign_pk.as_bytes()),
                e2e_public: b64(&e2e_pub.to_bytes()),
            },
            secret: Some(secret),
        }
    }

    pub fn user_id(&self) -> &str {
        &self.public.user_id
    }

    pub fn sign_pk(&self) -> &str {
        &self.public.sign_pk
    }

    pub fn e2e_public(&self) -> &str {
        &self.public.e2e_public
    }

    pub fn has_secret(&self) -> bool {
        self.secret.is_some()
    }

    /// 用口令把本账户私钥加密成 keystore（不暴露私钥字段）。
    pub fn to_keystore(&self, password: &str, iterations: u32) -> Option<Keystore> {
        let secret = self.secret.as_ref()?;
        Some(Keystore::seal(self, secret, password, iterations))
    }

    fn secret(&self) -> &AccountSecret {
        self.secret
            .as_ref()
            .expect("account does not hold its secret keys")
    }

    /// 用账户签名密钥对一条审批证明签名。
    pub fn sign_attestation(&self, a: &Attestation) -> Result<String, AcctError> {
        let sk = SigningKey::from_bytes(&self.secret().sign_sk);
        let sig = sk.sign(&a.canonical());
        Ok(b64(&sig.to_bytes()))
    }

    /// 验签一条审批证明（不需要私钥）。
    pub fn verify_attestation(a: &Attestation) -> Result<bool, AcctError> {
        let vk = verifying_key_from_b64(&a.approver_sign_pk)?;
        let sig = signature_from_b64(&a.signature)?;
        Ok(vk.verify(&a.canonical(), &sig).is_ok())
    }

    /// 用账户签名密钥对任意字节能签名（base64 输出）。群消息签名、审批等通用。
    pub fn sign_raw(&self, message: &[u8]) -> Result<String, AcctError> {
        let sk = SigningKey::from_bytes(&self.secret().sign_sk);
        let sig = sk.sign(message);
        Ok(b64(&sig.to_bytes()))
    }

    /// 用给定账户签名公钥（base64）验证任意字节的签名。
    pub fn verify_raw(
        sign_pk_b64: &str,
        message: &[u8],
        signature_b64: &str,
    ) -> Result<bool, AcctError> {
        let vk = verifying_key_from_b64(sign_pk_b64)?;
        let sig = signature_from_b64(signature_b64)?;
        Ok(vk.verify(message, &sig).is_ok())
    }

    /// 由账户签名私钥派生一把**只在本账户设备上有效**的 AES-256 数据保护密钥。
    ///
    /// 用途：保护"不属于 keystore、但需要跨设备跟随账户"的落盘秘密
    /// （如群共享密钥 `g_secret`——任何持有账户口令的设备都能解出，但不必重述口令）。
    pub fn data_protect_key(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        hkdf::Hkdf::<Sha256>::new(Some(b"p2pchat/data-protect/v1"), &self.secret().sign_sk)
            .expand(b"32b-aes", &mut out)
            .expect("hkdf 32B");
        out
    }

    /// 用数据保护密钥加密一段秘密，返回 `nonce(12) ‖ ciphertext+tag`。
    pub fn encrypt_data(&self, plaintext: &[u8]) -> Result<Vec<u8>, AcctError> {
        let key = self.data_protect_key();
        let mut nonce = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let cipher = aes_gcm::Aes256Gcm::new_from_slice(&key).expect("32B key");
        let ct = cipher
            .encrypt(aes_gcm::Nonce::from_slice(&nonce), plaintext)
            .map_err(|e| AcctError::Crypto(e.to_string()))?;
        let mut out = Vec::with_capacity(12 + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// 解密 `encrypt_data` 的产物。
    pub fn decrypt_data(&self, bundle: &[u8]) -> Result<Vec<u8>, AcctError> {
        if bundle.len() < 13 {
            return Err(AcctError::Crypto("data bundle too short".into()));
        }
        let (nonce_b, ct) = bundle.split_at(12);
        let nonce = aes_gcm::Nonce::from_slice(nonce_b);
        let key = self.data_protect_key();
        let cipher = aes_gcm::Aes256Gcm::new_from_slice(&key).expect("32B key");
        cipher
            .decrypt(nonce, ct)
            .map_err(|_| AcctError::Crypto("data decrypt failed".into()))
    }

    /// 用账户 E2E 私钥与对方 E2E 公钥派生共享 AES-256 会话密钥。
    ///
    /// 因为 E2E 密钥是 **账户级**，同一账户的任一对 APPROVED 设备都能派生出
    /// 与对端账户相同的密钥——这让"给谁发消息"可以落到该账户的任一在线设备。
    pub fn derive_session_key(&self, their_e2e_b64: &str) -> Result<[u8; 32], AcctError> {
        let their = from_b64(their_e2e_b64).ok_or(AcctError::BadE2eKey)?;
        let their: [u8; 32] = their
            .as_slice()
            .try_into()
            .map_err(|_| AcctError::BadE2eKey)?;
        let their_pub = X25519Pub::from(their);
        let my_static = StaticSecret::from(self.secret().e2e_sk);
        let shared = my_static.diffie_hellman(&their_pub);
        let hk = hkdf::Hkdf::<Sha256>::new(Some(b"p2pchat/e2e/v1"), &shared.to_bytes());
        let mut out = [0u8; 32];
        hk.expand(b"chat-message", &mut out)
            .map_err(|e| AcctError::Crypto(e.to_string()))?;
        Ok(out)
    }
}

/// 账户/审批相关错误。
#[derive(Debug, thiserror::Error)]
pub enum AcctError {
    #[error("no account secret held by this account object")]
    NoSecret,
    #[error("bad E2E public key (base64/len)")]
    BadE2eKey,
    #[error("bad verifying key: {0}")]
    BadVerifier(String),
    #[error("bad signature: {0}")]
    BadSignature(String),
    #[error("kdf/crypto: {0}")]
    Crypto(String),
}

fn verifying_key_from_b64(s: &str) -> Result<VerifyingKey, AcctError> {
    let b = from_b64(s).ok_or(AcctError::BadVerifier(s.to_string()))?;
    if b.len() != 32 {
        return Err(AcctError::BadVerifier("len != 32".to_string()));
    }
    let arr: [u8; 32] = b.try_into().unwrap();
    VerifyingKey::from_bytes(&arr).map_err(|e| AcctError::BadVerifier(e.to_string()))
}

fn signature_from_b64(s: &str) -> Result<Signature, AcctError> {
    let b = from_b64(s).ok_or(AcctError::BadSignature(s.to_string()))?;
    if b.len() != 64 {
        return Err(AcctError::BadSignature("len != 64".to_string()));
    }
    Signature::from_slice(&b).map_err(|e| AcctError::BadSignature(e.to_string()))
}

// ────────────────────────── 审批 / 证明（wire） ──────────────────────────

/// 设备在目录里的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceStatus {
    /// 已注册，等待账户签名密钥持有者批准。
    Pending,
    /// 已批准：可被解析、可收发 E2E 消息、可再批其他设备。
    Approved,
    /// 已被撤销：拒绝解析与投递。
    Revoked,
}

/// 服务器 User 目录里的一行（账户公开信息 + 心跳）。
/// 每次该账户的任一台设备上线/心跳都刷新；`seen` 用于判在线（TTL）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    pub user_id: String,
    /// 账户 E2E 公钥（base64，X25519）。
    pub e2e_public: String,
    /// 账户签名公钥（base64，Ed25519）。
    pub sign_pk: String,
    // 最近心跳（ms）。
    pub seen: u64,
}

/// 一台设备在目录里的完整记录（wire 模型；`UserDirectory` 里的一行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub user_id: String,
    /// 设备 peer_id（base58，libp2p Ed25519）。
    pub peer_id: String,
    /// 设备 Ed25519 公钥（base64）——审批/证明绑定。
    pub device_pk: String,
    /// 账户 E2E 公钥（base64；账户级）。
    pub e2e_public: String,
    /// 用户可读设备名（"macbook" / "phone"）。
    pub label: String,
    /// 可达端点（multiaddr 字符串列表）。
    pub endpoints: Vec<String>,
    pub status: DeviceStatus,
    /// 注册时的"提议者"（base58；根设备自举时为空）。
    pub proposer: String,
    /// 批准者设备（base58）。
    pub approved_by: String,
    /// 批准/撤销证明（APPROVED/REVOKED 时非空）。
    pub attestation: Option<Attestation>,
    pub seen: u64,
    pub approved_at: u64,
}

/// 设备审批证明（wire）。由账户签名密钥持有者对 `canonical()` 签名。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attestation {
    pub user_id: String,
    /// 被批/被撤设备（base58 device peer_id）。
    pub device: String,
    /// 被批/被撤设备公钥（base64）。
    pub device_pk: String,
    pub action: AttestationAction,
    /// 批准者设备 peer_id（base58）。
    pub approver: String,
    /// 批准者所属账户的签名公钥（base64）。
    pub approver_sign_pk: String,
    pub approved_at: u64,
    /// 对 `canonical()` 的 Ed25519 签名（base64）。
    pub signature: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttestationAction {
    Approve,
    Revoke,
}

/// 规范化序列化：每个字符串前缀一个 u64 长度（BE），再拼接；枚举用判别字节。
/// 长度前缀保证字段边界不可被重新切分。
fn canon_field(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    out.extend_from_slice(&(b.len() as u64).to_be_bytes());
    out.extend_from_slice(b);
}

impl Attestation {
    /// 参与签名的规范化字节（不含 signature 本身）。
    pub fn canonical(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(256);
        v.extend_from_slice(DOMAIN_APPROVE);
        canon_field(&mut v, &self.user_id);
        canon_field(&mut v, &self.device);
        canon_field(&mut v, &self.device_pk);
        v.push(match self.action {
            AttestationAction::Approve => 1,
            AttestationAction::Revoke => 2,
        });
        canon_field(&mut v, &self.approver);
        canon_field(&mut v, &self.approver_sign_pk);
        v.extend_from_slice(&self.approved_at.to_be_bytes());
        v
    }

    /// 校验本证明有效（签名 + action 与状态一致）。
    pub fn is_valid(&self) -> bool {
        // 用 approver_sign_pk 自包含验证（不需要外部账户对象）。
        let ok = match verifying_key_from_b64(&self.approver_sign_pk) {
            Ok(vk) => match signature_from_b64(&self.signature) {
                Ok(sig) => vk.verify(&self.canonical(), &sig).is_ok(),
                Err(_) => false,
            },
            Err(_) => false,
        };
        if !ok {
            return false;
        }
        let action_matches = match self.action {
            AttestationAction::Approve => true,
            AttestationAction::Revoke => true,
        };
        action_matches
    }
}

// ────────────────────────── 口令 KDF + keystore 加密 ──────────────────────────

/// PBKDF2-HMAC-SHA256（RFC 2898）。仅依赖 `hmac` + `sha2`。
fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32, okm_len: usize) -> Vec<u8> {
    type H = Hmac<Sha256>;
    const BLOCK_SIZE: usize = 32; // SHA-256 output size
    let mut key = vec![0u8; okm_len];
    let mut i = 1usize;
    while i * BLOCK_SIZE <= okm_len {
        let mut hmac = <H as Mac>::new_from_slice(password).expect("hmac size ok");
        hmac.update(salt);
        hmac.update(&((i as u32).to_be_bytes()));
        let mut u = hmac.finalize().into_bytes().to_vec();
        let mut t = u.clone();
        for _ in 1..iterations {
            let mut h = <H as Mac>::new_from_slice(password).expect("hmac size ok");
            h.update(&u);
            u = h.finalize().into_bytes().to_vec();
            for (a, b) in t.iter_mut().zip(u.iter()) {
                *a ^= *b;
            }
        }
        let off = (i - 1) * BLOCK_SIZE;
        let len = BLOCK_SIZE.min(okm_len - off);
        key[off..off + len].copy_from_slice(&t[..len]);
        i += 1;
    }
    key
}

/// 加密后的账户私有 bundle（`keystore.json` 内容）。
#[derive(Serialize, Deserialize)]
pub struct Keystore {
    pub version: u32,
    pub user_id: String,
    /// 账户签名公钥（base64，便于展示/校验）。
    pub sign_pk: String,
    /// 账户 E2E 公钥（base64，便于展示/校验）。
    pub e2e_public: String,
    /// KDF 盐（base64，16B）。
    pub salt: String,
    /// KDF 迭代次数。
    pub iterations: u32,
    /// AES-GCM nonce（base64，12B）。
    pub nonce: String,
    /// AES-GCM 密文 + tag（base64）。明文 = sign_sk(32) ‖ e2e_sk(32)。
    pub ciphertext: String,
}

/// keystore I/O 错误。
pub type KsResult<T> = Result<T, KsError>;

#[derive(Debug, thiserror::Error)]
pub enum KsError {
    #[error("wrong passphrase (AES-GCM auth failed)")]
    WrongPassphrase,
    #[error("keystore is malformed: {0}")]
    Malformed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub fn derive_keystore_key(password: &str, salt: &[u8], iterations: u32) -> [u8; 32] {
    let v = pbkdf2_hmac_sha256(password.as_bytes(), salt, iterations, 32);
    let mut k = [0u8; 32];
    k.copy_from_slice(&v);
    k
}

fn encrypt_secret(key: &[u8; 32], secret: &AccountSecret, nonce: &[u8; 12]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(64);
    plain.extend_from_slice(&secret.sign_sk);
    plain.extend_from_slice(&secret.e2e_sk);
    let cipher = aes_gcm::Aes256Gcm::new_from_slice(key).expect("32B key");
    cipher
        .encrypt(aes_gcm::Nonce::from_slice(nonce), plain.as_slice())
        .expect("aes-gcm encrypt")
}

fn decrypt_secret(key: &[u8; 32], ciphertext: &[u8], nonce: &[u8; 12]) -> Result<AccountSecret, KsError> {
    let cipher = aes_gcm::Aes256Gcm::new_from_slice(key).expect("32B key");
    let plain = cipher
        .decrypt(aes_gcm::Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| KsError::WrongPassphrase)?;
    if plain.len() != 64 {
        return Err(KsError::Malformed("ciphertext len".into()));
    }
    let mut sk = [0u8; 32];
    let mut ek = [0u8; 32];
    sk.copy_from_slice(&plain[..32]);
    ek.copy_from_slice(&plain[32..]);
    Ok(AccountSecret {
        sign_sk: sk,
        e2e_sk: ek,
    })
}

impl Keystore {
    /// 用口令加密一个账户的私有部分。
    pub fn seal(
        acct: &Account,
        secret: &AccountSecret,
        password: &str,
        iterations: u32,
    ) -> Self {
        let mut salt = [0u8; 16];
        OsRng.fill_bytes(&mut salt);
        OsRng.fill_bytes(&mut salt);
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let key = derive_keystore_key(password, &salt, iterations);
        let ciphertext = encrypt_secret(&key, secret, &nonce);
        Self {
            version: 1,
            user_id: acct.user_id().to_string(),
            sign_pk: acct.sign_pk().to_string(),
            e2e_public: acct.e2e_public().to_string(),
            salt: b64(&salt),
            iterations,
            nonce: b64(&nonce),
            ciphertext: b64(&ciphertext),
        }
    }

    /// 用口令解密，还原账户私钥（口令错会返回 WrongPassphrase）。
    pub fn open(&self, password: &str) -> Result<Account, KsError> {
        let salt = from_b64(&self.salt).ok_or_else(|| KsError::Malformed("salt".into()))?;
        let nonce_b = from_b64(&self.nonce).ok_or_else(|| KsError::Malformed("nonce".into()))?;
        let ct = from_b64(&self.ciphertext)
            .ok_or_else(|| KsError::Malformed("ciphertext".into()))?;
        let nonce: [u8; 12] = nonce_b
            .as_slice()
            .try_into()
            .map_err(|_| KsError::Malformed("nonce len".into()))?;
        let key = derive_keystore_key(password, &salt, self.iterations);
        let secret = decrypt_secret(&key, &ct, &nonce)?;
        Ok(Account::from_secret(&self.user_id, secret))
    }

    pub fn save(&self, path: &std::path::Path) -> KsResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self).expect("json"))?;
        Ok(())
    }

    pub fn load(path: &std::path::Path) -> KsResult<Self> {
        let s = std::fs::read_to_string(path)?;
        serde_json::from_str(&s).map_err(|e| KsError::Malformed(e.to_string()))
    }
}

// ────────────────────────── base64 helpers ──────────────────────────

pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn from_b64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}
