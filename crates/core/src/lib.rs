pub mod account;
pub mod friendq;
pub mod group;
pub mod identity;
pub mod logging;
pub mod message;
pub mod moment;
pub mod signal;
pub mod store;
pub mod stun;
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
    /// Last successful wake_peer timestamp per peer (ms since epoch). Used to
    /// rate-limit wake requests to ~1 per 30 s, bounding server load from a
    /// flaky peer.
    wake_last: std::sync::Mutex<std::collections::HashMap<String, u64>>,
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
        let db_path = base.join(profile).join("messages.db");
        let db = sqlite::open(&db_path).await?;
        // `boot` needs a pool so the *serving* side of the moment channel can
        // read our own `social_posts` + media files when a friend requests them.
        let (running, events) = sw::boot(&device, Some(db.clone())).await?;

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
            wake_last: std::sync::Mutex::new(std::collections::HashMap::new()),
        };

        client.reconnect_groups();
        Ok(client)
    }

    /// Reserve a circuit-relay slot on every known relay (skipped when a
    /// circuit is already reserved). Split out of [`Self::reconnect_groups`]
    /// so relay auto-discovery can trigger it independently of group re-sub.
    pub fn reserve_relays(&self) {
        let have_circuit = self.running.listen_addrs.lock().unwrap().iter().any(|a| {
            a.iter()
                .any(|p| matches!(p, libp2p::multiaddr::Protocol::P2pCircuit))
        });
        if have_circuit {
            return;
        }
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

    /// Ask the directory for available circuit relays (`GET /v1/relays`) and
    /// merge them into the local relay set, reserving a circuit on any that is
    /// new. `CHATX_RELAYS` keeps precedence: relays already configured via the
    /// environment are kept and the server's list only fills in what is missing.
    /// Fire-and-forget — a directory without relay discovery simply yields an
    /// empty list and this is a no-op.
    pub async fn discover_relays(&self) {
        if !self.running.relays.lock().unwrap().is_empty() {
            tracing::debug!("[dir] using relays from CHATX_RELAYS; skipping server discovery");
            return;
        }
        let server_relays = self.dir().list_relays();
        if server_relays.is_empty() {
            return;
        }
        let parsed = sw::parse_relay_strings(&server_relays);
        let mut existing = self.running.relays.lock().unwrap();
        for (peer, base) in parsed {
            if existing.iter().any(|(p, _)| *p == peer) {
                continue;
            }
            tracing::info!("[dir] discovered relay {peer} from directory");
            existing.push((peer, base));
        }
        drop(existing);
        self.reserve_relays();
    }

    pub fn reconnect_groups(&self) {
        self.reserve_relays();

        let group_ids: Vec<String> = self.groups.read().keys().cloned().collect();
        for group_id in &group_ids {
            if let Err(e) = self.running.cmd_tx.send(Cmd::GroupSubscribe {
                group_id: group_id.clone(),
            }) {
                tracing::warn!("re-subscribe group {group_id}: {e}");
            }
        }
    }

    /// Ask the directory to wake up `peer_base58` so it dials this device
    /// back. This is the coordinator step of TCP hole-punching: when a direct
    /// dial to the peer fails (both behind NAT, no shared relay), asking the
    /// server to poke it causes it to dial us at the same instant we are trying
    /// to dial it, which opens both NAT mappings. Best-effort — returns `true`
    /// when the server accepted the request; callers should treat `false`
    /// (older server / offline) as "no punch available" and continue with the
    /// relay fallback.
    pub fn wake_peer(&self, peer_base58: &str) -> bool {
        if peer_base58 == self.peer_base58() {
            return false;
        }
        // Per-peer cooldown: don't hammer the server if this peer is flaky.
        let now = crate::message::now_ms();
        {
            let map = self.wake_last.lock().unwrap();
            if let Some(&last) = map.get(peer_base58) {
                if now.saturating_sub(last) < 30_000 {
                    tracing::debug!(peer = peer_base58, "[hole-punch] throttled (within 30 s)");
                    return false;
                }
            }
        }
        let me = self.peer_base58();
        let ok = self.dir().notify(peer_base58, &me);
        if ok {
            self.wake_last
                .lock()
                .unwrap()
                .insert(peer_base58.to_string(), now);
            tracing::info!(
                peer = peer_base58,
                "[hole-punch] requested server wake for peer"
            );
        } else {
            tracing::debug!(
                peer = peer_base58,
                "[hole-punch] notify unsupported or failed; proceeding to relay fallback"
            );
        }
        ok
    }

    /// Dial a peer by peer id, treating it as a wake response: resolve the
    /// peer's endpoints and initiate a dial. Called by the notice pump when
    /// the peer asked us (via `wake_peer`) to dial back. No-op when we are
    /// already connected to them or when the peer id is malformed — the pump
    /// keeps polling regardless.
    pub fn dial_peer_by_id(&self, peer_base58: &str) {
        let me = self.peer_base58();
        if peer_base58 == me {
            return;
        }
        if self.peer_connected_str(peer_base58) {
            tracing::debug!(
                peer = peer_base58,
                "[hole-punch] already connected; skipping"
            );
            return;
        }
        let _ = self.warm_dial_peer(peer_base58);
    }

    /// Cheap connected check that accepts a base-58 peer id string.
    pub fn peer_connected_str(&self, peer_base58: &str) -> bool {
        let Ok(pid) = peer_base58.parse::<PeerId>() else {
            return false;
        };
        self.running
            .connected
            .lock()
            .unwrap()
            .get(&pid)
            .copied()
            .unwrap_or(0)
            > 0
    }

    /// Current link state for a DM peer, as a stable token the UI maps to the
    /// header status dot:
    ///   - `""`         — not connected and no dial in flight  → red
    ///   - `"dialing"`  — a dial / hole-punch in progress      → grey + pulsing
    ///   - `"relay"`    — connected via a relay circuit        → yellow
    ///   - `"direct"`   — connected over a direct transport    → green
    ///
    /// Once a link is established it beats any in-flight dial. The established
    /// kind comes from `link_direct` / `link_relay`, incremented and
    /// decremented on `ConnectionEstablished` / `ConnectionClosed` from the
    /// connection's `endpoint` (`/p2p-circuit` => relay bucket, otherwise
    /// direct): a non-zero `link_direct` count means at least
    /// one live direct transport is up → **direct** (green); otherwise a
    /// non-zero `link_relay` count means the peer is reachable but only over
    /// a relay circuit → **relay** (yellow). Both zero (or the peer not in
    /// `connected`) → fall through to dial state / offline.
    pub fn peer_link(&self, peer_base58: &str) -> &'static str {
        let Ok(pid) = peer_base58.parse::<PeerId>() else {
            return "";
        };
        let connected = self
            .running
            .connected
            .lock()
            .unwrap()
            .get(&pid)
            .copied()
            .unwrap_or(0)
            > 0;
        if connected {
            let direct = self
                .running
                .link_direct
                .lock()
                .unwrap()
                .get(&pid)
                .copied()
                .unwrap_or(0);
            if direct > 0 {
                return "direct";
            }
            let relay = self
                .running
                .link_relay
                .lock()
                .unwrap()
                .get(&pid)
                .copied()
                .unwrap_or(0);
            if relay > 0 {
                return "relay";
            }
            // connected but neither bucket has a record (e.g. a stale count
            // before the first post-restart event) — conservatively report
            // relay rather than falsely claiming green.
            return "relay";
        }
        if self.running.dialing.lock().unwrap().contains(&pid) {
            return "dialing";
        }
        ""
    }

    /// Run one notice-poll round: block (up to ~50 s) on the long-poll, and
    /// for every wake notice received, dial the sender back. Called by a
    /// background thread that the app pumps (see `start_notify_pump`). Returns
    /// the number of peers dialed in this round.
    pub fn process_wakes(&self) -> u32 {
        let me = self.peer_base58();
        let notices = self.dir().poll_notices(&me);
        let mut dialed = 0u32;
        for n in notices {
            if n.from == me {
                continue;
            }
            tracing::info!(from = %n.from, at = n.at, "[hole-punch] received wake — dialing back");
            self.dial_peer_by_id(&n.from);
            dialed += 1;
        }
        dialed
    }

    /// Start a background thread that long-polls the directory for wake
    /// notices and dials the sender back on arrival. Safe to call multiple
    /// times — only the first call spawns the thread.
    /// Start the DM redelivery loop: a tokio task that scans
    /// `dm_send_queue` for due rows (~1.5 s cadence) and reuses
    /// [`Self::try_send_dm_once`] to push them. Bumps the row's attempt
    /// counter on failure (or marks the message row `failed` when the 12
    /// attempt budget is exhausted), and removes the row on success.
    ///
    /// Safe to call multiple times — only the first call spawns.
    ///
    /// Runs on a **dedicated thread with its own `current_thread` tokio
    /// runtime** (mirrors `start_presence_loop`): the login flow invokes
    /// this from the Slint UI thread, which has no ambient tokio context, so
    /// `Handle::current()` would panic. A self-contained runtime also means
    /// the redelivery loop keeps pacing independently of the app's runtime.
    pub fn start_dm_redeliver(self: &Arc<Self>) {
        static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if STARTED.set(()).is_err() {
            return;
        }
        let client = self.clone();
        let _ = std::thread::Builder::new()
            .name("chatx-dm-redeliver".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build dm-redeliver runtime");
                rt.block_on(async {
                    let mut tick = tokio::time::interval(std::time::Duration::from_millis(1500));
                    // First tick fires immediately — pick up anything queued by
                    // a prior run (the app may restart while the peer is still
                    // offline and the local row is still SENDING).
                    loop {
                        tick.tick().await;
                        client.redeliver_due(None).await;
                    }
                });
            })
            .expect("spawn dm-redeliver thread");
    }

    /// One redelivery round: scan `dm_send_queue` for due rows and send
    /// each. Intentionally idempotent — multiple calls in quick succession
    /// (e.g. from a Connected-event handler) are harmless because rows are
    /// processed independently and the peer's ack is the terminal state.
    /// `peer` = `None` to drain all due rows (polling-task path), or a
    /// specific base58 peer id to drain only its rows (event fast-path).
    pub async fn redeliver_due(&self, peer: Option<&str>) {
        let Some(pool) = self.store.pool() else {
            return;
        };
        let res = match peer {
            Some(p) => sqlite::queue::due_for_peer(&pool, p, 16).await,
            None => sqlite::queue::due(&pool, 16).await,
        };
        let rows = match res {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("[redeliver] due scan failed: {e}");
                return;
            }
        };
        if rows.is_empty() {
            return;
        }
        for row in rows {
            // Re-read the text from the local `messages` row (the queue only
            // stores the id — single source of truth for content).
            let Some(text) = sqlite::messages::get(&pool, row.msg_id)
                .await
                .ok()
                .flatten()
                .and_then(|m| m.text_content)
            else {
                // Local row missing (user deleted?) — drop the queue row.
                let _ = sqlite::queue::give_up(&pool, row.msg_id).await;
                tracing::debug!(
                    msg_id = row.msg_id,
                    "[redeliver] local row missing — dropped"
                );
                continue;
            };
            if text.is_empty() {
                let _ = sqlite::queue::give_up(&pool, row.msg_id).await;
                continue;
            }
            tracing::info!(
                msg_id = row.msg_id,
                peer = %row.peer_base58,
                attempt = row.attempts,
                "[redeliver] retrying offline DM"
            );
            match self.try_send_dm_once(&row.peer_base58, &text, row.msg_id as u64).await {
                Ok(()) => {
                    let _ = self.store.set_status(row.msg_id, sqlite::STATUS_SENT).await;
                    let _ = sqlite::queue::give_up(&pool, row.msg_id).await; // drop from queue
                    tracing::info!(msg_id = row.msg_id, peer = %row.peer_base58, "[redeliver] DELIVERED");
                }
                Err(why) => {
                    let attempts = match sqlite::queue::bump(&pool, row.msg_id).await {
                        Ok(a) => a,
                        Err(e) => {
                            tracing::warn!("[redeliver] bump failed: {e}");
                            continue;
                        }
                    };
                    if attempts >= sqlite::queue::MAX_ATTEMPTS {
                        let _ = self
                            .store
                            .set_status(row.msg_id, sqlite::STATUS_FAILED)
                            .await;
                        let _ = sqlite::queue::give_up(&pool, row.msg_id).await;
                        tracing::warn!(
                            msg_id = row.msg_id,
                            peer = %row.peer_base58,
                            attempts,
                            "[redeliver] giving up — marked FAILED (reason: {why})"
                        );
                    } else {
                        tracing::debug!(
                            msg_id = row.msg_id,
                            peer = %row.peer_base58,
                            attempts,
                            "[redeliver] failed, scheduled next attempt ({why})"
                        );
                    }
                }
            }
        }
    }

    pub fn start_notify_pump(self: &Arc<Self>) {
        static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if STARTED.set(()).is_err() {
            return;
        }
        let client = self.clone();
        let _ = std::thread::Builder::new()
            .name("chatx-notify-pump".to_string())
            .spawn(move || {
                loop {
                    let n = client.process_wakes();
                    if n == 0 {
                        // No notices in this window; the long-poll already blocked
                        // for the server timeout, which is the natural pacing. No
                        // additional sleep needed (and one would just delay the
                        // next punch by a second or two).
                    }
                }
            })
            .expect("spawn notify pump thread");
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
            tracing::debug!("[group] connect_group {group_id}: dialed {dials} peers");
        }
        Ok(())
    }

    pub fn warm_dial_peer(&self, peer_base58: &str) -> Result<(), anyhow::Error> {
        let me = self.peer_base58();
        if peer_base58 == me {
            tracing::debug!("[dm] warm_dial skip: target is self");
            return Ok(());
        }
        let Ok(pid) = peer_base58.parse::<PeerId>() else {
            tracing::warn!("[dm] warm_dial skip: bad peer id {peer_base58}");
            return Ok(());
        };
        let conn_count = self
            .running
            .connected
            .lock()
            .unwrap()
            .get(&pid)
            .copied()
            .unwrap_or(0);
        if conn_count > 0 {
            // Stale `connected` count would silently skip every future dial —
            // log loudly so it's diagnosable. This is the "B stops trying to
            // reconnect A after a disconnect" bug: the counter leaked to >0.
            // tracing::debug!(
            //     "[dm] warm_dial skip {peer_base58}: already considered connected (count={conn_count})"
            // );
            return Ok(());
        }
        // Reach the peer by dialing every routable address it registered. A
        // single endpoint is often stale (NAT mapping changed), so fail over
        // across the candidates, and re-resolve the latest record between
        // passes so a fresh heartbeat lands before we give up. Loopback and
        // wildcard addresses are never dialed (`unroutable_reason`).
        // `swarm.dial` runs its own multi-addr failover with a ~5s default
        // timeout. Each dial attempt must outlive that, or we abandon an
        // in-flight dial and the "Timeout" fires before the handshake lands —
        // the exact A↔B startup failure: cross-subnet TCP+Noise didn't finish
        // within the old 1500ms window. Likewise keep more passes so we cover
        // the window where the peer is still booting and its fresh port hasn't
        // been heartbeated into the directory yet (we re-resolve each pass).
        const PASSES: usize = 2;
        const PER_ATTEMPT: std::time::Duration = std::time::Duration::from_millis(6000);
        const PASS_GAP: std::time::Duration = std::time::Duration::from_millis(800);
        const POLL: std::time::Duration = std::time::Duration::from_millis(100);

        for pass in 0..PASSES {
            if self.peer_connected(&pid) {
                return Ok(());
            }
            let rec = match self.dir.resolve_device(peer_base58) {
                Ok(rec) => rec,
                Err(e) => {
                    tracing::warn!("[dm] warm_dial resolve {peer_base58} failed: {e}");
                    break;
                }
            };
            let mut candidates: Vec<Multiaddr> = Vec::new();
            for endpoint in &rec.endpoints {
                let Ok(addr) = endpoint.parse::<Multiaddr>() else {
                    continue;
                };
                if let Some(reason) = unroutable_reason(&addr) {
                    tracing::debug!("[dm] warm_dial skip unroutable ({reason}): {addr}");
                    continue;
                }
                if !candidates.iter().any(|a| a == &addr) {
                    candidates.push(addr);
                }
            }
            if candidates.is_empty() {
                tracing::warn!("[dm] warm_dial skip {peer_base58}: no routable endpoints");
                break;
            }
            tracing::debug!(
                "[dm] warm_dial {peer_base58} pass={pass} candidates={}",
                candidates.len()
            );
            for addr in &candidates {
                if self.peer_connected(&pid) {
                    return Ok(());
                }
                if self
                    .running
                    .cmd_tx
                    .send(Cmd::Connect {
                        peer: pid.clone(),
                        addr: addr.clone(),
                    })
                    .is_err()
                {
                    continue;
                }
                tracing::debug!("[dm] warm_dial {peer_base58} -> {addr}");
                let deadline = std::time::Instant::now() + PER_ATTEMPT;
                while std::time::Instant::now() < deadline {
                    if self.peer_connected(&pid) {
                        tracing::debug!("[dm] warm_dial {peer_base58} connected via {addr}");
                        return Ok(());
                    }
                    std::thread::sleep(POLL);
                }
            }
            if pass + 1 < PASSES {
                std::thread::sleep(PASS_GAP);
            }
        }
        Ok(())
    }

    fn peer_connected(&self, peer: &PeerId) -> bool {
        self.running
            .connected
            .lock()
            .unwrap()
            .get(peer)
            .copied()
            .unwrap_or(0)
            > 0
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
            tracing::debug!("[group] disconnect {group_id}: closed {n} peers");
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

    /// Like `login`, but unseals the keystore with a pre-derived 32-byte key
    /// (the PBKDF2 output of the password) instead of the password string.
    /// Used by auto-login so the plaintext password never needs to be cached.
    pub async fn login_key(
        profile: &str,
        key: &[u8; 32],
        label: impl Into<String>,
        dir: Arc<D>,
    ) -> anyhow::Result<(Self, Account)> {
        let keystore_path = DeviceIdentity::keystore_path(profile);
        let ks = account::Keystore::load(&keystore_path)
            .map_err(|e| anyhow::anyhow!("keystore load: {e}"))?;
        let acct = ks
            .open_key(key)
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
        // NAT diagnostics (transition-based, warn once per state change):
        // surface the "registered behind NAT, nobody can dial me" failure mode
        // only when we cross into / out of the non-routable state. If every
        // advertised endpoint is private and we have no relay circuit, cross-NAT
        // peers cannot reach us. Once we recover (public IP / relay appears) we
        // log info-level so the operator can see the state flip — but we do NOT
        // spam the same warn line every 15s heartbeat while still behind NAT.
        let has_public = ep.iter().any(|e| is_routable_public_or_circuit(e));
        static NAT_WARNED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if !has_public {
            if NAT_WARNED
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
            {
                let stun_state = match stun::discover_cached() {
                    Some(_) => "STUN hit but symmetric NAT",
                    None => "STUN miss (outbound UDP blocked / offline / prewarm not done)",
                };
                tracing::warn!(
                    endpoints = %ep.join(","),
                    stun = %stun_state,
                    "no public or relay endpoint advertised — reachable only on the local LAN; \
                     cross-network dialing will fail. \
                     Check: (1) outbound UDP to ephemeral ports (2) CHATX_RELAYS configured \
                     (3) server /v1/relays has a usable relay"
                );
            }
        } else {
            if NAT_WARNED
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
            {
                tracing::info!(
                    endpoints = %ep.join(","),
                    "routable public endpoint now advertised — cross-NAT dialing should succeed"
                );
            }
        }
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

        // Prewarm the STUN cache off the login/UI thread: `stun::discover()`
        // does blocking UDP I/O for up to ~18 s worst case (6 servers × 3 s
        // each). Run it in the presence thread before the first heartbeat so
        // endpoints already carry the public IP by the first announce.
        let prewarm = client.clone();
        let _ = std::thread::Builder::new()
            .name("chatx-stun-prewarm".to_string())
            .spawn(move || {
                let res = stun::discover();
                match res {
                    Ok(Some(ip)) => tracing::info!(ip = %ip, "STUN prewarm OK"),
                    Ok(None) => tracing::debug!(
                        "STUN prewarm: no public IP found (NAT blocks UDP / offline)"
                    ),
                    Err(e) => tracing::warn!(%e, "STUN prewarm failed"),
                }
                // Trigger one immediate heartbeat so the directory learns our
                // new endpoints without waiting the 15 s tick.
                prewarm.heartbeat();
            })
            .expect("spawn stun prewarm thread");

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

    /// Build the endpoint list advertised to the directory. Prefers the real
    /// **public** IP (learned via STUN, see [`stun`]) and the actual listening
    /// port, which is the only address a peer on another network can dial. When
    /// STUN is unavailable we fall back to the previous LAN-egress behavior so
    /// same-subnet peers still connect.
    ///
    /// The lookup is non-blocking ([`stun::discover_cached`]): `discover()`
    /// (which does UDP I/O) is run in the prewarm thread started by
    /// [`Self::start_presence_loop`].
    fn endpoints_with_egress(&self) -> Vec<String> {
        let public = stun::discover_cached();
        let base = self.endpoints();
        if let Some(ip) = public {
            let ip_proto = match ip.ip {
                std::net::IpAddr::V4(_) => "ip4",
                std::net::IpAddr::V6(_) => "ip6",
            };
            let public_ip = ip.ip;
            let tcp_port = base.iter().find_map(|a| tcp_port_of(a));
            let udp_port = base.iter().find_map(|a| udp_port_of(a));
            if tcp_port.is_some() || udp_port.is_some() {
                let mut list = Vec::with_capacity(base.len() + 2);
                if let Some(port) = tcp_port {
                    list.push(format!("/{ip_proto}/{public_ip}/tcp/{port}"));
                }
                // QUIC is the primary direct path and what DCUtR hole-punching
                // coordinates over — only advertising the TCP public endpoint
                // previously left cross-network peers without a public QUIC
                // candidate, suppressing hole-punch success.
                if let Some(port) = udp_port {
                    list.push(format!("/{ip_proto}/{public_ip}/udp/{port}/quic-v1"));
                }
                list.extend(base);
                return list;
            }
            tracing::warn!(
                "STUN public IP {ip} found but no listening port known yet; \
                 advertising local endpoints only"
            );
        }
        // Fallback (STUN not yet done / no public IP): the legacy LAN egress
        // endpoint, which is useful for same-network peers.
        let mut v = base;
        if let Some(ip) = current_egress_ip() {
            if let Some(pos) = v.iter().position(|a| a.contains(&ip)) {
                v.swap_remove(pos);
            }
            if let Some(p) = v.iter().find_map(|a| tcp_port_of(a)) {
                v.insert(0, format!("/ip4/{ip}/tcp/{p}"));
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
        // Best-effort: learn the directory's circuit relays before we register,
        // so the freshly-announced endpoints already include a relay-circuit
        // address (the only NAT-traversal fallback when direct dial fails).
        self.discover_relays().await;
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
            client_id: None,
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

    /// Common dial + send-over-blob-channel plumbing. Resolves the peer's
    /// dialable endpoints in the directory, then for each endpoint dials and
    /// sends the binary request, waiting for the matching `PostResponse`.
    async fn dial_and_send_blob(
        &self,
        peer_base58: &str,
        op: crate::moment::PostReqOp,
        timeout: std::time::Duration,
    ) -> anyhow::Result<crate::moment::PostResponse> {
        let rec = self
            .dir
            .resolve_device(peer_base58)
            .map_err(|e| anyhow::anyhow!("device {peer_base58} not found: {e}"))?;
        if rec.endpoints.is_empty() {
            anyhow::bail!("device {peer_base58} is offline (no endpoints)");
        }
        let peer: PeerId = peer_base58
            .parse()
            .map_err(|e| anyhow::anyhow!("bad peer: {e}"))?;

        let mut candidates: Vec<Multiaddr> = Vec::new();
        for endpoint in &rec.endpoints {
            let Ok(addr) = endpoint.parse::<Multiaddr>() else {
                tracing::debug!("[moment] skip unparseable endpoint: {endpoint}");
                continue;
            };
            if let Some(reason) = unroutable_reason(&addr) {
                tracing::debug!("[moment] skip unroutable endpoint ({reason}): {addr}");
                continue;
            }
            candidates.push(addr);
        }
        if candidates.is_empty() {
            anyhow::bail!("device {peer_base58} has no dialable endpoint");
        }

        let mut last_err = String::new();
        for addr in &candidates {
            let id: u64 = rand::random();
            let req = crate::moment::PostRequest { id, op: op.clone() };
            let (tx, rx) = oneshot::channel::<anyhow::Result<crate::moment::PostResponse>>();
            if let Err(e) = self.running.cmd_tx.send(Cmd::Connect {
                peer: peer.clone(),
                addr: addr.clone(),
            }) {
                last_err = format!("cmd channel closed: {e}");
                continue;
            }
            if let Err(e) = self.running.cmd_tx.send(Cmd::SendBlob {
                peer,
                req,
                ack_tx: tx,
            }) {
                last_err = format!("cmd channel closed: {e}");
                continue;
            }
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(Ok(resp))) => {
                    tracing::debug!("[moment] blob ok id={id} via {addr}");
                    return Ok(resp);
                }
                Ok(Ok(Err(e))) => {
                    tracing::warn!("[moment] blob to {addr} failed: {e}");
                    last_err = e.to_string();
                }
                Ok(Err(_)) => last_err = "peer dropped connection before response".to_string(),
                Err(_) => {
                    tracing::warn!("[moment] no response within {timeout:?} via {addr}");
                    last_err =
                        format!("no response within {timeout}s", timeout = timeout.as_secs());
                }
            }
        }
        anyhow::bail!("failed to sync from {peer_base58}: {last_err}");
    }

    /// Request metadata for posts in `(since, until]` from a friend's device.
    pub async fn request_post_list(
        &self,
        peer_base58: &str,
        since: i64,
        until: i64,
    ) -> anyhow::Result<Vec<crate::moment::PostMeta>> {
        let resp = self
            .dial_and_send_blob(
                peer_base58,
                crate::moment::PostReqOp::List { since, until },
                std::time::Duration::from_secs(10),
            )
            .await?;
        match resp.payload {
            crate::moment::PostPayload::List { posts } => Ok(posts),
            crate::moment::PostPayload::Image { .. } => {
                anyhow::bail!("unexpected Image response to a List request")
            }
            crate::moment::PostPayload::Err { msg } => anyhow::bail!("remote error: {msg}"),
        }
    }

    /// Request raw bytes of one image (`kind` 0=original, 1=preview).
    pub async fn request_post_image(
        &self,
        peer_base58: &str,
        ts: i64,
        which: u32,
        kind: u32,
    ) -> anyhow::Result<Vec<u8>> {
        let resp = self
            .dial_and_send_blob(
                peer_base58,
                crate::moment::PostReqOp::Image { ts, which, kind },
                std::time::Duration::from_secs(20),
            )
            .await?;
        match resp.payload {
            crate::moment::PostPayload::Image { bytes } => Ok(bytes),
            crate::moment::PostPayload::List { .. } => {
                anyhow::bail!("unexpected List response to an Image request")
            }
            crate::moment::PostPayload::Err { msg } => anyhow::bail!("remote error: {msg}"),
        }
    }

    /// Send a DM to `peer_base58` and wait for the peer's ack.
    ///
    /// Behaviour:
    /// 1. Pre-insert a local `status = SENDING` row (UI-visible immediately,
    ///    "发送中" bubble).
    /// 2. Enqueue the row into the DM send queue (so the redelivery task can
    ///    retry it if the peer is offline right now).
    /// 3. Attempt a live dial (round 1 = all known endpoints, round 2 = wake
    ///    peer and re-try).
    /// 4a. On ack: bump row to `sent`, drop the queue row, return chat id.
    /// 4b. On failure: keep both the local row and the queue row (attempts
    ///     remain 0 — the redelivery task will own the backoff). Return an
    ///     `Err` describing the last attempt; the UI shows a transient
    ///     "sending in background" notice and the bubble stays in SENDING
    ///     state until the peer acks or the 12-attempt budget (~10 h) is
    ///     exhausted, at which point the row flips to `failed`.
    ///
    /// **Text only** — media (voice/image/file) keeps the legacy "fail and
    /// let the user retry" semantics: `send_dm` is only used for
    /// `Kind::Text` rows and the queue table is only populated with text
    /// rows.
    /// Stage a text DM for delivery WITHOUT dialing: pre-insert the local
    /// `status = SENDING` row and enqueue it for redelivery, then return the
    /// chat id + local message id immediately. This is the part the UI calls
    /// **before** any network I/O so the "发送中" bubble appears instantly,
    /// even when the peer is offline.
    ///
    /// Returns `Ok((chat_id, msg_id))`. The local-row insert is the contract
    /// (the UI relies on the row existing right after this returns), so a db
    /// failure here is a hard error; the enqueue is best-effort on top.
    pub async fn queue_dm_text(
        &self,
        peer_base58: &str,
        text: &str,
    ) -> anyhow::Result<(String, i64)> {
        let chat = store::dm_chat_id(&self.peer_base58(), peer_base58);
        let msg_id = self.store.insert_mine_sending(&chat, text).await?;
        if let Some(pool) = self.store.pool() {
            let _ = sqlite::queue::enqueue(&pool, msg_id, peer_base58, &chat).await;
        }
        tracing::debug!("[dm] queued peer={peer_base58} msg_id={msg_id} chat={chat}");
        Ok((chat, msg_id))
    }

    /// Attempt to deliver an already-queued text DM right now (the "live"
    /// path): try every dialable endpoint, then wake-and-retry. On ack the
    /// local row flips `SENDING`→`SENT` and the redelivery queue row is
    /// dropped. On failure the row stays `SENDING` and the
    /// `start_dm_redeliver` task owns the backoff.
    ///
    /// Returns `Ok(true)` when the peer acked this attempt, `Ok(false)` when
    /// it didn't (queued for retry). A directory/db-level error that prevented
    /// even attempting a dial is returned as `Err`.
    pub async fn deliver_dm(
        &self,
        msg_id: i64,
        peer_base58: &str,
        text: &str,
    ) -> anyhow::Result<bool> {
        match self.try_send_dm_once(peer_base58, text, msg_id as u64).await {
            Ok(()) => {
                if let Some(pool) = self.store.pool() {
                    let _ = self.store.set_status(msg_id, sqlite::STATUS_SENT).await;
                    let _ = sqlite::queue::give_up(&pool, msg_id).await; // drop queue row
                }
                tracing::debug!("[dm] delivered peer={peer_base58}");
                Ok(true)
            }
            Err(why) => {
                tracing::info!(
                    "[dm] live deliver failed peer={peer_base58} ({why}); redelivery loop owns it"
                );
                Ok(false)
            }
        }
    }

    /// Send a DM in one shot: [`queue_dm_text`] + [`deliver_dm`].
    ///
    /// **Text only** — media (voice/image/file) keeps the legacy "fail and
    /// let the user retry" semantics: `send_dm` is only used for
    /// `Kind::Text` rows and the `dm_send_queue` table is only populated with
    /// text rows.
    pub async fn send_dm(&self, peer_base58: &str, text: &str) -> anyhow::Result<String> {
        let (chat, msg_id) = self.queue_dm_text(peer_base58, text).await?;
        self.deliver_dm(msg_id, peer_base58, text).await?;
        Ok(chat)
    }

    /// Live dial attempt (round 1 = all endpoints, round 2 = wake + retry).
    /// Returns `Ok(())` on peer ack or `Err(last_err_description)`. Does NOT
    /// touch the local message row or queue — that's the caller's job so
    /// both the sync `send_dm` path and the async redelivery path can share
    /// the same dial logic.
    ///
    /// `client_id` is the stable sender-local id of this logical message; it
    /// is carried on every attempt so the receiver can dedup a redelivery.
    async fn try_send_dm_once(
        &self,
        peer_base58: &str,
        text: &str,
        client_id: u64,
    ) -> Result<(), String> {
        let rec = self
            .dir
            .resolve_device(peer_base58)
            .map_err(|e| format!("device {peer_base58} not found in directory: {e}"))?;
        if rec.endpoints.is_empty() {
            return Err(format!("device {peer_base58} is offline (no endpoints)"));
        }
        let peer: PeerId = peer_base58.parse().map_err(|e| format!("bad peer: {e}"))?;

        let mut candidates: Vec<Multiaddr> = Vec::new();
        for endpoint in &rec.endpoints {
            let Ok(addr) = endpoint.parse::<Multiaddr>() else {
                tracing::debug!("[dm] skip unparseable endpoint: {endpoint}");
                continue;
            };
            if let Some(reason) = unroutable_reason(&addr) {
                tracing::debug!("[dm] skip unroutable endpoint ({reason}): {addr}");
                continue;
            }
            candidates.push(addr);
        }
        if candidates.is_empty() {
            return Err(
                "device {peer_base58} has no dialable endpoint (only loopback/tcp0 addrs are registered — restart the peer with a LAN-reachable address)"
                    .to_string(),
            );
        }

        let from = self.peer_base58();
        let e2e = self.e2e_public().to_string();
        let timeout = std::time::Duration::from_secs(8);

        // Round 1: try every dialable endpoint. Track the last error for
        // diagnostics.
        let mut last_err = String::new();
        for addr in &candidates {
            tracing::debug!("[dm] attempting send -> {addr}");
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
                client_id: Some(client_id),
            });
            if let Err(e) = sent_cmd {
                last_err = format!("cmd channel closed: {e}");
                continue;
            }
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(Ok(()))) => {
                    tracing::info!(
                        path = classify_transport(addr),
                        "[dm] send delivered via {addr}"
                    );
                    return Ok(());
                }
                Ok(Ok(Err(e))) => {
                    tracing::warn!("[dm] send to {addr} failed: {e}");
                    last_err = e.to_string();
                }
                Ok(Err(_)) => {
                    last_err = "peer connection dropped before ack".to_string();
                }
                Err(_) => {
                    tracing::warn!("[dm] no ack from {addr} within {timeout:?}");
                    last_err = format!(
                        "no ack from peer within {timeout_secs}s",
                        timeout_secs = timeout.as_secs()
                    );
                }
            }
        }

        // Round 2: hole-punch. Ask the directory to poke the peer, wait a
        // short window for it to be woken and dial us back, then retry with
        // a fresh endpoint list. Without this the message is lost because
        // libp2p request-response doesn't buffer in-flight messages.
        if self.wake_peer(peer_base58) {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if let Ok(rec2) = self.dir.resolve_device(peer_base58) {
                for endpoint in &rec2.endpoints {
                    let Ok(addr) = endpoint.parse::<Multiaddr>() else {
                        continue;
                    };
                    if let Some(reason) = unroutable_reason(&addr) {
                        tracing::debug!("[dm] retry: skip unroutable ({reason}): {addr}");
                        continue;
                    }
                    tracing::debug!("[dm] retry (post-wake) -> {addr}");
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
                        client_id: Some(client_id),
                    });
                    if let Err(e) = sent_cmd {
                        last_err = format!("cmd channel closed: {e}");
                        continue;
                    }
                    match tokio::time::timeout(timeout, rx).await {
                        Ok(Ok(Ok(()))) => {
                            tracing::info!(
                                path = classify_transport(&addr),
                                "[dm] retry delivered via {addr}"
                            );
                            return Ok(());
                        }
                        Ok(Ok(Err(e))) => {
                            tracing::warn!("[dm] retry send to {addr} failed: {e}");
                            last_err = e.to_string();
                        }
                        Ok(Err(_)) => {
                            last_err = "peer connection dropped before ack".to_string();
                        }
                        Err(_) => {
                            last_err =
                                format!("no ack from peer within {}s (retry)", timeout.as_secs());
                        }
                    }
                }
            }
        }
        Err(format!("failed to deliver to {peer_base58}: {last_err}"))
    }

    /// Send a voice message to `peer_base58` over the DM channel and wait for
    /// the peer's ack. The clip itself is NOT persisted by core — the caller
    /// is expected to have already written the s16le bytes to a local path and
    /// will call [`Self::persist_voice`] after a successful ack (mirroring
    /// `send_dm` + `store.push`).
    pub async fn send_voice_cmd(
        &self,
        peer_base58: &str,
        data: Vec<u8>,
        rate: u32,
        ch: u32,
        duration_ms: u64,
    ) -> anyhow::Result<()> {
        tracing::debug!(
            "[voice] send_voice peer={peer_base58} bytes={} rate={rate} ch={ch} dur={duration_ms}ms",
            data.len()
        );
        let rec = self
            .dir
            .resolve_device(peer_base58)
            .map_err(|e| anyhow::anyhow!("device {peer_base58} not found: {e}"))?;
        if rec.endpoints.is_empty() {
            anyhow::bail!("device {peer_base58} is offline (no endpoints)");
        }
        let peer: PeerId = peer_base58
            .parse()
            .map_err(|e| anyhow::anyhow!("bad peer: {e}"))?;
        let from = self.peer_base58();
        let e2e = self.e2e_public().to_string();
        let timeout = std::time::Duration::from_secs(8);
        let mut last_err = String::new();
        for endpoint in &rec.endpoints {
            let Ok(addr) = endpoint.parse::<Multiaddr>() else {
                continue;
            };
            if let Some(reason) = unroutable_reason(&addr) {
                last_err = format!("{addr} unroutable ({reason})");
                continue;
            }
            if let Err(e) = self.running.cmd_tx.send(Cmd::Connect {
                peer: peer.clone(),
                addr: addr.clone(),
            }) {
                last_err = format!("cmd channel closed: {e}");
                continue;
            }
            let (tx, rx) = oneshot::channel::<anyhow::Result<()>>();
            if let Err(e) = self.running.cmd_tx.send(Cmd::SendVoice {
                peer: peer.clone(),
                from: from.clone(),
                e2e: e2e.clone(),
                rate,
                ch,
                duration_ms,
                data: data.clone(),
                ack_tx: Some(tx),
            }) {
                last_err = format!("cmd channel closed: {e}");
                continue;
            }
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(Ok(()))) => {
                    tracing::info!(
                        path = classify_transport(&addr),
                        "[voice] delivered via {addr}"
                    );
                    return Ok(());
                }
                Ok(Ok(Err(e))) => {
                    last_err = format!("send to {addr} failed: {e}");
                }
                Ok(Err(_)) => {
                    last_err = "peer connection dropped before ack".to_string();
                }
                Err(_) => {
                    last_err = format!("no ack from {addr} within {timeout:?}");
                }
            }
        }
        anyhow::bail!("failed to deliver to {peer_base58}: {last_err}");
    }

    /// Record a voice message in the chat store (after the clip is on disk).
    pub async fn persist_voice(
        &self,
        chat_id: &str,
        sender: &str,
        local_path: &str,
        duration_ms: u64,
    ) {
        let _ = self
            .store
            .push(
                chat_id,
                sender,
                "",
                false,
                store::Kind::Voice {
                    path: local_path.to_string(),
                    duration_ms,
                },
            )
            .await;
    }

    /// Record a photo in the chat store (after the image is on disk). Returns
    /// the row's timestamp id so the caller can tag an optimistic send badge.
    pub async fn persist_image(&self, chat_id: &str, sender: &str, local_path: &str) -> i64 {
        self.store
            .push(
                chat_id,
                sender,
                "",
                false,
                store::Kind::Image {
                    path: local_path.to_string(),
                },
            )
            .await
    }

    pub async fn persist_file(&self, chat_id: &str, sender: &str, name: &str, local_path: &str) {
        let _ = self
            .store
            .push(
                chat_id,
                sender,
                "",
                false,
                store::Kind::File {
                    path: local_path.to_string(),
                    name: name.to_string(),
                },
            )
            .await;
    }

    pub async fn send_image_cmd(&self, peer_base58: &str, data: Vec<u8>) -> anyhow::Result<()> {
        tracing::debug!("[image] send_image peer={peer_base58} bytes={}", data.len());
        let rec = self
            .dir
            .resolve_device(peer_base58)
            .map_err(|e| anyhow::anyhow!("device {peer_base58} not found: {e}"))?;
        if rec.endpoints.is_empty() {
            anyhow::bail!("device {peer_base58} is offline (no endpoints)");
        }
        let peer: PeerId = peer_base58
            .parse()
            .map_err(|e| anyhow::anyhow!("bad peer: {e}"))?;
        let from = self.peer_base58();
        let timeout = std::time::Duration::from_secs(15);
        let mut last_err = String::new();
        for endpoint in &rec.endpoints {
            let Ok(addr) = endpoint.parse::<Multiaddr>() else {
                continue;
            };
            if let Some(reason) = unroutable_reason(&addr) {
                last_err = format!("{addr} unroutable ({reason})");
                continue;
            }
            if let Err(e) = self.running.cmd_tx.send(Cmd::Connect {
                peer: peer.clone(),
                addr: addr.clone(),
            }) {
                last_err = format!("cmd channel closed: {e}");
                continue;
            }
            let (tx, rx) = oneshot::channel::<anyhow::Result<()>>();
            if let Err(e) = self.running.cmd_tx.send(Cmd::SendImage {
                peer: peer.clone(),
                from: from.clone(),
                data: data.clone(),
                ack_tx: Some(tx),
            }) {
                last_err = format!("cmd channel closed: {e}");
                continue;
            }
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(Ok(()))) => {
                    tracing::info!(
                        path = classify_transport(&addr),
                        "[image] delivered via {addr}"
                    );
                    return Ok(());
                }
                Ok(Ok(Err(e))) => {
                    last_err = format!("send to {addr} failed: {e}");
                }
                Ok(Err(_)) => {
                    last_err = "peer connection dropped before ack".to_string();
                }
                Err(_) => {
                    last_err = format!("no ack from {addr} within {timeout:?}");
                }
            }
        }
        anyhow::bail!("failed to deliver to {peer_base58}: {last_err}");
    }

    /// Acknowledge an inbound photo to `peer_base58` **after** the app has
    /// durably written the image file and persisted its row. Unblocks the
    /// sender's `send_image_cmd` waiter (flipping its badge to "sent"). A no-op
    /// if the inbound channel has already been acked or expired.
    pub async fn ack_image(&self, peer_base58: &str, id: u64) -> anyhow::Result<()> {
        let peer: PeerId = peer_base58
            .parse()
            .map_err(|e| anyhow::anyhow!("bad peer: {e}"))?;
        self.running
            .cmd_tx
            .send(Cmd::AckImage { peer, id })
            .map_err(|e| anyhow::anyhow!("swarm command sender closed: {e}"))?;
        tracing::debug!("[image] ack_image to={peer_base58} id={id}");
        Ok(())
    }

    /// Send a document file (pdf/docx/xlsx/pptx) to `peer_base58` over the DM channel and wait for the peer's ack; the caller writes `data` locally first and calls [`Self::persist_file`] after ack (mirrors `send_image_cmd` + `persist_image`).
    pub async fn send_file_cmd(
        &self,
        peer_base58: &str,
        name: String,
        data: Vec<u8>,
    ) -> anyhow::Result<()> {
        tracing::debug!("[file] send_file peer={peer_base58} bytes={}", data.len());
        let rec = self
            .dir
            .resolve_device(peer_base58)
            .map_err(|e| anyhow::anyhow!("device {peer_base58} not found: {e}"))?;
        if rec.endpoints.is_empty() {
            anyhow::bail!("device {peer_base58} is offline (no endpoints)");
        }
        let peer: PeerId = peer_base58
            .parse()
            .map_err(|e| anyhow::anyhow!("bad peer: {e}"))?;
        let from = self.peer_base58();
        let timeout = std::time::Duration::from_secs(15);
        let mut last_err = String::new();
        for endpoint in &rec.endpoints {
            let Ok(addr) = endpoint.parse::<Multiaddr>() else {
                continue;
            };
            if let Some(reason) = unroutable_reason(&addr) {
                last_err = format!("{addr} unroutable ({reason})");
                continue;
            }
            if let Err(e) = self.running.cmd_tx.send(Cmd::Connect {
                peer: peer.clone(),
                addr: addr.clone(),
            }) {
                last_err = format!("cmd channel closed: {e}");
                continue;
            }
            let (tx, rx) = oneshot::channel::<anyhow::Result<()>>();
            if let Err(e) = self.running.cmd_tx.send(Cmd::SendFile {
                peer: peer.clone(),
                from: from.clone(),
                name: name.clone(),
                data: data.clone(),
                ack_tx: Some(tx),
            }) {
                last_err = format!("cmd channel closed: {e}");
                continue;
            }
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(Ok(()))) => {
                    tracing::info!(
                        path = classify_transport(&addr),
                        "[file] delivered via {addr}"
                    );
                    return Ok(());
                }
                Ok(Ok(Err(e))) => {
                    last_err = format!("send to {addr} failed: {e}");
                }
                Ok(Err(_)) => {
                    last_err = "peer connection dropped before ack".to_string();
                }
                Err(_) => {
                    last_err = format!("no ack from {addr} within {timeout:?}");
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
                tracing::debug!("[friendq] skip unroutable endpoint ({reason}): {addr}");
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

        // Round 1: try every dialable endpoint. Track the last error for
        // diagnostics. (Same shape as `send_dm` Round 1.)
        for addr in &candidates {
            tracing::debug!("[friendq] send -> {peer_base58} via {addr}");
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
                client_id: None,
            }) {
                last_err = format!("cmd channel closed: {e}");
                continue;
            }
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(Ok(()))) => {
                    tracing::info!(
                        path = classify_transport(addr),
                        "[friendq] delivered via {addr}"
                    );
                    return Ok(());
                }
                Ok(Ok(Err(e))) => {
                    tracing::warn!("[friendq] send to {addr} failed: {e}");
                    last_err = e.to_string();
                }
                Ok(Err(_)) => last_err = "peer connection dropped before ack".to_string(),
                Err(_) => last_err = format!("no ack from peer within {:?}s", timeout.as_secs()),
            }
        }

        // Round 2: hole-punch. Ask the directory to poke the peer, wait a
        // short window for it to be woken / for its STUN-derived public
        // endpoint or a relay circuit to land, then dial the peer back using
        // a freshly-resolved endpoint list. Without this the request is lost
        // because libp2p request-response doesn't buffer in-flight messages.
        // Mirrors `send_dm` Round 2 exactly (lib.rs:1481-1522).
        if self.wake_peer(peer_base58) {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if let Ok(rec2) = self.dir.resolve_device(peer_base58) {
                for endpoint in &rec2.endpoints {
                    let Ok(addr) = endpoint.parse::<Multiaddr>() else {
                        continue;
                    };
                    if let Some(reason) = unroutable_reason(&addr) {
                        tracing::debug!("[friendq] retry: skip unroutable ({reason}): {addr}");
                        continue;
                    }
                    tracing::debug!("[friendq] retry (post-wake) -> {addr}");
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
                        client_id: None,
                    }) {
                        last_err = format!("cmd channel closed: {e}");
                        continue;
                    }
                    match tokio::time::timeout(timeout, rx).await {
                        Ok(Ok(Ok(()))) => {
                            tracing::info!(
                                path = classify_transport(&addr),
                                "[friendq] retry delivered via {addr}"
                            );
                            return Ok(());
                        }
                        Ok(Ok(Err(e))) => {
                            tracing::warn!("[friendq] retry send to {addr} failed: {e}");
                            last_err = e.to_string();
                        }
                        Ok(Err(_)) => last_err = "peer connection dropped before ack".to_string(),
                        Err(_) => {
                            last_err =
                                format!("no ack from peer within {}s (retry)", timeout.as_secs())
                        }
                    }
                }
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
            // Voice messages are persisted by the application layer
            // once it has written the clip to disk, so core does
            // nothing here.
            message::MsgKind::Voice => Ok(None),
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
                let plain = self.open_group_message(&group_id, &env)?;
                self.store_group_inbound(&env).await?;
                Ok(Some(InboundGroup::Message {
                    group_id,
                    from,
                    text: plain.text,
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
    ) -> anyhow::Result<group::GroupPlain> {
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
    ) -> anyhow::Result<group::GroupPlain> {
        let bytes = crate::account::from_b64(stored_b64)
            .ok_or_else(|| anyhow::anyhow!("stored group msg is not base64"))?;
        let env: group::SealedGroupMsg =
            serde_json::from_slice(&bytes).map_err(|e| anyhow::anyhow!("stored group msg: {e}"))?;
        self.open_group_message(group_id, &env)
    }

    fn seal_group_message_typed(
        &self,
        group_id: &str,
        kind: &group::GroupMsgKind,
        text: &str,
        data: Option<&[u8]>,
    ) -> anyhow::Result<group::SealedGroupMsg> {
        let g = self
            .get_group(group_id)
            .ok_or_else(|| anyhow::anyhow!("unknown group {group_id}"))?;
        let me = self.peer_base58();
        group::seal_message_typed(&g, &self.account, &me, kind, text, data)
            .map_err(|e| anyhow::anyhow!("seal group msg: {e}"))
    }

    /// Write a group media body to the app's local media dir and return its
    /// on-disk path. Mirrors the DM voice/image layout (`CHAV`+rate/ch PCM,
    /// `img-{ts}.<ext>` photo) so the chat renderer + viewer handle both
    /// identically regardless of whether it arrived by DM or group.
    fn write_group_media(
        &self,
        kind: &group::GroupMsgKind,
        data: &[u8],
    ) -> anyhow::Result<std::path::PathBuf> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let home = DeviceIdentity::home_dir();
        match kind {
            group::GroupMsgKind::Voice { rate, ch, .. } => {
                let ch = u32::max(*ch, 1);
                let dir = home.join("voice");
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(format!("v-{ts}.pcm"));
                let mut out = Vec::with_capacity(12 + data.len());
                out.extend_from_slice(b"CHAV");
                out.extend_from_slice(&rate.to_le_bytes());
                out.extend_from_slice(&ch.to_le_bytes());
                out.extend_from_slice(data);
                std::fs::write(&path, out)?;
                Ok(path)
            }
            group::GroupMsgKind::Image => {
                let dir = home.join("images");
                std::fs::create_dir_all(&dir)?;
                // Slint decodes by extension, so sniff the real format from
                // the magic bytes (PNG/JPEG/WebP) instead of blindly .jpg.
                let ext = if data.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
                    "png"
                } else if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xD8 {
                    "jpg"
                } else if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
                    "webp"
                } else {
                    "jpg"
                };
                let path = dir.join(format!("img-{ts}.{ext}"));
                std::fs::write(&path, data)?;
                Ok(path)
            }
            group::GroupMsgKind::File { name } => {
                let dir = home.join("files");
                std::fs::create_dir_all(&dir)?;
                // Sanitize the sender's basename (path separators / control / Windows-invalid chars) but keep the extension — viewers detect the format by suffix.
                let base = name.rsplit(['/', '\\', '\0']).next().unwrap_or(name).trim();
                let (stem_raw, ext) = match base.rfind('.') {
                    Some(i) if i > 0 => (&base[..i], base[i..].to_string()),
                    _ => (base, String::new()),
                };
                let stem: String = stem_raw
                    .chars()
                    .filter(|c| {
                        !c.is_control() && !matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|')
                    })
                    .collect();
                let stem = if stem.trim().is_empty() {
                    "file".to_string()
                } else {
                    stem
                };
                let path = dir.join(format!("f-{ts}__{stem}{ext}"));
                std::fs::write(&path, data)?;
                Ok(path)
            }
            group::GroupMsgKind::Text => anyhow::bail!("text has no media body"),
        }
    }

    /// Derive a `store::Kind` + local path for a group media body (or `None`
    /// for text). Writes the media file to disk and returns the Kind so the
    /// chat row renders a voice/image bubble instead of text.
    fn group_media_storage(
        &self,
        plain: &group::GroupPlain,
    ) -> anyhow::Result<Option<store::Kind>> {
        let Some(data) = &plain.data else {
            return Ok(None);
        };
        match &plain.kind {
            group::GroupMsgKind::Voice { duration_ms, .. } => {
                let path = self.write_group_media(&plain.kind, data)?;
                Ok(Some(store::Kind::Voice {
                    path: path.to_string_lossy().into_owned(),
                    duration_ms: *duration_ms,
                }))
            }
            group::GroupMsgKind::Image => {
                let path = self.write_group_media(&plain.kind, data)?;
                Ok(Some(store::Kind::Image {
                    path: path.to_string_lossy().into_owned(),
                }))
            }
            group::GroupMsgKind::File { name } => {
                let path = self.write_group_media(&plain.kind, data)?;
                Ok(Some(store::Kind::File {
                    path: path.to_string_lossy().into_owned(),
                    name: name.clone(),
                }))
            }
            group::GroupMsgKind::Text => Ok(None),
        }
    }

    /// Persist one group message row. Best-effort: if we can't open the
    /// envelope (e.g. group key not known yet), we still store it as an opaque
    /// Text envelope so the message is not lost and can be decrypted later
    /// once we hold the group.
    async fn store_group_row(
        &self,
        env: &group::SealedGroupMsg,
        plain: Option<&group::GroupPlain>,
    ) -> anyhow::Result<()> {
        let kind = match plain {
            Some(p) => self.group_media_storage(p)?.unwrap_or(store::Kind::Text),
            None => store::Kind::Text,
        };
        // Media rows (voice/image) keep their body in `media_path` (the on-disk
        // file) — exactly like DMs, whose media rows store an empty `text`.
        // We must NOT put the sealed envelope in `text` for media rows: it
        // embeds the WHOLE original as base64, so a single `store.load(chat)`
        // would pull tens/hundreds of MB of it into memory while the bubble
        // renders from the tiny preview on disk and never reads this column.
        // Text rows keep the sealed envelope in `text` so the chat window can
        // decrypt a plaintext preview.
        let (text_field, sealed) = match &kind {
            store::Kind::Voice { .. } | store::Kind::Image { .. } | store::Kind::File { .. } => {
                (String::new(), false)
            }
            store::Kind::Text => {
                let wire = serde_json::to_string(env)?;
                (crate::account::b64(wire.as_bytes()), true)
            }
        };
        let _ = self
            .store
            .push(&env.group_id, &env.from, &text_field, sealed, kind)
            .await;
        Ok(())
    }

    pub async fn store_group_outbound(&self, env: &group::SealedGroupMsg) -> anyhow::Result<()> {
        let plain = self.open_group_message(&env.group_id, env).ok();
        self.store_group_row(env, plain.as_ref()).await
    }

    pub async fn store_group_inbound(&self, env: &group::SealedGroupMsg) -> anyhow::Result<()> {
        let plain = self.open_group_message(&env.group_id, env).ok();
        self.store_group_row(env, plain.as_ref()).await
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

    /// Return the base58 peer ids of members that are *not* currently
    /// connected (P2P mode). Server mode is always fully reachable via the
    /// directory.
    fn p2p_unreachable_members(&self, group_id: &str) -> Vec<String> {
        let g = match self.get_group(group_id) {
            Some(g) if g.mode != group::GroupMode::Server => g,
            _ => return Vec::new(),
        };
        let me = self.peer_base58();
        let conns = self.running.connected.lock().unwrap();
        g.member_ids()
            .into_iter()
            .filter(|m| m != &me)
            .filter(|m| {
                if let Ok(pid) = m.parse::<PeerId>() {
                    *conns.get(&pid).unwrap_or(&0) == 0
                } else {
                    true
                }
            })
            .collect()
    }

    pub async fn send_group_message(&self, group_id: &str, text: &str) -> anyhow::Result<()> {
        self.send_group_typed(group_id, &group::GroupMsgKind::Text, text, None)
            .await
    }

    /// Send a group voice / image body over the same sealed + broadcast path as
    /// text. `data` is the raw payload (s16le PCM for voice, photo bytes for
    /// image); `kind` must match.
    pub async fn send_group_media(
        &self,
        group_id: &str,
        kind: &group::GroupMsgKind,
        data: Vec<u8>,
    ) -> anyhow::Result<()> {
        self.send_group_typed(group_id, kind, "", Some(data.as_slice()))
            .await
    }

    async fn send_group_typed(
        &self,
        group_id: &str,
        kind: &group::GroupMsgKind,
        text: &str,
        data: Option<&[u8]>,
    ) -> anyhow::Result<()> {
        let bytes = data.map(|d| d.len()).unwrap_or(0);
        tracing::debug!(
            "[group] send_group group={group_id} kind={:?} text_len={} bytes={bytes}",
            kind,
            text.len()
        );
        let env = self.seal_group_message_typed(group_id, kind, text, data)?;
        if self.group_mode(group_id) == group::GroupMode::Server {
            let seq = self.dir.post_group_message(group_id, &env)?;
            tracing::debug!(group_id, seq, "sent group msg via server");
        } else {
            if self.p2p_all_peers_offline(group_id) {
                let _ = self.connect_to_group(group_id);
                tracing::warn!("[group] all members offline for {group_id} — redial attempted");
                anyhow::bail!(
                    "group {group_id}: no other member online — \
                     retry once a peer is reachable"
                );
            }
            let missing: Vec<String> = self.p2p_unreachable_members(group_id);
            if !missing.is_empty() {
                tracing::warn!(
                    "[group] async redial {group_id}: {} member(s) offline {missing:?}",
                    missing.len()
                );
                let _ = self.connect_to_group(group_id);
            }
            let wire = group::msg_wire(&env)?;
            let topic = sw::IdentTopic::new(sw::group_topic_name(group_id));
            self.running
                .cmd_tx
                .send(Cmd::GroupPublish { topic, data: wire })
                .map_err(|e| anyhow::anyhow!("publish group msg: {e}"))?;
        }
        self.store_group_outbound(&env).await?;
        tracing::debug!("[group] send_group_message done group={group_id}");
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
        // Fast-path for offline DM redelivery: the moment a peer's address is
        // re-discovered (mDNS / new relay endpoint), push any pending rows
        // without waiting for the 1.5 s poll tick. Best-effort — the
        // redelivery task remains the backstop if this fails.
        if let sw::ChatEvent::Discovered { peer, .. } = &evt {
            let b58 = peer.to_base58();
            self.redeliver_due(Some(b58.as_str())).await;
        }
        match evt {
            E::Text { peer, req } => {
                let peer_b58 = peer.to_base58();
                match req.kind {
                    message::MsgKind::Dm => {
                        let text = req.text.clone().unwrap_or_default();
                        // Receiver-side idempotency. `req.client_id` is the
                        // sender's stable local id for this logical message,
                        // carried on every redelivery. On first sight we record
                        // it and persist; on a repeat of the same id (a
                        // redelivery that the sender re-sent because its ACK
                        // was lost/timed out) we drop the copy so the user
                        // never sees the same message twice. `None` means the
                        // sender is legacy or it's a control frame (friend
                        // request) — those are never deduplicated here.
                        let chat = store::dm_chat_id(&self.peer_base58(), &peer_b58);
                        if let (Some(cid), Some(pool)) = (req.client_id, self.store.pool()) {
                            let fresh = match sqlite::inbound_seen::mark(&pool, cid as i64, &peer_b58).await {
                                Ok(f) => f,
                                Err(e) => {
                                    // Table query failed — degrade to "persist it"
                                    // rather than silently dropping a real message.
                                    tracing::warn!("[dm] dedup mark failed: {e}; persisting");
                                    true
                                }
                            };
                            if !fresh {
                                tracing::debug!(
                                    client_id = cid,
                                    peer = %peer_b58,
                                    "[dm] duplicate redelivery dropped"
                                );
                                return None;
                            }
                            // Opportunistic retention sweep (cheap, indexed).
                            let _ = sqlite::inbound_seen::prune(&pool).await;
                        }
                        let _ = self
                            .store
                            .push(
                                &chat,
                                &peer_b58,
                                &text,
                                req.sealed.is_some(),
                                store::Kind::Text,
                            )
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
            // `ExternalAddrConfirmed` is surfaced to the application layer (the
            // event pump) so it can immediately re-announce endpoints via
            // `heartbeat()`; the core deliberately does not act on it here
            // since `self` is a bare `&Client`, not the owning `Arc`.
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

fn udp_port_of(addr: &str) -> Option<u16> {
    addr.parse::<Multiaddr>()
        .ok()
        .as_ref()
        .and_then(|a| {
            a.iter().find_map(|c| {
                if let libp2p::multiaddr::Protocol::Udp(p) = c {
                    Some(p)
                } else {
                    None
                }
            })
        })
        .filter(|p| *p != 0)
}

/// Classify a dialable multiaddr into a coarse transport token for log output.
/// Returns:
///   - `"relay-circuit"`  when the address tunnels through a relay
///   - `"direct-quic"`    when it is a direct QUIC/UDP path
///   - `"direct-tcp"`     when it is a direct TCP path
///   - `"other"`          for anything else (e.g. websocket, dns)
fn classify_transport(addr: &Multiaddr) -> &'static str {
    if addr
        .iter()
        .any(|c| matches!(c, libp2p::multiaddr::Protocol::P2pCircuit))
    {
        return "relay-circuit";
    }
    if addr
        .iter()
        .any(|c| matches!(c, libp2p::multiaddr::Protocol::Udp(_)))
    {
        return "direct-quic";
    }
    if addr
        .iter()
        .any(|c| matches!(c, libp2p::multiaddr::Protocol::Tcp(_)))
    {
        return "direct-tcp";
    }
    "other"
}

/// True when a multiaddr is dial-able from *another* network — i.e. it carries
/// a public (non-link-local / non-RFC1918, non-loopback) IPv4, or it is a
/// relay-circuit address (`/p2p-circuit/...`). Private LAN addresses such as
/// `192.168.*`, `10.*`, `172.16-31.*`, `127.*` and `::1` are NOT routable.
fn is_routable_public_or_circuit(addr: &str) -> bool {
    let Ok(m) = addr.parse::<Multiaddr>() else {
        return false;
    };
    let mut has_public_ip = false;
    for c in m.iter() {
        match c {
            libp2p::multiaddr::Protocol::P2pCircuit => return true,
            libp2p::multiaddr::Protocol::Ip4(ip) => {
                if !ip.is_loopback()
                    && !ip.is_unspecified()
                    && !ip.is_private()
                    && !ip.is_link_local()
                {
                    has_public_ip = true;
                }
            }
            libp2p::multiaddr::Protocol::Ip6(ip) => {
                if !ip.is_loopback() && !ip.is_unspecified() && !ip.is_unicast_link_local() {
                    has_public_ip = true;
                }
            }
            _ => {}
        }
    }
    has_public_ip
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
            // Note: we intentionally do NOT drop RFC1918 / link-local IPs here.
            // Peers on the same LAN only share a private `192.168.x` /
            // `10.x` / `172.16-31.x` address with each other — stripping it
            // breaks same-subnet direct connects.
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
