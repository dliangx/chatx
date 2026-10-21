-- ============================================================
-- 0003 Moment/post publish notifications.
--
-- A client that publishes a new `social_post` notifies the server via
-- `POST /v1/posts {user_id, peer_id, post_ts}` so other clients of the
-- user (and friends of friends, if we extend later) can learn about the
-- new post via `GET /v1/posts?since=&until=` and then dial the publisher's
-- device to fetch the actual content.
--
-- We deliberately only store a *pointer* (user, peer, timestamp) here —
-- the actual post content (text + images) stays on the publisher's
-- device and is transferred peer-to-peer over the moment channel.
--
-- `post_ts` is the publisher's `social_posts.timestamp` (integer ms).
-- Retention: we cap the index so a long-running server doesn't grow
-- unboundedly (see `trim_post_index` in db.rs).
--
-- Idempotent: IF NOT EXISTS / unique key, safe to re-run.
-- ============================================================

CREATE TABLE IF NOT EXISTS post_index (
    user_id     TEXT    NOT NULL,
    peer_id     TEXT    NOT NULL,
    post_ts     INTEGER NOT NULL,
    notified_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (peer_id, post_ts)
);

CREATE INDEX IF NOT EXISTS idx_post_index_ts
    ON post_index (post_ts DESC, user_id, peer_id);
