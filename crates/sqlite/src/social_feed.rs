use crate::{new_id, now_ms, Pool};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct SocialPost {
    pub id: i64,
    pub author_id: i64,
    pub content: Option<String>,
    pub media_urls: Option<String>,
    pub visibility: i64,
    pub timestamp: i64,
    pub like_count: i64,
    pub comment_count: i64,
}

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct SocialLike {
    pub post_id: i64,
    pub user_id: i64,
    pub created_at: i64,
}

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct SocialComment {
    pub id: i64,
    pub post_id: i64,
    pub author_id: i64,
    pub content: String,
    pub reply_to: Option<i64>,
    pub created_at: i64,
}

pub async fn create_post(
    pool: &Pool,
    author_id: i64,
    content: Option<&str>,
    media_urls: Option<&str>,
    visibility: i64,
) -> anyhow::Result<SocialPost> {
    let id = new_id();
    let ts = now_ms();
    sqlx::query(
        "INSERT INTO social_posts (id, author_id, content, media_urls, visibility, timestamp)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(id)
    .bind(author_id)
    .bind(content)
    .bind(media_urls)
    .bind(visibility)
    .bind(ts)
    .execute(pool)
    .await?;
    let row = get_post(pool, id).await?;
    row.ok_or_else(|| anyhow::anyhow!("post {id} vanished"))
}

pub async fn get_post(pool: &Pool, id: i64) -> anyhow::Result<Option<SocialPost>> {
    let row = sqlx::query_as::<_, SocialPost>("SELECT * FROM social_posts WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn delete_post(pool: &Pool, id: i64) -> anyhow::Result<bool> {
    let mut trans = pool.begin().await?;
    {
        sqlx::query("DELETE FROM social_comments WHERE post_id = ?1").bind(id).execute(trans.as_mut()).await?;
        sqlx::query("DELETE FROM social_likes WHERE post_id = ?1").bind(id).execute(trans.as_mut()).await?;
        let n = sqlx::query("DELETE FROM social_posts WHERE id = ?1").bind(id).execute(trans.as_mut()).await?;
        if n.rows_affected() == 0 {
            trans.rollback().await?;
            return Ok(false);
        }
    }
    trans.commit().await?;
    Ok(true)
}

pub async fn bump_like_count(pool: &Pool, post_id: i64, delta: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE social_posts SET like_count = MAX(0, like_count + ?2) WHERE id = ?1")
        .bind(post_id)
        .bind(delta)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn bump_comment_count(pool: &Pool, post_id: i64, delta: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE social_posts SET comment_count = MAX(0, comment_count + ?2) WHERE id = ?1")
        .bind(post_id)
        .bind(delta)
        .execute(pool)
        .await?;
    Ok(())
}

// --- likes -----------------------------------------------------------------

pub async fn like(pool: &Pool, post_id: i64, user_id: i64) -> anyhow::Result<bool> {
    let n = sqlx::query(
        "INSERT OR IGNORE INTO social_likes (post_id, user_id, created_at) VALUES (?1, ?2, ?3)",
    )
    .bind(post_id)
    .bind(user_id)
    .bind(now_ms())
    .execute(pool)
    .await?;
    if n.rows_affected() > 0 {
        bump_like_count(pool, post_id, 1).await?;
    }
    Ok(n.rows_affected() > 0)
}

pub async fn unlike(pool: &Pool, post_id: i64, user_id: i64) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM social_likes WHERE post_id = ?1 AND user_id = ?2")
        .bind(post_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    if n.rows_affected() > 0 {
        bump_like_count(pool, post_id, -1).await?;
    }
    Ok(n.rows_affected() > 0)
}

pub async fn has_liked(pool: &Pool, post_id: i64, user_id: i64) -> anyhow::Result<bool> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM social_likes WHERE post_id = ?1 AND user_id = ?2")
            .bind(post_id)
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.is_some())
}

pub async fn likers(pool: &Pool, post_id: i64) -> anyhow::Result<Vec<i64>> {
    let rows: Vec<(i64,)> =
        sqlx::query_as("SELECT user_id FROM social_likes WHERE post_id = ?1 ORDER BY created_at")
            .bind(post_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(u,)| u).collect())
}

// --- comments --------------------------------------------------------------

pub async fn add_comment(pool: &Pool, post_id: i64, author_id: i64, content: &str, reply_to: Option<i64>) -> anyhow::Result<SocialComment> {
    let id = new_id();
    let ts = now_ms();
    if get_post(pool, post_id).await?.is_none() {
        anyhow::bail!("post {post_id} does not exist");
    }
    sqlx::query(
        "INSERT INTO social_comments (id, post_id, author_id, content, reply_to, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(id)
    .bind(post_id)
    .bind(author_id)
    .bind(content)
    .bind(reply_to)
    .bind(ts)
    .execute(pool)
    .await?;
    bump_comment_count(pool, post_id, 1).await?;
    let row = sqlx::query_as::<_, SocialComment>("SELECT * FROM social_comments WHERE id = ?1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

pub async fn list_comments(pool: &Pool, post_id: i64, limit: u32, offset: u32) -> anyhow::Result<Vec<SocialComment>> {
    let rows = sqlx::query_as::<_, SocialComment>(
        "SELECT * FROM social_comments WHERE post_id = ?1 ORDER BY created_at LIMIT ?2 OFFSET ?3",
    )
    .bind(post_id)
    .bind(limit as i64)
    .bind(offset as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn delete_comment(pool: &Pool, id: i64) -> anyhow::Result<bool> {
    let existing: Option<(i64,)> =
        sqlx::query_as("SELECT post_id FROM social_comments WHERE id = ?1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let n = sqlx::query("DELETE FROM social_comments WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    if n.rows_affected() > 0 {
        if let Some((post_id,)) = existing {
            bump_comment_count(pool, post_id, -1).await?;
        }
    }
    Ok(n.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    #[tokio::test]
    async fn post_like_comment_roundtrip() {
        let pool = open_memory().await.unwrap();
        let post = create_post(&pool, 1, Some("hello world"), None, 0).await.unwrap();
        assert_eq!(post.like_count, 0);
        assert_eq!(post.comment_count, 0);

        assert!(like(&pool, post.id, 2).await.unwrap());
        assert!(!like(&pool, post.id, 2).await.unwrap(), "double like is a no-op");
        assert_eq!(get_post(&pool, post.id).await.unwrap().unwrap().like_count, 1);
        assert!(has_liked(&pool, post.id, 2).await.unwrap());
        assert_eq!(likers(&pool, post.id).await.unwrap(), vec![2]);

        assert!(unlike(&pool, post.id, 2).await.unwrap());
        assert!(!unlike(&pool, post.id, 2).await.unwrap());
        assert_eq!(get_post(&pool, post.id).await.unwrap().unwrap().like_count, 0);

        let c1 = add_comment(&pool, post.id, 1, "nice", None).await.unwrap();
        let _c2 = add_comment(&pool, post.id, 2, "@u1 agree", Some(c1.id)).await.unwrap();
        assert_eq!(get_post(&pool, post.id).await.unwrap().unwrap().comment_count, 2);
        assert_eq!(list_comments(&pool, post.id, 10, 0).await.unwrap().len(), 2);

        assert!(delete_comment(&pool, c1.id).await.unwrap());
        assert_eq!(get_post(&pool, post.id).await.unwrap().unwrap().comment_count, 1);

        assert!(delete_post(&pool, post.id).await.unwrap());
        assert!(get_post(&pool, post.id).await.unwrap().is_none());
        assert_eq!(list_comments(&pool, post.id, 10, 0).await.unwrap().len(), 0, "comments cascade-deleted");
        assert!(!delete_post(&pool, post.id).await.unwrap());
    }
}
