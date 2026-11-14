use crate::message::now_ms;
use sqlx::SqlitePool;
use sqlite::messages::{NewMessage, MSG_TYPE_TEXT, STATUS_SENDING};

/// A stored chat message — text, a voice clip, or a photo.
#[derive(Debug, Clone)]
pub struct StoredMsg {
    pub id: i64,
    pub chat_id: String,
    pub sender: String,
    /// For text rows: the body. For media rows: unused / empty.
    pub text: String,
    pub sealed: bool,
    pub t: u64,
    /// `Text` for plain rows; `Voice`/`Image` carry the media file path.
    pub kind: Kind,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Kind {
    #[default]
    Text,
    /// `path` = disk path to the s16le PCM clip, `duration_ms` = its length.
    Voice { path: String, duration_ms: u64 },
    /// `path` = disk path to a photo (original + compressed preview are the
    /// same compressed JPEG on disk; the UI decides how large to show it).
    Image { path: String },
    /// `path` = disk path to a document file (pdf/docx/xlsx/pptx); `name` = the sender's basename shown on the file bubble.
    File { path: String, name: String },
}

impl StoredMsg {
    pub fn is_outgoing(&self, me: &str) -> bool {
        self.sender == me
    }
}

/// Build a DM chat id from the peer's username.
///
/// DM conversations are keyed by the *person* (username), not by a
/// device PeerId, so that all of a peer's devices collapse into one
/// conversation in the chat list. The per-device libp2p id that p2p
/// features (calls / share / DM dial) still need is resolved on demand
/// from `contacts` / `devices`, not derived from the chat id.
///
/// The fallback caller path (no username resolvable from the directory,
/// e.g. peer never announced itself or directory not running) may pass
/// the peer's base58 here and get a `dm:<base58>` id that still keys one
/// row per *person's specific base58*, not per message. That's the best
/// we can do without a known username.
pub fn dm_chat_id(peer_username: &str) -> String {
    format!("dm:{peer_username}")
}

#[derive(Debug)]
pub struct Store {
    msgs: parking_lot::RwLock<Vec<StoredMsg>>,
    db: Option<SqlitePool>,
    me: String,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    /// Insert our own outbound text row with `status = sending` and return
    /// its id. The caller (core) uses the id to drive the offline queue:
    /// it is bumped to `sent` once the peer acks, or `failed` when the
    /// retry budget is exhausted. See `queue.rs`.
    pub async fn insert_mine_sending(
        &self,
        chat_id: &str,
        text: &str,
    ) -> anyhow::Result<i64> {
        let Some(db) = &self.db else {
            anyhow::bail!("Store::insert_mine_sending requires a sql backend");
        };
        let t = now_ms();
        let id = t as i64;
        let conversation_id = sqlite::conversations::ensure_dm(db, chat_id).await?;
        let sender_user_id = sqlite::devices::ensure_user_by_peer(db, &self.me).await?;
        let msg = NewMessage {
            id: Some(id),
            conversation_id,
            sender_user_id,
            sender_peer_id: Some(self.me.clone()),
            msg_type: MSG_TYPE_TEXT,
            text_content: Some(text.to_string()),
            timestamp: Some(t as i64),
            status: Some(STATUS_SENDING),
            from_me: true,
            ..Default::default()
        };
        let id = sqlite::messages::insert(db, &msg).await?;
        // Mirror in-memory so the active chat list reflects it immediately.
        self.msgs.write().push(StoredMsg {
            id,
            chat_id: chat_id.to_string(),
            sender: self.me.clone(),
            text: text.to_string(),
            sealed: false,
            t,
            kind: Kind::Text,
        });
        Ok(id)
    }

    /// Bump the status of a local message row (e.g. `sending` → `sent` on
    /// ack, or → `failed` after exhausted retries).
    pub async fn set_status(&self, msg_id: i64, status: i64) -> anyhow::Result<()> {
        let Some(db) = &self.db else {
            return Ok(());
        };
        sqlite::messages::set_status(db, msg_id, status).await
    }

    pub fn new() -> Self {
        Self {
            msgs: parking_lot::RwLock::new(Vec::new()),
            db: None,
            me: String::new(),
        }
    }

    pub fn with_sql(db: SqlitePool, me: impl Into<String>) -> Self {
        Self {
            msgs: parking_lot::RwLock::new(Vec::new()),
            db: Some(db),
            me: me.into(),
        }
    }

    pub async fn hydrate(&self, chat_id: &str) -> anyhow::Result<usize> {
        if let Some(db) = &self.db {
            let rows = sqlite::messages::load_all(db, chat_id).await?;
            let mut asc: Vec<StoredMsg> = rows.into_iter().map(row_into_stored).collect();
            asc.reverse();
            self.msgs.write().extend(asc);
            return Ok(self.msgs.read().len());
        }
        Ok(0)
    }

    /// Insert a new message. `kind = Kind::Text` for a plain text row,
    /// `Kind::Voice`/`Kind::Image` for media (the path is written to the
    /// `media_path` column). Returns the row's timestamp id (`t`) so a caller
    /// that needs to track the fresh row (e.g. an optimistic send badge) can
    /// key onto it.
    pub async fn push(
        &self,
        chat_id: &str,
        sender: &str,
        text: &str,
        sealed: bool,
        kind: Kind,
    ) -> i64 {
        let t = now_ms();
        let msg = StoredMsg {
            id: t as i64,
            chat_id: chat_id.to_string(),
            sender: sender.to_string(),
            text: text.to_string(),
            sealed,
            t,
            kind: kind.clone(),
        };
        self.msgs.write().push(msg.clone());
        if let Some(db) = &self.db {
            let mine = msg.sender == self.me;
            let (mkind, mpath, mdur) = match &msg.kind {
                Kind::Voice {
                    path,
                    duration_ms,
                } => (
                    sqlite::MediaKind::Voice,
                    Some(path.clone()),
                    Some(*duration_ms),
                ),
                Kind::Image { path } => (sqlite::MediaKind::Image, Some(path.clone()), None),
                Kind::File { path, .. } => (sqlite::MediaKind::File, Some(path.clone()), None),
                Kind::Text => (sqlite::MediaKind::None, None, None),
            };
            // File rows carry the sender's basename in the `text` column (no dedicated name slot in `media_*`) — `row_to_kind` reads it back from there.
            let row_text = match &msg.kind {
                Kind::File { name, .. } => name.clone(),
                _ => msg.text.clone(),
            };
            let _ = sqlite::messages::insert_row(
                db,
                &sqlite::MsgRow {
                    // Row `id` == its timestamp `t`, matching the in-memory
                    // `StoredMsg.id` and (for outbound media) the app's send
                    // badge key, so the offline-queue / replay / badge all
                    // reference one identity.
                    id: msg.t as i64,
                    chat_id: msg.chat_id,
                    sender: msg.sender,
                    text: row_text,
                    sealed: msg.sealed,
                    t: msg.t,
                    mine,
                    media_kind: mkind,
                    media_path: mpath,
                    media_duration_ms: mdur,
                },
            )
            .await;
        }
        msg.id
    }

    pub fn all(&self) -> Vec<StoredMsg> {
        self.msgs.read().clone()
    }

    pub async fn load(
        &self,
        chat_id: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<StoredMsg>> {
        if let Some(db) = &self.db {
            let rows =
                sqlite::messages::load_page(db, chat_id, limit as u32, offset as u32).await?;
            return Ok(rows
                .into_iter()
                .map(row_into_stored)
                .collect());
        }
        let buf: Vec<StoredMsg> = self
            .msgs
            .read()
            .iter()
            .rev()
            .filter(|m| m.chat_id == chat_id)
            .skip(offset)
            .take(limit)
            .cloned()
            .collect();
        Ok(buf)
    }

    pub async fn delete_before(&self, chat_id: &str, before_t: u64) -> anyhow::Result<usize> {
        if let Some(db) = &self.db {
            let n = sqlite::messages::delete_before(db, chat_id, before_t).await?;
            self.msgs.write().retain(|m| !(m.chat_id == chat_id && m.t < before_t));
            return Ok(n);
        }
        let mut buf = self.msgs.write();
        let before = buf.len();
        buf.retain(|m| !(m.chat_id == chat_id && m.t < before_t));
        Ok(before - buf.len())
    }

    pub fn len(&self) -> usize {
        self.msgs.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.msgs.read().is_empty()
    }

    pub fn persisted(&self) -> bool {
        self.db.is_some()
    }

    /// Access to the underlying SQLite pool (if this store is persistence-backed).
    pub fn pool(&self) -> Option<SqlitePool> {
        self.db.clone()
    }
}

fn row_into_stored(m: sqlite::MsgRow) -> StoredMsg {
    let kind = row_to_kind(&m);
    StoredMsg {
        id: m.id,
        chat_id: m.chat_id,
        sender: m.sender,
        text: m.text,
        sealed: m.sealed,
        t: m.t,
        kind,
    }
}

fn row_to_kind(m: &sqlite::MsgRow) -> Kind {
    match m.media_kind {
        sqlite::MediaKind::Voice => match (m.media_path.clone(), m.media_duration_ms) {
            (Some(p), Some(d)) if !p.is_empty() => Kind::Voice {
                path: p,
                duration_ms: d,
            },
            _ => Kind::Text,
        },
        sqlite::MediaKind::Image => match &m.media_path {
            Some(p) if !p.is_empty() => Kind::Image { path: p.clone() },
            _ => Kind::Text,
        },
        sqlite::MediaKind::File => match &m.media_path {
            Some(p) if !p.is_empty() => Kind::File {
                path: p.clone(),
                // Stored in the `text` column by `push` (no dedicated name slot for File in `media_*`); fall back to the file suffix.
                name: if m.text.is_empty() {
                    std::path::Path::new(p)
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                } else {
                    m.text.clone()
                },
            },
            _ => Kind::Text,
        },
        sqlite::MediaKind::None => Kind::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dm_id_keys_by_username() {
        // DM ids are keyed by the peer's username so that a single
        // conversation row covers every device of that user.
        let a = dm_chat_id("alice");
        assert_eq!(a, "dm:alice");
        assert_ne!(dm_chat_id("alice"), dm_chat_id("bob"));
    }

    #[tokio::test]
    async fn push_persists_and_roundtrips() {
        let db = sqlite::open_memory().await.unwrap();
        let store = Store::with_sql(db.clone(), "QmMe");
        let chat = dm_chat_id("alice");

        let _ = store.push(&chat, "QmMe", "hi", false, Kind::Text).await;
        let _ = store.push(&chat, "QmOther", "yo", true, Kind::Text).await;
        let me = "QmMe";
        assert_eq!(store.all().len(), 2);
        assert!(store
            .all()
            .iter()
            .any(|m| m.text == "hi" && m.is_outgoing(me)));
        assert!(store
            .all()
            .iter()
            .any(|m| m.text == "yo" && m.sealed && !m.is_outgoing(me)));

        assert_eq!(
            sqlite::messages::count(&db, &chat)
                .await
                .unwrap(),
            2
        );
        assert!(store.persisted());
    }

    #[tokio::test]
    async fn hydrate_paging_order_deterministic() {
        let db = sqlite::open_memory().await.unwrap();
        let chat = dm_chat_id("alice");
        for (i, txt) in ["old", "mid", "new"].into_iter().enumerate() {
            sqlite::messages::insert_row(
                &db,
                &sqlite::MsgRow {
                    id: 0,
                    chat_id: chat.clone(),
                    sender: "peer".into(),
                    text: txt.into(),
                    sealed: false,
                    t: 10 + i as u64,
                    mine: false,
                    media_kind: sqlite::MediaKind::None,
                    media_path: None,
                    media_duration_ms: None,
                },
            )
            .await
            .unwrap();
        }

        let store = Store::with_sql(db, "QmMe");
        assert_eq!(store.len(), 0, "buffer empty before load");
        let n = store.hydrate(&chat).await.unwrap();
        assert_eq!(n, 3, "all 3 rows loaded");
        assert_eq!(store.all()[0].text, "old");
        assert_eq!(store.all()[2].text, "new");

        let p0 = store.load(&chat, 2, 0).await.unwrap();
        assert_eq!(
            p0.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            vec!["new", "mid"]
        );
        let p1 = store.load(&chat, 2, 2).await.unwrap();
        assert_eq!(
            p1.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            vec!["old"]
        );
    }
}
