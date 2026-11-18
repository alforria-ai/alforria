-- What the server knows about where a session's text came from
-- (`session::store::SessionMark`). Server-owned: nothing a client sends
-- writes here, and a row is never removed below `admin`.
--
--   outside_content  the session has seen content from outside (a web
--                    page, an MCP tool's answer, an upload, a trigger's
--                    payload). Provenance only: what it writes to memory is
--                    unverified. It changes no permission.
--   outside_start    a run in the session was started by outside input (a
--                    webhook or watch trigger), or the session descends
--                    from one that was: the read-only ceiling applies.
CREATE TABLE IF NOT EXISTS session_mark (
    session_id TEXT NOT NULL,
    mark TEXT NOT NULL,
    time_created INTEGER NOT NULL,
    PRIMARY KEY (session_id, mark)
) WITHOUT ROWID;

-- The one mark there was lived in the session's metadata as
-- `untrustedInput`, where a client could write it, and it meant both things.
-- Anything stored there that is not plainly "no" (false, 0, null) counts,
-- and so does every descendant, however deep.
WITH RECURSIVE carrying(id) AS (
    SELECT id FROM session
    WHERE json_valid(metadata)
      AND json_type(metadata, '$.untrustedInput') IS NOT NULL
      AND json_type(metadata, '$.untrustedInput') NOT IN ('false', 'null')
      AND json_extract(metadata, '$.untrustedInput') IS NOT 0
    UNION
    SELECT child.id FROM session child JOIN carrying ON child.parent_id = carrying.id
)
INSERT OR IGNORE INTO session_mark (session_id, mark, time_created)
SELECT carrying.id, marks.mark, CAST(strftime('%s', 'now') AS INTEGER) * 1000
FROM carrying, (SELECT 'outside_start' AS mark UNION ALL SELECT 'outside_content') AS marks;

-- The metadata key is no longer read; it goes, so nothing shows a client a
-- mark that is not the server's.
UPDATE session SET metadata = json_remove(metadata, '$.untrustedInput')
WHERE json_valid(metadata) AND json_type(metadata, '$.untrustedInput') IS NOT NULL;
