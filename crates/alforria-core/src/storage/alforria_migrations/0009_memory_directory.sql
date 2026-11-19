-- Memory records of the `global` project are kept per directory.
--
-- A directory that is not a repository has no project of its own: every one
-- of them is the project `global`. Records were keyed on the project alone,
-- so all those directories shared one set, and a credential confined to one
-- directory read and changed the records of every other.
--
-- `directory` is where a `global` record was written: the instance's
-- resolved directory, the same string a session of that instance stores.
-- A record of a real project leaves it NULL (its key is the project, which
-- its worktrees share).
ALTER TABLE memory ADD COLUMN directory TEXT;

-- Records that exist already: the one trace of where a record was written
-- is the session its origin names (the `memory` tool and the flush before a
-- compaction record it; `/memory` does not). A record whose origin names no
-- session, a session that is gone, or cannot be read stays NULL. A NULL
-- directory on a `global` record matches no directory: only the operator
-- sees such a record, through `/memory/unattributed`, and files or deletes
-- it there.
UPDATE memory SET directory = (
    SELECT s.directory FROM session s
    WHERE s.id = CASE WHEN json_valid(memory.provenance)
                      THEN json_extract(memory.provenance, '$.sessionID') END
)
WHERE project_id = 'global' AND directory IS NULL;

CREATE INDEX memory_directory_idx ON memory (project_id, directory, archived, time_updated);
