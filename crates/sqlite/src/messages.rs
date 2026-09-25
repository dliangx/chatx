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
    pub id: i64,
    pub conversation_id: i64,
    pub sender_id: i64,
    pub msg_type: i64,
    pub text_content: Option<String>,
    pub media_path: Option<String>,
    pub media_size: Option<i64>,
    pub media_duration: Option<i64>,
    pub thumbnail_path: Option<String>,
    pub timestamp: i64,
    pub status: i64,
    pub reply_to: Option<i64>,
    pub is_encrypted: bool,
    pub sync_seq: Option<i64>,
    pub mentions: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct NewMessage {
    pub id: Option<i64>,
    pub conversation_id: i64,
    pub sender_id: i64,
    pub msg_type: i64,
    pub text_content: Option<String>,
    pub media_path: Option<String>,
    pub media_size: Option<i64>,
    pub media_duration: Option<i64>,
    pub thumbnail_path: Option<String>,
    pub timestamp: Option<i64>,
    pub status: Option<i64>,
    pub reply_to: Option<i64>,
    pub is_encrypted: bool,
    pub sync_seq: Option<i64>,
    pub mentions: Option<String>,
    /// True when this message was sent by the local user (does not bump unread).
    pub from_me: bool,
}

impl NewMessage {
    pub fn text(conversation_id: i64, sender: i64, text: impl Into<String>) -> Self {
        Self {
            conversation_id,
            sender_id: sender,
            msg_type: MSG_TYPE_TEXT,
            text_content: Some(text.into()),
            ..Default::default()
        }
    }
}

/// Insert a message and bump the owning conversation's denormalized fields.
pub async fn insert(pool: &Pool, m: &NewMessage) -> anyhow::Result<i64> {
    let id = m.id.unwrap_or_else(new_id);
    let ts = m.timestamp.unwrap_or_else(now_ms);
    let status = m.status.unwrap_or(STATUS_SENT);

    let mut trans = pool.begin().await?;
    {
        let unread_bump = if m.from_me || m.msg_type == MSG_TYPE_SYSTEM {
            0
        } else {
            1
        };
        sqlx::query(
            "INSERT INTO conversations (id, type, last_message_time, unread_count)
             VALUES (?1, 0, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                last_message_time = MAX(COALESCE(last_message_time, 0), ?2),
                unread_count      = MAX(0, unread_count + ?3)",
        )
        .bind(m.conversation_id)
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
        .bind(id)
        .bind(m.text_content.as_deref().unwrap_or_default())
        .bind(m.conversation_id)
        .execute(trans.as_mut())
        .await?;

        sqlx::query(
            "INSERT INTO messages
                (id, conversation_id, sender_id, msg_type, text_content, media_path, media_size,
                 media_duration, thumbnail_path, timestamp, status, reply_to, is_encrypted, sync_seq, mentions)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )
        .bind(id)
        .bind(m.conversation_id)
        .bind(m.sender_id)
        .bind(m.msg_type)
        .bind(&m.text_content)
        .bind(&m.media_path)
        .bind(m.media_size)
        .bind(m.media_duration)
        .bind(&m.thumbnail_path)
        .bind(ts)
        .bind(status)
        .bind(m.reply_to)
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
///
/// The string `chat_id`/`sender` are mapped onto integer `conversations.id` /
/// `users.id` via `conversations::ensure_dm` / `users::ensure_identity`.
pub async fn insert_row(pool: &Pool, m: &MsgRow) -> anyhow::Result<()> {
    let conversation_id = conversations::ensure_dm(pool, &m.chat_id).await?;
    let sender_id = crate::devices::ensure_user_by_peer(pool, &m.sender).await?;
    // A message from the peer reveals the DM's peer identity.
    if !m.mine {
        conversations::set_peer_id(pool, conversation_id, &m.sender).await?;
    }
    insert(
        pool,
        &NewMessage {
            conversation_id,
            sender_id,
            msg_type: MSG_TYPE_TEXT,
            text_content: Some(m.text.clone()),
            timestamp: Some(m.t as i64),
            is_encrypted: m.sealed,
            from_me: m.mine,
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

pub async fn get(pool: &Pool, id: i64) -> anyhow::Result<Option<Message>> {
    let row = sqlx::query_as::<_, Message>("SELECT * FROM messages WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Legacy shim: load all messages for a string chat id, resolving the sender's
/// text identity back from `users`.
pub async fn load_all(pool: &Pool, chat_id: &str) -> anyhow::Result<Vec<MsgRow>> {
    let Some(conversation_id) = conversations::resolve_id(pool, chat_id).await? else {
        return Ok(Vec::new());
    };
    load_rows(pool, conversation_id, chat_id, None, None).await
}

/// Legacy shim: paged load for a string chat id.
pub async fn load_page(pool: &Pool, chat_id: &str, limit: u32, offset: u32) -> anyhow::Result<Vec<MsgRow>> {
    let Some(conversation_id) = conversations::resolve_id(pool, chat_id).await? else {
        return Ok(Vec::new());
    };
    load_rows(pool, conversation_id, chat_id, Some(limit as i64), Some(offset as i64)).await
}

async fn load_rows(
    pool: &Pool,
    conversation_id: i64,
    chat_id: &str,
    limit: Option<i64>,
    offset: Option<i64>,
) -> anyhow::Result<Vec<MsgRow>> {
    let limit = limit.unwrap_or(-1);
    let offset = offset.unwrap_or(0);
    let rows: Vec<(i64, String, Option<String>, bool, i64)> = sqlx::query_as(
        "SELECT m.id, COALESCE(u.username, '') AS sender, m.text_content, m.is_encrypted, m.timestamp
         FROM messages m
         LEFT JOIN users u ON u.id = m.sender_id
         WHERE m.conversation_id = ?1
         ORDER BY m.timestamp DESC
         LIMIT ?2 OFFSET ?3",
    )
    .bind(conversation_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, sender, text, sealed, t)| MsgRow {
            id,
            chat_id: chat_id.to_string(),
            sender,
            text: text.unwrap_or_default(),
            sealed,
            t: t as u64,
            mine: false,
        })
        .collect())
}

/// Messages newer than `after_timestamp` (exclusive), ordered ascending.
pub async fn load_since(pool: &Pool, conversation_id: i64, after_timestamp: i64, limit: u32) -> anyhow::Result<Vec<Message>> {
    let rows = sqlx::query_as::<_, Message>(
        "SELECT * FROM messages
         WHERE conversation_id = ?1 AND timestamp > ?2
         ORDER BY timestamp ASC
         LIMIT ?3",
    )
    .bind(conversation_id)
    .bind(after_timestamp)
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Legacy shim: count messages for a string chat id.
pub async fn count(pool: &Pool, chat_id: &str) -> anyhow::Result<u32> {
    let Some(conversation_id) = conversations::resolve_id(pool, chat_id).await? else {
        return Ok(0);
    };
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM messages WHERE conversation_id = ?1")
        .bind(conversation_id)
        .fetch_one(pool)
        .await?;
    Ok(n as u32)
}

pub async fn delete(pool: &Pool, id: i64) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM messages WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

/// Legacy shim: delete messages before a timestamp for a string chat id.
pub async fn delete_before(pool: &Pool, chat_id: &str, before_t: u64) -> anyhow::Result<usize> {
    let Some(conversation_id) = conversations::resolve_id(pool, chat_id).await? else {
        return Ok(0);
    };
    let n = sqlx::query("DELETE FROM messages WHERE conversation_id = ?1 AND timestamp < ?2")
        .bind(conversation_id)
        .bind(before_t as i64)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() as usize)
}

pub async fn set_status(pool: &Pool, id: i64, status: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE messages SET status = ?2 WHERE id = ?1")
        .bind(id)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}

/// Convenience wrapper that forwards to `conversations` for ergonomics.
pub async fn clear_unread_for(pool: &Pool, conversation_id: i64) -> anyhow::Result<()> {
    conversations::clear_unread(pool, conversation_id).await
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
                    id: 0,
                    chat_id: "c".into(),
                    sender: "s".into(),
                    text: text.into(),
                    sealed: false,
                    t: (base + i as i64) as u64,
                    mine: false,
                },
            )
            .await
            .unwrap();
        }

        assert_eq!(count(&pool, "c").await.unwrap(), 3);

        let all = load_all(&pool, "c").await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].text.as_str(), "new");
        assert_eq!(all[2].text.as_str(), "old");

        let p0 = load_page(&pool, "c", 2, 0).await.unwrap();
        assert_eq!(p0.len(), 2);
        assert_eq!(p0[0].text.as_str(), "new");

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
                id: 0,
                chat_id: "dm".into(),
                sender: "a".into(),
                text: "hi".into(),
                sealed: false,
                t: now_ms() as u64,
                mine: false,
            },
        )
        .await
        .unwrap();
        let conv_id = conversations::resolve_id(&pool, "dm").await.unwrap().unwrap();
        let conv = conversations::get(&pool, conv_id).await.unwrap().unwrap();
        assert_eq!(conv.unread_count, 1);
        assert_eq!(conv.name.as_deref(), Some("dm"));
        conversations::clear_unread(&pool, conv_id).await.unwrap();
        assert_eq!(conversations::get(&pool, conv_id).await.unwrap().unwrap().unread_count, 0);
    }

    #[tokio::test]
    async fn load_group_conversation_by_name() {
        let pool = open_memory().await.unwrap();
        // A group conversation is stored with type=1 but must still resolve by name.
        crate::conversations::upsert(
            &pool,
            200,
            &crate::conversations::ConversationPatch {
                type_: Some(1),
                name: Some("g".into()),
                peer_id: None,
                avatar_path: None,
            },
        )
        .await
        .unwrap();
        insert(&pool, &NewMessage::text(200, 101, "hi group")).await.unwrap();

        let rows = load_all(&pool, "g").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text.as_str(), "hi group");
    }
}
