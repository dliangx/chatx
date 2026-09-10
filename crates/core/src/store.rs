//! 会话与消息存储。
//!
//! 存储模型按**会话**（`chat_id`）而非"对端"组织，1:1 与将来群聊共用一套结构：
//! - `chat_id`：会话 id。1:1 = `dm_chat_id(peer_a, peer_b)`（peer 规范化拼接，双向一致）；群聊 = 群 id。
//! - `sender`：本条消息发送方的 peer_id（base58），可能是自己也可能是对方。
//! - `outgoing`：是否本端发出。
//!
//! 落盘：进程内一份 `RwLock<Vec<StoredMsg>>` 作读缓冲（UI 零成本），写入时同步落 `sqlite`
//! （`~/.config/p2pchat/<profile>/messages.db`），重启后可 `with_sql` 预填恢复。
use crate::message::now_ms;
use parking_lot::{Mutex, RwLock};
use sqlite::MsgRow;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct StoredMsg {
    /// 会话 id（1:1 = `dm_chat_id`；群聊 = 群 id）。
    pub chat_id: String,
    /// 本条消息的发送方 peer_id（base58，含自己）。
    pub sender: String,
    /// 消息内容（`sealed=true` 时为 E2E 密文 payload）。
    pub text: String,
    /// 是否本端发出。
    pub outgoing: bool,
    /// 内容是否为密文。
    pub sealed: bool,
    /// 时间戳（毫秒，UTC）。
    pub t: u64,
}

/// 1:1 会话 id：把两端 peer_id 按字典序规范化后拼接（`|` 分隔），保证 A→B 与 B→A 得到同一 id。
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
    /// SQLite 连接（`None` 时退化为纯内存，用于测试）。
    db: Option<Arc<Mutex<rusqlite::Connection>>>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    /// 纯内存存储（不落盘，测试用 / 未来群聊可换后端）。
    pub fn new() -> Self {
        Self {
            msgs: RwLock::new(Vec::new()),
            db: None,
        }
    }

    /// 接 SQLite 落盘：打开库、建表。UI 读路径仍走内存缓冲。
    pub fn with_sql(db: Arc<Mutex<rusqlite::Connection>>) -> anyhow::Result<Self> {
        Ok(Self {
            msgs: RwLock::new(Vec::new()),
            db: Some(db),
        })
    }

    /// 启动时把已有会话的本地历史灌进内存缓冲（按 `t` 升序，旧→新）。
    pub fn hydrate(&self, chat_id: &str) -> anyhow::Result<usize> {
        let db = match &self.db {
            Some(d) => d,
            None => return Ok(0),
        };
        // load_page 返回 新→旧；反转为 旧→新 入缓冲，保持 UI "追加在末尾" 的直觉。
        let rows = sqlite::load_all(&db.lock(), chat_id)?;
        let mut asc = rows;
        asc.reverse();
        self.msgs.write().extend(asc.into_iter().map(from_row));
        Ok(self.msgs.read().len())
    }

    /// 记录一条消息：进内存缓冲 + 同步落盘（若已接库）。
    pub fn push(
        &self,
        chat_id: &str,
        sender: &str,
        text: &str,
        outgoing: bool,
        sealed: bool,
    ) {
        let t = now_ms();
        let msg = StoredMsg {
            chat_id: chat_id.to_string(),
            sender: sender.to_string(),
            text: text.to_string(),
            outgoing,
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
                    outgoing: msg.outgoing,
                    sealed: msg.sealed,
                    t: msg.t,
                },
            );
        }
    }

    /// 当前缓冲的全部消息（旧→新，按插入序）。
    pub fn all(&self) -> Vec<StoredMsg> {
        self.msgs.read().clone()
    }

    /// 分页拉某会话历史（新→旧）；无库时从内存缓冲取末尾 `limit` 条近似。
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

    /// 删除某会话 `before_t` 之前的消息，返回删除条数。
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

    /// 已接 SQLite 落盘。
    pub fn persisted(&self) -> bool {
        self.db.is_some()
    }
}

fn from_row(r: MsgRow) -> StoredMsg {
    StoredMsg {
        chat_id: r.chat_id,
        sender: r.sender,
        text: r.text,
        outgoing: r.outgoing,
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

        store.push(&chat, "QmMe", "hi", true, false);
        store.push(&chat, "QmOther", "yo", false, true);
        // 缓冲有两条，字段保留
        assert_eq!(store.all().len(), 2);
        assert!(store.all().iter().any(|m| m.text == "hi" && m.outgoing));
        assert!(store.all().iter().any(|m| m.text == "yo" && m.sealed && !m.outgoing));

        // 落盘成功
        assert_eq!(sqlite::count(&db.lock(), &chat).unwrap(), 2);
        assert!(store.persisted());
    }

    #[test]
    fn hydrate_paging_order_deterministic() {
        // 显式递增时间戳，避免毫秒级同值导致排序不确定
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        sqlite::init(&conn).unwrap();
        let db = Arc::new(Mutex::new(conn));
        let chat = dm_chat_id("QmMe", "QmOther");
        for (i, (txt, out)) in [("old", true), ("mid", false), ("new", true)].into_iter().enumerate() {
            sqlite::insert(
                &db.lock(),
                &MsgRow {
                    chat_id: chat.clone(),
                    sender: "peer".into(),
                    text: txt.into(),
                    outgoing: out,
                    sealed: false,
                    t: 10 + i as u64,
                },
            ).unwrap();
        }

        let store = Store::with_sql(db).unwrap();
        assert_eq!(store.len(), 0, "预填前缓冲为空");
        let n = store.hydrate(&chat).unwrap();
        assert_eq!(n, 3, "灌入全部 3 条");
        // 缓冲为 旧→新
        assert_eq!(store.all()[0].text, "old");
        assert_eq!(store.all()[2].text, "new");

        // 分页：新→旧
        let p0 = store.load(&chat, 2, 0).unwrap();
        assert_eq!(p0.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), vec!["new", "mid"]);
        let p1 = store.load(&chat, 2, 2).unwrap();
        assert_eq!(p1.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), vec!["old"]);
    }
}
