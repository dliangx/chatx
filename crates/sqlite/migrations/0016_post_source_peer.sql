-- 0016: Track which peer a synced post came from, so the viewer can
-- send a React op back to the post's owner when they like / comment.
-- `NULL` = post was created locally (not synced from a friend).
ALTER TABLE social_posts ADD COLUMN source_peer_id TEXT DEFAULT NULL;
