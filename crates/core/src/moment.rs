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
    /// Error from the serving device.
    Err { msg: String },
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
}

/// Serve an incoming `PostRequest` by reading our own `social_posts` table
/// and local media files. Called from `swarm_loop` (which owns the tokio
/// runtime) so this can `.await`.
pub async fn serve(req: PostRequest, pool: Option<SqlitePool>) -> anyhow::Result<PostPayload> {
    let pool = pool.ok_or_else(|| anyhow::anyhow!("no sqlite pool configured"))?;
    match req.op {
        PostReqOp::List { since, until } => {
            let rows = sqlite::social_feed::posts_between(&pool, since, until).await?;
            let posts: Vec<PostMeta> = rows
                .into_iter()
                .map(|p| {
                    let n_media = p.media_urls.as_deref().map(json_arr_len).unwrap_or(0) as u32;
                    let n_previews = p
                        .preview_urls
                        .as_deref()
                        .map(json_arr_len)
                        .unwrap_or(0) as u32;
                    PostMeta {
                        user_name: p.user_name,
                        content: p.content,
                        timestamp: p.timestamp,
                        n_media,
                        n_previews,
                    }
                })
                .collect();
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
                .ok_or_else(|| anyhow::anyhow!("media index {which} out of range"))?;
            let bytes = std::fs::read(path)
                .map_err(|e| anyhow::anyhow!("read {path}: {e}"))?;
            Ok(PostPayload::Image { bytes })
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
