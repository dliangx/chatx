-- 4. Preview images for social posts
--
-- `media_urls` stores the full-resolution originals (copied into CHATX_HOME);
-- `preview_urls` stores small JPEG previews (512px long edge) used for feed
-- cards / grid thumbnails. Both are JSON arrays, same order: the Nth preview
-- corresponds to the Nth original. `preview_urls` may be shorter than
-- `media_urls` (or NULL) when preview generation failed for some items —
-- readers fall back to the original at that index.
ALTER TABLE social_posts ADD COLUMN preview_urls TEXT;
