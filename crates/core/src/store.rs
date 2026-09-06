//! 会话与消息存储（M1 内存版；M2 落 rusqlite + 索引）。
//!
//! `me` 字段移除：客户端自身身份通过 `Client::user_id()` 直接访问，
//! `StoredMsg.peer` 已包含"对方"peer_id。
use crate::message::now_ms;
use libp2p::PeerId;
use parking_lot::RwLock;

#[derive(Debug, Clone)]
pub struct StoredMsg {
    pub peer: PeerId,
    pub text: String,
    pub outgoing: bool,
    pub sealed: bool,
    pub t: u64,
}

#[derive(Debug, Default)]
pub struct Store {
    msgs: RwLock<Vec<StoredMsg>>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, peer: PeerId, text: &str, outgoing: bool, sealed: bool) {
        self.msgs.write().push(StoredMsg {
            peer,
            text: text.to_string(),
            outgoing,
            sealed,
            t: now_ms(),
        });
    }

    pub fn all(&self) -> Vec<StoredMsg> {
        self.msgs.read().clone()
    }

    pub fn len(&self) -> usize {
        self.msgs.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.msgs.read().is_empty()
    }
}
