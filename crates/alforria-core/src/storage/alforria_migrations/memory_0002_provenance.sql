-- Where a memory record came from, as columns the service writes and every
-- query can filter on.
--
-- `trust` is 'verified' only for a record whose origin is known and may be
-- injected into later sessions. Everything else is 'unverified', and that is
-- the default: a row nobody classified is not injected.
ALTER TABLE memory ADD COLUMN trust TEXT NOT NULL DEFAULT 'unverified';
-- JSON: { source, sessionID?, untrusted, history? }.
ALTER TABLE memory ADD COLUMN provenance TEXT;

-- Records written before these columns existed carried their origin in
-- `metadata`, in two shapes. Bring over what can be told; the rest stays
-- unverified.

-- A compaction flush: `metadata.source = "compaction"` (first shape) or
-- `metadata.provenance.source = "flush"` (second).
UPDATE memory SET
    provenance = json_object(
        'source', 'flush',
        'sessionID', COALESCE(json_extract(metadata, '$.provenance.sessionID'), json_extract(metadata, '$.sessionID')),
        'untrusted', json(CASE WHEN COALESCE(json_extract(metadata, '$.provenance.untrusted'), 0) THEN 'true' ELSE 'false' END)),
    trust = 'unverified'
WHERE json_valid(metadata)
  AND (json_extract(metadata, '$.source') = 'compaction'
       OR json_extract(metadata, '$.provenance.source') = 'flush');

-- A client, through /memory (second shape only: the first shape left no
-- mark on these, so they cannot be told from anything else).
UPDATE memory SET
    provenance = json_object('source', 'api', 'untrusted', json('false')),
    trust = 'verified'
WHERE provenance IS NULL AND json_valid(metadata)
  AND json_extract(metadata, '$.provenance.source') = 'api'
  AND COALESCE(json_extract(metadata, '$.provenance.untrusted'), 0) = 0;

-- "This session carries untrusted input" has one definition: the
-- `untrustedInput` mark stored on the session, which a child session gets
-- from its parent when it is created. Sessions made before children
-- inherited the mark get it now, however deep they are.
WITH RECURSIVE carrying(id) AS (
    SELECT id FROM session WHERE COALESCE(json_extract(metadata, '$.untrustedInput'), 0)
    UNION
    SELECT child.id FROM session child JOIN carrying ON child.parent_id = carrying.id
)
UPDATE session
SET metadata = json_set(CASE WHEN json_valid(metadata) THEN metadata ELSE '{}' END, '$.untrustedInput', json('true'))
WHERE id IN (SELECT id FROM carrying);

-- A session whose chain of parents cannot be followed to its root (a parent
-- was deleted) cannot be shown to be free of outside text. It is not marked,
-- but nothing it wrote is promoted below.
CREATE TEMP TABLE memory_0002_unresolved AS
WITH RECURSIVE broken(id) AS (
    SELECT s.id FROM session s
    WHERE s.parent_id IS NOT NULL
      AND NOT EXISTS (SELECT 1 FROM session p WHERE p.id = s.parent_id)
    UNION
    SELECT child.id FROM session child JOIN broken ON child.parent_id = broken.id
)
SELECT id FROM broken;

-- The `memory` tool: `metadata.provenance.source = "tool"` (second shape) or
-- a bare `metadata.sessionID` (first). Verified only if the session still
-- exists, its parents can all be found, and it does not carry the mark.
UPDATE memory SET
    provenance = json_object(
        'source', 'tool',
        'sessionID', COALESCE(json_extract(metadata, '$.provenance.sessionID'), json_extract(metadata, '$.sessionID')),
        'untrusted', json(CASE WHEN
            COALESCE(json_extract(metadata, '$.provenance.untrusted'), 0)
            OR NOT EXISTS (
                SELECT 1 FROM session s
                WHERE s.id = COALESCE(json_extract(memory.metadata, '$.provenance.sessionID'), json_extract(memory.metadata, '$.sessionID')))
            OR EXISTS (
                SELECT 1 FROM memory_0002_unresolved u
                WHERE u.id = COALESCE(json_extract(memory.metadata, '$.provenance.sessionID'), json_extract(memory.metadata, '$.sessionID')))
            OR EXISTS (
                SELECT 1 FROM session s
                WHERE s.id = COALESCE(json_extract(memory.metadata, '$.provenance.sessionID'), json_extract(memory.metadata, '$.sessionID'))
                  AND COALESCE(json_extract(s.metadata, '$.untrustedInput'), 0))
            THEN 'true' ELSE 'false' END))
WHERE provenance IS NULL AND json_valid(metadata)
  AND (json_extract(metadata, '$.provenance.source') = 'tool'
       OR (json_extract(metadata, '$.provenance') IS NULL
           AND json_extract(metadata, '$.source') IS NULL
           AND json_extract(metadata, '$.sessionID') IS NOT NULL));

DROP TABLE memory_0002_unresolved;

UPDATE memory SET trust = 'verified'
WHERE json_extract(provenance, '$.source') = 'tool'
  AND json_extract(provenance, '$.untrusted') = 0;

-- The origin now lives in its own column; the old copies go.
UPDATE memory SET metadata = json_remove(metadata, '$.provenance')
WHERE json_valid(metadata) AND json_extract(metadata, '$.provenance') IS NOT NULL;
UPDATE memory SET metadata = json_remove(metadata, '$.source', '$.sessionID')
WHERE json_valid(metadata) AND json_extract(provenance, '$.source') IN ('flush', 'tool')
  AND (json_extract(metadata, '$.source') = 'compaction' OR json_extract(metadata, '$.sessionID') IS NOT NULL);
UPDATE memory SET metadata = NULL WHERE metadata = '{}';

CREATE INDEX memory_inject_idx ON memory (project_id, archived, trust, time_updated);
