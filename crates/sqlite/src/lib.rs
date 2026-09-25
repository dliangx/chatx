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
pub mod settings;
pub mod server;
pub mod social;
pub mod social_feed;
pub mod users;

pub use sqlx::{self, SqlitePool as Pool};
pub use sqlx::sqlite::SqlitePoolOptions;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MsgRow {
    pub id: i64,
    pub chat_id: String,
    pub sender: String,
    pub text: String,
    pub sealed: bool,
    pub t: u64,
    pub mine: bool,
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
    for m in MIGRATIONS {
        if applied.contains(&(m.version as i64)) {
            continue;
        }
        sqlx::raw_sql(m.sql).execute(pool).await?;
        sqlx::query(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, datetime('now'))",
        )
        .bind(m.version as i64)
        .execute(pool)
        .await?;
        ran.push(m.version);
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
