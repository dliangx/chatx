use crate::{new_id, now_ms, Pool};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct User {
    pub id: i64,
    pub username: Option<String>,
    pub nickname: Option<String>,
    pub avatar_path: Option<String>,
    pub bio: Option<String>,
    pub public_key: Option<String>,
    pub created_at: i64,
    pub updated_at: Option<i64>,
}

#[derive(Debug, Default, Clone)]
pub struct UserPatch {
    pub username: Option<String>,
    pub nickname: Option<String>,
    pub avatar_path: Option<String>,
    pub bio: Option<String>,
    pub public_key: Option<String>,
}

pub async fn upsert(pool: &Pool, id: i64, patch: &UserPatch) -> anyhow::Result<User> {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO users (id, username, nickname, avatar_path, bio, public_key, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
         ON CONFLICT(id) DO UPDATE SET
            username    = COALESCE(?2, username),
            nickname    = COALESCE(?3, nickname),
            avatar_path = COALESCE(?4, avatar_path),
            bio         = COALESCE(?5, bio),
            public_key  = COALESCE(?6, public_key),
            updated_at  = ?7",
    )
    .bind(id)
    .bind(&patch.username)
    .bind(&patch.nickname)
    .bind(&patch.avatar_path)
    .bind(&patch.bio)
    .bind(&patch.public_key)
    .bind(now)
    .execute(pool)
    .await?;
    let row = get(pool, id).await?;
    row.ok_or_else(|| anyhow::anyhow!("user {id} not found after upsert"))
}

pub async fn get(pool: &Pool, id: i64) -> anyhow::Result<Option<User>> {
    let row = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn get_by_username(pool: &Pool, username: &str) -> anyhow::Result<Option<User>> {
    let row = sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = ?1")
        .bind(username)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn list(pool: &Pool, limit: u32, offset: u32) -> anyhow::Result<Vec<User>> {
    let rows = sqlx::query_as::<_, User>(
        "SELECT * FROM users ORDER BY COALESCE(nickname, username) LIMIT ?1 OFFSET ?2",
    )
    .bind(limit as i64)
    .bind(offset as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Fuzzy search across username / nickname.
pub async fn search(pool: &Pool, q: &str, limit: u32) -> anyhow::Result<Vec<User>> {
    if q.is_empty() {
        return list(pool, limit, 0).await;
    }
    let like = format!("%{q}%");
    let rows = sqlx::query_as::<_, User>(
        "SELECT * FROM users
         WHERE username LIKE ?1 OR nickname LIKE ?1
         ORDER BY COALESCE(nickname, username)
         LIMIT ?2",
    )
    .bind(&like)
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn delete(pool: &Pool, id: i64) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM users WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

pub async fn count(pool: &Pool) -> anyhow::Result<u32> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;
    Ok(n as u32)
}

/// Find-or-create a user by its text identity (username). Used by the legacy
/// message shim to map a string sender onto an integer user id.
pub async fn ensure_identity(pool: &Pool, username: &str) -> anyhow::Result<i64> {
    if let Some(u) = get_by_username(pool, username).await? {
        return Ok(u.id);
    }
    let id = new_id();
    let now = now_ms();
    sqlx::query("INSERT INTO users (id, username, created_at) VALUES (?1, ?2, ?3)")
        .bind(id)
        .bind(username)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(id)
}

/// Reverse lookup: integer user id -> text identity (username).
pub async fn username(pool: &Pool, id: i64) -> anyhow::Result<Option<String>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT username FROM users WHERE id = ?1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.and_then(|(u,)| u))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    #[tokio::test]
    async fn upsert_get_search_delete_roundtrip() {
        let pool = open_memory().await.unwrap();
        let u = upsert(
            &pool,
            1,
            &UserPatch {
                username: Some("alice".into()),
                nickname: Some("Alice".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(u.id, 1);
        assert_eq!(u.nickname.as_deref(), Some("Alice"));

        let found = get_by_username(&pool, "alice").await.unwrap();
        assert!(found.is_some());

        upsert(
            &pool,
            1,
            &UserPatch {
                nickname: Some("Alicia".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            get(&pool, 1).await.unwrap().unwrap().nickname.as_deref(),
            Some("Alicia")
        );

        let hits = search(&pool, "alic", 10).await.unwrap();
        assert_eq!(hits.len(), 1);

        assert!(delete(&pool, 1).await.unwrap());
        assert!(!delete(&pool, 1).await.unwrap());
        assert!(get(&pool, 1).await.unwrap().is_none());
    }
}
