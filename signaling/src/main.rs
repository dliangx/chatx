
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use chatx_core::account::{AttestationAction, DeviceRecord, DeviceStatus, UserRecord};
use chatx_core::group::GroupPublic;
use chatx_core::message::now_ms;
use chatx_core::signal::{ONLINE_TTL_MS, UserResolve};
use serde::{Deserialize, Serialize};

mod db;

#[derive(Clone)]
struct App {
    users: Arc<Mutex<HashMap<String, UserRecord>>>,
    devices: Arc<Mutex<HashMap<String, DeviceRecord>>>,
    groups: Arc<Mutex<HashMap<String, GroupPublic>>>,
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

fn best_device(
    devices: &HashMap<String, DeviceRecord>,
    user_id: &str,
    exclude_peer: &str,
) -> Option<DeviceRecord> {
    let now = now_ms();
    let mut best: Option<&DeviceRecord> = None;
    for d in devices.values() {
        if d.user_id != user_id {
            continue;
        }
        if d.status != DeviceStatus::Approved {
            continue;
        }
        if now.saturating_sub(d.seen) > ONLINE_TTL_MS {
            continue;
        }
        if !exclude_peer.is_empty() && d.peer_id == exclude_peer {
            continue;
        }
        match best {
            None => best = Some(d),
            Some(b) if d.seen > b.seen => best = Some(d),
            _ => {}
        }
    }
    best.cloned()
}


async fn upsert_user(
    State(App { users, db, .. }): State<App>,
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
    users.lock().unwrap().insert(rec.user_id.clone(), rec);
    Ok(ok_json(&"ok"))
}

async fn resolve_user(
    State(App { users, .. }): State<App>,
    Path(user_id): Path<String>,
) -> ApiResult {
    let g = users.lock().unwrap();
    if let Some(r) = g.get(&user_id) {
        return Ok(ok_json(r));
    }
    let pref: Vec<&UserRecord> = g
        .values()
        .filter(|r| r.user_id.starts_with(&user_id))
        .collect();
    if pref.len() == 1 {
        return Ok(ok_json(pref[0]));
    }
    Err(bad(
        StatusCode::NOT_FOUND,
        format!("user {user_id} not found or ambiguous"),
    ))
}

async fn resolve_and_device(
    State(App { users, devices, .. }): State<App>,
    Path(user_id): Path<String>,
) -> ApiResult {
    let user = {
        let g = users.lock().unwrap();
        g.get(&user_id)
            .cloned()
            .or_else(|| {
                let pref: Vec<&UserRecord> = g
                    .values()
                    .filter(|r| r.user_id.starts_with(&user_id))
                    .collect();
                if pref.len() == 1 {
                    Some(pref[0].clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| bad(StatusCode::NOT_FOUND, format!("user {user_id} not found")))?
    };
    let devices = devices.lock().unwrap();
    let device = best_device(&devices, &user.user_id, "")
        .ok_or_else(|| bad(StatusCode::NOT_FOUND, "no online APPROVED device"))?;
    Ok(ok_json(&UserResolve { user, device }))
}

async fn list_user_devices(
    State(App { devices, .. }): State<App>,
    Path(user_id): Path<String>,
    Query(q): Query<IdQuery>,
) -> ApiResult {
    let g = devices.lock().unwrap();
    let mut out: Vec<DeviceRecord> = g
        .values()
        .filter(|d| d.user_id == user_id)
        .cloned()
        .collect();
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
    State(App { users, devices, .. }): State<App>,
    Query(q): Query<IdQuery>,
) -> ApiResult {
    let users = users.lock().unwrap();
    let devices = devices.lock().unwrap();
    let exclude = q.exclude.as_deref().unwrap_or_default();
    let mut out: Vec<UserResolve> = Vec::new();
    for user in users.values() {
        if let Some(device) = best_device(&devices, &user.user_id, exclude) {
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
fn approver_is_trusted(
    devices: &HashMap<String, DeviceRecord>,
    user_id: &str,
    approver: &str,
    self_peer: &str,
) -> bool {
    let Some(a) = devices.get(approver) else {
        return false;
    };
    a.status == DeviceStatus::Approved
        && a.user_id == user_id
        && approver != self_peer
}

/// Server-side authorization check for a device status transition.
///
/// The client is expected to sign an [`chatx_core::account::Attestation`]
/// with its account key. The *previous* status is read from the in-memory
/// directory, so the server does not trust arbitrary upserts.
///
/// - New device (no prior record): only `Pending`, or `Approved` via a valid
///   **self-attestation** (i.e. the first device of a fresh account).
/// - Status unchanged: allowed (heartbeat / endpoints refresh).
/// - `Pending -> Approved`: requires a valid signed attestation whose
///   `approver` is a distinct, already-approved device of the same account.
/// - `Approved / Revoked -> Revoked`: same requirements.
/// - Any other transition is rejected.
fn validate_device_update(
    devices: &HashMap<String, DeviceRecord>,
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
                att.action,
                expected,
                incoming.status
            ));
        }
        if !approver_is_trusted(devices, &incoming.user_id, &att.approver, &incoming.peer_id) {
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
    State(App { devices, db, .. }): State<App>,
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
    tracing::info!(peer_id = %rec.peer_id, user = %rec.user_id, status = ?rec.status, "device upsert");

    // Snapshot the current directory under the lock, then validate the
    // transition without holding the guard (the approver check reads the map).
    let (snapshot, existing) = {
        let g = devices.lock().unwrap().clone();
        let ex = g.get(&rec.peer_id).cloned();
        (g, ex)
    };
    if let Err(e) = validate_device_update(&snapshot, existing.as_ref(), &rec) {
        tracing::warn!(peer_id = %rec.peer_id, status = ?rec.status, %e, "device upsert rejected");
        return Err(bad(StatusCode::FORBIDDEN, e));
    }

    if let Err(e) = db::upsert_device(db.as_ref(), &rec).await {
        tracing::warn!(%e, "persisting device failed");
        return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "db error"));
    }
    devices.lock().unwrap().insert(rec.peer_id.clone(), rec);
    Ok(ok_json(&"ok"))
}

async fn resolve_device(
    State(App { devices, .. }): State<App>,
    Path(peer_id): Path<String>,
) -> ApiResult {
    let g = devices.lock().unwrap();
    if let Some(r) = g.get(&peer_id) {
        return Ok(ok_json(r));
    }
    let pref: Vec<&DeviceRecord> = g
        .values()
        .filter(|d| d.peer_id.starts_with(&peer_id))
        .collect();
    if pref.len() == 1 {
        return Ok(ok_json(pref[0]));
    }
    Err(bad(
        StatusCode::NOT_FOUND,
        format!("device {peer_id} not found or ambiguous"),
    ))
}

async fn delete_device(
    State(App { devices, db, .. }): State<App>,
    Path(peer_id): Path<String>,
) -> ApiResult {
    let removed = devices.lock().unwrap().remove(&peer_id).is_some();
    match db::delete_device(db.as_ref(), &peer_id).await {
        Ok(_) if removed => Ok(StatusCode::NO_CONTENT.into_response()),
        Ok(_) => Err(bad(StatusCode::NOT_FOUND, "not found")),
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
    State(App { devices, .. }): State<App>,
    Path(peer_id): Path<String>,
    body: String,
) -> ApiResult {
    let body: Presence = match serde_json::from_str(&body) {
        Ok(b) => b,
        Err(e) => return Err(bad(StatusCode::BAD_REQUEST, format!("bad Presence: {e}"))),
    };
    let mut g = devices.lock().unwrap();
    match g.get_mut(&peer_id) {
        Some(d) => {
            d.seen = body.seen.unwrap_or_else(now_ms);
            if let Some(ep) = body.endpoints {
                d.endpoints = ep;
            }
            tracing::debug!(peer_id = %d.peer_id, "device presence touch");
        }
        None => {
            return Err(bad(
                StatusCode::NOT_FOUND,
                format!("device {peer_id} not found"),
            ));
        }
    }
    Ok(ok_json(&"ok"))
}

async fn health() -> ApiResult {
    Ok(ok_json(&"ok"))
}


async fn upsert_group(
    State(App { groups, db, .. }): State<App>,
    Path(group_id): Path<String>,
    body: String,
) -> ApiResult {
    let mut g: GroupPublic = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return Err(bad(StatusCode::BAD_REQUEST, format!("bad GroupPublic: {e}"))),
    };
    g.group_id = group_id.clone();
    if let Err(e) = db::upsert_group(db.as_ref(), &g).await {
        tracing::warn!(%e, "persisting group failed");
        return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, "db error"));
    }
    groups.lock().unwrap().insert(g.group_id.clone(), g);
    Ok(ok_json(&"ok"))
}

async fn resolve_group(
    State(App { groups, .. }): State<App>,
    Path(group_id): Path<String>,
) -> ApiResult {
    let g = groups
        .lock()
        .unwrap()
        .get(&group_id)
        .cloned()
        .ok_or_else(|| bad(StatusCode::NOT_FOUND, format!("group {group_id} not found")))?;
    Ok(ok_json(&g))
}

async fn list_groups(
    State(App { groups, .. }): State<App>,
) -> ApiResult {
    let mut out: Vec<GroupPublic> = groups.lock().unwrap().values().cloned().collect();
    out.sort_by_key(|g| g.group_id.clone());
    Ok(ok_json(&out))
}

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "p2pchat_signal=info".into()))
        .try_init();

    let addr: SocketAddr = std::env::var("P2PCHAT_SIGNAL_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8787".into())
        .parse()
        .expect("P2PCHAT_SIGNAL_ADDR must be a socket address");

    let db_path = std::env::var("P2PCHAT_SIGNAL_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("signal.db"));
    let db = Arc::new(db::open(&db_path).await.expect("open sqlite db"));
    tracing::info!(path = %db_path.display(), "signaling db ready");

    let (users, devices, groups) = db::load_all(db.as_ref()).await.expect("load directory");
    tracing::info!(
        users = %users.len(),
        devices = %devices.len(),
        groups = %groups.len(),
        "directory restored"
    );

    let app = App {
        users: Arc::new(Mutex::new(users)),
        devices: Arc::new(Mutex::new(devices)),
        groups: Arc::new(Mutex::new(groups)),
        db,
    };

    let router = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/users", get(list_users))
        .route("/v1/users/{user_id}", put(upsert_user).get(resolve_user))
        .route("/v1/users/{user_id}/resolve", get(resolve_and_device))
        .route("/v1/users/{user_id}/devices", get(list_user_devices))
        .route(
            "/v1/devices/{peer_id}",
            put(upsert_device).get(resolve_device).delete(delete_device),
        )
        .route("/v1/devices/{peer_id}/presence", put(device_presence))
        .route("/v1/groups", get(list_groups))
        .route("/v1/groups/{group_id}", put(upsert_group).get(resolve_group))
        .with_state(app.clone());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind TCP listener");
    tracing::info!(%addr, "p2pchat-signal listening");
    axum::serve(listener, router).await.expect("serve");
}
