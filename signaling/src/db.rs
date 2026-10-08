//! SQLite persistence for the signaling server's in-memory directory.
//!
//! Backs the `App` maps (users / devices / groups) so a restart repopulates
//! the durable data: identity keys, device authorization, groups, and the
//! last-known device endpoints (so a direct reconnect can dial them).
//! The one thing reset to a cold start is liveness — `seen` is zeroed so no
//! device reads as "online" until it re-heartbeats (the presence TTL then
//! filters it out everywhere).
use std::collections::HashMap;

use chatx_core::account::{Attestation, DeviceRecord, DeviceStatus, UserRecord};
use chatx_core::group::GroupPublic;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::Pool;

pub type Sdb = Pool<sqlx::Sqlite>;

pub async fn open(path: &std::path::Path) -> anyhow::Result<Sdb> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let url = format!("sqlite:{}?mode=rwc", path.display());
    let pool = SqlitePoolOptions::new().max_connections(1).connect(&url).await?;
    {
        let mut conn = pool.acquire().await?;
        sqlx::query("PRAGMA journal_mode = WAL").execute(&mut *conn).await?;
        sqlx::query("PRAGMA synchronous = NORMAL").execute(&mut *conn).await?;
    }
    apply_migrations(&pool).await?;
    Ok(pool)
}

async fn apply_migrations(pool: &Sdb) -> anyhow::Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY);",
    )
    .execute(pool)
    .await?;
    const SQL: &str = include_str!("migrations/0001_signal.sql");
    sqlx::raw_sql(SQL).execute(pool).await?;
    Ok(())
}

fn status_to_str(s: &DeviceStatus) -> &'static str {
    match s {
        DeviceStatus::Pending => "pending",
        DeviceStatus::Approved => "approved",
        DeviceStatus::Revoked => "revoked",
    }
}

fn status_from_str(s: &str) -> DeviceStatus {
    match s {
        "approved" => DeviceStatus::Approved,
        "revoked" => DeviceStatus::Revoked,
        _ => DeviceStatus::Pending,
    }
}

// --- upserts (write-through) -------------------------------------------------

pub async fn upsert_user(db: &Sdb, u: &UserRecord) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO users (user_id, e2e_public, sign_pk, seen)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(user_id) DO UPDATE SET
            e2e_public = ?2,
            sign_pk    = ?3,
            seen       = ?4",
    )
    .bind(&u.user_id)
    .bind(&u.e2e_public)
    .bind(&u.sign_pk)
    .bind(u.seen as i64)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn upsert_device(db: &Sdb, d: &DeviceRecord) -> anyhow::Result<()> {
    let att: Option<String> = d
        .attestation
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| anyhow::anyhow!("attestation: {e}"))?;
    let eps = serde_json::to_string(&d.endpoints)?;
    sqlx::query(
        "INSERT INTO devices (peer_id, user_id, device_pk, e2e_public, label, status,
                              proposer, approved_by, approved_at, attestation, endpoints, seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(peer_id) DO UPDATE SET
            user_id      = ?2,
            device_pk    = ?3,
            e2e_public   = ?4,
            label        = ?5,
            status       = ?6,
            proposer     = ?7,
            approved_by  = ?8,
            approved_at  = ?9,
            attestation  = ?10,
            endpoints    = ?11,
            seen         = ?12",
    )
    .bind(&d.peer_id)
    .bind(&d.user_id)
    .bind(&d.device_pk)
    .bind(&d.e2e_public)
    .bind(&d.label)
    .bind(status_to_str(&d.status))
    .bind(&d.proposer)
    .bind(&d.approved_by)
    .bind(d.approved_at as i64)
    .bind(att)
    .bind(eps)
    .bind(d.seen as i64)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn upsert_group(db: &Sdb, g: &GroupPublic) -> anyhow::Result<()> {
    let members = serde_json::to_string(&g.members)?;
    sqlx::query(
        "INSERT INTO groups (group_id, owner, members, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(group_id) DO UPDATE SET
            owner      = ?2,
            members    = ?3,
            created_at = ?4",
    )
    .bind(&g.group_id)
    .bind(&g.owner)
    .bind(members)
    .bind(g.created_at as i64)
    .execute(db)
    .await?;
    Ok(())
}

// --- deletes ----------------------------------------------------------------

pub async fn delete_device(db: &Sdb, peer_id: &str) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM devices WHERE peer_id = ?1")
        .bind(peer_id)
        .execute(db)
        .await?;
    Ok(n.rows_affected() > 0)
}

// --- hydration (reload from disk) ------------------------------------------

/// Restore the directory from disk.
///
/// `seen` is zeroed so nothing is "online" immediately after a bounce; the
/// online-status filters (`now - seen > ONLINE_TTL_MS`) treat every restored
/// device as offline until it re-heartbeats / touches presence. `endpoints`
/// are kept as the last-known addresses for a potential direct reconnect.
pub async fn load_all(db: &Sdb) -> anyhow::Result<(
    HashMap<String, UserRecord>,
    HashMap<String, DeviceRecord>,
    HashMap<String, GroupPublic>,
)> {
    let users: HashMap<String, UserRecord> = sqlx::query_as::<_, UserRow>("SELECT * FROM users")
        .fetch_all(db)
        .await?
        .into_iter()
        .map(UserRow::into_record)
        .map(|r| (r.user_id.clone(), r))
        .collect();

    let devices: HashMap<String, DeviceRecord> = sqlx::query_as::<_, DevRow>("SELECT * FROM devices")
        .fetch_all(db)
        .await?
        .into_iter()
        .map(DevRow::into_record)
        .map(|r| (r.peer_id.clone(), r))
        .collect();

    let groups: HashMap<String, GroupPublic> = sqlx::query_as::<_, GrpRow>("SELECT * FROM groups")
        .fetch_all(db)
        .await?
        .into_iter()
        .map(GrpRow::into_record)
        .map(|r| (r.group_id.clone(), r))
        .collect();

    Ok((users, devices, groups))
}

// --- raw row shapes ---------------------------------------------------------

#[derive(sqlx::FromRow)]
struct UserRow {
    user_id: String,
    e2e_public: String,
    sign_pk: String,
    #[allow(dead_code)]
    seen: i64,
}

impl UserRow {
    fn into_record(self) -> UserRecord {
        UserRecord {
            user_id: self.user_id,
            e2e_public: self.e2e_public,
            sign_pk: self.sign_pk,
            seen: 0,
        }
    }
}

#[derive(sqlx::FromRow)]
struct DevRow {
    peer_id: String,
    user_id: String,
    device_pk: String,
    e2e_public: String,
    label: String,
    status: String,
    proposer: String,
    approved_by: String,
    approved_at: i64,
    attestation: Option<String>,
    endpoints: String,
    #[allow(dead_code)]
    seen: i64,
}

impl DevRow {
    fn into_record(self) -> DeviceRecord {
        DeviceRecord {
            user_id: self.user_id,
            peer_id: self.peer_id,
            device_pk: self.device_pk,
            e2e_public: self.e2e_public,
            label: self.label,
            endpoints: serde_json::from_str(&self.endpoints).unwrap_or_default(),
            status: status_from_str(&self.status),
            proposer: self.proposer,
            approved_by: self.approved_by,
            attestation: self
                .attestation
                .as_deref()
                .and_then(|s| serde_json::from_str::<Attestation>(s).ok()),
            seen: 0,
            approved_at: self.approved_at as u64,
        }
    }
}

#[derive(sqlx::FromRow)]
struct GrpRow {
    group_id: String,
    owner: String,
    members: String,
    created_at: i64,
}

impl GrpRow {
    fn into_record(self) -> GroupPublic {
        let members: std::collections::BTreeMap<String, chatx_core::group::MemberInfo> =
            serde_json::from_str(&self.members).unwrap_or_default();
        GroupPublic {
            group_id: self.group_id,
            owner: self.owner,
            members,
            created_at: self.created_at as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn tmp_db() -> (tempfile::TempDir, Sdb) {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir.path().join("signal.db")).await.unwrap();
        (dir, db)
    }

    #[tokio::test]
    async fn roundtrip_user_device_group() {
        let (_d, db) = tmp_db().await;

        upsert_user(
            &db,
            &UserRecord {
                user_id: "alice".into(),
                e2e_public: "E2E_ALICE".into(),
                sign_pk: "SIGN_ALICE".into(),
                seen: 1234,
            },
        )
        .await
        .unwrap();

        let att = Attestation {
            user_id: "alice".into(),
            device: "QmDev1".into(),
            device_pk: "PK1".into(),
            action: chatx_core::account::AttestationAction::Approve,
            approver: "QmRoot".into(),
            approver_sign_pk: "SIGN_ALICE".into(),
            approved_at: 999,
            signature: "sig==".into(),
        };
        upsert_device(
            &db,
            &DeviceRecord {
                user_id: "alice".into(),
                peer_id: "QmDev1".into(),
                device_pk: "PK1".into(),
                e2e_public: "E2E_ALICE".into(),
                label: "phone".into(),
                endpoints: vec!["/ip4/1.2.3.4/tcp/8787".into()],
                status: DeviceStatus::Approved,
                proposer: String::new(),
                approved_by: "QmRoot".into(),
                attestation: Some(att.clone()),
                seen: 2000,
                approved_at: 999,
            },
        )
        .await
        .unwrap();
        // a pending sibling device to verify multi-device + status round-trip
        upsert_device(
            &db,
            &DeviceRecord {
                user_id: "alice".into(),
                peer_id: "QmDev2".into(),
                device_pk: "PK2".into(),
                e2e_public: "E2E_ALICE".into(),
                label: "laptop".into(),
                endpoints: vec![],
                status: DeviceStatus::Pending,
                proposer: String::new(),
                approved_by: String::new(),
                attestation: None,
                seen: 1,
                approved_at: 0,
            },
        )
        .await
        .unwrap();

        let mut members = std::collections::BTreeMap::new();
        members.insert(
            "QmDev1".to_string(),
            chatx_core::group::MemberInfo {
                peer_id: "QmDev1".into(),
                sign_pk: "SIGN_ALICE".into(),
                e2e_public: "E2E_ALICE".into(),
            },
        );
        let grp = GroupPublic {
            group_id: "grp-1".into(),
            owner: "QmDev1".into(),
            members,
            created_at: 42,
        };
        upsert_group(&db, &grp).await.unwrap();

        // ---- simulate a server restart: fresh pool over the same file ----
        let (users, devices, groups) = load_all(&db).await.unwrap();

        assert_eq!(users.len(), 1);
        let u = &users["alice"];
        assert_eq!(u.e2e_public, "E2E_ALICE");
        assert_eq!(u.sign_pk, "SIGN_ALICE");
        assert_eq!(u.seen, 0, "seen must be reset cold");

        assert_eq!(devices.len(), 2);
        let d1 = &devices["QmDev1"];
        assert_eq!(d1.status, DeviceStatus::Approved);
        assert_eq!(d1.approved_by, "QmRoot");
        assert_eq!(d1.approved_at, 999);
        assert_eq!(
            d1.endpoints,
            vec!["/ip4/1.2.3.4/tcp/8787".to_string()],
            "last-known endpoints are restored for reconnection"
        );
        assert_eq!(d1.seen, 0, "seen must be reset cold (online state re-earned)");
        let a = d1.attestation.as_ref().expect("attestation restored");
        assert_eq!(a.device, "QmDev1");
        assert_eq!(a.signature, "sig==");
        assert_eq!(a.action, chatx_core::account::AttestationAction::Approve);

        assert_eq!(devices["QmDev2"].status, DeviceStatus::Pending);
        assert!(devices["QmDev2"].attestation.is_none());

        assert_eq!(groups.len(), 1);
        let g = &groups["grp-1"];
        assert_eq!(g.owner, "QmDev1");
        assert_eq!(g.created_at, 42);
        assert!(g.members.contains_key("QmDev1"));
        assert_eq!(g.members["QmDev1"].sign_pk, "SIGN_ALICE");

        // ---- delete round-trip ----
        assert!(delete_device(&db, "QmDev1").await.unwrap());
        let (_2u, devs2, _2g) = load_all(&db).await.unwrap();
        assert!(!devs2.contains_key("QmDev1"));
        assert!(delete_device(&db, "QmGhost").await.unwrap() == false);
    }
}
