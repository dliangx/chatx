//! Client-side DM offline queue. Each row is an outgoing DM (text, voice,
//! image or file) whose local `messages` row is still `status = sending` —
//! i.e. the peer never acked — pending a redelivery attempt.
//!
//! The redelivery task in `core::Client` is **passive**: it only fires a
//! queued row when the peer is already inbound-connected, and defers to the
//! peer's `Connected` fast-path otherwise. Failure of a single attempt merely
//! advances a per-pair **cooldown** ([`bump`]) so a half-dead link is not
//! re-hit every tick. Termination is by **deadline** only ([`DM_DEADLINE_MS`],
//! swept by [`sweep_expired`] → local row `status = failed`), not by an
//! attempt budget: in a passive model the peer may be offline for days and
//! still valid, so a count cap would give up too early.

use crate::{now_ms, Pool};
use sqlx::FromRow;

/// Per-attempt backoff schedule (seconds), capped at the last entry. A failed
/// attempt waits this long before the next; the cap (1h) keeps a wedged link
/// from being re-hit every redelivery tick (~1.5s) without ever giving up.
const BACKOFF_SECS: [i64; 7] = [5, 15, 60, 300, 900, 1800, 3600];

#[derive(Debug, FromRow, Clone)]
pub struct Row {
    pub msg_id: i64,
    pub peer_base58: String,
    pub chat_id: String,
    pub attempts: i64,
    pub next_try_ms: i64,
    pub created_at: i64,
    /// Hard expiry epoch (ms). After this [`sweep_expired`] drops the row and
    /// flips the local `messages` row to `FAILED`. `None` = legacy rows from
    /// before deadlines existed; those fall back to `created_at + DM_DEADLINE_MS`
    /// in the sweep. A nullable column maps naturally to `Option<i64>` via
    /// `FromRow`.
    pub deadline_ms: Option<i64>,
}

/// Hard expiry window (ms) for queued DM rows — text **and** media. After this
/// the redelivery sweep drops the row and flips the local `messages` row to
/// `FAILED`. Delivery is passive (we only send while the peer is connected),
/// so a peer that stays offline past the deadline is considered unreachable.
pub const DM_DEADLINE_MS: i64 = 3 * 24 * 60 * 60 * 1000; // 3 days

pub fn next_delay_ms(attempts: i64) -> i64 {
    let idx = (attempts as usize).min(BACKOFF_SECS.len() - 1);
    BACKOFF_SECS[idx] * 1000
}

/// Register `msg_id` as awaiting delivery. `deadline_ms` is a millisecond
/// epoch (swept to `FAILED` after it elapses — see [`DM_DEADLINE_MS`]); pass
/// `None` for legacy rows, which fall back to a `created_at`-based 3-day
/// window in [`sweep_expired`]. New enqueue sites always pass a deadline.
/// INSERT OR REPLACE makes re-enqueue idempotent (redeliver loop racing a
/// fresh `send_dm`).
pub async fn enqueue(
    pool: &Pool,
    msg_id: i64,
    peer_base58: &str,
    chat_id: &str,
    deadline_ms: Option<i64>,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO dm_send_queue
         (msg_id, peer_base58, chat_id, attempts, next_try_ms, created_at, deadline_ms)
         VALUES (?1, ?2, ?3, 0, ?4, ?4, ?5)",
    )
    .bind(msg_id)
    .bind(peer_base58)
    .bind(chat_id)
    .bind(now_ms())
    .bind(deadline_ms)
    .execute(pool)
    .await?;
    Ok(())
}

/// All rows whose retry cooldown has elapsed, oldest first.
pub async fn due(pool: &Pool, limit: u32) -> anyhow::Result<Vec<Row>> {
    sqlx::query_as::<_, Row>(
        "SELECT msg_id, peer_base58, chat_id, attempts, next_try_ms, created_at, deadline_ms
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

/// Drop all expired queue rows and return the distinct `msg_id`s the caller
/// should flip to status `FAILED`. A row is expired when its `deadline_ms`
/// has elapsed, or when `deadline_ms IS NULL` and its `created_at` is older
/// than [`DM_DEADLINE_MS`] (legacy text rows enqueued before deadlines were
/// added — otherwise they would live forever). Idempotent — the DELETE is the
/// authoritative action, and any subsequent redelivery for a swept row finds
/// no local message to replay (the app's `FAILED` badge persists).
pub async fn sweep_expired(pool: &Pool) -> anyhow::Result<Vec<i64>> {
    // Find the ids first so we can report them back, then delete. The
    // two-step is intentional: `DELETE ... RETURNING` is not available in
    // SQLite < 3.35, and we want a single deterministic return shape.
    let now = now_ms();
    let legacy_cutoff = now.saturating_sub(DM_DEADLINE_MS);
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT msg_id FROM dm_send_queue
         WHERE (deadline_ms IS NOT NULL AND deadline_ms <= ?1)
            OR (deadline_ms IS NULL AND created_at <= ?2)",
    )
    .bind(now)
    .bind(legacy_cutoff)
    .fetch_all(pool)
    .await?;
    if !ids.is_empty() {
        let _ = sqlx::query(
            "DELETE FROM dm_send_queue
             WHERE (deadline_ms IS NOT NULL AND deadline_ms <= ?1)
                OR (deadline_ms IS NULL AND created_at <= ?2)",
        )
        .bind(now)
        .bind(legacy_cutoff)
        .execute(pool)
        .await?;
    }
    Ok(ids)
}

/// Refresh the retry cooldown for a (msg, device) pair after a failed attempt.
/// Advances `next_try_ms` by the next backoff step so the pair is not due on
/// the very next redelivery tick — but never gives up; termination is left to
/// the [`DM_DEADLINE_MS`] sweep. Per-device counters stay independent so a
/// flaky device does not starve a fast one.
///
/// Returns the attempt count AFTER this bump (for logging), or `0` if the
/// pair was already removed from the queue (nothing to cool down).
pub async fn bump(pool: &Pool, msg_id: i64, peer_base58: &str) -> anyhow::Result<i64> {
    let delay = {
        let (a,): (i64,) =
            sqlx::query_as("SELECT attempts FROM dm_send_queue WHERE msg_id = ?1 AND peer_base58 = ?2")
                .bind(msg_id)
                .bind(peer_base58)
                .fetch_one(pool)
                .await?;
        next_delay_ms(a)
    };
    let n = sqlx::query(
        "UPDATE dm_send_queue SET attempts = attempts + 1, next_try_ms = ?3
          WHERE msg_id = ?1 AND peer_base58 = ?2",
    )
    .bind(msg_id)
    .bind(peer_base58)
    .bind(now_ms() + delay)
    .execute(pool)
    .await?;
    if n.rows_affected() == 0 {
        return Ok(0); // pair removed between SELECT and UPDATE — nothing to cool down
    }
    let (a,): (i64,) =
        sqlx::query_as("SELECT attempts FROM dm_send_queue WHERE msg_id = ?1 AND peer_base58 = ?2")
            .bind(msg_id)
            .bind(peer_base58)
            .fetch_one(pool)
            .await?;
    Ok(a)
}

/// Drop ONE (msg, device) pair from the queue after that device has either
/// acked (delivered) or exhausted its attempt budget. Other devices for the
/// same message are untouched and keep their own redelivery schedule.
pub async fn give_up_pair(pool: &Pool, msg_id: i64, peer_base58: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM dm_send_queue WHERE msg_id = ?1 AND peer_base58 = ?2")
        .bind(msg_id)
        .bind(peer_base58)
        .execute(pool)
        .await?;
    Ok(())
}

/// Drop ALL queue rows for a message. Used when the local `messages` row
/// itself is being removed or when a caller knows no device will ever get
/// this message (e.g. budget exhausted on every device at once).
pub async fn give_up(pool: &Pool, msg_id: i64) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM dm_send_queue WHERE msg_id = ?1")
        .bind(msg_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The set of (msg_id, peer_base58) pairs still pending delivery for a given
/// message id. Empty when the message is fully delivered (or has been
/// given up on every device).
pub async fn pending_peers_for_msg(
    pool: &Pool,
    msg_id: i64,
) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT peer_base58 FROM dm_send_queue WHERE msg_id = ?1")
        .bind(msg_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(p,)| p).collect())
}

/// All pending rows for a specific peer, **ignoring the retry cooldown**.
/// Used by the `Connected`/`Discovered` fast-path: the peer just came online,
/// which is the one moment a passive redelivery can succeed, so we must drain
/// it immediately even if the pair is mid-backoff. (The polling loop uses
/// [`due`], which honours the cooldown, to keep a half-dead link quiet.)
pub async fn due_for_peer(pool: &Pool, peer_base58: &str, limit: u32) -> anyhow::Result<Vec<Row>> {
    sqlx::query_as::<_, Row>(
        "SELECT msg_id, peer_base58, chat_id, attempts, next_try_ms, created_at, deadline_ms
         FROM dm_send_queue
         WHERE peer_base58 = ?1
         ORDER BY created_at ASC
         LIMIT ?2",
    )
    .bind(peer_base58)
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

/// Number of undelivered DMs pending to a specific peer, regardless of when
/// they become due. Used to decide whether a dropped peer's reconnect is worth
/// proactively pursuing (a peer with no pending DMs is not one we're waiting
/// on, and re-dialing a disconnected group member would fight `stop()` semantics).
pub async fn pending_count_for_peer(pool: &Pool, peer_base58: &str) -> anyhow::Result<u32> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dm_send_queue WHERE peer_base58 = ?1")
        .bind(peer_base58)
        .fetch_one(pool)
        .await?;
    Ok(n as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> Pool {
        crate::open_memory().await.unwrap()
    }

    #[tokio::test]
    async fn enqueue_bump_backoff_flow() {
        let p = pool().await;
        enqueue(&p, 1, "peerA", "chatA", None).await.unwrap();

        let rows = due(&p, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].msg_id, 1);
        assert_eq!(rows[0].attempts, 0);

        // Backoff grows monotonically and is capped.
        assert!(next_delay_ms(0) < next_delay_ms(3));
        assert_eq!(next_delay_ms(99), next_delay_ms(6));

        // Bumping advances the cooldown: after a bump the row is no longer due
        // (next_try_ms pushed into the future), and the counter grows without
        // any hard cap — termination is the deadline sweep's job, not the
        // attempt counter's.
        let first = bump(&p, 1, "peerA").await.unwrap();
        assert_eq!(first, 1);
        assert!(due(&p, 10).await.unwrap().is_empty());
        for _ in 0..30 {
            let _ = bump(&p, 1, "peerA").await.unwrap();
        }
        let last: (i64,) =
            sqlx::query_as("SELECT attempts FROM dm_send_queue WHERE msg_id = 1")
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(last.0, 31); // 31 bumps, still present — no exhaustion
        give_up(&p, 1).await.unwrap();
        assert!(due(&p, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn per_device_retry_is_independent() {
        let p = pool().await;
        // One message delivered to two devices: each has its own queue row.
        enqueue(&p, 1, "B", "dm:alice", None).await.unwrap();
        enqueue(&p, 1, "C", "dm:alice", None).await.unwrap();
        assert_eq!(due(&p, 10).await.unwrap().len(), 2);

        // Bumping B must NOT move C's row forward (per-pair independence).
        // We query the table directly because `bump` pushes the bumped pair's
        // `next_try_ms` into the future (so `due` hides it).
        let a = bump(&p, 1, "B").await.unwrap();
        assert_eq!(a, 1);
        let after: Vec<(String, i64)> = sqlx::query_as(
            "SELECT peer_base58, attempts
               FROM dm_send_queue WHERE msg_id = 1 ORDER BY peer_base58",
        )
        .fetch_all(&p)
        .await
        .unwrap();
        assert_eq!(after, vec![("B".into(), 1), ("C".into(), 0)]);

        // Giving up B removes ONLY B's row; C's pair keeps its own schedule.
        give_up_pair(&p, 1, "B").await.unwrap();
        let remaining = pending_peers_for_msg(&p, 1).await.unwrap();
        assert_eq!(remaining, vec!["C".to_string()]);

        // All pending now → the caller flips the local message row to SENT.
        give_up_pair(&p, 1, "C").await.unwrap();
        assert!(pending_peers_for_msg(&p, 1).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn due_for_peer_filters_by_peer() {
        let p = pool().await;
        enqueue(&p, 10, "peerX", "cX", None).await.unwrap();
        enqueue(&p, 11, "peerY", "cY", None).await.unwrap();

        let only_x = due_for_peer(&p, "peerX", 10).await.unwrap();
        assert_eq!(only_x.len(), 1);
        assert_eq!(only_x[0].msg_id, 10);

        let none = due_for_peer(&p, "peerMissing", 10).await.unwrap();
        assert!(none.is_empty());

        // enqueue is idempotent (re-sending the same message doesn't dup)
        enqueue(&p, 10, "peerX", "cX", None).await.unwrap();
        assert_eq!(due(&p, 100).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn sweep_expired_covers_legacy_null_deadline_rows() {
        let p = pool().await;
        // A legacy row: deadline_ms IS NULL, created just now.
        enqueue(&p, 100, "peerX", "cX", None).await.unwrap();
        // A fresh row WITH a deadline (the normal path).
        let deadline = crate::now_ms() + DM_DEADLINE_MS;
        enqueue(&p, 101, "peerX", "cX", Some(deadline)).await.unwrap();

        // Neither should be swept yet.
        assert!(sweep_expired(&p).await.unwrap().is_empty());

        // Age the legacy row past the 3-day window (backdate created_at).
        let old = crate::now_ms() - DM_DEADLINE_MS - 1000;
        let _ = sqlx::query(
            "UPDATE dm_send_queue SET created_at = ?3 WHERE msg_id = 100 AND peer_base58 = 'peerX'",
        )
        .bind(100)
        .bind("peerX")
        .bind(old)
        .execute(&p)
        .await
        .unwrap();

        // Now the legacy row is expired (and so is its sibling), the fresh
        // deadline row is not.
        let swept = sweep_expired(&p).await.unwrap();
        assert!(sweep_expired(&p).await.unwrap().is_empty());
        let swept_set: std::collections::HashSet<i64> = swept.into_iter().collect();
        assert!(swept_set.contains(&100), "legacy NULL-deadline row must be swept");
        assert!(!swept_set.contains(&101), "deadline not yet reached must be kept");
    }
}
