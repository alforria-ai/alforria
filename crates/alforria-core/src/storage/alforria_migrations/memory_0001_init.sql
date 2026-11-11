-- Long-term memory records (the `memory` tool, `/memory`).
CREATE TABLE memory (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    content TEXT NOT NULL,
    metadata TEXT,
    archived INTEGER NOT NULL DEFAULT 0,
    time_created INTEGER NOT NULL,
    time_updated INTEGER NOT NULL
);

CREATE INDEX memory_project_idx ON memory (project_id, archived, time_updated);

-- Full-text index over memory.content, kept in step by the memory service.
CREATE VIRTUAL TABLE memory_fts USING fts5(
    content,
    id UNINDEXED,
    tokenize = 'unicode61 remove_diacritics 2'
);

-- Full-text index over the text parts of past sessions, built lazily in the
-- background (see `memory/index.rs`). Nothing writes to it when a message is
-- written.
CREATE VIRTUAL TABLE transcript_fts USING fts5(
    text,
    part_id UNINDEXED,
    message_id UNINDEXED,
    session_id UNINDEXED,
    role UNINDEXED,
    tokenize = 'unicode61 remove_diacritics 2'
);

-- Where the transcript indexer has got to: `cursor` is the rowid of the last
-- part it looked at and `anchor` that part's id, so a rowid that was freed
-- and used again is noticed. An indexed part keeps its rowid in
-- transcript_fts.
CREATE TABLE transcript_index (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    cursor INTEGER NOT NULL,
    anchor TEXT,
    time_updated INTEGER NOT NULL
);

INSERT INTO transcript_index (id, cursor, time_updated) VALUES (1, 0, 0);
