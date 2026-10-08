-- ============================================================
-- 0001 Signaling server schema
-- Self-contained storage for the in-memory directory that the
-- signaling server keeps in `App` (users / devices / groups).
--
-- Unlike crates/sqlite (the client-local store, integer ids), the
-- server keys everything by the string libp2p PeerId / user id the
-- wire API already uses, so a row round-trips 1:1 with the
-- `chatx_core` records it serves.
--
-- Idempotent: all DDL uses IF NOT EXISTS, safe to re-run.
-- ============================================================

-- Users (one row per account; identity = e2e_public + sign_pk)
CREATE TABLE IF NOT EXISTS users (
    user_id      TEXT PRIMARY KEY,
    e2e_public   TEXT NOT NULL,
    sign_pk      TEXT NOT NULL,
    seen         INTEGER NOT NULL DEFAULT 0   -- last heartbeat; ephemeral, zeroed on reload
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
    seen           INTEGER NOT NULL DEFAULT 0           -- last presence; ephemeral, zeroed on reload
);

CREATE INDEX IF NOT EXISTS idx_devices_user ON devices(user_id);

-- Groups (one row per group; members = peer_id -> MemberInfo map as JSON)
CREATE TABLE IF NOT EXISTS groups (
    group_id    TEXT PRIMARY KEY,
    owner       TEXT NOT NULL,
    members     TEXT NOT NULL,      -- BTreeMap<peer_id, {peer_id, sign_pk, e2e_public}> as JSON
    created_at  INTEGER NOT NULL DEFAULT 0
);
