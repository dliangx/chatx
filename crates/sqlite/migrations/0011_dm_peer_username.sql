-- 0011 DM conversations are keyed by the peer's username, not a single
-- device PeerId. One row per *person* regardless of how many devices
-- that person signs in from. The per-device PeerId that p2p features
-- (calls, share, DM dial) still need is resolved on demand from
-- `contacts` / `devices`.
--
-- Rationale for the table rebuild: SQLite only added `DROP COLUMN` in
-- 3.35 (2021); the Android NDK and some older bundled SQLite builds
-- predate that, so the portable form is CREATE / INSERT / DROP / RENAME.

-- Best-effort re-key: DM rows whose legacy `peer_id` is already a
-- recognizable username become `dm:<that username>`. Idempotent (guarded
-- by `name NOT LIKE 'dm:%'`).
UPDATE conversations
   SET name = 'dm:' || peer_id
 WHERE type = 0
   AND name NOT LIKE 'dm:%'
   AND peer_id IS NOT NULL
   AND peer_id != ''
   AND peer_id NOT LIKE 'dm:%'
   AND EXISTS (SELECT 1 FROM users u WHERE u.username = conversations.peer_id);

-- Best-effort re-key: DM rows whose legacy `peer_id` is a libp2p PeerId
-- are mapped through devices -> users to the owner's username.
UPDATE conversations
   SET name = 'dm:' || (
         SELECT u.username
           FROM devices d
           JOIN users u ON u.id = d.user_id
          WHERE d.peer_id = conversations.peer_id
          LIMIT 1)
 WHERE type = 0
   AND name NOT LIKE 'dm:%'
   AND EXISTS (
         SELECT 1 FROM devices d
           JOIN users u ON u.id = d.user_id
          WHERE d.peer_id = conversations.peer_id);

-- Table rebuild: same shape minus the `peer_id` column.
CREATE TABLE IF NOT EXISTS _conversations_v2 (
    id                  INTEGER PRIMARY KEY,
    type                INTEGER NOT NULL,
    name                TEXT,
    avatar_path         TEXT,
    last_message_id     INTEGER,
    last_message_preview TEXT,
    last_message_time   INTEGER,
    unread_count        INTEGER DEFAULT 0,
    is_pinned           INTEGER DEFAULT 0,
    is_muted            INTEGER DEFAULT 0,
    draft               TEXT,
    last_sync_seq       INTEGER DEFAULT 0,
    last_read_seq       INTEGER DEFAULT 0
);

INSERT INTO _conversations_v2
    (id, type, name, avatar_path, last_message_id, last_message_preview,
     last_message_time, unread_count, is_pinned, is_muted, draft,
     last_sync_seq, last_read_seq)
SELECT id, type, name, avatar_path, last_message_id, last_message_preview,
       last_message_time, unread_count, is_pinned, is_muted, draft,
       last_sync_seq, last_read_seq
FROM conversations;

DROP TABLE conversations;

ALTER TABLE _conversations_v2 RENAME TO conversations;

CREATE INDEX IF NOT EXISTS idx_conversations_time ON conversations(last_message_time DESC);
CREATE INDEX IF NOT EXISTS idx_conversations_name ON conversations(name);
