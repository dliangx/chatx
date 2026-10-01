-- ============================================================
-- 0002 Notes (my note / personal notes)
-- A lightweight text-note store. Owned by a user via user_id.
-- Idempotent: DDL uses IF NOT EXISTS, safe to re-run.
-- ============================================================

CREATE TABLE IF NOT EXISTS notes (
    id          INTEGER PRIMARY KEY,            -- note id (app-generated)
    user_id     INTEGER NOT NULL,               -- owning user
    content     TEXT NOT NULL,                  -- note text
    created_at  INTEGER NOT NULL,               -- creation time (milliseconds)
    updated_at  INTEGER                          -- last edit time (milliseconds)
);

CREATE INDEX IF NOT EXISTS idx_notes_user ON notes(user_id, created_at DESC);
