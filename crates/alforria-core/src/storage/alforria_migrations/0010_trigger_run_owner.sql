-- Two servers on one data directory (M9).
--
-- `owner` is the process a run belongs to (`owner::ProcessOwner`, as
-- JSON): a starting server closes the unfinished runs of processes that
-- are gone, and leaves a live one's alone. Runs written before this have
-- none, and count as nobody's.
--
-- `active` is 1 while a run is queued or running and NULL once it is
-- settled. The unique index is the claim: of two servers that fire the
-- same trigger at the same moment, one insert succeeds.
ALTER TABLE trigger_run ADD COLUMN owner TEXT;
ALTER TABLE trigger_run ADD COLUMN active INTEGER;

CREATE UNIQUE INDEX trigger_run_active_idx ON trigger_run (directory, trigger_id) WHERE active = 1;
