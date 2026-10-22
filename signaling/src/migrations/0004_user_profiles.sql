-- ============================================================
-- 0004 user_profiles
--
-- Per-user self-reported display profile: nickname / bio / research /
-- public key / 40×40 avatar (base64-encoded JPEG or PNG, kept small so
-- it fits comfortably inside a JSON body).
--
-- The user_id is a foreign key to `users`; the row may exist before
-- (rare) or after the user first heartbeats, so we deliberately do NOT
-- declare a strict FK to avoid ordering issues on the very first PUT.
--
-- Idempotent: IF NOT EXISTS, safe to re-run.
-- ============================================================

CREATE TABLE IF NOT EXISTS user_profiles (
    user_id      TEXT PRIMARY KEY,
    nickname     TEXT NOT NULL DEFAULT '',
    bio          TEXT NOT NULL DEFAULT '',
    research     TEXT NOT NULL DEFAULT '',
    public_key   TEXT NOT NULL DEFAULT '',
    avatar       TEXT NOT NULL DEFAULT '',     -- base64-encoded image (JPEG/PNG, 40×40)
    updated_at   INTEGER NOT NULL DEFAULT 0
);
