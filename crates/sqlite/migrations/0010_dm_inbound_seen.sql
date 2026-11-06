-- Idempotent marker for inbound DM text we have already persisted.
--
-- `client_id` is the *sender's* stable local `messages.id` for one logical
-- message (a wall-clock millisecond timestamp). Because that value is only
-- unique *per sender*, the dedup key must be composite: two different peers
-- (or two bursts) can reuse the same `client_id` and must NOT collide.
--
-- `None` (legacy / test / fire-and-forget) does NOT go through this table —
-- those are treated as "cannot dedup."
--
-- Rows age out once they are well past the sender's maximum plausible retry
-- window (12 attempts / ~10h backoff ≈ 24h; 7 days is a comfortable safety
-- margin), so a legitimate late redelivery can never be pruned mid-flight.
CREATE TABLE IF NOT EXISTS dm_inbound_seen (
    client_id   INTEGER NOT NULL,
    peer_base58 TEXT    NOT NULL,
    seen_at_ms  INTEGER NOT NULL,
    PRIMARY KEY (client_id, peer_base58)
);
