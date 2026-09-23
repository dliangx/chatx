use crate::{now_ms, Pool};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct OfflineMessage {
    pub id: i64,
    pub recipient_id: String,
    pub message_id: String,
    pub sender_id: String,
    pub created_at: i64,
    pub delivered: bool,
}

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct PushToken {
    pub user_id: String,
    pub device_id: String,
    pub token: String,
    pub platform: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct SyncSequence {
    pub conversation_id: String,
    pub last_seq: i64,
}

// --- offline messages ------------------------------------------------------

pub async fn enqueue(pool: &Pool, recipient_id: &str, message_id: &str, sender_id: &str) -> anyhow::Result<bool> {
    let n = sqlx::query(
        "INSERT OR IGNORE INTO offline_messages (recipient_id, message_id, sender_id, created_at, delivered)
         VALUES (?1, ?2, ?3, ?4, 0)",
    )
    .bind(recipient_id)
    .bind(message_id)
    .bind(sender_id)
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(n.rows_affected() > 0)
}

pub async fn pending_for(pool: &Pool, recipient_id: &str, limit: u32) -> anyhow::Result<Vec<OfflineMessage>> {
    let rows = sqlx::query_as::<_, OfflineMessage>(
        "SELECT * FROM offline_messages
         WHERE recipient_id = ?1 AND delivered = 0
         ORDER BY created_at
         LIMIT ?2",
    )
    .bind(recipient_id)
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn mark_delivered(pool: &Pool, row_id: i64) -> anyhow::Result<bool> {
    let n = sqlx::query("UPDATE offline_messages SET delivered = 1 WHERE id = ?1")
        .bind(row_id)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

pub async fn count_pending(pool: &Pool, recipient_id: &str) -> anyhow::Result<u32> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM offline_messages WHERE recipient_id = ?1 AND delivered = 0",
    )
    .bind(recipient_id)
    .fetch_one(pool)
    .await?;
    Ok(n as u32)
}

// --- push tokens -----------------------------------------------------------

pub async fn upsert_push_token(
    pool: &Pool,
    user_id: &str,
    device_id: &str,
    token: &str,
    platform: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO push_tokens (user_id, device_id, token, platform, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(user_id, device_id) DO UPDATE SET
            token      = ?3,
            platform   = ?4,
            updated_at = ?5",
    )
    .bind(user_id)
    .bind(device_id)
    .bind(token)
    .bind(platform)
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_push_tokens(pool: &Pool, user_id: &str) -> anyhow::Result<Vec<PushToken>> {
    sqlx::query_as::<_, PushToken>("SELECT * FROM push_tokens WHERE user_id = ?1 ORDER BY updated_at DESC")
        .bind(user_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

pub async fn remove_push_token(pool: &Pool, user_id: &str, device_id: &str) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM push_tokens WHERE user_id = ?1 AND device_id = ?2")
        .bind(user_id)
        .bind(device_id)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

// --- sync sequences --------------------------------------------------------

pub async fn get_seq(pool: &Pool, conversation_id: &str) -> anyhow::Result<i64> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT last_seq FROM sync_sequences WHERE conversation_id = ?1")
        .bind(conversation_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(s,)| s).unwrap_or(0))
}

/// Atomically fetch-and-seek the current sequence (does not advance it).
pub async fn set_seq(pool: &Pool, conversation_id: &str, seq: i64) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO sync_sequences (conversation_id, last_seq) VALUES (?1, ?2)
         ON CONFLICT(conversation_id) DO UPDATE SET last_seq = ?2",
    )
    .bind(conversation_id)
    .bind(seq)
    .execute(pool)
    .await?;
    Ok(())
}

/// Atomically advance the sequence by `delta` (min 0) and return the new value.
pub async fn bump_seq(pool: &Pool, conversation_id: &str, delta: i64) -> anyhow::Result<i64> {
    let next = get_seq(pool, conversation_id).await? + delta.max(0);
    set_seq(pool, conversation_id, next).await?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    #[tokio::test]
    async fn offline_queue_flow() {
        let pool = open_memory().await.unwrap();
        assert!(enqueue(&pool, "u1", "m1", "sender").await.unwrap());
        assert!(!enqueue(&pool, "u1", "m1", "sender").await.unwrap(), "dedup by (recipient, message)");
        enqueue(&pool, "u1", "m2", "sender").await.unwrap();
        assert_eq!(count_pending(&pool, "u1").await.unwrap(), 2);

        let pending = pending_for(&pool, "u1", 10).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert!(mark_delivered(&pool, pending[0].id).await.unwrap());
        assert!(!mark_delivered(&pool, 999999).await.unwrap());
        assert_eq!(count_pending(&pool, "u1").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn push_tokens_and_sequences() {
        let pool = open_memory().await.unwrap();
        upsert_push_token(&pool, "u1", "d1", "tok-ios", "ios").await.unwrap();
        upsert_push_token(&pool, "u1", "d1", "tok-2", "ios").await.unwrap();
        upsert_push_token(&pool, "u1", "d2", "tok-android", "android").await.unwrap();
        assert_eq!(list_push_tokens(&pool, "u1").await.unwrap().len(), 2);

        assert!(remove_push_token(&pool, "u1", "d2").await.unwrap());
        assert_eq!(list_push_tokens(&pool, "u1").await.unwrap().len(), 1);

        assert_eq!(get_seq(&pool, "c1").await.unwrap(), 0);
        set_seq(&pool, "c1", 5).await.unwrap();
        assert_eq!(get_seq(&pool, "c1").await.unwrap(), 5);
        assert_eq!(bump_seq(&pool, "c1", 2).await.unwrap(), 7);
        assert_eq!(bump_seq(&pool, "c1", -5).await.unwrap(), 7, "negative delta ignored");
    }
}
