//! Receiver-side idempotency for inbound DM text. The sender carries a stable
//! `client_id` (its local `messages.id`) in every redelivery of the same
//! logical message, so the receiver can drop any copy after the first.
//!
//! Media rows and legacy senders (no `client_id`) are NOT covered — only the
//! `MsgKind::Dm` text path consults this table, see `core::Client::process_event`.

use crate::{now_ms, Pool};

/// Prune markers older than this so the table stays bounded even though one
/// row is inserted per unique inbound message. 7 days >> the sender's max
/// retry window (~10h over 12 attempts), so a legitimate redelivery can never
/// be pruned mid-flight and mistakenly let a duplicate through.
const PRUNE_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Record `client_id` as seen. Returns `true` if this was the FIRST time we've
/// seen it (caller: persist the message), `false` if it was already recorded
/// (caller: drop the message as a duplicate redelivery).
pub async fn mark(pool: &Pool, client_id: i64, peer_base58: &str) -> anyhow::Result<bool> {
    let res = sqlx::query(
        "INSERT OR IGNORE INTO dm_inbound_seen (client_id, peer_base58, seen_at_ms)
         VALUES (?1, ?2, ?3)",
    )
    .bind(client_id)
    .bind(peer_base58)
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Drop markers far past the retention window. Called opportunistically from
/// [`mark`]; the sweep is cheap (indexed by PK range) and self-limiting.
pub async fn prune(pool: &Pool) -> anyhow::Result<u64> {
    let cutoff = now_ms().saturating_sub(PRUNE_RETENTION_MS);
    let n = sqlx::query("DELETE FROM dm_inbound_seen WHERE seen_at_ms < ?1")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(n.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> Pool {
        crate::open_memory().await.unwrap()
    }

    #[tokio::test]
    async fn first_mark_is_fresh_repeat_is_not() {
        let p = pool().await;
        assert!(mark(&p, 100, "peerA").await.unwrap(), "first sight is fresh");
        assert!(!mark(&p, 100, "peerA").await.unwrap(), "repeat is a dup");
        assert!(!mark(&p, 100, "peerA").await.unwrap(), "third is also a dup");
    }

    #[tokio::test]
    async fn same_client_id_from_different_peers_is_not_a_dup() {
        let p = pool().await;
        // Two distinct senders happen to share the same wall-clock client_id.
        // They are different logical conversations and both must be persisted.
        assert!(mark(&p, 55, "peerA").await.unwrap());
        assert!(mark(&p, 55, "peerB").await.unwrap(), "different peer keeps its own message");
    }

    #[tokio::test]
    async fn distinct_client_ids_are_all_fresh() {
        let p = pool().await;
        for id in [1, 2, 3, 1000] {
            assert!(mark(&p, id, "peerA").await.unwrap(), "{id} is a new message");
        }
    }
}
