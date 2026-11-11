-- ============================================================
-- 0012 DM queue: one row per (message, device) pair.
--
-- A username maps to MANY devices (alice -> B, C). A single DM from a
-- friend must be delivered to EVERY approved device of the recipient so
-- each of their clients gets the message. Redelivery therefore tracks each
-- (msg, device) pair independently: a device that is still offline is
-- retried on its own backoff, while a device that already acked is dropped
-- from the queue.
--
-- The queue was previously keyed by `msg_id` alone (one recipient peer per
-- message). Recreate the table with a composite primary key.
-- ============================================================

CREATE TABLE IF NOT EXISTS dm_send_queue_v2 (
    msg_id        INTEGER NOT NULL,             -- local `messages.id` of the pending text row
    peer_base58   TEXT    NOT NULL,             -- ONE recipient device (dial target)
    chat_id       TEXT    NOT NULL,             -- DM chat id (dm:<username>)
    attempts      INTEGER NOT NULL DEFAULT 0,
    next_try_ms   INTEGER NOT NULL,             -- earliest retry timestamp (per pair)
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (msg_id, peer_base58)
);

-- Preserve any in-flight redelivery state from the old schema.
INSERT OR IGNORE INTO dm_send_queue_v2
    (msg_id, peer_base58, chat_id, attempts, next_try_ms, created_at)
SELECT msg_id, peer_base58, chat_id, attempts, next_try_ms, created_at
FROM dm_send_queue;

DROP TABLE dm_send_queue;
ALTER TABLE dm_send_queue_v2 RENAME TO dm_send_queue;
