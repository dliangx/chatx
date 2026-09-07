//! p2pchat-signal — 中心化目录服务器（axum）。
//!
//! 职责边界（方案4）：只存**账户公钥 + 设备公钥 + 端点 + 审批证明**；
//! 不存口令/私钥，不做消息存储 / 群组扇出。
//!
//!   GET    /v1/health                          探活
//!
//!   PUT    /v1/users/{user_id}                 刷新账户目录
//!   GET    /v1/users/{user_id}                  解析账户
//!   GET    /v1/users/{user_id}/resolve         账户 + 首选在线 APPROVED 设备
//!   GET    /v1/users/{user_id}/devices         账户所有设备（可选 ?status=）
//!   GET    /v1/users?exclude={peer_id}        在线用户列表
//!
//!   PUT    /v1/devices/{peer_id}               登记/刷新设备
//!   GET    /v1/devices/{peer_id}               解析设备
//!   DELETE /v1/devices/{peer_id}               撤销登记
//!
//! 跑：`cargo run -p p2pchat-signal`（默认 0.0.0.0:8787）

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use chatx_core::account::{DeviceRecord, DeviceStatus, UserRecord};
use chatx_core::message::now_ms;
use chatx_core::signal::{ONLINE_TTL_MS, UserResolve};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
struct App {
    users: Arc<Mutex<HashMap<String, UserRecord>>>,
    devices: Arc<Mutex<HashMap<String, DeviceRecord>>>,
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

/// 找某用户首选在线 APPROVED 设备。
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

// ── 账户 ──

async fn upsert_user(
    State(App { users, .. }): State<App>,
    Path(user_id): Path<String>,
    body: String,
) -> ApiResult {
    let mut rec: UserRecord = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return Err(bad(StatusCode::BAD_REQUEST, format!("bad UserRecord: {e}"))),
    };
    rec.user_id = user_id.clone();
    rec.seen = now_ms();
    users.lock().unwrap().insert(user_id, rec);
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
    State(App { users, devices }): State<App>,
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
    State(App { users, devices }): State<App>,
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

// ── 设备 ──

async fn upsert_device(
    State(App { devices, .. }): State<App>,
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
    devices.lock().unwrap().insert(peer_id, rec);
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
    State(App { devices, .. }): State<App>,
    Path(peer_id): Path<String>,
) -> ApiResult {
    if devices.lock().unwrap().remove(&peer_id).is_some() {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(bad(StatusCode::NOT_FOUND, "not found"))
    }
}

#[derive(Serialize, Deserialize)]
struct Presence {
    seen: Option<u64>,
    endpoints: Option<Vec<String>>,
}

/// 轻量 presence：只刷 `seen` + `endpoints`（不视为完整 upsert，不打 upsert 日志）。
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

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "p2pchat_signal=info".into()))
        .try_init();

    let addr: SocketAddr = std::env::var("P2PCHAT_SIGNAL_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8787".into())
        .parse()
        .expect("P2PCHAT_SIGNAL_ADDR must be a socket address");

    let app = App {
        users: Arc::new(Mutex::new(HashMap::new())),
        devices: Arc::new(Mutex::new(HashMap::new())),
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
        .with_state(app.clone());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind TCP listener");
    tracing::info!(%addr, "p2pchat-signal listening");
    axum::serve(listener, router).await.expect("serve");
}
