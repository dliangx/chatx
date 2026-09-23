use crate::message::now_ms;
use sqlx::SqlitePool;

#[derive(Debug, Clone)]
pub struct StoredMsg {
    pub chat_id: String,
    pub sender: String,
    pub text: String,
    pub sealed: bool,
    pub t: u64,
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
    format!("{x}|{y}")
}

#[derive(Debug)]
pub struct Store {
    msgs: parking_lot::RwLock<Vec<StoredMsg>>,
    db: Option<SqlitePool>,
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
        }
    }

    pub fn with_sql(db: SqlitePool) -> Self {
        Self {
            msgs: parking_lot::RwLock::new(Vec::new()),
            db: Some(db),
        }
    }

    pub async fn hydrate(&self, chat_id: &str) -> anyhow::Result<usize> {
        if let Some(db) = &self.db {
            let rows = sqlite::messages::load_all(db, chat_id).await?;
            let mut asc: Vec<StoredMsg> = rows
                .into_iter()
                .map(|m| StoredMsg {
                    chat_id: m.conversation_id,
                    sender: m.sender_id,
                    text: m.text_content.unwrap_or_default(),
                    sealed: m.is_encrypted,
                    t: m.timestamp as u64,
                })
                .collect();
            asc.reverse();
            self.msgs.write().extend(asc);
            return Ok(self.msgs.read().len());
        }
        Ok(0)
    }

    pub async fn push(&self, chat_id: &str, sender: &str, text: &str, sealed: bool) {
        let t = now_ms();
        let msg = StoredMsg {
            chat_id: chat_id.to_string(),
            sender: sender.to_string(),
            text: text.to_string(),
            sealed,
            t,
        };
        self.msgs.write().push(msg.clone());
        if let Some(db) = &self.db {
            let _ = sqlite::messages::insert_row(
                db,
                &sqlite::MsgRow {
                    chat_id: msg.chat_id,
                    sender: msg.sender,
                    text: msg.text,
                    sealed: msg.sealed,
                    t: msg.t,
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
                .map(|m| StoredMsg {
                    chat_id: m.conversation_id,
                    sender: m.sender_id,
                    text: m.text_content.unwrap_or_default(),
                    sealed: m.is_encrypted,
                    t: m.timestamp as u64,
                })
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
        let store = Store::with_sql(db.clone());
        let chat = dm_chat_id("QmMe", "QmOther");

        store.push(&chat, "QmMe", "hi", false).await;
        store.push(&chat, "QmOther", "yo", true).await;
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
                    chat_id: chat.clone(),
                    sender: "peer".into(),
                    text: txt.into(),
                    sealed: false,
                    t: 10 + i as u64,
                },
            )
            .await
            .unwrap();
        }

        let store = Store::with_sql(db);
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
