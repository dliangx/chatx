
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
    fn list_devices(&self, user_id: &str) -> Vec<DeviceRecord>;
    fn list_users(&self, exclude: &str) -> Vec<UserResolve>;
    fn list_pending(&self, user_id: &str) -> Vec<DeviceRecord>;

    fn upsert_group(&self, _g: &crate::group::GroupPublic) {}
    fn resolve_group(&self, group_id: &str) -> anyhow::Result<crate::group::GroupPublic>;
    fn list_groups(&self) -> Vec<crate::group::GroupPublic> {
        Vec::new()
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
pub struct InMemoryDirectory {
    users: Arc<Mutex<HashMap<String, UserRecord>>>,
    devices: Arc<Mutex<HashMap<String, DeviceRecord>>>,
    groups: Arc<Mutex<HashMap<String, crate::group::GroupPublic>>>,
}

impl InMemoryDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pair() -> (Arc<InMemoryDirectory>, Arc<InMemoryDirectory>) {
        let a = Arc::new(InMemoryDirectory::default());
        let b = Arc::new(InMemoryDirectory {
            users: Arc::clone(&a.users),
            devices: Arc::clone(&a.devices),
            groups: Arc::clone(&a.groups),
        });
        (a, b)
    }

    pub fn inject_user(&self, r: UserRecord) {
        self.users.lock().unwrap().insert(r.user_id.clone(), r);
    }

    pub fn inject_device(&self, r: DeviceRecord) {
        self.devices.lock().unwrap().insert(r.peer_id.clone(), r);
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
        let pref: Vec<&UserRecord> = t.values().filter(|r| r.user_id.starts_with(user_id)).collect();
        if pref.len() == 1 {
            return Ok(pref[0].clone());
        }
        anyhow::bail!("user {user_id} not found (or ambiguous prefix)")
    }

    fn resolve_user_and_device(&self, user_id: &str) -> anyhow::Result<UserResolve> {
        let user = self.resolve_user(user_id)?;
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
        let device = best.ok_or_else(|| anyhow::anyhow!("no online APPROVED device for user {}", user.user_id))?;
        Ok(UserResolve { user, device: device.clone() })
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
}


#[derive(Clone)]
pub struct HttpDirectory {
    base: String,
}

impl HttpDirectory {
    pub fn default_server() -> Self {
        Self::new(
            std::env::var("P2PCHAT_SIGNAL")
                .unwrap_or_else(|_| "http://192.168.1.2:8787".into()),
        )
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
            .map_err(|e| anyhow::anyhow!("signal 服务器不可达: {e}"))
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
            tracing::warn!("directory upsert_user 失败: {e}");
        }
    }

    fn upsert_device(&self, rec: &DeviceRecord) {
        let url = format!("{}/v1/devices/{}", self.base, rec.peer_id);
        let body = serde_json::to_string(rec).expect("serialize DeviceRecord");
        let res = ureq::request("PUT", &url)
            .set("content-type", "application/json")
            .send_string(&body);
        if let Err(e) = res {
            tracing::warn!("directory upsert_device 失败: {e}");
        }
    }

    fn resolve_device(&self, peer_id: &str) -> anyhow::Result<DeviceRecord> {
        let url = format!("{}/v1/devices/{peer_id}", self.base);
        let resp = ureq::request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("directory: {e}"))?;
        resp
            .into_json()
            .map_err(|e| anyhow::anyhow!("解析设备失败: {e}"))
    }

    fn resolve_user(&self, user_id: &str) -> anyhow::Result<UserRecord> {
        let url = format!("{}/v1/users/{user_id}", self.base);
        let resp = ureq::request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("directory: {e}"))?;
        resp
            .into_json()
            .map_err(|e| anyhow::anyhow!("解析用户失败: {e}"))
    }

    fn resolve_user_and_device(&self, user_id: &str) -> anyhow::Result<UserResolve> {
        let url = format!("{}/v1/users/{user_id}/resolve", self.base);
        let resp = ureq::request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("directory: {e}"))?;
        resp
            .into_json()
            .map_err(|e| anyhow::anyhow!("解析用户失败: {e}"))
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
            tracing::warn!("directory upsert_group 失败: {e}");
        }
    }

    fn resolve_group(&self, group_id: &str) -> anyhow::Result<crate::group::GroupPublic> {
        let url = format!("{}/v1/groups/{group_id}", self.base);
        let resp = ureq::request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("directory: {e}"))?;
        resp.into_json().map_err(|e| anyhow::anyhow!("解析群失败: {e}"))
    }

    fn list_groups(&self) -> Vec<crate::group::GroupPublic> {
        let url = format!("{}/v1/groups", self.base);
        match ureq::request("GET", &url).call() {
            Ok(resp) => resp.into_json::<Vec<crate::group::GroupPublic>>().unwrap_or_default(),
            Err(_) => Vec::new(),
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
                tracing::debug!("presence touch 失败: {e}");
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
