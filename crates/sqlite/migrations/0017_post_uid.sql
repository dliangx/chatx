-- 0017: Stable cross-device identity for a post (the `React` op keys on it).
--
-- `post_uid`: a random 63-bit id minted by the publisher at publish time and
-- carried on the wire (`PostMeta.post_uid`). The receiver stores it as-is
-- (does NOT mint its own) — unlike the local `id` (device-local) and
-- `timestamp` (bumped on re-broadcast) it is identical on every device that
-- has the post. This is the unique, immutable key for a post, fixing the
-- ambiguous (name + content) resolution and the unstable-timestamp problem.
ALTER TABLE social_posts ADD COLUMN post_uid INTEGER DEFAULT NULL;
CREATE INDEX IF NOT EXISTS idx_social_posts_post_uid ON social_posts(post_uid);
