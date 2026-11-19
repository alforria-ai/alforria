-- Permission ceilings that belong to a session (`session::store`,
-- `trigger::rules`): readings of the rules that cap whatever policy is in
-- force when a turn runs, and that nothing appended later widens. A trigger
-- run's session gets its creator's bounds (`trigger-creator`) and, when
-- outside input started it, the read-only ceiling (`outside-input`).
-- Server-owned like `session_mark`: inherited by children and forks, never
-- written from a request. `readings` is a JSON array of rulesets.
CREATE TABLE IF NOT EXISTS session_ceiling (
    session_id TEXT NOT NULL,
    name TEXT NOT NULL,
    readings TEXT NOT NULL,
    time_updated INTEGER NOT NULL,
    PRIMARY KEY (session_id, name)
) WITHOUT ROWID;
