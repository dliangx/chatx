use crate::now_ms;
use crate::Pool;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct Friendship {
    pub user_low: String,
    pub user_high: String,
    pub created_at: i64,
}

/// pending / accepted / rejected / ignored (stored as TEXT on the schema).
pub const FR_PENDING: &str = "pending";
pub const FR_ACCEPTED: &str = "accepted";
pub const FR_REJECTED: &str = "rejected";
pub const FR_IGNORED: &str = "ignored";

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct FriendRequest {
    pub id: i64,
    pub from_id: String,
    pub to_id: String,
    pub status: String,
    pub message: Option<String>,
    pub created_at: i64,
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct Follow {
    pub follower_id: String,
    pub following_id: String,
    pub created_at: i64,
}

fn ordered_pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

pub async fn add(pool: &Pool, a: &str, b: &str) -> anyhow::Result<()> {
    let (low, high) = ordered_pair(a, b);
    sqlx::query(
        "INSERT OR IGNORE INTO friendships (user_low, user_high, created_at) VALUES (?1, ?2, ?3)",
    )
    .bind(&low)
    .bind(&high)
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn remove(pool: &Pool, a: &str, b: &str) -> anyhow::Result<bool> {
    let (low, high) = ordered_pair(a, b);
    let n = sqlx::query("DELETE FROM friendships WHERE user_low = ?1 AND user_high = ?2")
        .bind(&low)
        .bind(&high)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

pub async fn has(pool: &Pool, a: &str, b: &str) -> anyhow::Result<bool> {
    let (low, high) = ordered_pair(a, b);
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM friendships WHERE user_low = ?1 AND user_high = ?2")
            .bind(&low)
            .bind(&high)
            .fetch_optional(pool)
            .await?;
    Ok(row.is_some())
}

pub async fn friends_of(pool: &Pool, user_id: &str) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT user_high FROM friendships WHERE user_low = ?1 ORDER BY user_high")
                .bind(user_id)
                .fetch_all(pool)
                .await?;
        out.extend(rows.into_iter().map(|(s,)| s));
    }
    {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT user_low FROM friendships WHERE user_high = ?1 ORDER BY user_low")
                .bind(user_id)
                .fetch_all(pool)
                .await?;
        out.extend(rows.into_iter().map(|(s,)| s));
    }
    Ok(out)
}

pub async fn common_friends(pool: &Pool, a: &str, b: &str) -> anyhow::Result<Vec<String>> {
    let mut mine = friends_of(pool, a).await?;
    let theirs = friends_of(pool, b).await?;
    mine.retain(|x| theirs.iter().any(|y| y == x));
    mine.sort();
    Ok(mine)
}

pub async fn count(pool: &Pool, user_id: &str) -> anyhow::Result<u32> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM friendships WHERE user_low = ?1 OR user_high = ?1",
    )
    .bind(user_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(n as u32)
}

// --- friend requests -------------------------------------------------------

pub async fn send_request(pool: &Pool, from_id: &str, to_id: &str, message: Option<&str>) -> anyhow::Result<i64> {
    if from_id == to_id {
        anyhow::bail!("cannot friend-request yourself");
    }
    let now = now_ms();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO friend_requests (from_id, to_id, status, message, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         ON CONFLICT(from_id, to_id) DO UPDATE SET
            status = ?3, message = COALESCE(?4, message), updated_at = ?5
         RETURNING id",
    )
    .bind(from_id)
    .bind(to_id)
    .bind(FR_PENDING)
    .bind(message)
    .bind(now)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

pub async fn list_inbox(pool: &Pool, user_id: &str, status: Option<&str>) -> anyhow::Result<Vec<FriendRequest>> {
    let status = status.unwrap_or(FR_PENDING);
    let rows = sqlx::query_as::<_, FriendRequest>(
        "SELECT * FROM friend_requests WHERE to_id = ?1 AND status = ?2
         ORDER BY created_at DESC",
    )
    .bind(user_id)
    .bind(status)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn list_sent(pool: &Pool, user_id: &str) -> anyhow::Result<Vec<FriendRequest>> {
    let rows = sqlx::query_as::<_, FriendRequest>(
        "SELECT * FROM friend_requests WHERE from_id = ?1 ORDER BY created_at DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn respond(pool: &Pool, request_id: i64, acceptor: &str, accept: bool) -> anyhow::Result<bool> {
    let existing: Option<(String, String, String)> = sqlx::query_as(
        "SELECT from_id, to_id, status FROM friend_requests WHERE id = ?1",
    )
    .bind(request_id)
    .fetch_optional(pool)
    .await?;
    let (from_id, to_id, status) = match existing {
        Some(r) => r,
        None => return Ok(false),
    };
    // Only the recipient (or sender cancelling their own pending request) may act.
    let allowed = (to_id == acceptor && status == FR_PENDING)
        || (from_id == acceptor && status == FR_PENDING);
    if !allowed {
        return Ok(false);
    }

    let new_status = if accept {
        FR_ACCEPTED
    } else if from_id == acceptor {
        FR_IGNORED
    } else {
        FR_REJECTED
    };
    let now = now_ms();
    sqlx::query("UPDATE friend_requests SET status = ?1, updated_at = ?2 WHERE id = ?3")
        .bind(new_status)
        .bind(now)
        .bind(request_id)
        .execute(pool)
        .await?;

    if accept {
        add(pool, &from_id, &to_id).await?;
    }
    Ok(true)
}

pub async fn cancel(pool: &Pool, request_id: i64) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM friend_requests WHERE id = ?1 AND status = 'pending'")
        .bind(request_id)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

// --- follows ---------------------------------------------------------------

pub async fn follow(pool: &Pool, follower: &str, following: &str) -> anyhow::Result<bool> {
    if follower == following {
        return Ok(false);
    }
    let n = sqlx::query(
        "INSERT OR IGNORE INTO follows (follower_id, following_id, created_at) VALUES (?1, ?2, ?3)",
    )
    .bind(follower)
    .bind(following)
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(n.rows_affected() > 0)
}

pub async fn unfollow(pool: &Pool, follower: &str, following: &str) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM follows WHERE follower_id = ?1 AND following_id = ?2")
        .bind(follower)
        .bind(following)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

pub async fn is_following(pool: &Pool, follower: &str, following: &str) -> anyhow::Result<bool> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM follows WHERE follower_id = ?1 AND following_id = ?2")
            .bind(follower)
            .bind(following)
            .fetch_optional(pool)
            .await?;
    Ok(row.is_some())
}

pub async fn followers(pool: &Pool, user_id: &str) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT follower_id FROM follows WHERE following_id = ?1 ORDER BY follower_id")
            .bind(user_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(s,)| s).collect())
}

pub async fn following(pool: &Pool, user_id: &str) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT following_id FROM follows WHERE follower_id = ?1 ORDER BY following_id")
            .bind(user_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(s,)| s).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    #[tokio::test]
    async fn friendships_roundtrip() {
        let pool = open_memory().await.unwrap();
        add(&pool, "alice", "bob").await.unwrap();
        assert!(has(&pool, "bob", "alice").await.unwrap(), "symmetric");
        assert!(!has(&pool, "alice", "carol").await.unwrap());
        assert_eq!(count(&pool, "alice").await.unwrap(), 1);
        assert_eq!(friends_of(&pool, "bob").await.unwrap(), vec!["alice".to_string()]);
        assert!(remove(&pool, "alice", "bob").await.unwrap());
        assert!(!remove(&pool, "alice", "bob").await.unwrap());
    }

    #[tokio::test]
    async fn friend_request_accept_and_reject_flow() {
        let pool = open_memory().await.unwrap();
        let id = send_request(&pool, "bob", "alice", Some("hi?")).await.unwrap();
        let inbox = list_inbox(&pool, "alice", None).await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].id, id);

        assert!(respond(&pool, id, "alice", true).await.unwrap());
        assert!(has(&pool, "alice", "bob").await.unwrap(), "acceptance creates friendship");

        let id2 = send_request(&pool, "carol", "alice", None).await.unwrap();
        assert!(respond(&pool, id2, "alice", false).await.unwrap(), "recipients may reject");

        let id3 = send_request(&pool, "dave", "alice", None).await.unwrap();
        assert!(!respond(&pool, id3, "carol", true).await.unwrap(), "third parties cannot respond");

        let id4 = send_request(&pool, "alice", "eve", None).await.unwrap();
        assert!(cancel(&pool, id4).await.unwrap(), "sender may cancel pending");
    }

    #[tokio::test]
    async fn follow_unfollow_lists() {
        let pool = open_memory().await.unwrap();
        assert!(follow(&pool, "a", "b").await.unwrap());
        assert!(!follow(&pool, "b", "b").await.unwrap(), "no self-follow");
        assert!(!follow(&pool, "a", "b").await.unwrap(), "duplicate follow no-op");

        assert!(is_following(&pool, "a", "b").await.unwrap());
        assert_eq!(followers(&pool, "b").await.unwrap(), vec!["a".to_string()]);
        assert_eq!(following(&pool, "a").await.unwrap(), vec!["b".to_string()]);

        assert!(unfollow(&pool, "a", "b").await.unwrap());
        assert!(!is_following(&pool, "a", "b").await.unwrap());
    }
}
