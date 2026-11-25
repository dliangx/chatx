//! Wire format for cross-device moment/post sync, carried over the binary
//! (CBOR) request-response channel `/chatx/moment/1`.
//!
//! The CBOR codec serializes `Vec<u8>` as a compact byte-string, so image
//! bytes travel as raw bytes (no base64).
//!
//! `PostRequest::id` correlates a request with its [`PostResponse`] so the
//! sender can match the oneshot waiter in `Running::pending_blobs`.
//!
//! We never carry our own device-local `social_posts.id` or `author_id` —
//! those are only meaningful on the device that created them. The receiver
            //! resolves the remote author (by friend username) to a local `users` row id
            //! (and stamps the post's `user_name` from the wire metadata)
//! and generates its own post `id`.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

/// A viewer reacted to a post we own. The app registers a handler to (1) bump
/// the local post's timestamp + notify the signal server so *our* friends
/// re-pull the now-updated post, and (2) refresh the feed views. `post_id` is
/// our local id of the post the reaction targeted; `username` is the reacting
/// viewer. Fired from `serve()` on the swarm task.
pub type ReactHandler = Arc<
    dyn Fn(i64, &str) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
>;

static REACT_HANDLER: OnceLock<ReactHandler> = OnceLock::new();

/// Register the React handler (idempotent — first call wins). Returns false if
/// one was already set (the app uses this to know whether to skip).
pub fn set_react_handler(h: ReactHandler) -> bool {
    REACT_HANDLER.set(h).is_ok()
}

fn react_handler() -> Option<&'static ReactHandler> {
    REACT_HANDLER.get()
}

/// The request the syncing client sends to a friend's device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostRequest {
    /// Application-level correlation id (unique per in-flight request).
    pub id: u64,
    pub op: PostReqOp,
}

/// What the requester wants from a friend's device.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PostReqOp {
    /// Metadata for all posts in `(since, until]` (millisecond timestamps).
    List { since: i64, until: i64 },
    /// Raw bytes of one image.
    /// `ts`    — the post timestamp (matches `social_posts.timestamp`).
    /// `which` — 0-based index into the post's media array.
    /// `kind`  — 0 = original, 1 = preview.
    Image {
        ts: i64,
        which: u32,
        #[serde(rename = "kind")]
        kind: u32,
    },
    /// A viewer's reaction (like/comment/recommend) to a post the owner
    /// published. The owner applies it to their own copy and re-broadcasts
    /// to their friends. `post_uid` is the immutable cross-device identity of
    /// the post (minted by the publisher, carried on the wire); `reaction` is
    /// the action data.
    React {
        post_uid: i64,
        reaction: ReactionWire,
    },
}

/// The response the friend's device returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostResponse {
    /// Mirrors `PostRequest::id` so the sender can match the waiter.
    pub id: u64,
    pub payload: PostPayload,
}

/// The body of a response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PostPayload {
    /// Metadata for posts returned by a `List` request.
    List { posts: Vec<PostMeta> },
    /// Raw image bytes returned by an `Image` request.
    Image { bytes: Vec<u8> },
    /// Ack for a `React` request — the owner applied the reaction and is
    /// (re-)broadcasting it to their friends. `echo` is the post identity the
    /// viewer sent, so the sender can confirm which reaction landed.
    ReactAck { applied: bool, echo: Option<ReactionWire> },
    /// Error from the serving device.
    Err { msg: String },
}

/// A viewer's reaction to a post, sent to the post's canonical owner.
/// Username-keyed (device-local ids are not portable). `Comment` carries the
/// full body + original creation time so the owner can idempotently upsert.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReactionWire {
    Like { username: String },
    Recommend { username: String },
    Comment { username: String, content: String, created_at: i64 },
}

/// A single post's metadata (does not include image bytes — the client
/// issues separate `Image` requests per index).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostMeta {
    /// Display name of the original author (at post time).
    #[serde(default)]
    pub user_name: Option<String>,
    pub content: Option<String>,
    pub timestamp: i64,
    /// Number of original images for this post.
    #[serde(default)]
    pub n_media: u32,
    /// Number of preview images for this post.
    #[serde(default)]
    pub n_previews: u32,
    /// Denormalized like count at pull time (read from the sender's
    /// `social_posts.like_count`). Old senders omit this — defaults to 0.
    #[serde(default)]
    pub like_count: i64,
    /// Denormalized recommend count at pull time.
    #[serde(default)]
    pub recommend_count: i64,
    /// Denormalized comment count at pull time.
    #[serde(default)]
    pub comment_count: i64,
    /// Liker identities (usernames + nicknames), so the receiver can rebuild
    /// the `social_likes` table locally. Empty on old senders.
    #[serde(default)]
    pub likers: Vec<LikeEntry>,
    /// Recommender identities. Same shape as likers.
    #[serde(default)]
    pub recommenders: Vec<RecommendEntry>,
    /// Full comment rows (with commenter identity + content + ts). The
    /// receiver upserts these into its own `social_comments` table — this is
    /// how comment content (not just the count) reaches a friend's client.
    #[serde(default)]
    pub comments: Vec<CommentEntry>,
    /// The post's immutable cross-device identity (minted once by the
    /// publisher). The receiver stores it as-is on the row it creates so it
    /// matches on every device — the key a later `React` op resolves on.
    #[serde(default)]
    pub post_uid: i64,
    /// The canonical owner peer (publisher's base58 PeerId). The receiver
    /// keeps this on the row it creates so a viewer can dial the publisher
    /// directly (not an intermediate relay) for a `React`.
    #[serde(default)]
    pub owner_peer: String,
}

/// One liker / recommender identity on the wire. We carry usernames (globally
/// unique within the friend graph) rather than device-local `user_id`s, since
/// those are only meaningful on the device that minted them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LikeEntry {
    pub username: String,
    #[serde(default)]
    pub nickname: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecommendEntry {
    pub username: String,
    #[serde(default)]
    pub nickname: Option<String>,
}

/// One comment row on the wire. Carries the commenter's username so the
/// receiver can resolve (or stub-create) a local `users` row; the content is
/// the actual comment body; the timestamp is the original comment creation
/// time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentEntry {
    pub username: String,
    #[serde(default)]
    pub nickname: Option<String>,
    pub content: String,
    pub created_at: i64,
}

/// Serve an incoming `PostRequest` by reading our own `social_posts` table
/// and local media files. Called from `swarm_loop` (which owns the tokio
/// runtime) so this can `.await`.
/// Serve an incoming `PostRequest` by reading our own `social_posts` table
/// and local media files. `requester` is the base58 peer id of the dialing
/// device (used for the `React` trust check). Called from `swarm_loop`
/// (which owns the tokio runtime) so this can `.await`.
pub async fn serve(
    req: PostRequest,
    pool: Option<SqlitePool>,
    requester: &str,
) -> anyhow::Result<PostPayload> {
    let pool = pool.ok_or_else(|| anyhow::anyhow!("no sqlite pool configured"))?;
    match req.op {
        PostReqOp::List { since, until } => {
            let rows = sqlite::social_feed::posts_between(&pool, since, until).await?;
            let mut posts: Vec<PostMeta> = Vec::with_capacity(rows.len());
            for p in rows {
                let n_media = p.media_urls.as_deref().map(json_arr_len).unwrap_or(0) as u32;
                let n_previews = p
                    .preview_urls
                    .as_deref()
                    .map(json_arr_len)
                    .unwrap_or(0) as u32;
                // Pull the likers / recommenders / comments so the receiver
                // can rebuild its local tables (username-keyed, since ids are
                // device-local). Old senders still get empty arrays.
                let likers = sqlite::social_feed::likers_with_profile(&pool, p.id)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| LikeEntry {
                        username: a.username,
                        nickname: a.nickname,
                    })
                    .collect();
                let recommenders = sqlite::social_feed::recommenders_with_profile(&pool, p.id)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| RecommendEntry {
                        username: a.username,
                        nickname: a.nickname,
                    })
                    .collect();
                let comments = sqlite::social_feed::comments_with_profile(&pool, p.id)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| CommentEntry {
                        username: c.username,
                        nickname: c.nickname,
                        content: c.content,
                        created_at: c.created_at,
                    })
                    .collect();
                // Stamp a stable cross-device identity if this row predates the
                // feature, so the receiver gets one that matches on every peer.
                let post_uid = match p.post_uid {
                    Some(u) => u,
                    None => {
                        let u = sqlite::new_id();
                        let _ = sqlite::social_feed::set_post_uid(&pool, p.id, u).await;
                        u
                    }
                };
                let owner_peer = p.source_peer_id.clone().unwrap_or_default();
                posts.push(PostMeta {
                    user_name: p.user_name,
                    content: p.content,
                    timestamp: p.timestamp,
                    n_media,
                    n_previews,
                    like_count: p.like_count,
                    recommend_count: p.recommend_count,
                    comment_count: p.comment_count,
                    likers,
                    recommenders,
                    comments,
                    post_uid,
                    owner_peer,
                });
            }
            Ok(PostPayload::List { posts })
        }
        PostReqOp::Image { ts, which, kind } => {
            let row = sqlite::social_feed::get_post_by_ts(&pool, ts)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no post with timestamp {ts}"))?;
            let list = match kind {
                0 => row.media_urls,
                _ => row.preview_urls,
            };
            let paths = parse_json_list(&list)?;
            let path = paths
                .get(which as usize)
                .ok_or_else(|| anyhow::anyhow!("media index {which} out of bounds"))?;
            let bytes = std::fs::read(path)
                .map_err(|e| anyhow::anyhow!("read {path}: {e}"))?;
            Ok(PostPayload::Image { bytes })
        }
        PostReqOp::React { post_uid, reaction } => {
            // Resolve by the immutable cross-device identity.
            let Some(post) = sqlite::social_feed::get_post_by_uid(&pool, post_uid).await? else {
                return Ok(PostPayload::ReactAck {
                    applied: false,
                    echo: Some(reaction),
                });
            };
            // Trust check: only a device that has an active friendship with the
            // post's author may push a reaction. Prevents strangers (or peers
            // who merely relayed the post) from fabricating likes/comments.
            let trusted = sqlite::social_feed::requester_is_friend_of(
                &pool,
                post.author_id,
                requester,
            )
            .await
            .unwrap_or(false);
            if !trusted {
                tracing::warn!(
                    "[moment] REACT rejected: requester={requester} not a friend of post author user_id={}",
                    post.author_id
                );
                return Ok(PostPayload::ReactAck {
                    applied: false,
                    echo: Some(reaction),
                });
            }

            // Apply the reaction to local tables.
            let applied = match &reaction {
                ReactionWire::Like { username } => {
                    sqlite::social_feed::apply_incoming_likers(
                        &pool,
                        post.id,
                        &[(username.clone(), None)],
                    )
                    .await
                    .unwrap_or(false)
                }
                ReactionWire::Recommend { username } => {
                    sqlite::social_feed::apply_incoming_recommenders(
                        &pool,
                        post.id,
                        &[(username.clone(), None)],
                    )
                    .await
                    .unwrap_or(false)
                }
                ReactionWire::Comment {
                    username,
                    content: body,
                    created_at,
                } => {
                    sqlite::social_feed::apply_incoming_comments(
                        &pool,
                        post.id,
                        &[(username.clone(), None, body.clone(), *created_at)],
                    )
                    .await
                    .unwrap_or(false)
                }
            };

            // Fire the app-level handler so the owner re-broadcasts to their
            // friends and refreshes the feed view.
            if applied {
                if let Some(handler) = react_handler() {
                    let username = match &reaction {
                        ReactionWire::Like { username }
                        | ReactionWire::Recommend { username }
                        | ReactionWire::Comment { username, .. } => username.clone(),
                    };
                    let post_id = post.id;
                    handler(post_id, &username).await;
                }
            }

            Ok(PostPayload::ReactAck {
                applied,
                echo: Some(reaction),
            })
        }
    }
}

/// Count elements in a JSON array encoded in a string (returns 0 if the
/// string is not a JSON array of any shape the app writes).
fn json_arr_len(s: &str) -> usize {
    let v: Vec<serde_json::Value> = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    v.len()
}

fn parse_json_list(s: &Option<String>) -> anyhow::Result<Vec<String>> {
    let Some(raw) = s else {
        return Ok(Vec::new());
    };
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let v: Vec<String> =
        serde_json::from_str(raw).map_err(|e| anyhow::anyhow!("bad json list: {e}"))?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_arr_len_counts_top_level() {
        assert_eq!(json_arr_len(""), 0);
        assert_eq!(json_arr_len("not json"), 0);
        assert_eq!(json_arr_len("[]"), 0);
        assert_eq!(json_arr_len(r#"["a"]"#), 1);
        assert_eq!(json_arr_len(r#"["a","b","c"]"#), 3);
        assert_eq!(json_arr_len(r#"["/x/a.jpg" , "/x/b.jpg"]"#), 2);
    }
}
