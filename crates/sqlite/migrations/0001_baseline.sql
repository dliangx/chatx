-- ============================================================
-- 0002 Core business tables
-- Users/devices, social relations, conversations & messages, groups,
-- settings & plugins, social feed, server-side tables
-- Note: coexists with the msgs table from 0001 (store.rs still reads/writes msgs);
-- this migration only adds the tables above, no conflicts.
-- Idempotent: all DDL uses IF NOT EXISTS, safe to re-run.
-- ============================================================

-- ============================================================
-- 1. Users & devices
-- ============================================================

-- Users table (basic info stored locally, account system on the server)
CREATE TABLE IF NOT EXISTS users (
    id              INTEGER PRIMARY KEY,        -- local account row id
    username        TEXT UNIQUE,                -- username (server-side)
    nickname        TEXT,                       -- display name
    avatar_path     TEXT,                       -- avatar path/URL
    bio             TEXT,                       -- bio
    public_key      TEXT,                       -- account public key (Ed25519)
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER
);

-- Devices table (multi-device login)
CREATE TABLE IF NOT EXISTS devices (
    device_id       INTEGER PRIMARY KEY,        -- unique device ID
    user_id         INTEGER NOT NULL,           -- owning user
    peer_id         TEXT NOT NULL UNIQUE,       -- device sub PeerId
    public_key      TEXT NOT NULL,              -- device public key
    certificate     TEXT,                       -- device certificate signed by account key
    platform        TEXT,                       -- ios / android / desktop
    push_token      TEXT,                       -- APNs/FCM token
    last_seen       INTEGER,                    -- last online time
    created_at      INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id)
);

CREATE INDEX IF NOT EXISTS idx_devices_user ON devices(user_id);
CREATE INDEX IF NOT EXISTS idx_devices_peer ON devices(peer_id);

-- ============================================================
-- 2. Social relations
-- ============================================================

-- Friendships (bidirectional; user_low / user_high guarantees uniqueness)
CREATE TABLE IF NOT EXISTS friendships (
    user_low        INTEGER NOT NULL,           -- lexicographically smaller user ID
    user_high       INTEGER NOT NULL,           -- lexicographically larger user ID
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (user_low, user_high)
);

CREATE INDEX IF NOT EXISTS idx_friendships_low ON friendships(user_low);
CREATE INDEX IF NOT EXISTS idx_friendships_high ON friendships(user_high);

-- Friend requests
CREATE TABLE IF NOT EXISTS friend_requests (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    from_id         INTEGER NOT NULL,
    to_id           INTEGER NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending',  -- pending / accepted / rejected / ignored
    message         TEXT,                       -- verification message
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER,
    UNIQUE (from_id, to_id)
);

CREATE INDEX IF NOT EXISTS idx_friend_requests_to ON friend_requests(to_id, status);

-- Follows (one-directional, like Xiaohongshu)
CREATE TABLE IF NOT EXISTS follows (
    follower_id     INTEGER NOT NULL,           -- the follower
    following_id    INTEGER NOT NULL,           -- the followed
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (follower_id, following_id)
);

CREATE INDEX IF NOT EXISTS idx_follows_following ON follows(following_id);

-- ============================================================
-- 3. Conversations & messages
-- ============================================================

-- Conversations table (one row per chat list item)
CREATE TABLE IF NOT EXISTS conversations (
    id                  INTEGER PRIMARY KEY,    -- conversation id (app-generated)
    type                INTEGER NOT NULL,       -- 0=DM, 1=group
    name                TEXT,                   -- routing key (DM: peer pair string; group: group id)
    peer_id             INTEGER,                -- DM peer's user id (for profile resolution)
    avatar_path         TEXT,                   -- avatar
    last_message_id     INTEGER,                -- last message ID (denormalized)
    last_message_preview TEXT,                  -- last message preview (denormalized)
    last_message_time   INTEGER,                -- last message time (for sorting)
    unread_count        INTEGER DEFAULT 0,      -- unread count
    is_pinned           INTEGER DEFAULT 0,      -- pinned or not
    is_muted            INTEGER DEFAULT 0,      -- muted or not
    draft               TEXT,                   -- draft
    last_sync_seq       INTEGER DEFAULT 0,      -- last synced server sequence
    last_read_seq       INTEGER DEFAULT 0       -- last read sequence (for groups)
);

CREATE INDEX IF NOT EXISTS idx_conversations_time ON conversations(last_message_time DESC);

-- Messages table (DMs and groups stored uniformly)
CREATE TABLE IF NOT EXISTS messages (
    id                  INTEGER PRIMARY KEY,    -- message ID (app-generated)
    conversation_id     INTEGER NOT NULL,       -- owning conversation
    sender_id           INTEGER NOT NULL,       -- sender user ID
    msg_type            INTEGER NOT NULL,       -- 0=text, 1=image, 2=voice, 3=video, 4=file, 5=system
    text_content        TEXT,                   -- text content (text messages only)
    media_path          TEXT,                   -- relative path of the media file
    media_size          INTEGER,                -- file size (bytes)
    media_duration      INTEGER,                -- voice/video duration (seconds)
    thumbnail_path      TEXT,                   -- thumbnail path
    timestamp           INTEGER NOT NULL,       -- send time (milliseconds)
    status              INTEGER NOT NULL DEFAULT 0, -- 0=sending,1=sent,2=delivered,3=read,4=failed
    reply_to            INTEGER,                -- ID of the replied-to message
    is_encrypted        INTEGER DEFAULT 0,      -- whether the content is encrypted
    sync_seq            INTEGER,                -- server sync sequence (groups)
    mentions            TEXT,                   -- list of mentioned PeerIds (JSON)
    FOREIGN KEY (conversation_id) REFERENCES conversations(id)
);

CREATE INDEX IF NOT EXISTS idx_messages_conv_time ON messages(conversation_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_messages_sync ON messages(conversation_id, sync_seq);

-- ============================================================
-- 4. Groups
-- ============================================================

-- Groups table
CREATE TABLE IF NOT EXISTS groups (
    id              INTEGER PRIMARY KEY,        -- group ID
    name            TEXT NOT NULL,
    avatar_path     TEXT,
    owner_id        INTEGER NOT NULL,           -- group owner user ID
    created_at      INTEGER NOT NULL,
    member_count    INTEGER DEFAULT 0,
    last_sync_seq   INTEGER DEFAULT 0           -- last synced group message sequence
);

-- Group members table
CREATE TABLE IF NOT EXISTS group_members (
    group_id        INTEGER NOT NULL,
    peer_id         TEXT NOT NULL,
    nickname        TEXT,                       -- group nickname
    avatar_path     TEXT,
    role            INTEGER DEFAULT 0,          -- 0=member, 1=admin, 2=owner
    joined_at       INTEGER NOT NULL,
    PRIMARY KEY (group_id, peer_id),
    FOREIGN KEY (group_id) REFERENCES groups(id)
);

CREATE INDEX IF NOT EXISTS idx_group_members_peer ON group_members(peer_id);

-- Group message read receipts (optional, for "who read this")
CREATE TABLE IF NOT EXISTS group_message_reads (
    message_id      INTEGER NOT NULL,
    peer_id         TEXT NOT NULL,
    read_at         INTEGER NOT NULL,
    PRIMARY KEY (message_id, peer_id)
);

CREATE INDEX IF NOT EXISTS idx_group_reads_peer ON group_message_reads(peer_id);

-- ============================================================
-- 5. Settings & plugins
-- ============================================================

-- Settings table (key-value pairs)
CREATE TABLE IF NOT EXISTS settings (
    key             TEXT PRIMARY KEY,
    value           TEXT NOT NULL
);

-- Plugins table (locally tracks installed plugins)
CREATE TABLE IF NOT EXISTS plugins (
    id              INTEGER PRIMARY KEY,        -- plugin ID
    name            TEXT NOT NULL,
    version         TEXT NOT NULL,
    path            TEXT NOT NULL,              -- storage path of the plugin package
    enabled         INTEGER DEFAULT 1,          -- enabled or not
    permissions     TEXT,                       -- permission list (JSON)
    installed_at    INTEGER NOT NULL
);

-- ============================================================
-- 6. Social feed (like Moments / Xiaohongshu)
-- ============================================================

-- Feed posts table
CREATE TABLE IF NOT EXISTS social_posts (
    id              INTEGER PRIMARY KEY,
    author_id       INTEGER NOT NULL,           -- author
    content         TEXT,                       -- text content
    media_urls      TEXT,                       -- list of media URLs/paths (JSON)
    visibility      INTEGER DEFAULT 0,          -- 0=public, 1=friends only, 2=private
    timestamp       INTEGER NOT NULL,
    like_count      INTEGER DEFAULT 0,
    comment_count   INTEGER DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_social_posts_author ON social_posts(author_id, timestamp DESC);

-- Likes table
CREATE TABLE IF NOT EXISTS social_likes (
    post_id         INTEGER NOT NULL,
    user_id         INTEGER NOT NULL,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (post_id, user_id)
);

-- Comments table
CREATE TABLE IF NOT EXISTS social_comments (
    id              INTEGER PRIMARY KEY,
    post_id         INTEGER NOT NULL,
    author_id       INTEGER NOT NULL,
    content         TEXT NOT NULL,
    reply_to        INTEGER,                    -- ID of the replied-to comment
    created_at      INTEGER NOT NULL,
    FOREIGN KEY (post_id) REFERENCES social_posts(id)
);

CREATE INDEX IF NOT EXISTS idx_social_comments_post ON social_comments(post_id, created_at);

-- ============================================================
-- 7. Server-side tables
-- ============================================================

-- Offline message queue (server-side staging)
CREATE TABLE IF NOT EXISTS offline_messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    recipient_id    INTEGER NOT NULL,           -- recipient account ID
    message_id      INTEGER NOT NULL,           -- message ID
    sender_id       INTEGER NOT NULL,
    created_at      INTEGER NOT NULL,
    delivered       INTEGER DEFAULT 0,          -- delivered or not
    UNIQUE (recipient_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_offline_recipient ON offline_messages(recipient_id, delivered);

-- Push tokens (server-side)
CREATE TABLE IF NOT EXISTS push_tokens (
    user_id         INTEGER NOT NULL,
    device_id       INTEGER NOT NULL,
    token           TEXT NOT NULL,
    platform        TEXT NOT NULL,              -- ios / android
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (user_id, device_id)
);

-- Message sync sequences (server maintains a global incrementing sequence per conversation)
CREATE TABLE IF NOT EXISTS sync_sequences (
    conversation_id INTEGER PRIMARY KEY,
    last_seq        INTEGER NOT NULL DEFAULT 0
);
