use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::account::{DeviceRecord, DeviceStatus, UserRecord};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserResolve {
    pub user: UserRecord,
    pub device: DeviceRecord,
}

pub const ONLINE_TTL_MS: u64 = 30_000;

pub trait DirectoryClient: Sync + Send {
    fn upsert_user(&self, user: &UserRecord);
    fn upsert_device(&self, rec: &DeviceRecord);
    fn resolve_device(&self, peer_id: &str) -> anyhow::Result<DeviceRecord>;
    fn resolve_user(&self, user_id: &str) -> anyhow::Result<UserRecord>;
    fn resolve_user_and_device(&self, user_id: &str) -> anyhow::Result<UserResolve>;
    fn resolve_user_registered(&self, user_id: &str) -> anyhow::Result<UserResolve> {
        let user = self.resolve_user(user_id)?;
        let device = self
            .list_devices(&user.user_id)
            .into_iter()
            .filter(|d| d.status == DeviceStatus::Approved)
            .max_by_key(|d| d.seen)
            .ok_or_else(|| anyhow::anyhow!("user {} has no APPROVED device", user.user_id))?;
        Ok(UserResolve { user, device })
    }
    fn list_devices(&self, user_id: &str) -> Vec<DeviceRecord>;
    fn list_users(&self, exclude: &str) -> Vec<UserResolve>;
    fn list_pending(&self, user_id: &str) -> Vec<DeviceRecord>;

    fn upsert_group(&self, _g: &crate::group::GroupPublic) {}
    fn resolve_group(&self, group_id: &str) -> anyhow::Result<crate::group::GroupPublic>;
    fn list_groups(&self) -> Vec<crate::group::GroupPublic> {
        Vec::new()
    }

    fn post_group_message(
        &self,
        _group_id: &str,
        _env: &crate::group::SealedGroupMsg,
    ) -> anyhow::Result<u64> {
        anyhow::bail!("directory does not support server-fanout group messages")
    }

    fn poll_group_messages(
        &self,
        _group_id: &str,
        _after: u64,
    ) -> anyhow::Result<Vec<crate::group::GroupMailEntry>> {
        Ok(Vec::new())
    }

    fn put_group_key_slot(
        &self,
        _group_id: &str,
        _peer_id: &str,
        _sender_e2e: &str,
        _sealed_b64: &str,
    ) -> anyhow::Result<()> {
        anyhow::bail!("directory does not support server-fanout key slots")
    }

    fn get_group_key_slot(
        &self,
        _group_id: &str,
        _peer_id: &str,
    ) -> anyhow::Result<(String, String)> {
        anyhow::bail!("directory does not support server-fanout key slots")
    }

    fn delete_group_key_slot(&self, _group_id: &str, _peer_id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    fn check(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn touch_presence(&self, peer_id: &str, endpoints: &[String]) -> bool {
        if let Ok(mut rec) = self.resolve_device(peer_id) {
            rec.seen = crate::message::now_ms();
            rec.endpoints = endpoints.to_vec();
            self.upsert_device(&rec);
            true
        } else {
            false
        }
    }
}

#[derive(Default, Clone)]
pub struct GroupKeySlots {
    peers: std::collections::BTreeMap<String, (String, String)>,
}

#[derive(Default, Clone)]
pub struct InMemoryDirectory {
    users: Arc<Mutex<HashMap<String, UserRecord>>>,
    devices: Arc<Mutex<HashMap<String, DeviceRecord>>>,
    groups: Arc<Mutex<HashMap<String, crate::group::GroupPublic>>>,
    inbox: Arc<Mutex<HashMap<String, Vec<crate::group::GroupMailEntry>>>>,
    keyslots: Arc<Mutex<HashMap<String, GroupKeySlots>>>,
}

impl InMemoryDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn shared(n: usize) -> Vec<Arc<InMemoryDirectory>> {
        let head = Arc::new(InMemoryDirectory::default());
        (0..n).map(|_| Self::clone_shared(&head)).collect()
    }

    fn clone_shared(src: &Arc<InMemoryDirectory>) -> Arc<InMemoryDirectory> {
        Arc::new(InMemoryDirectory {
            users: Arc::clone(&src.users),
            devices: Arc::clone(&src.devices),
            groups: Arc::clone(&src.groups),
            inbox: Arc::clone(&src.inbox),
            keyslots: Arc::clone(&src.keyslots),
        })
    }

    pub fn pair() -> (Arc<InMemoryDirectory>, Arc<InMemoryDirectory>) {
        let v = Self::shared(2);
        (v[0].clone(), v[1].clone())
    }

    pub fn inject_user(&self, r: UserRecord) {
        self.users.lock().unwrap().insert(r.user_id.clone(), r);
    }

    pub fn inject_device(&self, r: DeviceRecord) {
        self.devices.lock().unwrap().insert(r.peer_id.clone(), r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::group::{GroupMode, GroupPublic, MemberInfo};

    fn sample_user(id: &str) -> UserRecord {
        UserRecord {
            user_id: id.into(),
            e2e_public: format!("E2E-{id}"),
            sign_pk: format!("SIGN-{id}"),
            seen: 1,
        }
    }

    fn sample_group(id: &str) -> GroupPublic {
        let mut members = std::collections::BTreeMap::new();
        members.insert(
            "peer".into(),
            MemberInfo {
                peer_id: "peer".into(),
                sign_pk: "S".into(),
                e2e_public: "E".into(),
            },
        );
        GroupPublic {
            group_id: id.into(),
            owner: "owner".into(),
            members,
            created_at: 1,
            mode: GroupMode::Server,
        }
    }

    #[test]
    fn shared_key_slot_round_trip() {
        let handles = InMemoryDirectory::shared(2);
        assert_eq!(handles.len(), 2);
        assert_ne!(
            Arc::as_ptr(&handles[0]),
            Arc::as_ptr(&handles[1]),
            "handles must be distinct Arcs"
        );

        // Write via one handle, read via the other.
        handles[0]
            .put_group_key_slot("grp-x", "peer", "SENDER-E2E", "SEAL")
            .expect("put slot");
        let (sender, sealed) = handles[1]
            .get_group_key_slot("grp-x", "peer")
            .expect("get slot from another handle");
        assert_eq!(sender, "SENDER-E2E");
        assert_eq!(sealed, "SEAL");

        // Delete via one handle, gone from the other.
        let removed = handles[0]
            .delete_group_key_slot("grp-x", "peer")
            .expect("delete");
        assert!(removed);
        assert!(handles[1].get_group_key_slot("grp-x", "peer").is_err());

        // User upsert / resolve via different handles.
        handles[0].inject_user(sample_user("alice"));
        assert!(handles[1].resolve_user("alice").is_ok());

        // Group upsert / resolve via different handles.
        handles[0].upsert_group(&sample_group("grp-p"));
        assert!(handles[1].resolve_group("grp-p").is_ok());
    }

    #[test]
    fn shared_zero_is_empty() {
        assert!(InMemoryDirectory::shared(0).is_empty());
    }

    #[tokio::test]
    async fn pair_still_works() {
        let (a, b) = InMemoryDirectory::pair();
        a.inject_user(sample_user("bob"));
        assert!(b.resolve_user("bob").is_ok());
        a.upsert_group(&sample_group("grp-q"));
        assert!(b.resolve_group("grp-q").is_ok());
    }
}

impl DirectoryClient for InMemoryDirectory {
    fn upsert_user(&self, user: &UserRecord) {
        self.users
            .lock()
            .unwrap()
            .insert(user.user_id.clone(), user.clone());
    }

    fn upsert_device(&self, rec: &DeviceRecord) {
        self.devices
            .lock()
            .unwrap()
            .insert(rec.peer_id.clone(), rec.clone());
    }

    fn resolve_device(&self, peer_id: &str) -> anyhow::Result<DeviceRecord> {
        let t = self.devices.lock().unwrap();
        if let Some(r) = t.get(peer_id) {
            return Ok(r.clone());
        }
        for (_, r) in t.iter() {
            if r.peer_id.starts_with(peer_id) {
                return Ok(r.clone());
            }
        }
        anyhow::bail!("device {peer_id} not found in directory")
    }

    fn resolve_user(&self, user_id: &str) -> anyhow::Result<UserRecord> {
        let t = self.users.lock().unwrap();
        if let Some(r) = t.get(user_id) {
            return Ok(r.clone());
        }
        let pref: Vec<&UserRecord> = t
            .values()
            .filter(|r| r.user_id.starts_with(user_id))
            .collect();
        if pref.len() == 1 {
            return Ok(pref[0].clone());
        }
        anyhow::bail!("user {user_id} not found (or ambiguous prefix)")
    }

    fn resolve_user_and_device(&self, user_id: &str) -> anyhow::Result<UserResolve> {
        let user = self.resolve_user(user_id)?;
        eprintln!("resolve_user_and_device: user={:?}", user);
        let dt = self.devices.lock().unwrap();
        let now = crate::message::now_ms();
        let mut best: Option<&DeviceRecord> = None;
        for d in dt.values() {
            if d.user_id != user.user_id {
                continue;
            }
            if d.status != DeviceStatus::Approved {
                continue;
            }
            if now.saturating_sub(d.seen) > ONLINE_TTL_MS {
                continue;
            }
            match best {
                None => best = Some(d),
                Some(b) if d.seen > b.seen => best = Some(d),
                _ => {}
            }
        }
        let device = best.ok_or_else(|| {
            anyhow::anyhow!("no online APPROVED device for user {}", user.user_id)
        })?;
        Ok(UserResolve {
            user,
            device: device.clone(),
        })
    }

    fn list_devices(&self, user_id: &str) -> Vec<DeviceRecord> {
        self.devices
            .lock()
            .unwrap()
            .values()
            .filter(|d| d.user_id == user_id)
            .cloned()
            .collect()
    }

    fn list_users(&self, exclude: &str) -> Vec<UserResolve> {
        let ut = self.users.lock().unwrap();
        let dt = self.devices.lock().unwrap();
        let now = crate::message::now_ms();
        let mut out: Vec<UserResolve> = Vec::new();
        for (_, user) in ut.iter() {
            let mut best: Option<&DeviceRecord> = None;
            for d in dt.values() {
                if d.user_id != user.user_id {
                    continue;
                }
                if d.status != DeviceStatus::Approved {
                    continue;
                }
                if now.saturating_sub(d.seen) > ONLINE_TTL_MS {
                    continue;
                }
                if !exclude.is_empty() && d.peer_id == exclude {
                    continue;
                }
                match best {
                    None => best = Some(d),
                    Some(b) if d.seen > b.seen => best = Some(d),
                    _ => {}
                }
            }
            if let Some(d) = best {
                out.push(UserResolve {
                    user: user.clone(),
                    device: d.clone(),
                });
            }
        }
        out
    }

    fn list_pending(&self, user_id: &str) -> Vec<DeviceRecord> {
        self.devices
            .lock()
            .unwrap()
            .values()
            .filter(|d| d.user_id == user_id && d.status == DeviceStatus::Pending)
            .cloned()
            .collect()
    }

    fn upsert_group(&self, g: &crate::group::GroupPublic) {
        self.groups
            .lock()
            .unwrap()
            .insert(g.group_id.clone(), g.clone());
    }

    fn resolve_group(&self, group_id: &str) -> anyhow::Result<crate::group::GroupPublic> {
        self.groups
            .lock()
            .unwrap()
            .get(group_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("group {group_id} not found in directory"))
    }

    fn list_groups(&self) -> Vec<crate::group::GroupPublic> {
        self.groups.lock().unwrap().values().cloned().collect()
    }

    fn post_group_message(
        &self,
        group_id: &str,
        env: &crate::group::SealedGroupMsg,
    ) -> anyhow::Result<u64> {
        let wire = crate::group::msg_wire(env)?;
        let b64 = crate::account::b64(&wire);
        let mut inbox = self.inbox.lock().unwrap();
        let list = inbox.entry(group_id.to_string()).or_default();
        let seq = list.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
        list.push(crate::group::GroupMailEntry {
            group_id: group_id.to_string(),
            seq,
            from: env.from.clone(),
            t: crate::message::now_ms(),
            wire: b64,
        });
        Ok(seq)
    }

    fn poll_group_messages(
        &self,
        group_id: &str,
        after: u64,
    ) -> anyhow::Result<Vec<crate::group::GroupMailEntry>> {
        let inbox = self.inbox.lock().unwrap();
        let out: Vec<_> = inbox
            .get(group_id)
            .into_iter()
            .flatten()
            .filter(|e| e.seq > after)
            .cloned()
            .collect();
        Ok(out)
    }

    fn put_group_key_slot(
        &self,
        group_id: &str,
        peer_id: &str,
        sender_e2e: &str,
        sealed_b64: &str,
    ) -> anyhow::Result<()> {
        let mut slots = self.keyslots.lock().unwrap();
        let slot = slots.entry(group_id.to_string()).or_default();
        slot.peers.insert(
            peer_id.to_string(),
            (sender_e2e.to_string(), sealed_b64.to_string()),
        );
        Ok(())
    }

    fn get_group_key_slot(
        &self,
        group_id: &str,
        peer_id: &str,
    ) -> anyhow::Result<(String, String)> {
        let slots = self.keyslots.lock().unwrap();
        let slot = slots
            .get(group_id)
            .ok_or_else(|| anyhow::anyhow!("no key slots for group {group_id}"))?;
        slot.peers
            .get(peer_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no key slot for {peer_id} in group {group_id}"))
    }

    fn delete_group_key_slot(&self, group_id: &str, peer_id: &str) -> anyhow::Result<bool> {
        let mut slots = self.keyslots.lock().unwrap();
        let slot = match slots.get_mut(group_id) {
            Some(s) => s,
            None => return Ok(false),
        };
        let removed = slot.peers.remove(peer_id).is_some();
        if slot.peers.is_empty() {
            slots.remove(group_id);
        }
        Ok(removed)
    }
}

#[derive(Clone)]
pub struct HttpDirectory {
    base: String,
}

impl HttpDirectory {
    pub fn default_server() -> Self {
        Self::new(Self::resolve_default_url())
    }

    pub fn resolve_default_url() -> String {
        // 1. CHATX_SIGNAL env var (highest priority)
        if let Ok(v) = std::env::var("CHATX_SIGNAL") {
            if !v.trim().is_empty() {
                return v;
            }
        }
        // 2. ~/.config/chatx/<profile>/config.toml → signal_url
        //    If the user config does not exist, seed it from the crate
        //    default (crates/core/config.toml) so it is editable in one place.
        let profile = crate::identity::DeviceIdentity::default_profile();
        let dir = crate::identity::DeviceIdentity::profile_dir(&profile);
        let path = dir.join("config.toml");
        if !path.exists() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&path, include_str!("../config.toml"));
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(cfg) = toml::from_str::<toml::Value>(&text) {
                if let Some(url) = cfg.get("signal_url").and_then(|v| v.as_str()) {
                    if !url.trim().is_empty() {
                        return url.to_string();
                    }
                }
            }
        }
        return "https://signal.chatx.dev".into();
    }

    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub fn ping(&self) -> anyhow::Result<()> {
        let url = format!("{}/v1/health", self.base);
        ureq::request("GET", &url)
            .call()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("signal server unreachable: {e}"))
    }
}

impl Default for HttpDirectory {
    fn default() -> Self {
        Self::default_server()
    }
}

impl DirectoryClient for HttpDirectory {
    fn check(&self) -> anyhow::Result<()> {
        self.ping()
    }

    fn upsert_user(&self, user: &UserRecord) {
        let url = format!("{}/v1/users/{}", self.base, user.user_id);
        let body = serde_json::to_string(user).expect("serialize UserRecord");
        let res = ureq::request("PUT", &url)
            .set("content-type", "application/json")
            .send_string(&body);
        if let Err(e) = res {
            tracing::warn!("directory upsert_user failed: {e}");
        }
    }

    fn upsert_device(&self, rec: &DeviceRecord) {
        let url = format!("{}/v1/devices/{}", self.base, rec.peer_id);
        let body = serde_json::to_string(rec).expect("serialize DeviceRecord");
        let res = ureq::request("PUT", &url)
            .set("content-type", "application/json")
            .send_string(&body);
        if let Err(e) = res {
            tracing::warn!("directory upsert_device failed: {e}");
        }
    }

    fn resolve_device(&self, peer_id: &str) -> anyhow::Result<DeviceRecord> {
        let url = format!("{}/v1/devices/{peer_id}", self.base);
        let resp = ureq::request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("directory: {e}"))?;
        resp.into_json()
            .map_err(|e| anyhow::anyhow!("resolve device failed: {e}"))
    }

    fn resolve_user(&self, user_id: &str) -> anyhow::Result<UserRecord> {
        let url = format!("{}/v1/users/{user_id}", self.base);
        let resp = ureq::request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("directory: {e}"))?;
        resp.into_json()
            .map_err(|e| anyhow::anyhow!("resolve user failed: {e}"))
    }

    fn resolve_user_and_device(&self, user_id: &str) -> anyhow::Result<UserResolve> {
        let url = format!("{}/v1/users/{user_id}/resolve", self.base);
        let resp = ureq::request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("directory: {e}"))?;
        resp.into_json()
            .map_err(|e| anyhow::anyhow!("resolve user failed: {e}"))
    }

    fn list_devices(&self, user_id: &str) -> Vec<DeviceRecord> {
        let url = format!("{}/v1/users/{user_id}/devices", self.base);
        match ureq::request("GET", &url).call() {
            Ok(resp) => resp.into_json::<Vec<DeviceRecord>>().unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn list_users(&self, exclude: &str) -> Vec<UserResolve> {
        let mut url = format!("{}/v1/users", self.base);
        if !exclude.is_empty() {
            url.push_str(&format!("?exclude={}", exclude));
        }
        match ureq::request("GET", &url).call() {
            Ok(resp) => resp.into_json::<Vec<UserResolve>>().unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn list_pending(&self, user_id: &str) -> Vec<DeviceRecord> {
        let url = format!("{}/v1/users/{user_id}/devices?status=pending", self.base);
        match ureq::request("GET", &url).call() {
            Ok(resp) => resp.into_json::<Vec<DeviceRecord>>().unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn upsert_group(&self, g: &crate::group::GroupPublic) {
        let url = format!("{}/v1/groups/{}", self.base, g.group_id);
        let body = serde_json::to_string(g).expect("serialize GroupPublic");
        let res = ureq::request("PUT", &url)
            .set("content-type", "application/json")
            .send_string(&body);
        if let Err(e) = res {
            tracing::warn!("directory upsert_group failed: {e}");
        }
    }

    fn resolve_group(&self, group_id: &str) -> anyhow::Result<crate::group::GroupPublic> {
        let url = format!("{}/v1/groups/{group_id}", self.base);
        let resp = ureq::request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("directory: {e}"))?;
        resp.into_json()
            .map_err(|e| anyhow::anyhow!("resolve group failed: {e}"))
    }

    fn list_groups(&self) -> Vec<crate::group::GroupPublic> {
        let url = format!("{}/v1/groups", self.base);
        match ureq::request("GET", &url).call() {
            Ok(resp) => resp
                .into_json::<Vec<crate::group::GroupPublic>>()
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn post_group_message(
        &self,
        group_id: &str,
        env: &crate::group::SealedGroupMsg,
    ) -> anyhow::Result<u64> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            from: &'a str,
            wire: String,
        }
        let wire = crate::group::msg_wire(env)?;
        let body = serde_json::to_string(&Body {
            from: &env.from,
            wire: crate::account::b64(&wire),
        })
        .expect("serialize group msg body");
        let url = format!("{}/v1/groups/{group_id}/messages", self.base);
        let resp = ureq::request("POST", &url)
            .set("content-type", "application/json")
            .send_string(&body)
            .map_err(|e| anyhow::anyhow!("post group message: {e}"))?;
        #[derive(serde::Deserialize)]
        struct Out {
            seq: u64,
        }
        resp.into_json::<Out>()
            .map(|o| o.seq)
            .map_err(|e| anyhow::anyhow!("bad post group message response: {e}"))
    }

    fn poll_group_messages(
        &self,
        group_id: &str,
        after: u64,
    ) -> anyhow::Result<Vec<crate::group::GroupMailEntry>> {
        let url = format!("{}/v1/groups/{group_id}/messages?after={after}", self.base);
        match ureq::request("GET", &url).call() {
            Ok(resp) => resp
                .into_json::<Vec<crate::group::GroupMailEntry>>()
                .map_err(|e| anyhow::anyhow!("poll group messages: {e}")),
            Err(_) => Ok(Vec::new()),
        }
    }

    fn put_group_key_slot(
        &self,
        group_id: &str,
        peer_id: &str,
        sender_e2e: &str,
        sealed_b64: &str,
    ) -> anyhow::Result<()> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            sender_e2e: &'a str,
            sealed: &'a str,
        }
        let body = serde_json::to_string(&Body {
            sender_e2e,
            sealed: sealed_b64,
        })
        .expect("serialize key slot body");
        let url = format!("{}/v1/groups/{group_id}/keys/{peer_id}", self.base);
        let res = ureq::request("PUT", &url)
            .set("content-type", "application/json")
            .send_string(&body);
        res.map(|_| ())
            .map_err(|e| anyhow::anyhow!("put group key slot: {e}"))
    }

    fn get_group_key_slot(
        &self,
        group_id: &str,
        peer_id: &str,
    ) -> anyhow::Result<(String, String)> {
        let url = format!("{}/v1/groups/{group_id}/keys/{peer_id}", self.base);
        let resp = ureq::request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("get group key slot: {e}"))?;
        let v: (String, String) = resp
            .into_json()
            .map_err(|e| anyhow::anyhow!("bad key slot response: {e}"))?;
        Ok(v)
    }

    fn delete_group_key_slot(&self, group_id: &str, peer_id: &str) -> anyhow::Result<bool> {
        let url = format!("{}/v1/groups/{group_id}/keys/{peer_id}", self.base);
        match ureq::request("DELETE", &url).call() {
            Ok(_) => Ok(true),
            Err(ureq::Error::Status(404, _)) => Ok(false),
            Err(e) => Err(anyhow::anyhow!("delete group key slot: {e}")),
        }
    }

    fn touch_presence(&self, peer_id: &str, endpoints: &[String]) -> bool {
        #[derive(serde::Serialize)]
        struct Presence {
            seen: u64,
            endpoints: Vec<String>,
        }
        let url = format!("{}/v1/devices/{peer_id}/presence", self.base);
        let body = serde_json::to_string(&Presence {
            seen: crate::message::now_ms(),
            endpoints: endpoints.to_vec(),
        })
        .unwrap_or_else(|_| "{}".into());
        match ureq::request("PUT", &url)
            .set("content-type", "application/json")
            .send_string(&body)
        {
            Ok(_) => true,
            Err(e) => {
                tracing::debug!("presence touch failed: {e}");
                false
            }
        }
    }
}

#[derive(Default, Clone)]
pub struct NullDirectory;

impl DirectoryClient for NullDirectory {
    fn upsert_user(&self, _user: &UserRecord) {}
    fn upsert_device(&self, _rec: &DeviceRecord) {}
    fn resolve_device(&self, peer_id: &str) -> anyhow::Result<DeviceRecord> {
        anyhow::bail!("null directory: device {peer_id} not found")
    }
    fn resolve_user(&self, user_id: &str) -> anyhow::Result<UserRecord> {
        anyhow::bail!("null directory: user {user_id} not found")
    }
    fn resolve_user_and_device(&self, user_id: &str) -> anyhow::Result<UserResolve> {
        anyhow::bail!("null directory: user {user_id} not found")
    }
    fn list_devices(&self, _user_id: &str) -> Vec<DeviceRecord> {
        Vec::new()
    }
    fn list_users(&self, _exclude: &str) -> Vec<UserResolve> {
        Vec::new()
    }
    fn list_pending(&self, _user_id: &str) -> Vec<DeviceRecord> {
        Vec::new()
    }

    fn resolve_group(&self, group_id: &str) -> anyhow::Result<crate::group::GroupPublic> {
        anyhow::bail!("null directory: group {group_id} not found")
    }
}
