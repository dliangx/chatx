
pub mod account;
pub mod group;
pub mod identity;
pub mod message;
pub mod signal;
pub mod store;
pub mod swarm;

pub use libp2p;

use account::{Account, Attestation, AttestationAction, DeviceRecord, DeviceStatus, UserRecord};
use group::{Group, MemberInfo};
use identity::DeviceIdentity;
use libp2p::{Multiaddr, PeerId};
use signal::{DirectoryClient, UserResolve};
use std::collections::BTreeMap;
use std::sync::Arc;
use store::Store;
use swarm::{self as sw, Cmd, Running};
use parking_lot::RwLock;

pub struct Client<D: DirectoryClient + ?Sized> {
    account: Account,
    device: DeviceIdentity,
    dir: Arc<D>,
    running: Arc<Running>,
    store: Arc<Store>,
    events: sw::EventRx,
    groups: RwLock<BTreeMap<String, Group>>,
    groups_dir: std::path::PathBuf,
}

#[derive(Debug)]
pub enum InboundGroup {
    Key { group_id: String },
    Message {
        group_id: String,
        from: String,
        text: String,
    },
}

impl<D: DirectoryClient + ?Sized> Client<D> {

    pub async fn from_parts(
        account: Account,
        device: DeviceIdentity,
        dir: Arc<D>,
        base: &std::path::Path,
        profile: &str,
    ) -> anyhow::Result<Self> {
        let (running, events) = sw::boot(&device).await?;
        let db_path = base.join(profile).join("messages.db");
        let db = sqlite::open(&db_path).await?;
        let store = Arc::new(Store::with_sql(db));

        let groups_dir = DeviceIdentity::base_groups_dir(base, profile);
        let mut groups = BTreeMap::new();
        if groups_dir.is_dir() {
            for entry in std::fs::read_dir(&groups_dir)? {
                let path = entry?.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    if let Ok(g) = Group::load(&account, &path) {
                        groups.insert(g.group_id.clone(), g);
                    }
                }
            }
        }

        Ok(Self {
            account,
            device,
            dir,
            running,
            store,
            events,
            groups: RwLock::new(groups),
            groups_dir,
        })
    }

    pub async fn bootstrap(
        profile: &str,
        user_id: impl Into<String>,
        passphrase: &str,
        dir: Arc<D>,
    ) -> anyhow::Result<(Self, Account)> {
        let uid = user_id.into();
        let keystore_path = DeviceIdentity::keystore_path(profile);

        if let Ok(ks) = account::Keystore::load(&keystore_path) {
            let acct = ks
                .open(passphrase)
                .map_err(|e| anyhow::anyhow!("keystore open: {e}"))?;
            if acct.user_id() != uid {
                anyhow::bail!(
                    "profile {profile} belongs to {existing}, pick a different profile for a new account (uid={uid})",
                    existing = acct.user_id()
                );
            }
            let device = DeviceIdentity::load_or_create(profile)?;
            let c = Self::from_parts(acct.clone(), device, dir, &DeviceIdentity::home_dir(), profile).await?;
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

        let c = Self::from_parts(acct.clone(), device, dir, &DeviceIdentity::home_dir(), profile).await?;
        c.refresh_directory_auto(uid.clone()).await?;
        Ok((c, acct))
    }

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
                    "not found locally: {path}\n  cause: {e}\n  fix: generate it on an approved\
                     device, then copy keystore.json into this profile dir (USB drive/cloud drive/email all work)",
                    path = keystore_path.display()
                )
            }
        };
        let acct = ks
            .open(passphrase)
            .map_err(|e| anyhow::anyhow!("keystore open: {e}"))?;
        let device = DeviceIdentity::load_or_create(profile)?;
        let c = Self::from_parts(acct.clone(), device, dir, &DeviceIdentity::home_dir(), profile).await?;
        c.refresh_directory_auto(label).await?;
        Ok((c, acct))
    }

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
                    "profile {profile} belongs to {existing}, pick a different one (uid={uid})",
                    existing = acct.user_id()
                );
            }
            let device = DeviceIdentity::load(&device_path)
                .map_err(|e| anyhow::anyhow!("load device: {e}"))?;
            let c = Self::from_parts(acct.clone(), device, dir, base, profile).await?;
            c.refresh_directory_auto(uid.clone()).await?;
            return Ok((c, acct));
        }

        let acct = Account::generate(&uid);
        let ks = acct
            .to_keystore(passphrase, account::KDF_ITERATIONS)
            .ok_or_else(|| anyhow::anyhow!("account holds no secret"))?;
        ks.save(&keystore_path)?;

        let device = DeviceIdentity::generate();
        device.save(&device_path)?;

        let c = Self::from_parts(acct.clone(), device, dir, base, profile).await?;
        c.refresh_directory_auto(uid.clone()).await?;
        Ok((c, acct))
    }

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
                    "not found locally: {path}\n  cause: {e}",
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
        let c = Self::from_parts(acct.clone(), device, dir, base, profile).await?;
        c.refresh_directory_auto(label).await?;
        Ok((c, acct))
    }

    pub fn approve_device(&self, target_peer: &str) -> anyhow::Result<Attestation> {
        let att = self.sign_attestation_for_resolve(target_peer, AttestationAction::Approve)?;
        let mut rec = self.dir.resolve_device(&att.device)?;
        rec.status = DeviceStatus::Approved;
        rec.attestation = Some(att.clone());
        rec.approved_at = att.approved_at;
        rec.approved_by = att.approver.clone();
        self.dir.upsert_device(&rec);

        self.heartbeat();
        Ok(att)
    }

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

    fn sign_attestation_for_resolve(
        &self,
        target_peer: &str,
        action: AttestationAction,
    ) -> anyhow::Result<Attestation> {
        let me = self.dir.resolve_device(self.device.peer_base58())?;
        if me.user_id != self.account.user_id() {
            anyhow::bail!("this device is not registered under user {}", self.account.user_id());
        }
        if me.status != DeviceStatus::Approved {
            anyhow::bail!("this device is {:?} — only APPROVED can issue attestations", me.status);
        }

        let target = self.dir.resolve_device(target_peer)?;
        if target.user_id != self.account.user_id() {
            anyhow::bail!(
                "target {} belongs to user {}, account {} has no permission to attest",
                target_peer,
                target.user_id,
                self.account.user_id()
            );
        }
        match (action, target.status) {
            (AttestationAction::Approve, DeviceStatus::Pending) => {}
            (AttestationAction::Revoke, DeviceStatus::Approved)
            | (AttestationAction::Revoke, DeviceStatus::Revoked) => {}
            (a, s) => anyhow::bail!("action={:?} inconsistent with target status {:?}", a, s),
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


    pub fn heartbeat(&self) {
        let user_rec = UserRecord {
            user_id: self.account.user_id().to_string(),
            e2e_public: self.account.e2e_public().to_string(),
            sign_pk: self.account.sign_pk().to_string(),
            seen: message::now_ms(),
        };
        self.dir.upsert_user(&user_rec);

        let touched = self
            .dir
            .touch_presence(self.device.peer_base58(), &self.endpoints());
        if !touched {
            self.fallback_register();
        }
    }

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

    async fn refresh_directory_as_root(&self) -> anyhow::Result<()> {
        let user_rec = UserRecord {
            user_id: self.account.user_id().to_string(),
            e2e_public: self.account.e2e_public().to_string(),
            sign_pk: self.account.sign_pk().to_string(),
            seen: message::now_ms(),
        };
        self.dir.upsert_user(&user_rec);

        let my = self.device.peer_base58();
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

    async fn refresh_directory_as_member(&self, label: impl Into<String>) -> anyhow::Result<()> {
        let user_rec = UserRecord {
            user_id: self.account.user_id().to_string(),
            e2e_public: self.account.e2e_public().to_string(),
            sign_pk: self.account.sign_pk().to_string(),
            seen: message::now_ms(),
        };
        self.dir.upsert_user(&user_rec);

        let my = self.device.peer_base58();
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

    async fn refresh_directory_auto(&self, label: impl Into<String>) -> anyhow::Result<()> {
        self.dir.check()?;
        let my = self.device.peer_base58();
        if let Ok(existing) = self.dir.resolve_device(&my) {
            if existing.user_id == self.account.user_id() {
                return self.refresh_directory_as_member(label).await;
            }
        }

        let others = self
            .dir()
            .list_devices(self.account.user_id())
            .into_iter()
            .filter(|d| d.peer_id != my)
            .count();

        if others == 0 {
            self.refresh_directory_as_root().await
        } else {
            self.refresh_directory_as_member(label).await
        }
    }


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

    pub fn endpoints(&self) -> Vec<String> {
        self.running
            .listen_addrs
            .lock()
            .unwrap()
            .iter()
            .map(|a| a.to_string())
            .collect()
    }


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
            .ok_or_else(|| anyhow::anyhow!("device reported no endpoints"))?;
        let addr: Multiaddr = first_addr
            .parse()
            .map_err(|e| anyhow::anyhow!("bad endpoint: {e}"))?;
        self.running.cmd_tx.send(Cmd::Connect {
            peer,
            addr: addr.clone(),
        })?;
        Ok((peer, addr))
    }

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

    pub async fn send_dm(&self, peer_base58: &str, text: &str) -> anyhow::Result<String> {
        let rec = self
            .dir
            .resolve_device(peer_base58)
            .map_err(|e| anyhow::anyhow!("device {peer_base58} not found in directory: {e}"))?;
        let endpoint = rec
            .endpoints
            .first()
            .ok_or_else(|| anyhow::anyhow!("device {peer_base58} is offline (no endpoints)"))?
            .clone();
        let peer: PeerId = peer_base58
            .parse()
            .map_err(|e| anyhow::anyhow!("bad peer: {e}"))?;
        let addr: Multiaddr = endpoint
            .parse()
            .map_err(|e| anyhow::anyhow!("bad endpoint: {e}"))?;
        self.running.cmd_tx.send(Cmd::Connect { peer: peer.clone(), addr })?;
        self.send_text(peer, text)?;
        let chat = store::dm_chat_id(&self.peer_base58(), peer_base58);
        self.store.push(&chat, &self.peer_base58(), text, false).await;
        Ok(chat)
    }

    pub fn all_messages(&self) -> Vec<store::StoredMsg> {
        self.store.all()
    }

    pub async fn history(&self, chat_id: &str, limit: usize, offset: usize) -> anyhow::Result<Vec<store::StoredMsg>> {
        self.store.load(chat_id, limit, offset).await
    }

    pub fn shared_key(&self, their_e2e_public: &str) -> anyhow::Result<[u8; 32]> {
        let k = self
            .account
            .derive_session_key(their_e2e_public)
            .map_err(|e| anyhow::anyhow!("derive: {e}"))?;
        Ok(k)
    }

    pub async fn next_event(&mut self) -> Option<sw::ChatEvent> {
        self.events.recv().await
    }

    pub async fn process_inbound(&self, evt: &sw::ChatEvent) -> anyhow::Result<Option<InboundGroup>> {
        match evt {
            sw::ChatEvent::Text { req, .. } => self.apply_inbound(req).await,
            _ => Ok(None),
        }
    }

    pub async fn apply_inbound(&self, req: &message::ChatRequest) -> anyhow::Result<Option<InboundGroup>> {
        match req.kind {
            message::MsgKind::Dm => Ok(None),
            message::MsgKind::GroupKey => {
                let group_id = req
                    .group_id
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("GroupKey missing group_id"))?;
                let bundle = req
                    .sealed
                    .as_deref()
                    .and_then(crate::account::from_b64)
                    .ok_or_else(|| anyhow::anyhow!("bad group key bundle"))?;
                let secret = group::open_secret(self.account(), &req.e2e, &bundle)
                    .map_err(|e| anyhow::anyhow!("open group key: {e}"))?;
                let pubinfo = self
                    .dir
                    .resolve_group(&group_id)
                    .map_err(|e| anyhow::anyhow!("resolve group dir: {e}"))?;
                let mut g = group::Group::with_secret(pubinfo.group_id, pubinfo.owner, secret);
                for (pid, mi) in pubinfo.members {
                    g.set_member(mi);
                    let _ = pid;
                }
                let me = self.peer_base58();
                if !g.has(&me) {
                    g.set_member(group::MemberInfo {
                        peer_id: me.clone(),
                        sign_pk: self.account.sign_pk().to_string(),
                        e2e_public: self.account.e2e_public().to_string(),
                    });
                }
                self.save_group(&g)?;
                let _ = self
                    .running
                    .cmd_tx
                    .send(Cmd::GroupSubscribe { group_id: group_id.clone() });
                Ok(Some(InboundGroup::Key { group_id }))
            }
            message::MsgKind::GroupMsg => {
                let group_id = req
                    .group_id
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("GroupMsg missing group_id"))?;
                let wire = req
                    .sealed
                    .as_deref()
                    .and_then(crate::account::from_b64)
                    .ok_or_else(|| anyhow::anyhow!("bad group msg blob"))?;
                let env: group::SealedGroupMsg =
                    serde_json::from_slice(&wire).map_err(|e| anyhow::anyhow!("group msg: {e}"))?;
                let from = env.from.clone();
                let text = self.open_group_message(&group_id, &env)?;
                self.store_group_inbound(&env).await?;
                Ok(Some(InboundGroup::Message { group_id, from, text }))
            }
        }
    }


    pub fn create_group(
        &self,
        group_id: impl Into<String>,
        members: Vec<MemberInfo>,
    ) -> anyhow::Result<group::Group> {
        let group_id = group_id.into();
        let me_peer = self.peer_base58();
        let owner = me_peer.clone();
        let mut g = group::Group::create(&group_id, owner);
        g.set_member(MemberInfo {
            peer_id: me_peer.clone(),
            sign_pk: self.account.sign_pk().to_string(),
            e2e_public: self.account.e2e_public().to_string(),
        });
        for m in members {
            g.set_member(m);
        }
        self.save_group(&g)?;
        if let Err(e) = self
            .running
            .cmd_tx
            .send(Cmd::GroupSubscribe { group_id: group_id.clone() })
        {
            tracing::warn!("GroupSubscribe cmd failed: {e}");
        }
        Ok(g)
    }

    pub fn add_member(&self, group_id: &str, info: MemberInfo) -> anyhow::Result<()> {
        let mut g = self.groups.write().get_mut(group_id).cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown group {group_id}"))?;
        g.set_member(info.clone());
        self.save_group(&g)?;
        self
            .running
            .cmd_tx
            .send(Cmd::GroupSubscribe { group_id: group_id.into() })
            .map_err(|e| anyhow::anyhow!("GroupSubscribe cmd failed: {e}"))?;
        // The new member must receive the group secret: redistribute the key
        // (sealed per-peer) so the added member can decrypt future group messages.
        if !info.peer_id.is_empty() && info.peer_id != self.peer_base58() {
            self.publish_group_key(group_id)?;
        }
        Ok(())
    }

    pub fn rotate_group(&self, group_id: &str) -> anyhow::Result<group::Group> {
        let mut g = self.groups.write().get_mut(group_id).cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown group {group_id}"))?;
        g.rotate();
        self.save_group(&g)?;
        self.publish_group_key(group_id)?;
        Ok(g)
    }

    pub fn leave_group(&self, group_id: &str) -> anyhow::Result<()> {
        if !self.groups.read().contains_key(group_id) {
            anyhow::bail!("unknown group {group_id}");
        }
        let _ = self
            .running
            .cmd_tx
            .send(Cmd::GroupUnsubscribe { group_id: group_id.into() });
        self.groups.write().remove(group_id);
        let _ = std::fs::remove_file(self.groups_dir.join(format!("{group_id}.json")));
        Ok(())
    }

    /// Subscribes the local swarm to the group's gossipsub topic.
    ///
    /// This is the first step after learning a group_id out-of-band: it opens the
    /// pub/sub stream for that group so the owner's key publication can be
    /// received. The caller must subsequently call `drain_inbound` until
    /// `InboundGroup::Key` for this group arrives, at which point the local
    /// `Group` record exists and `send_group_message` will work.
    pub fn join_group(&self, group_id: &str) -> anyhow::Result<()> {
        let _ = self
            .running
            .cmd_tx
            .send(Cmd::GroupSubscribe { group_id: group_id.into() });
        Ok(())
    }

    pub fn get_group(&self, group_id: &str) -> Option<group::Group> {
        self.groups.read().get(group_id).cloned()
    }

    pub fn list_groups(&self) -> Vec<group::Group> {
        self.groups.read().values().cloned().collect()
    }

    pub fn other_peer_of(&self, chat_id: &str) -> Option<String> {
        chat_id
            .split('|')
            .filter(|p| !p.is_empty())
            .find(|p| *p != self.peer_base58())
            .map(|s| s.to_string())
    }

    pub fn seal_group_message(
        &self,
        group_id: &str,
        text: &str,
    ) -> anyhow::Result<group::SealedGroupMsg> {
        let g = self.get_group(group_id).ok_or_else(|| anyhow::anyhow!("unknown group {group_id}"))?;
        let me = self.peer_base58();
        group::seal_message(&g, &self.account, &me, text)
            .map_err(|e| anyhow::anyhow!("seal group msg: {e}"))
    }

    pub fn open_group_message(
        &self,
        group_id: &str,
        env: &group::SealedGroupMsg,
    ) -> anyhow::Result<String> {
        if env.group_id != group_id {
            anyhow::bail!("group id mismatch");
        }
        let g = self.get_group(group_id).ok_or_else(|| anyhow::anyhow!("unknown group {group_id}"))?;
        group::open_message(&g.secret(), env).map_err(|e| anyhow::anyhow!("open group msg: {e}"))
    }

    pub async fn store_group_outbound(&self, env: &group::SealedGroupMsg) -> anyhow::Result<()> {
        let wire = serde_json::to_string(env)?;
        let b64 = crate::account::b64(wire.as_bytes());
        self.store.push(&env.group_id, &env.from, &b64, true).await;
        Ok(())
    }

    pub async fn store_group_inbound(&self, env: &group::SealedGroupMsg) -> anyhow::Result<()> {
        let wire = serde_json::to_string(env)?;
        let b64 = crate::account::b64(wire.as_bytes());
        self.store.push(&env.group_id, &env.from, &b64, true).await;
        Ok(())
    }

    pub fn announce_group(&self, group_id: &str) -> anyhow::Result<group::GroupPublic> {
        let g = self
            .get_group(group_id)
            .ok_or_else(|| anyhow::anyhow!("unknown group {group_id}"))?;
        let pubinfo = g.to_public();
        self.dir.upsert_group(&pubinfo);
        Ok(pubinfo)
    }

    /// Sends the group's current secret to every other member over the
    /// request/response channel (one sealed bundle per member). The receiver
    /// handles it in `apply_inbound` under `MsgKind::GroupKey`.
    pub fn publish_group_key(&self, group_id: &str) -> anyhow::Result<usize> {
        let g = self
            .get_group(group_id)
            .ok_or_else(|| anyhow::anyhow!("unknown group {group_id}"))?;
        let me = self.peer_base58();
        let my_e2e = self.e2e_public().to_string();
        let secret = g.secret();
        let mut sent = 0;
        for peer in g.member_ids() {
            if peer == me {
                continue;
            }
            let their_e2e = match g.member(&peer) {
                Some(mi) => mi.e2e_public.clone(),
                None => continue,
            };
            let bundle = group::seal_secret(self.account(), &their_e2e, &secret)
                .map_err(|e| anyhow::anyhow!("seal key: {e}"))?;
            let sealed = crate::account::b64(&bundle);
            let pid = match peer.parse::<PeerId>() {
                Ok(p) => p,
                Err(_) => continue,
            };
            // Best-effort dial: pull endpoint from directory if available,
            // but skip the addr requirement when the peer is already a local
            // neighbour (request/response works over any established stream).
            if let Ok(rec) = self.dir.resolve_device(&peer) {
                if let Some(endpoint) = rec.endpoints.first() {
                    if let Ok(addr) = endpoint.parse::<Multiaddr>() {
                        let _ = self.running.cmd_tx.send(Cmd::Connect { peer: pid.clone(), addr });
                    }
                }
            }
            if self
                .running
                .cmd_tx
                .send(Cmd::GroupKeyDirect {
                    peer: pid,
                    from: me.clone(),
                    e2e: my_e2e.clone(),
                    group_id: group_id.into(),
                    sealed,
                })
                .is_ok()
            {
                sent += 1;
            }
        }
        Ok(sent)
    }

    pub async fn send_group_message(&self, group_id: &str, text: &str) -> anyhow::Result<()> {
        let env = self.seal_group_message(group_id, text)?;
        let wire = group::msg_wire(&env)?;
        let topic = sw::IdentTopic::new(sw::group_topic_name(group_id));
        self
            .running
            .cmd_tx
            .send(Cmd::GroupPublish { topic, data: wire })
            .map_err(|e| anyhow::anyhow!("publish group msg: {e}"))?;
        self.store_group_outbound(&env).await?;
        Ok(())
    }

    fn save_group(&self, g: &group::Group) -> anyhow::Result<()> {
        g.save(&self.account, &self.groups_dir.join(format!("{}.json", g.group_id)))?;
        self.groups.write().insert(g.group_id.clone(), g.clone());
        Ok(())
    }

    pub async fn drain_inbound(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(evt) = self.events.try_recv() {
            match evt {
                sw::ChatEvent::Text { peer, req } => {
                    let peer_b58 = peer.to_base58();
                    match req.kind {
                        message::MsgKind::Dm => {
                            let text = req.text.clone().unwrap_or_default();
                            let chat = store::dm_chat_id(&self.peer_base58(), &peer_b58);
                            self.store.push(&chat, &peer_b58, &text, req.sealed.is_some()).await;
                            out.push(chat);
                        }
                        _ => {
                            if let Ok(Some(r)) = self.apply_inbound(&req).await {
                                let gid = match r {
                                    InboundGroup::Key { group_id } => group_id,
                                    InboundGroup::Message { group_id, .. } => group_id,
                                };
                                out.push(gid);
                            }
                        }
                    }
                }
                sw::ChatEvent::GroupPacket { topic, from, data } => {
                    let Some(group_id) = sw::topic_to_group_id(&topic) else {
                        continue;
                    };
                    if from.to_base58() == self.peer_base58() {
                        continue;
                    }
                    if !self.group_member(&group_id) {
                        continue;
                    }
                    let wire = match group::parse_group_wire(&data) {
                        Ok(w) => w,
                        Err(e) => {
                            tracing::warn!("parse group wire for {group_id}: {e}");
                            continue;
                        }
                    };
                    if wire.env.group_id != group_id {
                        continue;
                    }
                    match self.open_group_message(&group_id, &wire.env) {
                        Ok(_text) => {
                            let _ = self.store_group_inbound(&wire.env).await;
                            out.push(group_id);
                        }
                        Err(e) => tracing::warn!("open group msg {group_id}: {e}"),
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn group_member(&self, group_id: &str) -> bool {
        let guard = self.groups.read();
        guard
            .get(group_id)
            .map(|g| g.has(&self.peer_base58()))
            .unwrap_or(false)
    }

}
