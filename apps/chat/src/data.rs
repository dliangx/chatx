use pinyin::ToPinyin;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct ChatRow {
    pub key: i32,
    /// Underlying conversation key (string chat id: DM pair hash or group id).
    pub chat_id: String,
    pub title: String,
    pub preview: String,
    pub is_group: bool,
    pub time_ms: i64,
    pub unread: i64,
    pub peer_id: String,
    /// Peer/group avatar file path (empty when none).
    pub avatar_path: String,
}

#[derive(Debug, Clone)]
pub struct ContactRow {
    pub key: i32,
    pub user_id: i64,
    pub peer_id: String,
    pub name: String,
    /// WeChat-style A-Z section the contact belongs in (pinyin initial).
    pub letter: char,
    pub is_following: bool,
    pub follows_me: bool,
    pub image: String,
}

pub fn contact_letter(name: &str) -> char {
    for c in name.chars() {
        if c.is_ascii_alphabetic() {
            return c.to_ascii_uppercase();
        }
        if let Some(p) = c.to_pinyin() {
            return p.first_letter().chars().next().unwrap_or('#').to_ascii_uppercase();
        }
    }
    '#'
}

/// Direction of a friend request relative to me.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrqDir {
    /// Someone asked to add me (shows in the Contacts inbox).
    Inbox,
    /// I asked to add them (shows in the add-contact flow as pending).
    Sent,
}

/// One friend request, in the inbox (they asked) or my sent list (I asked).
/// `peer_id` is always the OTHER party's libp2p peer id.
#[derive(Debug, Clone)]
pub struct FriendRequestData {
    pub id: i32,
    pub peer_id: String,
    /// Best-effort display name (username if known, else peer id).
    pub name: String,
    pub time_ms: i64,
    pub dir: FrqDir,
}

#[derive(Debug, Clone)]
pub struct DiscoverRow {
    pub key: i32,
    pub post_id: i64,
    pub title: String,
    pub author: String,
    pub likes: i64,
    pub liked: bool,
    /// First media file path (cover) for the discover card. Empty when the
    /// post has no media.
    pub cover: String,
}

#[derive(Debug, Clone)]
pub struct NoteRowData {
    /// Stable 32-bit UI key (Slint `int` is 32-bit; note ids are 64-bit).
    pub key: i32,
    /// Underlying SQLite note id (64-bit), used for DB writes/deletes.
    pub note_id: i64,
    pub content: String,
    pub created_at: i64,
}

/// In-memory state for one account. All reads by the UI go through this.
/// Writes mutate this first, then fire-and-forget persistence to SQLite.
#[derive(Debug, Clone, Default)]
pub struct DataBackend {
    pub chats: Vec<ChatRow>,
    pub contacts: Vec<ContactRow>,
    pub friend_requests: Vec<FriendRequestData>,
    pub discover: Vec<DiscoverRow>,
    /// My personal notes, newest first.
    pub notes: Vec<NoteRowData>,
    chat_key_to_id: HashMap<i32, String>,
    contact_key_to_id: HashMap<i32, i64>,
    /// Shared key <-> post_id maps so one post is one row key across all feed lists.
    key_to_post: HashMap<i32, i64>,
    post_to_key: HashMap<i64, i32>,
    /// Stable key <-> note id maps (UI keys are 32-bit, note ids are 64-bit).
    key_to_note: HashMap<i32, i64>,
    note_to_key: HashMap<i64, i32>,
    /// Stable UI key -> friend request (dir + other peer) for accept/reject.
    frq_key_to: HashMap<i32, (FrqDir, String)>,
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

    pub fn contact_id_for(&self, key: i32) -> Option<i64> {
        self.contact_key_to_id.get(&key).copied()
    }

    /// Resolve a contact list key to its stored peer/user id text (the value
    /// used for directory lookups), if the contact still exists.
    pub fn peer_id_for(&self, key: i32) -> Option<&str> {
        self.contacts.iter().find(|r| r.key == key).map(|r| r.peer_id.as_str())
    }

    /// Resolve a contact list key to its directory username (the value used
    /// for `/v1/users/` lookups), if the contact still exists.
    pub fn username_for(&self, key: i32) -> Option<&str> {
        self.contacts.iter().find(|r| r.key == key).map(|r| r.name.as_str())
    }

    pub fn post_id_for(&self, key: i32) -> Option<i64> {
        self.key_to_post.get(&key).copied()
    }

    pub fn key_for_post(&mut self, post_id: i64) -> i32 {
        if let Some(k) = self.post_to_key.get(&post_id) {
            return *k;
        }
        let key = self.bump();
        self.post_to_key.insert(post_id, key);
        self.key_to_post.insert(key, post_id);
        key
    }

    pub fn key_for_note(&mut self, note_id: i64) -> i32 {
        if let Some(k) = self.note_to_key.get(&note_id) {
            return *k;
        }
        let key = self.bump();
        self.note_to_key.insert(note_id, key);
        self.key_to_note.insert(key, note_id);
        key
    }

    pub fn note_id_for(&self, key: i32) -> Option<i64> {
        self.key_to_note.get(&key).copied()
    }

    /// Apply a like/unlike toggle to every list that contains `key`.
    /// Returns the new liked state + count of the first row touched.
    pub fn toggle_like(&mut self, key: i32) -> (bool, i64) {
        let mut result = (false, 0);
        for row in self.discover.iter_mut() {
            if row.key == key {
                row.liked = !row.liked;
                row.likes += if row.liked { 1 } else { -1 };
                result = (row.liked, row.likes);
            }
        }
        result
    }

    /// Apply an unread-count bump / clear for a conversation.
    pub fn clear_unread(&mut self, key: i32) {
        if let Some(row) = self.chats.iter_mut().find(|r| r.key == key) {
            row.unread = 0;
        }
    }

    /// Remove a conversation from the list by its UI key.
    pub fn remove_chat(&mut self, key: i32) {
        self.chats.retain(|r| r.key != key);
        self.chat_key_to_id.remove(&key);
    }

    /// Remove a contact from the list by its UI key.
    pub fn remove_contact(&mut self, key: i32) {
        self.contact_key_to_id.remove(&key);
        self.contacts.retain(|r| r.key != key);
    }


    pub fn append_chat(&mut self, chat_id: String, title: String, is_group: bool, peer_id: String) -> i32 {
        let key = self.bump();
        self.chat_key_to_id.insert(key, chat_id.clone());
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.chats.push(ChatRow {
            key,
            chat_id,
            title,
            preview: String::new(),
            is_group,
            time_ms: now_ms,
            unread: 0,
            peer_id,
            avatar_path: String::new(),
        });
        key
    }

    /// Append a new friend to the in-memory contact list.
    pub fn append_contact(&mut self, user_id: i64, peer_id: String, name: String, image: String) -> i32 {
        let key = self.bump();
        self.contact_key_to_id.insert(key, user_id);
        self.contacts.push(ContactRow {
            key,
            user_id,
            peer_id,
            letter: contact_letter(&name),
            name,
            is_following: false,
            follows_me: false,
            image,
        });
        key
    }

    /// True if a contact already exists for `peer_id` or `name`.
    pub fn has_contact(&self, peer_id: &str, name: &str) -> bool {
        self.contacts.iter().any(|c| c.peer_id == peer_id || c.name == name)
    }

    /// Record a friend request in the UI state and return its stable key.
    pub fn add_friend_request(
        &mut self,
        other_peer: &str,
        name: String,
        time_ms: i64,
        dir: FrqDir,
    ) -> i32 {
        let key = self.bump();
        self.frq_key_to.insert(key, (dir, other_peer.to_string()));
        self.friend_requests.push(FriendRequestData {
            id: key,
            peer_id: other_peer.to_string(),
            name,
            time_ms,
            dir,
        });
        key
    }

    /// Resolve a friend-request UI key back to its (dir, other peer).
    pub fn frq_for(&self, key: i32) -> Option<(FrqDir, &str)> {
        self.frq_key_to
            .get(&key)
            .map(|(dir, peer)| (*dir, peer.as_str()))
    }

    /// Remove a friend request by UI key.
    pub fn remove_friend_request(&mut self, key: i32) {
        self.friend_requests.retain(|r| r.id != key);
        self.frq_key_to.remove(&key);
    }

    /// Remove any friend request between me and `other` of the given direction.
    pub fn remove_friend_request_if(&mut self, other: &str, dir: FrqDir) {
        let mut hit: Option<i32> = None;
        for r in &self.friend_requests {
            if r.peer_id == other && r.dir == dir {
                hit = Some(r.id);
                break;
            }
        }
        if let Some(k) = hit {
            self.remove_friend_request(k);
        }
    }
}

pub type ArcBackend = Arc<RwLock<DataBackend>>;

/// Resolve a libp2p peer id (or username) to a display name: the account's
/// nickname, falling back to its username, falling back to the raw string.
pub async fn name_for_peer(pool: &SqlitePool, peer: &str) -> String {
    if !peer.is_empty() {
        if let Ok(Some(user_id)) = sqlite::devices::ensure_user_by_peer(pool, peer).await {
            if let Ok(Some(u)) = sqlite::users::get(pool, user_id).await {
                let label = u
                    .nickname
                    .clone()
                    .filter(|s| !s.is_empty())
                    .or_else(|| u.username.clone().filter(|s| !s.is_empty()));
                if let Some(label) = label {
                    return label;
                }
            }
        }
    }
    if !peer.is_empty() {
        if let Ok(Some(u)) = sqlite::users::get_by_username(pool, peer).await {
            let label = u
                .nickname
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| u.username.clone().filter(|s| !s.is_empty()));
            if let Some(label) = label {
                return label;
            }
        }
    }
    peer.to_string()
}

/// Load the friend-request state (inbox + sent) for `me_peer` into `b`.
pub async fn load_fr_requests(pool: &SqlitePool, me_peer: &str, b: &mut DataBackend) -> anyhow::Result<()> {
    b.friend_requests.clear();
    b.frq_key_to.clear();

    let inbox = sqlite::social::frq_inbox(pool, me_peer).await?;
    for r in inbox {
        let name = r.note.unwrap_or_else(|| r.requester.clone());
        b.add_friend_request(&r.requester, name, r.created_at, FrqDir::Inbox);
    }

    let sent = sqlite::social::frq_sent(pool, me_peer).await?;
    for r in sent {
        b.add_friend_request(&r.recipient, r.note.unwrap_or_else(|| r.recipient.clone()), r.created_at, FrqDir::Sent);
    }
    Ok(())
}

pub async fn load(pool: &SqlitePool, me: &str) -> anyhow::Result<(DataBackend, String)> {
    let mut b = DataBackend::default();

    // Resolve the string account id onto an integer user row id.
    let me_id = sqlite::users::get_by_username(pool, me)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no user row for {me}; run login to create one"))?
        .id;
    let my_nickname = sqlite::users::get(pool, me_id)
        .await?
        .and_then(|u| u.nickname)
        .unwrap_or_default();

    // ---- chat list ----
    let items = sqlite::joins::chat_list(pool, me, 200).await?;
    for c in items {
        let key = b.bump();
        // Chat key: the string chat id (stored in `name` for core-created rows).
        let chat_key = c.name.clone().unwrap_or_else(|| c.id.to_string());
        // Group vs DM: the chat id is authoritative. core always names group
        // ids `grp-…` whereas DM ids are `c<hash>` (and never `grp-`), so the
        // `conversations.type` column — which `ensure_dm` forces to 0 — cannot
        // be trusted for groups. Fall back to it for legacy rows.
        let is_group = chat_key.starts_with("grp-") || c.type_ == 1;
        let title = if is_group {
            "Group".to_string()
        } else {
            c.peer_nickname
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| c.peer_username.clone().filter(|s| !s.is_empty()))
                .or_else(|| c.name.clone().filter(|s| !s.is_empty()))
                .unwrap_or_else(|| c.id.to_string())
        };
        b.chat_key_to_id.insert(key, chat_key.clone());
        b.chats.push(ChatRow {
            key,
            chat_id: chat_key,
            title,
            // Group previews at rest are ciphertext (E2E); the chat window
            // decrypts them, but the list preview is left empty rather than
            // surfacing the base64 blob.
            preview: if is_group {
                String::new()
            } else {
                c.last_message_preview.unwrap_or_default()
            },
            is_group,
            time_ms: c.last_message_time.unwrap_or(0),
            unread: c.unread_count,
            peer_id: c.peer_id.unwrap_or_default(),
            avatar_path: c.avatar_path.clone().unwrap_or_default(),
        });
    }

    // ---- contacts (friends) ----
    let friends = sqlite::joins::friends_with_profile(pool, me_id, 500).await?;
    for f in friends {
        let key = b.bump();
        let name = f.username
            .clone()
            .unwrap_or_else(|| f.user_id.to_string());
        let user_id = f.user_id;
        b.contact_key_to_id.insert(key, user_id);
        let image = f.avatar_path.clone().unwrap_or_default();

        let peer_id = f.device_peer_id.clone().unwrap_or_default();
        b.contacts.push(ContactRow {
            key,
            user_id,
            peer_id,
            letter: contact_letter(&name),
            name,
            is_following: f.i_follow_them,
            follows_me: f.they_follow_me,
            image,
        });
    }

    // WeChat-style ordering: group by letter ('#' last), alphabetical inside.
    let rank = |c: char| if c == '#' { '[' } else { c }; // '[' sorts just after 'Z'
    b.contacts.sort_by(|a, b| rank(a.letter)
        .cmp(&rank(b.letter))
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));

    // ---- discovery feed ----
    let posts = sqlite::joins::feed(pool, me_id, true, 200, 0).await?;
    for p in posts {
        let key = b.key_for_post(p.id);
        b.discover.push(DiscoverRow {
            key,
            post_id: p.id,
            title: p.content.unwrap_or_default(),
            author: p.author_nickname
                .unwrap_or_else(|| p.author_username.unwrap_or_default()),
            likes: p.like_count,
            liked: p.i_liked,
            cover: first_preview_or_media(&p.preview_urls, &p.media_urls),
        });
    }

    Ok((b, my_nickname))
}

/// Card cover path: prefer the (small) preview of a post; fall back to the
/// full-resolution original (e.g. old rows that predate `preview_urls` or
/// posts whose previews failed to generate).
pub fn first_preview_or_media(preview_urls: &Option<String>, media_urls: &Option<String>) -> String {
    let p = first_media(preview_urls);
    if !p.is_empty() {
        return p;
    }
    first_media(media_urls)
}

/// First non-empty file path from a stored `media_urls` column (JSON array or
/// bare path), for the discover card cover. Empty string when the post has
/// no media.
pub fn first_media(media_urls: &Option<String>) -> String {
    let Some(s) = media_urls.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
        return String::new();
    };
    if let Ok(v) = serde_json::from_str::<Vec<String>>(s) {
        if let Some(first) = v.into_iter().find(|x| !x.trim().is_empty()) {
            return first;
        }
        return String::new();
    }
    if !s.starts_with('[') {
        return s.to_string();
    }
    String::new()
}

pub async fn refresh_chat_row(pool: &SqlitePool, me: &str, b: &mut DataBackend, chat_id: &str) -> anyhow::Result<bool> {
    // Use full chat_list to find the matching entry (cheap for a local sqlite on a single user).
    let items = sqlite::joins::chat_list(pool, me, 200).await?;
    let Some(src) = items.iter().find(|c| c.name.as_deref() == Some(chat_id)) else {
        // Idempotent: group conversations live only in the in-memory backend
        // (created by `append_chat`, never persisted to the sqlite
        // `conversations` table), so this fallback is re-entered on every
        // inbound refresh. Reuse the existing row instead of appending a
        // duplicate on each message.
        if b.chats.iter().any(|r| r.chat_id == chat_id) {
            return Ok(false);
        }
        // New conversation: append. Group ids (`grp-…` from the gossipsub
        // topic) are not in the sqlite chat rows; classify by the `grp-`
        // prefix so they land in the group list — otherwise they fall into the
        // DM list and render the stored ciphertext. The creator's `append_chat`
        // already set a friendly title, so a generic one suffices here.
        let is_group = chat_id.starts_with("grp-");
        let title = if is_group {
            "Group".to_string()
        } else {
            chat_id.to_string()
        };
        let key = b.bump();
        b.chat_key_to_id.insert(key, chat_id.to_string());
        b.chats.push(ChatRow {
            key,
            chat_id: chat_id.into(),
            title,
            preview: String::new(),
            is_group,
            time_ms: 0,
            unread: 0,
            peer_id: String::new(),
            avatar_path: String::new(),
        });
        return Ok(true);
    };
    let src_chat_key = src.name.clone().unwrap_or_else(|| src.id.to_string());
    let is_new = !b.chats.iter().any(|r| r.chat_id == src_chat_key);
    if is_new {
        // The conversation exists in sqlite (created by `ensure_dm` when the
        // first message landed) but no in-memory row yet: materialize it now.
        let is_group = chat_id.starts_with("grp-") || src.type_ == 1;
        let title = if is_group {
            "Group".to_string()
        } else {
            src.peer_nickname
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| src.peer_username.clone().filter(|s| !s.is_empty()))
                .or_else(|| src.name.clone().filter(|s| !s.is_empty()))
                .unwrap_or_else(|| src.id.to_string())
        };
        let key = b.bump();
        b.chat_key_to_id.insert(key, src_chat_key.clone());
        b.chats.push(ChatRow {
            key,
            chat_id: src_chat_key.clone(),
            title,
            preview: if is_group {
                String::new()
            } else {
                src.last_message_preview.clone().unwrap_or_default()
            },
            is_group,
            time_ms: src.last_message_time.unwrap_or(0),
            unread: src.unread_count,
            peer_id: src.peer_id.clone().unwrap_or_default(),
            avatar_path: src.avatar_path.clone().unwrap_or_default(),
        });
        return Ok(true);
    }
    let Some(row) = b.chats.iter_mut().find(|r| r.chat_id == src_chat_key) else {
        return Ok(false);
    };
    // Group vs DM is decided by the chat id prefix (`grp-…`), not sqlite's
    // `conversations.type` — `insert_row` always writes type=0 via
    // `ensure_dm`, so a group's type column is unreliable.
    let is_group = chat_id.starts_with("grp-");
    // Title: same fallback chain as initial build (load_backend) — peer
    // display niceness should survive in-place refreshes.
    let title = if is_group {
        // Group messages are stored as ciphertext; a generic label is what the
        // receiving side has (the creator's `append_chat` already set a real
        // title for their own copy).
        "Group".to_string()
    } else {
        src.peer_nickname
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| src.peer_username.clone().filter(|s| !s.is_empty()))
            .or_else(|| src.name.clone().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| src.id.to_string())
    };
    let peer_id = src.peer_id.clone().unwrap_or_default();
    let new = ChatRow {
        key: row.key,
        chat_id: src_chat_key.clone(),
        title,
        // Group previews are ciphertext at rest, so keep whatever the row
        // already showed rather than overwriting with the base64 blob.
        preview: if is_group {
            row.preview.clone()
        } else {
            src.last_message_preview.clone().unwrap_or_default()
        },
        is_group,
        time_ms: src.last_message_time.unwrap_or(0),
        unread: src.unread_count,
        peer_id: peer_id.clone(),
        avatar_path: src.avatar_path.clone().unwrap_or_default(),
    };
    let changed = new.title != row.title
        || new.preview != row.preview
        || new.unread != row.unread
        || new.time_ms != row.time_ms
        || new.peer_id != row.peer_id
        || new.avatar_path != row.avatar_path;
    *row = new;
    Ok(changed)
}

/// Persist a discovery like/unlike toggle to SQLite (fire and forget).
pub async fn persist_like_toggle(pool: &SqlitePool, me: &str, post_id: i64, want_liked: bool) -> anyhow::Result<()> {
    let me_id = sqlite::users::get_by_username(pool, me)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no user row for {me}; run login to create one"))?
        .id;
    let currently = sqlite::social_feed::has_liked(pool, post_id, me_id).await?;
    if currently == want_liked {
        return Ok(());
    }
    if want_liked {
        sqlite::social_feed::like(pool, post_id, me_id).await?;
    } else {
        sqlite::social_feed::unlike(pool, post_id, me_id).await?;
    }
    Ok(())
}

/// Resolve a group-message sender to a display name: the account's nickname,
/// falling back to its username, falling back to the raw sender string. The
/// sender may be a libp2p PeerId (group members are keyed by device) or, for
/// legacy rows, a username.
pub async fn group_sender_name(pool: &SqlitePool, sender: &str) -> String {
    if let Ok(Some(user_id)) = sqlite::devices::ensure_user_by_peer(pool, sender).await {
        if let Ok(Some(u)) = sqlite::users::get(pool, user_id).await {
            let label = u
                .nickname
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| u.username.clone().filter(|s| !s.is_empty()));
            if let Some(label) = label {
                return label;
            }
        }
    }
    if !sender.is_empty() {
        if let Ok(Some(u)) = sqlite::users::get_by_username(pool, sender).await {
            let label = u
                .nickname
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| u.username.clone().filter(|s| !s.is_empty()));
            if let Some(label) = label {
                return label;
            }
        }
    }
    sender.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_key_mapping() {
        let mut b = DataBackend::default();
        // These two 64-bit ids share the same low 32 bits, so a naive
        // `id as i32` would alias them; the stable key must not.
        let id_a: i64 = 0x0000_0000_0000_0001;
        let id_b: i64 = 0x0001_0000_0000_0001;
        assert_eq!(id_a as u32, id_b as u32);
        let k1 = b.key_for_note(id_a);
        let k2 = b.key_for_note(id_b);
        assert_ne!(k1, k2);
        assert_eq!(b.note_id_for(k1), Some(id_a));
        assert_eq!(b.note_id_for(k2), Some(id_b));
        // Reusing the same note id returns the same key.
        assert_eq!(b.key_for_note(id_a), k1);
        assert_eq!(b.note_id_for(999), None);
    }

    #[test]
    fn toggle_like_math() {
        let mut b = DataBackend::default();
        let key = 42;
        b.post_to_key.insert(1, key);
        b.key_to_post.insert(key, 1);
        b.discover.push(DiscoverRow {
            key,
            post_id: 1,
            title: "hi".into(),
            author: "a".into(),
            likes: 3,
            liked: false,
            cover: String::new(),
        });
        let (liked, likes) = b.toggle_like(key);
        assert!(liked);
        assert_eq!(likes, 4);
        let (liked, likes) = b.toggle_like(key);
        assert!(!liked);
        assert_eq!(likes, 3);
    }

    #[tokio::test]
    async fn load_populates_discover() {
        let pool = sqlite::open_memory().await.unwrap();
        let me = "me";
        // seed a friend + a post
        sqlite::users::upsert(
            &pool,
            1,
            &sqlite::users::UserPatch {
                username: Some("me".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        sqlite::users::upsert(
            &pool,
            2,
            &sqlite::users::UserPatch {
                nickname: Some("One".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        sqlite::social::add(&pool, 1, 2).await.unwrap();
        let post = sqlite::social_feed::create_post(&pool, 2, Some("hello"), None, None, 0).await.unwrap();
        sqlite::social_feed::like(&pool, post.id, 1).await.unwrap();

        let (b, _nickname) = load(&pool, me).await.unwrap();
        assert_eq!(b.contacts.len(), 1);
        assert_eq!(b.contacts[0].name, "2"); // username is empty -> falls back to user_id
        assert_eq!(b.discover.len(), 1);
        assert!(b.discover[0].liked);
        assert_eq!(b.discover[0].likes, 1);
    }

    #[tokio::test]
    async fn refresh_chat_row_materializes_new_dm() {
        // Regression: the receiving side of a DM has no in-memory chat row,
        // but the sqlite `conversations` row is created by `ensure_dm` at
        // message-insert time. `refresh_chat_row` must materialize the
        // in-memory row from the sqlite row (previously it would silently
        // return Ok(false), leaving the UI chat list empty).
        let pool = sqlite::open_memory().await.unwrap();
        let me = "me-peer";
        let me_id = 100;
        sqlite::users::upsert(
            &pool,
            me_id,
            &sqlite::users::UserPatch {
                username: Some(me.to_string()),
                nickname: Some("Me".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let their_id = 200;
        sqlite::users::upsert(
            &pool,
            their_id,
            &sqlite::users::UserPatch {
                username: Some("them".into()),
                nickname: Some("Them".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // Bind a device for the peer so chat_list resolves the join → title.
        sqlite::devices::bind_device(&pool, their_id, "them-peer").await.unwrap();

        // The sender side already pushed a message into the shared store:
        // `insert_row` ensures the conversation exists in sqlite.
        let dm = chatx_core::store::dm_chat_id(me, "them-peer");
        let _ = sqlite::messages::insert_row(
            &pool,
            &sqlite::MsgRow {
                id: 0,
                chat_id: dm.clone(),
                sender: "them-peer".into(),
                text: "hello!".into(),
                sealed: false,
                t: 1234,
                mine: false,
            },
        )
        .await
        .unwrap();

        // Receiver's in-memory state: empty.
        let mut b = DataBackend::default();
        let _me_user = me_id;
        let changed = refresh_chat_row(&pool, me, &mut b, &dm).await.unwrap();
        assert!(changed, "inbound first message must produce a new chat row");
        let row = b.chats.iter().find(|r| r.chat_id == dm).expect("chat row present");
        assert!(!row.is_group);
        assert_eq!(row.peer_id, "them-peer");
        assert_eq!(row.title, "Them");
        assert_eq!(row.preview, "hello!");
        assert_eq!(row.unread, 1);
        assert_eq!(row.time_ms, 1234);

        // Idempotency: a second refresh must not duplicate the row.
        let _ = refresh_chat_row(&pool, me, &mut b, &dm).await.unwrap();
        let n_dm = b.chats.iter().filter(|r| r.chat_id == dm).count();
        assert_eq!(n_dm, 1, "no duplicate rows on subsequent refresh");
    }
}
