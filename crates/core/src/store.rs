use crate::message::now_ms;
use parking_lot::{Mutex, RwLock};
use sqlite::MsgRow;
use std::sync::Arc;

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
    msgs: RwLock<Vec<StoredMsg>>,
    db: Option<Arc<Mutex<rusqlite::Connection>>>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    pub fn new() -> Self {
        Self {
            msgs: RwLock::new(Vec::new()),
            db: None,
        }
    }

    pub fn with_sql(db: Arc<Mutex<rusqlite::Connection>>) -> anyhow::Result<Self> {
        Ok(Self {
            msgs: RwLock::new(Vec::new()),
            db: Some(db),
        })
    }

    pub fn hydrate(&self, chat_id: &str) -> anyhow::Result<usize> {
        let db = match &self.db {
            Some(d) => d,
            None => return Ok(0),
        };
        let rows = sqlite::load_all(&db.lock(), chat_id)?;
        let mut asc = rows;
        asc.reverse();
        self.msgs.write().extend(asc.into_iter().map(from_row));
        Ok(self.msgs.read().len())
    }

    pub fn push(&self, chat_id: &str, sender: &str, text: &str, sealed: bool) {
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
            let _ = sqlite::insert(
                &db.lock(),
                &MsgRow {
                    chat_id: msg.chat_id,
                    sender: msg.sender,
                    text: msg.text,
                    sealed: msg.sealed,
                    t: msg.t,
                },
            );
        }
    }

    pub fn all(&self) -> Vec<StoredMsg> {
        self.msgs.read().clone()
    }

    pub fn load(&self, chat_id: &str, limit: usize, offset: usize) -> anyhow::Result<Vec<StoredMsg>> {
        if let Some(db) = &self.db {
            let rows = sqlite::load_page(&db.lock(), chat_id, limit as u32, offset as u32)?;
            return Ok(rows.into_iter().map(from_row).collect());
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

    pub fn delete_before(&self, chat_id: &str, before_t: u64) -> anyhow::Result<usize> {
        if let Some(db) = &self.db {
            let n = sqlite::delete_before(&db.lock(), chat_id, before_t)?;
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
}

fn from_row(r: MsgRow) -> StoredMsg {
    StoredMsg {
        chat_id: r.chat_id,
        sender: r.sender,
        text: r.text,
        sealed: r.sealed,
        t: r.t,
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

    #[test]
    fn push_persists_and_roundtrips() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        sqlite::init(&db).unwrap();
        let db = Arc::new(Mutex::new(db));
        let store = Store::with_sql(db.clone()).unwrap();
        let chat = dm_chat_id("QmMe", "QmOther");

        store.push(&chat, "QmMe", "hi", false);
        store.push(&chat, "QmOther", "yo", true);
        let me = "QmMe";
        assert_eq!(store.all().len(), 2);
        assert!(store.all().iter().any(|m| m.text == "hi" && m.is_outgoing(me)));
        assert!(store.all().iter().any(|m| m.text == "yo" && m.sealed && !m.is_outgoing(me)));

        assert_eq!(sqlite::count(&db.lock(), &chat).unwrap(), 2);
        assert!(store.persisted());
    }

    #[test]
    fn hydrate_paging_order_deterministic() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        sqlite::init(&conn).unwrap();
        let db = Arc::new(Mutex::new(conn));
        let chat = dm_chat_id("QmMe", "QmOther");
        for (i, txt) in ["old", "mid", "new"].into_iter().enumerate() {
            sqlite::insert(
                &db.lock(),
                &MsgRow {
                    chat_id: chat.clone(),
                    sender: "peer".into(),
                    text: txt.into(),
                    sealed: false,
                    t: 10 + i as u64,
                },
            ).unwrap();
        }

        let store = Store::with_sql(db).unwrap();
        assert_eq!(store.len(), 0, "buffer empty before load");
        let n = store.hydrate(&chat).unwrap();
        assert_eq!(n, 3, "all 3 rows loaded");
        assert_eq!(store.all()[0].text, "old");
        assert_eq!(store.all()[2].text, "new");

        let p0 = store.load(&chat, 2, 0).unwrap();
        assert_eq!(p0.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), vec!["new", "mid"]);
        let p1 = store.load(&chat, 2, 2).unwrap();
        assert_eq!(p1.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), vec!["old"]);
    }
}
