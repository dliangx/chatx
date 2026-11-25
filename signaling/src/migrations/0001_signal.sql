-- ============================================================
-- Chatx signaling server schema (final state)
--
-- Self-contained storage for the in-memory directory the signaling
-- server keeps (users / devices / groups), B-mode group mail, post
-- notifications, user profiles, and server-mediated friend requests.
--
-- The server keys everything by the string libp2p PeerId / user id the
-- wire API already uses, so a row round-trips 1:1 with the `chatx_core`
-- records it serves.
--
-- All tables created directly at their final shape. No ALTERs — the
-- `groups.mode` / `groups.name` / `groups.nicknames` columns that were
-- previously added at runtime are inlined here.
--
-- Idempotent: every DDL uses IF NOT EXISTS, safe to re-run.
-- ============================================================

-- Users (one row per account; identity = e2e_public + sign_pk)
CREATE TABLE IF NOT EXISTS users (
    user_id      TEXT PRIMARY KEY,
    e2e_public   TEXT NOT NULL,
    sign_pk      TEXT NOT NULL,
    seen         INTEGER NOT NULL DEFAULT 0   -- last heartbeat
);

-- Devices (one row per device sub-PeerId; authorization chain is durable)
CREATE TABLE IF NOT EXISTS devices (
    peer_id        TEXT PRIMARY KEY,
    user_id        TEXT NOT NULL,
    device_pk      TEXT NOT NULL,
    e2e_public     TEXT NOT NULL,
    label          TEXT NOT NULL DEFAULT '',
    status         TEXT NOT NULL DEFAULT 'pending',   -- pending / approved / revoked
    proposer       TEXT NOT NULL DEFAULT '',
    approved_by    TEXT NOT NULL DEFAULT '',
    approved_at    INTEGER NOT NULL DEFAULT 0,
    attestation    TEXT,                                -- Attestation JSON, NULL when absent
    endpoints      TEXT NOT NULL DEFAULT '[]',          -- JSON array of multiaddrs
    seen           INTEGER NOT NULL DEFAULT 0           -- last presence
);

CREATE INDEX IF NOT EXISTS idx_devices_user ON devices(user_id);

-- Groups (one row per group; members = peer_id -> MemberInfo map as JSON)
CREATE TABLE IF NOT EXISTS groups (
    group_id    TEXT PRIMARY KEY,
    owner       TEXT NOT NULL,
    members     TEXT NOT NULL,      -- BTreeMap<peer_id, {peer_id, sign_pk, e2e_public}> as JSON
    created_at  INTEGER NOT NULL DEFAULT 0,
    mode        TEXT NOT NULL DEFAULT 'p2p',  -- p2p / server (B-mode fanout)
    name        TEXT NOT NULL DEFAULT '',     -- group display name
    nicknames   TEXT NOT NULL DEFAULT ''      -- per-member nickname map as JSON
);

-- B-mode (server-fanout group): one row per sealed group envelope.
-- `seq` is a monotonically increasing cursor assigned by the server so
-- client polls can use `?after=<seq>` to fetch new entries only. `wire`
-- holds the base64 of the `GroupWire{env}` JSON — the envelope is
-- E2E-encrypted under the group key (never the server's key).
CREATE TABLE IF NOT EXISTS group_messages (
    group_id     TEXT    NOT NULL,
    seq          INTEGER NOT NULL,
    sender_peer  TEXT    NOT NULL,
    t            INTEGER NOT NULL DEFAULT 0,
    wire         TEXT    NOT NULL,
    PRIMARY KEY (group_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_group_messages_group_seq
    ON group_messages (group_id, seq);

-- B-mode (server-fanout group): one row per group's member key slots.
-- `slots` is the base64 AES-GCM bundle per member, encrypted under each
-- member's own e2e identity; the server cannot open it.
CREATE TABLE IF NOT EXISTS group_key_slots (
    group_id     TEXT PRIMARY KEY,
    owner        TEXT NOT NULL,
    slots        TEXT NOT NULL DEFAULT '{}'   -- BTreeMap<peer_id, {sender_e2e, sealed}> as JSON
);

-- Moment/post publish notifications: only a pointer (user, peer, ts) is
-- stored — the actual post content stays on the publisher's device and is
-- transferred peer-to-peer.
CREATE TABLE IF NOT EXISTS post_index (
    user_id     TEXT    NOT NULL,
    peer_id     TEXT    NOT NULL,
    post_ts     INTEGER NOT NULL,
    notified_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (peer_id, post_ts)
);

CREATE INDEX IF NOT EXISTS idx_post_index_ts
    ON post_index (post_ts DESC, user_id, peer_id);

-- Per-user self-reported display profile (nickname / bio / research /
-- public key / 40×40 base64 avatar). Deliberately no strict FK to users.
CREATE TABLE IF NOT EXISTS user_profiles (
    user_id      TEXT PRIMARY KEY,
    nickname     TEXT NOT NULL DEFAULT '',
    bio          TEXT NOT NULL DEFAULT '',
    research     TEXT NOT NULL DEFAULT '',
    public_key   TEXT NOT NULL DEFAULT '',
    avatar       TEXT NOT NULL DEFAULT '',     -- base64-encoded image (JPEG/PNG, 40×40)
    updated_at   INTEGER NOT NULL DEFAULT 0
);

-- Server-mediated friend requests (requester_user -> recipient_user).
-- `actor_peer` is the acting device's peer_id; `actor_user` is that
-- device's owner username (display hint).
CREATE TABLE IF NOT EXISTS friend_requests (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    requester_user TEXT NOT NULL,
    recipient_user TEXT NOT NULL,
    actor_peer     TEXT NOT NULL DEFAULT '',
    actor_user     TEXT NOT NULL DEFAULT '',
    status         TEXT NOT NULL DEFAULT 'pending',
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER,
    UNIQUE (requester_user, recipient_user)
);
CREATE INDEX IF NOT EXISTS idx_fr_requests_inbox
    ON friend_requests (recipient_user, status);
