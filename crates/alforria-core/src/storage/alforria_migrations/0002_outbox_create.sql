-- Webhook deliveries (`hook.webhook`): one row per (hook, event), kept
-- until delivered and then for the retention period. The row id is the
-- delivery id receivers deduplicate on.
CREATE TABLE IF NOT EXISTS outbox (
    id TEXT PRIMARY KEY,
    hook TEXT NOT NULL,
    event_type TEXT NOT NULL,
    body TEXT NOT NULL,
    time_created INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    time_next INTEGER NOT NULL,
    time_delivered INTEGER,
    time_failed INTEGER,
    last_error TEXT
);
CREATE INDEX IF NOT EXISTS outbox_pending ON outbox (time_delivered, time_failed, time_next);
