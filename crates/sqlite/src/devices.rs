use crate::{now_ms, Pool};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct Device {
    pub device_id: String,
    pub user_id: String,
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
    pub user_id: Option<String>,
    pub peer_id: Option<String>,
    pub public_key: Option<String>,
    pub certificate: Option<String>,
    pub platform: Option<String>,
    pub push_token: Option<String>,
    pub last_seen: Option<i64>,
}

pub async fn upsert(pool: &Pool, device_id: &str, patch: &DevicePatch) -> anyhow::Result<()> {
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
    .bind(patch.user_id.as_deref())
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

pub async fn get(pool: &Pool, device_id: &str) -> anyhow::Result<Option<Device>> {
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

pub async fn list_by_user(pool: &Pool, user_id: &str) -> anyhow::Result<Vec<Device>> {
    let rows = sqlx::query_as::<_, Device>("SELECT * FROM devices WHERE user_id = ?1 ORDER BY created_at")
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn touch_last_seen(pool: &Pool, device_id: &str) -> anyhow::Result<()> {
    let now = now_ms();
    sqlx::query("UPDATE devices SET last_seen = ?2 WHERE device_id = ?1")
        .bind(device_id)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &Pool, device_id: &str) -> anyhow::Result<bool> {
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
        crate::users::upsert(&pool, "u1", &crate::users::UserPatch::default()).await.unwrap();
        upsert(
            &pool,
            "d1",
            &DevicePatch {
                user_id: Some("u1".into()),
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
            "d2",
            &DevicePatch {
                user_id: Some("u1".into()),
                peer_id: Some("peer-2".into()),
                public_key: Some("pub-2".into()),
                platform: Some("android".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let by_user = list_by_user(&pool, "u1").await.unwrap();
        assert_eq!(by_user.len(), 2);

        let by_peer = get_by_peer(&pool, "peer-2").await.unwrap();
        assert_eq!(by_peer.unwrap().device_id, "d2");

        touch_last_seen(&pool, "d1").await.unwrap();
        assert!(get(&pool, "d1").await.unwrap().unwrap().last_seen.is_some());

        assert!(delete(&pool, "d2").await.unwrap());
        assert_eq!(list_by_user(&pool, "u1").await.unwrap().len(), 1);
    }
}
