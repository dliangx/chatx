
use aes_gcm::aead::{Aead, KeyInit};
use ed25519_dalek::{Signature, SigningKey, VerifyingKey, Signer, Verifier};
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{PublicKey as X25519Pub, StaticSecret};

pub const KDF_ITERATIONS: u32 = 200_000;
const DOMAIN_APPROVE: &[u8] = b"p2pchat/attest/v1";


#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountPublic {
    pub user_id: String,

    pub sign_pk: String,

    pub e2e_public: String,
}

#[derive(Clone)]
pub struct AccountSecret {

    pub sign_sk: [u8; 32],

    pub e2e_sk: [u8; 32],
}

#[derive(Clone)]
pub struct Account {
    pub public: AccountPublic,
    secret: Option<AccountSecret>,
}

impl Account {

    pub fn from_public(p: AccountPublic) -> Self {
        Self {
            public: p,
            secret: None,
        }
    }


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


    pub fn to_keystore(&self, password: &str, iterations: u32) -> Option<Keystore> {
        let secret = self.secret.as_ref()?;
        Some(Keystore::seal(self, secret, password, iterations))
    }

    fn secret(&self) -> &AccountSecret {
        self.secret
            .as_ref()
            .expect("account does not hold its secret keys")
    }


    pub fn sign_attestation(&self, a: &Attestation) -> Result<String, AcctError> {
        let sk = SigningKey::from_bytes(&self.secret().sign_sk);
        let sig = sk.sign(&a.canonical());
        Ok(b64(&sig.to_bytes()))
    }


    pub fn verify_attestation(a: &Attestation) -> Result<bool, AcctError> {
        let vk = verifying_key_from_b64(&a.approver_sign_pk)?;
        let sig = signature_from_b64(&a.signature)?;
        Ok(vk.verify(&a.canonical(), &sig).is_ok())
    }


    pub fn sign_raw(&self, message: &[u8]) -> Result<String, AcctError> {
        let sk = SigningKey::from_bytes(&self.secret().sign_sk);
        let sig = sk.sign(message);
        Ok(b64(&sig.to_bytes()))
    }


    pub fn verify_raw(
        sign_pk_b64: &str,
        message: &[u8],
        signature_b64: &str,
    ) -> Result<bool, AcctError> {
        let vk = verifying_key_from_b64(sign_pk_b64)?;
        let sig = signature_from_b64(signature_b64)?;
        Ok(vk.verify(message, &sig).is_ok())
    }





    pub fn data_protect_key(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        hkdf::Hkdf::<Sha256>::new(Some(b"p2pchat/data-protect/v1"), &self.secret().sign_sk)
            .expand(b"32b-aes", &mut out)
            .expect("hkdf 32B");
        out
    }


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


#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceStatus {

    Pending,

    Approved,

    Revoked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    pub user_id: String,

    pub e2e_public: String,

    pub sign_pk: String,

    pub seen: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub user_id: String,

    pub peer_id: String,

    pub device_pk: String,

    pub e2e_public: String,

    pub label: String,

    pub endpoints: Vec<String>,
    pub status: DeviceStatus,

    pub proposer: String,

    pub approved_by: String,

    pub attestation: Option<Attestation>,
    pub seen: u64,
    pub approved_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attestation {
    pub user_id: String,

    pub device: String,

    pub device_pk: String,
    pub action: AttestationAction,

    pub approver: String,

    pub approver_sign_pk: String,
    pub approved_at: u64,

    pub signature: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttestationAction {
    Approve,
    Revoke,
}

fn canon_field(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    out.extend_from_slice(&(b.len() as u64).to_be_bytes());
    out.extend_from_slice(b);
}

impl Attestation {

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


    pub fn is_valid(&self) -> bool {

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


fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32, okm_len: usize) -> Vec<u8> {
    type H = Hmac<Sha256>;
    const BLOCK_SIZE: usize = 32;
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

#[derive(Serialize, Deserialize)]
pub struct Keystore {
    pub version: u32,
    pub user_id: String,

    pub sign_pk: String,

    pub e2e_public: String,

    pub salt: String,

    pub iterations: u32,

    pub nonce: String,

    pub ciphertext: String,
}

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


pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn from_b64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}
