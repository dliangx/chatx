use crate::message::now_ms;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

/// A stored chat message — either plain text or a voice message.
#[derive(Debug, Clone)]
pub struct StoredMsg {
    pub id: i64,
    pub chat_id: String,
    pub sender: String,
    /// For text rows: the body. For voice rows: unused / empty.
    pub text: String,
    pub sealed: bool,
    pub t: u64,
    /// `None` for text rows; `Some((pcm_path, duration_ms))` for voice rows.
    pub kind: Kind,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Kind {
    #[default]
    Text,
    /// `0` = disk path to the s16le PCM clip, `1` = duration in milliseconds.
    Voice { path: String, duration_ms: u64 },
}

impl StoredMsg {
    pub fn is_outgoing(&self, me: &str) -> bool {
        self.sender == me
    }
}

pub fn dm_chat_id(peer_a: &str, peer_b: &str) -> String {
    let (x, y) = if peer_a <= peer_b {
        (peer_a, peer_b)
    } else {
        (peer_b, peer_a)
    };
    hash_chat_id(&[x, y])
}

pub fn hash_chat_id(parts: &[&str]) -> String {
    let mut acc: Vec<u8> = Vec::new();
    parts.iter().for_each(|s| {
        acc.extend_from_slice(s.as_bytes());
        acc.push(b'|');
    });
    let digest = Sha256::digest(&acc);
    format!("c{}", hex(&digest[..8]))
}


fn hex(bytes: &[u8]) -> String {
    const CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(CHARS[(b >> 4) as usize] as char);
        out.push(CHARS[(b & 0x0f) as usize] as char);
    }
    out
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

    /// Insert a new message. Pass `kind = None` for a plain text row, or
    /// `Some((pcm_path, duration_ms))` for a voice clip (the `path` is
    /// written to the `media_path` column).
    pub async fn push(
        &self,
        chat_id: &str,
        sender: &str,
        text: &str,
        sealed: bool,
        kind: Option<(String, u64)>,
    ) {
        let t = now_ms();
        let msg = StoredMsg {
            id: t as i64,
            chat_id: chat_id.to_string(),
            sender: sender.to_string(),
            text: text.to_string(),
            sealed,
            t,
            kind: kind
                .map(|(p, d)| Kind::Voice {
                    path: p,
                    duration_ms: d,
                })
                .unwrap_or_default(),
        };
        self.msgs.write().push(msg.clone());
        if let Some(db) = &self.db {
            let mine = msg.sender == self.me;
            let (vpath, vdur) = match &msg.kind {
                Kind::Voice {
                    path,
                    duration_ms,
                } => (Some(path.clone()), Some(*duration_ms)),
                Kind::Text => (None, None),
            };
            let _ = sqlite::messages::insert_row(
                db,
                &sqlite::MsgRow {
                    id: 0,
                    chat_id: msg.chat_id,
                    sender: msg.sender,
                    text: msg.text,
                    sealed: msg.sealed,
                    t: msg.t,
                    mine,
                    media_path: vpath,
                    media_duration_ms: vdur,
                },
            )
            .await;
        }
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
    match (m.media_path.as_deref(), m.media_duration_ms) {
        (Some(p), Some(d)) if !p.is_empty() => Kind::Voice {
            path: p.to_string(),
            duration_ms: d,
        },
        _ => Kind::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dm_id_is_symmetric() {
        let a = "QmAAA";
        let b = "QmBBB";
        assert_eq!(dm_chat_id(a, b), dm_chat_id(b, a));
        assert_ne!(dm_chat_id(a, b), b);
    }

    #[tokio::test]
    async fn push_persists_and_roundtrips() {
        let db = sqlite::open_memory().await.unwrap();
        let store = Store::with_sql(db.clone(), "QmMe");
        let chat = dm_chat_id("QmMe", "QmOther");

        store.push(&chat, "QmMe", "hi", false, None).await;
        store.push(&chat, "QmOther", "yo", true, None).await;
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
        let chat = dm_chat_id("QmMe", "QmOther");
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
