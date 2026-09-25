//! Cross-table queries for common chat scenarios.
//!
//! These are the "heavy" queries an IM client runs against the local DB:
//! chat list with denormalized last message, message history with sender
//! profiles, friend lists with profiles, group rosters, social feed with
//! author + like state, global search, etc.

use crate::Pool;
use sqlx::{FromRow, Row as _RowTrait};

/// One row of the chat list: the conversation plus the profile of the DM peer
/// or the group info for group conversations.
#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct ConversationListItem {
    pub id: i64,
    pub type_: i64,
    pub name: Option<String>,
    pub avatar_path: Option<String>,
    pub last_message_id: Option<i64>,
    pub last_message_preview: Option<String>,
    pub last_message_time: Option<i64>,
    pub unread_count: i64,
    pub is_pinned: bool,
    pub is_muted: bool,
    pub draft: Option<String>,
    // profile join (users)
    pub peer_username: Option<String>,
    pub peer_nickname: Option<String>,
    // group join
    pub group_owner_id: Option<i64>,
    pub member_count: Option<i64>,
}

/// Chat list for the current user, ordered for the UI:
/// pinned first, then by last activity.
pub async fn chat_list(pool: &Pool, me: &str, limit: u32) -> anyhow::Result<Vec<ConversationListItem>> {
    let _ = me;
    let rows = sqlx::query_as::<_, ConversationListItem>(
        "SELECT
            c.id              AS id,
            c.type            AS type_,
            c.name            AS name,
            COALESCE(u.avatar_path, g.avatar_path, c.avatar_path) AS avatar_path,
            c.last_message_id AS last_message_id,
            c.last_message_preview AS last_message_preview,
            c.last_message_time   AS last_message_time,
            c.unread_count AS unread_count,
            c.is_pinned  AS is_pinned,
            c.is_muted   AS is_muted,
            c.draft      AS draft,
            u.username   AS peer_username,
            u.nickname   AS peer_nickname,
            g.owner_id   AS group_owner_id,
            g.member_count AS member_count
         FROM conversations c
         LEFT JOIN users u ON u.id = c.peer_id
         LEFT JOIN groups g ON g.id = c.id
         ORDER BY c.is_pinned DESC, COALESCE(c.last_message_time, 0) DESC
         LIMIT ?1",
    )
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// A message enriched with sender profile and reply-target preview.
#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct MessageWithSender {
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
    // sender join (users)
    pub sender_nickname: Option<String>,
    pub sender_username: Option<String>,
    pub sender_avatar: Option<String>,
    // group-nickname fallback (group_members) for this conversation
    pub group_nickname: Option<String>,
    // reply-target preview
    pub reply_to_preview: Option<String>,
    pub reply_to_sender: Option<String>,
}

/// Message history for a conversation with sender profile resolved from
/// `users` (DM) or `group_members` (group). Ordered newest-first for paging.
pub async fn history_with_sender(
    pool: &Pool,
    conversation_id: i64,
    limit: u32,
    offset: u32,
) -> anyhow::Result<Vec<MessageWithSender>> {
    let rows = sqlx::query_as::<_, MessageWithSender>(
        "SELECT
            m.id, m.conversation_id, m.sender_id, m.msg_type, m.text_content,
            m.media_path, m.media_size, m.media_duration, m.thumbnail_path,
            m.timestamp, m.status, m.reply_to, m.is_encrypted, m.sync_seq, m.mentions,
            u.nickname    AS sender_nickname,
            u.username    AS sender_username,
            u.avatar_path AS sender_avatar,
            gm.nickname   AS group_nickname,
            r.text_content AS reply_to_preview,
            ru.nickname   AS reply_to_sender
         FROM messages m
         LEFT JOIN users u ON u.id = m.sender_id
         LEFT JOIN group_members gm ON gm.group_id = m.conversation_id AND gm.peer_id = m.sender_id
         LEFT JOIN messages r ON r.id = m.reply_to
         LEFT JOIN users ru ON ru.id = r.sender_id
         WHERE m.conversation_id = ?1
         ORDER BY m.timestamp DESC
         LIMIT ?2 OFFSET ?3",
    )
    .bind(conversation_id)
    .bind(limit as i64)
    .bind(offset as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// A friend row with the friend's profile + whether each side follows the other.
#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct FriendEntry {
    pub user_id: i64,
    pub username: Option<String>,
    pub nickname: Option<String>,
    pub avatar_path: Option<String>,
    pub since: i64,
    pub i_follow_them: bool,
    pub they_follow_me: bool,
}

pub async fn friends_with_profile(pool: &Pool, me: i64, limit: u32) -> anyhow::Result<Vec<FriendEntry>> {
    let rows = sqlx::query_as::<_, FriendEntry>(
        "SELECT
            f.user_id,
            u.username, u.nickname, u.avatar_path,
            f.created_at AS since,
            EXISTS(SELECT 1 FROM follows fl WHERE fl.follower_id = ?1 AND fl.following_id = f.user_id) AS i_follow_them,
            EXISTS(SELECT 1 FROM follows fl WHERE fl.follower_id = f.user_id AND fl.following_id = ?2) AS they_follow_me
         FROM (
            SELECT user_high AS user_id, created_at
              FROM friendships WHERE user_low = ?3
            UNION ALL
            SELECT user_low AS user_id, created_at
              FROM friendships WHERE user_high = ?4
         ) f
         LEFT JOIN users u ON u.id = f.user_id
         ORDER BY COALESCE(u.nickname, u.username)
         LIMIT ?5",
    )
    .bind(me)
    .bind(me)
    .bind(me)
    .bind(me)
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Group roster with role + optional user profile, ordered by role then name.
#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct GroupRosterEntry {
    pub peer_id: String,
    pub role: i64,
    pub nickname: Option<String>,
    pub group_nickname: Option<String>,
    pub joined_at: i64,
    pub username: Option<String>,
    pub avatar_path: Option<String>,
}

pub async fn group_roster(pool: &Pool, group_id: i64) -> anyhow::Result<Vec<GroupRosterEntry>> {
    let rows = sqlx::query_as::<_, GroupRosterEntry>(
        "SELECT
            gm.peer_id, gm.role, gm.nickname, gm.group_nickname, gm.joined_at,
            u.username,
            COALESCE(u.avatar_path, gm.avatar_path) AS avatar_path
         FROM group_members gm
         LEFT JOIN users u ON u.id = gm.peer_id
         WHERE gm.group_id = ?1
         ORDER BY gm.role DESC, COALESCE(gm.nickname, u.nickname, u.username, gm.peer_id)",
    )
    .bind(group_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Feed item joined with author profile and the viewer's like state.
#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct FeedItem {
    pub id: i64,
    pub author_id: i64,
    pub content: Option<String>,
    pub media_urls: Option<String>,
    pub visibility: i64,
    pub timestamp: i64,
    pub like_count: i64,
    pub comment_count: i64,
    pub author_nickname: Option<String>,
    pub author_username: Option<String>,
    pub author_avatar: Option<String>,
    pub i_liked: bool,
}

/// Global public feed (optional friends-only), including the viewer's like state.
pub async fn feed(pool: &Pool, viewer: i64, include_friend_posts: bool, limit: u32, offset: u32) -> anyhow::Result<Vec<FeedItem>> {
    let author_filter = if include_friend_posts {
        "AND (p.author_id = ?1 OR p.visibility = 0 OR EXISTS(SELECT 1 FROM friendships fr WHERE (fr.user_low = ?1 AND fr.user_high = p.author_id) OR (fr.user_high = ?1 AND fr.user_low = p.author_id)))"
    } else {
        "AND (p.author_id = ?1 OR p.visibility = 0)"
    };
    let sql = format!(
        "SELECT
            p.id, p.author_id, p.content, p.media_urls, p.visibility, p.timestamp,
            p.like_count, p.comment_count,
            u.nickname    AS author_nickname,
            u.username    AS author_username,
            u.avatar_path AS author_avatar,
            EXISTS(SELECT 1 FROM social_likes l WHERE l.post_id = p.id AND l.user_id = ?4) AS i_liked
         FROM social_posts p
         LEFT JOIN users u ON u.id = p.author_id
         WHERE 1=1 {author_filter}
         ORDER BY p.timestamp DESC
         LIMIT ?5 OFFSET ?6"
    );
    let rows: Vec<FeedItem> = sqlx::query_as(&sql)
        .bind(viewer)
        .bind(viewer)
        .bind(viewer)
        .bind(viewer)
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Author's public posts (profile page).
pub async fn author_feed(pool: &Pool, author_id: i64, limit: u32, offset: u32) -> anyhow::Result<Vec<FeedItem>> {
    let rows = sqlx::query_as::<_, FeedItem>(
        "SELECT
            p.id, p.author_id, p.content, p.media_urls, p.visibility, p.timestamp,
            p.like_count, p.comment_count,
            u.nickname    AS author_nickname,
            u.username    AS author_username,
            u.avatar_path AS author_avatar,
            EXISTS(SELECT 1 FROM social_likes l WHERE l.post_id = p.id AND l.user_id = ?1) AS i_liked
         FROM social_posts p
         LEFT JOIN users u ON u.id = p.author_id
         WHERE p.author_id = ?1
         ORDER BY p.timestamp DESC
         LIMIT ?2 OFFSET ?3",
    )
    .bind(author_id)
    .bind(limit as i64)
    .bind(offset as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Comments for a post, with comment-author profile.
#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct CommentWithAuthor {
    pub id: i64,
    pub post_id: i64,
    pub author_id: i64,
    pub content: String,
    pub reply_to: Option<i64>,
    pub created_at: i64,
    pub author_nickname: Option<String>,
    pub author_avatar: Option<String>,
}

pub async fn post_comments(pool: &Pool, post_id: i64, limit: u32, offset: u32) -> anyhow::Result<Vec<CommentWithAuthor>> {
    let rows = sqlx::query_as::<_, CommentWithAuthor>(
        "SELECT
            sc.id, sc.post_id, sc.author_id, sc.content, sc.reply_to, sc.created_at,
            u.nickname    AS author_nickname,
            u.avatar_path AS author_avatar
         FROM social_comments sc
         LEFT JOIN users u ON u.id = sc.author_id
         WHERE sc.post_id = ?1
         ORDER BY sc.created_at
         LIMIT ?2 OFFSET ?3",
    )
    .bind(post_id)
    .bind(limit as i64)
    .bind(offset as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Global search across users (username/nickname) and message text.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    pub kind: &'static str,
    pub id: i64,
    pub text: String,
    pub extra: Option<String>,
}

pub async fn search(pool: &Pool, q: &str, limit: u32) -> anyhow::Result<Vec<SearchHit>> {
    if q.trim().is_empty() {
        return Ok(Vec::new());
    }
    let like = format!("{}{}{}", "%", q.trim(), "%");
    let mut out = Vec::new();

    let mut user_rows = sqlx::query("SELECT id, COALESCE(nickname, username) AS label, username
                                     FROM users WHERE username LIKE ?1 OR nickname LIKE ?1
                                     ORDER BY COALESCE(nickname, username) LIMIT ?2")
        .bind(&like)
        .bind(limit as i64)
        .fetch_all(pool)
        .await?;
    for r in user_rows.drain(..) {
        let id: i64 = r.get("id");
        let label: String = r.get("label");
        let username: Option<String> = r.try_get("username").ok().flatten();
        out.push(SearchHit { kind: "user", id, text: label, extra: username });
    }

    let mut msg_rows = sqlx::query("SELECT m.id, m.text_content, m.conversation_id
                                    FROM messages m WHERE m.text_content LIKE ?1
                                    ORDER BY m.timestamp DESC LIMIT ?2")
        .bind(&like)
        .bind(limit as i64)
        .fetch_all(pool)
        .await?;
    for r in msg_rows.drain(..) {
        let id: i64 = r.get("id");
        let text: Option<String> = r.try_get("text_content").ok().flatten();
        let conv: i64 = r.get("conversation_id");
        out.push(SearchHit { kind: "message", id, text: text.unwrap_or_default(), extra: Some(conv.to_string()) });
    }

    let mut group_rows = sqlx::query("SELECT g.id, g.name, g.owner_id
                                      FROM groups g WHERE g.name LIKE ?1
                                      ORDER BY g.name LIMIT ?2")
        .bind(&like)
        .bind(limit as i64)
        .fetch_all(pool)
        .await?;
    for r in group_rows.drain(..) {
        let id: i64 = r.get("id");
        let name: String = r.get("name");
        let owner: i64 = r.get("owner_id");
        out.push(SearchHit { kind: "group", id, text: name, extra: Some(owner.to_string()) });
    }

    out.truncate(limit as usize);
    Ok(out)
}

/// Convenience: everything a UI needs to render "who is online in this group".
pub async fn group_online_snapshot(pool: &Pool, group_id: i64) -> anyhow::Result<Vec<GroupRosterEntry>> {
    group_roster(pool, group_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        conversations::{self, ConversationPatch}, groups, messages::{self, NewMessage},
        open_memory, users, users::UserPatch,
    };

    #[tokio::test]
    async fn chat_list_resolves_dm_profile_and_group() {
        let pool = open_memory().await.unwrap();

        // DM conversation id == peer user id (2) so the profile join resolves.
        users::upsert(
            &pool,
            2,
            &UserPatch {
                username: Some("bea".into()),
                nickname: Some("Bea".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        conversations::upsert(&pool, 2, &ConversationPatch { type_: Some(0), name: None, peer_id: Some(2), avatar_path: None }).await.unwrap();
        messages::insert(&pool, &NewMessage::text(2, 1, "hello")).await.unwrap();

        // Group conversation
        groups::create(&pool, 10, "Squad", 1, None).await.unwrap();
        groups::add_member(&pool, 10, "peer-b", groups::ROLE_MEMBER).await.unwrap();
        conversations::upsert(&pool, 10, &ConversationPatch { type_: Some(1), name: Some("Squad".into()), peer_id: None, avatar_path: None }).await.unwrap();
        messages::insert(&pool, &NewMessage::text(10, 1, "hi group")).await.unwrap();

        let list = chat_list(&pool, "me", 10).await.unwrap();
        assert_eq!(list.len(), 2);

        let dm = list.iter().find(|c| c.id == 2).unwrap();
        assert_eq!(dm.peer_nickname.as_deref(), Some("Bea"));
        assert_eq!(dm.peer_username.as_deref(), Some("bea"));
        assert_eq!(dm.last_message_preview.as_deref(), Some("hello"));

        let g = list.iter().find(|c| c.id == 10).unwrap();
        assert_eq!(g.group_owner_id, Some(1));
        assert_eq!(g.member_count, Some(2));
    }

    #[tokio::test]
    async fn history_with_sender_resolves_profiles_and_reply() {
        let pool = open_memory().await.unwrap();
        users::upsert(&pool, 10, &UserPatch { nickname: Some("Alice".into()), ..Default::default() }).await.unwrap();
        conversations::upsert(&pool, 100, &ConversationPatch { type_: Some(0), name: None, peer_id: None, avatar_path: None }).await.unwrap();

        let first = messages::insert(&pool, &NewMessage::text(100, 10, "first")).await.unwrap();
        let second = messages::insert(
            &pool,
            &NewMessage {
                conversation_id: 100,
                sender_id: 1,
                msg_type: messages::MSG_TYPE_TEXT,
                text_content: Some("in reply".into()),
                reply_to: Some(first),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let history = history_with_sender(&pool, 100, 10, 0).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].id, second);
        assert_eq!(history[0].reply_to, Some(first));
        assert_eq!(history[0].reply_to_preview.as_deref(), Some("first"));

        let alice_row = history.iter().find(|m| m.sender_id == 10).unwrap();
        assert_eq!(alice_row.sender_nickname.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn friends_with_profile_and_feed_joins() {
        let pool = open_memory().await.unwrap();
        // me & two users
        users::upsert(&pool, 1, &UserPatch { nickname: Some("Me".into()), ..Default::default() }).await.unwrap();
        users::upsert(&pool, 2, &UserPatch { nickname: Some("One".into()), ..Default::default() }).await.unwrap();
        users::upsert(&pool, 3, &UserPatch { nickname: Some("Two".into()), ..Default::default() }).await.unwrap();

        crate::social::add(&pool, 1, 2).await.unwrap();
        crate::social::add(&pool, 1, 3).await.unwrap();
        crate::social::follow(&pool, 2, 1).await.unwrap();

        let friends = friends_with_profile(&pool, 1, 10).await.unwrap();
        assert_eq!(friends.len(), 2);
        let u1 = friends.iter().find(|f| f.user_id == 2).unwrap();
        assert_eq!(u1.nickname.as_deref(), Some("One"));
        assert!(u1.they_follow_me);
        assert!(!u1.i_follow_them);

        let me_follows = friends.iter().find(|f| f.user_id == 3).unwrap();
        assert!(!me_follows.they_follow_me);

        // Feed with i_liked
        let post = crate::social_feed::create_post(&pool, 2, Some("hello feed"), None, 0).await.unwrap();
        crate::social_feed::like(&pool, post.id, 1).await.unwrap();
        let items = feed(&pool, 1, true, 10, 0).await.unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].i_liked);
        assert_eq!(items[0].author_nickname.as_deref(), Some("One"));

        let c = crate::social_feed::add_comment(&pool, post.id, 3, "nice post", None).await.unwrap();
        let comments = post_comments(&pool, post.id, 10, 0).await.unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, c.id);
        assert_eq!(comments[0].author_nickname.as_deref(), Some("Two"));
    }

    #[tokio::test]
    async fn global_search_across_users_messages_groups() {
        let pool = open_memory().await.unwrap();
        users::upsert(&pool, 1, &UserPatch { username: Some("findable".into()), nickname: Some("Findable".into()), ..Default::default() }).await.unwrap();
        conversations::upsert(&pool, 100, &ConversationPatch { type_: Some(0), name: None, peer_id: None, avatar_path: None }).await.unwrap();
        messages::insert(&pool, &NewMessage::text(100, 1, "needle in haystack")).await.unwrap();
        groups::create(&pool, 10, "Squadron", 1, None).await.unwrap();

        let hits = search(&pool, "find", 10).await.unwrap();
        assert!(hits.iter().any(|h| h.kind == "user" && h.text == "Findable"));
        let hits = search(&pool, "needle", 10).await.unwrap();
        assert!(hits.iter().any(|h| h.kind == "message" && h.text.contains("needle")));
        let hits = search(&pool, "squadron", 10).await.unwrap();
        assert!(hits.iter().any(|h| h.kind == "group" && h.text == "Squadron"));

        assert!(search(&pool, "", 10).await.unwrap().is_empty());
    }
}
