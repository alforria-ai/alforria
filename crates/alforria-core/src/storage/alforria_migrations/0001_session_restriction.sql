-- Sessions created by, or on behalf of, a caller below `admin`
-- (`session::caller`). A row means the session is restricted for life,
-- whoever or whatever prompts it; only an admin lifts it.
CREATE TABLE IF NOT EXISTS session_restriction (
    session_id TEXT PRIMARY KEY,
    time_created INTEGER NOT NULL
);
