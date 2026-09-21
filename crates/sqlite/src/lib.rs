
use rusqlite::{params, Connection};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MsgRow {
    pub chat_id: String,
    pub sender: String,
    pub text: String,
    pub sealed: bool,
    pub t: u64,
}


pub struct Migration {
    pub version: u32,
    pub sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: include_str!("../migrations/0001_baseline.sql"),
    },
];

fn ensure_migrations_table(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            applied_at TEXT    NOT NULL
        );",
        [],
    )?;
    Ok(())
}

pub fn apply_migrations(conn: &Connection) -> anyhow::Result<Vec<u32>> {
    ensure_migrations_table(conn)?;
    let applied: std::collections::HashSet<u32> = {
        let mut stmt = conn.prepare("SELECT version FROM schema_migrations")?;
        let rows = stmt.query_map([], |r| r.get::<_, u32>(0))?;
        rows.collect::<std::result::Result<std::collections::HashSet<_>, _>>()
            .unwrap_or_default()
    };
    let mut ran = Vec::new();
    for m in MIGRATIONS {
        if applied.contains(&m.version) {
            continue;
        }
        conn.execute_batch(m.sql)?;
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, strftime('%s','now'))",
            params![m.version],
        )?;
        ran.push(m.version);
    }
    Ok(ran)
}

pub fn open(path: &std::path::Path) -> anyhow::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    apply_migrations(&conn)?;
    Ok(conn)
}

pub fn init(conn: &Connection) -> anyhow::Result<()> {
    apply_migrations(conn).map(|_| ())
}

pub fn insert(conn: &Connection, m: &MsgRow) -> anyhow::Result<()> {
    let id = new_msg_id();
    conn.execute(
        "INSERT INTO conversations (id, type) VALUES (?1, 0)
         ON CONFLICT(id) DO UPDATE SET
            last_message_time  = COALESCE(last_message_time, ?2),
            last_message_preview = COALESCE(last_message_preview, ?3)",
        params![m.chat_id, m.t as i64, m.text],
    )?;
    conn.execute(
        "INSERT INTO messages (id, conversation_id, sender_id, msg_type, text_content, is_encrypted, timestamp)
         VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)",
        params![id, m.chat_id, m.sender, m.text, m.sealed as i64, m.t as i64],
    )?;
    Ok(())
}

pub fn load_all(conn: &Connection, chat_id: &str) -> anyhow::Result<Vec<MsgRow>> {
    let mut stmt = conn.prepare(
        "SELECT conversation_id, sender_id, text_content, is_encrypted, timestamp
         FROM messages WHERE conversation_id = ?1 ORDER BY timestamp DESC",
    )?;
    let rows = stmt.query_map(params![chat_id], row_from)?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
}

pub fn load_page(
    conn: &Connection,
    chat_id: &str,
    limit: u32,
    offset: u32,
) -> anyhow::Result<Vec<MsgRow>> {
    let mut stmt = conn.prepare(
        "SELECT conversation_id, sender_id, text_content, is_encrypted, timestamp
         FROM messages WHERE conversation_id = ?1 ORDER BY timestamp DESC LIMIT ?2 OFFSET ?3",
    )?;
    let rows = stmt.query_map(params![chat_id, limit, offset], row_from)?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
}

pub fn delete_before(conn: &Connection, chat_id: &str, before_t: u64) -> anyhow::Result<usize> {
    let n = conn.execute(
        "DELETE FROM messages WHERE conversation_id = ?1 AND timestamp < ?2",
        params![chat_id, before_t as i64],
    )?;
    Ok(n)
}

pub fn count(conn: &Connection, chat_id: &str) -> anyhow::Result<u32> {
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages WHERE conversation_id = ?1", params![chat_id], |r| r.get(0))?;
    Ok(n as u32)
}

fn row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<MsgRow> {
    Ok(MsgRow {
        chat_id: row.get::<_, String>(0)?,
        sender: row.get::<_, String>(1)?,
        text: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
        sealed: row.get::<_, i64>(3)? != 0,
        t: row.get::<_, i64>(4)? as u64,
    })
}

fn new_msg_id() -> String {
    let mut buf = [0u8; 16];
    for b in buf.iter_mut() {
        *b = rand::random();
    }
    let hex: String = buf.iter().map(|b| format!("{:02x}", b)).collect();
    format!("m_{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_load_roundtrip_and_page() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();

        let a = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        insert(&conn, &MsgRow { chat_id: "c".into(), sender: "s".into(), text: "hi".into(), sealed: false, t: a }).unwrap();
        insert(&conn, &MsgRow { chat_id: "c".into(), sender: "p".into(), text: "yo".into(), sealed: false, t: a + 1 }).unwrap();
        insert(&conn, &MsgRow { chat_id: "c".into(), sender: "s".into(), text: "hey".into(), sealed: true, t: a + 2 }).unwrap();

        assert_eq!(count(&conn, "c").unwrap(), 3);

        let all = load_all(&conn, "c").unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].text, "hey");
        assert_eq!(all[2].text, "hi");
        assert_eq!(all[0].sealed, true);
        assert_eq!(all[0].sender, "s");
        assert_eq!(all[1].sender, "p");

        let page = load_page(&conn, "c", 2, 0).unwrap();
        assert_eq!(page.len(), 2);
        let page2 = load_page(&conn, "c", 2, 2).unwrap();
        assert_eq!(page2.len(), 1);

        let removed = delete_before(&conn, "c", a + 2).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(count(&conn, "c").unwrap(), 1);
    }

    #[test]
    fn baseline_tables_are_created_and_rerun_is_a_noop() {
        let conn = Connection::open_in_memory().unwrap();
        let first = init_and_captured(&conn).unwrap();
        assert!(!first.is_empty(), "首跑应该应用至少一条迁移");

        let second = init_and_captured(&conn).unwrap();
        assert!(second.is_empty(), "重跑不应再应用任何迁移，got: {second:?}");

        let tables: Vec<String> = {
            let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0)).unwrap()
                .collect::<std::result::Result<_,_>>().unwrap()
        };
        for want in [
            "users","devices","friendships","friend_requests","follows",
            "conversations","messages","groups","group_members","group_message_reads",
            "settings","plugins","social_posts","social_likes","social_comments",
            "offline_messages","push_tokens","sync_sequences","schema_migrations",
        ] {
            assert!(tables.iter().any(|t| t == want), "missing table: {want} (have: {tables:?})");
        }
    }

    fn init_and_captured(conn: &Connection) -> anyhow::Result<Vec<u32>> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version    INTEGER PRIMARY KEY,
                applied_at TEXT    NOT NULL
            );",
        )?;
        apply_migrations(conn)
    }
}
