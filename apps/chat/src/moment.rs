use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use chatx_core::moment::PostMeta;
use chatx_core::signal::PostPointer;
use slint::{ComponentHandle, Weak};

use crate::data::{ArcBackend, DiscoverRow};
use crate::{AppState, Directory, MainWindow};
use chatx_core::Client;

/// One friend to sync. The `HashMap` we build is already keyed by `peer_id`,
/// so this struct only carries the fields we actually look up on each round.
#[derive(Debug, Clone)]
struct FriendCursor {
    /// Best display name for the friend (nickname, falling back to username).
    display: String,
    local_users_id: i64,
    since_ts: i64,
}

/// One synced post, ready to push to the UI.
struct SyncedNode {
    author: String,
    content: String,
    previews: Vec<String>,
    media: Vec<String>,
    post_id: i64,
    /// Wire-carried denormalized counters (authoritative from the sender).
    likes: i64,
    recommends: i64,
    comments: i64,
    /// The post's canonical owner peer (from the wire; the publisher's device).
    /// Used to route a viewer's like/comment back to the owner (B path).
    owner_peer: String,
    /// The post's immutable cross-device identity (the wire's `post_uid`).
    post_uid: i64,
}

static ROUND_RUNNING: AtomicBool = AtomicBool::new(false);

/// Trigger a sync round (idempotent: a second call while one is running is
/// a no-op). Called from the `tab-changed(2)` Slint callback and from a
/// background timer.
pub fn trigger_round(weak: Weak<MainWindow>) {
    if ROUND_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let rt = crate::runtime();
    rt.spawn(async move {
        let _ = run_round(weak).await;
        ROUND_RUNNING.store(false, Ordering::SeqCst);
    });
}

/// Start a background 60-second timer that fires a sync round on every tick
/// (cheap when there is no work). Gives "切到 Discover 就请求" a fallback so
/// a missed `tab-changed` event still gets picked up within a minute.
pub fn start_timer(weak: Weak<MainWindow>) {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let rt = crate::runtime();
    rt.spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            trigger_round(weak.clone());
        }
    });
}

/// Apply the wire-carried reactions (likers, recommenders, comments) of a
/// pulled post to the local DB. Maps the core wire entries (username-keyed)
/// to the sqlite tuple form and upserts them. Returns `true` when any new
/// reaction/comment landed locally — the caller uses that to decide whether
/// to re-publish the feed views.
async fn apply_post_reactions(
    pool: &sqlx::SqlitePool,
    post_id: i64,
    meta: &PostMeta,
) -> anyhow::Result<bool> {
    let likers: Vec<(String, Option<String>)> =
        meta.likers.iter().map(|l| (l.username.clone(), l.nickname.clone())).collect();
    let recommenders: Vec<(String, Option<String>)> = meta
        .recommenders
        .iter()
        .map(|r| (r.username.clone(), r.nickname.clone()))
        .collect();
    let comments: Vec<(String, Option<String>, String, i64)> = meta
        .comments
        .iter()
        .map(|c| (c.username.clone(), c.nickname.clone(), c.content.clone(), c.created_at))
        .collect();

    let any_like = sqlite::social_feed::apply_incoming_likers(pool, post_id, &likers).await?;
    let any_rec = sqlite::social_feed::apply_incoming_recommenders(pool, post_id, &recommenders).await?;
    let any_cmt = sqlite::social_feed::apply_incoming_comments(pool, post_id, &comments).await?;
    Ok(any_like || any_rec || any_cmt)
}

/// One sync round.
async fn run_round(weak: Weak<MainWindow>) -> anyhow::Result<()> {
    let client = {
        let g = crate::CLIENT.lock().unwrap();
        g.clone()
    };
    let Some(client) = client else {
        return Ok(());
    };
    let pool = {
        let g = crate::POOL.lock().unwrap();
        g.clone()
    };
    let Some(pool) = pool else {
        return Ok(());
    };
    let backend = {
        let g = crate::BACKEND.lock().unwrap();
        g.clone()
    };
    let Some(backend) = backend else {
        return Ok(());
    };
    let me = crate::current_me();
    if me.is_empty() {
        return Ok(());
    }
    let me_id = match sqlite::users::get_by_username(&pool, &me).await? {
        Some(u) => u.id,
        None => return Ok(()),
    };
    let friends = sqlite::joins::friends_with_profile(&pool, me_id, 500).await?;

    let mut friend_by_peer: HashMap<String, FriendCursor> = HashMap::new();
    for f in friends {
        let Some(peer) = f.device_peer_id else {
            continue;
        };
        if peer.is_empty() {
            continue;
        }
        let display = f
            .nickname
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| f.username.clone().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| me.clone());
        let since_ts = sqlite::moment_sync::read(&pool, &peer)
            .await
            .ok()
            .flatten()
            .unwrap_or(0);
        friend_by_peer.insert(
            peer,
            FriendCursor {
                display,
                local_users_id: f.user_id,
                since_ts,
            },
        );
    }
    if friend_by_peer.is_empty() {
        tracing::debug!("[moment] no friends with a dialable device — nothing to sync");
        return Ok(());
    }

    // Ask the server for post pointers in `(earliest_cursor, now]`.
    let earliest_cursor = friend_by_peer
        .values()
        .map(|f| f.since_ts)
        .min()
        .unwrap_or(0);
    let now_ts = sqlite::now_ms();
    let pointers: Vec<PostPointer> = client
        .dir()
        .posts_since(earliest_cursor, now_ts)
        .unwrap_or_default();
    let mut work: Vec<(i64, String)> = Vec::new();
    for (peer, f) in &friend_by_peer {
        let upper = pointers
            .iter()
            .filter(|p| &p.peer_id == peer && p.post_ts > f.since_ts)
            .map(|p| p.post_ts)
            .max()
            .unwrap_or(0);
        if upper > 0 {
            work.push((upper, peer.clone()));
        }
    }
    // Newest friend first (stable tie-break by peer).
    work.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    if work.is_empty() {
        return Ok(());
    }

    let mut nodes: Vec<SyncedNode> = Vec::new();
    for (_upper, peer_id) in work {
        let f = match friend_by_peer.get(&peer_id) {
            Some(f) => f.clone(),
            None => continue,
        };
        let posts = match client.request_post_list(&peer_id, f.since_ts, _upper).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("[moment] request_post_list({peer_id}) failed: {e}");
                continue;
            }
        };
        let mut post_metas: Vec<PostMeta> = posts
            .into_iter()
            .filter(|m| m.timestamp > f.since_ts)
            .collect();
        if post_metas.is_empty() {
            continue;
        }
        // Newest first within this friend.
        post_metas.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        let mut friend_newest: i64 = 0;
        for meta in &post_metas {
            friend_newest = friend_newest.max(meta.timestamp);
            // Display name fallback, resolved once; used both for the
            // dedup identity check below and the stored row.
            let user_name = meta
                .user_name
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(f.display.as_str());

            // If we already have this post, check whether its denormalized
            // counters (like / recommend / comment) have changed on the sender
            // side and, if so, overwrite them locally. This is the path that
            // keeps B (like/recommend counts) and C (comment count) in sync
            // across clients — the wire carries the sender's current counts,
            // so a re-pull after a friend's `add_post_comment` /
            // `toggle_detail_like` refreshes the receiver's local copy without
            // re-fetching media or re-inserting the row.
            let existing = sqlite::social_feed::get_post_by_identity(
                &pool,
                f.local_users_id,
                user_name,
                meta.content.as_deref(),
            )
            .await
            .ok()
            .flatten();
            if let Some(existing) = existing {
                // Apply any likers / recommenders / comments that arrived with
                // this re-pull (idempotent upserts; safe if already present).
                let any_new_reactions =
                    apply_post_reactions(&pool, existing.id, meta).await.unwrap_or(false);
                let dirty = existing.like_count != meta.like_count
                    || existing.recommend_count != meta.recommend_count
                    || existing.comment_count != meta.comment_count
                    || any_new_reactions;
                if dirty {
                    if let Err(e) = sqlite::social_feed::set_counts(
                        &pool,
                        existing.id,
                        meta.like_count,
                        meta.recommend_count,
                        meta.comment_count,
                    )
                    .await
                    {
                        tracing::warn!(
                            "[moment] set_counts for post id={} failed: {e}",
                            existing.id
                        );
                    }
                    // Refresh the in-memory feed row so the discover card
                    // reflects the new counts without a re-enter.
                    if let Some(ui) = weak.upgrade() {
                        let state = ui.global::<AppState>();
                        crate::publish_to_views(&state, backend.clone());
                    }
                }
                continue;
            }
            // Per-post media folder (timestamp-suffixed) so different posts
            // never collide even though they share the friend's base group.
            let group = format!("sync_{}_{}", peer_short(&peer_id), meta.timestamp);
            let media_json = fetch_and_store_media(&client, &peer_id, meta, &group, true)
                .await
                .unwrap_or_default();
            let preview_json = fetch_and_store_media(&client, &peer_id, meta, &group, false)
                .await
                .unwrap_or_default();

            let created = match sqlite::social_feed::create_post(
                &pool,
                f.local_users_id,
                Some(user_name),
                meta.content.as_deref(),
                if media_json.is_empty() {
                    None
                } else {
                    Some(media_json.as_str())
                },
                if preview_json.is_empty() {
                    None
                } else {
                    Some(preview_json.as_str())
                },
                0,
                // Store the friend's wire identity as the row's timestamp, so
                // cursor windows, `get_post_by_ts` and dedup all agree.
                meta.timestamp,
            )
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "[moment] create_post for friend={peer_id} ts={} failed: {e}",
                        meta.timestamp
                    );
                    continue;
                }
            };

            // Rebuild the local reaction tables (likers / recommenders /
            // comments) for this freshly-created post.
            if let Err(e) = apply_post_reactions(&pool, created.id, meta).await {
                tracing::warn!("[moment] apply_post_reactions for post id={} failed: {e}", created.id);
            }
            // Preserve the post's immutable cross-device identity (the wire's
            // `post_uid`) so a later `React` resolves to the same post on every
            // device. Fall back to minting one if this row predates the feature.
            if meta.post_uid != 0 {
                if let Err(e) = sqlite::social_feed::set_post_uid(&pool, created.id, meta.post_uid).await {
                    tracing::warn!("[moment] set_post_uid for post id={} failed: {e}", created.id);
                }
            } else {
                let uid = sqlite::new_id();
                let _ = sqlite::social_feed::set_post_uid(&pool, created.id, uid).await;
            }
            // Record the canonical owner peer. Prefer the wire's `owner_peer`
            // (the publisher's device); fall back to the peer we pulled from
            // when the sender is pre-feature.
            let owner_peer = if meta.owner_peer.is_empty() {
                peer_id.clone()
            } else {
                meta.owner_peer.clone()
            };
            if let Err(e) = sqlite::social_feed::set_source_peer(&pool, created.id, &owner_peer).await {
                tracing::warn!("[moment] set_source_peer for post id={} failed: {e}", created.id);
            }

            let media: Vec<String> = serde_json::from_str(&media_json).unwrap_or_default();
            let previews: Vec<String> = serde_json::from_str(&preview_json).unwrap_or_default();
            nodes.push(SyncedNode {
                author: user_name.to_string(),
                content: meta.content.clone().unwrap_or_default(),
                media,
                previews,
                post_id: created.id,
                likes: meta.like_count,
                recommends: meta.recommend_count,
                comments: meta.comment_count,
                owner_peer,
                post_uid: meta.post_uid,
            });
        }
        sqlite::moment_sync::upsert(&pool, &peer_id, friend_newest).await?;
    }

    if !nodes.is_empty() {
        push_nodes_to_ui(weak, backend, nodes).await?;
    }
    Ok(())
}

/// A short, filesystem-safe suffix of a base58 peer_id for use in media
/// group folders.
fn peer_short(peer: &str) -> String {
    let mut s = String::new();
    for c in peer.chars().take(8) {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        }
    }
    if s.len() < 4 {
        s.push_str("peer");
    }
    s
}

async fn fetch_and_store_media(
    client: &Client<Directory>,
    peer: &str,
    meta: &PostMeta,
    group: &str,
    is_original: bool,
) -> anyhow::Result<String> {
    let n = if is_original {
        meta.n_media
    } else {
        meta.n_previews
    };
    if n == 0 {
        return Ok(String::new());
    }
    let mut out = Vec::with_capacity(n as usize);
    for which in 0..n {
        let bytes = client
            .request_post_image(peer, meta.timestamp, which, if is_original { 0 } else { 1 })
            .await?;
        // Store by the container the bytes actually carry (sniffed), not a
        // hard-coded "jpg": WebP is transcoded to JPEG, HEIC kept honest —
        // see `camera::posts::store_received_image`.
        let stored = camera::posts::store_received_image(group, which + 1, !is_original, &bytes)
            .map_err(|e| anyhow::anyhow!("store_received_image: {e}"))?;
        out.push(stored.to_string_lossy().into_owned());
    }
    if out.is_empty() {
        Ok(String::new())
    } else {
        Ok(serde_json::to_string(&out).unwrap_or_default())
    }
}

async fn push_nodes_to_ui(
    weak: Weak<MainWindow>,
    backend: ArcBackend,
    nodes: Vec<SyncedNode>,
) -> anyhow::Result<()> {
    let mut rows: Vec<DiscoverRow> = Vec::new();
    for n in nodes {
        let cover = n
            .previews
            .first()
            .cloned()
            .or_else(|| n.media.first().cloned())
            .unwrap_or_default();
        rows.push(DiscoverRow {
            key: 0,
            post_id: n.post_id,
            title: n.content,
            author: n.author,
            likes: n.likes,
            liked: false,
            recommends: n.recommends,
            recommended: false,
            comments: n.comments,
            source_peer_id: Some(n.owner_peer),
            post_uid: n.post_uid,
            cover,
        });
    }
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            {
                let mut g = backend.blocking_write();
                for r in rows.iter().rev() {
                    // Safety net: never surface a post the in-memory feed
                    // already shows (a second insert with a fresh key would
                    // render the same post twice).
                    if g.has_post(r.post_id) {
                        continue;
                    }
                    let key = g.key_for_post(r.post_id);
                    g.discover.insert(0, DiscoverRow { key, ..r.clone() });
                }
            }
            let state = ui.global::<AppState>();
            crate::publish_to_views(&state, backend.clone());
        }
    });
    Ok(())
}
