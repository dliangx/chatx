-- 0013 — dm_send_queue rows carry a hard deadline. Text rows set
-- deadline_ms = NULL (indefinite — passive-deferral forever, wait for the
-- peer's inbound link as long as the app is running). Media rows (voice /
-- image / file) set a deadline: when the redeliver loop sweeps past it, it
-- drops the queue row and flips the local `messages` row to FAILED so the
-- app's send badge can render "failed". The wire kind is derived from the
-- local `messages.row.msg_type` at replay time (no separate column needed).

ALTER TABLE dm_send_queue
    ADD COLUMN deadline_ms INTEGER;
