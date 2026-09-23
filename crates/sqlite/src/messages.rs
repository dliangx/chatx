use crate::conversations;
use crate::{new_id, now_ms, MsgRow, Pool};
use sqlx::FromRow;

/// 0=text, 1=image, 2=voice, 3=video, 4=file, 5=system
pub const MSG_TYPE_TEXT: i64 = 0;
pub const MSG_TYPE_IMAGE: i64 = 1;
pub const MSG_TYPE_VOICE: i64 = 2;
pub const MSG_TYPE_VIDEO: i64 = 3;
pub const MSG_TYPE_FILE: i64 = 4;
pub const MSG_TYPE_SYSTEM: i64 = 5;

/// 0=sending, 1=sent, 2=delivered, 3=read, 4=failed
pub const STATUS_SENDING: i64 = 0;
pub const STATUS_SENT: i64 = 1;
pub const STATUS_DELIVERED: i64 = 2;
pub const STATUS_READ: i64 = 3;
pub const STATUS_FAILED: i64 = 4;

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub sender_id: String,
    pub msg_type: i64,
    pub text_content: Option<String>,
    pub media_path: Option<String>,
    pub media_size: Option<i64>,
    pub media_duration: Option<i64>,
    pub thumbnail_path: Option<String>,
    pub timestamp: i64,
    pub status: i64,
    pub reply_to: Option<String>,
    pub is_encrypted: bool,
    pub sync_seq: Option<i64>,
    pub mentions: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct NewMessage {
    pub id: Option<String>,
    pub conversation_id: String,
    pub sender_id: String,
    pub msg_type: i64,
    pub text_content: Option<String>,
    pub media_path: Option<String>,
    pub media_size: Option<i64>,
    pub media_duration: Option<i64>,
    pub thumbnail_path: Option<String>,
    pub timestamp: Option<i64>,
    pub status: Option<i64>,
    pub reply_to: Option<String>,
    pub is_encrypted: bool,
    pub sync_seq: Option<i64>,
    pub mentions: Option<String>,
}

impl NewMessage {
    pub fn text(conversation_id: impl Into<String>, sender: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            sender_id: sender.into(),
            msg_type: MSG_TYPE_TEXT,
            text_content: Some(text.into()),
            ..Default::default()
        }
    }
}

/// Insert a message and bump the owning conversation's denormalized fields.
pub async fn insert(pool: &Pool, m: &NewMessage) -> anyhow::Result<String> {
    let id = m.id.clone().unwrap_or_else(|| new_id("m"));
    let ts = m.timestamp.unwrap_or_else(now_ms);
    let status = m.status.unwrap_or(STATUS_SENT);

    let mut trans = pool.begin().await?;
    {
        let unread_bump = match m.msg_type {
            MSG_TYPE_SYSTEM => 0,
            _ => 1,
        };
        sqlx::query(
            "INSERT INTO conversations (id, type, name, last_message_time, unread_count)
             VALUES (?1, 0, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                last_message_time = MAX(COALESCE(last_message_time, 0), ?2),
                unread_count      = MAX(0, unread_count + ?3)",
        )
        .bind(&m.conversation_id)
        .bind(ts)
        .bind(unread_bump)
        .execute(trans.as_mut())
        .await?;

        sqlx::query(
            "UPDATE conversations SET
                last_message_id   = ?1,
                last_message_preview = ?2
             WHERE id = ?3",
        )
        .bind(&id)
        .bind(m.text_content.as_deref().unwrap_or_default())
        .bind(&m.conversation_id)
        .execute(trans.as_mut())
        .await?;

        sqlx::query(
            "INSERT INTO messages
                (id, conversation_id, sender_id, msg_type, text_content, media_path, media_size,
                 media_duration, thumbnail_path, timestamp, status, reply_to, is_encrypted, sync_seq, mentions)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )
        .bind(&id)
        .bind(&m.conversation_id)
        .bind(&m.sender_id)
        .bind(m.msg_type)
        .bind(&m.text_content)
        .bind(&m.media_path)
        .bind(m.media_size)
        .bind(m.media_duration)
        .bind(&m.thumbnail_path)
        .bind(ts)
        .bind(status)
        .bind(&m.reply_to)
        .bind(m.is_encrypted)
        .bind(m.sync_seq)
        .bind(&m.mentions)
        .execute(trans.as_mut())
        .await?;
    }
    trans.commit().await?;
    Ok(id)
}

/// Legacy shim over [`insert`] preserving the old `MsgRow` fields,
/// used by `chatx-core` for its outgoing/inbound text messages.
pub async fn insert_row(pool: &Pool, m: &MsgRow) -> anyhow::Result<()> {
    let preview: String = if m.text.chars().count() > 64 {
        let mut out: String = m.text.chars().take(64).collect();
        out.push('…');
        out
    } else {
        m.text.clone()
    };
    insert(
        pool,
        &NewMessage {
            conversation_id: m.chat_id.clone(),
            sender_id: m.sender.clone(),
            msg_type: MSG_TYPE_TEXT,
            text_content: Some(m.text.clone()),
            timestamp: Some(m.t as i64),
            is_encrypted: m.sealed,
            ..Default::default()
        },
    )
    .await?;
    let _ = preview;
    Ok(())
}

pub async fn get(pool: &Pool, id: &str) -> anyhow::Result<Option<Message>> {
    let row = sqlx::query_as::<_, Message>("SELECT * FROM messages WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn load_all(pool: &Pool, chat_id: &str) -> anyhow::Result<Vec<Message>> {
    let rows = sqlx::query_as::<_, Message>(
        "SELECT * FROM messages WHERE conversation_id = ?1 ORDER BY timestamp DESC",
    )
    .bind(chat_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn load_page(pool: &Pool, chat_id: &str, limit: u32, offset: u32) -> anyhow::Result<Vec<Message>> {
    let rows = sqlx::query_as::<_, Message>(
        "SELECT * FROM messages WHERE conversation_id = ?1
         ORDER BY timestamp DESC LIMIT ?2 OFFSET ?3",
    )
    .bind(chat_id)
    .bind(limit as i64)
    .bind(offset as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Messages newer than `after_timestamp` (exclusive), ordered ascending.
pub async fn load_since(pool: &Pool, chat_id: &str, after_timestamp: i64, limit: u32) -> anyhow::Result<Vec<Message>> {
    let rows = sqlx::query_as::<_, Message>(
        "SELECT * FROM messages
         WHERE conversation_id = ?1 AND timestamp > ?2
         ORDER BY timestamp ASC
         LIMIT ?3",
    )
    .bind(chat_id)
    .bind(after_timestamp)
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn count(pool: &Pool, chat_id: &str) -> anyhow::Result<u32> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM messages WHERE conversation_id = ?1")
        .bind(chat_id)
        .fetch_one(pool)
        .await?;
    Ok(n as u32)
}

pub async fn delete(pool: &Pool, id: &str) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM messages WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

pub async fn delete_before(pool: &Pool, chat_id: &str, before_t: u64) -> anyhow::Result<usize> {
    let n = sqlx::query("DELETE FROM messages WHERE conversation_id = ?1 AND timestamp < ?2")
        .bind(chat_id)
        .bind(before_t as i64)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() as usize)
}

pub async fn set_status(pool: &Pool, id: &str, status: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE messages SET status = ?2 WHERE id = ?1")
        .bind(id)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}

/// Convenience wrappers that forward to `conversations` for ergonomics.
pub async fn clear_unread_for(pool: &Pool, chat_id: &str) -> anyhow::Result<()> {
    conversations::clear_unread(pool, chat_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;
    use crate::MsgRow as LegacyRow;

    #[tokio::test]
    async fn insert_and_page_ordering() {
        let pool = open_memory().await.unwrap();
        let base = now_ms();
        for (i, text) in ["old", "mid", "new"].into_iter().enumerate() {
            insert_row(
                &pool,
                &LegacyRow {
                    chat_id: "c".into(),
                    sender: "s".into(),
                    text: text.into(),
                    sealed: false,
                    t: (base + i as i64) as u64,
                },
            )
            .await
            .unwrap();
        }

        assert_eq!(count(&pool, "c").await.unwrap(), 3);

        let all = load_all(&pool, "c").await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].text_content.as_deref(), Some("new"));
        assert_eq!(all[2].text_content.as_deref(), Some("old"));

        let p0 = load_page(&pool, "c", 2, 0).await.unwrap();
        assert_eq!(p0.len(), 2);
        assert_eq!(p0[0].text_content.as_deref(), Some("new"));

        let p1 = load_page(&pool, "c", 2, 2).await.unwrap();
        assert_eq!(p1.len(), 1);

        let removed = delete_before(&pool, "c", (base + 2) as u64).await.unwrap();
        assert_eq!(removed, 2);
        assert_eq!(count(&pool, "c").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn insert_bumps_conversation_unread() {
        let pool = open_memory().await.unwrap();
        insert_row(
            &pool,
            &LegacyRow {
                chat_id: "dm".into(),
                sender: "a".into(),
                text: "hi".into(),
                sealed: false,
                t: now_ms() as u64,
            },
        )
        .await
        .unwrap();
        let conv = conversations::get(&pool, "dm").await.unwrap().unwrap();
        assert_eq!(conv.unread_count, 1);
        assert_eq!(conv.name.as_deref(), Some("dm"));
        conversations::clear_unread(&pool, "dm").await.unwrap();
        assert_eq!(conversations::get(&pool, "dm").await.unwrap().unwrap().unread_count, 0);
    }
}
