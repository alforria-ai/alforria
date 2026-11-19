-- Close the runs an earlier build left unfinished.
--
-- `0010_trigger_run_owner` added `owner` and `active` and left both NULL
-- on the rows that were already there. A starting server closes the
-- unfinished runs of processes that are gone by reading the rows with
-- `active = 1`, so a run an earlier build left `queued` or `running` was
-- never closed after the upgrade and read as still going for ever.
--
-- Such a row has no owner: no process of this scheme can still be running
-- it. Every run this scheme leaves unfinished has `active = 1`, and is not
-- touched. Running this again, or on a database that never held such a
-- row, changes nothing.
UPDATE trigger_run
SET data = json_set(
    data,
    '$.status', 'failed',
    '$.error', 'interrupted: an earlier version of the server stopped before this run finished; the run was closed when the database was upgraded',
    '$.time.ended', CAST(strftime('%s', 'now') AS INTEGER) * 1000)
WHERE active IS NULL
  AND json_valid(data)
  AND json_extract(data, '$.status') IN ('queued', 'running');
