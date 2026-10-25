-- ============================================================
-- 0007 Post recommendations (a separate, friend-level "recommend" on top
--      of the existing like/heart flow).
--
-- `recommend_count` mirrors `like_count`: a denormalized counter kept in
-- sync with `social_recommends` so feed cards can render it without a join.
-- `social_recommends` is one row per (post, user) recommender — same shape
-- as `social_likes`.
--
-- Idempotent: IF NOT EXISTS + guarded ALTER, safe to re-run.
-- ============================================================

ALTER TABLE social_posts ADD COLUMN recommend_count INTEGER DEFAULT 0;

CREATE TABLE IF NOT EXISTS social_recommends (
    post_id         INTEGER NOT NULL,
    user_id         INTEGER NOT NULL,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (post_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_social_recommends_post ON social_recommends(post_id);
