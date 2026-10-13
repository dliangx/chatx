-- ============================================================
-- 0002 B-mode (server-fanout group) tables.
--
-- A server-fanout group has a per-group ordered inbox and a set of
-- per-member key slots:
--
--   * group_messages: one row per sealed group envelope. `seq` is a
--     monotonically increasing cursor assigned by the server so client
--     polls can use `?after=<seq>` to fetch new entries only. `wire`
--     holds the base64 of the `GroupWire{env}` JSON — the envelope is
--     E2E-encrypted under the group key (never the server's key), so
--     the server never sees plaintext.
--
--   * group_key_slots: one row per member of a server group. `sealed`
--     is the base64 `AES-GCM(nonce)||ct` bundle of the group secret,
--     encrypted under the member's own e2e identity; the server cannot
--     open it. Retained until overwritten by `Owner` re-publishing a
--     key rotation.
--
-- Idempotent: all DDL uses IF NOT EXISTS, safe to re-run.
-- ============================================================

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

CREATE TABLE IF NOT EXISTS group_key_slots (
    group_id     TEXT PRIMARY KEY,
    owner        TEXT NOT NULL,
    -- BTreeMap<peer_id, {sender_e2e, sealed}> as JSON
    slots        TEXT NOT NULL DEFAULT '{}'
);
