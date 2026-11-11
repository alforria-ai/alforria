//! The `trigger`, `trigger_state` and `trigger_run` tables
//! (`storage/alforria_migrations/trigger_0001_init.sql`).

use std::sync::Arc;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::owner::{Liveness, ProcessOwner};
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
    pub inherited: Option<crate::trigger::CreatorBounds>,
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

/// The reason `shell_env::tighten` is given when a `webhook` or `watch`
/// trigger appears after startup.
pub const OUTSIDE_TRIGGER_REASON: &str = "webhook-or-watch-trigger";

/// Whether any stored trigger, of any directory, is started from outside
/// the server: a `webhook` or a `watch` trigger that is not disabled. Part
/// of what makes a server unattended (`ServerPolicy::unattended_or_remote`
/// in the server crate). A row this build cannot read does not count.
pub fn any_outside_trigger(storage: &Storage) -> Result<bool, CoreError> {
    storage.with_connection(|conn| {
        let mut stmt = conn.prepare("SELECT spec FROM \"trigger\"")?;
        let specs = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(specs
            .iter()
            .filter_map(|spec| serde_json::from_str::<StoredSpec>(spec).ok())
            .any(|stored| stored.spec.starts_from_outside()))
    })
}

/// One directory's slice of the trigger tables.
#[derive(Clone)]
pub struct TriggerStore {
    storage: Arc<Storage>,
    directory: String,
    project_id: String,
    /// The process the runs written through this store belong to.
    owner: ProcessOwner,
}

/// What became of a scheduled tick ([`TriggerStore::claim_tick`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickClaim {
    /// Another process sharing the database took the tick: it is theirs.
    Lost,
    /// The tick is this process's and so is the run, which is stored.
    Run,
    /// The tick is this process's, but the trigger has a run going (here or
    /// in another process): nothing is stored, and the tick is skipped.
    Busy,
}

/// Mark `run` as one that will never finish.
fn interrupt(run: &mut TriggerRun, now_ms: u64) {
    run.status = RunStatus::Failed;
    run.error = Some(crate::trigger::INTERRUPTED.to_string());
    run.time.ended = Some(now_ms);
}

impl TriggerStore {
    pub fn new(storage: Arc<Storage>, directory: &std::path::Path, project_id: &str) -> Self {
        TriggerStore {
            storage,
            directory: directory.to_string_lossy().into_owned(),
            project_id: project_id.to_string(),
            owner: ProcessOwner::current().clone(),
        }
    }

    /// The same slice, written as `owner` (a test's second process).
    pub fn with_owner(mut self, owner: ProcessOwner) -> Self {
        self.owner = owner;
        self
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
        // A trigger that something outside the server starts makes the
        // server unattended from now on, however it came to be stored (the
        // HTTP route, the `schedule` tool): the shell environment's
        // posture tightens before the trigger can fire.
        if trigger.stored.spec.starts_from_outside() {
            crate::shell_env::tighten(OUTSIDE_TRIGGER_REASON);
        }
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

    /// What every trigger of the directory has stored about it, by id.
    pub fn states(&self) -> Result<Vec<(String, TriggerState)>, CoreError> {
        self.storage.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT trigger_id, session_id, paused, last_scheduled, last_fired, time_seen
                 FROM trigger_state WHERE directory = ?1",
            )?;
            let rows = stmt
                .query_map([&self.directory], |row| {
                    let stamp = |value: Option<i64>| value.map(|value| value.max(0) as u64);
                    Ok((
                        row.get::<_, String>(0)?,
                        TriggerState {
                            session_id: row.get(1)?,
                            paused: row.get::<_, Option<i64>>(2)?.map(|value| value != 0),
                            last_scheduled: stamp(row.get(3)?),
                            last_fired: stamp(row.get(4)?),
                            time_seen: row.get::<_, i64>(5)?.max(0) as u64,
                        },
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// A run ended: remember when, and the session a `reuse` trigger keeps.
    /// Only these two columns are written, so a process that did not
    /// schedule the trigger does not put back the `last_scheduled` it
    /// happens to remember over the scheduler's.
    pub fn finish(
        &self,
        id: &str,
        last_fired: u64,
        session_id: Option<&str>,
    ) -> Result<(), CoreError> {
        self.storage.with_connection(|conn| {
            conn.execute(
                "UPDATE trigger_state SET last_fired = ?1, session_id = COALESCE(?2, session_id)
                 WHERE directory = ?3 AND trigger_id = ?4",
                params![last_fired as i64, session_id, self.directory, id],
            )?;
            Ok(())
        })
    }

    /// Insert `run` as its trigger's one unsettled run. `false` when the
    /// trigger already has one that a live process owns. One that a
    /// process which is gone left behind is closed, and the place taken.
    fn insert_active(
        &self,
        conn: &rusqlite::Connection,
        run: &TriggerRun,
    ) -> Result<bool, CoreError> {
        let data =
            serde_json::to_string(run).map_err(|error| CoreError::Storage(error.to_string()))?;
        let owner = serde_json::to_string(&self.owner)
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        let insert = |conn: &rusqlite::Connection| {
            conn.execute(
                "INSERT OR IGNORE INTO trigger_run
                    (id, directory, trigger_id, data, time_started, owner, active)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
                params![
                    run.id,
                    self.directory,
                    run.trigger_id,
                    data,
                    run.time.started as i64,
                    owner
                ],
            )
        };
        if insert(conn)? > 0 {
            return Ok(true);
        }
        let holder = conn
            .query_row(
                "SELECT data, owner FROM trigger_run
                 WHERE directory = ?1 AND trigger_id = ?2 AND active = 1",
                params![self.directory, run.trigger_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let Some((held, holder)) = holder else {
            return Ok(insert(conn)? > 0);
        };
        let gone = holder
            .and_then(|owner| serde_json::from_str::<ProcessOwner>(&owner).ok())
            .is_none_or(|owner| owner.gone());
        if !gone {
            return Ok(false);
        }
        match serde_json::from_str::<TriggerRun>(&held) {
            Ok(mut held) => {
                interrupt(&mut held, run.time.started);
                let data = serde_json::to_string(&held)
                    .map_err(|error| CoreError::Storage(error.to_string()))?;
                conn.execute(
                    "UPDATE trigger_run SET data = ?1, active = NULL WHERE id = ?2",
                    params![data, held.id],
                )?;
            }
            // A row this build cannot read no longer holds the trigger.
            Err(_) => {
                conn.execute(
                    "UPDATE trigger_run SET active = NULL
                     WHERE directory = ?1 AND trigger_id = ?2 AND active = 1",
                    params![self.directory, run.trigger_id],
                )?;
            }
        }
        Ok(insert(conn)? > 0)
    }

    /// Take a fire for this process — a webhook call, a watch, a manual
    /// run: store `run` as the trigger's one unsettled run. `false` means
    /// the trigger has a run going in another process sharing the
    /// database, and this fire does not run.
    pub fn claim_run(&self, run: &TriggerRun) -> Result<bool, CoreError> {
        self.storage
            .with_connection(|conn| self.insert_active(conn, run))
    }

    /// Take a scheduled tick for this process: move `last_scheduled` from
    /// what this process last saw to `now`, and store `run` for it, in one
    /// transaction. A process that dies right after has left the run to
    /// say that the tick was taken and never answered
    /// ([`Self::fail_orphaned`]).
    pub fn claim_tick(
        &self,
        id: &str,
        expected: Option<u64>,
        now: u64,
        run: &TriggerRun,
    ) -> Result<TickClaim, CoreError> {
        self.storage.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let changed = tx.execute(
                "UPDATE trigger_state SET last_scheduled = ?1
                 WHERE directory = ?2 AND trigger_id = ?3 AND last_scheduled IS ?4",
                params![
                    now as i64,
                    self.directory,
                    id,
                    expected.map(|value| value as i64)
                ],
            )?;
            if changed == 0 {
                return Ok(TickClaim::Lost);
            }
            let claim = if self.insert_active(&tx, run)? {
                TickClaim::Run
            } else {
                TickClaim::Busy
            };
            tx.commit()?;
            Ok(claim)
        })
    }

    /// Insert or replace a run, then drop the oldest beyond the cap. A run
    /// that is settled no longer holds its trigger.
    pub fn put_run(&self, run: &TriggerRun) -> Result<(), CoreError> {
        let data =
            serde_json::to_string(run).map_err(|error| CoreError::Storage(error.to_string()))?;
        let owner = serde_json::to_string(&self.owner)
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        self.storage.with_connection(|conn| {
            conn.execute(
                "INSERT INTO trigger_run (id, directory, trigger_id, data, time_started, owner)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (id) DO UPDATE SET
                    data = excluded.data,
                    active = CASE WHEN ?7 THEN NULL ELSE active END",
                params![
                    run.id,
                    self.directory,
                    run.trigger_id,
                    data,
                    run.time.started as i64,
                    owner,
                    run.status.settled(),
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

    /// Runs that can never finish: close them. Those are the unsettled runs
    /// of processes that are gone (and of builds that recorded no owner).
    /// A run another live process owns is that process's, and is left
    /// alone. Where a platform cannot tell whether a process is alive (not
    /// a Unix), every other process's run counts as abandoned.
    pub fn fail_orphaned(&self, now_ms: u64) -> Result<usize, CoreError> {
        let unfinished: Vec<TriggerRun> = self.storage.with_connection(|conn| {
            let mut stmt =
                conn.prepare("SELECT data, owner FROM trigger_run WHERE directory = ?1")?;
            let rows = stmt
                .query_map([&self.directory], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, CoreError>(
                rows.into_iter()
                    .filter_map(|(data, owner)| {
                        Some((serde_json::from_str::<TriggerRun>(&data).ok()?, owner))
                    })
                    .filter(|(run, _)| !run.status.settled())
                    .filter(|(_, owner)| {
                        owner
                            .as_deref()
                            .and_then(|owner| serde_json::from_str::<ProcessOwner>(owner).ok())
                            .is_none_or(|owner| owner.liveness() != Liveness::Alive)
                    })
                    .map(|(run, _)| run)
                    .collect(),
            )
        })?;
        for mut run in unfinished.iter().cloned() {
            interrupt(&mut run, now_ms);
            self.put_run(&run)?;
        }
        Ok(unfinished.len())
    }
}
