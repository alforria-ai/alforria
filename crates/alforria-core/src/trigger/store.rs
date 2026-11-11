//! The `trigger`, `trigger_state` and `trigger_run` tables
//! (`storage/alforria_migrations/trigger_0001_init.sql`).

use std::sync::Arc;

use alforria_schema::permission_v1::PermissionV1Ruleset;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::storage::Storage;
use crate::trigger::{RunStatus, TriggerRun, TriggerSpec, MAX_RUNS_KEPT};
use crate::CoreError;

/// What the `spec` column holds: the spec, plus the rules the trigger
/// inherited from the session that created it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StoredSpec {
    #[serde(flatten)]
    pub spec: TriggerSpec,
    #[serde(
        rename = "inheritedPermission",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub inherited: Option<PermissionV1Ruleset>,
    /// Whoever created the trigger was restricted (`session::caller`): a
    /// credential below `admin`, or the `schedule` tool in a restricted
    /// session. Recorded when it is created and never changed; its runs
    /// are bound by it for as long as it exists.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restricted: bool,
    /// The session that created or last changed it had seen outside
    /// content (`SessionMark::OutsideContent`): its prompt may hold what an
    /// outsider wrote, so the sessions of its runs carry that mark too —
    /// provenance, not the read-only ceiling. Set, never cleared.
    #[serde(
        rename = "outsideContent",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub outside_content: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredTrigger {
    pub id: String,
    pub stored: StoredSpec,
    pub origin_session_id: Option<String>,
    pub time_created: u64,
    pub time_updated: u64,
}

/// What the scheduler remembers about one trigger between restarts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TriggerState {
    pub session_id: Option<String>,
    pub paused: Option<bool>,
    /// The last tick the scheduler dealt with, fired or skipped.
    pub last_scheduled: Option<u64>,
    pub last_fired: Option<u64>,
    /// When the scheduler first saw the trigger.
    pub time_seen: u64,
}

/// One directory's slice of the trigger tables.
#[derive(Clone)]
pub struct TriggerStore {
    storage: Arc<Storage>,
    directory: String,
    project_id: String,
}

impl TriggerStore {
    pub fn new(storage: Arc<Storage>, directory: &std::path::Path, project_id: &str) -> Self {
        TriggerStore {
            storage,
            directory: directory.to_string_lossy().into_owned(),
            project_id: project_id.to_string(),
        }
    }

    pub fn list(&self) -> Result<Vec<StoredTrigger>, CoreError> {
        self.storage.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, spec, origin_session_id, time_created, time_updated
                 FROM \"trigger\" WHERE directory = ?1 ORDER BY time_created, id",
            )?;
            let rows = stmt
                .query_map([&self.directory], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut out = Vec::new();
            for (id, spec, origin_session_id, created, updated) in rows {
                // A row this build cannot read is left alone, not dropped.
                let Ok(stored) = serde_json::from_str::<StoredSpec>(&spec) else {
                    tracing::warn!("skipping trigger {id}: its stored spec does not parse");
                    continue;
                };
                out.push(StoredTrigger {
                    id,
                    stored,
                    origin_session_id,
                    time_created: created.max(0) as u64,
                    time_updated: updated.max(0) as u64,
                });
            }
            Ok(out)
        })
    }

    pub fn put(&self, trigger: &StoredTrigger) -> Result<(), CoreError> {
        let spec = serde_json::to_string(&trigger.stored)
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        self.storage.with_connection(|conn| {
            conn.execute(
                "INSERT INTO \"trigger\"
                    (directory, id, project_id, spec, origin_session_id, time_created, time_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (directory, id) DO UPDATE SET
                    spec = excluded.spec,
                    origin_session_id = excluded.origin_session_id,
                    time_updated = excluded.time_updated",
                params![
                    self.directory,
                    trigger.id,
                    self.project_id,
                    spec,
                    trigger.origin_session_id,
                    trigger.time_created as i64,
                    trigger.time_updated as i64,
                ],
            )?;
            Ok(())
        })
    }

    pub fn delete(&self, id: &str) -> Result<bool, CoreError> {
        self.storage.with_connection(|conn| {
            let removed = conn.execute(
                "DELETE FROM \"trigger\" WHERE directory = ?1 AND id = ?2",
                params![self.directory, id],
            )?;
            conn.execute(
                "DELETE FROM trigger_state WHERE directory = ?1 AND trigger_id = ?2",
                params![self.directory, id],
            )?;
            conn.execute(
                "DELETE FROM trigger_run WHERE directory = ?1 AND trigger_id = ?2",
                params![self.directory, id],
            )?;
            Ok(removed > 0)
        })
    }

    pub fn state(&self, id: &str) -> Result<Option<TriggerState>, CoreError> {
        self.storage.with_connection(|conn| {
            let state = conn
                .query_row(
                    "SELECT session_id, paused, last_scheduled, last_fired, time_seen
                     FROM trigger_state WHERE directory = ?1 AND trigger_id = ?2",
                    params![self.directory, id],
                    |row| {
                        Ok(TriggerState {
                            session_id: row.get(0)?,
                            paused: row.get::<_, Option<i64>>(1)?.map(|value| value != 0),
                            last_scheduled: row
                                .get::<_, Option<i64>>(2)?
                                .map(|value| value.max(0) as u64),
                            last_fired: row
                                .get::<_, Option<i64>>(3)?
                                .map(|value| value.max(0) as u64),
                            time_seen: row.get::<_, i64>(4)?.max(0) as u64,
                        })
                    },
                )
                .optional()?;
            Ok(state)
        })
    }

    pub fn put_state(&self, id: &str, state: &TriggerState) -> Result<(), CoreError> {
        self.storage.with_connection(|conn| {
            conn.execute(
                "INSERT INTO trigger_state
                    (directory, trigger_id, session_id, paused, last_scheduled, last_fired, time_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (directory, trigger_id) DO UPDATE SET
                    session_id = excluded.session_id,
                    paused = excluded.paused,
                    last_scheduled = excluded.last_scheduled,
                    last_fired = excluded.last_fired",
                params![
                    self.directory,
                    id,
                    state.session_id,
                    state.paused.map(i64::from),
                    state.last_scheduled.map(|value| value as i64),
                    state.last_fired.map(|value| value as i64),
                    state.time_seen as i64,
                ],
            )?;
            Ok(())
        })
    }

    /// Take a scheduled tick for this process: move `last_scheduled` from
    /// what this process last saw to `now`, atomically. `false` means another
    /// process sharing the database got there first, and the tick is theirs.
    pub fn claim_tick(&self, id: &str, expected: Option<u64>, now: u64) -> Result<bool, CoreError> {
        self.storage.with_connection(|conn| {
            let changed = conn.execute(
                "UPDATE trigger_state SET last_scheduled = ?1
                 WHERE directory = ?2 AND trigger_id = ?3 AND last_scheduled IS ?4",
                params![
                    now as i64,
                    self.directory,
                    id,
                    expected.map(|value| value as i64)
                ],
            )?;
            Ok(changed > 0)
        })
    }

    /// Insert or replace a run, then drop the oldest beyond the cap.
    pub fn put_run(&self, run: &TriggerRun) -> Result<(), CoreError> {
        let data =
            serde_json::to_string(run).map_err(|error| CoreError::Storage(error.to_string()))?;
        self.storage.with_connection(|conn| {
            conn.execute(
                "INSERT INTO trigger_run (id, directory, trigger_id, data, time_started)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (id) DO UPDATE SET data = excluded.data",
                params![
                    run.id,
                    self.directory,
                    run.trigger_id,
                    data,
                    run.time.started as i64
                ],
            )?;
            conn.execute(
                "DELETE FROM trigger_run WHERE directory = ?1 AND trigger_id = ?2 AND id NOT IN (
                    SELECT id FROM trigger_run WHERE directory = ?1 AND trigger_id = ?2
                    ORDER BY time_started DESC, id DESC LIMIT ?3)",
                params![self.directory, run.trigger_id, MAX_RUNS_KEPT as i64],
            )?;
            Ok(())
        })
    }

    /// A trigger's runs, newest first.
    pub fn runs(&self, id: &str, limit: usize) -> Result<Vec<TriggerRun>, CoreError> {
        self.storage.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT data FROM trigger_run WHERE directory = ?1 AND trigger_id = ?2
                 ORDER BY time_started DESC, id DESC LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(params![self.directory, id, limit as i64], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows
                .iter()
                .filter_map(|data| serde_json::from_str(data).ok())
                .collect())
        })
    }

    pub fn run(&self, run_id: &str) -> Result<Option<TriggerRun>, CoreError> {
        self.storage.with_connection(|conn| {
            let data = conn
                .query_row(
                    "SELECT data FROM trigger_run WHERE directory = ?1 AND id = ?2",
                    params![self.directory, run_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            Ok(data.and_then(|data| serde_json::from_str(&data).ok()))
        })
    }

    /// Runs a previous process left unfinished can never finish: mark them.
    pub fn fail_unfinished(&self, now_ms: u64) -> Result<usize, CoreError> {
        let unfinished: Vec<TriggerRun> = self.storage.with_connection(|conn| {
            let mut stmt = conn.prepare("SELECT data FROM trigger_run WHERE directory = ?1")?;
            let rows = stmt
                .query_map([&self.directory], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, CoreError>(
                rows.iter()
                    .filter_map(|data| serde_json::from_str::<TriggerRun>(data).ok())
                    .filter(|run| !run.status.settled())
                    .collect(),
            )
        })?;
        for mut run in unfinished.iter().cloned() {
            run.status = RunStatus::Failed;
            run.error = Some(crate::trigger::INTERRUPTED.to_string());
            run.time.ended = Some(now_ms);
            self.put_run(&run)?;
        }
        Ok(unfinished.len())
    }
}
