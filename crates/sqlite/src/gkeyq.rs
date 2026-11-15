//! P2P group-key redelivery queue. One row per (group, member) pair whose
//! group-secret delivery never acked — the dial failed and the member
//! missed the key. Retries are passive (only when the member is connected
//! locally again) and have no deadline: a group key, unlike a media message,
//! is cheap to resend and the member can never recover it any other way in
//! P2P mode. The receiver is idempotent, so redelivery is always safe.

use crate::{now_ms, Pool};
use sqlx::FromRow;

/// Per-attempt backoff schedule (seconds), capped at the last entry. The
/// retry interval grows 5s → 15s → 60s → 5m → 15m → 30m and stays at 30m,
/// so a permanently offline member costs one cheap send attempt every
/// ~30 minutes instead of every loop tick.
const BACKOFF_SECS: [i64; 6] = [5, 15, 60, 300, 900, 1800];

#[derive(Debug, FromRow, Clone)]
pub struct Row {
    pub group_id: String,
    pub peer_base58: String,
    pub attempts: i64,
    pub next_try_ms: i64,
    pub created_at: i64,
}

pub fn next_delay_ms(attempts: i64) -> i64 {
    let idx = (attempts as usize).min(BACKOFF_SECS.len() - 1);
    BACKOFF_SECS[idx] * 1000
}

/// Register a (group, member) pair as awaiting key delivery. INSERT OR
/// REPLACE makes re-enqueue idempotent (a rotate racing a failed publish
/// just resets the counter — the next attempt redelivers the NEW secret).
pub async fn enqueue(
    pool: &Pool,
    group_id: &str,
    peer_base58: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO group_key_queue
         (group_id, peer_base58, attempts, next_try_ms, created_at)
         VALUES (?1, ?2, 0, ?3, ?3)",
    )
    .bind(group_id)
    .bind(peer_base58)
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

/// All rows whose retry cooldown has elapsed, oldest first.
pub async fn due(pool: &Pool, limit: u32) -> anyhow::Result<Vec<Row>> {
    sqlx::query_as::<_, Row>(
        "SELECT group_id, peer_base58, attempts, next_try_ms, created_at
         FROM group_key_queue
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

/// Due rows for a specific peer only (used by the Connected/Discovered
/// event fast-path for immediate redelivery).
pub async fn due_for_peer(
    pool: &Pool,
    peer_base58: &str,
    limit: u32,
) -> anyhow::Result<Vec<Row>> {
    sqlx::query_as::<_, Row>(
        "SELECT group_id, peer_base58, attempts, next_try_ms, created_at
         FROM group_key_queue
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

/// Bump the attempt counter and refresh the retry cooldown for a pair.
pub async fn bump(pool: &Pool, group_id: &str, peer_base58: &str) -> anyhow::Result<()> {
    let (a,): (i64,) = sqlx::query_as(
        "SELECT attempts FROM group_key_queue WHERE group_id = ?1 AND peer_base58 = ?2",
    )
    .bind(group_id)
    .bind(peer_base58)
    .fetch_one(pool)
    .await?;
    let _ = sqlx::query(
        "UPDATE group_key_queue SET attempts = attempts + 1, next_try_ms = ?3
         WHERE group_id = ?1 AND peer_base58 = ?2",
    )
    .bind(group_id)
    .bind(peer_base58)
    .bind(now_ms() + next_delay_ms(a))
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop one (group, member) pair — the key was delivered (ack) or the pair
/// is no longer relevant (group gone, member kicked, owner left).
pub async fn remove(pool: &Pool, group_id: &str, peer_base58: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM group_key_queue WHERE group_id = ?1 AND peer_base58 = ?2")
        .bind(group_id)
        .bind(peer_base58)
        .execute(pool)
        .await?;
    Ok(())
}

/// Drop ALL rows for a group (owner left the group / group deleted).
pub async fn remove_all(pool: &Pool, group_id: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM group_key_queue WHERE group_id = ?1")
        .bind(group_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> Pool {
        crate::open_memory().await.unwrap()
    }

    #[tokio::test]
    async fn enqueue_bump_flow() {
        let p = pool().await;
        enqueue(&p, "grp-1", "peerA").await.unwrap();

        let rows = due(&p, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group_id, "grp-1");
        assert_eq!(rows[0].attempts, 0);

        // Backoff grows monotonically and caps.
        assert!(next_delay_ms(0) < next_delay_ms(3));
        assert_eq!(next_delay_ms(99), next_delay_ms(5));

        bump(&p, "grp-1", "peerA").await.unwrap();
        // After a bump the row is no longer due (cooldown pushed out).
        assert!(due(&p, 10).await.unwrap().is_empty());

        remove(&p, "grp-1", "peerA").await.unwrap();
        assert!(due_for_peer(&p, "peerA", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn per_pair_independence_and_remove_all() {
        let p = pool().await;
        enqueue(&p, "grp-1", "A").await.unwrap();
        enqueue(&p, "grp-1", "B").await.unwrap();
        enqueue(&p, "grp-2", "A").await.unwrap();
        assert_eq!(due(&p, 10).await.unwrap().len(), 3);

        // Bumping A in grp-1 leaves A in grp-2 and B untouched.
        bump(&p, "grp-1", "A").await.unwrap();
        let due_a_grp2 = due_for_peer(&p, "A", 10).await.unwrap();
        assert_eq!(due_a_grp2.len(), 1);
        assert_eq!(due_a_grp2[0].group_id, "grp-2");

        remove_all(&p, "grp-1").await.unwrap();
        let remaining = due_for_peer(&p, "A", 10).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(due_for_peer(&p, "B", 10).await.unwrap().is_empty());
    }
}
