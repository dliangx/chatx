use crate::Pool;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct MomentSync {
    pub peer_id: String,
    pub last_sync_ts: i64,
}

pub async fn read(pool: &Pool, peer_id: &str) -> anyhow::Result<Option<i64>> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT last_sync_ts FROM moment_sync WHERE peer_id = ?1")
            .bind(peer_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(t,)| t))
}

pub async fn upsert(pool: &Pool, peer_id: &str, ts: i64) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO moment_sync (peer_id, last_sync_ts)
         VALUES (?1, ?2)
         ON CONFLICT(peer_id) DO UPDATE SET last_sync_ts = ?2",
    )
    .bind(peer_id)
    .bind(ts)
    .execute(pool)
    .await?;
    Ok(())
}

/// All rows (the sync loop reads everyone's cursor at once).
pub async fn all(pool: &Pool) -> anyhow::Result<Vec<MomentSync>> {
    let rows =
        sqlx::query_as::<_, MomentSync>("SELECT * FROM moment_sync ORDER BY peer_id")
            .fetch_all(pool)
            .await?;
    Ok(rows)
}

pub async fn delete(pool: &Pool, peer_id: &str) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM moment_sync WHERE peer_id = ?1")
        .bind(peer_id)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    #[tokio::test]
    async fn upsert_and_read_roundtrip() {
        let pool = open_memory().await.unwrap();
        assert!(read(&pool, "A").await.unwrap().is_none());
        upsert(&pool, "A", 1000).await.unwrap();
        assert_eq!(read(&pool, "A").await.unwrap(), Some(1000));
        upsert(&pool, "A", 2000).await.unwrap();
        assert_eq!(read(&pool, "A").await.unwrap(), Some(2000));
        let all = all(&pool).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].peer_id, "A");
        assert!(delete(&pool, "A").await.unwrap());
        assert!(read(&pool, "A").await.unwrap().is_none());
    }
}
