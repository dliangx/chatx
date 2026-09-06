//! 用户/设备目录客户端（方案4）。
//!
//! 服务器持两张表：
//! - **User 目录**：`user_id → UserRecord`（账户公开信息 + 心跳）
//! - **Device 目录**：`peer_id → DeviceRecord`（设备信任状态 + 审批证明）
//!
//! 客户端操作：
//! - `upsert_user`    — 每次设备上线都刷新账户目录行（公开信息无变化，只刷 `seen`）
//! - `upsert_device`  — 设备登记/刷新（PENDING / APPROVED / REVOKED）
//! - `resolve_user`   — user_id → 账户公开信息 + **首选在线 APPROVED 设备**
//! - `list_users`     — 列出所有在线 APPROVED 用户（peer_id 精确排除）
//! - `list_devices`   — 列一台账户的所有设备（含 PENDING，供 UI 审批）
//! - `list_pending`   — 列 PENDING 设备
//!
//! 服务端只存**公钥 + 端点 + 审批证明**，绝不存口令/私钥。E2E 与审批都在端侧完成，
//! 服务器不可信。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::account::{DeviceRecord, DeviceStatus, UserRecord};

/// 用户 + 首选在线设备（用于客户端"在线用户列表"与"连接"）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserResolve {
    pub user: UserRecord,
    /// 首选在线 APPROVED 设备（按 seen 降序）。
    pub device: DeviceRecord,
}

/// 在线 TTL（毫秒），30s 未刷新视为离线。
pub const ONLINE_TTL_MS: u64 = 30_000;

/// 抽象目录客户端。
pub trait DirectoryClient: Sync + Send {
    /// 刷新用户目录（新设备或老设备上线时调）。
    fn upsert_user(&self, user: &UserRecord);
    /// 登记/刷新一台设备（带状态）。
    fn upsert_device(&self, rec: &DeviceRecord);
    /// 精确或前缀解析一台设备。
    fn resolve_device(&self, peer_id: &str) -> anyhow::Result<DeviceRecord>;
    /// 精确或唯一前缀解析一个用户。
    fn resolve_user(&self, user_id: &str) -> anyhow::Result<UserRecord>;
    /// 解析一个用户 → 账户 + 首选在线 APPROVED 设备。
    fn resolve_user_and_device(&self, user_id: &str) -> anyhow::Result<UserResolve>;
    /// 列一台账户所有设备。
    fn list_devices(&self, user_id: &str) -> Vec<DeviceRecord>;
    /// 列在线 APPROVED 用户（peer_id 精确排除）。
    fn list_users(&self, exclude: &str) -> Vec<UserResolve>;
    /// 列 PENDING 设备（用于 UI 审批）。
    fn list_pending(&self, user_id: &str) -> Vec<DeviceRecord>;
    /// 轻量 presence 心跳：只刷新 `seen` + `endpoints`（保留 status/attestation）。
    /// 返回 `true` 表示设备已存在并被刷新，`false` 表示目录里没有该设备
    /// （调用方应随后做一次完整重登记）。默认实现走 `upsert_device` 等价路径。
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

// ────────────────────────── 内存实现（测试 / 联调） ──────────────────────────

/// 共享同一对表的两个实例（A/B 互相 resolve）。
#[derive(Default, Clone)]
pub struct InMemoryDirectory {
    users: Arc<Mutex<HashMap<String, UserRecord>>>,
    devices: Arc<Mutex<HashMap<String, DeviceRecord>>>,
}

impl InMemoryDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    /// 创建一对共享表的（同一 Arc 持有 → A 写的 B 能读到）。
    pub fn pair() -> (Arc<InMemoryDirectory>, Arc<InMemoryDirectory>) {
        let a = Arc::new(InMemoryDirectory::default());
        let b = Arc::new(InMemoryDirectory {
            users: Arc::clone(&a.users),
            devices: Arc::clone(&a.devices),
        });
        (a, b)
    }

    /// 直接注入用户行（测试/联调用）。
    pub fn inject_user(&self, r: UserRecord) {
        self.users.lock().unwrap().insert(r.user_id.clone(), r);
    }

    /// 直接注入设备行。
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
                // 排除自己（用 peer_id，不是 user_id）
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
}

// ────────────────────────── HTTP 实现（生产 / 跨机） ──────────────────────────

/// 连接中心化的 `p2pchat-signal` 服务器。
///
/// 阻塞式（ureq），可任意线程调。默认 `http://127.0.0.1:8787`，`P2PCHAT_SIGNAL` 覆盖。
#[derive(Clone)]
pub struct HttpDirectory {
    base: String,
}

impl HttpDirectory {
    pub fn default_server() -> Self {
        Self::new(
            std::env::var("P2PCHAT_SIGNAL")
                .unwrap_or_else(|_| "http://127.0.0.1:8787".into()),
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

    /// 探活：GET /v1/health。
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

    /// 走轻量 presence 通道。返回 `true` = 200（设备存在已刷新）；
    /// `false` = 404 / 其它错误（目录里没有该设备，调用方应重登记）。
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
        // Ok(_) = presence 成功（设备在目录里）。
        // Err   = 404（目录里没有，如服务器重启）或网络异常 → 调用方会触发重登记。
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

// ────────────────────────── 空实现（无服务器） ──────────────────────────

/// 不连任何服务器；所有目录操作返回空/Err。UI 会看到"无在线用户"。
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
}
