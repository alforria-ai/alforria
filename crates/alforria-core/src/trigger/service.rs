//! The trigger scheduler: what is due, what runs, and what is remembered.
//!
//! [`TriggerService::tick`] is the whole scheduler: given the time, it fires
//! what is due and plans the next occurrence. The background loop only
//! decides when to call it, so tests drive it with a clock of their own.
//!
//! A trigger never has two runs at once: a tick that arrives while the last
//! run is still going (or still waiting for a slot) is recorded as skipped.
//! Across triggers, `concurrency` runs go at a time and the rest wait, which
//! cannot pile up because each trigger waits at most once.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use alforria_schema::permission_v1::PermissionV1Ruleset;
use futures::FutureExt;
use serde_json::{json, Value};

use crate::event::{EventBus, PublishOptions};
use crate::owner::RoleLock;
use crate::tool::permission::from_config;
use crate::trigger::rules::CreatorBounds;
use crate::trigger::store::{StoredSpec, StoredTrigger, TickClaim, TriggerState, TriggerStore};
use crate::trigger::watch::{self, WatchHandle, WatchIgnore};
use crate::trigger::{
    format_duration, Creator, Report, RunStatus, RunTime, Schedule, SessionMode, TriggerError,
    TriggerOps, TriggerPayload, TriggerRecord, TriggerRun, TriggerSource, TriggerSpec, TriggerTurn,
    MAX_AGENT_TRIGGERS, MAX_PAYLOAD_BYTES, MAX_STORED_TRIGGERS, TRIGGER_COMPLETED, TRIGGER_FAILED,
    TRIGGER_FIRED, TRIGGER_SKIPPED, TRIGGER_UPDATED,
};
use crate::Clock;

/// Where triggers live and how the scheduler behaves.
#[derive(Debug, Clone)]
pub struct TriggerSettings {
    pub directory: PathBuf,
    pub project_id: String,
    pub home: PathBuf,
    /// `false` when `triggers.enabled` is `false` (or the environment
    /// turns triggers off): nothing is accepted, fired or offered.
    pub enabled: bool,
    /// `triggers.enabled` is not written. Triggers are then off until
    /// there is one: the scheduler does not start and the `schedule` tool
    /// is not offered ([`TriggerService::active`]) while the directory has
    /// no trigger, in the config or stored — a server whose owner uses
    /// none of this runs as it did before triggers existed. The first one
    /// created (`POST /trigger`) starts the scheduler.
    pub on_demand: bool,
    /// How many runs go at once, across all triggers.
    pub concurrency: usize,
    /// The most entries one `watch` may cover.
    pub max_watched: usize,
    /// Directories no watch reports: alforria's own.
    pub ignore: Vec<PathBuf>,
    /// The `trigger` map from the config.
    pub config: BTreeMap<String, TriggerSpec>,
    /// The least time between two accepted deliveries to one webhook
    /// trigger (`triggers.webhook_min_interval`); 0 is no limit.
    pub webhook_min_interval_ms: u64,
    /// Where the lock that says which process schedules this directory's
    /// triggers lives (`<data>/lock`). `None` is a service alone on its
    /// database (a test, an embedder): it schedules, and asks nobody.
    pub lock_dir: Option<PathBuf>,
}

impl TriggerSettings {
    pub const DEFAULT_CONCURRENCY: usize = 1;
    pub const DEFAULT_MAX_WATCHED: usize = 10_000;
    /// One delivery a second per webhook trigger unless configured.
    pub const DEFAULT_WEBHOOK_MIN_INTERVAL_MS: u64 = 1_000;
}

/// The longest the loop sleeps without looking at the clock again, so a
/// suspended machine or a changed clock is noticed.
const MAX_SLEEP_MS: u64 = 15_000;
/// A stream of changes fires a watch after this many debounce periods even
/// if it never goes quiet.
const MAX_DEBOUNCE_PERIODS: u64 = 10;
/// How many changed paths a watch run is told about.
const MAX_WATCH_PATHS: usize = 50;
/// The longest final answer kept on a run.
const MAX_RUN_TEXT: usize = 64 * 1024;
/// Finished one-shot triggers are dropped after this long.
const KEEP_FINISHED_MS: u64 = 24 * 60 * 60 * 1000;
/// How often a follower asks whether the scheduler's place is free.
pub const FOLLOWER_RETRY_MS: u64 = 5_000;
/// How a run whose turn panicked starts its error.
pub const RUN_PANICKED: &str = "the run panicked";
/// Why a queued run did not start: the server was shutting down by the
/// time a slot was free.
pub const STOPPED_BEFORE_START: &str = "the server was shutting down before this run's turn came";
/// Why a fire did not run when the trigger's run is another process's.
const BUSY_ELSEWHERE: &str =
    "the previous run has not finished (another server sharing this data directory is running it)";

/// The file whose lock says which process schedules `directory`'s
/// triggers: one per directory, since each process schedules the
/// directories it has open.
pub fn scheduler_lock(lock_dir: &std::path::Path, directory: &std::path::Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(directory.to_string_lossy().as_bytes());
    lock_dir.join(format!("scheduler-{}.lock", &hex::encode(digest)[..16]))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Due {
    at: u64,
    /// The tick being answered (differs from `at` for a caught-up tick).
    scheduled: u64,
    reason: &'static str,
}

#[derive(Debug)]
struct PendingWatch {
    first: u64,
    deadline: u64,
    paths: BTreeSet<String>,
}

/// Who a trigger is, for the question of which session it may name.
struct Named<'a> {
    origin_session_id: Option<&'a str>,
    inherited: Option<&'a CreatorBounds>,
    outside_content: bool,
    restricted: bool,
}

struct Entry {
    source: TriggerSource,
    spec: TriggerSpec,
    inherited: Option<CreatorBounds>,
    origin_session_id: Option<String>,
    /// Its creator was restricted (`StoredSpec::restricted`).
    restricted: bool,
    /// `StoredSpec::outside_content`.
    outside_content: bool,
    /// `StoredSpec::outside_start`.
    outside_start: bool,
    schedule: Result<Schedule, String>,
    state: TriggerState,
    time_created: u64,
    time_updated: u64,
    next: Option<Due>,
    busy: bool,
    pending: Option<PendingWatch>,
    watcher: Option<WatchHandle>,
    watch_error: Option<String>,
}

impl Entry {
    fn enabled(&self) -> bool {
        self.spec.enabled.unwrap_or(true) && self.state.paused != Some(true)
    }

    fn record(&self, id: &str) -> TriggerRecord {
        TriggerRecord {
            id: id.to_string(),
            source: self.source,
            spec: self.spec.clone(),
            enabled: self.enabled(),
            session_id: self.state.session_id.clone(),
            origin_session_id: self.origin_session_id.clone(),
            restricted: self.restricted,
            outside_content: self.outside_content,
            time_created: self.time_created,
            time_updated: self.time_updated,
            last_fired: self.state.last_fired,
            next: self.next.filter(|_| self.enabled()).map(|due| due.at),
            running: self.busy,
            error: self
                .schedule
                .as_ref()
                .err()
                .cloned()
                .or_else(|| self.watch_error.clone()),
        }
    }
}

/// Plan the next occurrence. At startup a missed tick may be caught up;
/// otherwise the next occurrence is always in the future, so a machine that
/// slept through ten ticks fires once, not ten times.
fn plan(
    schedule: &Schedule,
    spec: &TriggerSpec,
    state: &TriggerState,
    now: u64,
    startup: bool,
) -> Option<Due> {
    let base = state.last_scheduled.unwrap_or(state.time_seen);
    let catch_up = startup && spec.catch_up == Some(true);
    match schedule {
        Schedule::Cron(cron) => {
            if catch_up {
                if let Some(missed) = cron.previous_at_or_before(now).filter(|at| *at > base) {
                    return Some(Due {
                        at: now,
                        scheduled: missed,
                        reason: "catch_up",
                    });
                }
            }
            cron.next_after(now).map(|at| Due {
                at,
                scheduled: at,
                reason: "schedule",
            })
        }
        Schedule::Interval(every) => {
            let due = base.saturating_add(*every);
            if due > now {
                return Some(Due {
                    at: due,
                    scheduled: due,
                    reason: "schedule",
                });
            }
            if catch_up {
                return Some(Due {
                    at: now,
                    scheduled: due,
                    reason: "catch_up",
                });
            }
            let at = now.saturating_add(*every);
            Some(Due {
                at,
                scheduled: at,
                reason: "schedule",
            })
        }
        Schedule::At(at) => {
            if state.last_fired.is_some() || state.last_scheduled.is_some() {
                return None;
            }
            if *at > now {
                return Some(Due {
                    at: *at,
                    scheduled: *at,
                    reason: "schedule",
                });
            }
            // A reminder missed while the server was down is still wanted,
            // unless the trigger says otherwise.
            if startup && spec.catch_up == Some(false) {
                return None;
            }
            Some(Due {
                at: now,
                scheduled: *at,
                reason: if startup { "catch_up" } else { "schedule" },
            })
        }
        Schedule::Watch { .. } | Schedule::Webhook => None,
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !id.starts_with('.')
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", ulid::Ulid::new().to_string().to_lowercase())
}

pub struct TriggerService {
    ops: Arc<dyn TriggerOps>,
    events: Arc<EventBus>,
    store: TriggerStore,
    clock: Arc<dyn Clock>,
    settings: TriggerSettings,
    ignore: Arc<WatchIgnore>,
    entries: Mutex<BTreeMap<String, Entry>>,
    slots: Arc<tokio::sync::Semaphore>,
    wake: tokio::sync::Notify,
    settled: tokio::sync::Notify,
    started: AtomicBool,
    /// [`Self::start`] was called: a scheduler is wanted here, and one that
    /// was waiting for a first trigger starts when it is created.
    start_requested: AtomicBool,
    /// Triggers are in use here ([`Self::active`]). Set, never cleared: a
    /// directory whose last trigger is deleted keeps its scheduler and its
    /// tool until the instance is rebuilt.
    activated: AtomicBool,
    /// Set by [`TriggerService::stop`]: nothing fires any more.
    stopped: AtomicBool,
    /// When each webhook trigger last accepted a delivery.
    hook_times: Mutex<BTreeMap<String, u64>>,
    /// The scheduler's place for this directory, among the processes that
    /// share the data directory. `None`: there is nobody to share with.
    role: Option<RoleLock>,
    /// This process asked for the place and another had it.
    follower: AtomicBool,
    /// Runs that passed the shutdown check and whose turn has not ended.
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
}

/// Counts a run as in flight until its turn has ended, however it ends.
struct InFlight(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl TriggerService {
    /// Load the config's triggers and the stored ones, and plan each. Nothing
    /// runs until [`Self::start`] (or a test calls [`Self::tick`]).
    pub fn new(
        ops: Arc<dyn TriggerOps>,
        events: Arc<EventBus>,
        storage: Arc<crate::storage::Storage>,
        clock: Arc<dyn Clock>,
        settings: TriggerSettings,
    ) -> Arc<TriggerService> {
        let store = TriggerStore::new(storage, &settings.directory, &settings.project_id);
        let now = clock.now_ms();
        // Only what a process that is gone left: a live one's runs are its.
        if let Err(error) = store.fail_orphaned(now) {
            tracing::warn!("failed to close unfinished trigger runs: {error}");
        }
        // A config read after startup (an instance boot) can declare a
        // trigger that outside input starts: the posture follows.
        if settings.enabled
            && settings
                .config
                .values()
                .any(|spec| spec.starts_from_outside())
        {
            crate::trigger::store::tighten_for_outside_trigger();
        }
        let mut entries = BTreeMap::new();
        for (id, spec) in &settings.config {
            if !valid_id(id) {
                tracing::warn!("ignoring trigger \"{id}\": ids use letters, digits, - _ and .");
                continue;
            }
            entries.insert(
                id.clone(),
                Self::load_entry(
                    &store,
                    id,
                    TriggerSource::Config,
                    spec.clone(),
                    None,
                    None,
                    false,
                    (false, false),
                    now,
                    now,
                    now,
                ),
            );
        }
        for stored in store.list().unwrap_or_else(|error| {
            tracing::warn!("failed to load stored triggers: {error}");
            Vec::new()
        }) {
            if entries.contains_key(&stored.id) {
                tracing::warn!(
                    "stored trigger \"{}\" is shadowed by the config's trigger of the same name",
                    stored.id
                );
                continue;
            }
            let entry = Self::load_entry(
                &store,
                &stored.id,
                TriggerSource::Runtime,
                stored.stored.spec,
                stored.stored.inherited,
                stored.origin_session_id,
                stored.stored.restricted,
                (stored.stored.outside_content, stored.stored.outside_start),
                stored.time_created,
                stored.time_updated,
                now,
            );
            entries.insert(stored.id, entry);
        }
        let ignore = Arc::new(WatchIgnore::new(settings.ignore.clone()));
        let slots = Arc::new(tokio::sync::Semaphore::new(settings.concurrency.max(1)));
        let role = settings
            .lock_dir
            .as_ref()
            .map(|dir| RoleLock::new(scheduler_lock(dir, &settings.directory)));
        let activated = settings.enabled && (!settings.on_demand || !entries.is_empty());
        Arc::new(TriggerService {
            start_requested: AtomicBool::new(false),
            activated: AtomicBool::new(activated),
            role,
            follower: AtomicBool::new(false),
            in_flight: Arc::default(),
            ops,
            events,
            store,
            clock,
            settings,
            ignore,
            entries: Mutex::new(entries),
            slots,
            wake: tokio::sync::Notify::new(),
            settled: tokio::sync::Notify::new(),
            hook_times: Mutex::new(BTreeMap::new()),
            started: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn load_entry(
        store: &TriggerStore,
        id: &str,
        source: TriggerSource,
        spec: TriggerSpec,
        inherited: Option<CreatorBounds>,
        origin_session_id: Option<String>,
        restricted: bool,
        (outside_content, outside_start): (bool, bool),
        time_created: u64,
        time_updated: u64,
        now: u64,
    ) -> Entry {
        let known = store.state(id).ok().flatten();
        let first_sight = known.is_none();
        let state = known.unwrap_or(TriggerState {
            time_seen: now,
            ..Default::default()
        });
        if first_sight {
            let _ = store.put_state(id, &state);
        }
        let schedule = spec.schedule();
        let next = schedule
            .as_ref()
            .ok()
            .and_then(|schedule| plan(schedule, &spec, &state, now, !first_sight));
        if let Err(error) = &schedule {
            tracing::warn!("trigger \"{id}\" cannot run: {error}");
        }
        Entry {
            source,
            spec,
            inherited,
            origin_session_id,
            restricted,
            outside_content,
            outside_start,
            schedule,
            state,
            time_created: if source == TriggerSource::Config {
                state_time(store, id, time_created)
            } else {
                time_created
            },
            time_updated,
            next,
            busy: false,
            pending: None,
            watcher: None,
            watch_error: None,
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Triggers are not turned off: requests are accepted.
    pub fn enabled(&self) -> bool {
        self.settings.enabled
    }

    /// Triggers are in use here: `triggers.enabled` is `true`, or it is not
    /// written and the directory has (or has had, since this instance was
    /// built) a trigger. What the scheduler and the `schedule` tool wait
    /// for (`TriggerSettings::on_demand`).
    pub fn active(&self) -> bool {
        self.settings.enabled && self.activated.load(Ordering::SeqCst)
    }

    /// A trigger exists from now on: what was waiting for one starts.
    fn activate(self: &Arc<Self>) {
        if !self.settings.enabled || self.activated.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.start_requested.load(Ordering::SeqCst) {
            self.start();
        }
    }

    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    fn require_enabled(&self) -> Result<(), TriggerError> {
        if self.settings.enabled {
            Ok(())
        } else {
            Err(TriggerError::Disabled)
        }
    }

    // -- Leader and follower ---------------------------------------------

    /// Whether this process holds the scheduler's place for the directory,
    /// without asking for it.
    fn scheduling(&self) -> bool {
        self.role.as_ref().is_none_or(RoleLock::held)
    }

    /// Whether another process schedules this directory's triggers: this
    /// one asked for the place and did not get it.
    pub fn is_follower(&self) -> bool {
        self.follower.load(Ordering::SeqCst) && !self.scheduling()
    }

    /// Within one process a directory's instance can be booted again (a
    /// config change disposes it): the service of the instance that was
    /// replaced still holds the scheduler's place, and would keep the new
    /// one a follower for as long as the process lives. The new one stops
    /// it — nothing fires from the old instance any more; the turns it has
    /// running finish — and registers itself as the one to stop next time.
    fn supersede(self: &Arc<Self>) {
        static SCHEDULERS: Mutex<BTreeMap<PathBuf, std::sync::Weak<TriggerService>>> =
            Mutex::new(BTreeMap::new());
        let Some(role) = &self.role else {
            return;
        };
        let previous = SCHEDULERS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(role.path().to_path_buf(), Arc::downgrade(self));
        if let Some(previous) = previous.and_then(|previous| previous.upgrade()) {
            if !Arc::ptr_eq(&previous, self) {
                previous.stop();
            }
        }
    }

    /// Whether this process is the directory's scheduler, taking the place
    /// if it is free. A follower asks again on every pass of its loop, so
    /// it takes over within [`FOLLOWER_RETRY_MS`] of the scheduler's exit.
    fn leads(self: &Arc<Self>, now: u64) -> bool {
        let Some(role) = &self.role else {
            return true;
        };
        if role.held() {
            return true;
        }
        if self.stopped() {
            return false;
        }
        if !role.acquire() {
            if !self.follower.swap(true, Ordering::SeqCst) {
                tracing::info!(
                    "another process schedules the triggers of {}: this one is a follower. It \
                     runs what it is asked to (a webhook call, a manual run) and takes over \
                     when that process exits",
                    self.settings.directory.display()
                );
            }
            return false;
        }
        if self.follower.load(Ordering::SeqCst) {
            self.take_over(now);
        }
        true
    }

    /// The scheduler exited and this process has its place: close what it
    /// left unfinished, and plan from what it wrote down, as at a start.
    fn take_over(self: &Arc<Self>, now: u64) {
        tracing::info!(
            "the process that scheduled the triggers of {} is gone: this one schedules them now",
            self.settings.directory.display()
        );
        if let Err(error) = self.store.fail_orphaned(now) {
            tracing::warn!("failed to close unfinished trigger runs: {error}");
        }
        self.sync_stored(now);
        let ids: Vec<String> = {
            let mut entries = self.lock();
            for (id, entry) in entries.iter_mut() {
                if let Ok(Some(theirs)) = self.store.state(id) {
                    entry.state = theirs;
                }
                entry.next = entry
                    .schedule
                    .as_ref()
                    .ok()
                    .and_then(|schedule| plan(schedule, &entry.spec, &entry.state, now, true));
            }
            entries.keys().cloned().collect()
        };
        for id in ids {
            self.sync_watch(&id);
        }
    }

    /// Bring this process's picture of the directory's triggers up to what
    /// is stored: with several processes on one database, a trigger is
    /// created, changed, paused or deleted through any of them, and the
    /// scheduler is one of them. Nothing changes when this process wrote
    /// everything itself.
    fn sync_stored(self: &Arc<Self>, now: u64) {
        if self.role.is_none() {
            return;
        }
        let mut rewatch: Vec<String> = Vec::new();
        {
            // Read under the lock every write is made under, so a trigger
            // this process is creating is not taken for one deleted.
            let mut entries = self.lock();
            let (Ok(stored), Ok(states)) = (self.store.list(), self.store.states()) else {
                return;
            };
            let states: BTreeMap<String, TriggerState> = states.into_iter().collect();
            let gone: Vec<String> = entries
                .iter()
                .filter(|(id, entry)| {
                    entry.source == TriggerSource::Runtime
                        && !entry.busy
                        && !stored.iter().any(|trigger| trigger.id == **id)
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in gone {
                entries.remove(&id);
            }
            for trigger in stored {
                match entries.get_mut(&trigger.id) {
                    None => {
                        let entry = Self::load_entry(
                            &self.store,
                            &trigger.id,
                            TriggerSource::Runtime,
                            trigger.stored.spec,
                            trigger.stored.inherited,
                            trigger.origin_session_id,
                            trigger.stored.restricted,
                            (trigger.stored.outside_content, trigger.stored.outside_start),
                            trigger.time_created,
                            trigger.time_updated,
                            now,
                        );
                        rewatch.push(trigger.id.clone());
                        entries.insert(trigger.id, entry);
                    }
                    Some(entry) if entry.source == TriggerSource::Runtime => {
                        entry.outside_content |= trigger.stored.outside_content;
                        entry.outside_start |= trigger.stored.outside_start;
                        if entry.time_updated == trigger.time_updated {
                            continue;
                        }
                        let rescheduled = entry.spec.on != trigger.stored.spec.on;
                        entry.schedule = trigger.stored.spec.schedule();
                        entry.spec = trigger.stored.spec;
                        entry.inherited = trigger.stored.inherited;
                        entry.time_updated = trigger.time_updated;
                        if rescheduled {
                            entry.pending = None;
                            if let Some(theirs) = states.get(&trigger.id) {
                                entry.state = theirs.clone();
                            }
                            entry.next = entry.schedule.as_ref().ok().and_then(|schedule| {
                                plan(schedule, &entry.spec, &entry.state, now, false)
                            });
                        }
                        rewatch.push(trigger.id);
                    }
                    // The config's trigger of the same name shadows it.
                    Some(_) => {}
                }
            }
            for (id, entry) in entries.iter_mut() {
                let Some(theirs) = states.get(id) else {
                    continue;
                };
                let was = entry.enabled();
                entry.state.paused = theirs.paused;
                if !entry.busy {
                    entry.state.session_id = theirs.session_id.clone();
                    entry.state.last_fired = theirs.last_fired;
                }
                if entry.enabled() != was {
                    if entry.enabled() {
                        entry.state.last_scheduled = theirs.last_scheduled;
                        entry.next = entry.schedule.as_ref().ok().and_then(|schedule| {
                            plan(schedule, &entry.spec, &entry.state, now, false)
                        });
                    }
                    rewatch.push(id.clone());
                }
            }
        }
        for id in rewatch {
            self.sync_watch(&id);
        }
    }

    fn publish(&self, definition: &crate::event::definition::Definition, data: Value) {
        let _ = self
            .events
            .publish(definition, data, PublishOptions::default());
    }

    fn changed(&self, id: &str) {
        self.publish(&TRIGGER_UPDATED, json!({ "triggerID": id }));
        self.wake.notify_one();
    }

    // -- Reading ----------------------------------------------------------

    pub fn list(&self) -> Vec<TriggerRecord> {
        self.lock()
            .iter()
            .map(|(id, entry)| entry.record(id))
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<TriggerRecord> {
        self.lock().get(id).map(|entry| entry.record(id))
    }

    /// A trigger's runs, newest first.
    pub fn runs(&self, id: &str, limit: usize) -> Result<Vec<TriggerRun>, TriggerError> {
        if !self.lock().contains_key(id) {
            return Err(TriggerError::NotFound(format!("trigger not found: {id}")));
        }
        self.store
            .runs(id, limit.clamp(1, crate::trigger::MAX_RUNS_KEPT))
            .map_err(|error| TriggerError::Failed(error.to_string()))
    }

    pub fn run(&self, run_id: &str) -> Option<TriggerRun> {
        self.store.run(run_id).ok().flatten()
    }

    // -- Writing ----------------------------------------------------------

    /// Create a stored trigger on the operator's behalf. `inherited` is the
    /// permission ruleset of the session that asked for it, so a trigger an
    /// agent creates can do no more than the agent could.
    pub fn create(
        self: &Arc<Self>,
        id: Option<String>,
        spec: TriggerSpec,
        origin_session_id: Option<String>,
        inherited: Option<PermissionV1Ruleset>,
    ) -> Result<TriggerRecord, TriggerError> {
        self.create_as(
            id,
            spec,
            Creator {
                origin_session_id,
                inherited: inherited.map(CreatorBounds::Flat),
                restricted: false,
                outside_content: false,
                outside_start: false,
            },
        )
    }

    /// A session a trigger names must be one the trigger may name
    /// (`rules::check_named_session`): of this project, and one whose own
    /// policy and marks are the right ones for the run.
    fn check_target(&self, spec: &TriggerSpec, who: &Named<'_>) -> Result<(), TriggerError> {
        let SessionMode::Existing(session_id) = spec.session_mode() else {
            return Ok(());
        };
        crate::trigger::rules::check_named_session(&crate::trigger::rules::NamedSession {
            session_id: &session_id,
            target: self.ops.target(&session_id),
            origin_session_id: who.origin_session_id,
            bounded: who.inherited.is_some_and(|bounds| !bounds.is_empty()),
            outside_start: spec.on.webhook == Some(true) || spec.on.watch.is_some(),
            outside_content: who.outside_content,
            restricted: who.restricted,
        })
        .map_err(|refusal| match refusal {
            crate::trigger::rules::NamedSessionRefusal::Invalid(message) => {
                TriggerError::Invalid(message)
            }
            crate::trigger::rules::NamedSessionRefusal::Forbidden(message) => {
                TriggerError::Forbidden(message)
            }
        })
    }

    /// Whether a caller that is, or is not, restricted may change, fire or
    /// delete trigger `id`. A restricted caller acts on restricted
    /// triggers only — its own kind, as with sessions: the config's and
    /// the operator's are the operator's to change, and firing one would
    /// start a turn with the operator's rights. An unknown id is left to
    /// the operation's own answer.
    pub fn check_actor(&self, id: &str, actor_restricted: bool) -> Result<(), TriggerError> {
        if !actor_restricted {
            return Ok(());
        }
        match self.lock().get(id) {
            Some(entry) if !entry.restricted => Err(TriggerError::Forbidden(format!(
                "trigger \"{id}\" belongs to the operator: this caller can read it, \
                 but not change or fire it"
            ))),
            _ => Ok(()),
        }
    }

    /// Whether a session's `schedule` tool may leave the outside-content
    /// mark on trigger `id` — which it must before it changes it. A
    /// trigger that runs in a named session can only carry the mark if
    /// that session has it (`rules::check_named_session`).
    fn check_mark(&self, entry: &Entry) -> Result<(), TriggerError> {
        self.check_target(
            &entry.spec,
            &Named {
                origin_session_id: entry.origin_session_id.as_deref(),
                inherited: entry.inherited.as_ref(),
                outside_content: true,
                restricted: entry.restricted,
            },
        )
    }

    /// Record that a session which had seen outside content changed a
    /// stored trigger (`StoredSpec::outside_content`): from now on its
    /// runs' sessions carry that mark. Never cleared.
    pub fn mark_outside_content(&self, id: &str) -> Result<(), TriggerError> {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(id) else {
            return Err(TriggerError::NotFound(format!("trigger not found: {id}")));
        };
        if entry.outside_content || entry.source != TriggerSource::Runtime {
            return Ok(());
        }
        self.check_mark(entry)?;
        self.store
            .put(&StoredTrigger {
                id: id.to_string(),
                stored: StoredSpec {
                    spec: entry.spec.clone(),
                    inherited: entry.inherited.clone(),
                    restricted: entry.restricted,
                    outside_content: true,
                    outside_start: entry.outside_start,
                },
                origin_session_id: entry.origin_session_id.clone(),
                time_created: entry.time_created,
                time_updated: entry.time_updated,
            })
            .map_err(|error| TriggerError::Failed(error.to_string()))?;
        entry.outside_content = true;
        Ok(())
    }

    /// Record that a session outside input had started changed a stored
    /// trigger (`StoredSpec::outside_start`): from now on its runs in a
    /// session of their own are started by outside input. Never cleared.
    pub fn mark_outside_start(&self, id: &str) -> Result<(), TriggerError> {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(id) else {
            return Err(TriggerError::NotFound(format!("trigger not found: {id}")));
        };
        if entry.outside_start || entry.source != TriggerSource::Runtime {
            return Ok(());
        }
        self.store
            .put(&StoredTrigger {
                id: id.to_string(),
                stored: StoredSpec {
                    spec: entry.spec.clone(),
                    inherited: entry.inherited.clone(),
                    restricted: entry.restricted,
                    outside_content: entry.outside_content,
                    outside_start: true,
                },
                origin_session_id: entry.origin_session_id.clone(),
                time_created: entry.time_created,
                time_updated: entry.time_updated,
            })
            .map_err(|error| TriggerError::Failed(error.to_string()))?;
        entry.outside_start = true;
        Ok(())
    }

    /// Create a stored trigger for `creator`. What the creator was is
    /// recorded with it and binds every run (`StoredSpec::restricted`).
    pub fn create_as(
        self: &Arc<Self>,
        id: Option<String>,
        spec: TriggerSpec,
        creator: Creator,
    ) -> Result<TriggerRecord, TriggerError> {
        let Creator {
            origin_session_id,
            inherited,
            restricted,
            outside_content,
            outside_start,
        } = creator;
        self.require_enabled()?;
        let schedule = spec.schedule().map_err(TriggerError::Invalid)?;
        self.check_target(
            &spec,
            &Named {
                origin_session_id: origin_session_id.as_deref(),
                inherited: inherited.as_ref(),
                outside_content,
                restricted,
            },
        )?;
        let id = match id {
            Some(id) if valid_id(&id) => id,
            Some(id) => {
                return Err(TriggerError::Invalid(format!(
                    "\"{id}\" is not a valid trigger id: use up to 64 letters, digits, - _ or ."
                )))
            }
            None => new_id("trg"),
        };
        let now = self.clock.now_ms();
        self.prune_finished(now);
        if let Schedule::Watch { path, .. } = &schedule {
            self.watch_path(path, TriggerSource::Runtime)
                .map_err(TriggerError::Invalid)?;
        }
        let record = {
            let mut entries = self.lock();
            if entries.contains_key(&id) {
                return Err(TriggerError::Conflict(format!(
                    "a trigger named \"{id}\" already exists"
                )));
            }
            let stored = entries
                .values()
                .filter(|entry| entry.source == TriggerSource::Runtime)
                .count();
            // Triggers nobody with the operator's rights wrote: an agent's,
            // or a restricted caller's.
            let by_agents = entries
                .values()
                .filter(|entry| entry.origin_session_id.is_some() || entry.restricted)
                .count();
            if (origin_session_id.is_some() || restricted) && by_agents >= MAX_AGENT_TRIGGERS {
                return Err(TriggerError::Conflict(format!(
                    "agents have already scheduled {MAX_AGENT_TRIGGERS} triggers here; delete some first"
                )));
            }
            if stored >= MAX_STORED_TRIGGERS {
                return Err(TriggerError::Conflict(format!(
                    "this directory already has {MAX_STORED_TRIGGERS} stored triggers; delete some first"
                )));
            }
            let state = TriggerState {
                time_seen: now,
                ..Default::default()
            };
            self.store
                .put(&StoredTrigger {
                    id: id.clone(),
                    stored: StoredSpec {
                        spec: spec.clone(),
                        inherited: inherited.clone(),
                        restricted,
                        outside_content,
                        outside_start,
                    },
                    origin_session_id: origin_session_id.clone(),
                    time_created: now,
                    time_updated: now,
                })
                .and_then(|()| self.store.put_state(&id, &state))
                .map_err(|error| TriggerError::Failed(error.to_string()))?;
            let next = plan(&schedule, &spec, &state, now, false);
            let entry = Entry {
                source: TriggerSource::Runtime,
                spec,
                inherited,
                origin_session_id,
                restricted,
                outside_content,
                outside_start,
                schedule: Ok(schedule),
                state,
                time_created: now,
                time_updated: now,
                next,
                busy: false,
                pending: None,
                watcher: None,
                watch_error: None,
            };
            let record = entry.record(&id);
            entries.insert(id.clone(), entry);
            record
        };
        // The first trigger of a directory that had none starts its
        // scheduler (`TriggerSettings::on_demand`); the watch below needs
        // it to have taken the scheduler's place.
        self.activate();
        self.sync_watch(&id);
        self.changed(&id);
        Ok(self.get(&id).unwrap_or(record))
    }

    /// Change a trigger. `patch` is a JSON merge patch over the spec (`null`
    /// removes a key). A config trigger only takes `enabled`: the rest of it
    /// is the config's to say.
    pub fn update(
        self: &Arc<Self>,
        id: &str,
        patch: &Value,
    ) -> Result<TriggerRecord, TriggerError> {
        self.require_enabled()?;
        let Some(patch_object) = patch.as_object() else {
            return Err(TriggerError::Invalid(
                "the body must be a JSON object".to_string(),
            ));
        };
        let now = self.clock.now_ms();
        {
            let mut entries = self.lock();
            let Some(entry) = entries.get_mut(id) else {
                return Err(TriggerError::NotFound(format!("trigger not found: {id}")));
            };
            if entry.source == TriggerSource::Config {
                let enabled = match (patch_object.len(), patch_object.get("enabled")) {
                    (1, Some(Value::Bool(enabled))) => *enabled,
                    _ => {
                        return Err(TriggerError::Conflict(format!(
                            "trigger \"{id}\" is defined in the config: edit it there. \
                             Only `enabled` can be changed here, to pause or resume it."
                        )))
                    }
                };
                let was = entry.enabled();
                entry.state.paused = Some(!enabled);
                if entry.enabled() && !was {
                    Self::replan(entry, now);
                }
                entry.time_updated = now;
                self.store
                    .put_state(id, &entry.state)
                    .map_err(|error| TriggerError::Failed(error.to_string()))?;
            } else {
                let mut value = serde_json::to_value(&entry.spec)
                    .map_err(|error| TriggerError::Failed(error.to_string()))?;
                crate::merge::merge_patch(&mut value, patch);
                // `on` holds exactly one kind, so a new one replaces the old
                // one instead of joining it.
                if let Some(on @ Value::Object(_)) = patch_object.get("on") {
                    value["on"] = crate::merge::strip_nulls(on);
                }
                let spec: TriggerSpec = serde_json::from_value(value)
                    .map_err(|error| TriggerError::Invalid(error.to_string()))?;
                let schedule = spec.schedule().map_err(TriggerError::Invalid)?;
                if let Schedule::Watch { path, .. } = &schedule {
                    self.watch_path(path, TriggerSource::Runtime)
                        .map_err(TriggerError::Invalid)?;
                }
                // Whoever changes it, the trigger stays its creator's: a
                // new target, or a new kind for the old one, is held to
                // what its creator may name.
                if spec.session != entry.spec.session || spec.on != entry.spec.on {
                    self.check_target(
                        &spec,
                        &Named {
                            origin_session_id: entry.origin_session_id.as_deref(),
                            inherited: entry.inherited.as_ref(),
                            outside_content: entry.outside_content,
                            restricted: entry.restricted,
                        },
                    )?;
                }
                let was = entry.enabled();
                let rescheduled = entry.spec.on != spec.on;
                // A different kind of trigger is not the same conversation:
                // the session a reuse trigger kept is let go, and the next
                // run starts one of its own. Otherwise a webhook trigger
                // turned into a cron one went on, trusted, in the session
                // that holds the outside text — and a cron trigger turned
                // into a webhook one fed outside text to a session with the
                // trusted run's rules.
                let kind_changed = entry
                    .schedule
                    .as_ref()
                    .map_or(true, |before| before.kind() != schedule.kind());
                if kind_changed {
                    entry.state.session_id = None;
                }
                entry.spec = spec;
                entry.schedule = Ok(schedule);
                // Resuming through `enabled: true` also lifts a pause.
                if patch_object.get("enabled") == Some(&Value::Bool(true)) {
                    entry.state.paused = None;
                }
                if rescheduled {
                    entry.state.last_scheduled = None;
                    entry.state.last_fired = None;
                    entry.state.time_seen = now;
                    entry.pending = None;
                }
                if rescheduled || (entry.enabled() && !was) {
                    Self::replan(entry, now);
                }
                entry.time_updated = now;
                self.store
                    .put(&StoredTrigger {
                        id: id.to_string(),
                        stored: StoredSpec {
                            spec: entry.spec.clone(),
                            inherited: entry.inherited.clone(),
                            restricted: entry.restricted,
                            outside_content: entry.outside_content,
                            outside_start: entry.outside_start,
                        },
                        origin_session_id: entry.origin_session_id.clone(),
                        time_created: entry.time_created,
                        time_updated: now,
                    })
                    .and_then(|()| self.store.put_state(id, &entry.state))
                    .map_err(|error| TriggerError::Failed(error.to_string()))?;
            }
        }
        self.sync_watch(id);
        self.changed(id);
        self.get(id)
            .ok_or_else(|| TriggerError::NotFound(format!("trigger not found: {id}")))
    }

    /// A trigger that comes back from a pause, or whose `on` changed, starts
    /// counting from now.
    fn replan(entry: &mut Entry, now: u64) {
        if matches!(entry.schedule, Ok(Schedule::Interval(_))) {
            entry.state.last_scheduled = Some(now);
        }
        entry.next = entry
            .schedule
            .as_ref()
            .ok()
            .and_then(|schedule| plan(schedule, &entry.spec, &entry.state, now, false));
    }

    pub fn delete(self: &Arc<Self>, id: &str) -> Result<bool, TriggerError> {
        self.require_enabled()?;
        {
            let mut entries = self.lock();
            match entries.get(id) {
                None => return Ok(false),
                Some(entry) if entry.source == TriggerSource::Config => {
                    return Err(TriggerError::Conflict(format!(
                        "trigger \"{id}\" is defined in the config: remove it there, \
                         or pause it with `enabled: false`"
                    )));
                }
                Some(_) => {}
            }
            self.store
                .delete(id)
                .map_err(|error| TriggerError::Failed(error.to_string()))?;
            entries.remove(id);
        }
        self.changed(id);
        Ok(true)
    }

    /// Drop stored one-shot triggers that fired more than a day ago.
    fn prune_finished(&self, now: u64) {
        let mut entries = self.lock();
        let finished: Vec<String> = entries
            .iter()
            .filter(|(_, entry)| {
                entry.source == TriggerSource::Runtime
                    && !entry.busy
                    && matches!(entry.schedule, Ok(Schedule::At(_)))
                    && entry
                        .state
                        .last_fired
                        .is_some_and(|fired| fired.saturating_add(KEEP_FINISHED_MS) < now)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in finished {
            if self.store.delete(&id).is_ok() {
                entries.remove(&id);
            }
        }
    }

    // -- Watching ---------------------------------------------------------

    /// Where a `watch` path points. A stored trigger stays inside the
    /// directory; the config may name any path.
    fn watch_path(&self, path: &str, source: TriggerSource) -> Result<PathBuf, String> {
        let expanded = match path.strip_prefix("~/") {
            Some(rest) => self.settings.home.join(rest),
            None => PathBuf::from(path),
        };
        // Resolved as the kernel resolves the spelling
        // (`protect::resolve_path`): `project/link` is wherever `link`
        // leads, and `project/link/../x` is beside that, not `project/x`.
        let spelled = self.settings.directory.join(expanded);
        if source != TriggerSource::Runtime {
            // The operator's own config: no boundary to keep, so a path
            // that cannot be resolved yet keeps its spelling.
            return Ok(crate::protect::resolve(&spelled));
        }
        let outside = || {
            format!(
                "watch path \"{path}\" is outside {}; only your own config may watch other places",
                self.settings.directory.display()
            )
        };
        let resolved = crate::protect::resolve_path(&spelled).map_err(|_| outside())?;
        let root = crate::protect::resolve(&self.settings.directory);
        if !crate::protect::within(&resolved, &root) {
            return Err(outside());
        }
        Ok(resolved)
    }

    /// Make a trigger's watcher match its spec.
    fn sync_watch(self: &Arc<Self>, id: &str) {
        // A follower watches nothing: the scheduler does, and a change
        // would otherwise fire once in each process.
        if !self.started.load(Ordering::SeqCst) || self.stopped() || !self.scheduling() {
            return;
        }
        let target = {
            let mut entries = self.lock();
            let Some(entry) = entries.get_mut(id) else {
                return;
            };
            entry.watcher = None;
            entry.watch_error = None;
            match (&entry.schedule, entry.enabled()) {
                (Ok(Schedule::Watch { path, .. }), true) => Some((path.clone(), entry.source)),
                _ => None,
            }
        };
        let Some((path, source)) = target else {
            return;
        };
        let weak = Arc::downgrade(self);
        let owner = id.to_string();
        let result = self.watch_path(&path, source).and_then(|resolved| {
            watch::watch(
                &resolved,
                (source == TriggerSource::Runtime).then_some(self.settings.directory.as_path()),
                self.ignore.clone(),
                self.settings.max_watched,
                move |paths| {
                    if let Some(service) = weak.upgrade() {
                        service.path_changed(&owner, &paths);
                    }
                },
            )
        });
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(id) else {
            return;
        };
        match result {
            Ok(handle) => entry.watcher = Some(handle),
            Err(error) => {
                tracing::warn!("trigger \"{id}\" is not watching: {error}");
                entry.watch_error = Some(error);
            }
        }
    }

    /// Something under a watched path changed: (re)start the debounce.
    pub fn path_changed(&self, id: &str, paths: &[PathBuf]) {
        if !self.scheduling() {
            return;
        }
        let now = self.clock.now_ms();
        {
            let mut entries = self.lock();
            let Some(entry) = entries.get_mut(id) else {
                return;
            };
            let Ok(Schedule::Watch { debounce_ms, .. }) = &entry.schedule else {
                return;
            };
            if !entry.enabled() {
                return;
            }
            let debounce = *debounce_ms;
            let pending = entry.pending.get_or_insert_with(|| PendingWatch {
                first: now,
                deadline: now,
                paths: BTreeSet::new(),
            });
            let latest = pending
                .first
                .saturating_add(debounce.max(1000).saturating_mul(MAX_DEBOUNCE_PERIODS));
            pending.deadline = now.saturating_add(debounce).min(latest);
            for path in paths {
                if pending.paths.len() >= MAX_WATCH_PATHS {
                    break;
                }
                let shown = path
                    .strip_prefix(&self.settings.directory)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned();
                pending.paths.insert(shown);
            }
        }
        self.wake.notify_one();
    }

    // -- Scheduling -------------------------------------------------------

    /// Start the watchers and the loop that calls [`Self::tick`]. Needs a
    /// tokio runtime; without one (or when triggers are disabled) nothing
    /// is scheduled. While triggers are not in use ([`Self::active`]) the
    /// call is remembered and nothing starts: no loop, no watcher, no claim
    /// on the scheduler's place. The first trigger created starts it.
    pub fn start(self: &Arc<Self>) {
        self.start_requested.store(true, Ordering::SeqCst);
        if !self.active() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("no async runtime: triggers will not fire");
            return;
        };
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        // Before the watchers: only the directory's scheduler has any.
        self.supersede();
        self.leads(self.clock.now_ms());
        let ids: Vec<String> = self.lock().keys().cloned().collect();
        for id in ids {
            self.sync_watch(&id);
        }
        let weak = Arc::downgrade(self);
        runtime.spawn(async move {
            loop {
                let Some(service) = weak.upgrade() else {
                    break;
                };
                if service.stopped.load(Ordering::SeqCst) {
                    break;
                }
                let now = service.clock.now_ms();
                service.tick(now);
                let wait = if service.scheduling() {
                    service.until_next(service.clock.now_ms())
                } else {
                    FOLLOWER_RETRY_MS
                };
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(wait)) => {}
                    _ = service.wake.notified() => {}
                }
            }
        });
    }

    /// Stop for good (a server shutting down): nothing fires from now on,
    /// by the clock, a watch, a webhook or a request, and the watchers are
    /// dropped. Runs already started are turns like any other and are
    /// settled with them.
    ///
    /// A run that is still waiting for a slot does not start
    /// ([`Self::execute`] looks again once it has one), and once this
    /// returns [`Self::in_flight`] only goes down: the runs it counts are
    /// the turns left to settle.
    pub fn stop(&self) {
        {
            // Under the lock a run takes to pass its last check, so none
            // passes it after this.
            let mut entries = self.lock();
            self.stopped.store(true, Ordering::SeqCst);
            for entry in entries.values_mut() {
                entry.watcher = None;
                entry.pending = None;
            }
        }
        // The scheduler's place is free for a process that lives on.
        if let Some(role) = &self.role {
            role.release();
        }
        self.wake.notify_one();
    }

    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// How many runs are past the shutdown check with their turn not yet
    /// ended: after [`Self::stop`], the turns a shutdown still has to
    /// settle, including one that has not reached its session yet.
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Milliseconds until something may be due.
    fn until_next(&self, now: u64) -> u64 {
        let entries = self.lock();
        let earliest = entries
            .values()
            .filter(|entry| entry.enabled())
            .flat_map(|entry| {
                entry
                    .next
                    .map(|due| due.at)
                    .into_iter()
                    .chain(entry.pending.as_ref().map(|pending| pending.deadline))
            })
            .min();
        match earliest {
            Some(at) => at.saturating_sub(now).clamp(20, MAX_SLEEP_MS),
            None => MAX_SLEEP_MS,
        }
    }

    /// Fire everything that is due at `now` and plan what comes next.
    /// Returns the runs it started or skipped.
    pub fn tick(self: &Arc<Self>, now: u64) -> Vec<TriggerRun> {
        if !self.settings.enabled || self.stopped() {
            return Vec::new();
        }
        if !self.leads(now) {
            // Still told of what the scheduler and the other followers
            // stored, so a request that reaches this process finds it.
            self.sync_stored(now);
            return Vec::new();
        }
        self.sync_stored(now);
        let mut due: Vec<(TriggerRun, Option<TickClaim>, Option<TriggerPayload>)> = Vec::new();
        {
            let mut entries = self.lock();
            for (id, entry) in entries.iter_mut() {
                if !entry.enabled() {
                    continue;
                }
                let Ok(schedule) = &entry.schedule else {
                    continue;
                };
                if let Some(next) = entry.next.filter(|next| next.at <= now) {
                    // Two servers on one database must not both run the
                    // tick, and a tick that is taken has its run on record
                    // before anything else happens.
                    let run = Self::new_run(id, next.reason, Some(next.scheduled), now);
                    match self
                        .store
                        .claim_tick(id, entry.state.last_scheduled, now, &run)
                    {
                        Ok(TickClaim::Lost) => {
                            if let Ok(Some(theirs)) = self.store.state(id) {
                                entry.state.last_scheduled = theirs.last_scheduled;
                                entry.state.last_fired = theirs.last_fired;
                                entry.state.session_id = theirs.session_id;
                            }
                        }
                        Ok(claim) => {
                            due.push((run, Some(claim), None));
                            entry.state.last_scheduled = Some(now);
                        }
                        Err(error) => {
                            tracing::warn!("trigger \"{id}\" could not claim its tick: {error}");
                            continue;
                        }
                    }
                    entry.next = plan(schedule, &entry.spec, &entry.state, now, false);
                }
                if entry
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.deadline <= now)
                {
                    let pending = entry.pending.take().expect("checked above");
                    let mut text = pending.paths.into_iter().collect::<Vec<_>>().join("\n");
                    if text.is_empty() {
                        text = "(no paths reported)".to_string();
                    }
                    due.push((
                        Self::new_run(id, "watch", None, self.clock.now_ms()),
                        None,
                        Some(TriggerPayload {
                            label: "list of changed paths".to_string(),
                            text,
                        }),
                    ));
                }
            }
        }
        due.into_iter()
            .filter_map(|(run, claim, payload)| self.launch(run, claim, payload).ok())
            .collect()
    }

    /// `POST /trigger/{id}/run` — fire now, whatever the trigger waits for.
    pub fn run_now(self: &Arc<Self>, id: &str) -> Result<TriggerRun, TriggerError> {
        self.require_enabled()?;
        self.fire(id, "manual", None)
    }

    /// `POST /trigger/{id}/hook` — the webhook entry. The body reaches the
    /// turn as fenced, untrusted data.
    pub fn hook(self: &Arc<Self>, id: &str, body: &[u8]) -> Result<TriggerRun, TriggerError> {
        self.require_enabled()?;
        {
            let entries = self.lock();
            let Some(entry) = entries.get(id) else {
                return Err(TriggerError::NotFound(format!("trigger not found: {id}")));
            };
            if !matches!(entry.schedule, Ok(Schedule::Webhook)) {
                return Err(TriggerError::Conflict(format!(
                    "trigger \"{id}\" is not a webhook trigger (its `on` has no `webhook: true`)"
                )));
            }
            if !entry.enabled() {
                return Err(TriggerError::Conflict(format!(
                    "trigger \"{id}\" is paused"
                )));
            }
        }
        if body.len() > MAX_PAYLOAD_BYTES {
            return Err(TriggerError::Invalid(format!(
                "the webhook body is larger than {MAX_PAYLOAD_BYTES} bytes"
            )));
        }
        // Per trigger, not per caller: whoever holds the credential, one
        // receiver is not started faster than this.
        let min = self.settings.webhook_min_interval_ms;
        if min > 0 {
            let now = self.clock.now_ms();
            let mut times = self.hook_times.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(last) = times.get(id) {
                let since = now.saturating_sub(*last);
                if since < min {
                    return Err(TriggerError::TooSoon {
                        retry_after_ms: min - since,
                    });
                }
            }
            times.insert(id.to_string(), now);
        }
        let text = String::from_utf8_lossy(body).trim().to_string();
        let payload = (!text.is_empty()).then(|| TriggerPayload {
            label: "webhook body".to_string(),
            text,
        });
        self.fire(id, "webhook", payload)
    }

    fn new_run(id: &str, reason: &str, scheduled: Option<u64>, now: u64) -> TriggerRun {
        TriggerRun {
            id: new_id("trr"),
            trigger_id: id.to_string(),
            status: RunStatus::Queued,
            reason: reason.to_string(),
            session_id: None,
            scheduled,
            time: RunTime {
                started: now,
                ended: None,
            },
            quiet: None,
            report: None,
            text: None,
            error: None,
            cause: None,
        }
    }

    /// Fire outside the schedule: a webhook call, a watch, a manual run.
    fn fire(
        self: &Arc<Self>,
        id: &str,
        reason: &str,
        payload: Option<TriggerPayload>,
    ) -> Result<TriggerRun, TriggerError> {
        self.launch(
            Self::new_run(id, reason, None, self.clock.now_ms()),
            None,
            payload,
        )
    }

    /// Start `run`'s turn, or record why it does not start.
    ///
    /// Every fire is claimed in the database before it runs: a trigger has
    /// one unsettled run across all the processes that share the data
    /// directory, and the insert that makes a run that one decides between
    /// two of them. A scheduled tick's run was claimed with its tick
    /// (`tick`); any other is claimed here. The fire that loses is recorded
    /// as skipped, as it is when the run in the way is this process's own.
    fn launch(
        self: &Arc<Self>,
        mut run: TriggerRun,
        tick: Option<TickClaim>,
        payload: Option<TriggerPayload>,
    ) -> Result<TriggerRun, TriggerError> {
        if self.stopped() {
            return Err(TriggerError::Conflict(
                "the server is shutting down: nothing is fired any more".to_string(),
            ));
        }
        let (id, now) = (run.trigger_id.clone(), run.time.started);
        let id = id.as_str();
        let (reason, scheduled) = (run.reason.clone(), run.scheduled);
        let turn = {
            let mut entries = self.lock();
            let Some(entry) = entries.get_mut(id) else {
                return Err(TriggerError::NotFound(format!("trigger not found: {id}")));
            };
            if let Err(error) = &entry.schedule {
                return Err(TriggerError::Invalid(format!(
                    "trigger \"{id}\" cannot run: {error}"
                )));
            }
            let claimed = if entry.busy {
                Err("the previous run has not finished")
            } else {
                match tick {
                    Some(TickClaim::Run) => Ok(()),
                    Some(_) => Err(BUSY_ELSEWHERE),
                    None => match self.store.claim_run(&run) {
                        Ok(true) => Ok(()),
                        Ok(false) => Err(BUSY_ELSEWHERE),
                        Err(error) => return Err(TriggerError::Failed(error.to_string())),
                    },
                }
            };
            match claimed {
                Err(cause) => Err(cause),
                Ok(()) => {
                    entry.busy = true;
                    // The session a `reuse` trigger keeps is whichever
                    // process's run ended last.
                    if self.role.is_some() {
                        if let Ok(Some(theirs)) = self.store.state(id) {
                            entry.state.session_id = theirs.session_id;
                        }
                    }
                    // Keyed on what the run carries, not only on the kind
                    // of trigger: any payload is outside text. The rules
                    // themselves are put together where the agent's are
                    // known (`trigger::rules`).
                    //
                    // A trigger a session outside input had started wrote
                    // is that input's doing (`StoredSpec::outside_start`):
                    // its run in a session of its own is started by
                    // outside input as well. One that runs in the session
                    // that wrote it (`current`; a tool's trigger names no
                    // other) is a turn of that session, which carries the
                    // mark and its ceiling itself.
                    let own_session =
                        !matches!(entry.spec.session_mode(), SessionMode::Existing(_));
                    let untrusted = payload.is_some()
                        || matches!(
                            entry.schedule,
                            Ok(Schedule::Webhook | Schedule::Watch { .. })
                        )
                        || (entry.outside_start && own_session);
                    Ok(TriggerTurn {
                        trigger_id: id.to_string(),
                        run_id: run.id.clone(),
                        title: entry
                            .spec
                            .title
                            .clone()
                            .filter(|title| !title.trim().is_empty())
                            .unwrap_or_else(|| id.to_string()),
                        agent: entry.spec.agent.clone(),
                        model: entry.spec.model_ref(),
                        mode: entry.spec.session_mode(),
                        session_id: entry.state.session_id.clone(),
                        origin_session_id: entry.origin_session_id.clone(),
                        inherited: entry.inherited.clone().unwrap_or_default(),
                        own: entry
                            .spec
                            .permission
                            .as_ref()
                            .map(|own| from_config(own, &self.settings.home))
                            .unwrap_or_default(),
                        untrusted,
                        restricted: entry.restricted,
                        outside_content: entry.outside_content,
                        operator_authored: entry.origin_session_id.is_none() && !entry.restricted,
                        prompt: entry.spec.prompt.clone(),
                        payload,
                        now_ms: now,
                    })
                }
            }
        };
        let turn = match turn {
            Ok(turn) => turn,
            Err(cause) => {
                run.status = RunStatus::Skipped;
                run.cause = Some(cause.to_string());
                run.time.ended = Some(now);
                let _ = self.store.put_run(&run);
                let mut data = json!({
                    "triggerID": id,
                    "runID": run.id,
                    "reason": reason,
                    "cause": run.cause,
                });
                if let Some(scheduled) = scheduled {
                    data["scheduled"] = json!(scheduled);
                }
                self.publish(&TRIGGER_SKIPPED, data);
                return Ok(run);
            }
        };
        let mut data = json!({ "triggerID": id, "runID": run.id, "reason": reason });
        if let Some(scheduled) = scheduled {
            data["scheduled"] = json!(scheduled);
        }
        self.publish(&TRIGGER_FIRED, data);
        let service = Arc::clone(self);
        let queued = run.clone();
        tokio::spawn(async move { service.execute(turn, queued).await });
        Ok(run)
    }

    async fn execute(self: Arc<Self>, turn: TriggerTurn, mut run: TriggerRun) {
        let slot = self.slots.clone().acquire_owned().await;
        // The wait for a slot can outlast the server: `stop` may have come
        // while this run was queued, and a turn started now would begin
        // after the shutdown had settled the others. Looked at under the
        // lock `stop` takes, so a run either does not start or is counted
        // before `stop` returns.
        let started = {
            let _entries = self.lock();
            (!self.stopped()).then(|| {
                self.in_flight.fetch_add(1, Ordering::SeqCst);
                InFlight(self.in_flight.clone())
            })
        };
        let Some(started) = started else {
            drop(slot);
            self.abandon(&turn, run);
            return;
        };
        run.status = RunStatus::Running;
        let _ = self.store.put_run(&run);
        // A panic in the turn is a failed run like any other. Left to
        // unwind, it would end this task with the run still `running` and
        // its claim held by a process that is alive: every later fire of
        // the trigger, here and in any process sharing the database, would
        // be skipped until this one exits.
        let result = std::panic::AssertUnwindSafe(async { self.ops.run_turn(&turn).await })
            .catch_unwind()
            .await
            .unwrap_or_else(|panic| {
                let what = panic
                    .downcast_ref::<&str>()
                    .map(|text| text.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "no message".to_string());
                tracing::error!(
                    "trigger \"{}\" panicked in its run: {what}",
                    turn.trigger_id
                );
                Err(crate::trigger::TurnFailure {
                    session_id: None,
                    message: format!("{RUN_PANICKED}: {what}"),
                })
            });
        drop(started);
        drop(slot);
        let now = self.clock.now_ms();
        let duration = now.saturating_sub(run.time.started);
        run.time.ended = Some(now);
        let (quiet_token, report) = {
            let entries = self.lock();
            entries
                .get(&turn.trigger_id)
                .map(|entry| (entry.spec.quiet_token.clone(), entry.spec.report()))
                .unwrap_or((None, Report::Final))
        };
        let session_id = match result {
            Ok(outcome) => {
                let quiet = quiet_token
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|token| !token.is_empty() && outcome.text.trim() == token);
                let decision = if quiet { Report::None } else { report };
                let mut text = outcome.text;
                if text.len() > MAX_RUN_TEXT {
                    let mut end = MAX_RUN_TEXT;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                }
                run.status = RunStatus::Completed;
                run.session_id = Some(outcome.session_id.clone());
                run.quiet = Some(quiet);
                run.report = Some(decision);
                let mut data = json!({
                    "triggerID": turn.trigger_id,
                    "runID": run.id,
                    "sessionID": outcome.session_id,
                    "duration": duration,
                    "quiet": quiet,
                    "report": decision.as_str(),
                });
                if decision == Report::Final {
                    data["text"] = json!(text);
                }
                run.text = Some(text);
                let _ = self.store.put_run(&run);
                self.publish(&TRIGGER_COMPLETED, data);
                Some(outcome.session_id)
            }
            Err(failure) => {
                run.status = RunStatus::Failed;
                run.session_id = failure.session_id.clone();
                run.error = Some(failure.message.clone());
                let _ = self.store.put_run(&run);
                let mut data = json!({
                    "triggerID": turn.trigger_id,
                    "runID": run.id,
                    "duration": duration,
                    "error": failure.message,
                });
                if let Some(session_id) = &failure.session_id {
                    data["sessionID"] = json!(session_id);
                }
                self.publish(&TRIGGER_FAILED, data);
                failure.session_id
            }
        };
        {
            let mut entries = self.lock();
            if let Some(entry) = entries.get_mut(&turn.trigger_id) {
                entry.busy = false;
                entry.state.last_fired = Some(now);
                let kept = session_id.filter(|_| turn.mode == SessionMode::Reuse);
                if let Some(session_id) = &kept {
                    entry.state.session_id = Some(session_id.clone());
                }
                let _ = self.store.finish(&turn.trigger_id, now, kept.as_deref());
            }
        }
        self.settled.notify_waiters();
        self.wake.notify_one();
    }

    /// A queued run that will not start, because the server is stopping:
    /// recorded as skipped. The tick it answered is given back, so the next
    /// start fires it if the trigger catches up.
    fn abandon(&self, turn: &TriggerTurn, mut run: TriggerRun) {
        let now = self.clock.now_ms();
        run.status = RunStatus::Skipped;
        run.cause = Some(STOPPED_BEFORE_START.to_string());
        run.time.ended = Some(now);
        let _ = self.store.put_run(&run);
        if run.scheduled.is_some() {
            let _ = self.store.unclaim_tick(&turn.trigger_id, run.time.started);
        }
        let mut data = json!({
            "triggerID": turn.trigger_id,
            "runID": run.id,
            "reason": run.reason,
            "cause": run.cause,
        });
        if let Some(scheduled) = run.scheduled {
            data["scheduled"] = json!(scheduled);
        }
        self.publish(&TRIGGER_SKIPPED, data);
        if let Some(entry) = self.lock().get_mut(&turn.trigger_id) {
            entry.busy = false;
        }
        self.settled.notify_waiters();
    }

    /// Wait for a run to finish (or be skipped) and return it.
    pub async fn wait(&self, run_id: &str) -> Option<TriggerRun> {
        loop {
            let settled = self.settled.notified();
            tokio::pin!(settled);
            // Register before reading, so a finish in between is not missed.
            settled.as_mut().enable();
            let run = self.run(run_id)?;
            if run.status.settled() {
                return Some(run);
            }
            settled.await;
        }
    }

    /// A one-line description of what a trigger waits for.
    pub fn describe(record: &TriggerRecord) -> String {
        let on = &record.spec.on;
        if let Some(cron) = &on.cron {
            return match &on.timezone {
                Some(zone) => format!("cron {cron} ({zone})"),
                None => format!("cron {cron} (UTC)"),
            };
        }
        if let Some(interval) = &on.interval {
            return match crate::trigger::parse_duration(interval) {
                Ok(ms) => format!("every {}", format_duration(ms)),
                Err(_) => format!("every {interval}"),
            };
        }
        if let Some(at) = &on.at {
            return format!("once at {at}");
        }
        if let Some(watch) = &on.watch {
            return format!("when {} changes", watch.path);
        }
        "on a webhook call".to_string()
    }
}

/// A config trigger has no row of its own: its age is when it was first seen.
fn state_time(store: &TriggerStore, id: &str, fallback: u64) -> u64 {
    store
        .state(id)
        .ok()
        .flatten()
        .map(|state| state.time_seen)
        .unwrap_or(fallback)
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
