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

#[derive(Clone)]
struct App {
    db: Arc<db::Sdb>,
}

#[derive(Deserialize)]
struct IdQuery {
    #[serde(default)]
    exclude: Option<String>,
    #[serde(default)]
    status: Option<String>,
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
        .unwrap_or_else(|_| "0.0.0.0:8787".into())
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

    let app = App { db };

    let router = make_router(app.clone());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind TCP listener");
    tracing::info!(%addr, "chatx-signal listening");

    // Built-in circuit-relay v2 server (enable with a CHATX_RELAY_ADDR value,
    // disable with `CHATX_RELAY_ADDR=off`). Defaults to 0.0.0.0:4039.
    let relay_spec = std::env::var("CHATX_RELAY_ADDR").unwrap_or_else(|_| "0.0.0.0:4039".into());
    if relay_spec.to_ascii_lowercase() != "off" {
        let relay_addr: SocketAddr = relay_spec
            .parse()
            .expect("CHATX_RELAY_ADDR must be host:port or `off`");
        tokio::spawn(async move {
            if let Err(e) = relay::serve(relay_addr).await {
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
                    let app = App { db: db.clone() };
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
