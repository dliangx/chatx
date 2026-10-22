use std::collections::HashMap;

use chatx_core::account::{Attestation, DeviceRecord, DeviceStatus, UserRecord};
use chatx_core::friendq::UserProfile;
use chatx_core::group::GroupPublic;
use sqlx::Pool;
use sqlx::sqlite::SqlitePoolOptions;

pub type Sdb = Pool<sqlx::Sqlite>;

pub async fn open(path: &std::path::Path) -> anyhow::Result<Sdb> {
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
        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&mut *conn)
            .await?;
        sqlx::query("PRAGMA synchronous = NORMAL")
            .execute(&mut *conn)
            .await?;
    }
    apply_migrations(&pool).await?;
    Ok(pool)
}

async fn apply_migrations(pool: &Sdb) -> anyhow::Result<()> {
    sqlx::query("CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY);")
        .execute(pool)
        .await?;
    for sql in [
        include_str!("migrations/0001_signal.sql"),
        include_str!("migrations/0002_group_mail.sql"),
        include_str!("migrations/0003_post_index.sql"),
        include_str!("migrations/0004_user_profiles.sql"),
    ] {
        sqlx::raw_sql(sql).execute(pool).await?;
    }
    ensure_group_mode_column(pool).await?;
    Ok(())
}

async fn ensure_group_mode_column(pool: &Sdb) -> anyhow::Result<()> {
    let has: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('groups') WHERE name = 'mode'")
            .fetch_one(pool)
            .await?;
    if has == 0 {
        sqlx::query("ALTER TABLE groups ADD COLUMN mode TEXT NOT NULL DEFAULT 'p2p'")
            .execute(pool)
            .await?;
    }
    Ok(())
}

fn status_to_str(s: &DeviceStatus) -> &'static str {
    match s {
        DeviceStatus::Pending => "pending",
        DeviceStatus::Approved => "approved",
        DeviceStatus::Revoked => "revoked",
    }
}

fn status_from_str(s: &str) -> DeviceStatus {
    match s {
        "approved" => DeviceStatus::Approved,
        "revoked" => DeviceStatus::Revoked,
        _ => DeviceStatus::Pending,
    }
}

// --- upserts (write-through) -------------------------------------------------

pub async fn upsert_user(db: &Sdb, u: &UserRecord) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO users (user_id, e2e_public, sign_pk, seen)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(user_id) DO UPDATE SET
            e2e_public = ?2,
            sign_pk    = ?3,
            seen       = ?4",
    )
    .bind(&u.user_id)
    .bind(&u.e2e_public)
    .bind(&u.sign_pk)
    .bind(u.seen as i64)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn upsert_device(db: &Sdb, d: &DeviceRecord) -> anyhow::Result<()> {
    let att: Option<String> = d
        .attestation
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| anyhow::anyhow!("attestation: {e}"))?;
    let eps = serde_json::to_string(&d.endpoints)?;
    sqlx::query(
        "INSERT INTO devices (peer_id, user_id, device_pk, e2e_public, label, status,
                              proposer, approved_by, approved_at, attestation, endpoints, seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(peer_id) DO UPDATE SET
            user_id      = ?2,
            device_pk    = ?3,
            e2e_public   = ?4,
            label        = ?5,
            status       = ?6,
            proposer     = ?7,
            approved_by  = ?8,
            approved_at  = ?9,
            attestation  = ?10,
            endpoints    = ?11,
            seen         = ?12",
    )
    .bind(&d.peer_id)
    .bind(&d.user_id)
    .bind(&d.device_pk)
    .bind(&d.e2e_public)
    .bind(&d.label)
    .bind(status_to_str(&d.status))
    .bind(&d.proposer)
    .bind(&d.approved_by)
    .bind(d.approved_at as i64)
    .bind(att)
    .bind(eps)
    .bind(d.seen as i64)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn upsert_group(db: &Sdb, g: &GroupPublic) -> anyhow::Result<()> {
    let members = serde_json::to_string(&g.members)?;
    let mode = match g.mode {
        chatx_core::group::GroupMode::Server => "server",
        chatx_core::group::GroupMode::P2P => "p2p",
    };
    sqlx::query(
        "INSERT INTO groups (group_id, owner, members, created_at, mode)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(group_id) DO UPDATE SET
            owner      = ?2,
            members    = ?3,
            created_at = ?4,
            mode       = ?5",
    )
    .bind(&g.group_id)
    .bind(&g.owner)
    .bind(members)
    .bind(g.created_at as i64)
    .bind(mode)
    .execute(db)
    .await?;
    Ok(())
}

// --- deletes ----------------------------------------------------------------

pub async fn delete_device(db: &Sdb, peer_id: &str) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM devices WHERE peer_id = ?1")
        .bind(peer_id)
        .execute(db)
        .await?;
    Ok(n.rows_affected() > 0)
}

// --- B-mode (server-fanout group) -----------------------------------------
pub const GROUP_MESSAGES_KEEP_WINDOW: u64 = 10_000;

pub async fn trim_group_messages(
    db: &Sdb,
    group_id: &str,
    keep_window: u64,
) -> anyhow::Result<u64> {
    if keep_window == 0 {
        return Ok(0);
    }
    let max_seq: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM group_messages WHERE group_id = ?1")
            .bind(group_id)
            .fetch_one(db)
            .await?;
    let floor = max_seq - keep_window as i64 + 1;
    if floor <= 0 {
        return Ok(0);
    }
    let n = sqlx::query("DELETE FROM group_messages WHERE group_id = ?1 AND seq < ?2")
        .bind(group_id)
        .bind(floor)
        .execute(db)
        .await?;
    Ok(n.rows_affected() as u64)
}

pub async fn append_group_message(
    db: &Sdb,
    group_id: &str,
    sender_peer: &str,
    t: u64,
    wire: &str,
) -> anyhow::Result<u64> {
    let next: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM group_messages WHERE group_id = ?1",
    )
    .bind(group_id)
    .fetch_one(db)
    .await?;
    sqlx::query(
        "INSERT INTO group_messages (group_id, seq, sender_peer, t, wire)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(group_id)
    .bind(next)
    .bind(sender_peer)
    .bind(t as i64)
    .bind(wire)
    .execute(db)
    .await?;
    // Enforce the cap (no-op under the window).
    let _ = trim_group_messages(db, group_id, GROUP_MESSAGES_KEEP_WINDOW).await?;
    Ok(next as u64)
}

pub async fn list_group_messages(
    db: &Sdb,
    group_id: &str,
    after: u64,
) -> anyhow::Result<Vec<chatx_core::group::GroupMailEntry>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        seq: i64,
        sender_peer: String,
        t: i64,
        wire: String,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT seq, sender_peer, t, wire FROM group_messages
             WHERE group_id = ?1 AND seq > ?2 ORDER BY seq",
    )
    .bind(group_id)
    .bind(after as i64)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| chatx_core::group::GroupMailEntry {
            group_id: group_id.to_string(),
            seq: r.seq as u64,
            from: r.sender_peer,
            t: r.t as u64,
            wire: r.wire,
        })
        .collect())
}

/// Upsert a single member's sealed group-secret slot.
pub async fn upsert_group_key_slot(
    db: &Sdb,
    group_id: &str,
    owner: &str,
    peer_id: &str,
    e2e_public: &str,
    sealed_b64: &str,
) -> anyhow::Result<()> {
    // Read current slots if any, update this member's entry, write back.
    let mut slots: std::collections::BTreeMap<String, (String, String)> =
        sqlx::query_scalar("SELECT slots FROM group_key_slots WHERE group_id = ?1")
            .bind(group_id)
            .fetch_optional(db)
            .await?
            .map(|s: String| serde_json::from_str(&s).unwrap_or_default())
            .unwrap_or_default();
    slots.insert(
        peer_id.to_string(),
        (e2e_public.to_string(), sealed_b64.to_string()),
    );
    let json = serde_json::to_string(&slots)?;
    sqlx::query(
        "INSERT INTO group_key_slots (group_id, owner, slots)
         VALUES (?1, ?2, ?3)
         ON CONFLICT (group_id) DO UPDATE SET owner = ?2, slots = ?3",
    )
    .bind(group_id)
    .bind(owner)
    .bind(&json)
    .execute(db)
    .await?;
    Ok(())
}

/// Load all key slots for one group.
pub async fn load_group_key_slots(
    db: &Sdb,
    group_id: &str,
) -> anyhow::Result<(String, std::collections::BTreeMap<String, (String, String)>)> {
    #[derive(sqlx::FromRow)]
    struct Row {
        owner: String,
        slots: String,
    }
    let row: Option<Row> =
        sqlx::query_as::<_, Row>("SELECT owner, slots FROM group_key_slots WHERE group_id = ?1")
            .bind(group_id)
            .fetch_optional(db)
            .await?;
    let Some(row) = row else {
        return Ok((String::new(), std::collections::BTreeMap::new()));
    };
    let slots: std::collections::BTreeMap<String, (String, String)> =
        serde_json::from_str(&row.slots).unwrap_or_default();
    Ok((row.owner, slots))
}

pub async fn delete_group_key_slot(
    db: &Sdb,
    group_id: &str,
    peer_id: &str,
) -> anyhow::Result<bool> {
    #[derive(sqlx::FromRow)]
    struct Row {
        slots: String,
    }
    let row: Option<Row> =
        sqlx::query_as::<_, Row>("SELECT slots FROM group_key_slots WHERE group_id = ?1")
            .bind(group_id)
            .fetch_optional(db)
            .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let mut slots: std::collections::BTreeMap<String, (String, String)> =
        serde_json::from_str(&row.slots).unwrap_or_default();
    let removed = slots.remove(peer_id).is_some();
    if removed {
        let json = serde_json::to_string(&slots)?;
        if slots.is_empty() {
            sqlx::query("DELETE FROM group_key_slots WHERE group_id = ?1")
                .bind(group_id)
                .execute(db)
                .await?;
        } else {
            sqlx::query("UPDATE group_key_slots SET slots = ?2 WHERE group_id = ?1")
                .bind(group_id)
                .bind(json)
                .execute(db)
                .await?;
        }
    }
    Ok(removed)
}

// --- Moment/post publish notifications -------------------------------------

/// Retention bound: keep at most this many `post_index` rows. The index only
/// needs to be fresh enough that a client returning within a few days sees
/// its friends' new posts; everything older is already synced (the per-friend
/// cursor in `moment_sync` is the real high-water mark).
pub const POST_INDEX_KEEP: i64 = 50_000;

/// Record that `peer_id` (account `user_id`) published a post at `post_ts`.
/// Idempotent: re-notifying an existing (peer, post_ts) just refreshes
/// `notified_at`.
pub async fn notify_post(
    db: &Sdb,
    user_id: &str,
    peer_id: &str,
    post_ts: i64,
) -> anyhow::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    sqlx::query(
        "INSERT INTO post_index (user_id, peer_id, post_ts, notified_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(peer_id, post_ts) DO UPDATE SET
            user_id     = ?1,
            notified_at = ?4",
    )
    .bind(user_id)
    .bind(peer_id)
    .bind(post_ts)
    .bind(now)
    .execute(db)
    .await?;
    trim_post_index(db).await?;
    Ok(())
}

/// Return all `(user_id, peer_id, post_ts)` where `since < post_ts <= until`,
/// newest first. This is the "is any of my friends publishing a new post"
/// query the sync loop uses.
pub async fn posts_since(
    db: &Sdb,
    since: i64,
    until: i64,
) -> anyhow::Result<Vec<(String, String, i64)>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        user_id: String,
        peer_id: String,
        post_ts: i64,
    }
    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        "SELECT user_id, peer_id, post_ts FROM post_index
         WHERE post_ts > ?1 AND post_ts <= ?2
         ORDER BY post_ts DESC",
    )
    .bind(since)
    .bind(until)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.user_id, r.peer_id, r.post_ts))
        .collect())
}

async fn trim_post_index(db: &Sdb) -> anyhow::Result<u64> {
    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM post_index").fetch_one(db).await?;
    if total <= POST_INDEX_KEEP {
        return Ok(0);
    }
    let n = sqlx::query(
        "DELETE FROM post_index
         WHERE (peer_id, post_ts) NOT IN (
             SELECT peer_id, post_ts FROM post_index
             ORDER BY post_ts DESC LIMIT ?1
         )",
    )
    .bind(POST_INDEX_KEEP)
    .execute(db)
    .await?;
    Ok(n.rows_affected() as u64)
}

// --- read helpers (every request reads straight from the db) ----------------

pub async fn get_user_exact(db: &Sdb, user_id: &str) -> anyhow::Result<Option<UserRecord>> {
    let row: Option<UserRow> = sqlx::query_as::<_, UserRow>("SELECT * FROM users WHERE user_id = ?1")
        .bind(user_id)
        .fetch_optional(db)
        .await?;
    Ok(row.map(UserRow::into_record))
}

pub async fn resolve_user_db(db: &Sdb, user_id: &str) -> anyhow::Result<UserRecord> {
    if let Some(r) = get_user_exact(db, user_id).await? {
        return Ok(r);
    }
    let rows: Vec<UserRow> = sqlx::query_as::<_, UserRow>(
        "SELECT * FROM users WHERE user_id LIKE ?1 || '%'",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?;
    let rows: Vec<UserRecord> = rows.into_iter().map(UserRow::into_record).collect();
    match rows.len() {
        1 => Ok(rows.into_iter().next().unwrap()),
        _ => anyhow::bail!("user {user_id} not found or ambiguous"),
    }
}

pub async fn resolve_device_db(db: &Sdb, peer_id: &str) -> anyhow::Result<DeviceRecord> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM devices WHERE peer_id = ?1",
    )
    .bind(peer_id)
    .fetch_one(db)
    .await?;
    if n > 0 {
        let row: DevRow =
            sqlx::query_as::<_, DevRow>("SELECT * FROM devices WHERE peer_id = ?1")
                .bind(peer_id)
                .fetch_one(db)
                .await?;
        return Ok(row.into_record());
    }
    let rows: Vec<DevRow> = sqlx::query_as::<_, DevRow>(
        "SELECT * FROM devices WHERE peer_id LIKE ?1 || '%'",
    )
    .bind(peer_id)
    .fetch_all(db)
    .await?;
    let rows: Vec<DeviceRecord> = rows.into_iter().map(DevRow::into_record).collect();
    match rows.len() {
        1 => Ok(rows.into_iter().next().unwrap()),
        _ => anyhow::bail!("device {peer_id} not found or ambiguous"),
    }
}

pub async fn list_user_devices_db(db: &Sdb, user_id: &str) -> anyhow::Result<Vec<DeviceRecord>> {
    let mut out: Vec<DeviceRecord> = Vec::new();
    let rows: Vec<DevRow> =
        sqlx::query_as::<_, DevRow>("SELECT * FROM devices WHERE user_id = ?1 ORDER BY peer_id")
            .bind(user_id)
            .fetch_all(db)
            .await?;
    for r in rows {
        out.push(r.into_record());
    }
    Ok(out)
}

/// True if `approver` is an APPROVED device of the same account (`user_id`),
/// distinct from `self_peer`.
pub async fn approver_is_trusted(
    db: &Sdb,
    user_id: &str,
    approver: &str,
    self_peer: &str,
) -> anyhow::Result<bool> {
    let Ok(Some(a)) = get_device_exact(db, approver).await else {
        return Ok(false);
    };
    Ok(a.status == DeviceStatus::Approved && a.user_id == user_id && approver != self_peer)
}

pub async fn get_device_exact(db: &Sdb, peer_id: &str) -> anyhow::Result<Option<DeviceRecord>> {
    let row: Option<DevRow> =
        sqlx::query_as::<_, DevRow>("SELECT * FROM devices WHERE peer_id = ?1")
            .bind(peer_id)
            .fetch_optional(db)
            .await?;
    Ok(row.map(DevRow::into_record))
}

/// A user + their best online APPROVED device (newest `seen` <= TTL), or `None`
/// if the user has no such device.
pub async fn best_online_for_user(
    db: &Sdb,
    user_id: &str,
    exclude_peer: &str,
    now: u64,
) -> anyhow::Result<Option<DeviceRecord>> {
    let rows = list_user_devices_db(db, user_id).await?;
    use chatx_core::signal::ONLINE_TTL_MS;
    let mut best: Option<&DeviceRecord> = None;
    for d in &rows {
        if d.status != DeviceStatus::Approved {
            continue;
        }
        if now.saturating_sub(d.seen) > ONLINE_TTL_MS {
            continue;
        }
        if !exclude_peer.is_empty() && d.peer_id == exclude_peer {
            continue;
        }
        match best {
            None => best = Some(d),
            Some(b) if d.seen > b.seen => best = Some(d),
            _ => {}
        }
    }
    Ok(best.cloned())
}

pub async fn list_users(db: &Sdb) -> anyhow::Result<Vec<UserRecord>> {
    let mut out: Vec<UserRecord> = Vec::new();
    let rows: Vec<UserRow> = sqlx::query_as::<_, UserRow>("SELECT * FROM users")
        .fetch_all(db)
        .await?;
    for r in rows {
        out.push(r.into_record());
    }
    Ok(out)
}

// --- user profile (display info) -------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(dead_code)] // `user_id` / `updated_at` are present to match the
                    // SELECT * row shape but only `updated_at` is consumed.
struct ProfileRow {
    user_id: String,
    nickname: String,
    bio: String,
    research: String,
    public_key: String,
    avatar: String,
    updated_at: i64,
}

impl ProfileRow {
    fn into_profile(r: Self) -> UserProfile {
        UserProfile {
            nickname: (!r.nickname.is_empty()).then(|| r.nickname),
            bio: (!r.bio.is_empty()).then(|| r.bio),
            research: (!r.research.is_empty()).then(|| r.research),
            public_key: (!r.public_key.is_empty()).then(|| r.public_key),
            avatar: (!r.avatar.is_empty()).then(|| r.avatar),
        }
    }
}

pub async fn publish_profile_db(
    db: &Sdb,
    user_id: &str,
    p: &UserProfile,
) -> anyhow::Result<()> {
    let now = chatx_core::message::now_ms();
    let nick = p.nickname.as_deref().unwrap_or("").to_string();
    let bio = p.bio.as_deref().unwrap_or("").to_string();
    let research = p.research.as_deref().unwrap_or("").to_string();
    let public_key = p.public_key.as_deref().unwrap_or("").to_string();
    let avatar = p.avatar.as_deref().unwrap_or("").to_string();
    sqlx::query(
        "INSERT INTO user_profiles (user_id, nickname, bio, research, public_key, avatar, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(user_id) DO UPDATE SET
            nickname   = COALESCE(NULLIF(?2, ''), nickname),
            bio        = COALESCE(NULLIF(?3, ''), bio),
            research   = COALESCE(NULLIF(?4, ''), research),
            public_key = COALESCE(NULLIF(?5, ''), public_key),
            avatar     = COALESCE(NULLIF(?6, ''), avatar),
            updated_at = ?7",
    )
    .bind(user_id)
    .bind(nick)
    .bind(bio)
    .bind(research)
    .bind(public_key)
    .bind(avatar)
    .bind(now as i64)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn fetch_profile_db(db: &Sdb, user_id: &str) -> anyhow::Result<Option<UserProfile>> {
    let row: Option<ProfileRow> =
        sqlx::query_as::<_, ProfileRow>("SELECT * FROM user_profiles WHERE user_id = ?1")
            .bind(user_id)
            .fetch_optional(db)
            .await?;
    Ok(row.map(ProfileRow::into_profile))
}

pub async fn get_group_db(db: &Sdb, group_id: &str) -> anyhow::Result<Option<GroupPublic>> {
    let row: Option<GrpRow> =
        sqlx::query_as::<_, GrpRow>("SELECT * FROM groups WHERE group_id = ?1")
            .bind(group_id)
            .fetch_optional(db)
            .await?;
    Ok(row.map(GrpRow::into_record))
}

pub async fn list_groups_db(db: &Sdb) -> anyhow::Result<Vec<GroupPublic>> {
    let mut out: Vec<GroupPublic> = Vec::new();
    let rows: Vec<GrpRow> = sqlx::query_as::<_, GrpRow>("SELECT * FROM groups")
        .fetch_all(db)
        .await?;
    for r in rows {
        out.push(r.into_record());
    }
    Ok(out)
}

/// Update a device's `seen` timestamp and (optionally) its endpoints. Returns
/// `true` if the device existed.
pub async fn update_device_presence(
    db: &Sdb,
    peer_id: &str,
    seen: u64,
    endpoints: Option<Vec<String>>,
) -> anyhow::Result<bool> {
    let endpoints_json = match &endpoints {
        Some(ep) => Some(serde_json::to_string(ep)?),
        None => None,
    };
    let n = if let Some(eps) = &endpoints_json {
        sqlx::query("UPDATE devices SET seen = ?1, endpoints = ?2 WHERE peer_id = ?3")
            .bind(seen as i64)
            .bind(eps)
            .bind(peer_id)
            .execute(db)
            .await?
    } else {
        sqlx::query("UPDATE devices SET seen = ?1 WHERE peer_id = ?2")
            .bind(seen as i64)
            .bind(peer_id)
            .execute(db)
            .await?
    };
    Ok(n.rows_affected() > 0)
}

pub async fn load_all(
    db: &Sdb,
) -> anyhow::Result<(
    HashMap<String, UserRecord>,
    HashMap<String, DeviceRecord>,
    HashMap<String, GroupPublic>,
)> {
    let users: HashMap<String, UserRecord> = sqlx::query_as::<_, UserRow>("SELECT * FROM users")
        .fetch_all(db)
        .await?
        .into_iter()
        .map(UserRow::into_record)
        .map(|r| (r.user_id.clone(), r))
        .collect();

    let devices: HashMap<String, DeviceRecord> =
        sqlx::query_as::<_, DevRow>("SELECT * FROM devices")
            .fetch_all(db)
            .await?
            .into_iter()
            .map(DevRow::into_record)
            .map(|r| (r.peer_id.clone(), r))
            .collect();

    let groups: HashMap<String, GroupPublic> = sqlx::query_as::<_, GrpRow>("SELECT * FROM groups")
        .fetch_all(db)
        .await?
        .into_iter()
        .map(GrpRow::into_record)
        .map(|r| (r.group_id.clone(), r))
        .collect();

    Ok((users, devices, groups))
}

// --- raw row shapes ---------------------------------------------------------

#[derive(sqlx::FromRow)]
struct UserRow {
    user_id: String,
    e2e_public: String,
    sign_pk: String,
    #[allow(dead_code)]
    seen: i64,
}

impl UserRow {
    fn into_record(self) -> UserRecord {
        UserRecord {
            user_id: self.user_id,
            e2e_public: self.e2e_public,
            sign_pk: self.sign_pk,
            seen: self.seen as u64,
        }
    }
}

#[derive(sqlx::FromRow)]
struct DevRow {
    peer_id: String,
    user_id: String,
    device_pk: String,
    e2e_public: String,
    label: String,
    status: String,
    proposer: String,
    approved_by: String,
    approved_at: i64,
    attestation: Option<String>,
    endpoints: String,
    #[allow(dead_code)]
    seen: i64,
}

impl DevRow {
    fn into_record(self) -> DeviceRecord {
        DeviceRecord {
            user_id: self.user_id,
            peer_id: self.peer_id,
            device_pk: self.device_pk,
            e2e_public: self.e2e_public,
            label: self.label,
            endpoints: serde_json::from_str(&self.endpoints).unwrap_or_default(),
            status: status_from_str(&self.status),
            proposer: self.proposer,
            approved_by: self.approved_by,
            attestation: self
                .attestation
                .as_deref()
                .and_then(|s| serde_json::from_str::<Attestation>(s).ok()),
            seen: self.seen as u64,
            approved_at: self.approved_at as u64,
        }
    }
}

#[derive(sqlx::FromRow)]
struct GrpRow {
    group_id: String,
    owner: String,
    members: String,
    created_at: i64,
    mode: Option<String>,
}

impl GrpRow {
    fn into_record(self) -> GroupPublic {
        let members: std::collections::BTreeMap<String, chatx_core::group::MemberInfo> =
            serde_json::from_str(&self.members).unwrap_or_default();
        let mode = match self.mode.as_deref() {
            Some("server") => chatx_core::group::GroupMode::Server,
            _ => chatx_core::group::GroupMode::P2P,
        };
        GroupPublic {
            group_id: self.group_id,
            owner: self.owner,
            members,
            created_at: self.created_at as u64,
            mode,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn tmp_db() -> (tempfile::TempDir, Sdb) {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir.path().join("signal.db")).await.unwrap();
        (dir, db)
    }

    #[tokio::test]
    async fn roundtrip_user_device_group() {
        let (_d, db) = tmp_db().await;

        upsert_user(
            &db,
            &UserRecord {
                user_id: "alice".into(),
                e2e_public: "E2E_ALICE".into(),
                sign_pk: "SIGN_ALICE".into(),
                seen: 1234,
            },
        )
        .await
        .unwrap();

        let att = Attestation {
            user_id: "alice".into(),
            device: "QmDev1".into(),
            device_pk: "PK1".into(),
            action: chatx_core::account::AttestationAction::Approve,
            approver: "QmRoot".into(),
            approver_sign_pk: "SIGN_ALICE".into(),
            approved_at: 999,
            signature: "sig==".into(),
        };
        upsert_device(
            &db,
            &DeviceRecord {
                user_id: "alice".into(),
                peer_id: "QmDev1".into(),
                device_pk: "PK1".into(),
                e2e_public: "E2E_ALICE".into(),
                label: "phone".into(),
                endpoints: vec!["/ip4/1.2.3.4/tcp/8787".into()],
                status: DeviceStatus::Approved,
                proposer: String::new(),
                approved_by: "QmRoot".into(),
                attestation: Some(att.clone()),
                seen: 2000,
                approved_at: 999,
            },
        )
        .await
        .unwrap();
        // a pending sibling device to verify multi-device + status round-trip
        upsert_device(
            &db,
            &DeviceRecord {
                user_id: "alice".into(),
                peer_id: "QmDev2".into(),
                device_pk: "PK2".into(),
                e2e_public: "E2E_ALICE".into(),
                label: "laptop".into(),
                endpoints: vec![],
                status: DeviceStatus::Pending,
                proposer: String::new(),
                approved_by: String::new(),
                attestation: None,
                seen: 1,
                approved_at: 0,
            },
        )
        .await
        .unwrap();

        let mut members = std::collections::BTreeMap::new();
        members.insert(
            "QmDev1".to_string(),
            chatx_core::group::MemberInfo {
                peer_id: "QmDev1".into(),
                sign_pk: "SIGN_ALICE".into(),
                e2e_public: "E2E_ALICE".into(),
            },
        );
        let grp = GroupPublic {
            group_id: "grp-1".into(),
            owner: "QmDev1".into(),
            members,
            created_at: 42,
            mode: chatx_core::group::GroupMode::P2P,
        };
        upsert_group(&db, &grp).await.unwrap();

        // ---- simulate a server restart: fresh pool over the same file ----
        let (users, devices, groups) = load_all(&db).await.unwrap();

        assert_eq!(users.len(), 1);
        let u = &users["alice"];
        assert_eq!(u.e2e_public, "E2E_ALICE");
        assert_eq!(u.sign_pk, "SIGN_ALICE");
        assert_eq!(u.seen, 1234, "last heartbeat is restored from the db");

        assert_eq!(devices.len(), 2);
        let d1 = &devices["QmDev1"];
        assert_eq!(d1.status, DeviceStatus::Approved);
        assert_eq!(d1.approved_by, "QmRoot");
        assert_eq!(d1.approved_at, 999);
        assert_eq!(
            d1.endpoints,
            vec!["/ip4/1.2.3.4/tcp/8787".to_string()],
            "last-known endpoints are restored for reconnection"
        );
        assert_eq!(
            d1.seen, 2000,
            "last presence is restored from the db"
        );
        let a = d1.attestation.as_ref().expect("attestation restored");
        assert_eq!(a.device, "QmDev1");
        assert_eq!(a.signature, "sig==");
        assert_eq!(a.action, chatx_core::account::AttestationAction::Approve);

        assert_eq!(devices["QmDev2"].status, DeviceStatus::Pending);
        assert!(devices["QmDev2"].attestation.is_none());

        assert_eq!(groups.len(), 1);
        let g = &groups["grp-1"];
        assert_eq!(g.owner, "QmDev1");
        assert_eq!(g.created_at, 42);
        assert!(g.members.contains_key("QmDev1"));
        assert_eq!(g.members["QmDev1"].sign_pk, "SIGN_ALICE");

        // ---- delete round-trip ----
        assert!(delete_device(&db, "QmDev1").await.unwrap());
        let (_2u, devs2, _2g) = load_all(&db).await.unwrap();
        assert!(!devs2.contains_key("QmDev1"));
        assert!(delete_device(&db, "QmGhost").await.unwrap() == false);
    }

    #[tokio::test]
    async fn group_mail_and_key_slots_roundtrip() {
        let (_d, db) = tmp_db().await;
        let gid = "grp-b";

        // Inbox: three appends yield strictly increasing seqs.
        let s1 = append_group_message(&db, gid, "peera", 100, "WIRE-A")
            .await
            .unwrap();
        let s2 = append_group_message(&db, gid, "peerb", 200, "WIRE-B")
            .await
            .unwrap();
        let s3 = append_group_message(&db, gid, "peerc", 300, "WIRE-C")
            .await
            .unwrap();
        assert!(s1 < s2 && s2 < s3, "seqs must be strictly increasing");

        // Poll `after=0` returns everything in order.
        let all = list_group_messages(&db, gid, 0).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].wire, "WIRE-A");
        assert_eq!(all[2].wire, "WIRE-C");
        assert_eq!(all[2].group_id, gid);

        // Cursor semantics: `after=seq` excludes that entry.
        let tail = list_group_messages(&db, gid, s2).await.unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].wire, "WIRE-C");
        assert!(list_group_messages(&db, gid, s3).await.unwrap().is_empty());

        // Key slots: upsert two members, read back both, then overwrite one.
        upsert_group_key_slot(&db, gid, "owner", "Qmbob", "E2E-BOB", "SEAL-BOB")
            .await
            .unwrap();
        upsert_group_key_slot(&db, gid, "owner", "Qmcarol", "E2E-CAROL", "SEAL-CAROL")
            .await
            .unwrap();
        let (_owner, slots) = load_group_key_slots(&db, gid).await.unwrap();
        assert_eq!(slots.len(), 2);
        assert_eq!(slots["Qmbob"].0, "E2E-BOB");
        assert_eq!(slots["Qmbob"].1, "SEAL-BOB");

        // Overwrite bob's slot (e.g. key rotation); count unchanged.
        upsert_group_key_slot(&db, gid, "owner", "Qmbob", "E2E-BOB", "SEAL-BOB-2")
            .await
            .unwrap();
        let (_o2, slots2) = load_group_key_slots(&db, gid).await.unwrap();
        assert_eq!(slots2.len(), 2, "overwrite does not add a third slot");
        assert_eq!(slots2["Qmbob"].1, "SEAL-BOB-2");

        // Unknown group yields empty slots, not an error.
        let (_no, empty) = load_group_key_slots(&db, "grp-unknown").await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn kick_deletes_key_slot() {
        let (_d, db) = tmp_db().await;
        let gid = "grp-k";

        upsert_group_key_slot(&db, gid, "owner", "Alice", "E2E-A", "SEAL-A")
            .await
            .unwrap();
        upsert_group_key_slot(&db, gid, "owner", "Bob", "E2E-B", "SEAL-B")
            .await
            .unwrap();

        // Kick Alice.
        assert!(delete_group_key_slot(&db, gid, "Alice").await.unwrap());
        let (_o, slots) = load_group_key_slots(&db, gid).await.unwrap();
        assert!(!slots.contains_key("Alice"));
        assert!(slots.contains_key("Bob"), "Bob stays after Alice is kicked");
        assert_eq!(slots.len(), 1);

        // Kick Bob — row deleted entirely.
        assert!(delete_group_key_slot(&db, gid, "Bob").await.unwrap());
        let (_o2, slots2) = load_group_key_slots(&db, gid).await.unwrap();
        assert!(slots2.is_empty());

        // Repeat kicks return false.
        assert!(!delete_group_key_slot(&db, gid, "Alice").await.unwrap());
        assert!(!delete_group_key_slot(&db, "ghost", "Alice").await.unwrap());
    }

    #[tokio::test]
    async fn inbox_trim_enforces_keep_window() {
        let (_d, db) = tmp_db().await;
        let gid = "grp-trim";

        // Append 7 messages, then request keep_window=3. The oldest 4 go.
        for i in 0..7u64 {
            append_group_message(&db, gid, "owner", i, &format!("WIRE-{i}"))
                .await
                .unwrap();
        }
        assert_eq!(list_group_messages(&db, gid, 0).await.unwrap().len(), 7);

        let removed = trim_group_messages(&db, gid, 3).await.unwrap();
        assert_eq!(removed, 4, "keep 3 -> delete 4 oldest");

        let kept = list_group_messages(&db, gid, 0).await.unwrap();
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].wire, "WIRE-4", "oldest kept is WIRE-4");
        assert_eq!(kept[2].wire, "WIRE-6", "newest kept is WIRE-6");

        // Trimming to a window >= current row count is a no-op.
        assert_eq!(trim_group_messages(&db, gid, 10).await.unwrap(), 0);
        assert_eq!(list_group_messages(&db, gid, 0).await.unwrap().len(), 3);

        // Trimming an unknown group is a no-op.
        assert_eq!(trim_group_messages(&db, "ghost", 3).await.unwrap(), 0);
    }
}
