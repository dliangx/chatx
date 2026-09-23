use crate::Pool;
use sqlx::FromRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationType {
    Dm = 0,
    Group = 1,
}

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct Conversation {
    pub id: String,
    pub type_: i64,
    pub name: Option<String>,
    pub avatar_path: Option<String>,
    pub last_message_id: Option<String>,
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
    pub avatar_path: Option<String>,
}

pub async fn upsert(pool: &Pool, id: &str, patch: &ConversationPatch) -> anyhow::Result<Conversation> {
    let t = patch.type_.unwrap_or(0);
    sqlx::query(
        "INSERT INTO conversations (id, type, name, avatar_path)
         VALUES (?1, ?2, COALESCE(?3, ?1), ?4)
         ON CONFLICT(id) DO UPDATE SET
            type        = COALESCE(?2, type),
            name        = COALESCE(?3, name),
            avatar_path = COALESCE(?4, avatar_path)",
    )
    .bind(id)
    .bind(t)
    .bind(&patch.name)
    .bind(&patch.avatar_path)
    .execute(pool)
    .await?;
    let row = get(pool, id).await?;
    row.ok_or_else(|| anyhow::anyhow!("conversation {id} not found after upsert"))
}

pub async fn get(pool: &Pool, id: &str) -> anyhow::Result<Option<Conversation>> {
    let row = sqlx::query_as::<_, Conversation>(
        "SELECT id, type AS type_, name, avatar_path, last_message_id, last_message_preview,
                last_message_time, unread_count, is_pinned, is_muted, draft, last_sync_seq, last_read_seq
         FROM conversations WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Update the denormalized last-message fields after a new message lands.
pub async fn touch_last_message(
    pool: &Pool,
    id: &str,
    last_message_id: Option<&str>,
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

pub async fn clear_unread(pool: &Pool, id: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE conversations SET unread_count = 0 WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_pinned(pool: &Pool, id: &str, pinned: bool) -> anyhow::Result<()> {
    sqlx::query("UPDATE conversations SET is_pinned = ?2 WHERE id = ?1")
        .bind(id)
        .bind(pinned)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_muted(pool: &Pool, id: &str, muted: bool) -> anyhow::Result<()> {
    sqlx::query("UPDATE conversations SET is_muted = ?2 WHERE id = ?1")
        .bind(id)
        .bind(muted)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_draft(pool: &Pool, id: &str, draft: Option<&str>) -> anyhow::Result<()> {
    sqlx::query("UPDATE conversations SET draft = ?2 WHERE id = ?1")
        .bind(id)
        .bind(draft)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_last_read_seq(pool: &Pool, id: &str, seq: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE conversations SET last_read_seq = ?2 WHERE id = ?1")
        .bind(id)
        .bind(seq)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_last_sync_seq(pool: &Pool, id: &str, seq: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE conversations SET last_sync_seq = ?2 WHERE id = ?1")
        .bind(id)
        .bind(seq)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_all(pool: &Pool) -> anyhow::Result<Vec<Conversation>> {
    let rows = sqlx::query_as::<_, Conversation>(
        "SELECT id, type AS type_, name, avatar_path, last_message_id, last_message_preview,
                last_message_time, unread_count, is_pinned, is_muted, draft, last_sync_seq, last_read_seq
         FROM conversations
         ORDER BY is_pinned DESC, last_message_time DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn delete(pool: &Pool, id: &str) -> anyhow::Result<bool> {
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

        let a = upsert(&pool, "a", &ConversationPatch { type_: Some(0), name: None, avatar_path: None }).await.unwrap();
        assert_eq!(a.type_, 0);
        assert_eq!(a.name.as_deref(), Some("a"));

        let _ = upsert(&pool, "b", &ConversationPatch { type_: Some(1), name: Some("Team".into()), avatar_path: None }).await.unwrap();

        touch_last_message(&pool, "a", Some("m1"), "hello", base + 10, 1).await.unwrap();
        touch_last_message(&pool, "a", Some("m2"), "world", base + 20, 1).await.unwrap();
        touch_last_message(&pool, "b", Some("m9"), "hi team", base + 5, 0).await.unwrap();

        let list = list_all(&pool).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "a", "a is newer so it sorts first");
        assert_eq!(list[0].unread_count, 2);

        set_pinned(&pool, "b", true).await.unwrap();
        let list = list_all(&pool).await.unwrap();
        assert_eq!(list[0].id, "b", "pinned conversation first");

        clear_unread(&pool, "a").await.unwrap();
        assert_eq!(get(&pool, "a").await.unwrap().unwrap().unread_count, 0);
    }
}
