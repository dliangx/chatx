use crate::Pool;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct Setting {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct Plugin {
    pub id: String,
    pub name: String,
    pub version: String,
    pub path: String,
    pub enabled: bool,
    pub permissions: Option<String>,
    pub installed_at: i64,
}

pub async fn get(pool: &Pool, key: &str) -> anyhow::Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE `key` = ?1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(v,)| v))
}

pub async fn set(pool: &Pool, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO settings (`key`, value) VALUES (?1, ?2)
         ON CONFLICT(`key`) DO UPDATE SET value = ?2",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_bool(pool: &Pool, key: &str, default: bool) -> anyhow::Result<bool> {
    Ok(get(pool, key)
        .await?
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(default))
}

pub async fn get_i64(pool: &Pool, key: &str, default: i64) -> anyhow::Result<i64> {
    Ok(get(pool, key).await?.and_then(|v| v.parse().ok()).unwrap_or(default))
}

pub async fn delete(pool: &Pool, key: &str) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM settings WHERE `key` = ?1")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

pub async fn all(pool: &Pool) -> anyhow::Result<Vec<Setting>> {
    let rows = sqlx::query_as::<_, Setting>("SELECT `key`, value FROM settings ORDER BY `key`")
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

// --- plugins ---------------------------------------------------------------

pub async fn upsert_plugin(
    pool: &Pool,
    id: &str,
    name: &str,
    version: &str,
    path: &str,
    permissions: Option<&str>,
) -> anyhow::Result<()> {
    let now = crate::now_ms();
    sqlx::query(
        "INSERT INTO plugins (id, name, version, path, enabled, permissions, installed_at)
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET
            name        = ?2,
            version     = ?3,
            path        = ?4,
            permissions = ?5",
    )
    .bind(id)
    .bind(name)
    .bind(version)
    .bind(path)
    .bind(permissions)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_plugin(pool: &Pool, id: &str) -> anyhow::Result<Option<Plugin>> {
    sqlx::query_as::<_, Plugin>("SELECT * FROM plugins WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

pub async fn list_plugins(pool: &Pool, include_disabled: bool) -> anyhow::Result<Vec<Plugin>> {
    let rows = if include_disabled {
        sqlx::query_as::<_, Plugin>("SELECT * FROM plugins ORDER BY installed_at")
            .fetch_all(pool)
            .await?
    } else {
        sqlx::query_as::<_, Plugin>("SELECT * FROM plugins WHERE enabled = 1 ORDER BY installed_at")
            .fetch_all(pool)
            .await?
    };
    Ok(rows)
}

pub async fn set_plugin_enabled(pool: &Pool, id: &str, enabled: bool) -> anyhow::Result<()> {
    sqlx::query("UPDATE plugins SET enabled = ?2 WHERE id = ?1")
        .bind(id)
        .bind(enabled)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_plugin(pool: &Pool, id: &str) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM plugins WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(n.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    #[tokio::test]
    async fn kv_roundtrip_and_plugins() {
        let pool = open_memory().await.unwrap();
        set(&pool, "theme", "dark").await.unwrap();
        assert_eq!(get(&pool, "theme").await.unwrap().as_deref(), Some("dark"));
        set(&pool, "theme", "light").await.unwrap();
        assert_eq!(get(&pool, "theme").await.unwrap().as_deref(), Some("light"));
        assert_eq!(all(&pool).await.unwrap().len(), 1);

        set(&pool, "beta", "1").await.unwrap();
        assert!(get_bool(&pool, "beta", false).await.unwrap());
        assert!(!get_bool(&pool, "missing", false).await.unwrap());
        assert!(get_bool(&pool, "missing", true).await.unwrap());
        assert_eq!(get_i64(&pool, "n", 7).await.unwrap(), 7);

        assert!(delete(&pool, "beta").await.unwrap());
        assert!(get(&pool, "beta").await.unwrap().is_none());

        upsert_plugin(&pool, "p1", "PluginOne", "1.0.0", "/tmp/p", None).await.unwrap();
        upsert_plugin(&pool, "p1", "PluginOne", "1.1.0", "/tmp/p2", Some("[\"net\"]")).await.unwrap();
        let p = get_plugin(&pool, "p1").await.unwrap().unwrap();
        assert_eq!(p.version, "1.1.0");
        set_plugin_enabled(&pool, "p1", false).await.unwrap();
        assert!(list_plugins(&pool, false).await.unwrap().is_empty());
        assert_eq!(list_plugins(&pool, true).await.unwrap().len(), 1);
        assert!(delete_plugin(&pool, "p1").await.unwrap());
    }
}
