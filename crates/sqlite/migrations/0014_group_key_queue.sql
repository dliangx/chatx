-- ============================================================
-- 0014 P2P group-key redelivery queue.
--
-- When the owner creates a P2P group (or adds members / rotates the key)
-- the group secret is delivered once, peer by peer, over a direct libp2p
-- link (`deliver_group_key_all`). A member whose dial never succeeds
-- (offline, NAT, unreachable endpoint) misses the key and is silently
-- excluded from the group — there was no durable record of the failure,
-- so the key was never resent.
--
-- This table records each (group, member) pair whose delivery never acked.
-- A background loop (`Client::start_group_key_redeliver`) rescans it and
-- re-sends the key whenever the member is connected locally again; the
-- Connected/Discovered event handler in `process_event` triggers the same
-- redelivery immediately when a pending member dials in. The receiver side
-- (`apply_inbound::GroupKey`) is idempotent, so redelivery is safe.
-- Rows self-clean: the loop drops them when the group no longer exists
-- locally or the peer is no longer a member (owner left / member kicked).
-- ============================================================

CREATE TABLE IF NOT EXISTS group_key_queue (
    group_id      TEXT NOT NULL,     -- group id (grp-<num>)
    peer_base58   TEXT NOT NULL,     -- member device that never acked the key
    attempts      INTEGER NOT NULL DEFAULT 0,
    next_try_ms   INTEGER NOT NULL,  -- earliest retry timestamp
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (group_id, peer_base58)
);
