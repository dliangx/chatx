use crate::{now_ms, Pool};
use sqlx::FromRow;

/// 0=member, 1=admin, 2=owner
pub const ROLE_MEMBER: i64 = 0;
pub const ROLE_ADMIN: i64 = 1;
pub const ROLE_OWNER: i64 = 2;

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub avatar_path: Option<String>,
    pub owner_id: String,
    pub created_at: i64,
    pub member_count: i64,
    pub last_sync_seq: i64,
}

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct GroupMember {
    pub group_id: String,
    pub peer_id: String,
    pub nickname: Option<String>,
    pub avatar_path: Option<String>,
    pub role: i64,
    pub joined_at: i64,
}

pub async fn create(pool: &Pool, id: &str, name: &str, owner_id: &str, avatar_path: Option<&str>) -> anyhow::Result<()> {
    let now = now_ms();
    let mut trans = pool.begin().await?;
    {
        sqlx::query(
            "INSERT INTO groups (id, name, avatar_path, owner_id, created_at, member_count, last_sync_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, 0)
             ON CONFLICT(id) DO UPDATE SET name = ?2, owner_id = ?4",
        )
        .bind(id)
        .bind(name)
        .bind(avatar_path)
        .bind(owner_id)
        .bind(now)
        .execute(trans.as_mut())
        .await?;
        upsert_member(trans.as_mut(), id, owner_id, ROLE_OWNER).await?;
    }
    trans.commit().await?;
    Ok(())
}

pub async fn get(pool: &Pool, id: &str) -> anyhow::Result<Option<Group>> {
    sqlx::query_as::<_, Group>("SELECT * FROM groups WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

pub async fn list(pool: &Pool) -> anyhow::Result<Vec<Group>> {
    sqlx::query_as::<_, Group>("SELECT * FROM groups ORDER BY created_at DESC")
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

pub async fn set_name(pool: &Pool, id: &str, name: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE groups SET name = ?2 WHERE id = ?1")
        .bind(id)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_avatar(pool: &Pool, id: &str, path: Option<&str>) -> anyhow::Result<()> {
    sqlx::query("UPDATE groups SET avatar_path = ?2 WHERE id = ?1")
        .bind(id)
        .bind(path)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_last_sync_seq(pool: &Pool, id: &str, seq: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE groups SET last_sync_seq = ?2 WHERE id = ?1")
        .bind(id)
        .bind(seq)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &Pool, id: &str) -> anyhow::Result<bool> {
    let mut trans = pool.begin().await?;
    {
        sqlx::query("DELETE FROM group_members WHERE group_id = ?1").bind(id).execute(trans.as_mut()).await?;
        let n = sqlx::query("DELETE FROM groups WHERE id = ?1").bind(id).execute(trans.as_mut()).await?;
        if n.rows_affected() == 0 {
            trans.rollback().await?;
            return Ok(false);
        }
        sqlx::query("DELETE FROM group_message_reads WHERE message_id IN (
            SELECT id FROM messages WHERE conversation_id = ?1
        )")
        .bind(id)
        .execute(trans.as_mut())
        .await?;
    }
    trans.commit().await?;
    Ok(true)
}

// --- members ---------------------------------------------------------------

/// Upsert a member inside an open transaction.
pub async fn upsert_member(
    tx: &mut sqlx::SqliteConnection,
    group_id: &str,
    peer_id: &str,
    role: i64,
) -> anyhow::Result<()> {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO group_members (group_id, peer_id, role, joined_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(group_id, peer_id) DO UPDATE SET role = ?3",
    )
    .bind(group_id)
    .bind(peer_id)
    .bind(role)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    sync_member_count(tx, group_id).await?;
    Ok(())
}

/// Add a member and refresh `member_count` within one transaction.
pub async fn add_member(pool: &Pool, group_id: &str, peer_id: &str, role: i64) -> anyhow::Result<()> {
    let mut trans = pool.begin().await?;
    {
        upsert_member(trans.as_mut(), group_id, peer_id, role).await?;
    }
    trans.commit().await?;
    Ok(())
}

/// Remove a member and refresh `member_count` within one transaction.
pub async fn remove_member(pool: &Pool, group_id: &str, peer_id: &str) -> anyhow::Result<bool> {
    let mut trans = pool.begin().await?;
    {
        let n = sqlx::query("DELETE FROM group_members WHERE group_id = ?1 AND peer_id = ?2")
            .bind(group_id)
            .bind(peer_id)
            .execute(trans.as_mut())
            .await?;
        if n.rows_affected() == 0 {
            trans.rollback().await?;
            return Ok(false);
        }
        sync_member_count(trans.as_mut(), group_id).await?;
    }
    trans.commit().await?;
    Ok(true)
}

async fn sync_member_count(tx: &mut sqlx::SqliteConnection, group_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE groups SET
            member_count = (SELECT COUNT(*) FROM group_members WHERE group_id = ?1)
         WHERE id = ?1",
    )
    .bind(group_id)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

pub async fn list_members(pool: &Pool, group_id: &str) -> anyhow::Result<Vec<GroupMember>> {
    let rows = sqlx::query_as::<_, GroupMember>(
        "SELECT * FROM group_members WHERE group_id = ?1 ORDER BY joined_at",
    )
    .bind(group_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn member_ids(pool: &Pool, group_id: &str) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT peer_id FROM group_members WHERE group_id = ?1 ORDER BY joined_at")
            .bind(group_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(s,)| s).collect())
}

pub async fn is_member(pool: &Pool, group_id: &str, peer_id: &str) -> anyhow::Result<bool> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM group_members WHERE group_id = ?1 AND peer_id = ?2")
            .bind(group_id)
            .bind(peer_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.is_some())
}

pub async fn set_member_role(pool: &Pool, group_id: &str, peer_id: &str, role: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE group_members SET role = ?3 WHERE group_id = ?1 AND peer_id = ?2")
        .bind(group_id)
        .bind(peer_id)
        .bind(role)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_member_nickname(pool: &Pool, group_id: &str, peer_id: &str, nickname: Option<&str>) -> anyhow::Result<()> {
    sqlx::query("UPDATE group_members SET nickname = ?3 WHERE group_id = ?1 AND peer_id = ?2")
        .bind(group_id)
        .bind(peer_id)
        .bind(nickname)
        .execute(pool)
        .await?;
    Ok(())
}

/// Groups that a given peer participates in.
pub async fn groups_of(pool: &Pool, peer_id: &str) -> anyhow::Result<Vec<Group>> {
    let rows = sqlx::query_as::<_, Group>(
        "SELECT g.* FROM groups g
         JOIN group_members m ON m.group_id = g.id
         WHERE m.peer_id = ?1
         ORDER BY g.created_at DESC",
    )
    .bind(peer_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// --- message reads ---------------------------------------------------------

#[derive(Debug, Clone, FromRow, serde::Serialize, serde::Deserialize)]
pub struct GroupMessageRead {
    pub message_id: String,
    pub peer_id: String,
    pub read_at: i64,
}

pub async fn mark_read(pool: &Pool, message_id: &str, peer_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO group_message_reads (message_id, peer_id, read_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(message_id, peer_id) DO UPDATE SET read_at = ?3",
    )
    .bind(message_id)
    .bind(peer_id)
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn readers(pool: &Pool, message_id: &str) -> anyhow::Result<Vec<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT peer_id FROM group_message_reads WHERE message_id = ?1 ORDER BY read_at")
            .bind(message_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(s,)| s).collect())
}

pub async fn read_count(pool: &Pool, message_id: &str) -> anyhow::Result<u32> {
    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM group_message_reads WHERE message_id = ?1")
            .bind(message_id)
            .fetch_one(pool)
            .await?;
    Ok(n as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_memory;

    #[tokio::test]
    async fn create_and_membership_roundtrip() {
        let pool = open_memory().await.unwrap();
        create(&pool, "g1", "Test Group", "owner", None).await.unwrap();
        let g = get(&pool, "g1").await.unwrap().unwrap();
        assert_eq!(g.owner_id, "owner");
        assert_eq!(g.member_count, 1, "creator is the initial owner member");
        assert!(is_member(&pool, "g1", "owner").await.unwrap());

        add_member(&pool, "g1", "alice", ROLE_MEMBER).await.unwrap();
        add_member(&pool, "g1", "bob", ROLE_ADMIN).await.unwrap();
        let g = get(&pool, "g1").await.unwrap().unwrap();
        assert_eq!(g.member_count, 3);

        assert_eq!(member_ids(&pool, "g1").await.unwrap().len(), 3);
        assert!(groups_of(&pool, "alice").await.unwrap().iter().any(|g| g.id == "g1"));

        set_member_role(&pool, "g1", "alice", ROLE_OWNER).await.unwrap();
        let members = list_members(&pool, "g1").await.unwrap();
        assert!(members.iter().any(|m| m.peer_id == "alice" && m.role == ROLE_OWNER));

        set_member_nickname(&pool, "g1", "alice", Some("Al")).await.unwrap();
        assert!(list_members(&pool, "g1").await.unwrap().iter().any(|m| m.nickname.as_deref() == Some("Al")));

        assert!(remove_member(&pool, "g1", "alice").await.unwrap());
        assert_eq!(get(&pool, "g1").await.unwrap().unwrap().member_count, 2);
        assert!(!remove_member(&pool, "g1", "alice").await.unwrap());

        mark_read(&pool, "m1", "owner").await.unwrap();
        mark_read(&pool, "m1", "bob").await.unwrap();
        assert_eq!(read_count(&pool, "m1").await.unwrap(), 2);
        assert_eq!(readers(&pool, "m1").await.unwrap().len(), 2);

        assert!(delete(&pool, "g1").await.unwrap());
        assert!(get(&pool, "g1").await.unwrap().is_none());
        assert!(list_members(&pool, "g1").await.unwrap().is_empty());
        assert!(!delete(&pool, "g1").await.unwrap());
    }
}
