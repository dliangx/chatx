-- ============================================================
-- 0003 Friend requests (peer-based, exchanged over the P2P DM channel)
--
-- `requester` / `recipient` are the libp2p peer ids (base58) of the two
-- devices that took part in the exchange, because the local user-row ids are
-- not shared across devices. `status` is pending / accepted / rejected.
-- `note` is a free-form display name (typically the *other* party's username)
-- that the row's owner captured at send / receive time.
-- Idempotent: IF NOT EXISTS, safe to re-run.
-- ============================================================

CREATE TABLE IF NOT EXISTS fr_requests (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    requester    TEXT NOT NULL,
    recipient    TEXT NOT NULL,
    note         TEXT,
    status       TEXT NOT NULL DEFAULT 'pending',
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER,
    UNIQUE (requester, recipient)
);

CREATE INDEX IF NOT EXISTS idx_fr_requests_recipient ON fr_requests(recipient, status);
CREATE INDEX IF NOT EXISTS idx_fr_requests_requester ON fr_requests(requester, status);
