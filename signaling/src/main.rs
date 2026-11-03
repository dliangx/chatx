use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use chatx_core::account::{AttestationAction, DeviceRecord, DeviceStatus, UserRecord};
use chatx_core::friendq::UserProfile;
use chatx_core::group::GroupPublic;
use chatx_core::message::now_ms;
use chatx_core::signal::UserResolve;
use serde::{Deserialize, Serialize};

mod db;
mod relay;

/// A single "peer X wants to reach me" wake notice. Carried by the
/// `/v1/notify` + `/v1/notifications` long-poll pair so two cross-NAT peers
/// can coordinate a simultaneous dial attempt over TCP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Notice {
    pub(crate) peer_id: String,
    pub(crate) from: String,
    pub(crate) at: u64,
}

/// Server-side broadcast of wake notices. One `Notice` (target-peer, sender,
/// timestamp) is pushed by `POST /v1/notify`; a `GET /v1/notifications`
/// long-poll handler subscribes, waits up to ~45 s, and drains every notice
/// for the caller's own peer id in arrival order. Uses `tokio::sync::broadcast`
/// so every subscriber sees the same stream (broadcast is fine for the wake
/// case — duplicates across subscribers are harmless, and the cap is just a
/// memory ceiling).
#[derive(Debug, Clone)]
pub(crate) struct Notifies {
    sender: tokio::sync::broadcast::Sender<Notice>,
    poll_timeout: std::time::Duration,
}

impl Notifies {
    pub(crate) fn new(poll_timeout: std::time::Duration) -> Self {
        let (sender, _rx) = tokio::sync::broadcast::channel(256);
        Self { sender, poll_timeout }
    }
    pub(crate) fn push(&self, notice: Notice) {
        // `send` returns a "no receivers" error if nobody is long-polling —
        // expected on startup; ignore (log at debug in the handler instead).
        let _ = self.sender.send(notice);
    }
    /// Wait (long-poll) up to `poll_timeout` for notices addressed to
    /// `peer_id`. Returns on the first matching notice, or after the timeout
    /// with `[]`. Extra buffered notices for other peers are drained to
    /// `continue()` — they belong to other subscribers.
    pub(crate) async fn wait_for(&self, peer_id: &str) -> Vec<Notice> {
        let mut rx = self.sender.subscribe();
        let deadline = tokio::time::Instant::now() + self.poll_timeout;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Vec::new();
            }
            let remaining = deadline - now;
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(n)) if n.peer_id == peer_id => return vec![n],
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => return Vec::new(),
                Err(_) => return Vec::new(),
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct App {
    db: Arc<db::Sdb>,
    /// Advertised circuit-relay multiaddrs (populated by the relay task once it
    /// is listening). Served via `GET /v1/relays` so clients can discover a
    /// relay without operator-provided `CHATX_RELAYS`.
    relays: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Wake-notice fan-out for TCP hole-punching. A client that wants to reach
    /// B calls `POST /v1/notify?peer_id=<B>`; B's long-poll
    /// (`GET /v1/notifications?peer_id=<B>`) wakes up within seconds, and both
    /// sides now simultaneously dial each other — which opens both NAT mappings
    /// in the same window.
    notifies: Notifies,
}

#[derive(Deserialize)]
struct IdQuery {
    #[serde(default)]
    exclude: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize)]
struct NotifyQuery {
    #[serde(default)]
    peer_id: String,
}

type ApiResult = Result<Response, (StatusCode, String)>;

fn bad(code: StatusCode, msg: impl std::fmt::Display) -> (StatusCode, String) {
    (code, msg.to_string())
}

fn ok_json<T: serde::Serialize + ?Sized>(v: &T) -> Response {
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        serde_json::to_string(v).unwrap_or("null".into()),
    )
        .into_response()
}

async fn upsert_user(
    State(App { db, .. }): State<App>,
    Path(user_id): Path<String>,
    body: String,
) -> ApiResult {
    let mut rec: UserRecord = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return Err(bad(StatusCode::BAD_REQUEST, format!("bad UserRecord: {e}"))),
    };
    rec.user_id = user_id.clone();
    rec.seen = now_ms();
    if let Err(e) = db::upsert_user(db.as_ref(), &rec).await {
        tracing::warn!(%e, "persisting user failed");
        return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "db error"));
    }
    Ok(ok_json(&"ok"))
}

async fn resolve_user(
    State(App { db, .. }): State<App>,
    Path(user_id): Path<String>,
) -> ApiResult {
    match db::resolve_user_db(db.as_ref(), &user_id).await {
        Ok(r) => Ok(ok_json(&r)),
        Err(_) => Err(bad(
            StatusCode::NOT_FOUND,
            format!("user {user_id} not found or ambiguous"),
        )),
    }
}

async fn fetch_user_profile(
    State(App { db, .. }): State<App>,
    Path(user_id): Path<String>,
) -> ApiResult {
    match db::fetch_profile_db(db.as_ref(), &user_id).await {
        Ok(Some(p)) => Ok(ok_json(&p)),
        Ok(None) => Err(bad(
            StatusCode::NOT_FOUND,
            format!("user {user_id} has no profile"),
        )),
        Err(e) => {
            tracing::warn!(%e, "fetch profile failed");
            Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "db error"))
        }
    }
}

async fn upsert_user_profile(
    State(App { db, .. }): State<App>,
    Path(user_id): Path<String>,
    body: String,
) -> ApiResult {
    let p: UserProfile = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => return Err(bad(StatusCode::BAD_REQUEST, format!("bad UserProfile: {e}"))),
    };
    match db::publish_profile_db(db.as_ref(), &user_id, &p).await {
        Ok(()) => Ok(ok_json(&"ok")),
        Err(e) => {
            tracing::warn!(%e, "publish profile failed");
            Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "db error"))
        }
    }
}

async fn resolve_and_device(
    State(App { db, .. }): State<App>,
    Path(user_id): Path<String>,
) -> ApiResult {
    let user = match db::resolve_user_db(db.as_ref(), &user_id).await {
        Ok(u) => u,
        Err(_) => {
            return Err(bad(
                StatusCode::NOT_FOUND,
                format!("user {user_id} not found"),
            ))
        }
    };
    let Some(device) = db::best_online_for_user(db.as_ref(), &user.user_id, "", now_ms())
        .await
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    else {
        return Err(bad(StatusCode::NOT_FOUND, "no online APPROVED device"));
    };
    let out = UserResolve { user, device };
    Ok(ok_json(&out))
}

async fn list_user_devices(
    State(App { db, .. }): State<App>,
    Path(user_id): Path<String>,
    Query(q): Query<IdQuery>,
) -> ApiResult {
    let mut out: Vec<DeviceRecord> =
        db::list_user_devices_db(db.as_ref(), &user_id)
            .await
            .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let Some(status) = q.status {
        let want = match status.as_str() {
            "pending" => DeviceStatus::Pending,
            "approved" => DeviceStatus::Approved,
            "revoked" => DeviceStatus::Revoked,
            other => return Err(bad(StatusCode::BAD_REQUEST, format!("bad status {other}"))),
        };
        out.retain(|d| d.status == want);
    }
    out.sort_by_key(|d| d.peer_id.clone());
    Ok(ok_json(&out))
}

async fn list_users(
    State(App { db, .. }): State<App>,
    Query(q): Query<IdQuery>,
) -> ApiResult {
    let users =
        db::list_users(db.as_ref())
            .await
            .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let exclude = q.exclude.as_deref().unwrap_or_default();
    let now = now_ms();
    let mut out: Vec<UserResolve> = Vec::new();
    for user in users {
        if let Some(device) = db::best_online_for_user(db.as_ref(), &user.user_id, exclude, now)
            .await
            .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        {
            out.push(UserResolve {
                user: user.clone(),
                device,
            });
        }
    }
    Ok(ok_json(&out))
}

/// True if `approver` is an APPROVED device of the same account (`user_id`),
/// distinct from the target device. An account can only approve/attest via
/// one of its already-trusted devices.
async fn approver_is_trusted(
    db: &db::Sdb,
    user_id: &str,
    approver: &str,
    self_peer: &str,
) -> Result<bool, String> {
    db::approver_is_trusted(db, user_id, approver, self_peer)
        .await
        .map_err(|e| e.to_string())
}

async fn validate_device_update(
    db: &db::Sdb,
    existing: Option<&DeviceRecord>,
    incoming: &DeviceRecord,
) -> Result<(), String> {
    let Some(prev) = existing else {
        match incoming.status {
            DeviceStatus::Pending => return Ok(()),
            DeviceStatus::Approved => {
                let Some(att) = incoming.attestation.as_ref() else {
                    return Err("new APPROVED device requires an attestation".into());
                };
                if att.action != AttestationAction::Approve {
                    return Err("root attestation must be an approval".into());
                }
                if att.device != incoming.peer_id {
                    return Err("root attestation device mismatch".into());
                }
                if att.approver != incoming.peer_id {
                    return Err("root device must self-approve (approver == device)".into());
                }
                if !att.is_valid() {
                    return Err("root attestation signature is invalid".into());
                }
                return Ok(());
            }
            DeviceStatus::Revoked => return Err("cannot revoke an unregistered device".into()),
        }
    };

    if incoming.user_id != prev.user_id {
        return Err("account mismatch: device belongs to a different user".into());
    }

    if incoming.status == prev.status {
        return Ok(());
    }

    let is_upgrade_or_revoke = matches!(
        (incoming.status, prev.status),
        (DeviceStatus::Approved, DeviceStatus::Pending)
            | (DeviceStatus::Revoked, DeviceStatus::Approved)
            | (DeviceStatus::Revoked, DeviceStatus::Revoked)
    );

    if is_upgrade_or_revoke {
        let Some(att) = incoming.attestation.as_ref() else {
            return Err(format!(
                "status change to {:?} requires an attestation",
                incoming.status
            ));
        };
        if !att.is_valid() {
            return Err("attestation signature is invalid".into());
        }
        if att.device != incoming.peer_id {
            return Err("attestation is for a different device".into());
        }
        if att.user_id != incoming.user_id {
            return Err("attestation user mismatch".into());
        }
        let expected = match incoming.status {
            DeviceStatus::Approved => AttestationAction::Approve,
            _ => AttestationAction::Revoke,
        };
        if att.action != expected {
            return Err(format!(
                "attestation action mismatch: got {:?}, expected {:?} (status {:?})",
                att.action, expected, incoming.status
            ));
        }
        if !approver_is_trusted(db, &incoming.user_id, &att.approver, &incoming.peer_id)
            .await?
        {
            return Err("approver is not an approved device of this account".into());
        }
        return Ok(());
    }

    if incoming.status == DeviceStatus::Pending && prev.status == DeviceStatus::Approved {
        return Err("cannot demote an APPROVED device to Pending".into());
    }
    if incoming.status == DeviceStatus::Pending && prev.status == DeviceStatus::Revoked {
        return Err("cannot move a REVOKED device to Pending".into());
    }

    Ok(())
}

async fn upsert_device(
    State(App { db, .. }): State<App>,
    Path(peer_id): Path<String>,
    body: String,
) -> ApiResult {
    let mut rec: DeviceRecord = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return Err(bad(
                StatusCode::BAD_REQUEST,
                format!("bad DeviceRecord: {e}"),
            ));
        }
    };
    rec.peer_id = peer_id.clone();
    rec.seen = now_ms();
    tracing::info!(peer_id = %rec.peer_id, user = %rec.user_id, status = ?rec.status, endpoints = ?rec.endpoints, "device upsert");

    let existing = db::get_device_exact(db.as_ref(), &rec.peer_id)
        .await
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match validate_device_update(db.as_ref(), existing.as_ref(), &rec).await {
        Ok(()) => {}
        Err(e) => {
            tracing::warn!(peer_id = %rec.peer_id, status = ?rec.status, %e, "device upsert rejected");
            return Err(bad(StatusCode::FORBIDDEN, e));
        }
    }

    if let Err(e) = db::upsert_device(db.as_ref(), &rec).await {
        tracing::warn!(%e, "persisting device failed");
        return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "db error"));
    }
    Ok(ok_json(&"ok"))
}

async fn resolve_device(
    State(App { db, .. }): State<App>,
    Path(peer_id): Path<String>,
) -> ApiResult {
    match db::resolve_device_db(db.as_ref(), &peer_id).await {
        Ok(r) => Ok(ok_json(&r)),
        Err(_) => Err(bad(
            StatusCode::NOT_FOUND,
            format!("device {peer_id} not found or ambiguous"),
        )),
    }
}

async fn delete_device(
    State(App { db, .. }): State<App>,
    Path(peer_id): Path<String>,
) -> ApiResult {
    match db::delete_device(db.as_ref(), &peer_id).await {
        Ok(true) => Ok(StatusCode::NO_CONTENT.into_response()),
        Ok(false) => Err(bad(StatusCode::NOT_FOUND, "not found")),
        Err(e) => {
            tracing::warn!(%e, "deleting device failed");
            Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "db error"))
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Presence {
    seen: Option<u64>,
    endpoints: Option<Vec<String>>,
}

async fn device_presence(
    State(App { db, .. }): State<App>,
    Path(peer_id): Path<String>,
    body: String,
) -> ApiResult {
    let body: Presence = match serde_json::from_str(&body) {
        Ok(b) => b,
        Err(e) => return Err(bad(StatusCode::BAD_REQUEST, format!("bad Presence: {e}"))),
    };
    tracing::info!(peer_id = %peer_id, endpoints = ?body.endpoints, seen = ?body.seen, "device presence");
    let updated = db::update_device_presence(db.as_ref(), &peer_id, body.seen.unwrap_or_else(now_ms), body.endpoints)
        .await
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !updated {
        return Err(bad(
            StatusCode::NOT_FOUND,
            format!("device {peer_id} not found"),
        ));
    }
    Ok(ok_json(&"ok"))
}

async fn health() -> ApiResult {
    Ok(ok_json(&"ok"))
}

/// List the advertised relay addresses so a client can discover a NAT
/// traversal relay without an operator-supplied `CHATX_RELAYS`. Each entry is a
/// full multiaddr of the form `/ip4/<host>/tcp/<port>/p2p/<relay_peer_id>`.
async fn list_relays(State(app): State<App>) -> ApiResult {
    let relays = app.relays.lock().await.clone();
    let out: Vec<chatx_core::signal::RelayInfo> =
        relays.into_iter().map(|addr| chatx_core::signal::RelayInfo { addr }).collect();
    Ok(ok_json(&out))
}

/// `POST /v1/notify?peer_id=<target_peer_id>` — body: `{"from": "<my_peer>"}`.
/// Ask the server to wake up `target_peer_id` immediately. Used as the first
/// step of TCP hole-punching: A can't reach B through direct dials, so A asks
/// the directory to poke B, who will then dial A back on top of A's own
/// outbound attempt — opening both NAT mappings at the same instant.
///
/// The server does not need to validate `from` here: this is a best-effort
/// wake, not an authenticated channel, and the only action it triggers is
/// making a peer try to dial another peer it is already allowed to talk to.
async fn notify_peer(
    State(app): State<App>,
    Query(q): Query<NotifyQuery>,
    body: String,
) -> ApiResult {
    #[derive(Deserialize)]
    struct Body {
        #[serde(default)]
        from: String,
    }
    let Body { from } = serde_json::from_str(&body)
        .map_err(|e| bad(StatusCode::BAD_REQUEST, format!("bad body: {e}")))?;
    if q.peer_id.is_empty() {
        return Err(bad(StatusCode::BAD_REQUEST, "peer_id query param required"));
    }
    if from == q.peer_id {
        // A self-notification ("wake me up") is a no-op that could also be
        // used as a cheap probe; reject it.
        return Err(bad(
            StatusCode::BAD_REQUEST,
            "from must be a different peer_id",
        ));
    }
    app.notifies.push(Notice {
        peer_id: q.peer_id.clone(),
        from,
        at: now_ms(),
    });
    Ok(ok_json(&"ok"))
}

/// `GET /v1/notifications?peer_id=<my_peer>` — long-poll the server for
/// wake notices addressed to this peer. Returns within ~45 s with either
/// `[]` (timeout, no wakes) or `[Notice, ...]` (wakes; the server drains
/// the full burst before replying).
async fn poll_notices(
    State(app): State<App>,
    Query(q): Query<NotifyQuery>,
) -> ApiResult {
    if q.peer_id.is_empty() {
        return Err(bad(StatusCode::BAD_REQUEST, "peer_id query param required"));
    }
    let out = app.notifies.wait_for(&q.peer_id).await;
    Ok(ok_json(&out))
}

async fn upsert_group(
    State(App { db, .. }): State<App>,
    Path(group_id): Path<String>,
    body: String,
) -> ApiResult {
    let mut g: GroupPublic = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return Err(bad(
                StatusCode::BAD_REQUEST,
                format!("bad GroupPublic: {e}"),
            ));
        }
    };
    g.group_id = group_id.clone();
    if let Err(e) = db::upsert_group(db.as_ref(), &g).await {
        tracing::warn!(%e, "persisting group failed");
        return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "db error"));
    }
    Ok(ok_json(&"ok"))
}

async fn resolve_group(
    State(App { db, .. }): State<App>,
    Path(group_id): Path<String>,
) -> ApiResult {
    let group = db::get_group_db(db.as_ref(), &group_id)
        .await
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match group {
        Some(g) => Ok(ok_json(&g)),
        None => Err(bad(
            StatusCode::NOT_FOUND,
            format!("group {group_id} not found"),
        )),
    }
}

async fn list_groups(State(App { db, .. }): State<App>) -> ApiResult {
    let mut out: Vec<GroupPublic> =
        db::list_groups_db(db.as_ref())
            .await
            .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    out.sort_by_key(|g| g.group_id.clone());
    Ok(ok_json(&out))
}

#[derive(serde::Deserialize)]
struct GroupMsgBody {
    from: String,
    wire: String,
}

async fn post_group_message(
    State(App { db, .. }): State<App>,
    Path(group_id): Path<String>,
    body: String,
) -> ApiResult {
    let b: GroupMsgBody = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return Err(bad(StatusCode::BAD_REQUEST, format!("bad body: {e}"))),
    };

    let group = db::get_group_db(db.as_ref(), &group_id)
        .await
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let is_member = group
        .as_ref()
        .map(|g| g.owner == b.from || g.members.values().any(|m| m.peer_id == b.from))
        .unwrap_or(false);
    if !is_member {
        return Err(bad(
            StatusCode::FORBIDDEN,
            format!(
                "sender {from} is not a member of group {group_id}",
                from = b.from
            ),
        ));
    }
    let seq = match db::append_group_message(
        db.as_ref(),
        &group_id,
        &b.from,
        chatx_core::message::now_ms(),
        &b.wire,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%e, "appending group message failed");
            return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "db error"));
        }
    };
    tracing::debug!(%group_id, seq, from = %b.from, "group message appended");
    #[derive(serde::Serialize)]
    struct Out {
        seq: u64,
    }
    Ok(ok_json(&Out { seq }))
}

async fn poll_group_messages(
    State(App { db, .. }): State<App>,
    Path(group_id): Path<String>,
    Query(q): Query<MsgQuery>,
) -> ApiResult {
    match db::list_group_messages(db.as_ref(), &group_id, q.after).await {
        Ok(out) => Ok(ok_json(&out)),
        Err(e) => {
            tracing::warn!(%e, "listing group messages failed");
            Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "db error"))
        }
    }
}

#[derive(serde::Deserialize)]
struct MsgQuery {
    #[serde(default)]
    after: u64,
}

#[derive(serde::Deserialize)]
struct KeySlotBody {
    sender_e2e: String,
    sealed: String,
}

async fn put_group_key_slot(
    State(App { db, .. }): State<App>,
    Path((group_id, peer_id)): Path<(String, String)>,
    body: String,
) -> ApiResult {
    let owner = db::get_group_db(db.as_ref(), &group_id)
        .await
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(|g| g.owner)
        .ok_or_else(|| bad(
            StatusCode::NOT_FOUND,
            format!("group {group_id} not found"),
        ))?;
    let b: KeySlotBody = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return Err(bad(StatusCode::BAD_REQUEST, format!("bad body: {e}"))),
    };
    if let Err(e) = db::upsert_group_key_slot(
        db.as_ref(),
        &group_id,
        &owner,
        &peer_id,
        &b.sender_e2e,
        &b.sealed,
    )
    .await
    {
        tracing::warn!(%e, "storing group key slot failed");
        return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "db error"));
    }
    Ok(ok_json(&"ok"))
}

async fn get_group_key_slot(
    State(App { db, .. }): State<App>,
    Path((group_id, peer_id)): Path<(String, String)>,
) -> ApiResult {
    let (_owner, slots) = match db::load_group_key_slots(db.as_ref(), &group_id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(%e, "loading group key slots failed");
            return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "db error"));
        }
    };
    match slots.get(&peer_id).cloned() {
        Some(v) => Ok(ok_json(&v)),
        None => Err(bad(
            StatusCode::NOT_FOUND,
            format!("no key slot for {peer_id}"),
        )),
    }
}

async fn delete_group_key_slot(
    State(App { db, .. }): State<App>,
    Path((group_id, peer_id)): Path<(String, String)>,
) -> ApiResult {
    let removed = match db::delete_group_key_slot(db.as_ref(), &group_id, &peer_id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(%e, "deleting group key slot failed");
            return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "db error"));
        }
    };
    if removed {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(bad(
            StatusCode::NOT_FOUND,
            format!("no key slot for {peer_id}"),
        ))
    }
}

/// Record a moment/post publish from a client (fire-and-forget).
#[derive(Deserialize)]
struct NotifyPostBody {
    user_id: String,
    peer_id: String,
    post_ts: i64,
}

async fn notify_post(
    State(App { db, .. }): State<App>,
    body: String,
) -> ApiResult {
    let body: NotifyPostBody =
        serde_json::from_str(&body).map_err(|e| bad(StatusCode::BAD_REQUEST, e.to_string()))?;
    if let Err(e) =
        db::notify_post(db.as_ref(), &body.user_id, &body.peer_id, body.post_ts).await
    {
        tracing::warn!(%e, "persisting notify_post failed");
        return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "db error"));
    }
    Ok(ok_json(&"ok"))
}

/// Query `(user_id, peer_id, post_ts)` pointers in `(since, until]`, newest
/// first. The sync loop on a client uses this to learn "are any of my
/// friends publishing a new post?" and then dials their devices over the
/// binary moment channel to fetch the actual content.
#[derive(Deserialize)]
struct PostsSinceQuery {
    #[serde(default)]
    since: i64,
    #[serde(default)]
    until: i64,
}

#[derive(Serialize)]
struct PostPointer {
    user_id: String,
    peer_id: String,
    post_ts: i64,
}

async fn list_posts(
    State(App { db, .. }): State<App>,
    Query(q): Query<PostsSinceQuery>,
) -> ApiResult {
    let out = db::posts_since(db.as_ref(), q.since, q.until)
        .await
        .map_err(|e| bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .into_iter()
        .map(|(user_id, peer_id, post_ts)| PostPointer {
            user_id,
            peer_id,
            post_ts,
        })
        .collect::<Vec<_>>();
    Ok(ok_json(&out))
}

pub(crate) fn make_router(app: App) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/relays", get(list_relays))
        .route("/v1/notify", post(notify_peer))
        .route("/v1/notifications", get(poll_notices))
        .route("/v1/users", get(list_users))
        .route("/v1/users/{user_id}", put(upsert_user).get(resolve_user))
        .route(
            "/v1/users/{user_id}/profile",
            put(upsert_user_profile).get(fetch_user_profile),
        )
        .route("/v1/users/{user_id}/resolve", get(resolve_and_device))
        .route("/v1/users/{user_id}/devices", get(list_user_devices))
        .route(
            "/v1/devices/{peer_id}",
            put(upsert_device).get(resolve_device).delete(delete_device),
        )
        .route("/v1/devices/{peer_id}/presence", put(device_presence))
        .route("/v1/groups", get(list_groups))
        .route(
            "/v1/groups/{group_id}",
            put(upsert_group).get(resolve_group),
        )
        .route(
            "/v1/groups/{group_id}/messages",
            post(post_group_message).get(poll_group_messages),
        )
        .route(
            "/v1/groups/{group_id}/keys/{peer_id}",
            put(put_group_key_slot)
                .get(get_group_key_slot)
                .delete(delete_group_key_slot),
        )
        .route("/v1/posts", post(notify_post).get(list_posts))
        .with_state(app)
}

/// Signal-server state lives under `~/.chatx-signal/` (log + sqlite),
/// separate from the client's `~/.chatx/`.
fn signal_home() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let dir = home.join(".chatx-signal");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[tokio::main]
async fn main() {
    let home = signal_home();
    let log_path = home.join("chatx-signal.log");

    let _ = chatx_core::logging::init_at(&log_path);
    eprintln!("chatx-signal log file: {}", log_path.display());

    let addr: SocketAddr = std::env::var("CHATX_SIGNAL_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".into())
        .parse()
        .expect("CHATX_SIGNAL_ADDR must be a socket address");

    let db_path = home.join("signal.db");
    let db = Arc::new(db::open(&db_path).await.expect("open sqlite db"));
    tracing::info!(path = %db_path.display(), "signaling db ready");

    let (users, devices, groups) = db::load_all(db.as_ref()).await.expect("load directory");
    tracing::info!(
        users = %users.len(),
        devices = %devices.len(),
        groups = %groups.len(),
        "directory restored"
    );

    let relays = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    // Long-poll waits up to 45 s: long enough that the ~15 s client presence
    // tick comfortably fits, short enough that a server restart or peer churn
    // doesn't strand a connection for a full minute.
    let notifies = Notifies::new(std::time::Duration::from_secs(45));
    let app = App {
        db,
        relays: relays.clone(),
        notifies,
    };

    let router = make_router(app.clone());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind TCP listener");
    tracing::info!(%addr, "chatx-signal listening");

    // Built-in circuit-relay v2 server (enable with a CHATX_RELAY_ADDR value,
    // disable with `CHATX_RELAY_ADDR=off`). Defaults to 0.0.0.0:1883. Once the
    // relay is listening it publishes its advertise multiaddr into `relays` so
    // clients can discover it via `GET /v1/relays`.
    let relay_spec = std::env::var("CHATX_RELAY_ADDR").unwrap_or_else(|_| "0.0.0.0:1883".into());
    if relay_spec.to_ascii_lowercase() != "off" {
        let relay_addr: SocketAddr = relay_spec
            .parse()
            .expect("CHATX_RELAY_ADDR must be host:port or `off`");
        let relays = relays.clone();
        tokio::spawn(async move {
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<String>();
            let serve = tokio::spawn(relay::start(relay_addr, ready_tx));
            // Await the ready notification *and* run the serve task; publish the
            // advertise address for the clients' `GET /v1/relays`.
            let advertise = ready_rx.await;
            match advertise {
                Ok(addr) => {
                    relays.lock().await.push(addr.clone());
                    tracing::info!(%addr, "relay advertised via GET /v1/relays");
                }
                Err(e) => tracing::warn!(%e, "relay ready channel closed before announcing"),
            }
            if let Err(e) = serve.await {
                tracing::error!(%e, "relay exited");
            }
        });
    } else {
        tracing::info!("circuit-relay disabled");
    }

    axum::serve(listener, router).await.expect("serve");
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use chatx_core::group::GroupMode;

    /// Start the real axum router (with a real SQLite db) on a background
    /// thread with its own tokio runtime, and hand back the base URL. The
    /// server lives as long as the guard (dropped when the test ends).
    struct Server {
        base: String,
        _thread: std::thread::JoinHandle<()>,
        _db: tempfile::TempDir,
    }

    impl Server {
        fn start() -> Self {
            let dir = tempfile::tempdir().unwrap();
            // Bind the listener OUTSIDE the server thread so we know the port
            // before spawning it, and can report the address back.
            let (tx, rx) = std::sync::mpsc::channel::<std::net::SocketAddr>();
            let path = dir.path().join("signal.db").to_path_buf();

            let thread = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let db = Arc::new(db::open(&path).await.unwrap());
                    let app = App {
                        db: db.clone(),
                        relays: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                        // Short poll (1 s) so the long-poll test doesn't block.
                        notifies: Notifies::new(std::time::Duration::from_secs(1)),
                    };
                    // Seed one server-mode group: owner "owner" + member "member-a".
                    let mut members = std::collections::BTreeMap::new();
                    for (pid, sign, e2e) in
                        [("owner", "S-OWNER", "E-OWNER"), ("member-a", "S-A", "E-A")]
                    {
                        members.insert(
                            pid.to_string(),
                            chatx_core::group::MemberInfo {
                                peer_id: pid.into(),
                                sign_pk: sign.into(),
                                e2e_public: e2e.into(),
                            },
                        );
                    }
                    let grp = GroupPublic {
                        group_id: "grp-it".into(),
                        owner: "owner".into(),
                        members,
                        created_at: 1,
                        mode: GroupMode::Server,
                    };
                    db::upsert_group(app.db.as_ref(), &grp).await.unwrap();

                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let addr = listener.local_addr().unwrap();
                    tx.send(addr).unwrap();
                    axum::serve(listener, make_router(app)).await.unwrap();
                });
            });

            let addr = rx.recv().unwrap();
            Server {
                base: format!("http://{addr}"),
                _thread: thread,
                _db: dir,
            }
        }
    }

    #[test]
    fn relays_endpoint_returns_empty_list() {
        use chatx_core::signal::{DirectoryClient, HttpDirectory};
        let server = Server::start();
        let dir = HttpDirectory::new(server.base.clone());

        // The test server starts without a relay task, so `GET /v1/relays`
        // must answer with an empty JSON list (not an error).
        assert!(dir.list_relays().is_empty(), "server with no relay lists none");
    }

    /// Round-trip a wake notice: A asks to wake B; B's long-poll receives it
    /// (with the short 1 s poll timeout configured on the test `App`).
    /// A concurrent long-poll that times out must still respond (with `[]`),
    /// not hang forever.
    #[test]
    fn wake_notice_roundtrip() {
        use chatx_core::signal::{DirectoryClient, HttpDirectory, Notice};
        let server = Server::start();
        let dir = HttpDirectory::new(server.base.clone());

        // B starts a long-poll for "peer-B" in a background thread.
        let poll_dir = HttpDirectory::new(server.base.clone());
        let join = std::thread::spawn(move || {
            poll_dir.poll_notices("peer-B")
        });

        // Give B a moment to be subscribed, then A posts a wake.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let wake_dir = HttpDirectory::new(server.base.clone());
        assert!(
            wake_dir.notify("peer-B", "peer-A"),
            "notify must return true for an accepted wake"
        );

        // B should receive the notice within the test's 1 s window.
        let notices = join.join().expect("poll thread panicked");
        assert_eq!(notices.len(), 1, "B should see exactly one wake");
        assert_eq!(notices[0].peer_id, "peer-B");
        assert_eq!(notices[0].from, "peer-A");
        assert!(notices[0].at > 0);
    }

    /// A peer polling for notices it was never sent must time out (empty
    /// result), not hang. With the test server's 1 s poll timeout this
    /// returns quickly.
    #[test]
    fn wake_notice_timeout_when_nothing() {
        use chatx_core::signal::DirectoryClient;
        use chatx_core::signal::HttpDirectory;
        let server = Server::start();
        let dir = HttpDirectory::new(server.base.clone());
        let start = std::time::Instant::now();
        let out = dir.poll_notices("nobody-wakes-me");
        let dur = start.elapsed();
        assert!(out.is_empty(), "no wake was sent, so the result is empty");
        // Should not take much longer than the test's 1 s poll timeout.
        assert!(
            dur.as_secs() <= 5,
            "poll_notices took too long: {dur:?} (test expects ~1 s timeout)"
        );
    }

    #[test]
    fn b_mode_endpoints_over_http() {
        use chatx_core::signal::{DirectoryClient, HttpDirectory};
        let server = Server::start();
        let dir = HttpDirectory::new(server.base.clone());

        // resolve_group works.
        let pubinfo = dir.resolve_group("grp-it").expect("resolve group");
        assert_eq!(pubinfo.mode, GroupMode::Server);

        // Key slot PUT + GET round-trip.
        dir.put_group_key_slot("grp-it", "member-a", "E-OWNER", "SEAL-A")
            .expect("put key slot");
        let (sender_e2e, sealed) = dir
            .get_group_key_slot("grp-it", "member-a")
            .expect("get key slot");
        assert_eq!(sender_e2e, "E-OWNER");
        assert_eq!(sealed, "SEAL-A");

        fn post(base: &str, from: &str) -> std::result::Result<u64, ureq::Error> {
            #[derive(serde::Deserialize)]
            struct Out {
                seq: u64,
            }
            let url = format!("{base}/v1/groups/grp-it/messages");
            let body = serde_json::json!({"from": from, "wire": "WIRE"}).to_string();
            let resp = ureq::request("POST", &url)
                .set("content-type", "application/json")
                .send_string(&body)?;
            Ok(resp.into_json::<Out>().expect("parse post response").seq)
        }

        // Member "owner" is allowed (membership check passes).
        let seq = post(&server.base, "owner").expect("owner may post");
        assert!(seq >= 1, "posted message must have a seq");

        // Non-member is rejected with 403.
        match post(&server.base, "intruder") {
            Err(ureq::Error::Status(403, _)) => {}
            other => panic!("expected 403 for non-member, got {other:?}"),
        }

        // Poll returns the one accepted message.
        let msgs = dir.poll_group_messages("grp-it", 0).expect("poll messages");
        assert_eq!(msgs.len(), 1, "only the accepted message is stored");
        assert_eq!(msgs[0].from, "owner");

        // Delete the key slot, then it is gone.
        assert!(
            dir.delete_group_key_slot("grp-it", "member-a")
                .expect("delete slot"),
            "slot must be removed"
        );
        assert!(
            !dir.delete_group_key_slot("grp-it", "member-a")
                .expect("second delete"),
            "second delete must be a no-op"
        );
        assert!(
            dir.get_group_key_slot("grp-it", "member-a").is_err(),
            "slot must be gone after delete"
        );

        drop(server); // drop tempdir
    }
}
