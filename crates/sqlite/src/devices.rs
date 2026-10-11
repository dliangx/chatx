use crate::{now_ms, Pool};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct Device {
    pub device_id: i64,
    pub user_id: i64,
    pub peer_id: String,
    pub public_key: String,
    pub certificate: Option<String>,
    pub platform: Option<String>,
    pub push_token: Option<String>,
    pub last_seen: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Default, Clone)]
pub struct DevicePatch {
    pub user_id: Option<i64>,
    pub peer_id: Option<String>,
    pub public_key: Option<String>,
    pub certificate: Option<String>,
    pub platform: Option<String>,
    pub push_token: Option<String>,
    pub last_seen: Option<i64>,
}

pub async fn upsert(pool: &Pool, device_id: i64, patch: &DevicePatch) -> anyhow::Result<()> {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO devices (device_id, user_id, peer_id, public_key, certificate, platform, push_token, last_seen, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(device_id) DO UPDATE SET
            user_id     = COALESCE(?2, user_id),
            peer_id     = COALESCE(?3, peer_id),
            public_key  = COALESCE(?4, public_key),
            certificate = COALESCE(?5, certificate),
            platform    = COALESCE(?6, platform),
            push_token  = COALESCE(?7, push_token),
            last_seen   = COALESCE(?8, last_seen)",
    )
    .bind(device_id)
    .bind(patch.user_id)
    .bind(patch.peer_id.as_deref())
    .bind(patch.public_key.as_deref())
    .bind(patch.certificate.as_deref())
    .bind(patch.platform.as_deref())
    .bind(patch.push_token.as_deref())
    .bind(patch.last_seen)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get(pool: &Pool, device_id: i64) -> anyhow::Result<Option<Device>> {
    let row = sqlx::query_as::<_, Device>("SELECT * FROM devices WHERE device_id = ?1")
        .bind(device_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn get_by_peer(pool: &Pool, peer_id: &str) -> anyhow::Result<Option<Device>> {
    let row = sqlx::query_as::<_, Device>("SELECT * FROM devices WHERE peer_id = ?1")
        .bind(peer_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Resolve a libp2p PeerId to a known user id by looking up the `devices`
/// table. Returns `Ok(None)` when no device row exists for that peer — the
/// caller decides how to render the message (typically with a NULL user id).
///
/// This function is **read-only**: it never creates a `users` row. Historically
/// it called `users::ensure_identity(peer_id)`, which ghost-created user rows
/// keyed by the base58 PeerId (the bug fixed in migration 3).
pub async fn ensure_user_by_peer(pool: &Pool, peer_id: &str) -> anyhow::Result<Option<i64>> {
    let d = get_by_peer(pool, peer_id).await?;
    Ok(d.map(|d| d.user_id))
}

/// Bind a libp2p `devices` row for `peer_id`, owned by `user_id`. Idempotent:
/// reuses an existing device row for the same peer. Shared by the login path
/// (own device) and the add-contact path (a friend's device).
pub async fn bind_device(pool: &Pool, user_id: i64, peer_id: &str) -> anyhow::Result<i64> {
    let device_id = match get_by_peer(pool, peer_id).await? {
        Some(d) => d.device_id,
        None => crate::new_id(),
    };
    upsert(
        pool,
        device_id,
        &DevicePatch {
            user_id: Some(user_id),
            peer_id: Some(peer_id.into()),
            public_key: Some(String::new()),
            ..Default::default()
        },
    )
    .await?;
    Ok(device_id)
}

/// Bind an account's `users` row to a libp2p `devices` row so that incoming
/// messages from that device can be resolved to a real `users.id` (with the
/// account's real username) instead of leaving the sender as a raw peer id.
///
/// Call on every login: creates the `users` row on first login (username =
/// the account's user_id string, e.g. "alice") and binds this peer to it.
/// Own-user rows intentionally get **no** nickname — nickname is "what I show
/// for *them*", so it stays empty for myself. Repeat logins are idempotent.
pub async fn bind_user_device(
    pool: &Pool,
    username: &str,
    peer_id: &str,
) -> anyhow::Result<(i64, i64)> {
    let user_id = match crate::users::get_by_username(pool, username).await? {
        Some(u) => u.id,
        None => crate::users::upsert(
            pool,
            crate::new_id(),
            &crate::users::UserPatch {
                username: Some(username.to_string()),
                ..Default::default()
            },
        )
        .await?
        .id,
    };
    let device_id = bind_device(pool, user_id, peer_id).await?;
    Ok((user_id, device_id))
}

pub async fn list_by_user(pool: &Pool, user_id: i64) -> anyhow::Result<Vec<Device>> {
    let rows = sqlx::query_as::<_, Device>("SELECT * FROM devices WHERE user_id = ?1 ORDER BY created_at")
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn touch_last_seen(pool: &Pool, device_id: i64) -> anyhow::Result<()> {
    let now = now_ms();
    sqlx::query("UPDATE devices SET last_seen = ?2 WHERE device_id = ?1")
        .bind(device_id)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &Pool, device_id: i64) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM devices WHERE device_id = ?1")
        .bind(device_id)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    #[tokio::test]
    async fn upsert_and_list_roundtrip() {
        let pool = open_memory().await.unwrap();
        crate::users::upsert(&pool, 1, &crate::users::UserPatch::default()).await.unwrap();
        upsert(
            &pool,
            101,
            &DevicePatch {
                user_id: Some(1),
                peer_id: Some("peer-1".into()),
                public_key: Some("pub-1".into()),
                platform: Some("ios".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        upsert(
            &pool,
            102,
            &DevicePatch {
                user_id: Some(1),
                peer_id: Some("peer-2".into()),
                public_key: Some("pub-2".into()),
                platform: Some("android".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let by_user = list_by_user(&pool, 1).await.unwrap();
        assert_eq!(by_user.len(), 2);

        let by_peer = get_by_peer(&pool, "peer-2").await.unwrap();
        assert_eq!(by_peer.unwrap().device_id, 102);

        touch_last_seen(&pool, 101).await.unwrap();
        assert!(get(&pool, 101).await.unwrap().unwrap().last_seen.is_some());

        assert!(delete(&pool, 102).await.unwrap());
        assert_eq!(list_by_user(&pool, 1).await.unwrap().len(), 1);
    }
}
