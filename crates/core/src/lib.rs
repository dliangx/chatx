pub mod account;
pub mod friendq;
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
use parking_lot::RwLock;
use signal::{DirectoryClient, UserResolve};
use std::collections::BTreeMap;
use std::sync::Arc;
use store::Store;
use swarm::{self as sw, Cmd, Running};
use tokio::sync::oneshot;

pub const GROUP_P2P_MAX: usize = 15;

pub struct Client<D: DirectoryClient + ?Sized> {
    account: Account,
    device: DeviceIdentity,
    dir: Arc<D>,
    running: Arc<Running>,
    store: Arc<Store>,
    events: tokio::sync::Mutex<sw::EventRx>,
    groups: RwLock<BTreeMap<String, Group>>,
    groups_dir: std::path::PathBuf,
}

#[derive(Debug)]
pub enum InboundGroup {
    Key {
        group_id: String,
    },
    Message {
        group_id: String,
        from: String,
        text: String,
    },
}

impl<D: DirectoryClient + ?Sized + 'static> Client<D> {
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

        let peer_b58 = device.peer_base58().to_string();
        let username = account.user_id().to_string();
        sqlite::devices::bind_user_device(&db, &username, &peer_b58)
            .await
            .map_err(|e| anyhow::anyhow!("bind user+device: {e}"))?;
        let store = Arc::new(Store::with_sql(db, peer_b58));

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

        let client = Self {
            account,
            device,
            dir,
            running,
            store,
            events: tokio::sync::Mutex::new(events),
            groups: RwLock::new(groups),
            groups_dir,
        };

        client.reconnect_groups();
        Ok(client)
    }

    pub fn reconnect_groups(&self) {
        let have_circuit = self.running.listen_addrs.lock().unwrap().iter().any(|a| {
            a.iter()
                .any(|p| matches!(p, libp2p::multiaddr::Protocol::P2pCircuit))
        });
        if !have_circuit {
            let relays: Vec<(PeerId, Multiaddr)> = self.running.relays.lock().unwrap().clone();
            for (peer, base) in relays {
                if let Err(e) = self.running.cmd_tx.send(Cmd::ReserveRelay {
                    relay_peer: peer,
                    relay_addr: base,
                }) {
                    tracing::warn!("reserve relay {peer}: {e}");
                }
            }
        }

        let group_ids: Vec<String> = self.groups.read().keys().cloned().collect();
        for group_id in &group_ids {
            if let Err(e) = self.running.cmd_tx.send(Cmd::GroupSubscribe {
                group_id: group_id.clone(),
            }) {
                tracing::warn!("re-subscribe group {group_id}: {e}");
            }
        }

    }

    pub fn connect_to_group(&self, group_id: &str) -> Result<(), anyhow::Error> {
        let g = self
            .get_group(group_id)
            .ok_or_else(|| anyhow::anyhow!("unknown group {group_id}"))?;
        if g.mode == crate::group::GroupMode::Server {
            return Ok(());
        }
        let me = self.peer_base58();
        let mut dials = 0usize;
        for pid_b58 in g.member_ids() {
            if pid_b58 == me {
                continue;
            }
            if dials >= sw::MAX_DIALS_PER_TICK {
                break;
            }
            let Ok(pid) = pid_b58.parse::<PeerId>() else {
                continue;
            };
            if self
                .running
                .connected
                .lock()
                .unwrap()
                .get(&pid)
                .copied()
                .unwrap_or(0)
                > 0
            {
                continue;
            }
            if self.dial_suppressed(&pid_b58) {
                continue;
            }
            let Ok(rec) = self.dir.resolve_device(&pid_b58) else {
                continue;
            };
            let Some(endpoint) = rec
                .endpoints
                .iter()
                .filter_map(|e| e.parse::<Multiaddr>().ok())
                .find(|a| unroutable_reason(a).is_none())
            else {
                continue;
            };
            if self
                .running
                .cmd_tx
                .send(Cmd::Connect {
                    peer: pid,
                    addr: endpoint,
                })
                .is_ok()
            {
                dials += 1;
                self.dial_attempted(&pid_b58);
            }
        }
        if dials > 0 {
            eprintln!("[group] connect_group {group_id}: dialed {dials} peers");
        }
        Ok(())
    }

    pub fn warm_dial_peer(&self, peer_base58: &str) -> Result<(), anyhow::Error> {
        let me = self.peer_base58();
        if peer_base58 == me {
            return Ok(());
        }
        let Ok(pid) = peer_base58.parse::<PeerId>() else {
            return Ok(());
        };
        if self
            .running
            .connected
            .lock()
            .unwrap()
            .get(&pid)
            .copied()
            .unwrap_or(0)
            > 0
        {
            return Ok(());
        }
        if self.dial_suppressed(&peer_base58) {
            return Ok(());
        }
        let rec = self
            .dir
            .resolve_device(peer_base58)
            .map_err(|e| anyhow::anyhow!("device {peer_base58} not found in directory: {e}"))?;
        let endpoint = rec
            .endpoints
            .iter()
            .filter_map(|e| e.parse::<Multiaddr>().ok())
            .find(|a| unroutable_reason(a).is_none());
        let Some(endpoint) = endpoint else {
            eprintln!(
                "[dm] warm_dial skip {peer_base58}: only loopback/wildcard addrs registered"
            );
            return Ok(());
        };
        if self
            .running
            .cmd_tx
            .send(Cmd::Connect {
                peer: pid,
                addr: endpoint.clone(),
            })
            .is_ok()
        {
            self.dial_attempted(&peer_base58);
            eprintln!("[dm] warm_dial {peer_base58} -> {endpoint}");
        }
        Ok(())
    }

    pub fn disconnect_from_group(&self, group_id: &str) {
        let g = match self.get_group(group_id) {
            Some(g) => g,
            None => return,
        };
        if g.mode == crate::group::GroupMode::Server {
            return;
        }
        let me = self.peer_base58();
        let mut n = 0usize;
        for pid_b58 in g.member_ids() {
            if pid_b58 == me {
                continue;
            }
            let Ok(pid) = pid_b58.parse::<PeerId>() else {
                continue;
            };
            let is_conn = self
                .running
                .connected
                .lock()
                .unwrap()
                .get(&pid)
                .copied()
                .unwrap_or(0)
                > 0;
            if !is_conn {
                continue;
            }
            if self
                .running
                .cmd_tx
                .send(Cmd::Disconnect { peer: pid })
                .is_ok()
            {
                n += 1;
            }
        }
        if n > 0 {
            eprintln!("[group] disconnect {group_id}: closed {n} peers");
        }
    }

    fn dial_suppressed(&self, peer: &str) -> bool {
        let (last, fails) = {
            let m = self.running.dial_backoff.lock().unwrap();
            match m.get(peer) {
                Some(v) => *v,
                None => return false,
            }
        };
        let elapsed = crate::message::now_ms().saturating_sub(last);
        let delay = sw::DIAL_BACKOFF_BASE_MS.saturating_mul(1u64 << fails.min(6));
        elapsed < delay.min(sw::DIAL_BACKOFF_MAX_MS)
    }

    fn dial_attempted(&self, peer: &str) {
        let mut m = self.running.dial_backoff.lock().unwrap();
        let entry = m.entry(peer.to_string()).or_insert((0, 0));
        entry.0 = crate::message::now_ms();
        entry.1 = entry.1.saturating_add(1);
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
            let c = Self::from_parts(
                acct.clone(),
                device,
                dir,
                &DeviceIdentity::home_dir(),
                profile,
            )
            .await?;
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

        let c = Self::from_parts(
            acct.clone(),
            device,
            dir,
            &DeviceIdentity::home_dir(),
            profile,
        )
        .await?;
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
        let c = Self::from_parts(
            acct.clone(),
            device,
            dir,
            &DeviceIdentity::home_dir(),
            profile,
        )
        .await?;
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
            anyhow::bail!(
                "this device is not registered under user {}",
                self.account.user_id()
            );
        }
        if me.status != DeviceStatus::Approved {
            anyhow::bail!(
                "this device is {:?} — only APPROVED can issue attestations",
                me.status
            );
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
        let ep = self.endpoints_with_egress();
        let user_rec = UserRecord {
            user_id: self.account.user_id().to_string(),
            e2e_public: self.account.e2e_public().to_string(),
            sign_pk: self.account.sign_pk().to_string(),
            seen: message::now_ms(),
        };
        self.dir.upsert_user(&user_rec);

        match self.dir.resolve_device(self.device.peer_base58()) {
            Ok(mut rec) => {
                rec.endpoints = ep;
                rec.seen = message::now_ms();
                self.dir.upsert_device(&rec);
            }
            Err(_) => {
                self.fallback_register();
            }
        }
    }

    pub fn start_presence_loop(self: &Arc<Self>) {
        static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if STARTED.set(()).is_err() {
            return;
        }
        let client = self.clone();
        let handle = std::thread::Builder::new()
            .name("chatx-presence".to_string())
            .spawn(move || {
                let mut tick = 0u32;
                loop {
                    tick = tick.wrapping_add(1);
                    std::thread::sleep(std::time::Duration::from_secs(15));
                    client.heartbeat();
                }
            })
            .expect("spawn presence thread");
        let _ = handle;
    }

    fn endpoints_with_egress(&self) -> Vec<String> {
        let mut v = self.endpoints();
        if let Some(ip) = current_egress_ip() {
            if let Some(pos) = v.iter().position(|a| a.contains(&ip)) {
                v.swap_remove(pos);
            }
            let port = v.iter().find_map(|a| tcp_port_of(a));
            match port {
                Some(p) => v.insert(0, format!("/ip4/{ip}/tcp/{p}")),
                None => {
                    eprintln!("[dir] no listening port found; not advertising egress {ip}");
                }
            }
        }
        v
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
            endpoints: self.endpoints_with_egress(),
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
            endpoints: self.endpoints_with_egress(),
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
            endpoints: self.endpoints_with_egress(),
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
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        self.running
            .listen_addrs
            .lock()
            .unwrap()
            .iter()
            .map(|a| a.to_string())
            .for_each(|a| {
                seen.insert(a);
            });
        seen.into_iter().collect()
    }

    pub fn resolve_user(&self, user_id: &str) -> anyhow::Result<UserResolve> {
        self.dir.resolve_user_and_device(user_id)
    }

    pub fn resolve_user_registered(&self, user_id: &str) -> anyhow::Result<UserResolve> {
        self.dir.resolve_user_registered(user_id)
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
            ack_tx: None,
        })?;
        Ok(())
    }

    pub fn dial_peer(&self, peer_base58: &str) -> anyhow::Result<PeerId> {
        let rec = self
            .dir
            .resolve_device(peer_base58)
            .map_err(|e| anyhow::anyhow!("peer {peer_base58} not in directory: {e}"))?;
        let endpoint = rec
            .endpoints
            .first()
            .ok_or_else(|| anyhow::anyhow!("peer {peer_base58} is offline (no endpoints)"))?;
        let peer: PeerId = peer_base58
            .parse()
            .map_err(|e| anyhow::anyhow!("bad peer id: {e}"))?;
        let addr: Multiaddr = endpoint
            .parse()
            .map_err(|e| anyhow::anyhow!("bad endpoint: {e}"))?;
        self.running.cmd_tx.send(Cmd::Connect {
            peer: peer.clone(),
            addr,
        })?;
        Ok(peer)
    }

    pub fn send_audio(
        &self,
        peer: PeerId,
        rate: u32,
        ch: u32,
        data: Vec<u8>,
    ) -> anyhow::Result<()> {
        let from = self.peer_base58();
        let e2e = self.e2e_public().to_string();
        self.running.cmd_tx.send(Cmd::SendAudio {
            peer,
            from,
            e2e,
            rate,
            ch,
            data,
        })?;
        Ok(())
    }

    pub fn send_webrtc_signal(&self, peer: PeerId, payload: &str) -> anyhow::Result<()> {
        let from = self.peer_base58();
        let e2e = self.e2e_public().to_string();
        self.running.cmd_tx.send(Cmd::SendWebrtc {
            peer,
            from,
            e2e,
            payload: payload.to_string(),
        })?;
        Ok(())
    }

    pub async fn send_dm(&self, peer_base58: &str, text: &str) -> anyhow::Result<String> {
        eprintln!("[dm] send_dm peer={peer_base58} text_len={}", text.len());
        let rec = self
            .dir
            .resolve_device(peer_base58)
            .map_err(|e| anyhow::anyhow!("device {peer_base58} not found in directory: {e}"))?;
        if rec.endpoints.is_empty() {
            anyhow::bail!("device {peer_base58} is offline (no endpoints)");
        }
        let peer: PeerId = peer_base58
            .parse()
            .map_err(|e| anyhow::anyhow!("bad peer: {e}"))?;

        let mut candidates: Vec<Multiaddr> = Vec::new();
        for endpoint in &rec.endpoints {
            let Ok(addr) = endpoint.parse::<Multiaddr>() else {
                eprintln!("[dm] skip unparseable endpoint: {endpoint}");
                continue;
            };
            if let Some(reason) = unroutable_reason(&addr) {
                eprintln!("[dm] skip unroutable endpoint ({reason}): {addr}");
                continue;
            }
            candidates.push(addr);
        }
        if candidates.is_empty() {
            anyhow::bail!(
                "device {peer_base58} has no dialable endpoint (only loopback/tcp0 addrs are registered — restart the peer with a LAN-reachable address)"
            );
        }

        let from = self.peer_base58();
        let e2e = self.e2e_public().to_string();
        let timeout = std::time::Duration::from_secs(8);

        let mut last_err = String::new();
        for addr in &candidates {
            eprintln!("[dm] attempting send -> {addr}");
            if let Err(e) = self.running.cmd_tx.send(Cmd::Connect {
                peer: peer.clone(),
                addr: addr.clone(),
            }) {
                last_err = format!("cmd channel closed: {e}");
                continue;
            }
            let (tx, rx) = oneshot::channel::<anyhow::Result<()>>();
            let sent_cmd = self.running.cmd_tx.send(Cmd::SendText {
                peer: peer.clone(),
                from: from.clone(),
                e2e: e2e.clone(),
                text: text.to_string(),
                ack_tx: Some(tx),
            });
            if let Err(e) = sent_cmd {
                last_err = format!("cmd channel closed: {e}");
                continue;
            }
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(Ok(()))) => {
                    eprintln!("[dm] acked via {addr}");
                    let chat = store::dm_chat_id(&self.peer_base58(), peer_base58);
                    self.store
                        .push(&chat, &self.peer_base58(), text, false)
                        .await;
                    eprintln!("[dm] send_dm done, chat={chat}");
                    return Ok(chat);
                }
                Ok(Ok(Err(e))) => {
                    eprintln!("[dm] send to {addr} failed: {e}");
                    last_err = e.to_string();
                }
                Ok(Err(_)) => {
                    last_err = "peer connection dropped before ack".to_string();
                }
                Err(_) => {
                    eprintln!("[dm] no ack from {addr} within {timeout:?}");
                    last_err = format!(
                        "no ack from peer within {timeout_secs}s",
                        timeout_secs = timeout.as_secs()
                    );
                }
            }
        }
        anyhow::bail!("failed to deliver to {peer_base58}: {last_err}");
    }

    /// Send a friend-request frame to `peer_base58` over the DM channel and
    /// wait for the peer's ack. Unlike `send_dm`, the frame is NOT stored in
    /// the chat history.
    pub async fn send_friendq(&self, peer_base58: &str, frame: &str) -> anyhow::Result<()> {
        let rec = self
            .dir
            .resolve_device(peer_base58)
            .map_err(|e| anyhow::anyhow!("device {peer_base58} not found in directory: {e}"))?;
        if rec.endpoints.is_empty() {
            anyhow::bail!("device {peer_base58} is offline (no endpoints)");
        }
        let peer: PeerId = peer_base58
            .parse()
            .map_err(|e| anyhow::anyhow!("bad peer: {e}"))?;

        let mut candidates: Vec<Multiaddr> = Vec::new();
        for endpoint in &rec.endpoints {
            let Ok(addr) = endpoint.parse::<Multiaddr>() else {
                continue;
            };
            if let Some(reason) = unroutable_reason(&addr) {
                eprintln!("[friendq] skip unroutable endpoint ({reason}): {addr}");
                continue;
            }
            candidates.push(addr);
        }
        if candidates.is_empty() {
            anyhow::bail!("device {peer_base58} has no dialable endpoint");
        }

        let from = self.peer_base58();
        let e2e = self.e2e_public().to_string();
        let timeout = std::time::Duration::from_secs(8);
        let mut last_err = String::new();
        for addr in &candidates {
            eprintln!("[friendq] send -> {peer_base58} via {addr}");
            if let Err(e) = self.running.cmd_tx.send(Cmd::Connect {
                peer: peer.clone(),
                addr: addr.clone(),
            }) {
                last_err = format!("cmd channel closed: {e}");
                continue;
            }
            let (tx, rx) = oneshot::channel::<anyhow::Result<()>>();
            if let Err(e) = self.running.cmd_tx.send(Cmd::SendText {
                peer: peer.clone(),
                from: from.clone(),
                e2e: e2e.clone(),
                text: frame.to_string(),
                ack_tx: Some(tx),
            }) {
                last_err = format!("cmd channel closed: {e}");
                continue;
            }
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(Ok(()))) => {
                    eprintln!("[friendq] acked via {addr}");
                    return Ok(());
                }
                Ok(Ok(Err(e))) => {
                    eprintln!("[friendq] send to {addr} failed: {e}");
                    last_err = e.to_string();
                }
                Ok(Err(_)) => last_err = "peer connection dropped before ack".to_string(),
                Err(_) => last_err = format!("no ack from peer within {:?}s", timeout.as_secs()),
            }
        }
        anyhow::bail!("failed to deliver friendq to {peer_base58}: {last_err}");
    }

    pub fn all_messages(&self) -> Vec<store::StoredMsg> {
        self.store.all()
    }

    pub async fn history(
        &self,
        chat_id: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<store::StoredMsg>> {
        self.store.load(chat_id, limit, offset).await
    }

    pub fn shared_key(&self, their_e2e_public: &str) -> anyhow::Result<[u8; 32]> {
        let k = self
            .account
            .derive_session_key(their_e2e_public)
            .map_err(|e| anyhow::anyhow!("derive: {e}"))?;
        Ok(k)
    }

    pub async fn next_event(&self) -> Option<sw::ChatEvent> {
        self.events.lock().await.recv().await
    }

    pub async fn try_next_event(&self) -> Option<sw::ChatEvent> {
        self.events.lock().await.try_recv().ok()
    }

    pub async fn process_inbound(
        &self,
        evt: &sw::ChatEvent,
    ) -> anyhow::Result<Option<InboundGroup>> {
        match evt {
            sw::ChatEvent::Text { req, .. } => self.apply_inbound(req).await,
            _ => Ok(None),
        }
    }

    pub async fn apply_inbound(
        &self,
        req: &message::ChatRequest,
    ) -> anyhow::Result<Option<InboundGroup>> {
        match req.kind {
            message::MsgKind::Dm => Ok(None),
            message::MsgKind::Audio => Ok(None),
            message::MsgKind::Webrtc => Ok(None),
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
                let _ = self.running.cmd_tx.send(Cmd::GroupSubscribe {
                    group_id: group_id.clone(),
                });
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
                Ok(Some(InboundGroup::Message {
                    group_id,
                    from,
                    text,
                }))
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
        if let Err(e) = self.running.cmd_tx.send(Cmd::GroupSubscribe {
            group_id: group_id.clone(),
        }) {
            tracing::warn!("GroupSubscribe cmd failed: {e}");
        }
        Ok(g)
    }

    pub fn add_member(&self, group_id: &str, info: MemberInfo) -> anyhow::Result<()> {
        let mut g = self
            .groups
            .write()
            .get_mut(group_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown group {group_id}"))?;
        g.set_member(info.clone());
        self.save_group(&g)?;
        let mode = g.mode;
        drop(g);

        if mode == group::GroupMode::Server {
            let secret = self
                .get_group(group_id)
                .ok_or_else(|| anyhow::anyhow!("unknown group {group_id}"))?
                .secret();
            let me = self.peer_base58();
            if !info.peer_id.is_empty() && info.peer_id != me {
                let bundle = group::seal_secret(&self.account, &info.e2e_public, &secret)
                    .map_err(|e| anyhow::anyhow!("seal key: {e}"))?;
                let sealed = crate::account::b64(&bundle);
                let my_e2e = self.e2e_public().to_string();
                self.dir
                    .put_group_key_slot(group_id, &info.peer_id, &my_e2e, &sealed)?;
            }
            self.announce_group(group_id)?;
            return Ok(());
        }

        self.running
            .cmd_tx
            .send(Cmd::GroupSubscribe {
                group_id: group_id.into(),
            })
            .map_err(|e| anyhow::anyhow!("GroupSubscribe cmd failed: {e}"))?;
        if !info.peer_id.is_empty() && info.peer_id != self.peer_base58() {
            self.publish_group_key(group_id)?;
        }
        Ok(())
    }

    pub fn rotate_group(&self, group_id: &str) -> anyhow::Result<group::Group> {
        let mut g = self
            .groups
            .read()
            .get(group_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown group {group_id}"))?;
        let mode = g.mode;
        let me = self.peer_base58();
        let my_e2e = self.e2e_public().to_string();
        g.rotate();
        let secret = g.secret();

        if mode == group::GroupMode::Server {
            let members = g.members.clone();
            for (peer, mi) in members.iter() {
                if peer.as_str() == me.as_str() {
                    continue;
                }
                let bundle = group::seal_secret(&self.account, &mi.e2e_public, &secret)
                    .map_err(|e| anyhow::anyhow!("re-seal key for {peer}: {e}"))?;
                let sealed = crate::account::b64(&bundle);
                self.dir
                    .put_group_key_slot(group_id, peer, &my_e2e, &sealed)
                    .map_err(|e| anyhow::anyhow!("put rotated key slot {peer}: {e}"))?;
            }
            self.save_group(&g)?;
            self.announce_group(group_id)?;
            return Ok(g);
        }
        self.save_group(&g)?;
        self.publish_group_key(group_id)?;
        Ok(g)
    }

    pub fn kick_member(&self, group_id: &str, peer_id: &str) -> anyhow::Result<bool> {
        let mut g = self
            .groups
            .read()
            .get(group_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown group {group_id}"))?;
        let existed = g.remove_member(peer_id);
        if !existed {
            return Ok(false);
        }
        self.save_group(&g)?;
        if g.mode == group::GroupMode::Server {
            let _ = self.dir.delete_group_key_slot(group_id, peer_id);
            self.announce_group(group_id)?;
        }
        Ok(true)
    }

    pub fn leave_group(&self, group_id: &str) -> anyhow::Result<()> {
        if !self.groups.read().contains_key(group_id) {
            anyhow::bail!("unknown group {group_id}");
        }
        let _ = self.running.cmd_tx.send(Cmd::GroupUnsubscribe {
            group_id: group_id.into(),
        });
        self.groups.write().remove(group_id);
        let _ = std::fs::remove_file(self.groups_dir.join(format!("{group_id}.json")));
        Ok(())
    }

    pub fn join_group(&self, group_id: &str) -> anyhow::Result<()> {
        let _ = self.running.cmd_tx.send(Cmd::GroupSubscribe {
            group_id: group_id.into(),
        });
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
        let g = self
            .get_group(group_id)
            .ok_or_else(|| anyhow::anyhow!("unknown group {group_id}"))?;
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
        let g = self
            .get_group(group_id)
            .ok_or_else(|| anyhow::anyhow!("unknown group {group_id}"))?;
        group::open_message(&g.secret(), env).map_err(|e| anyhow::anyhow!("open group msg: {e}"))
    }

    pub fn open_stored_group_message(
        &self,
        group_id: &str,
        stored_b64: &str,
    ) -> anyhow::Result<String> {
        let bytes = crate::account::from_b64(stored_b64)
            .ok_or_else(|| anyhow::anyhow!("stored group msg is not base64"))?;
        let env: group::SealedGroupMsg =
            serde_json::from_slice(&bytes).map_err(|e| anyhow::anyhow!("stored group msg: {e}"))?;
        self.open_group_message(group_id, &env)
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
            if let Ok(rec) = self.dir.resolve_device(&peer) {
                if let Some(endpoint) = rec.endpoints.first() {
                    if let Ok(addr) = endpoint.parse::<Multiaddr>() {
                        let _ = self.running.cmd_tx.send(Cmd::Connect {
                            peer: pid.clone(),
                            addr,
                        });
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

    fn p2p_all_peers_offline(&self, group_id: &str) -> bool {
        let g = match self.get_group(group_id) {
            Some(g) if g.mode != group::GroupMode::Server => g,
            _ => return false,
        };
        let me = self.peer_base58();
        let mut others = 0usize;
        let mut any_online = false;
        let conns = self.running.connected.lock().unwrap();
        for p in g.member_ids() {
            if p == me {
                continue;
            }
            others += 1;
            if let Ok(pid) = p.parse::<PeerId>() {
                if *conns.get(&pid).unwrap_or(&0) > 0 {
                    any_online = true;
                    break;
                }
            }
        }
        others > 0 && !any_online
    }

    pub async fn send_group_message(&self, group_id: &str, text: &str) -> anyhow::Result<()> {
        eprintln!(
            "[group] send_group_message group={group_id} text_len={}",
            text.len()
        );
        let env = self.seal_group_message(group_id, text)?;
        if self.group_mode(group_id) == group::GroupMode::Server {
            let seq = self.dir.post_group_message(group_id, &env)?;
            tracing::debug!(group_id, seq, "sent group msg via server");
        } else {
            if self.p2p_all_peers_offline(group_id) {
                eprintln!(
                    "[group] all P2P peers offline — attempting re-dial for {group_id}"
                );
                let _ = self.connect_to_group(group_id);
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_secs(6);
                while std::time::Instant::now() < deadline {
                    if !self.p2p_all_peers_offline(group_id) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
                if self.p2p_all_peers_offline(group_id) {
                    anyhow::bail!(
                        "group {group_id}: no P2P peer online — \
                         open the chat once (or wait for the peer) and retry"
                    );
                }
                eprintln!("[group] re-dial succeeded for {group_id}");
            }
            let wire = group::msg_wire(&env)?;
            let topic = sw::IdentTopic::new(sw::group_topic_name(group_id));
            self.running
                .cmd_tx
                .send(Cmd::GroupPublish { topic, data: wire })
                .map_err(|e| anyhow::anyhow!("publish group msg: {e}"))?;
        }
        self.store_group_outbound(&env).await?;
        eprintln!("[group] send_group_message done group={group_id}");
        Ok(())
    }

    fn group_mode(&self, group_id: &str) -> group::GroupMode {
        self.groups
            .read()
            .get(group_id)
            .map(|g| g.mode)
            .unwrap_or_default()
    }

    pub fn create_server_group(
        &self,
        group_id: impl Into<String>,
        members: Vec<MemberInfo>,
    ) -> anyhow::Result<group::Group> {
        let group_id = group_id.into();
        let me_peer = self.peer_base58();
        let mut g = group::Group::create(&group_id, me_peer.clone());
        g.set_mode(group::GroupMode::Server);
        g.set_member(MemberInfo {
            peer_id: me_peer.clone(),
            sign_pk: self.my_sign_pk().to_string(),
            e2e_public: self.e2e_public().to_string(),
        });
        let secret = g.secret();
        for m in members {
            if m.peer_id == me_peer {
                continue;
            }
            let bundle = group::seal_secret(&self.account, &m.e2e_public, &secret)
                .map_err(|e| anyhow::anyhow!("seal key for {}: {e}", m.peer_id))?;
            let sealed = crate::account::b64(&bundle);
            let my_e2e = self.e2e_public().to_string();
            self.dir
                .put_group_key_slot(&group_id, &m.peer_id, &my_e2e, &sealed)?;
            g.set_member(m);
        }
        self.save_group(&g)?;
        self.announce_group(&group_id)?;
        Ok(g)
    }

    pub fn join_server_group(&self, group_id: &str) -> anyhow::Result<group::Group> {
        let pubinfo = self.dir.resolve_group(group_id)?;
        let me = self.peer_base58();
        let my_info = pubinfo.members.get(&me).cloned().ok_or_else(|| {
            anyhow::anyhow!("not a member of group {group_id} (ask the owner to add you)")
        })?;

        let (sender_e2e, sealed_b64) = self.dir.get_group_key_slot(group_id, &me)?;
        let bundle = crate::account::from_b64(&sealed_b64)
            .ok_or_else(|| anyhow::anyhow!("bad server key slot for {group_id}"))?;

        let secret = group::open_secret(&self.account, &sender_e2e, &bundle)
            .map_err(|e| anyhow::anyhow!("open group key: {e}"))?;
        let mut g = group::Group::with_secret(pubinfo.group_id, pubinfo.owner, secret);
        g.set_mode(pubinfo.mode);
        for (pid, mi) in pubinfo.members {
            g.set_member(mi);
            let _ = pid;
        }
        if !g.has(&me) {
            g.set_member(my_info);
        }
        self.save_group(&g)?;
        Ok(g)
    }

    pub async fn fetch_server_inbox_from(&self, group_id: &str, from_seq: u64) -> Vec<String> {
        let entries = match self.dir.poll_group_messages(group_id, from_seq) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("fetch_server_inbox_from {group_id}: {e}");
                return Vec::new();
            }
        };
        let me = self.peer_base58();
        let mut touched = Vec::new();
        for entry in entries {
            if entry.from == me {
                continue;
            }
            let wire_b = match crate::account::from_b64(&entry.wire) {
                Some(b) => b,
                None => continue,
            };
            let _ = group::parse_group_wire(&wire_b);
            let _ = self.running.injected.send(sw::ChatEvent::GroupMail {
                topic: sw::group_topic_name(group_id),
                from: PeerId::random(),
                data: wire_b,
                seq: entry.seq,
            });
            touched.push(group_id.to_string());
        }
        touched
    }

    pub async fn poll_group_server_inbox(&self) -> Vec<String> {
        let work: Vec<(String, u64)> = {
            let groups = self.groups.read();
            groups
                .values()
                .filter(|g| g.mode == group::GroupMode::Server)
                .map(|g| (g.group_id.clone(), g.last_server_seq))
                .collect()
        };

        let me = self.peer_base58();
        let mut touched: Vec<String> = Vec::new();
        for (sid, after) in work {
            let entries = match self.dir.poll_group_messages(&sid, after) {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!("poll server inbox {sid}: {e}");
                    continue;
                }
            };
            for entry in &entries {
                if entry.from == me {
                    continue;
                }
                let wire_b = match crate::account::from_b64(&entry.wire) {
                    Some(b) => b,
                    None => {
                        tracing::warn!(
                            "malformed server inbox entry (b64) for {sid} @ seq {}",
                            entry.seq
                        );
                        continue;
                    }
                };
                let wire = match group::parse_group_wire(&wire_b) {
                    Ok(w) => w,
                    Err(e) => {
                        tracing::warn!(
                            "malformed server inbox wire {sid} @ seq {}: {e}",
                            entry.seq
                        );
                        continue;
                    }
                };
                if wire.env.group_id != sid {
                    continue;
                }
                let _ = self.running.injected.send(sw::ChatEvent::GroupMail {
                    topic: sw::group_topic_name(&sid),
                    from: PeerId::random(),
                    data: wire_b,
                    seq: entry.seq,
                });
                touched.push(sid.clone());
            }
        }
        touched
    }

    fn advance_group_cursor(&self, group_id: &str, seq: u64) {
        if let Some(g) = self.groups.write().get_mut(group_id) {
            if seq > g.last_server_seq {
                g.last_server_seq = seq;
                let _ = self.persist_group_to_disk(g);
            }
        }
    }

    fn save_group(&self, g: &group::Group) -> anyhow::Result<()> {
        self.persist_group_to_disk(g)?;
        self.groups.write().insert(g.group_id.clone(), g.clone());
        Ok(())
    }

    fn persist_group_to_disk(&self, g: &group::Group) -> anyhow::Result<()> {
        g.save(
            &self.account,
            &self.groups_dir.join(format!("{}.json", g.group_id)),
        )?;
        Ok(())
    }

    pub async fn process_event(&self, evt: sw::ChatEvent) -> Option<String> {
        use sw::ChatEvent as E;
        match evt {
            E::Text { peer, req } => {
                let peer_b58 = peer.to_base58();
                match req.kind {
                    message::MsgKind::Dm => {
                        let text = req.text.clone().unwrap_or_default();
                        let chat = store::dm_chat_id(&self.peer_base58(), &peer_b58);
                        self.store
                            .push(&chat, &peer_b58, &text, req.sealed.is_some())
                            .await;
                        Some(chat)
                    }
                    _ => {
                        if let Ok(Some(r)) = self.apply_inbound(&req).await {
                            match r {
                                InboundGroup::Key { group_id } => Some(group_id),
                                InboundGroup::Message { group_id, .. } => Some(group_id),
                            }
                        } else {
                            None
                        }
                    }
                }
            }
            E::GroupPacket { topic, from, data } => {
                self.inbound_group_wire(&topic, from, &data, 0).await
            }
            E::GroupMail {
                topic,
                from,
                data,
                seq,
            } => {
                let gid = self.inbound_group_wire(&topic, from, &data, seq).await;
                if let Some(gid) = &gid {
                    self.advance_group_cursor(gid, seq);
                }
                gid
            }
            _ => None,
        }
    }

    async fn inbound_group_wire(
        &self,
        topic: &str,
        from: PeerId,
        data: &[u8],
        seq: u64,
    ) -> Option<String> {
        let group_id = sw::topic_to_group_id(topic)?;
        if from.to_base58() == self.peer_base58() {
            return None;
        }
        if !self.group_member(&group_id) {
            return None;
        }
        let wire = match group::parse_group_wire(data) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("parse group wire for {group_id}: {e}");
                return None;
            }
        };
        if wire.env.group_id != group_id {
            return None;
        }
        match self.open_group_message(&group_id, &wire.env) {
            Ok(_text) => {
                if seq > 0 {
                    let b64 =
                        crate::account::b64(&serde_json::to_vec(&wire.env).unwrap_or_default());
                    let dup = self
                        .store
                        .all()
                        .iter()
                        .any(|m| m.chat_id == group_id && m.text == b64);
                    if dup {
                        tracing::debug!(group_id, "drop duplicate group envelope");
                        return Some(group_id);
                    }
                }
                let _ = self.store_group_inbound(&wire.env).await;
                Some(group_id)
            }
            Err(e) => {
                tracing::warn!("open group msg {group_id}: {e}");
                None
            }
        }
    }

    pub async fn drain_inbound(&self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(evt) = self.try_next_event().await {
            if let Some(id) = self.process_event(evt).await {
                out.push(id);
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

fn tcp_port_of(addr: &str) -> Option<u16> {
    addr.parse::<Multiaddr>()
        .ok()
        .as_ref()
        .and_then(|a| {
            a.iter().find_map(|c| {
                if let libp2p::multiaddr::Protocol::Tcp(p) = c {
                    Some(p)
                } else {
                    None
                }
            })
        })
        .filter(|p| *p != 0)
}

fn unroutable_reason(addr: &Multiaddr) -> Option<&'static str> {
    let mut saw_tcp0 = false;
    for comp in addr.iter() {
        match comp {
            libp2p::multiaddr::Protocol::Tcp(0) => saw_tcp0 = true,
            libp2p::multiaddr::Protocol::Ip4(ip) if ip.is_loopback() || ip.is_unspecified() => {
                return Some("loopback/wildcard ip");
            }
            libp2p::multiaddr::Protocol::Ip6(ip) if ip.is_loopback() || ip.is_unspecified() => {
                return Some("loopback/wildcard ip");
            }
            _ => {}
        }
    }
    if saw_tcp0 {
        return Some("tcp/0 placeholder");
    }
    None
}

fn current_egress_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:53").ok()?;
    sock.local_addr().ok()?.ip().to_string().into()
}
