use crate::Pool;
use sqlx::FromRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationType {
    Dm = 0,
    Group = 1,
}

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct Conversation {
    pub id: i64,
    pub type_: i64,
    pub name: Option<String>,
    pub peer_id: Option<String>,
    pub avatar_path: Option<String>,
    pub last_message_id: Option<i64>,
    pub last_message_preview: Option<String>,
    pub last_message_time: Option<i64>,
    pub unread_count: i64,
    pub is_pinned: bool,
    pub is_muted: bool,
    pub draft: Option<String>,
    pub last_sync_seq: i64,
    pub last_read_seq: i64,
}

#[derive(Debug, Default, Clone)]
pub struct ConversationPatch {
    pub type_: Option<i64>,
    pub name: Option<String>,
    pub peer_id: Option<String>,
    pub avatar_path: Option<String>,
}

pub async fn upsert(pool: &Pool, id: i64, patch: &ConversationPatch) -> anyhow::Result<Conversation> {
    let t = patch.type_.unwrap_or(0);
    sqlx::query(
        "INSERT INTO conversations (id, type, name, peer_id, avatar_path)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
            type        = COALESCE(?2, type),
            name        = COALESCE(?3, name),
            peer_id     = COALESCE(?4, peer_id),
            avatar_path = COALESCE(?5, avatar_path)",
    )
    .bind(id)
    .bind(t)
    .bind(&patch.name)
    .bind(patch.peer_id.as_deref())
    .bind(&patch.avatar_path)
    .execute(pool)
    .await?;
    let row = get(pool, id).await?;
    row.ok_or_else(|| anyhow::anyhow!("conversation {id} not found after upsert"))
}

pub async fn get(pool: &Pool, id: i64) -> anyhow::Result<Option<Conversation>> {
    let row = sqlx::query_as::<_, Conversation>(
        "SELECT id, type AS type_, name, peer_id, avatar_path, last_message_id, last_message_preview,
                last_message_time, unread_count, is_pinned, is_muted, draft, last_sync_seq, last_read_seq
         FROM conversations WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Resolve the integer id of a conversation keyed by its text `name`
/// (the legacy message shim stores the string chat id in `name`, for both
/// DMs and groups).
pub async fn resolve_id(pool: &Pool, chat_id: &str) -> anyhow::Result<Option<i64>> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM conversations WHERE name = ?1")
            .bind(chat_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(id,)| id))
}

/// Find-or-create a DM conversation keyed by its text `name`.
pub async fn ensure_dm(pool: &Pool, chat_id: &str) -> anyhow::Result<i64> {
    if let Some(id) = resolve_id(pool, chat_id).await? {
        return Ok(id);
    }
    let id = crate::new_id();
    sqlx::query("INSERT INTO conversations (id, type, name) VALUES (?1, 0, ?2)")
        .bind(id)
        .bind(chat_id)
        .execute(pool)
        .await?;
    Ok(id)
}

/// Record the DM peer's identity (libp2p PeerId / username) if not already set.
pub async fn set_peer_id(pool: &Pool, id: i64, peer_id: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE conversations SET peer_id = COALESCE(peer_id, ?2) WHERE id = ?1")
        .bind(id)
        .bind(peer_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Update the denormalized last-message fields after a new message lands.
pub async fn touch_last_message(
    pool: &Pool,
    id: i64,
    last_message_id: Option<i64>,
    preview: &str,
    ts: i64,
    unread_increment: i64,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE conversations SET
            last_message_id   = COALESCE(?2, last_message_id),
            last_message_preview = ?3,
            last_message_time = MAX(COALESCE(last_message_time, 0), ?4),
            unread_count      = MAX(0, unread_count + ?5)
         WHERE id = ?1",
    )
    .bind(id)
    .bind(last_message_id)
    .bind(preview)
    .bind(ts)
    .bind(unread_increment)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn clear_unread(pool: &Pool, id: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE conversations SET unread_count = 0 WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_pinned(pool: &Pool, id: i64, pinned: bool) -> anyhow::Result<()> {
    sqlx::query("UPDATE conversations SET is_pinned = ?2 WHERE id = ?1")
        .bind(id)
        .bind(pinned)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_muted(pool: &Pool, id: i64, muted: bool) -> anyhow::Result<()> {
    sqlx::query("UPDATE conversations SET is_muted = ?2 WHERE id = ?1")
        .bind(id)
        .bind(muted)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_draft(pool: &Pool, id: i64, draft: Option<&str>) -> anyhow::Result<()> {
    sqlx::query("UPDATE conversations SET draft = ?2 WHERE id = ?1")
        .bind(id)
        .bind(draft)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_last_read_seq(pool: &Pool, id: i64, seq: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE conversations SET last_read_seq = ?2 WHERE id = ?1")
        .bind(id)
        .bind(seq)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_last_sync_seq(pool: &Pool, id: i64, seq: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE conversations SET last_sync_seq = ?2 WHERE id = ?1")
        .bind(id)
        .bind(seq)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_all(pool: &Pool) -> anyhow::Result<Vec<Conversation>> {
    let rows = sqlx::query_as::<_, Conversation>(
        "SELECT id, type AS type_, name, peer_id, avatar_path, last_message_id, last_message_preview,
                last_message_time, unread_count, is_pinned, is_muted, draft, last_sync_seq, last_read_seq
         FROM conversations
         ORDER BY is_pinned DESC, last_message_time DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn delete(pool: &Pool, id: i64) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM conversations WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{now_ms, open_memory};

    #[tokio::test]
    async fn upsert_touch_and_order() {
        let pool = open_memory().await.unwrap();
        let base = now_ms();

        let a = upsert(&pool, 1, &ConversationPatch { type_: Some(0), name: Some("a".into()), peer_id: None, avatar_path: None }).await.unwrap();
        assert_eq!(a.type_, 0);
        assert_eq!(a.name.as_deref(), Some("a"));

        let _ = upsert(&pool, 2, &ConversationPatch { type_: Some(1), name: Some("Team".into()), peer_id: None, avatar_path: None }).await.unwrap();

        touch_last_message(&pool, 1, Some(1001), "hello", base + 10, 1).await.unwrap();
        touch_last_message(&pool, 1, Some(1002), "world", base + 20, 1).await.unwrap();
        touch_last_message(&pool, 2, Some(1009), "hi team", base + 5, 0).await.unwrap();

        let list = list_all(&pool).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, 1, "1 is newer so it sorts first");
        assert_eq!(list[0].unread_count, 2);

        set_pinned(&pool, 2, true).await.unwrap();
        let list = list_all(&pool).await.unwrap();
        assert_eq!(list[0].id, 2, "pinned conversation first");

        clear_unread(&pool, 1).await.unwrap();
        assert_eq!(get(&pool, 1).await.unwrap().unwrap().unread_count, 0);
    }
}
