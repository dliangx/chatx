use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::account::{DeviceRecord, DeviceStatus, UserRecord};
use crate::friendq::UserProfile;

/// Shared `ureq::Agent` with hard timeouts. `http_client().request(...)` default has
/// `timeout_read = None` — a socket that connects and then stalls blocks the
/// caller indefinitely. Our presence thread + all directory clients would
/// therefore hang for minutes / forever on a single bad network blip, leaving
/// `seen` stale and the device "offline" (until restart). Capping every
/// network call gives us self-healing within seconds.
fn http_client() -> &'static ureq::Agent {
    static A: std::sync::LazyLock<ureq::Agent> = std::sync::LazyLock::new(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(5))
            .timeout_read(std::time::Duration::from_secs(8))
            .timeout_write(std::time::Duration::from_secs(8))
            .build()
    });
    &A
}

/// Dedicated agent for the wake-notice long-poll. The server holds the
/// connection open for up to ~45 s before timing out, so we need a matching
/// client read timeout — we use 50 s to leave some margin, but cap it below
/// ~60 s so a stuck server can't wedge the pump.
fn poll_http_client() -> &'static ureq::Agent {
    static P: std::sync::LazyLock<ureq::Agent> = std::sync::LazyLock::new(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(5))
            .timeout_read(std::time::Duration::from_secs(50))
            .timeout_write(std::time::Duration::from_secs(8))
            .build()
    });
    &P
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserResolve {
    pub user: UserRecord,
    pub device: DeviceRecord,
}

/// One `post_index` pointer row returned by `GET /v1/posts`: identifies the
/// publisher's account + device + post timestamp, so a client can dial that
/// device over the binary moment channel to fetch the post's content.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PostPointer {
    pub user_id: String,
    pub peer_id: String,
    pub post_ts: i64,
}

/// A relay advertisement returned by `GET /v1/relays`. Clients use this
/// address to reserve a circuit relay slot on the relay.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RelayInfo {
    /// Full multiaddr including `/p2p/<relay_peer_id>` — exactly the value
    /// that used to be put into `CHATX_RELAYS`.
    pub addr: String,
}

/// A built-in STUN server advertisement returned by `GET /v1/stun`. The
/// signaling server can host a STUN binding responder (RFC 5389) so clients on
/// a restricted network — where public STUN providers like `stun.l.google.com`
/// are blocked — still learn their NAT fingerprint (public IP + EIM/EDM) from
/// the operator's own infra.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StunInfo {
    /// `host:port` (e.g. `103.12.34.56:3478`) — the address a client sends a
    /// STUN Binding request to.
    pub addr: String,
}

/// A "peer X wants to reach me" wake notice, delivered by the directory to
/// coordinate simultaneous TCP dials (hole punching). A client long-polls
/// `/v1/notifications` and receives a `[Notice, ...]` payload within ~45 s.
///
/// Semantics:
///   - `peer_id`: target (the long-polling client's own peer id; the sender
///     is identified by `from`).
///   - `from`:  the peer id of the peer that asked the server to wake this
///     target. The target should now dial this peer (via `warm_dial_peer`)
///     as soon as this notice is observed.
///   - `at`: when the sender called the server (ms since epoch).
///   - `kind`:  event kind. "wake" (default) for hole-punch; "fr_inquire"
///     (someone wants to add me), "fr_accept" (my add request was accepted),
///     "fr_reject" (my add request was declined).
///   - `payload`: kind-specific data. `None` for wake. For friend events, a
///     JSON value with `actor_user` (username of the counter-party) and —
///     only on incoming friend-request — `actor_peer` (counter-party's
///     device peer id), `actor_profile` (optional UserProfile).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Notice {
    pub peer_id: String,
    pub from: String,
    pub at: u64,
    #[serde(default)]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
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

    /// Delete a device record from the directory. `Ok(true)` if it existed
    /// and was removed; `Ok(false)` if it wasn't found. Used by the UI's
    /// "Reset" button on a `Revoked` device so the peer can re-register
    /// (next login → `refresh_directory_auto` → root/first-device approval
    /// flow, or a fresh Pending state the other devices can Approve).
    fn delete_device(&self, _peer_id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    fn delete_group_key_slot(&self, _group_id: &str, _peer_id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    /// Moment/post sync: notify the directory that this device published a
    /// post at `post_ts` (so other devices can learn about it later via
    /// `posts_since`).
    fn notify_post(&self, _user_id: &str, _peer_id: &str, _post_ts: i64) -> anyhow::Result<()> {
        anyhow::bail!("directory does not support moment/post notifications")
    }

    /// Moment/post sync: list post pointers in `(since, until]` (newest
    /// first). Used by the sync loop to discover which friends have new
    /// posts to fetch.
    fn posts_since(&self, _since: i64, _until: i64) -> anyhow::Result<Vec<PostPointer>> {
        anyhow::bail!("directory does not support moment/post sync")
    }

    /// Publish this user's display profile ( nickname / bio / research / E2E
    /// public key / 40×40 avatar) so other users can fetch it when adding a
    /// contact. Best-effort: a directory without profile storage simply
    /// ignores it.
    fn publish_profile(&self, _user_id: &str, _profile: &UserProfile) -> anyhow::Result<()> {
        Ok(())
    }

    /// Fetch a user's self-reported display profile from the directory.
    /// Returns `Ok(None)` when the directory has no profile row for the user.
    fn fetch_profile(&self, _user_id: &str) -> anyhow::Result<Option<UserProfile>> {
        Ok(None)
    }

    // --- server-mediated friend requests ------------------------------------
    //
    // The signaling server is the authoritative ledger. A request is
    // registered once (`fr_send`) and the long-poll `poll_notices` fan-out
    // (kind `fr_inquire`) alerts the recipient's other devices. Decisions are
    // pushed back over the same channel with `fr_accept` / `fr_reject`.
    // The `actor_peer` is the device acting (so the recipient can map the
    // event onto one of its local peer-keyed `fr_requests` rows);
    // `actor_user` is the username acting.

    /// Register a pending friend request and fan it out to the recipient's
    /// devices via the long-poll channel. Best-effort: any HTTP failure is
    /// surfaced to the caller (they choose to retry / report to the user).
    fn fr_send(
        &self,
        _requester_user: &str,
        _recipient_user: &str,
        _actor_peer: &str,
        _actor_user: &str,
    ) -> anyhow::Result<()> {
        anyhow::bail!("directory does not support server-mediated friend requests")
    }

    /// Mark the pending request between `actor` and `other` as `accepted`
    /// (`true`) or `rejected` (`false`) on the server side and fan the
    /// decision out to the counterparty's devices.
    fn fr_decide(
        &self,
        _actor_user: &str,
        _other_user: &str,
        _accept: bool,
    ) -> anyhow::Result<()> {
        anyhow::bail!("directory does not support server-mediated friend decisions")
    }

    /// List the recipient's (username-keyed) pending inbound friend requests
    /// known to the server. Used to reconcile the inbox after an offline
    /// period: the local `fr_requests` table is peer-keyed, so this gives the
    /// missing rows. Returns empty when the server does not support it.
    fn fr_inbox_users(&self, _recipient_user: &str) -> Vec<(String, String)> {
        Vec::new()
    }

    fn check(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Discover available circuit relays from the directory. Returns a list of
    /// full multiaddrs of the form `/ip4/host/tcp/port/p2p/<peer_id>`.
    /// Empty when the directory has no relays configured or the endpoint is
    /// not supported (older servers).
    fn list_relays(&self) -> Vec<String> {
        Vec::new()
    }

    /// Discover an operator-hosted STUN server from the directory
    /// (`GET /v1/stun`). Returns `host:port` strings. Empty when the directory
    /// hosts no STUN server or the endpoint is unsupported (older servers) —
    /// callers then fall back to the built-in public STUN list.
    fn list_stun(&self) -> Vec<String> {
        Vec::new()
    }

    /// Ask the directory to wake up `peer_id` immediately (server-side wake
    /// push). Best-effort: returns `true` when the server accepted the notice
    /// and `false` on any failure (older server, network drop, timeout). The
    /// peer will receive it on its own long-poll (`poll_notices`) within one
    /// poll window (server-side ~45 s; the client-side timeout is shorter).
    /// Used as a step in TCP hole-punching to coordinate a simultaneous dial.
    fn notify(&self, _peer_id: &str, _from: &str) -> bool {
        false
    }

    /// Long-poll the directory for a wake notice addressed to this peer.
    /// Returns an empty `Vec` when:
    ///   - the server is an older version that doesn't implement this
    ///   - there is no pending notice and the timeout expires
    ///   - the request otherwise fails
    ///
    /// Returns a `Vec` with one or more entries when a wake arrived.
    ///
    /// The implementation SHOULD cap its own deadline below ~60 s; the
    /// caller (the pump task) runs this in a background std::thread and is
    /// not sensitive to the exact duration.
    fn poll_notices(&self, _peer_id: &str) -> Vec<Notice> {
        Vec::new()
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
        tracing::debug!("resolve_user_and_device: user={:?}", user);
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

    fn delete_device(&self, peer_id: &str) -> anyhow::Result<bool> {
        let removed = self.devices.lock().unwrap().remove(peer_id).is_some();
        Ok(removed)
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
        http_client().request("GET", &url)
            .call()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("signal server unreachable: {e}"))
    }

    /// Notify the directory that `peer_id` (account `user_id`) published a
    /// post at `post_ts`. Fire-and-forget: the publish succeeded locally and
    /// a server-side notification failure must not roll that back.
    pub fn notify_post(&self, user_id: &str, peer_id: &str, post_ts: i64) -> anyhow::Result<()> {
        let url = format!("{}/v1/posts", self.base);
        let body = serde_json::json!({
            "user_id": user_id,
            "peer_id": peer_id,
            "post_ts": post_ts,
        });
        http_client().request("POST", &url)
            .set("content-type", "application/json")
            .send_string(&body.to_string())
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("notify_post failed: {e}"))
    }

    /// All post pointers in `(since, until]`, newest first. The sync loop
    /// uses this to learn which friends have new posts to fetch.
    pub fn posts_since(&self, since: i64, until: i64) -> anyhow::Result<Vec<PostPointer>> {
        let url = format!("{}/v1/posts?since={since}&until={until}", self.base);
        let resp = http_client().request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("posts_since failed: {e}"))?;
        resp.into_json::<Vec<PostPointer>>()
            .map_err(|e| anyhow::anyhow!("bad PostPointer list: {e}"))
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

    fn list_relays(&self) -> Vec<String> {
        let url = format!("{}/v1/relays", self.base);
        match http_client().request("GET", &url).call() {
            Ok(resp) => resp
                .into_json::<Vec<RelayInfo>>()
                .unwrap_or_default()
                .into_iter()
                .map(|r| r.addr)
                .collect(),
            Err(e) => {
                tracing::debug!("[dir] list_relays failed: {e}");
                Vec::new()
            }
        }
    }

    fn list_stun(&self) -> Vec<String> {
        let url = format!("{}/v1/stun", self.base);
        match http_client().request("GET", &url).call() {
            Ok(resp) => resp
                .into_json::<Vec<StunInfo>>()
                .unwrap_or_default()
                .into_iter()
                .map(|s| s.addr)
                .collect(),
            Err(e) => {
                // Older servers without a STUN endpoint return 404 — that is
                // expected and not an error worth surfacing; the client simply
                // keeps its built-in public STUN list.
                tracing::debug!("[dir] list_stun failed or unsupported: {e}");
                Vec::new()
            }
        }
    }

    fn notify(&self, peer_id: &str, from: &str) -> bool {
        let url = format!("{}/v1/notify?peer_id={}", self.base, peer_id);
        let body = format!(r#"{{"from":"{}"}}"#, from);
        match http_client()
            .request("POST", &url)
            .set("content-type", "application/json")
            .send_string(&body)
        {
            Ok(_) => true,
            Err(e) => {
                tracing::debug!("[dir] notify {peer_id} failed: {e}");
                false
            }
        }
    }

    fn poll_notices(&self, peer_id: &str) -> Vec<Notice> {
        let url = format!("{}/v1/notifications?peer_id={}", self.base, peer_id);
        match poll_http_client().request("GET", &url).call() {
            Ok(resp) => match resp.into_json::<Vec<Notice>>() {
                Ok(notices) => notices,
                Err(e) => {
                    tracing::debug!("[dir] poll_notices parse failed: {e}");
                    Vec::new()
                }
            },
            Err(e) => {
                tracing::debug!("[dir] poll_notices failed: {e}");
                Vec::new()
            }
        }
    }

    fn upsert_user(&self, user: &UserRecord) {
        let url = format!("{}/v1/users/{}", self.base, user.user_id);
        let body = serde_json::to_string(user).expect("serialize UserRecord");
        let res = http_client().request("PUT", &url)
            .set("content-type", "application/json")
            .send_string(&body);
        if let Err(e) = res {
            tracing::warn!("directory upsert_user failed: {e}");
        }
    }

    fn upsert_device(&self, rec: &DeviceRecord) {
        let url = format!("{}/v1/devices/{}", self.base, rec.peer_id);
        let body = serde_json::to_string(rec).expect("serialize DeviceRecord");
        let res = http_client().request("PUT", &url)
            .set("content-type", "application/json")
            .send_string(&body);
        if let Err(e) = res {
            tracing::warn!("directory upsert_device failed: {e}");
        }
    }

    fn resolve_device(&self, peer_id: &str) -> anyhow::Result<DeviceRecord> {
        let url = format!("{}/v1/devices/{peer_id}", self.base);
        let resp = http_client().request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("directory: {e}"))?;
        resp.into_json()
            .map_err(|e| anyhow::anyhow!("resolve device failed: {e}"))
    }

    fn delete_device(&self, peer_id: &str) -> anyhow::Result<bool> {
        let url = format!("{}/v1/devices/{peer_id}", self.base);
        match http_client().request("DELETE", &url).call() {
            Ok(_) => Ok(true),
            Err(ureq::Error::Status(code, _)) => {
                // 404 → the record no longer exists; treat as a successful no-op.
                if code == 404 {
                    Ok(false)
                } else {
                    Err(anyhow::anyhow!("delete device: HTTP {code}"))
                }
            }
            Err(e) => Err(anyhow::anyhow!("delete device failed: {e}")),
        }
    }

    fn resolve_user(&self, user_id: &str) -> anyhow::Result<UserRecord> {
        let url = format!("{}/v1/users/{user_id}", self.base);
        let resp = http_client().request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("directory: {e}"))?;
        resp.into_json()
            .map_err(|e| anyhow::anyhow!("resolve user failed: {e}"))
    }

    fn resolve_user_and_device(&self, user_id: &str) -> anyhow::Result<UserResolve> {
        let url = format!("{}/v1/users/{user_id}/resolve", self.base);
        let resp = http_client().request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("directory: {e}"))?;
        resp.into_json()
            .map_err(|e| anyhow::anyhow!("resolve user failed: {e}"))
    }

    fn list_devices(&self, user_id: &str) -> Vec<DeviceRecord> {
        let url = format!("{}/v1/users/{user_id}/devices", self.base);
        match http_client().request("GET", &url).call() {
            Ok(resp) => resp.into_json::<Vec<DeviceRecord>>().unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn list_users(&self, exclude: &str) -> Vec<UserResolve> {
        let mut url = format!("{}/v1/users", self.base);
        if !exclude.is_empty() {
            url.push_str(&format!("?exclude={}", exclude));
        }
        match http_client().request("GET", &url).call() {
            Ok(resp) => resp.into_json::<Vec<UserResolve>>().unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn list_pending(&self, user_id: &str) -> Vec<DeviceRecord> {
        let url = format!("{}/v1/users/{user_id}/devices?status=pending", self.base);
        match http_client().request("GET", &url).call() {
            Ok(resp) => resp.into_json::<Vec<DeviceRecord>>().unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    fn publish_profile(&self, user_id: &str, profile: &UserProfile) -> anyhow::Result<()> {
        let url = format!("{}/v1/users/{user_id}/profile", self.base);
        let body = serde_json::to_string(profile).expect("serialize UserProfile");
        http_client().request("PUT", &url)
            .set("content-type", "application/json")
            .send_string(&body)
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("publish profile failed: {e}"))
    }

    fn fetch_profile(&self, user_id: &str) -> anyhow::Result<Option<UserProfile>> {
        let url = format!("{}/v1/users/{user_id}/profile", self.base);
        match http_client().request("GET", &url).call() {
            Ok(resp) => resp
                .into_json::<UserProfile>()
                .map(Some)
                .map_err(|e| anyhow::anyhow!("fetch profile failed: {e}")),
            Err(ureq::Error::Status(code, _)) if code == 404 => Ok(None),
            Err(e) => Err(anyhow::anyhow!("fetch profile failed: {e}")),
        }
    }

    fn fr_send(
        &self,
        requester_user: &str,
        recipient_user: &str,
        actor_peer: &str,
        actor_user: &str,
    ) -> anyhow::Result<()> {
        let url = format!("{}/v1/friend-requests", self.base);
        let body = serde_json::json!({
            "requester": requester_user,
            "recipient": recipient_user,
            "actor_peer": actor_peer,
            "actor_user": actor_user,
        })
        .to_string();
        http_client().request("POST", &url)
            .set("content-type", "application/json")
            .send_string(&body)
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("fr_send failed: {e}"))
    }

    fn fr_decide(&self, actor_user: &str, other_user: &str, accept: bool) -> anyhow::Result<()> {
        let action = if accept { "accept" } else { "reject" };
        let url = format!(
            "{}/v1/friend-requests/{actor_user}/{other_user}/{action}",
            self.base
        );
        http_client().request("PUT", &url).send_string("null")
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("fr_decide failed: {e}"))
    }

    fn upsert_group(&self, g: &crate::group::GroupPublic) {
        let url = format!("{}/v1/groups/{}", self.base, g.group_id);
        let body = serde_json::to_string(g).expect("serialize GroupPublic");
        let res = http_client().request("PUT", &url)
            .set("content-type", "application/json")
            .send_string(&body);
        if let Err(e) = res {
            tracing::warn!("directory upsert_group failed: {e}");
        }
    }

    fn resolve_group(&self, group_id: &str) -> anyhow::Result<crate::group::GroupPublic> {
        let url = format!("{}/v1/groups/{group_id}", self.base);
        let resp = http_client().request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("directory: {e}"))?;
        resp.into_json()
            .map_err(|e| anyhow::anyhow!("resolve group failed: {e}"))
    }

    fn list_groups(&self) -> Vec<crate::group::GroupPublic> {
        let url = format!("{}/v1/groups", self.base);
        match http_client().request("GET", &url).call() {
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
        let resp = http_client().request("POST", &url)
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
        match http_client().request("GET", &url).call() {
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
        let res = http_client().request("PUT", &url)
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
        let resp = http_client().request("GET", &url)
            .call()
            .map_err(|e| anyhow::anyhow!("get group key slot: {e}"))?;
        let v: (String, String) = resp
            .into_json()
            .map_err(|e| anyhow::anyhow!("bad key slot response: {e}"))?;
        Ok(v)
    }

    fn delete_group_key_slot(&self, group_id: &str, peer_id: &str) -> anyhow::Result<bool> {
        let url = format!("{}/v1/groups/{group_id}/keys/{peer_id}", self.base);
        match http_client().request("DELETE", &url).call() {
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
        match http_client().request("PUT", &url)
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
