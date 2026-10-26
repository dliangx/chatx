-- Denormalized display name of the post's original author. Carried over the
-- moment-sync wire so a device can show the publisher's name even when the
-- author has no local `users` row (or an anonymous one).
ALTER TABLE social_posts ADD COLUMN user_name TEXT;

-- Backfill from the local users table where the author is known.
UPDATE social_posts
   SET user_name = (SELECT COALESCE(u.nickname, u.username)
                      FROM users u
                     WHERE u.id = social_posts.author_id)
 WHERE user_name IS NULL;
