use crate::{Pool, new_id, now_ms};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct SocialPost {
    pub id: i64,
    pub author_id: i64,
    pub content: Option<String>,
    pub media_urls: Option<String>,
    pub preview_urls: Option<String>,
    pub visibility: i64,
    pub timestamp: i64,
    pub like_count: i64,
    pub comment_count: i64,
    #[sqlx(default)]
    pub recommend_count: i64,
    /// Denormalized display name of the original author (at post time), used
    /// for wire transport and as a fallback when the author has no local
    /// `users` row.
    #[sqlx(default)]
    pub user_name: Option<String>,
    /// The base58 peer id of the canonical source (the publisher's device).
    /// Set at publish (own peer) and preserved on pull (the wire's value), so
    /// a viewer who pulled the post from an intermediate still dials the
    /// publisher directly for a `React`. NULL for pre-feature rows / local
    /// posts created before the publisher stamped it.
    #[sqlx(default)]
    pub source_peer_id: Option<String>,
    /// Stable cross-device identity (random 63-bit), minted once by the
    /// publisher and carried on the wire. Identical on every device holding
    /// the post — the immutable key the `React` op resolves on.
    #[sqlx(default)]
    pub post_uid: Option<i64>,
}

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct SocialRecommend {
    pub post_id: i64,
    pub user_id: i64,
    pub created_at: i64,
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
    user_name: Option<&str>,
    content: Option<&str>,
    media_urls: Option<&str>,
    preview_urls: Option<&str>,
    visibility: i64,
    timestamp: i64,
) -> anyhow::Result<SocialPost> {
    let id = new_id();
    sqlx::query(
        "INSERT INTO social_posts (id, author_id, user_name, content, media_urls, preview_urls, visibility, timestamp)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(id)
    .bind(author_id)
    .bind(user_name)
    .bind(content)
    .bind(media_urls)
    .bind(preview_urls)
    .bind(visibility)
    .bind(timestamp)
    .execute(pool)
    .await?;
    let row = get_post(pool, id).await?;
    row.ok_or_else(|| anyhow::anyhow!("post {id} vanished"))
}

/// True when a post with the given `(author_id, timestamp)` already exists
/// locally. The wire protocol identifies a post by timestamp, so this is the
/// idempotency key used to avoid re-pulling a friend's post we already have.
pub async fn has_post_by_author_ts(pool: &Pool, author_id: i64, ts: i64) -> anyhow::Result<bool> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM social_posts WHERE author_id = ?1 AND timestamp = ?2")
            .bind(author_id)
            .bind(ts)
            .fetch_optional(pool)
            .await?;
    Ok(row.is_some())
}

/// Find a local post by its wire identity (`author_id`, `user_name`,
/// `content`). Used by the receiver to locate an existing post whose
/// denormalized counters may have changed on the sender side.
pub async fn get_post_by_identity(
    pool: &Pool,
    author_id: i64,
    user_name: &str,
    content: Option<&str>,
) -> anyhow::Result<Option<SocialPost>> {
    let row = sqlx::query_as::<_, SocialPost>(
        "SELECT * FROM social_posts WHERE author_id = ?1 AND (user_name IS ?2) AND (content IS ?3) ORDER BY timestamp DESC LIMIT 1",
    )
    .bind(author_id)
    .bind(Some(user_name))
    .bind(content)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn has_synced_post(
    pool: &Pool,
    author_id: i64,
    user_name: &str,
    content: Option<&str>,
    wire_ts: i64,
) -> anyhow::Result<bool> {
    if has_post_by_author_ts(pool, author_id, wire_ts).await? {
        return Ok(true);
    }
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM social_posts
          WHERE (user_name IS ?1)
            AND (content IS ?2)",
    )
    .bind(Some(user_name))
    .bind(content)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

pub async fn get_post(pool: &Pool, id: i64) -> anyhow::Result<Option<SocialPost>> {
    let row = sqlx::query_as::<_, SocialPost>("SELECT * FROM social_posts WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Look up a post by its `timestamp` (used by the binary moment channel —
/// the remote client identifies posts by timestamp, not by our local `id`).
pub async fn get_post_by_ts(pool: &Pool, ts: i64) -> anyhow::Result<Option<SocialPost>> {
    let row = sqlx::query_as::<_, SocialPost>("SELECT * FROM social_posts WHERE timestamp = ?1")
        .bind(ts)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// All posts where `since < timestamp <= until`, ordered newest first.
/// Used by the binary moment channel serving side and by the background
/// sync loop.
pub async fn posts_between(pool: &Pool, since: i64, until: i64) -> anyhow::Result<Vec<SocialPost>> {
    let rows = sqlx::query_as::<_, SocialPost>(
        "SELECT * FROM social_posts WHERE timestamp > ?1 AND timestamp <= ?2
         ORDER BY timestamp DESC",
    )
    .bind(since)
    .bind(until)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn delete_post(pool: &Pool, id: i64) -> anyhow::Result<bool> {
    let mut trans = pool.begin().await?;
    {
        sqlx::query("DELETE FROM social_comments WHERE post_id = ?1")
            .bind(id)
            .execute(trans.as_mut())
            .await?;
        sqlx::query("DELETE FROM social_likes WHERE post_id = ?1")
            .bind(id)
            .execute(trans.as_mut())
            .await?;
        sqlx::query("DELETE FROM social_recommends WHERE post_id = ?1")
            .bind(id)
            .execute(trans.as_mut())
            .await?;
        let n = sqlx::query("DELETE FROM social_posts WHERE id = ?1")
            .bind(id)
            .execute(trans.as_mut())
            .await?;
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

/// Liker identities (username + optional nickname) for a post. Used by the
/// wire-serve path to ship the full likers list to a friend's client — the
/// receiver resolves each username to its own local `users.id`.
#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct ActorWithProfile {
    pub username: String,
    pub nickname: Option<String>,
}

pub async fn likers_with_profile(pool: &Pool, post_id: i64) -> anyhow::Result<Vec<ActorWithProfile>> {
    let rows: Vec<ActorWithProfile> = sqlx::query_as(
        "SELECT COALESCE(u.username, '') AS username, u.nickname
           FROM social_likes l JOIN users u ON u.id = l.user_id
          WHERE l.post_id = ?1
          ORDER BY l.created_at",
    )
    .bind(post_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().filter(|r| !r.username.is_empty()).collect())
}

pub async fn recommenders_with_profile(pool: &Pool, post_id: i64) -> anyhow::Result<Vec<ActorWithProfile>> {
    let rows: Vec<ActorWithProfile> = sqlx::query_as(
        "SELECT COALESCE(u.username, '') AS username, u.nickname
           FROM social_recommends r JOIN users u ON u.id = r.user_id
          WHERE r.post_id = ?1
          ORDER BY r.created_at",
    )
    .bind(post_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().filter(|r| !r.username.is_empty()).collect())
}

/// Comment rows (with commenter identity + body + timestamp) for a post.
/// Wire transport: the receiver uses `username` to resolve (or stub-create)
/// a local `users` row, then upserts the comment into `social_comments`.
#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct CommentRowWithAuthor {
    pub id: i64,
    pub post_id: i64,
    pub username: String,
    pub nickname: Option<String>,
    pub content: String,
    pub reply_to: Option<i64>,
    pub created_at: i64,
}

pub async fn comments_with_profile(pool: &Pool, post_id: i64) -> anyhow::Result<Vec<CommentRowWithAuthor>> {
    let rows: Vec<CommentRowWithAuthor> = sqlx::query_as(
        "SELECT sc.id, sc.post_id,
                COALESCE(u.username, '') AS username, u.nickname,
                sc.content, sc.reply_to, sc.created_at
           FROM social_comments sc
           LEFT JOIN users u ON u.id = sc.author_id
          WHERE sc.post_id = ?1
          ORDER BY sc.created_at",
    )
    .bind(post_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().filter(|r| !r.username.is_empty()).collect())
}

/// Apply a batch of incoming likers (from the wire) to local `social_likes`
/// rows. Returns `true` if any row was newly inserted.
pub async fn apply_incoming_likers(
    pool: &Pool,
    post_id: i64,
    actors: &[(String, Option<String>)],
) -> anyhow::Result<bool> {
    let mut any_new = false;
    for (username, nickname) in actors {
        let author_id = resolve_or_stub_user(pool, username, nickname.as_deref()).await?;
        let n = sqlx::query(
            "INSERT OR IGNORE INTO social_likes (post_id, user_id, created_at) VALUES (?1, ?2, ?3)",
        )
        .bind(post_id)
        .bind(author_id)
        .bind(crate::now_ms())
        .execute(pool)
        .await?;
        if n.rows_affected() > 0 {
            bump_like_count(pool, post_id, 1).await?;
            any_new = true;
        }
    }
    Ok(any_new)
}

pub async fn apply_incoming_recommenders(
    pool: &Pool,
    post_id: i64,
    actors: &[(String, Option<String>)],
) -> anyhow::Result<bool> {
    let mut any_new = false;
    for (username, nickname) in actors {
        let author_id = resolve_or_stub_user(pool, username, nickname.as_deref()).await?;
        let n = sqlx::query(
            "INSERT OR IGNORE INTO social_recommends (post_id, user_id, created_at) VALUES (?1, ?2, ?3)",
        )
        .bind(post_id)
        .bind(author_id)
        .bind(crate::now_ms())
        .execute(pool)
        .await?;
        if n.rows_affected() > 0 {
            bump_recommend_count(pool, post_id, 1).await?;
            any_new = true;
        }
    }
    Ok(any_new)
}

/// Apply a batch of incoming comments (from the wire) to local
/// `social_comments` rows. Returns `true` if any row was newly inserted.
pub async fn apply_incoming_comments(
    pool: &Pool,
    post_id: i64,
    comments: &[(String, Option<String>, String, i64)],
) -> anyhow::Result<bool> {
    let mut any_new = false;
    for (username, nickname, content, created_at) in comments {
        let (_, inserted) = upsert_incoming_comment(
            pool,
            post_id,
            username,
            nickname.as_deref(),
            content,
            *created_at,
        )
        .await?;
        if inserted {
            any_new = true;
        }
    }
    Ok(any_new)
}

/// Resolve a username to a local `users.id`, creating a stub row if the user
/// is unknown locally. This keeps `post_comments` joins working and lets us
/// re-resolve later when the real profile syncs in.
async fn resolve_or_stub_user(
    pool: &Pool,
    username: &str,
    nickname: Option<&str>,
) -> anyhow::Result<i64> {
    if let Some(u) = crate::users::get_by_username(pool, username).await? {
        return Ok(u.id);
    }
    let id = crate::new_id();
    let patch = crate::users::UserPatch {
        username: Some(username.to_string()),
        nickname: nickname.map(|s| s.to_string()),
        ..Default::default()
    };
    crate::users::upsert(pool, id, &patch).await?;
    Ok(id)
}

/// Upsert a single incoming comment from the wire. Resolves the commenter's
/// `username` to a local `users.id` (creating a stub row if necessary) and
/// then upserts the comment by `(post_id, content, created_at)`.
/// Returns `(local_id, inserted)` — `inserted` is false when an equivalent
/// row already existed locally.
pub async fn upsert_incoming_comment(
    pool: &Pool,
    post_id: i64,
    username: &str,
    nickname: Option<&str>,
    content: &str,
    created_at: i64,
) -> anyhow::Result<(i64, bool)> {
    let author_id = resolve_or_stub_user(pool, username, nickname).await?;
    let existing: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM social_comments WHERE post_id = ?1 AND content = ?2 AND created_at = ?3",
    )
    .bind(post_id)
    .bind(content)
    .bind(created_at)
    .fetch_optional(pool)
    .await?;
    if let Some((id,)) = existing {
        return Ok((id, false));
    }
    let id = crate::new_id();
    sqlx::query(
        "INSERT INTO social_comments (id, post_id, author_id, content, reply_to, created_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
    )
    .bind(id)
    .bind(post_id)
    .bind(author_id)
    .bind(content)
    .bind(created_at)
    .execute(pool)
    .await?;
    bump_comment_count(pool, post_id, 1).await?;
    Ok((id, true))
}

// --- recommends -----------------------------------------------------------

pub async fn bump_recommend_count(pool: &Pool, post_id: i64, delta: i64) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE social_posts SET recommend_count = MAX(0, recommend_count + ?2) WHERE id = ?1",
    )
    .bind(post_id)
    .bind(delta)
    .execute(pool)
    .await?;
    Ok(())
}

/// Overwrite the three denormalized counters with the authoritative values
/// received over the wire (used when a re-pulled post already exists locally
/// and its counts have changed on the sender side).
pub async fn set_counts(pool: &Pool, post_id: i64, like: i64, recommend: i64, comment: i64) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE social_posts SET like_count = ?2, recommend_count = ?3, comment_count = ?4 WHERE id = ?1",
    )
    .bind(post_id)
    .bind(like.max(0))
    .bind(recommend.max(0))
    .bind(comment.max(0))
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_timestamp(pool: &Pool, post_id: i64, ts: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE social_posts SET timestamp = ?2 WHERE id = ?1")
        .bind(post_id)
        .bind(ts)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record which device this synced post came from, so a viewer's
/// like/comment can later be routed back to the post's canonical owner.
pub async fn set_source_peer(pool: &Pool, post_id: i64, peer: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE social_posts SET source_peer_id = ?2 WHERE id = ?1")
        .bind(post_id)
        .bind(peer)
        .execute(pool)
        .await?;
    Ok(())
}

/// Stamp the post's immutable cross-device identity (`post_uid`). The
/// publisher mints one at publish time; the receiver copies the wire's value
/// onto the row it creates so it matches on every device.
pub async fn set_post_uid(pool: &Pool, post_id: i64, uid: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE social_posts SET post_uid = ?2 WHERE id = ?1")
        .bind(post_id)
        .bind(uid)
        .execute(pool)
        .await?;
    Ok(())
}

/// Look up a post by its immutable cross-device identity (see `set_post_uid`).
pub async fn get_post_by_uid(pool: &Pool, uid: i64) -> anyhow::Result<Option<SocialPost>> {
    let row = sqlx::query_as::<_, SocialPost>("SELECT * FROM social_posts WHERE post_uid = ?1")
        .bind(uid)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Trust check for the `React` path: returns `true` when the peer id
/// (base58) of the device dialing in matches any device row of a user that
/// has an active friendship with `owner_user_id`. This prevents a non-friend
/// (or a stranger) from pushing fabricated reactions onto a friend's post.
pub async fn requester_is_friend_of(
    pool: &Pool,
    owner_user_id: i64,
    requester_peer_id: &str,
) -> anyhow::Result<bool> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM friendships f
           JOIN devices d ON d.user_id = f.user_low
          WHERE f.user_high = ?1 AND d.peer_id = ?2
          UNION ALL
          SELECT 1 FROM friendships f
           JOIN devices d ON d.user_id = f.user_high
          WHERE f.user_low = ?1 AND d.peer_id = ?2
          LIMIT 1",
    )
    .bind(owner_user_id)
    .bind(requester_peer_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

pub async fn recommend(pool: &Pool, post_id: i64, user_id: i64) -> anyhow::Result<bool> {
    let n = sqlx::query(
        "INSERT OR IGNORE INTO social_recommends (post_id, user_id, created_at) VALUES (?1, ?2, ?3)",
    )
    .bind(post_id)
    .bind(user_id)
    .bind(now_ms())
    .execute(pool)
    .await?;
    if n.rows_affected() > 0 {
        bump_recommend_count(pool, post_id, 1).await?;
    }
    Ok(n.rows_affected() > 0)
}

pub async fn unrecommend(pool: &Pool, post_id: i64, user_id: i64) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM social_recommends WHERE post_id = ?1 AND user_id = ?2")
        .bind(post_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    if n.rows_affected() > 0 {
        bump_recommend_count(pool, post_id, -1).await?;
    }
    Ok(n.rows_affected() > 0)
}

pub async fn has_recommended(pool: &Pool, post_id: i64, user_id: i64) -> anyhow::Result<bool> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM social_recommends WHERE post_id = ?1 AND user_id = ?2")
            .bind(post_id)
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.is_some())
}

pub async fn recommenders(pool: &Pool, post_id: i64) -> anyhow::Result<Vec<i64>> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        "SELECT user_id FROM social_recommends WHERE post_id = ?1 ORDER BY created_at",
    )
    .bind(post_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(u,)| u).collect())
}

// --- comments --------------------------------------------------------------

pub async fn add_comment(
    pool: &Pool,
    post_id: i64,
    author_id: i64,
    content: &str,
    reply_to: Option<i64>,
) -> anyhow::Result<SocialComment> {
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

pub async fn list_comments(
    pool: &Pool,
    post_id: i64,
    limit: u32,
    offset: u32,
) -> anyhow::Result<Vec<SocialComment>> {
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
        let post = create_post(
            &pool,
            1,
            Some("u1"),
            Some("hello world"),
            None,
            None,
            0,
            1000,
        )
        .await
        .unwrap();
        assert_eq!(post.preview_urls, None);
        assert_eq!(post.like_count, 0);
        assert_eq!(post.comment_count, 0);

        assert!(like(&pool, post.id, 2).await.unwrap());
        assert!(
            !like(&pool, post.id, 2).await.unwrap(),
            "double like is a no-op"
        );
        assert_eq!(
            get_post(&pool, post.id).await.unwrap().unwrap().like_count,
            1
        );
        assert!(has_liked(&pool, post.id, 2).await.unwrap());
        assert_eq!(likers(&pool, post.id).await.unwrap(), vec![2]);

        assert!(unlike(&pool, post.id, 2).await.unwrap());
        assert!(!unlike(&pool, post.id, 2).await.unwrap());
        assert_eq!(
            get_post(&pool, post.id).await.unwrap().unwrap().like_count,
            0
        );

        let c1 = add_comment(&pool, post.id, 1, "nice", None).await.unwrap();
        let _c2 = add_comment(&pool, post.id, 2, "@u1 agree", Some(c1.id))
            .await
            .unwrap();
        assert_eq!(
            get_post(&pool, post.id)
                .await
                .unwrap()
                .unwrap()
                .comment_count,
            2
        );
        assert_eq!(list_comments(&pool, post.id, 10, 0).await.unwrap().len(), 2);

        assert!(delete_comment(&pool, c1.id).await.unwrap());
        assert_eq!(
            get_post(&pool, post.id)
                .await
                .unwrap()
                .unwrap()
                .comment_count,
            1
        );

        assert!(delete_post(&pool, post.id).await.unwrap());
        assert!(get_post(&pool, post.id).await.unwrap().is_none());
        assert_eq!(
            list_comments(&pool, post.id, 10, 0).await.unwrap().len(),
            0,
            "comments cascade-deleted"
        );
        assert!(!delete_post(&pool, post.id).await.unwrap());
    }

    #[tokio::test]
    async fn has_synced_post_identity() {
        let pool = open_memory().await.unwrap();
        // Fresh rows store the wire timestamp: exact ts identity matches.
        create_post(&pool, 1, Some("u1"), Some("hi"), None, None, 0, 123)
            .await
            .unwrap();
        assert!(
            has_synced_post(&pool, 1, "u1", Some("hi"), 123)
                .await
                .unwrap(),
            "wire ts identity matches"
        );
        // A re-shared post arrives under a bumped ts (recommend propagation)
        // — the stable name+content identity must still match.
        assert!(
            has_synced_post(&pool, 1, "u1", Some("hi"), 99999)
                .await
                .unwrap(),
            "content identity catches a post re-shared with a bumped ts"
        );
        // The report scenario: our OWN post comes back to us after a friend
        // re-recommended it — attributed to the friend's local row (a
        // different author_id), with the ts bumped. The wire carries no
        // author id, only the name, so (name, content) must match.
        assert!(
            has_synced_post(&pool, 7, "u1", Some("hi"), 99996)
                .await
                .unwrap(),
            "own post returned via a friend's re-recommend is deduped"
        );
        // A genuinely different post (different content) must not match.
        assert!(
            !has_synced_post(&pool, 1, "u1", Some("different"), 99998)
                .await
                .unwrap()
        );
        // A different author (different display name) posting the same text
        // must be kept distinct — the name is part of the identity.
        create_post(&pool, 1, Some("other"), Some("hi"), None, None, 0, 55)
            .await
            .unwrap();
        assert!(
            has_synced_post(&pool, 1, "other", Some("hi"), 55)
                .await
                .unwrap()
        );
        // And a post under yet another name is not swallowed by either row.
        assert!(
            !has_synced_post(&pool, 1, "third", Some("hi"), 56)
                .await
                .unwrap()
        );
        // Legacy rows stored a *local* fetch time, not the wire ts — they are
        // only catchable via the content identity.
        create_post(&pool, 1, Some("u2"), Some("legacy"), None, None, 0, 424242)
            .await
            .unwrap();
        assert!(
            has_synced_post(&pool, 1, "u2", Some("legacy"), 555)
                .await
                .unwrap()
        );
        // Empty content is stored as NULL — the `IS` match must be NULL-safe.
        create_post(&pool, 2, Some("u3"), None, Some("m"), None, 0, 777)
            .await
            .unwrap();
        assert!(has_synced_post(&pool, 2, "u3", None, 777).await.unwrap());
        assert!(
            !has_synced_post(&pool, 2, "u3", Some("text"), 888)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn post_preview_urls_roundtrip() {
        let pool = open_memory().await.unwrap();
        let media = r#"["/h/media/posts/p_1/1.jpg"]"#;
        let preview = r#"["/h/media/posts/p_1/preview/1.jpg"]"#;
        let post = create_post(
            &pool,
            1,
            Some("u1"),
            Some("with photos"),
            Some(media),
            Some(preview),
            0,
            1000,
        )
        .await
        .unwrap();
        assert_eq!(post.user_name.as_deref(), Some("u1"));
        assert_eq!(post.preview_urls.as_deref(), Some(preview));
        let back = get_post(&pool, post.id).await.unwrap().unwrap();
        assert_eq!(back.preview_urls, post.preview_urls);
        assert_eq!(back.media_urls.as_deref(), Some(media));
    }
}
