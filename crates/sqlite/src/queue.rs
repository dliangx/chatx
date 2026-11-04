//! Client-side DM offline queue. Each row is an outgoing text message whose
//! local `messages` row is still `status = sending` — i.e. the peer never
//! acked — pending a redelivery attempt.
//!
//! The redelivery task in `core::Client` scans [`due`] until the peer acks
//! (row removed, local row `status` → `sent`) or the attempt budget is
//! exhausted (row removed, local row `status` → `failed`). This module owns
//! retry bookkeeping only; the caller flips the message row status.
//!
//! Text only by design — media clips are multi-MB and resending them would
//! dominate; media keeps the "retry manually on failure" behaviour.

use crate::{now_ms, Pool};
use sqlx::FromRow;

/// Total redelivery attempts before giving up (first send + 11 retries).
pub const MAX_ATTEMPTS: i64 = 12;

/// Per-attempt backoff schedule (seconds), capped at the last entry. With
/// 11 retries over this schedule the total wait is ~10h — comfortably
/// inside a 24h delivery window while still giving up on permanently
/// unreachable peers.
const BACKOFF_SECS: [i64; 7] = [5, 15, 60, 300, 900, 1800, 3600];

#[derive(Debug, FromRow, Clone)]
pub struct Row {
    pub msg_id: i64,
    pub peer_base58: String,
    pub chat_id: String,
    pub attempts: i64,
    pub next_try_ms: i64,
    pub created_at: i64,
}

pub fn next_delay_ms(attempts: i64) -> i64 {
    let idx = (attempts as usize).min(BACKOFF_SECS.len() - 1);
    BACKOFF_SECS[idx] * 1000
}

/// Register `msg_id` as awaiting delivery. INSERT OR REPLACE makes
/// re-enqueue idempotent (redeliver loop racing a fresh `send_dm`).
pub async fn enqueue(
    pool: &Pool,
    msg_id: i64,
    peer_base58: &str,
    chat_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO dm_send_queue (msg_id, peer_base58, chat_id, attempts, next_try_ms, created_at)
         VALUES (?1, ?2, ?3, 0, ?4, ?4)",
    )
    .bind(msg_id)
    .bind(peer_base58)
    .bind(chat_id)
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

/// All rows whose retry cooldown has elapsed, oldest first.
pub async fn due(pool: &Pool, limit: u32) -> anyhow::Result<Vec<Row>> {
    sqlx::query_as::<_, Row>(
        "SELECT msg_id, peer_base58, chat_id, attempts, next_try_ms, created_at
         FROM dm_send_queue
         WHERE next_try_ms <= ?1
         ORDER BY created_at ASC
         LIMIT ?2",
    )
    .bind(now_ms())
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// Bump the attempt counter and refresh the retry cooldown.
///
/// Returns the attempt count AFTER this bump. When it reaches
/// [`MAX_ATTEMPTS`] the caller is expected to call [`give_up`] and mark the
/// local message row failed.
pub async fn bump(pool: &Pool, msg_id: i64) -> anyhow::Result<i64> {
    let delay = {
        let (a,): (i64,) = sqlx::query_as("SELECT attempts FROM dm_send_queue WHERE msg_id = ?1")
            .bind(msg_id)
            .fetch_one(pool)
            .await?;
        next_delay_ms(a)
    };
    let n = sqlx::query(
        "UPDATE dm_send_queue SET attempts = attempts + 1, next_try_ms = ?2 WHERE msg_id = ?1",
    )
    .bind(msg_id)
    .bind(now_ms() + delay)
    .execute(pool)
    .await?;
    if n.rows_affected() == 0 {
        return Ok(MAX_ATTEMPTS); // already removed — treat as exhausted
    }
    let (a,): (i64,) =
        sqlx::query_as("SELECT attempts FROM dm_send_queue WHERE msg_id = ?1")
            .bind(msg_id)
            .fetch_one(pool)
            .await?;
    Ok(a)
}

/// Drop the row after the budget is exhausted. The caller flips the local
/// message row to `status = 4 (failed)`.
pub async fn give_up(pool: &Pool, msg_id: i64) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM dm_send_queue WHERE msg_id = ?1")
        .bind(msg_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Due rows for a specific peer only (used by the Connected-event handler
/// for immediate redelivery).
pub async fn due_for_peer(pool: &Pool, peer_base58: &str, limit: u32) -> anyhow::Result<Vec<Row>> {
    sqlx::query_as::<_, Row>(
        "SELECT msg_id, peer_base58, chat_id, attempts, next_try_ms, created_at
         FROM dm_send_queue
         WHERE peer_base58 = ?1 AND next_try_ms <= ?2
         ORDER BY created_at ASC
         LIMIT ?3",
    )
    .bind(peer_base58)
    .bind(now_ms())
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// All currently pending (undelivered) message ids, regardless of when they
/// become due. Used by the UI to flag "发送中" rows in the chat bubble.
pub async fn all_pending_ids(pool: &Pool) -> anyhow::Result<Vec<i64>> {
    sqlx::query_scalar("SELECT msg_id FROM dm_send_queue")
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> Pool {
        crate::open_memory().await.unwrap()
    }

    #[tokio::test]
    async fn enqueue_bump_exhaustion_flow() {
        let p = pool().await;
        enqueue(&p, 1, "peerA", "chatA").await.unwrap();

        let rows = due(&p, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].msg_id, 1);
        assert_eq!(rows[0].attempts, 0);

        // Backoff grows monotonically and is capped.
        assert!(next_delay_ms(0) < next_delay_ms(3));
        assert_eq!(next_delay_ms(99), next_delay_ms(6));

        // Bumping eventually exhausts.
        let mut attempts = 0i64;
        for _ in 0..MAX_ATTEMPTS {
            attempts = bump(&p, 1).await.unwrap();
        }
        assert_eq!(attempts, MAX_ATTEMPTS);
        give_up(&p, 1).await.unwrap();
        assert!(due(&p, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn due_for_peer_filters_by_peer() {
        let p = pool().await;
        enqueue(&p, 10, "peerX", "cX").await.unwrap();
        enqueue(&p, 11, "peerY", "cY").await.unwrap();

        let only_x = due_for_peer(&p, "peerX", 10).await.unwrap();
        assert_eq!(only_x.len(), 1);
        assert_eq!(only_x[0].msg_id, 10);

        let none = due_for_peer(&p, "peerMissing", 10).await.unwrap();
        assert!(none.is_empty());

        // enqueue is idempotent (re-sending the same message doesn't dup)
        enqueue(&p, 10, "peerX", "cX").await.unwrap();
        assert_eq!(due(&p, 100).await.unwrap().len(), 2);
    }
}
