//! Moment (post) cross-device sync — the *receiving* side.
//!
//! When a friend's client publishes a new `social_post`, it notifies the
//! signal server with `(user_id, peer_id, post_ts)`. This module:
//!
//! 1. Asks the server "who published a post in `(since, now]`?".
//! 2. Filters the answer to our friends (by `peer_id`).
//! 3. For each friend (newest-first), fetches post metadata + image bytes
//!    over the binary moment channel.
//! 4. Stores the post locally with a fresh local `id`, the friend's
//!    local `users.id` as `author_id`, and the locally-stored media
//!    paths in `media_urls`/`preview_urls`.
//! 5. Advances our per-friend `moment_sync` cursor.
//! 6. Pushes each row to the in-memory Discover list one at a time on the
//!    Slint event-loop thread.

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
    username: String,
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
        let username = f.username.unwrap_or_else(|| me.clone());
        let since_ts = sqlite::moment_sync::read(&pool, &peer)
            .await
            .ok()
            .flatten()
            .unwrap_or(0);
        friend_by_peer.insert(
            peer,
            FriendCursor {
                username,
                local_users_id: f.user_id,
                since_ts,
            },
        );
    }
    if friend_by_peer.is_empty() {
        eprintln!("[moment] no friends with a dialable device — nothing to sync");
        return Ok(());
    }

    // Ask the server for post pointers in `(earliest_cursor, now]`.
    let earliest_cursor = friend_by_peer.values().map(|f| f.since_ts).min().unwrap_or(0);
    let now_ts = sqlite::now_ms();
    let pointers: Vec<PostPointer> = client
        .dir()
        .posts_since(earliest_cursor, now_ts)
        .unwrap_or_default();
    eprintln!(
        "[moment] server returned {} pointer(s); {} friend(s)",
        pointers.len(),
        friend_by_peer.len()
    );

    // Work = one entry per friend that has a newer post than our cursor.
    // `upper` is the newest post_ts that friend published (the inclusive
    // window bound used in `request_post_list`); the friend's *entire* new
    // post set is then fetched in a single round-trip, so we never re-pull or
    // duplicate a post.
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
        eprintln!("[moment] no new posts for our friends — nothing to fetch");
        return Ok(());
    }

    let mut nodes: Vec<SyncedNode> = Vec::new();
    for (_upper, peer_id) in work {
        let f = match friend_by_peer.get(&peer_id) {
            Some(f) => f.clone(),
            None => continue,
        };
        let posts = match client
            .request_post_list(&peer_id, f.since_ts, _upper)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[moment] request_post_list({peer_id}) failed: {e}");
                continue;
            }
        };
        let mut post_metas: Vec<PostMeta> =
            posts.into_iter().filter(|m| m.timestamp > f.since_ts).collect();
        if post_metas.is_empty() {
            continue;
        }
        // Newest first within this friend.
        post_metas.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        let mut friend_newest: i64 = 0;
        for meta in &post_metas {
            friend_newest = friend_newest.max(meta.timestamp);
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
                meta.content.as_deref(),
                if media_json.is_empty() { None } else { Some(media_json.as_str()) },
                if preview_json.is_empty() { None } else { Some(preview_json.as_str()) },
                0,
            )
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "[moment] create_post for friend={peer_id} ts={} failed: {e}",
                        meta.timestamp
                    );
                    continue;
                }
            };

            let media: Vec<String> = serde_json::from_str(&media_json).unwrap_or_default();
            let previews: Vec<String> = serde_json::from_str(&preview_json).unwrap_or_default();
            nodes.push(SyncedNode {
                author: f.username.clone(),
                content: meta.content.clone().unwrap_or_default(),
                media,
                previews,
                post_id: created.id,
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

/// Fetch every `meta.n_media` originals (or `n_previews` previews) for the
/// post, storing them under `group` in the local post-store. Returns the
/// JSON array of stored absolute paths (empty string when the post has
/// no media in that category).
async fn fetch_and_store_media(
    client: &Client<Directory>,
    peer: &str,
    meta: &PostMeta,
    group: &str,
    is_original: bool,
) -> anyhow::Result<String> {
    let n = if is_original { meta.n_media } else { meta.n_previews };
    if n == 0 {
        return Ok(String::new());
    }
    let mut out = Vec::with_capacity(n as usize);
    for which in 0..n {
        let bytes = client
            .request_post_image(peer, meta.timestamp, which, if is_original { 0 } else { 1 })
            .await?;
        let stored = if is_original {
            camera::posts::store_bytes(group, which + 1, "jpg", &bytes)
                .map_err(|e| anyhow::anyhow!("store_bytes: {e}"))?
        } else {
            camera::posts::store_preview_bytes(group, which + 1, "jpg", &bytes)
                .map_err(|e| anyhow::anyhow!("store_preview_bytes: {e}"))?
        };
        out.push(stored.to_string_lossy().into_owned());
    }
    if out.is_empty() {
        Ok(String::new())
    } else {
        Ok(serde_json::to_string(&out).unwrap_or_default())
    }
}

/// Push each synced node to the in-memory Discover row set (newest-first,
/// one at a time) and refresh the Slint views. The row is inserted at the
/// top of `discover`, so the user sees each new post appear at the very
/// top in the order they were inserted.
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
            likes: 0,
            liked: false,
            cover,
        });
    }
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            {
                let mut g = backend.blocking_write();
                for r in rows.iter().rev() {
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
