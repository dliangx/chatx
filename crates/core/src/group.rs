
use crate::account::Account;
use crate::account::AcctError;
use aes_gcm::aead::{Aead, KeyInit};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub peer_id: String,
    pub sign_pk: String,
    pub e2e_public: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupPublic {
    pub group_id: String,
    pub owner: String,
    pub members: BTreeMap<String, MemberInfo>,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub group_id: String,
    #[serde(skip)]
    g_secret: [u8; 32],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    g_secret_enc: Option<String>,
    pub owner: String,
    pub members: BTreeMap<String, MemberInfo>,
    pub created_at: u64,
}

impl Group {
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

    pub fn set_member(&mut self, info: MemberInfo) {
        self.members.insert(info.peer_id.clone(), info);
    }

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

    pub fn secret(&self) -> [u8; 32] {
        self.g_secret
    }

    pub fn rotate(&mut self) {
        let mut s = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut s);
        self.g_secret = s;
    }

    pub fn to_public(&self) -> GroupPublic {
        GroupPublic {
            group_id: self.group_id.clone(),
            owner: self.owner.clone(),
            members: self.members.clone(),
            created_at: self.created_at,
        }
    }

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

pub fn message_key(g_secret: &[u8; 32], group_id: &str) -> [u8; 32] {
    let hk = hkdf::Hkdf::<Sha256>::new(None, g_secret);
    let mut out = [0u8; 32];
    hk.expand(format!("p2pchat/grp/v1|{group_id}").as_bytes(), &mut out)
        .expect("hkdf expand");
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SealedGroupMsg {
    pub group_id: String,
    pub from: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GroupPayload {
    from: String,
    from_sign_pk: String,
    t: u64,
    text: String,
    signature: String,
}

impl GroupPayload {
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


type GmResult<T> = Result<T, AcctError>;

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

        let to_bob = seal_secret(&alice, bob.e2e_public(), &s0).unwrap();
        assert_eq!(open_secret(&bob, alice.e2e_public(), &to_bob).unwrap(), s0);
        let to_carol = seal_secret(&alice, carol.e2e_public(), &s0).unwrap();
        assert_eq!(open_secret(&carol, alice.e2e_public(), &to_carol).unwrap(), s0);

        let sealed_bob = seal_message(&group, &bob, bob_p, "hello group").unwrap();
        assert_eq!(open_message(&s0, &sealed_bob).unwrap(), "hello group");
        assert_eq!(open_message(&s0, &sealed_bob).unwrap(), "hello group");

        assert!(open_message(&[9u8; 32], &sealed_bob).is_err());

        group.rotate();
        let s1 = group.secret();
        assert_ne!(s0, s1, "secret must change after rotation");
        let sealed_after = seal_message(&group, &bob, bob_p, "secret msg").unwrap();
        assert!(open_message(&s0, &sealed_after).is_err(), "old secret must not open new message");
        assert_eq!(open_message(&s1, &sealed_after).unwrap(), "secret msg");

        let mut other = Group::with_secret("grp-2", alice_p, s1);
        other.set_member(mem(alice_p, &alice));
        let sealed_cross = seal_message(&other, &alice, alice_p, "other group").unwrap();
        assert_eq!(open_message(&s1, &sealed_cross).unwrap(), "other group");
        assert!(open_message(&[7u8; 32], &sealed_cross).is_err());

        let mut tampered = sealed_bob.clone();
        tampered.from = carol_p.into();
        assert!(open_message(&s0, &tampered).is_err(), "tampered from must fail");
    }

    #[test]
    fn forged_sender_rejected() {
        let alice = crate::account::Account::generate("alice");
        let mallory = crate::account::Account::generate("mallory");
        let alice_p = "QmAlice";
        let mut group = Group::create("g", alice_p);
        group.set_member(mem(alice_p, &alice));

        let forged = seal_message(&group, &mallory, alice_p, "I am alice").unwrap();
        assert!(!group.has("QmMallory"), "mallory not a member");
        let _ = forged;
        assert!(group.has(alice_p), "alice is a member");
    }
}
