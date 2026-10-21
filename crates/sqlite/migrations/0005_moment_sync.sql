-- ============================================================
-- 0005 Moment (post) sync cursor.
--
-- Per-friend high-water mark of the `social_posts.timestamp` we have
-- already imported from that friend's device. The sync loop reads this
-- to know the `since` window to query on the friend side, so a
-- friend that was offline during publishing is not skipped.
--
-- `peer_id` is the base58 libp2p PeerId of the friend's *Approved*
-- device that last answered a sync request (i.e. the dial target).
-- We key by `peer_id` — not the integer `users.id` — because the
-- friends are identified on the wire by peer.
--
-- Idempotent: IF NOT EXISTS, safe to re-run.
-- ============================================================

CREATE TABLE IF NOT EXISTS moment_sync (
    peer_id       TEXT    PRIMARY KEY,
    last_sync_ts  INTEGER NOT NULL DEFAULT 0
);
