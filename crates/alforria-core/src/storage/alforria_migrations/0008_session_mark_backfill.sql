-- Repairs what earlier builds of this branch left behind, so that every
-- upgrade path ends where a fresh database starts.
--
-- Two things went wrong before `session_mark` existed (0006):
--
--   * `memory_0002_provenance` was edited in place. A database that ran its
--     first body looked only at a session and its direct parent, so a record
--     written at depth 3 of a webhook run was promoted to `verified`.
--   * Builds from before the mark tagged a trigger's session with
--     `metadata.triggerID` and nothing else. Such a session, and everything
--     below it, is unmarked, and what it wrote to memory is `verified`.
--
-- Nothing here can be told from the journal (the id is the same either
-- way), so this runs for everyone, fails closed, and changes nothing on a
-- database that has neither sessions nor records. Every statement leaves a
-- database it has already run on as it is.

-- 1 and 2. A session a trigger started carries both marks unless its
-- trigger is still stored and is plainly started by the clock: `on` holds
-- `cron`, `interval` or `at`, no `watch`, no `webhook: true`, and nothing
-- this build does not know. A trigger that is gone, that only the config
-- declares (it has no row), whose spec does not parse, or that belongs to
-- another directory proves nothing, and the session is marked. So is every
-- descendant of a session with the ceiling mark, however deep (the rule of
-- 0006, over the server's table this time).
WITH RECURSIVE
started(id, directory, trigger_id) AS (
    SELECT id, directory,
           CASE WHEN json_valid(metadata) THEN
               CASE WHEN json_type(metadata, '$.triggerID') = 'text'
                    THEN json_extract(metadata, '$.triggerID') END
           END
    FROM session
    WHERE CASE WHEN json_valid(metadata)
               THEN json_type(metadata, '$.triggerID') IS NOT NULL
               ELSE 0 END
),
clock(directory, id) AS (
    SELECT directory, id FROM "trigger"
    WHERE CASE WHEN json_valid(spec) THEN
              json_type(spec, '$.on') = 'object'
              AND (json_type(spec, '$.on.cron') = 'text'
                   OR json_type(spec, '$.on.interval') = 'text'
                   OR json_type(spec, '$.on.at') = 'text')
              AND COALESCE(json_type(spec, '$.on.webhook'), 'null') IN ('false', 'null')
              AND COALESCE(json_type(spec, '$.on.watch'), 'null') = 'null'
              AND NOT EXISTS (
                  SELECT 1 FROM json_each(spec, '$.on') AS field
                  WHERE field.key NOT IN ('cron', 'timezone', 'interval', 'at', 'webhook', 'watch'))
          ELSE 0 END
),
carrying(id) AS (
    SELECT started.id FROM started
    WHERE NOT EXISTS (
        SELECT 1 FROM clock
        WHERE clock.directory = started.directory AND clock.id = started.trigger_id)
    UNION
    SELECT session_id FROM session_mark WHERE mark = 'outside_start'
    UNION
    SELECT child.id FROM session child JOIN carrying ON child.parent_id = carrying.id
)
INSERT OR IGNORE INTO session_mark (session_id, mark, time_created)
SELECT carrying.id, marks.mark, CAST(strftime('%s', 'now') AS INTEGER) * 1000
FROM carrying, (SELECT 'outside_start' AS mark UNION ALL SELECT 'outside_content') AS marks;

-- The ceiling mark bounds nothing by itself: a turn is judged by the
-- ceilings stored for its session (`session_ceiling`, 0007). A session with
-- the mark and no `outside-input` ceiling gets the read-only one, as
-- `trigger::rules::outside_ceiling` writes it for a trigger with no
-- `permission` block of its own (a test compares the two). One a session
-- already has is left alone, and a trigger's next run in a session it
-- reuses writes its own again.
INSERT OR IGNORE INTO session_ceiling (session_id, name, readings, time_updated)
SELECT session_id, 'outside-input',
       '[[{"permission":"*","pattern":"*","action":"deny"},{"permission":"read","pattern":"*","action":"allow"},{"permission":"grep","pattern":"*","action":"allow"},{"permission":"glob","pattern":"*","action":"allow"},{"permission":"list","pattern":"*","action":"allow"},{"permission":"lsp","pattern":"*","action":"allow"},{"permission":"todowrite","pattern":"*","action":"allow"},{"permission":"invalid","pattern":"*","action":"allow"}]]',
       CAST(strftime('%s', 'now') AS INTEGER) * 1000
FROM session_mark WHERE mark = 'outside_start';

-- 3. A record a session wrote (the `memory` tool, a flush) is unverified
-- when that session has seen outside content or cannot be found: the test
-- `SessionStore::seen_outside_content` makes, with the marks as they now
-- stand. A record with no session (the user's, through /memory) is left as
-- it is, and so is one whose origin does not parse: it is already read as
-- unverified.
UPDATE memory SET
    trust = 'unverified',
    provenance = json_set(provenance, '$.untrusted', json('true'))
WHERE CASE WHEN json_valid(provenance) THEN
          (trust IS NOT 'unverified' OR json_type(provenance, '$.untrusted') IS NOT 'true')
          AND (json_type(provenance, '$.sessionID') IS NOT NULL
               OR COALESCE(json_extract(provenance, '$.source'), '') IN ('tool', 'flush'))
          AND (NOT EXISTS (
                   SELECT 1 FROM session
                   WHERE session.id = json_extract(memory.provenance, '$.sessionID'))
               OR EXISTS(SELECT 1 FROM session_mark WHERE session_mark.session_id = json_extract(memory.provenance, '$.sessionID') AND session_mark.mark = 'outside_content'))
      ELSE 0 END;

-- 4. Both bodies of `memory_0002_provenance` end with this index; said
-- again so that no path through them ends without it.
CREATE INDEX IF NOT EXISTS memory_inject_idx ON memory (project_id, archived, trust, time_updated);
