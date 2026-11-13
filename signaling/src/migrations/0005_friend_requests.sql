-- Server-mediated friend requests (requester_user -> recipient_user).
-- `actor_peer` is the peer_id of the *acting* device (the initiator for
-- Inquire, the decider for Accept/Reject) so the recipient can update its
-- local peer-keyed `fr_requests` row. `actor_user` is the acting device's
-- owner username, carried as the display hint.
CREATE TABLE IF NOT EXISTS friend_requests (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    requester_user TEXT NOT NULL,
    recipient_user TEXT NOT NULL,
    actor_peer     TEXT NOT NULL DEFAULT '',
    actor_user     TEXT NOT NULL DEFAULT '',
    status         TEXT NOT NULL DEFAULT 'pending',
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER,
    UNIQUE (requester_user, recipient_user)
);
CREATE INDEX IF NOT EXISTS idx_fr_requests_inbox
    ON friend_requests (recipient_user, status);
