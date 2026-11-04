-- ============================================================
-- DM send queue (client-side offline redelivery)
--
-- One row per outbound DM text message that has NOT been acked by the
-- remote peer yet. The redelivery task re-sends due rows until the peer
-- acks (delivered=1) or the attempt budget is exhausted (status=failed on
-- the local `messages` row). Text only — media is intentionally not queued
-- (64MiB resends would dominate; media keeps "retry manually on failure").
-- ============================================================

CREATE TABLE IF NOT EXISTS dm_send_queue (
    msg_id        INTEGER PRIMARY KEY,          -- local `messages.id` of the pending text row
    peer_base58   TEXT    NOT NULL,             -- recipient libp2p PeerId (dial target)
    chat_id       TEXT    NOT NULL,             -- DM chat id (both peers' base58, sorted)
    attempts      INTEGER NOT NULL DEFAULT 0,
    next_try_ms   INTEGER NOT NULL,             -- earliest retry timestamp
    created_at    INTEGER NOT NULL
);
