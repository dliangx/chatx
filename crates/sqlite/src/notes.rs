use crate::{new_id, now_ms, Pool};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct Note {
    pub id: i64,
    pub user_id: i64,
    pub content: String,
    pub created_at: i64,
    pub updated_at: Option<i64>,
}

pub async fn create_note(pool: &Pool, user_id: i64, content: &str) -> anyhow::Result<Note> {
    let id = new_id();
    let ts = now_ms();
    sqlx::query(
        "INSERT INTO notes (id, user_id, content, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(id)
    .bind(user_id)
    .bind(content)
    .bind(ts)
    .bind(ts)
    .execute(pool)
    .await?;
    let row = get_note(pool, id).await?;
    row.ok_or_else(|| anyhow::anyhow!("note {id} vanished"))
}

pub async fn get_note(pool: &Pool, id: i64) -> anyhow::Result<Option<Note>> {
    let row = sqlx::query_as::<_, Note>("SELECT * FROM notes WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// List all notes for a user, newest first.
pub async fn list_notes(pool: &Pool, user_id: i64) -> anyhow::Result<Vec<Note>> {
    let rows = sqlx::query_as::<_, Note>("SELECT * FROM notes WHERE user_id = ?1 ORDER BY created_at DESC, id DESC")
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn delete_note(pool: &Pool, id: i64) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM notes WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    #[tokio::test]
    async fn note_roundtrip() {
        let pool = open_memory().await.unwrap();
        let n1 = create_note(&pool, 1, "hello").await.unwrap();
        let n2 = create_note(&pool, 1, "world").await.unwrap();
        create_note(&pool, 2, "other user").await.unwrap();

        let mine = list_notes(&pool, 1).await.unwrap();
        assert_eq!(mine.len(), 2);
        // newest first
        assert_eq!(mine[0].id, n2.id);

        assert!(get_note(&pool, n1.id).await.unwrap().is_some());
        assert!(delete_note(&pool, n1.id).await.unwrap());
        assert!(delete_note(&pool, n1.id).await.unwrap() == false);
        assert_eq!(list_notes(&pool, 1).await.unwrap().len(), 1);
    }
}
