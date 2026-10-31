//! SQLite data layer for chatx, backed by sqlx (async, tokio runtime).
//!
//! Every table from `migrations/0001_baseline.sql` has a matching module:
//! single-table CRUD plus cross-table queries in [`joins`].
//! All functions take a `&Pool` so callers manage connection lifetimes.

pub mod conversations;
pub mod devices;
pub mod groups;
pub mod joins;
pub mod messages;
pub mod moment_sync;
pub mod notes;
pub mod settings;
pub mod server;
pub mod social;
pub mod social_feed;
pub mod users;

pub use sqlx::{self, SqlitePool as Pool};
pub use sqlx::sqlite::SqlitePoolOptions;

/// Which kind of media `MsgRow::media_path` points at (used to disambiguate a
/// voice clip from a photo, since both store a `media_path`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum MediaKind {
    #[default]
    None,
    Voice,
    Image,
    /// A document file (pdf/docx/xlsx/pptx); the on-disk path lives in `msg.media_path`, the sender's basename rides in the `text` column.
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MsgRow {
    pub id: i64,
    pub chat_id: String,
    pub sender: String,
    /// Plain-text body. For media rows this is empty; the file is the
    /// on-disk path referenced by `media_path`.
    pub text: String,
    pub sealed: bool,
    pub t: u64,
    pub mine: bool,
    /// Local file path of a media clip/photo; `None` for plain text rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_path: Option<String>,
    /// What `media_path` holds: a voice clip or a photo (`None` = plain text).
    #[serde(default)]
    pub media_kind: MediaKind,
    /// Voice-clip duration in milliseconds; `None` for text/image rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_duration_ms: Option<u64>,
}

/// Auto-collected by `build.rs` from `migrations/*.sql` (lexicographic order).
/// Drop a new `NNNN_name.sql` into that directory — no code change required.
mod migrations_generated {
    include!(concat!(env!("OUT_DIR"), "/migrations_generated.rs"));
}
use migrations_generated::MIGRATIONS;

pub async fn apply_migrations(pool: &Pool) -> anyhow::Result<Vec<u32>> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            applied_at TEXT    NOT NULL
        );",
    )
    .execute(pool)
    .await?;

    let applied: std::collections::HashSet<i64> = {
        let rows: Vec<(i64,)> = sqlx::query_as("SELECT version FROM schema_migrations")
            .fetch_all(pool)
            .await?;
        rows.into_iter().map(|(v,)| v).collect()
    };

    let mut ran = Vec::new();
    for (version, sql) in MIGRATIONS {
        if applied.contains(&(*version as i64)) {
            continue;
        }
        sqlx::raw_sql(sql).execute(pool).await?;
        sqlx::query(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, datetime('now'))",
        )
        .bind(*version as i64)
        .execute(pool)
        .await?;
        ran.push(*version);
    }
    Ok(ran)
}

pub async fn open(path: &std::path::Path) -> anyhow::Result<Pool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let url = format!("sqlite:{}?mode=rwc", path.display());
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await?;
    {
        let mut conn = pool.acquire().await?;
        sqlx::query("PRAGMA journal_mode = WAL").execute(&mut *conn).await?;
        sqlx::query("PRAGMA synchronous = NORMAL").execute(&mut *conn).await?;
        sqlx::query("PRAGMA busy_timeout = 5000").execute(&mut *conn).await?;
        sqlx::query("PRAGMA foreign_keys = ON").execute(&mut *conn).await?;
    }
    apply_migrations(&pool).await?;
    Ok(pool)
}

/// In-memory pool for tests and the memory-backed store path.
/// `min_connections(1)` keeps the single connection (and thus the
/// in-memory database) alive for the lifetime of the pool.
pub async fn open_memory() -> anyhow::Result<Pool> {
    let pool = SqlitePoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .connect("sqlite::memory:")
        .await?;
    {
        let mut conn = pool.acquire().await?;
        sqlx::query("PRAGMA foreign_keys = ON").execute(&mut *conn).await?;
    }
    apply_migrations(&pool).await?;
    Ok(pool)
}

pub async fn init(pool: &Pool) -> anyhow::Result<()> {
    apply_migrations(pool).await?;
    Ok(())
}

/// Generate a unique app-side integer id (63-bit positive value).
pub fn new_id() -> i64 {
    let v: u64 = rand::random();
    (v >> 1) as i64
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
