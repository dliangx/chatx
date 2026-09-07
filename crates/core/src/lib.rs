//! p2pchat-core — P2P 聊天核心（方案4：账户/设备分离 + 审批 + E2E）。
//!
//! 模块分工：
//! - [`account`]   账户身份（user_id + Ed25519 签名 / X25519 E2E）+ 口令加密 keystore
//! - [`identity`]  设备身份（libp2p Ed25519 → peer_id）+ profile 目录约定
//! - [`message`]   request-response JSON 消息模型
//! - [`signal`]    用户/设备目录客户端（InMemory / Http / Null）
//! - [`swarm`]     libp2p 传输/发现/聊天 + 事件循环
//! - [`store`]     会话与消息存储
//!
//! 客户端 API（`Client`）：
//! - **首台设备**（账户下尚无其它设备）：`bootstrap`/`login` → 自动自批 APPROVED
//! - **后续设备**（账户下已有其它设备）：`bootstrap`/`login` → 登记 PENDING（等批准）
//! - **批准**：`approve_device`（签名证明 + 目录置 APPROVED）
//! - **撤销**：`revoke_device`（签名证明 + 目录置 REVOKED）
//! - **业务**：`resolve_user` / `connect` / `send_text`（E2E 走账户密钥）
//! - **心跳**：`heartbeat` — 极简在线心跳，只刷新 seen/endpoints（重注册仍在登录时）

pub mod account;
pub mod identity;
pub mod message;
pub mod signal;
pub mod store;
pub mod swarm;

/// 透传 libp2p 常用类型（PeerId / Multiaddr）给宿主，避免重复依赖。
pub use libp2p;

use account::{Account, Attestation, AttestationAction, DeviceRecord, DeviceStatus, UserRecord};
use identity::DeviceIdentity;
use libp2p::{Multiaddr, PeerId};
use signal::{DirectoryClient, UserResolve};
use std::sync::Arc;
use store::Store;
use swarm::{self as sw, Cmd, Running};

/// 高层客户端：账户 + 设备 + 目录 + 存储 + swarm。
///
/// `D: DirectoryClient` 决定目录后端（测试用 `InMemoryDirectory`，桌面用 `HttpDirectory`）。
pub struct Client<D: DirectoryClient + ?Sized> {
    account: Account,
    device: DeviceIdentity,
    dir: Arc<D>,
    running: Arc<Running>,
    store: Arc<Store>,
    events: sw::EventRx,
}

impl<D: DirectoryClient + ?Sized> Client<D> {
    // ── 构造 ──

    /// 从已解密的账户 + 已有设备启动（不生成身份，不注册）。
    /// 调用方负责调用 `heartbeat` / `register_device_*` 进行目录注册。
    pub async fn from_parts(
        account: Account,
        device: DeviceIdentity,
        dir: Arc<D>,
    ) -> anyhow::Result<Self> {
        let (running, events) = sw::boot(&device).await?;
        let store = Arc::new(Store::new());
        Ok(Self {
            account,
            device,
            dir,
            running,
            store,
            events,
        })
    }

    /// **首台设备**：生成账户 + 设备，用口令把账户私钥加密落盘，设备自批 APPROVED。
    ///
    /// 若该 profile 已有 `keystore.json` 则退化为登录（幂等）。
    pub async fn bootstrap(
        profile: &str,
        user_id: impl Into<String>,
        passphrase: &str,
        dir: Arc<D>,
    ) -> anyhow::Result<(Self, Account)> {
        let uid = user_id.into();
        let keystore_path = DeviceIdentity::keystore_path(profile);

        if let Ok(ks) = account::Keystore::load(&keystore_path) {
            // 已有账户：校验 user_id 一致
            let acct = ks
                .open(passphrase)
                .map_err(|e| anyhow::anyhow!("keystore open: {e}"))?;
            if acct.user_id() != uid {
                anyhow::bail!(
                    "profile {profile} 属于 {existing}，新账户请另选 profile (uid={uid})",
                    existing = acct.user_id()
                );
            }
            let device = DeviceIdentity::load_or_create(profile)?;
            let c = Self::from_parts(acct.clone(), device, dir).await?;
            // 目录驱动判定：首台（账户下尚无其它设备）→ 自动 APPROVED；
            //              已有其它设备 → 本台为新设备 → 登记 PENDING 等批准；
            //              本设备早已登记 → 保持其原状态。
            c.refresh_directory_auto(uid.clone()).await?;
            return Ok((c, acct));
        }

        // 全新：生成账户 + 口令加密
        let acct = Account::generate(&uid);
        let ks = acct
            .to_keystore(passphrase, account::KDF_ITERATIONS)
            .ok_or_else(|| anyhow::anyhow!("account holds no secret"))?;
        ks.save(&keystore_path)?;

        let device = DeviceIdentity::generate();
        device.save(&DeviceIdentity::device_path(profile))?;

        let c = Self::from_parts(acct.clone(), device, dir).await?;
        c.refresh_directory_auto(uid.clone()).await?;
        Ok((c, acct))
    }

    /// **后续设备**：用口令解密已有账户，本机设备登记 PENDING（等批准）。
    pub async fn login(
        profile: &str,
        passphrase: &str,
        label: impl Into<String>,
        dir: Arc<D>,
    ) -> anyhow::Result<(Self, Account)> {
        let keystore_path = DeviceIdentity::keystore_path(profile);
        let ks = match account::Keystore::load(&keystore_path) {
            Ok(ks) => ks,
            Err(e) => {
                anyhow::bail!(
                    "本地找不到 {path}\n  原因：{e}\n  解决：在已批准设备上生成后，\
                     把 keystore.json 拷到此 profile 目录下（U 盘/网盘/邮件均可）",
                    path = keystore_path.display()
                )
            }
        };
        let acct = ks
            .open(passphrase)
            .map_err(|e| anyhow::anyhow!("keystore open: {e}"))?;
        let device = DeviceIdentity::load_or_create(profile)?;
        let c = Self::from_parts(acct.clone(), device, dir).await?;
        c.refresh_directory_auto(label).await?;
        Ok((c, acct))
    }

    /// `bootstrap` 的可指定 base 目录版本（测试隔离 / 多账户并存用，避免依赖 `$P2PCHAT_HOME`）。
    pub async fn bootstrap_in(
        base: &std::path::Path,
        profile: &str,
        user_id: impl Into<String>,
        passphrase: &str,
        dir: Arc<D>,
    ) -> anyhow::Result<(Self, Account)> {
        let uid = user_id.into();
        let keystore_path = DeviceIdentity::base_keystore_path(base, profile);
        let device_path = DeviceIdentity::base_device_path(base, profile);

        if let Ok(ks) = account::Keystore::load(&keystore_path) {
            let acct = ks
                .open(passphrase)
                .map_err(|e| anyhow::anyhow!("keystore open: {e}"))?;
            if acct.user_id() != uid {
                anyhow::bail!(
                    "profile {profile} 属于 {existing}，新账户请另选 (uid={uid})",
                    existing = acct.user_id()
                );
            }
            let device = DeviceIdentity::load(&device_path)
                .map_err(|e| anyhow::anyhow!("load device: {e}"))?;
            let c = Self::from_parts(acct.clone(), device, dir).await?;
            c.refresh_directory_auto(uid.clone()).await?;
            return Ok((c, acct));
        }

        let acct = Account::generate(&uid);
        let ks = acct
            .to_keystore(passphrase, account::KDF_ITERATIONS)
            .ok_or_else(|| anyhow::anyhow!("account holds no secret"))?;
        ks.save(&keystore_path)?;

        let device = DeviceIdentity::generate();
        device.save(&DeviceIdentity::device_path(profile))?;

        let c = Self::from_parts(acct.clone(), device, dir).await?;
        // 新规则：若该 user_id 名下已有其它设备 → PENDING；否则首台 → APPROVED
        c.refresh_directory_auto(uid.clone()).await?;
        Ok((c, acct))
    }

    /// `login` 的可指定 base 目录版本。
    pub async fn login_in(
        base: &std::path::Path,
        profile: &str,
        passphrase: &str,
        label: impl Into<String>,
        dir: Arc<D>,
    ) -> anyhow::Result<(Self, Account)> {
        let keystore_path = DeviceIdentity::base_keystore_path(base, profile);
        let device_path = DeviceIdentity::base_device_path(base, profile);
        let ks = match account::Keystore::load(&keystore_path) {
            Ok(ks) => ks,
            Err(e) => {
                anyhow::bail!(
                    "本地找不到 {path}\n  原因：{e}",
                    path = keystore_path.display()
                )
            }
        };
        let acct = ks
            .open(passphrase)
            .map_err(|e| anyhow::anyhow!("keystore open: {e}"))?;
        let device = match DeviceIdentity::load(&device_path) {
            Ok(d) => d,
            Err(_) => {
                let d = DeviceIdentity::generate();
                d.save(&device_path)?;
                d
            }
        };
        let c = Self::from_parts(acct.clone(), device, dir).await?;
        c.refresh_directory_auto(label).await?;
        Ok((c, acct))
    }

    /// 由已批准设备批准一台 PENDING 设备。返回签好的证明。
    pub fn approve_device(&self, target_peer: &str) -> anyhow::Result<Attestation> {
        let att = self.sign_attestation_for_resolve(target_peer, AttestationAction::Approve)?;
        // 更新目录：设备置 APPROVED + 附证明
        let mut rec = self.dir.resolve_device(&att.device)?;
        rec.status = DeviceStatus::Approved;
        rec.attestation = Some(att.clone());
        rec.approved_at = att.approved_at;
        rec.approved_by = att.approver.clone();
        self.dir.upsert_device(&rec);

        // 刷新账户目录（sign_pk 不变，仅 seen 心跳）
        self.heartbeat();
        Ok(att)
    }

    /// 撤销一台已批准设备（允许自撤销）。
    pub fn revoke_device(&self, target_peer: &str) -> anyhow::Result<Attestation> {
        let att = self.sign_attestation_for_resolve(target_peer, AttestationAction::Revoke)?;
        let mut rec = self.dir.resolve_device(&att.device)?;
        rec.status = DeviceStatus::Revoked;
        rec.attestation = Some(att.clone());
        rec.approved_at = att.approved_at;
        rec.approved_by = att.approver.clone();
        self.dir.upsert_device(&rec);
        Ok(att)
    }

    /// 用本账户签名密钥对一台设备签出审批/撤销证明。
    /// 前置：
    /// - 本设备在目录里是 APPROVED（防伪造）
    /// - 目标 user_id == 本账户 user_id
    /// - action 与目标状态相容（Approve → PENDING；Revoke → 非 PENDING）
    fn sign_attestation_for_resolve(
        &self,
        target_peer: &str,
        action: AttestationAction,
    ) -> anyhow::Result<Attestation> {
        // 1) 确认本设备在目录里是 APPROVED
        let me = self.dir.resolve_device(self.device.peer_base58())?;
        if me.user_id != self.account.user_id() {
            anyhow::bail!("本设备未登记在 user {}", self.account.user_id());
        }
        if me.status != DeviceStatus::Approved {
            anyhow::bail!("本设备当前 {:?} —— 只有 APPROVED 能签发审批证明", me.status);
        }

        // 2) 目标状态校验
        let target = self.dir.resolve_device(target_peer)?;
        if target.user_id != self.account.user_id() {
            anyhow::bail!(
                "target {} 属 user {}，本账户 {} 无权签发",
                target_peer,
                target.user_id,
                self.account.user_id()
            );
        }
        match (action, target.status) {
            (AttestationAction::Approve, DeviceStatus::Pending) => {}
            (AttestationAction::Revoke, DeviceStatus::Approved)
            | (AttestationAction::Revoke, DeviceStatus::Revoked) => {}
            (a, s) => anyhow::bail!("action={:?} 与目标状态 {:?} 不一致", a, s),
        }

        let base = Attestation {
            user_id: self.account.user_id().to_string(),
            device: target.peer_id.clone(),
            device_pk: target.device_pk.clone(),
            action,
            approver: me.peer_id.clone(),
            approver_sign_pk: self.account.sign_pk().to_string(),
            approved_at: message::now_ms(),
            signature: String::new(),
        };
        let sig = self
            .account
            .sign_attestation(&base)
            .map_err(|e| anyhow::anyhow!("sign attestation: {e}"))?;
        Ok(Attestation {
            signature: sig,
            ..base
        })
    }

    // ── 极简心跳（只刷 presence，重注册只发生在登录时） ──

    /// 极简心跳：只刷新"在线"标记（seen）与 endpoints，**不重新上报完整设备记录**。
    ///
    /// 设备登记（含状态/审批）只在登录时通过 [`refresh_directory_auto`] 完成；
    /// heartbeat 只发一条轻量 presence 请求：
    /// - trait 默认：`touch_presence(peer, endpoints)` → 仅改 `seen` + `endpoints`
    /// - 若目录里没有本设备（如服务器重启清空），才按原状态做一次完整 `upsert_device`
    pub fn heartbeat(&self) {
        // 账户层 presence（不变）
        let user_rec = UserRecord {
            user_id: self.account.user_id().to_string(),
            e2e_public: self.account.e2e_public().to_string(),
            sign_pk: self.account.sign_pk().to_string(),
            seen: message::now_ms(),
        };
        self.dir.upsert_user(&user_rec);

        // 设备：走轻量通道（只刷 seen + endpoints，不视为完整 upsert）
        let touched = self
            .dir
            .touch_presence(self.device.peer_base58(), &self.endpoints());
        // 若目录里没有本设备（如服务器重启清空）→ 兜底完整重登记一次
        if !touched {
            self.fallback_register();
        }
    }

    /// 极端场景（目录清空）兜底：按登录时确定的状态完整上报一次。
    fn fallback_register(&self) {
        let is_approved = self
            .status()
            .map(|s| s == DeviceStatus::Approved)
            .unwrap_or(false);
        let status = if is_approved {
            DeviceStatus::Approved
        } else {
            DeviceStatus::Pending
        };
        self.register_device_rec("member", status);
    }

    /// 用给定 label + status 登记本设备到目录（登录与极端场景共用）。
    fn register_device_rec(&self, label: &str, status: DeviceStatus) {
        let rec = DeviceRecord {
            user_id: self.account.user_id().to_string(),
            peer_id: self.device.peer_base58().to_string(),
            device_pk: self.device.public_base64(),
            e2e_public: self.account.e2e_public().to_string(),
            label: label.into(),
            endpoints: self.endpoints(),
            status,
            proposer: String::new(),
            approved_by: String::new(),
            attestation: None,
            seen: message::now_ms(),
            approved_at: 0,
        };
        self.dir.upsert_device(&rec);
    }

    /// 刷新本设备目录（根设备自批 APPROVED）。
    async fn refresh_directory_as_root(&self) -> anyhow::Result<()> {
        // 账户 + 设备（本设备）→ 目录
        let user_rec = UserRecord {
            user_id: self.account.user_id().to_string(),
            e2e_public: self.account.e2e_public().to_string(),
            sign_pk: self.account.sign_pk().to_string(),
            seen: message::now_ms(),
        };
        self.dir.upsert_user(&user_rec);

        let my = self.device.peer_base58();
        // 签名本设备自批证明（approver == 本设备 == target）
        let base = Attestation {
            user_id: self.account.user_id().to_string(),
            device: my.to_string(),
            device_pk: self.device.public_base64(),
            action: AttestationAction::Approve,
            approver: my.to_string(),
            approver_sign_pk: self.account.sign_pk().to_string(),
            approved_at: message::now_ms(),
            signature: String::new(),
        };
        let sig = self
            .account
            .sign_attestation(&base)
            .map_err(|e| anyhow::anyhow!("sign attestation (root): {e}"))?;
        let att = Attestation {
            signature: sig,
            ..base
        };

        let approved_at = message::now_ms();
        let device_rec = DeviceRecord {
            user_id: self.account.user_id().to_string(),
            peer_id: my.to_string(),
            device_pk: self.device.public_base64(),
            e2e_public: self.account.e2e_public().to_string(),
            label: "primary".into(),
            endpoints: self.endpoints(),
            status: DeviceStatus::Approved,
            proposer: String::new(),
            approved_by: att.approver.clone(),
            attestation: Some(att),
            seen: message::now_ms(),
            approved_at,
        };
        self.dir.upsert_device(&device_rec);
        tracing::info!(
            user_id = %self.account.user_id(),
            me = my,
            "root device registered (APPROVED)"
        );
        Ok(())
    }

    /// 刷新本设备目录（成员设备 PENDING）。
    async fn refresh_directory_as_member(&self, label: impl Into<String>) -> anyhow::Result<()> {
        let user_rec = UserRecord {
            user_id: self.account.user_id().to_string(),
            e2e_public: self.account.e2e_public().to_string(),
            sign_pk: self.account.sign_pk().to_string(),
            seen: message::now_ms(),
        };
        self.dir.upsert_user(&user_rec);

        let my = self.device.peer_base58();
        // 状态继承
        let existing = self
            .dir
            .resolve_device(my)
            .ok()
            .filter(|d| d.user_id == self.account.user_id());
        let (status, approved_by, attestation, approved_at) = match existing {
            Some(d) if d.status == DeviceStatus::Approved => (
                DeviceStatus::Approved,
                d.approved_by.clone(),
                d.attestation.clone(),
                d.approved_at,
            ),
            Some(d) => (
                d.status,
                d.approved_by.clone(),
                d.attestation.clone(),
                d.approved_at,
            ),
            None => (DeviceStatus::Pending, String::new(), None, 0),
        };

        let device_rec = DeviceRecord {
            user_id: self.account.user_id().to_string(),
            peer_id: my.to_string(),
            device_pk: self.device.public_base64(),
            e2e_public: self.account.e2e_public().to_string(),
            label: label.into(),
            endpoints: self.endpoints(),
            status,
            proposer: String::new(),
            approved_by,
            attestation,
            seen: message::now_ms(),
            approved_at,
        };
        self.dir.upsert_device(&device_rec);
        tracing::info!(
            user_id = %self.account.user_id(),
            me = my,
            status = ?device_rec.status,
            "member device registered"
        );
        Ok(())
    }

    /// 目录驱动的设备状态判定（新规则）：
    /// - 本设备在目录里**已有记录**           → 保持其状态（心跳刷新）
    /// - 本设备**尚无记录**，且账户下**无其它设备** → 视为新注册首台 → 自批 APPROVED
    /// - 本设备**尚无记录**，但账户下**已有其它设备** → 视为新设备 → 登记 PENDING 等批准
    ///
    /// 取代原先"本 profile 是否首次 bootstrap"的本地启发式，改为完全看**目录里
    /// 该 user_id 名下有几台设备**，避免跨 profile / 跨机的误判。
    async fn refresh_directory_auto(&self, label: impl Into<String>) -> anyhow::Result<()> {
        // 后端可达性硬闸门：登录/注册前必须能连上目录，否则直接失败、不跳主界面。
        self.dir.check()?;
        let my = self.device.peer_base58();
        // 1) 本设备已有记录 → 心跳（保留原状态）
        if let Ok(existing) = self.dir.resolve_device(&my) {
            if existing.user_id == self.account.user_id() {
                // 复用 member 刷新（保留状态）
                return self.refresh_directory_as_member(label).await;
            }
        }

        // 2) 本设备未登记：看账户下是否已有其它设备
        let others = self
            .dir()
            .list_devices(self.account.user_id())
            .into_iter()
            .filter(|d| d.peer_id != my)
            .count();

        if others == 0 {
            // 首台 → 自批 APPROVED
            self.refresh_directory_as_root().await
        } else {
            // 新设备 → PENDING
            self.refresh_directory_as_member(label).await
        }
    }

    // ── 访问器 ──

    pub fn account(&self) -> &Account {
        &self.account
    }

    pub fn user_id(&self) -> &str {
        self.account.user_id()
    }

    pub fn device(&self) -> &DeviceIdentity {
        &self.device
    }

    pub fn peer_id(&self) -> PeerId {
        self.device.peer_id()
    }

    pub fn peer_base58(&self) -> String {
        self.device.peer_base58().to_string()
    }

    pub fn e2e_public(&self) -> &str {
        self.account.e2e_public()
    }

    pub fn my_sign_pk(&self) -> &str {
        self.account.sign_pk()
    }

    /// 本设备在目录中的当前状态（APPROVED/PENDING/REVOKED）。
    pub fn status(&self) -> anyhow::Result<DeviceStatus> {
        let d = self.dir.resolve_device(self.device.peer_base58())?;
        Ok(d.status)
    }

    pub fn running(&self) -> Arc<Running> {
        Arc::clone(&self.running)
    }

    pub fn dir(&self) -> Arc<D> {
        Arc::clone(&self.dir)
    }

    pub fn store(&self) -> Arc<Store> {
        Arc::clone(&self.store)
    }

    /// 本机监听地址 → multiaddr 字符串列表。
    pub fn endpoints(&self) -> Vec<String> {
        self.running
            .listen_addrs
            .lock()
            .unwrap()
            .iter()
            .map(|a| a.to_string())
            .collect()
    }

    // ── 目录 ──

    pub fn resolve_user(&self, user_id: &str) -> anyhow::Result<UserResolve> {
        self.dir.resolve_user_and_device(user_id)
    }

    pub fn online_users(&self) -> Vec<UserResolve> {
        self.dir.list_users(self.device.peer_base58())
    }

    pub fn my_devices(&self) -> Vec<DeviceRecord> {
        self.dir.list_devices(self.account.user_id())
    }

    pub fn pending_devices(&self) -> Vec<DeviceRecord> {
        self.dir.list_pending(self.account.user_id())
    }

    // ── 业务 ──

    /// 发起连接：用 `UserResolve` 里的设备地址拨号。
    pub fn connect(&self, ur: &UserResolve) -> anyhow::Result<(PeerId, Multiaddr)> {
        let peer: PeerId = ur
            .device
            .peer_id
            .parse()
            .map_err(|e| anyhow::anyhow!("bad peer: {e}"))?;
        let first_addr = ur
            .device
            .endpoints
            .first()
            .ok_or_else(|| anyhow::anyhow!("device 未上报 endpoints"))?;
        let addr: Multiaddr = first_addr
            .parse()
            .map_err(|e| anyhow::anyhow!("bad endpoint: {e}"))?;
        self.running.cmd_tx.send(Cmd::Connect {
            peer,
            addr: addr.clone(),
        })?;
        Ok((peer, addr))
    }

    /// 发送文本（request，对方回 ack）。请求里携带**本账户 E2E 公钥**让对方能派生密钥。
    pub fn send_text(&self, peer: PeerId, text: &str) -> anyhow::Result<()> {
        let from = self.peer_base58();
        let e2e = self.e2e_public().to_string();
        self.running.cmd_tx.send(Cmd::SendText {
            peer,
            from,
            e2e,
            text: text.to_string(),
        })?;
        Ok(())
    }

    /// 用本账户 E2E 私钥与对方 E2E 公钥派生共享 AES-256 密钥。
    pub fn shared_key(&self, their_e2e_public: &str) -> anyhow::Result<[u8; 32]> {
        let k = self
            .account
            .derive_session_key(their_e2e_public)
            .map_err(|e| anyhow::anyhow!("derive: {e}"))?;
        Ok(k)
    }

    /// 取下一条客户端事件（阻塞）。需在异步上下文（tokio）中调用。
    pub async fn next_event(&mut self) -> Option<sw::ChatEvent> {
        self.events.recv().await
    }
}
