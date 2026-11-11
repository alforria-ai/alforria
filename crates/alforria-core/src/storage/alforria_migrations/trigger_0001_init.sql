-- Triggers created at runtime (the `schedule` tool, `POST /trigger`).
-- Triggers declared in config are not stored here; only their state is.
CREATE TABLE "trigger" (
    directory TEXT NOT NULL,
    id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    spec TEXT NOT NULL,
    origin_session_id TEXT,
    time_created INTEGER NOT NULL,
    time_updated INTEGER NOT NULL,
    PRIMARY KEY (directory, id)
);

-- What the scheduler remembers about a trigger, config-declared or stored.
CREATE TABLE trigger_state (
    directory TEXT NOT NULL,
    trigger_id TEXT NOT NULL,
    session_id TEXT,
    paused INTEGER,
    last_scheduled INTEGER,
    last_fired INTEGER,
    time_seen INTEGER NOT NULL,
    PRIMARY KEY (directory, trigger_id)
);

CREATE TABLE trigger_run (
    id TEXT PRIMARY KEY,
    directory TEXT NOT NULL,
    trigger_id TEXT NOT NULL,
    data TEXT NOT NULL,
    time_started INTEGER NOT NULL
);

CREATE INDEX trigger_run_trigger_idx ON trigger_run (directory, trigger_id, time_started);
