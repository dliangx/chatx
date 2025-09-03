use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{sqlite::SqlitePoolOptions, FromRow, SqlitePool};

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct User {
    #[sqlx(rename = "id")]
    pub id: String,
    #[sqlx(rename = "username")]
    pub username: String,
    #[sqlx(rename = "email")]
    pub email: String,
    #[sqlx(rename = "password_hash")]
    pub password_hash: String,
    #[sqlx(rename = "created_at")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "last_seen")]
    pub last_seen: Option<DateTime<Utc>>,
    #[sqlx(rename = "status")]
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ChatRoom {
    #[sqlx(rename = "id")]
    pub id: String,
    #[sqlx(rename = "name")]
    pub name: String,
    #[sqlx(rename = "room_type")]
    pub room_type: String,
    #[sqlx(rename = "created_by")]
    pub created_by: String,
    #[sqlx(rename = "created_at")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "description")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct RoomMember {
    #[sqlx(rename = "id")]
    pub id: String,
    #[sqlx(rename = "room_id")]
    pub room_id: String,
    #[sqlx(rename = "user_id")]
    pub user_id: String,
    #[sqlx(rename = "joined_at")]
    pub joined_at: DateTime<Utc>,
    #[sqlx(rename = "role")]
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Message {
    #[sqlx(rename = "id")]
    pub id: String,
    #[sqlx(rename = "room_id")]
    pub room_id: String,
    #[sqlx(rename = "sender_id")]
    pub sender_id: String,
    #[sqlx(rename = "content")]
    pub content: String,
    #[sqlx(rename = "message_type")]
    pub message_type: String,
    #[sqlx(rename = "created_at")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "edited_at")]
    pub edited_at: Option<DateTime<Utc>>,
    #[sqlx(rename = "reply_to")]
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Friendship {
    #[sqlx(rename = "id")]
    pub id: String,
    #[sqlx(rename = "user_id")]
    pub user_id: String,
    #[sqlx(rename = "friend_id")]
    pub friend_id: String,
    #[sqlx(rename = "status")]
    pub status: String,
    #[sqlx(rename = "created_at")]
    pub created_at: DateTime<Utc>,
    #[sqlx(rename = "updated_at")]
    pub updated_at: DateTime<Utc>,
}

pub struct Database {
    pool: SqlitePool,
}

impl Database {
    pub async fn new(database_url: &str) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await?;

        Ok(Self { pool })
    }

    pub async fn init(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS users (
                id TEXT PRIMARY KEY,
                username TEXT UNIQUE NOT NULL,
                email TEXT UNIQUE NOT NULL,
                password_hash TEXT NOT NULL,
                created_at DATETIME NOT NULL,
                last_seen DATETIME,
                status TEXT DEFAULT 'offline'
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS chat_rooms (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                room_type TEXT NOT NULL,
                created_by TEXT NOT NULL,
                created_at DATETIME NOT NULL,
                description TEXT,
                FOREIGN KEY (created_by) REFERENCES users (id)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS room_members (
                id TEXT PRIMARY KEY,
                room_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                joined_at DATETIME NOT NULL,
                role TEXT DEFAULT 'member',
                FOREIGN KEY (room_id) REFERENCES chat_rooms (id),
                FOREIGN KEY (user_id) REFERENCES users (id),
                UNIQUE(room_id, user_id)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY,
                room_id TEXT NOT NULL,
                sender_id TEXT NOT NULL,
                content TEXT NOT NULL,
                message_type TEXT DEFAULT 'text',
                created_at DATETIME NOT NULL,
                edited_at DATETIME,
                reply_to TEXT,
                FOREIGN KEY (room_id) REFERENCES chat_rooms (id),
                FOREIGN KEY (sender_id) REFERENCES users (id),
                FOREIGN KEY (reply_to) REFERENCES messages (id)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS friendships (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                friend_id TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at DATETIME NOT NULL,
                updated_at DATETIME NOT NULL,
                FOREIGN KEY (user_id) REFERENCES users (id),
                FOREIGN KEY (friend_id) REFERENCES users (id),
                UNIQUE(user_id, friend_id)
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // User operations
    pub async fn create_user(&self, user: &User) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO users (id, username, email, password_hash, created_at, last_seen, status)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&user.id)
        .bind(&user.username)
        .bind(&user.email)
        .bind(&user.password_hash)
        .bind(user.created_at)
        .bind(user.last_seen)
        .bind(&user.status)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_user_by_username(&self, username: &str) -> Result<Option<User>> {
        let user = sqlx::query_as::<_, User>(
            r#"
            SELECT * FROM users WHERE username = ?
            "#,
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;

        Ok(user)
    }

    pub async fn get_user_by_id(&self, user_id: &str) -> Result<Option<User>> {
        let user = sqlx::query_as::<_, User>(
            r#"
            SELECT * FROM users WHERE id = ?
            "#,
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(user)
    }

    pub async fn update_user_status(&self, user_id: &str, status: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE users SET status = ?, last_seen = ? WHERE id = ?
            "#,
        )
        .bind(status)
        .bind(Utc::now())
        .bind(user_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // Chat room operations
    pub async fn create_chat_room(&self, room: &ChatRoom) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO chat_rooms (id, name, room_type, created_by, created_at, description)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&room.id)
        .bind(&room.name)
        .bind(&room.room_type)
        .bind(&room.created_by)
        .bind(room.created_at)
        .bind(&room.description)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_chat_room(&self, room_id: &str) -> Result<Option<ChatRoom>> {
        let room = sqlx::query_as::<_, ChatRoom>(
            r#"
            SELECT * FROM chat_rooms WHERE id = ?
            "#,
        )
        .bind(room_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(room)
    }

    pub async fn get_user_chat_rooms(&self, user_id: &str) -> Result<Vec<ChatRoom>> {
        let rooms = sqlx::query_as::<_, ChatRoom>(
            r#"
            SELECT cr.* FROM chat_rooms cr
            JOIN room_members rm ON cr.id = rm.room_id
            WHERE rm.user_id = ?
            ORDER BY cr.created_at DESC
            "#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rooms)
    }

    pub async fn get_direct_chat_room(
        &self,
        user1_id: &str,
        user2_id: &str,
    ) -> Result<Option<ChatRoom>> {
        let room = sqlx::query_as::<_, ChatRoom>(
            r#"
            SELECT cr.* FROM chat_rooms cr
            JOIN room_members rm1 ON cr.id = rm1.room_id
            JOIN room_members rm2 ON cr.id = rm2.room_id
            WHERE cr.room_type = 'direct'
            AND rm1.user_id = ? AND rm2.user_id = ?
            "#,
        )
        .bind(user1_id)
        .bind(user2_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(room)
    }

    // Room member operations
    pub async fn add_room_member(&self, member: &RoomMember) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO room_members (id, room_id, user_id, joined_at, role)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(&member.id)
        .bind(&member.room_id)
        .bind(&member.user_id)
        .bind(member.joined_at)
        .bind(&member.role)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_room_members(&self, room_id: &str) -> Result<Vec<User>> {
        let users = sqlx::query_as::<_, User>(
            r#"
            SELECT u.* FROM users u
            JOIN room_members rm ON u.id = rm.user_id
            WHERE rm.room_id = ?
            ORDER BY u.username
            "#,
        )
        .bind(room_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(users)
    }

    // Message operations
    pub async fn create_message(&self, message: &Message) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO messages (id, room_id, sender_id, content, message_type, created_at, edited_at, reply_to)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&message.id)
        .bind(&message.room_id)
        .bind(&message.sender_id)
        .bind(&message.content)
        .bind(&message.message_type)
        .bind(message.created_at)
        .bind(message.edited_at)
        .bind(&message.reply_to)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_messages(
        &self,
        room_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Message>> {
        let messages = sqlx::query_as::<_, Message>(
            r#"
            SELECT m.* FROM messages m
            WHERE m.room_id = ?
            ORDER BY m.created_at DESC
            LIMIT ? OFFSET ?
            "#,
        )
        .bind(room_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(messages)
    }

    pub async fn get_message(&self, message_id: &str) -> Result<Option<Message>> {
        let message = sqlx::query_as::<_, Message>(
            r#"
            SELECT * FROM messages WHERE id = ?
            "#,
        )
        .bind(message_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(message)
    }

    // Friendship operations
    pub async fn create_friendship(&self, friendship: &Friendship) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO friendships (id, user_id, friend_id, status, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&friendship.id)
        .bind(&friendship.user_id)
        .bind(&friendship.friend_id)
        .bind(&friendship.status)
        .bind(friendship.created_at)
        .bind(friendship.updated_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_friends(&self, user_id: &str) -> Result<Vec<User>> {
        let friends = sqlx::query_as::<_, User>(
            r#"
            SELECT u.* FROM users u
            JOIN friendships f ON u.id = f.friend_id
            WHERE f.user_id = ? AND f.status = 'accepted'
            UNION
            SELECT u.* FROM users u
            JOIN friendships f ON u.id = f.user_id
            WHERE f.friend_id = ? AND f.status = 'accepted'
            ORDER BY username
            "#,
        )
        .bind(user_id)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(friends)
    }

    pub async fn get_friend_requests(&self, user_id: &str) -> Result<Vec<User>> {
        let requests = sqlx::query_as::<_, User>(
            r#"
            SELECT u.* FROM users u
            JOIN friendships f ON u.id = f.user_id
            WHERE f.friend_id = ? AND f.status = 'pending'
            ORDER BY f.created_at DESC
            "#,
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(requests)
    }

    pub async fn update_friendship_status(&self, friendship_id: &str, status: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE friendships SET status = ?, updated_at = ? WHERE id = ?
            "#,
        )
        .bind(status)
        .bind(Utc::now())
        .bind(friendship_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}
