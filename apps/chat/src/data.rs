//! App-side in-memory data layer for the chat / contacts / discovery tabs.
//!
//! Strategy (per the product requirements):
//!   - At startup (after login) we load rows from SQLite into memory
//!     (`DataBackend`) and push them to the slint views.
//!   - While running, the UI reads from memory (the slint row arrays are the
//!     memory). Any data change first updates memory, then asynchronously
//!     persists to SQLite.
//!   - We never read SQLite on a per-frame basis; only on start or when the
//!     caller asks for a full refresh.

use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct ChatRow {
    /// Stable display integer id handed to slint (navigation payload).
    pub key: i32,
    /// Underlying conversation id (DM peer id or group id).
    pub chat_id: String,
    pub title: String,
    pub preview: String,
    pub is_group: bool,
    pub time_ms: i64,
    pub unread: i64,
}

#[derive(Debug, Clone)]
pub struct ContactRow {
    pub key: i32,
    pub user_id: String,
    pub peer_id: String,
    pub name: String,
    pub is_following: bool,
    pub follows_me: bool,
}

#[derive(Debug, Clone)]
pub struct DiscoverRow {
    pub key: i32,
    pub post_id: String,
    pub title: String,
    pub author: String,
    pub likes: i64,
    pub liked: bool,
}

/// In-memory state for one account. All reads by the UI go through this.
/// Writes mutate this first, then fire-and-forget persistence to SQLite.
#[derive(Debug, Clone, Default)]
pub struct DataBackend {
    pub me: String,
    pub chats: Vec<ChatRow>,
    pub contacts: Vec<ContactRow>,
    pub discover: Vec<DiscoverRow>,
    chat_key_to_id: HashMap<i32, String>,
    contact_key_to_id: HashMap<i32, String>,
    feed_post_to_key: HashMap<String, i32>,
    next_key: i32,
}

impl DataBackend {
    fn bump(&mut self) -> i32 {
        self.next_key += 1;
        self.next_key
    }

    pub fn chat_id_for(&self, key: i32) -> Option<&str> {
        self.chat_key_to_id.get(&key).map(|s| s.as_str())
    }

    pub fn contact_id_for(&self, key: i32) -> Option<&str> {
        self.contact_key_to_id.get(&key).map(|s| s.as_str())
    }

    pub fn post_id_for(&self, key: i32) -> Option<&str> {
        self.feed_post_to_key.iter().find_map(|(post_id, k)| if *k == key { Some(post_id.as_str()) } else { None })
    }

    /// Apply a like/unlike toggle to memory. Returns the new liked state.
    pub fn toggle_like(&mut self, key: i32) -> (bool, i64) {
        if let Some(row) = self.discover.iter_mut().find(|r| r.key == key) {
            row.liked = !row.liked;
            row.likes += if row.liked { 1 } else { -1 };
            (row.liked, row.likes)
        } else {
            (false, 0)
        }
    }

    /// Apply an unread-count bump / clear for a conversation.
    pub fn clear_unread(&mut self, key: i32) {
        if let Some(row) = self.chats.iter_mut().find(|r| r.key == key) {
            row.unread = 0;
        }
    }
}

pub type ArcBackend = Arc<RwLock<DataBackend>>;

/// Load all rows for `me` straight from SQLite into a fresh backend.
///
/// NOTE: called from an async runtime; safe to call at most once per session.
/// We do not cache the pool inside the backend — the app keeps a copy of the
/// pool for follow-up async persistence.
pub async fn load(pool: &SqlitePool, me: &str) -> anyhow::Result<DataBackend> {
    let mut b = DataBackend { me: me.to_string(), ..Default::default() };

    // ---- chat list ----
    let items = sqlite::joins::chat_list(pool, me, 200).await?;
    for c in items {
        let key = b.bump();
        // Title: group -> its name; DM -> saved name, else the joined peer nickname, else the id.
        let title = if c.type_ == 1 {
            c.name.clone().unwrap_or_else(|| c.id.clone())
        } else {
            c.name
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| c.peer_nickname.clone().filter(|s| !s.is_empty()))
                .or_else(|| c.peer_username.clone().filter(|s| !s.is_empty()))
                .unwrap_or_else(|| c.id.clone())
        };
        b.chat_key_to_id.insert(key, c.id.clone());
        b.chats.push(ChatRow {
            key,
            chat_id: c.id,
            title,
            preview: c.last_message_preview.unwrap_or_default(),
            is_group: c.type_ == 1,
            time_ms: c.last_message_time.unwrap_or(0),
            unread: c.unread_count,
        });
    }

    // ---- contacts (friends) ----
    let friends = sqlite::joins::friends_with_profile(pool, me, 500).await?;
    for f in friends {
        let key = b.bump();
        let name = f.nickname
            .clone()
            .or_else(|| f.username.clone())
            .unwrap_or_else(|| f.user_id.clone());
        let user_id = f.user_id.clone();
        b.contact_key_to_id.insert(key, user_id.clone());
        b.contacts.push(ContactRow {
            key,
            user_id: user_id.clone(),
            // We store peer_id == user_id until devices are exposed through core.
            // The p2p transport can resolve by username/peer later.
            peer_id: user_id,
            name,
            is_following: f.i_follow_them,
            follows_me: f.they_follow_me,
        });
    }

    // ---- discovery feed ----
    let posts = sqlite::joins::feed(pool, me, true, 200, 0).await?;
    for p in posts {
        let key = b.bump();
        b.feed_post_to_key.insert(p.id.clone(), key);
        b.discover.push(DiscoverRow {
            key,
            post_id: p.id,
            title: p.content.unwrap_or_default(),
            author: p.author_nickname
                .unwrap_or_else(|| p.author_username.unwrap_or_default()),
            likes: p.like_count,
            liked: p.i_liked,
        });
    }
    Ok(b)
}

/// Re-read a single conversation chat-list preview & unread from SQLite (e.g.
/// after receiving a message) and patch the in-memory row; returns whether the
/// row changed.
pub async fn refresh_chat_row(pool: &SqlitePool, b: &mut DataBackend, chat_id: &str) -> anyhow::Result<bool> {
    // Use full chat_list to find the matching entry (cheap for a local sqlite on a single user).
    let items = sqlite::joins::chat_list(pool, &b.me, 200).await?;
    let Some(src) = items.iter().find(|c| c.id == chat_id) else {
        // New conversation (e.g. first message from an unknown peer): append.
        let key = b.bump();
        b.chat_key_to_id.insert(key, chat_id.to_string());
        b.chats.push(ChatRow {
            key,
            chat_id: chat_id.into(),
            title: chat_id.into(),
            preview: String::new(),
            is_group: false,
            time_ms: 0,
            unread: 0,
        });
        return Ok(true);
    };
    let Some(row) = b.chats.iter_mut().find(|r| r.chat_id == src.id) else {
        return Ok(false);
    };
    let new = ChatRow {
        key: row.key,
        chat_id: src.id.clone(),
        title: src.name.clone().unwrap_or_default(),
        preview: src.last_message_preview.clone().unwrap_or_default(),
        is_group: src.type_ == 1,
        time_ms: src.last_message_time.unwrap_or(0),
        unread: src.unread_count,
    };
    let changed = new.title != row.title
        || new.preview != row.preview
        || new.unread != row.unread
        || new.time_ms != row.time_ms;
    *row = new;
    Ok(changed)
}

/// Persist a discovery like/unlike toggle to SQLite (fire and forget).
pub async fn persist_like_toggle(pool: &SqlitePool, me: &str, post_id: &str, want_liked: bool) -> anyhow::Result<()> {
    let currently = sqlite::social_feed::has_liked(pool, post_id, me).await?;
    if currently == want_liked {
        return Ok(());
    }
    if want_liked {
        sqlite::social_feed::like(pool, post_id, me).await?;
    } else {
        sqlite::social_feed::unlike(pool, post_id, me).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_like_math() {
        let mut b = DataBackend { me: "me".into(), ..Default::default() };
        let key = 42;
        b.feed_post_to_key.insert("p1".into(), key);
        b.discover.push(DiscoverRow {
            key,
            post_id: "p1".into(),
            title: "hi".into(),
            author: "a".into(),
            likes: 3,
            liked: false,
        });
        let (liked, likes) = b.toggle_like(key);
        assert!(liked);
        assert_eq!(likes, 4);
        let (liked, likes) = b.toggle_like(key);
        assert!(!liked);
        assert_eq!(likes, 3);
    }

    #[tokio::test]
    async fn load_populates_all_three_lists() {
        let pool = sqlite::open_memory().await.unwrap();
        let me = "me";
        // seed a friend + a post
        sqlite::users::upsert(
            &pool,
            "u1",
            &sqlite::users::UserPatch {
                nickname: Some("One".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        sqlite::social::add(&pool, me, "u1").await.unwrap();
        let post = sqlite::social_feed::create_post(&pool, "u1", Some("hello"), None, 0).await.unwrap();
        sqlite::social_feed::like(&pool, &post.id, me).await.unwrap();

        let b = load(&pool, me).await.unwrap();
        assert_eq!(b.contacts.len(), 1);
        assert!(b.contacts[0].name.contains("One"));
        assert_eq!(b.discover.len(), 1);
        assert!(b.discover[0].liked);
        assert_eq!(b.discover[0].likes, 1);
    }
}
