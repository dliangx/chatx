//! 消息持久化（rusqlite）。
//!
//! Phase 1：把聊天消息从内存 `Vec` 落到每个 profile 一个的 `messages.db`，
//! 重启后可恢复；按 `chat_id` 维度存储（1:1 与未来群聊共用一套结构）。
//!
//! 表结构（见 `SCHEMA`）：`msgs(id, chat_id, sender, text, outgoing, sealed, t)`，
//! 复合索引 `(chat_id, t DESC)` 支撑"按会话拉最近 N 条历史"的高频查询。
//!
//! 线程模型：`rusqlite::Connection` 非 `Sync`，但 `sqlite` crate 的 `Connection` 是
//! `Send` + `Sync`（`single-thread` 模式不支持 sync，`bundled` 默认多线程）。这里用
//! `Mutex<Connection>` 串行化写，读也走同一把锁（SQLite 单连接最稳，WAL 下读不阻塞）。

use rusqlite::{params, Connection};

/// 一条消息的持久化行（与 `chatx_core::store::StoredMsg` 一一对应，避免跨 crate 循环依赖）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MsgRow {
    /// 会话 id：1:1 用 `min(peer_a,peer_b)|max(peer_a,peer_b)` 规范化；群聊用群 id。
    pub chat_id: String,
    /// 本条消息发送方的 peer_id（base58，包含自己）。
    pub sender: String,
    /// 消息内容（`sealed=true` 时为 E2E 密文 payload）。
    pub text: String,
    /// 是否我发出去的（true=本端）。
    pub outgoing: bool,
    /// 内容是否为密文。
    pub sealed: bool,
    /// 时间戳（毫秒，UTC）。
    pub t: u64,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS msgs (
    id       INTEGER PRIMARY KEY,
    chat_id  TEXT    NOT NULL,
    sender   TEXT    NOT NULL,
    text     TEXT    NOT NULL,
    outgoing INTEGER NOT NULL,
    sealed   INTEGER NOT NULL,
    t        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_msgs_chat_t ON msgs(chat_id, t DESC);
"#;

/// 打开（不存在则创建）一个持久化存储。启用 WAL：写不阻塞读、崩溃恢复更稳。
pub fn open(path: &std::path::Path) -> anyhow::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// 在给定连接上建表（连接可能已由 `open` 建好，供测试注入内存库）。
pub fn init(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

/// 写入一条消息（`Mutex<Connection>` 串行化）。
pub fn insert(conn: &Connection, m: &MsgRow) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO msgs (chat_id, sender, text, outgoing, sealed, t) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![m.chat_id, m.sender, m.text, m.outgoing as i64, m.sealed as i64, m.t as i64],
    )?;
    Ok(())
}

/// 按会话时间倒序返回全部消息（新→旧）。
pub fn load_all(conn: &Connection, chat_id: &str) -> anyhow::Result<Vec<MsgRow>> {
    let mut stmt = conn.prepare(
        "SELECT chat_id, sender, text, outgoing, sealed, t
         FROM msgs WHERE chat_id = ?1 ORDER BY t DESC",
    )?;
    let rows = stmt.query_map(params![chat_id], row_from)?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
}

/// 分页拉历史：按时间倒序取 `limit` 条，`offset` 为已跳过条数（新→旧）。
pub fn load_page(
    conn: &Connection,
    chat_id: &str,
    limit: u32,
    offset: u32,
) -> anyhow::Result<Vec<MsgRow>> {
    let mut stmt = conn.prepare(
        "SELECT chat_id, sender, text, outgoing, sealed, t
         FROM msgs WHERE chat_id = ?1 ORDER BY t DESC LIMIT ?2 OFFSET ?3",
    )?;
    let rows = stmt.query_map(params![chat_id, limit, offset], row_from)?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
}

/// 删除 `t` 之前的消息；返回删除条数。
pub fn delete_before(conn: &Connection, chat_id: &str, before_t: u64) -> anyhow::Result<usize> {
    let n = conn.execute(
        "DELETE FROM msgs WHERE chat_id = ?1 AND t < ?2",
        params![chat_id, before_t as i64],
    )?;
    Ok(n)
}

/// 某会话消息条数。
pub fn count(conn: &Connection, chat_id: &str) -> anyhow::Result<u32> {
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM msgs WHERE chat_id = ?1", params![chat_id], |r| r.get(0))?;
    Ok(n as u32)
}

fn row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<MsgRow> {
    Ok(MsgRow {
        chat_id: row.get(0)?,
        sender: row.get(1)?,
        text: row.get(2)?,
        outgoing: row.get::<_, i64>(3)? != 0,
        sealed: row.get::<_, i64>(4)? != 0,
        t: row.get::<_, i64>(5)? as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_load_roundtrip_and_page() {
        // 内存库，无文件
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();

        let a = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        insert(&conn, &MsgRow { chat_id: "c".into(), sender: "s".into(), text: "hi".into(), outgoing: true, sealed: false, t: a }).unwrap();
        insert(&conn, &MsgRow { chat_id: "c".into(), sender: "p".into(), text: "yo".into(), outgoing: false, sealed: false, t: a + 1 }).unwrap();
        insert(&conn, &MsgRow { chat_id: "c".into(), sender: "s".into(), text: "hey".into(), outgoing: true, sealed: true, t: a + 2 }).unwrap();

        assert_eq!(count(&conn, "c").unwrap(), 3);

        let all = load_all(&conn, "c").unwrap();
        assert_eq!(all.len(), 3);
        // 新→旧
        assert_eq!(all[0].text, "hey");
        assert_eq!(all[2].text, "hi");
        assert_eq!(all[0].sealed, true);
        assert_eq!(all[1].outgoing, false);

        let page = load_page(&conn, "c", 2, 0).unwrap();
        assert_eq!(page.len(), 2);
        let page2 = load_page(&conn, "c", 2, 2).unwrap();
        assert_eq!(page2.len(), 1);

        let removed = delete_before(&conn, "c", a + 2).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(count(&conn, "c").unwrap(), 1);
    }
}
