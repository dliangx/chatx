-- Idempotency marker for inbound group messages we have already persisted.
--
-- A group message now travels BOTH paths at once: live gossipsub AND the
-- server inbox (offline catch-up). The receiver therefore sees the same
-- logical message twice — once via `GroupPacket` and once via `GroupMail`.
--
-- `client_id` is the *sender's* stable id for one logical group message
-- (wall-clock millisecond timestamp), identical on both copies. It is only
-- unique *per sender*, so the dedup key must be composite: two different
-- senders can reuse the same `client_id` and must NOT collide.
--
-- `None` (legacy senders) does NOT go through this table — those are treated
-- as "cannot dedup."
--
-- Rows age out well past any plausible catch-up window (the server inbox
-- retains ~10k messages), so a legitimate late delivery can never be pruned
-- mid-flight and mistaken for a duplicate.
CREATE TABLE IF NOT EXISTS group_inbound_seen (
    client_id   INTEGER NOT NULL,
    peer_base58 TEXT    NOT NULL,
    seen_at_ms  INTEGER NOT NULL,
    PRIMARY KEY (client_id, peer_base58)
);
