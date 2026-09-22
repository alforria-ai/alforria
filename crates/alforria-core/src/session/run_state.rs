//! Session run state — port of `session/run-state.ts` plus the
//! `effect/runner.ts` state machine it is built on.
//!
//! The TS runner serializes per-session work with four states —
//! `Idle | Running | Shell | ShellThenRun` — guarded by a synchronized ref
//! and using Effect's `Deferred`/`Latch`/`Fiber` for coordination. The Rust
//! port keeps the state machine but swaps the primitives: `tokio` tasks for
//! fibers, a watch channel for `Deferred` and `Latch`, and work factories
//! (`Work<A, E>`) for `Effect` values, since a Rust future cannot be
//! re-executed after completion and the runner starts work more than once
//! (`ShellThenRun`).
//!
//! Callbacks (`on_idle`/`on_busy`/`on_interrupt`) always run *outside* the
//! runner state lock: run-state wires them to the runner map and the status
//! service, and running them under the lock would allow a lock cycle with
//! [`SessionRunState::cancel`] (which looks runners up in the map).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use alforria_schema::session_status::SessionStatusInfo;

use crate::session::error::{BusyError, SessionError};
use crate::session::message::WithParts;
use crate::session::status::SessionStatusService;
use crate::CoreError;
use tokio_util::sync::CancellationToken;

use crate::tool::def::BoxFuture;

// ---------------------------------------------------------------------------
// Runner primitives (effect/runner.ts)
// ---------------------------------------------------------------------------

/// A restartable `Effect.Effect<A, E>`: a factory that produces a fresh
/// future on every call.
pub type Work<A, E> = Arc<dyn Fn() -> BoxFuture<'static, Result<A, E>> + Send + Sync>;

/// An `Effect.Effect<void>`-shaped callback factory.
type Effect = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// `Runner.Cancelled` — surfaces when work was interrupted and no
/// `on_interrupt` was supplied. TS dies with the defect here; the Rust
/// port surfaces it as an error instead (a defect would poison the tokio
/// task worker).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("RunnerCancelled")]
pub struct Cancelled;

/// `RunnerError` — the `E | Cancelled` channel of a run's `Deferred`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunnerError<E> {
    #[error(transparent)]
    Work(E),
    #[error("RunnerCancelled")]
    Cancelled(Cancelled),
}

/// `Runner.Busy` — [`Runner::start_shell`] on a non-idle runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("RunnerBusy")]
pub struct Busy;

/// `Runner.startShell` channel: `E | Busy`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShellError<E> {
    #[error(transparent)]
    Runner(#[from] RunnerError<E>),
    #[error(transparent)]
    Busy(#[from] Busy),
}

/// A `Deferred.Deferred<T>`: a single value completed once, awaitable many
/// times. Backed by a watch channel (`Option<T>`: `None` = pending).
pub struct Deferred<T> {
    tx: tokio::sync::watch::Sender<Option<T>>,
}

impl<T: Clone + Send + 'static> Deferred<T> {
    pub fn new() -> Deferred<T> {
        Deferred {
            tx: tokio::sync::watch::channel(None).0,
        }
    }

    /// `Deferred.await` — resolve when [`Deferred::complete`] ran.
    pub async fn get(&self) -> Option<T> {
        let mut rx = self.tx.subscribe();
        loop {
            if let Some(value) = rx.borrow().clone() {
                return Some(value);
            }
            if rx.changed().await.is_err() {
                return None;
            }
        }
    }

    /// `Deferred.done` — complete with `value` (no-op when already done).
    pub fn complete(&self, value: T) {
        self.tx.send_modify(|slot| {
            if slot.is_none() {
                *slot = Some(value);
            }
        });
    }

    /// `Deferred.isDone`.
    pub fn is_done(&self) -> bool {
        self.tx.borrow().is_some()
    }
}

impl<T: Clone + Send + 'static> Default for Deferred<T> {
    fn default() -> Self {
        Deferred::new()
    }
}

/// `Latch.Latch` — starts closed, `open()` releases every waiter.
#[derive(Clone)]
pub struct Latch {
    tx: Arc<tokio::sync::watch::Sender<bool>>,
}

impl Latch {
    pub fn closed() -> Latch {
        Latch {
            tx: Arc::new(tokio::sync::watch::channel(false).0),
        }
    }

    /// `Latch.open`.
    pub fn open(&self) {
        self.tx.send_modify(|value| {
            *value = true;
        });
    }

    /// `Latch.await` — resolve once opened.
    pub async fn wait(&self) {
        let mut rx = self.tx.subscribe();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// The done channel of a run: `A | E | Cancelled`.
type Done<A, E> = Result<A, RunnerError<E>>;

struct RunHandle<A, E> {
    id: u64,
    done: Arc<Deferred<Done<A, E>>>,
    /// The run's abort signal. Cancelling it lets the work observe the
    /// interrupt cooperatively and run its interrupt handlers (TS: the
    /// fiber interrupt runs `Effect.onInterrupt` finalizers) instead of
    /// being dropped mid-flight.
    cancel: CancellationToken,
}

struct PendingHandle<A, E> {
    /// Kept for TS parity (`PendingHandle.id`); `start_run` assigns its
    /// own id when the pending run starts (runner.ts:96-106).
    #[allow(dead_code)]
    id: u64,
    done: Arc<Deferred<Done<A, E>>>,
    cancel: CancellationToken,
    work: Work<A, E>,
}

#[derive(Clone)]
struct ShellHandle {
    id: u64,
    cancelled: Arc<Deferred<()>>,
    ready: Option<Latch>,
    abort: tokio::task::AbortHandle,
}

enum RunnerState<A, E> {
    Idle,
    Running(RunHandle<A, E>),
    Shell(ShellHandle),
    ShellThenRun(ShellHandle, PendingHandle<A, E>),
}

struct RunnerInner<A, E> {
    state: Mutex<RunnerState<A, E>>,
    next_id: AtomicU64,
    on_idle: Option<Effect>,
    on_busy: Option<Effect>,
    on_interrupt: Option<Work<A, E>>,
}

/// `Runner.make` (runner.ts:54-166): serializes a session's work.
#[derive(Clone)]
pub struct Runner<A, E> {
    inner: Arc<RunnerInner<A, E>>,
}

/// `Runner.make` options (runner.ts:55-59).
pub struct RunnerOptions<A, E> {
    pub on_idle: Option<Effect>,
    pub on_busy: Option<Effect>,
    pub on_interrupt: Option<Work<A, E>>,
}

enum CancelAction<A, E> {
    Nothing,
    Running {
        cancel: CancellationToken,
        done: Arc<Deferred<Done<A, E>>>,
    },
    Shell(ShellHandle),
    ShellThenRun {
        shell: ShellHandle,
        done: Arc<Deferred<Done<A, E>>>,
    },
}

impl<A, E> RunnerInner<A, E> {
    fn lock_state(&self) -> MutexGuard<'_, RunnerState<A, E>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<A: Clone + Send + Sync + 'static, E: Clone + Send + Sync + 'static> Runner<A, E> {
    pub fn new(options: RunnerOptions<A, E>) -> Runner<A, E> {
        Runner {
            inner: Arc::new(RunnerInner {
                state: Mutex::new(RunnerState::Idle),
                next_id: AtomicU64::new(0),
                on_idle: options.on_idle,
                on_busy: options.on_busy,
                on_interrupt: options.on_interrupt,
            }),
        }
    }

    fn next_id(&self) -> u64 {
        self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// `Runner.busy` (runner.ts:163-165).
    pub fn busy(&self) -> bool {
        !matches!(*self.inner.lock_state(), RunnerState::Idle)
    }

    /// `idle` callback run *outside* the state lock.
    async fn run_idle(&self) {
        if let Some(on_idle) = self.inner.on_idle.clone() {
            (on_idle)().await;
        }
    }

    /// `awaitDone` (runner.ts:80-81): `Cancelled` resolves to
    /// `onInterrupt` (or dies — surfacing [`RunnerError::Cancelled`] here).
    async fn await_done(&self, done: Arc<Deferred<Done<A, E>>>) -> Result<A, RunnerError<E>> {
        match done.get().await {
            Some(Ok(value)) => Ok(value),
            Some(Err(RunnerError::Work(err))) => Err(RunnerError::Work(err)),
            Some(Err(RunnerError::Cancelled(_))) => self.on_interrupt().await,
            None => Err(RunnerError::Cancelled(Cancelled)),
        }
    }

    /// `awaitDone`'s catchTag — `onInterrupt ?? die(Cancelled)`.
    async fn on_interrupt(&self) -> Result<A, RunnerError<E>> {
        match self.inner.on_interrupt.clone() {
            Some(work) => (work)().await.map_err(RunnerError::Work),
            None => Err(RunnerError::Cancelled(Cancelled)),
        }
    }

    /// `finishRun` (runner.ts:83-94): transition `Running` → `Idle` when
    /// the run is still current, then complete the done channel.
    async fn finish_run(&self, id: u64, done: Arc<Deferred<Done<A, E>>>, exit: Result<A, E>) {
        let was_current = {
            let mut state = self.inner.lock_state();
            match &mut *state {
                RunnerState::Running(run) if run.id == id => {
                    *state = RunnerState::Idle;
                    true
                }
                _ => false,
            }
        };
        if was_current {
            self.run_idle().await;
        }
        done.complete(exit.map_err(RunnerError::Work));
    }

    /// `startRun` (runner.ts:96-106): fork the work with an `onExit` that
    /// runs `finishRun`.
    fn start_run(
        &self,
        work: Work<A, E>,
        done: Arc<Deferred<Done<A, E>>>,
        cancel: CancellationToken,
    ) -> RunHandle<A, E> {
        let id = self.next_id();
        let runner = self.clone();
        let task_done = done.clone();
        tokio::spawn(async move {
            let exit = (work)().await;
            // Effect.onExit → finishRun
            runner.finish_run(id, task_done, exit).await;
        });
        RunHandle { id, done, cancel }
    }

    /// `ensureRunning` (runner.ts:108-136).
    pub async fn ensure_running(
        &self,
        cancel: CancellationToken,
        work: Work<A, E>,
    ) -> Result<A, RunnerError<E>> {
        let done = {
            let mut state = self.inner.lock_state();
            match &mut *state {
                RunnerState::Running(run) => run.done.clone(),
                RunnerState::ShellThenRun(_, run) => run.done.clone(),
                RunnerState::Shell(_) => {
                    let done = Arc::new(Deferred::new());
                    let run = PendingHandle {
                        id: self.next_id(),
                        done: done.clone(),
                        cancel,
                        work,
                    };
                    if let RunnerState::Shell(shell) =
                        std::mem::replace(&mut *state, RunnerState::Idle)
                    {
                        *state = RunnerState::ShellThenRun(shell, run);
                    }
                    done
                }
                RunnerState::Idle => {
                    let done = Arc::new(Deferred::new());
                    let run = self.start_run(work, done.clone(), cancel);
                    *state = RunnerState::Running(run);
                    done
                }
            }
        };
        self.await_done(done).await
    }

    /// `startShell` (runner.ts:138-158): `Busy` unless idle.
    pub async fn start_shell(
        &self,
        work: Work<A, E>,
        ready: Option<Latch>,
    ) -> Result<A, ShellError<E>> {
        let fiber = {
            let mut state = self.inner.lock_state();
            if !matches!(*state, RunnerState::Idle) {
                return Err(Busy.into());
            }
            let cancelled = Arc::new(Deferred::<()>::new());
            let id = self.next_id();
            let inner = self.inner.clone();
            let task = tokio::spawn(async move {
                let exit = (work)().await;
                // Effect.ensuring(finishShell(id)) — runs on work exit.
                let runner = Runner { inner };
                runner.finish_shell(id).await;
                exit
            });
            let handle = ShellHandle {
                id,
                cancelled,
                ready,
                abort: task.abort_handle(),
            };
            *state = RunnerState::Shell(handle);
            task
        };
        if let Some(on_busy) = self.inner.on_busy.clone() {
            (on_busy)().await;
        }
        // Await the fiber (runner.ts:139-152): an interrupt resolves to
        // `onInterrupt`; work failures propagate.
        match fiber.await {
            Ok(exit) => exit.map_err(RunnerError::Work).map_err(ShellError::Runner),
            Err(err) if err.is_cancelled() => self.on_interrupt().await.map_err(ShellError::Runner),
            Err(err) => std::panic::resume_unwind(err.into_panic()),
        }
    }

    /// `finishShell` (runner.ts:109-124): back to `Idle`, or start the
    /// pending run (`ShellThenRun`).
    async fn finish_shell(&self, id: u64) {
        let idle = {
            let mut state = self.inner.lock_state();
            match std::mem::replace(&mut *state, RunnerState::Idle) {
                RunnerState::Shell(shell) if shell.id == id => true,
                RunnerState::ShellThenRun(shell, run) if shell.id == id => {
                    let handle = self.start_run(run.work, run.done, run.cancel);
                    *state = RunnerState::Running(handle);
                    false
                }
                other => {
                    *state = other;
                    false
                }
            }
        };
        if idle {
            self.run_idle().await;
        }
    }

    /// `stopShell` (runner.ts:126-131).
    async fn stop_shell(&self, shell: &ShellHandle) {
        if let Some(ready) = &shell.ready {
            ready.wait().await;
        }
        shell.cancelled.complete(());
        shell.abort.abort();
    }

    /// `cancel` (runner.ts:160-165).
    pub async fn cancel(&self) {
        let action = {
            let mut state = self.inner.lock_state();
            match std::mem::replace(&mut *state, RunnerState::Idle) {
                RunnerState::Idle => CancelAction::Nothing,
                RunnerState::Running(run) => CancelAction::Running {
                    cancel: run.cancel,
                    done: run.done,
                },
                RunnerState::Shell(shell) => CancelAction::Shell(shell),
                RunnerState::ShellThenRun(shell, run) => CancelAction::ShellThenRun {
                    shell,
                    done: run.done,
                },
            }
        };
        match action {
            CancelAction::Nothing => {}
            CancelAction::Running { cancel, done } => {
                cancel.cancel();
                done.complete(Err(RunnerError::Cancelled(Cancelled)));
                self.run_idle().await;
            }
            CancelAction::Shell(shell) => {
                self.stop_shell(&shell).await;
                self.run_idle().await;
            }
            CancelAction::ShellThenRun { shell, done } => {
                self.stop_shell(&shell).await;
                done.complete(Err(RunnerError::Cancelled(Cancelled)));
                self.run_idle().await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Background jobs (packages/core/src/background-job.ts — the surface M5 needs)
// ---------------------------------------------------------------------------

/// `BackgroundJob.Status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundJobStatus {
    Running,
    Completed,
    Error,
    Cancelled,
}

impl BackgroundJobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            BackgroundJobStatus::Running => "running",
            BackgroundJobStatus::Completed => "completed",
            BackgroundJobStatus::Error => "error",
            BackgroundJobStatus::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for BackgroundJobStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `BackgroundJob.Info` — the fields `cancelBackgroundJobs` reads.
#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundJobInfo {
    pub id: String,
    pub status: BackgroundJobStatus,
    /// `metadata?: Record<string, unknown>` — only `sessionId` and
    /// `parentSessionId` are consulted.
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

impl BackgroundJobInfo {
    fn metadata_session_id(&self, key: &str) -> Option<&str> {
        self.metadata
            .as_ref()
            .and_then(|metadata| metadata.get(key))
            .and_then(serde_json::Value::as_str)
    }
}

/// `BackgroundJob.Service` — only `list`/`cancel` are consumed by the
/// session run state; the full registry lands in a later milestone.
pub trait BackgroundJobs: Send + Sync {
    fn list(&self) -> Result<Vec<BackgroundJobInfo>, CoreError>;
    fn cancel(&self, id: &str) -> Result<(), CoreError>;
}

/// `cancelBackgroundJobs` (run-state.ts:111-143): fixpoint — a job matches
/// when it is running, not yet cancelled, and its id (or `sessionId` /
/// `parentSessionId` metadata) is pending; cancelling adds it (and its
/// `sessionId`) to the pending set, so child chains cancel too.
pub fn cancel_background_jobs(
    background: &dyn BackgroundJobs,
    session_id: &str,
) -> Result<(), CoreError> {
    let jobs = background.list()?;
    let mut pending: HashSet<String> = HashSet::new();
    pending.insert(session_id.to_string());
    let mut cancelled: HashSet<String> = HashSet::new();
    let matches =
        |job: &BackgroundJobInfo, pending: &HashSet<String>, cancelled: &HashSet<String>| {
            if job.status != BackgroundJobStatus::Running {
                return false;
            }
            if cancelled.contains(&job.id) {
                return false;
            }
            if pending.contains(&job.id) {
                return true;
            }
            if let Some(id) = job.metadata_session_id("sessionId") {
                if pending.contains(id) {
                    return true;
                }
            }
            if let Some(id) = job.metadata_session_id("parentSessionId") {
                if pending.contains(id) {
                    return true;
                }
            }
            false
        };
    loop {
        let batch: Vec<&BackgroundJobInfo> = jobs
            .iter()
            .filter(|job| matches(job, &pending, &cancelled))
            .collect();
        if batch.is_empty() {
            break;
        }
        for job in batch {
            background.cancel(&job.id)?;
            cancelled.insert(job.id.clone());
            pending.insert(job.id.clone());
            if let Some(id) = job.metadata_session_id("sessionId") {
                pending.insert(id.to_string());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SessionRunState (session/run-state.ts)
// ---------------------------------------------------------------------------

/// `SessionRunState.Service` state (run-state.ts:19-106).
pub struct SessionRunState {
    background: Arc<dyn BackgroundJobs>,
    status: Arc<SessionStatusService>,
    runners: Mutex<HashMap<String, Arc<Runner<WithParts, SessionError>>>>,
}

impl SessionRunState {
    pub fn new(
        background: Arc<dyn BackgroundJobs>,
        status: Arc<SessionStatusService>,
    ) -> Arc<Self> {
        Arc::new(SessionRunState {
            background,
            status,
            runners: Mutex::new(HashMap::new()),
        })
    }

    fn lock_runners(
        &self,
    ) -> MutexGuard<'_, HashMap<String, Arc<Runner<WithParts, SessionError>>>> {
        self.runners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `runner` (run-state.ts:33-52): get-or-create. An existing runner
    /// keeps its original `onInterrupt` (TS behavior).
    fn runner(
        self: &Arc<Self>,
        session_id: &str,
        on_interrupt: Work<WithParts, SessionError>,
    ) -> Arc<Runner<WithParts, SessionError>> {
        let mut runners = self.lock_runners();
        if let Some(existing) = runners.get(session_id) {
            return existing.clone();
        }
        let weak = Arc::downgrade(self);
        let session = session_id.to_string();
        let on_idle: Effect = Arc::new(move || {
            let weak = weak.clone();
            let session = session.clone();
            Box::pin(async move {
                let Some(this) = weak.upgrade() else { return };
                this.lock_runners().remove(&session);
                let _ = this.status.set(&session, SessionStatusInfo::Idle);
            })
        });
        let weak = Arc::downgrade(self);
        let session = session_id.to_string();
        let on_busy: Effect = Arc::new(move || {
            let weak = weak.clone();
            let session = session.clone();
            Box::pin(async move {
                let Some(this) = weak.upgrade() else { return };
                let _ = this.status.set(&session, SessionStatusInfo::Busy);
            })
        });
        let runner = Arc::new(Runner::new(RunnerOptions {
            on_idle: Some(on_idle),
            on_busy: Some(on_busy),
            on_interrupt: Some(on_interrupt),
        }));
        runners.insert(session_id.to_string(), runner.clone());
        runner
    }

    /// `assertNotBusy` (run-state.ts:54-58).
    pub fn assert_not_busy(self: &Arc<Self>, session_id: &str) -> Result<(), BusyError> {
        let busy = self
            .lock_runners()
            .get(session_id)
            .is_some_and(|runner| runner.busy());
        if busy {
            return Err(BusyError {
                session_id: session_id.to_string(),
            });
        }
        Ok(())
    }

    /// `cancel` (run-state.ts:60-70).
    pub async fn cancel(self: &Arc<Self>, session_id: &str) -> Result<(), CoreError> {
        cancel_background_jobs(self.background.as_ref(), session_id)?;
        let existing = self.lock_runners().get(session_id).cloned();
        match existing {
            None => {
                self.status.set(session_id, SessionStatusInfo::Idle)?;
                Ok(())
            }
            Some(runner) => {
                runner.cancel().await;
                Ok(())
            }
        }
    }

    /// `ensureRunning` (run-state.ts:72-76).
    pub async fn ensure_running(
        self: &Arc<Self>,
        session_id: &str,
        cancel: CancellationToken,
        on_interrupt: Work<WithParts, SessionError>,
        work: Work<WithParts, SessionError>,
    ) -> Result<WithParts, RunnerError<SessionError>> {
        let runner = self.runner(session_id, on_interrupt);
        runner.ensure_running(cancel, work).await
    }

    /// `startShell` (run-state.ts:78-92) — `RunnerBusy` maps to
    /// `Session.BusyError`.
    pub async fn start_shell(
        self: &Arc<Self>,
        session_id: &str,
        on_interrupt: Work<WithParts, SessionError>,
        work: Work<WithParts, SessionError>,
        ready: Option<Latch>,
    ) -> Result<WithParts, SessionError> {
        let runner = self.runner(session_id, on_interrupt);
        runner
            .start_shell(work, ready)
            .await
            .map_err(|err| match err {
                ShellError::Busy(_) => SessionError::Busy(BusyError {
                    session_id: session_id.to_string(),
                }),
                ShellError::Runner(RunnerError::Work(err)) => err,
                ShellError::Runner(RunnerError::Cancelled(_)) => {
                    // unreachable: the runner always has an onInterrupt here
                    SessionError::Busy(BusyError {
                        session_id: session_id.to_string(),
                    })
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::bus::EventBus;
    use alforria_schema::session_v1::{UserTime, V1Message, V1UserModel};
    use std::time::Duration;

    fn box_work<A: Clone + Send + Sync + 'static, E: Send + Sync + 'static>(
        value: A,
    ) -> Work<A, E> {
        Arc::new(move || {
            let value = value.clone();
            Box::pin(async move { Ok(value) })
        })
    }

    // ------------------------------------------------------------------ Runner

    #[tokio::test]
    async fn ensure_running_runs_work_and_goes_idle() {
        let runner = Runner::<String, String>::new(RunnerOptions {
            on_idle: None,
            on_busy: None,
            on_interrupt: None,
        });
        let result = runner
            .ensure_running(CancellationToken::new(), box_work("hello".to_string()))
            .await
            .unwrap();
        assert_eq!(result, "hello");
        assert!(!runner.busy());
    }

    #[tokio::test]
    async fn concurrent_ensure_running_joins_one_run() {
        let calls = Arc::new(AtomicU64::new(0));
        let calls_clone = calls.clone();
        let work: Work<String, String> = Arc::new(move || {
            let calls = calls_clone.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok("shared".to_string())
            })
        });
        let runner = Runner::<String, String>::new(RunnerOptions {
            on_idle: None,
            on_busy: None,
            on_interrupt: None,
        });
        let first = runner.clone();
        let second = runner.clone();
        let (a, b) = tokio::join!(
            first.ensure_running(CancellationToken::new(), work.clone()),
            second.ensure_running(CancellationToken::new(), work),
        );
        assert_eq!(a.unwrap(), "shared");
        assert_eq!(b.unwrap(), "shared");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn callbacks_fire_on_idle_and_busy() {
        let idle = Arc::new(AtomicU64::new(0));
        let busy = Arc::new(AtomicU64::new(0));
        let idle_cb = idle.clone();
        let busy_cb = busy.clone();
        let runner = Runner::<String, String>::new(RunnerOptions {
            on_idle: Some(Arc::new(move || {
                let idle = idle_cb.clone();
                Box::pin(async move {
                    idle.fetch_add(1, Ordering::Relaxed);
                })
            })),
            on_busy: Some(Arc::new(move || {
                let busy = busy_cb.clone();
                Box::pin(async move {
                    busy.fetch_add(1, Ordering::Relaxed);
                })
            })),
            on_interrupt: None,
        });
        runner
            .ensure_running(CancellationToken::new(), box_work("work".to_string()))
            .await
            .unwrap();
        assert_eq!(idle.load(Ordering::Relaxed), 1);
        assert_eq!(busy.load(Ordering::Relaxed), 0);
        assert!(!runner.busy());

        let _ = runner
            .start_shell(box_work("shell".to_string()), None)
            .await;
        assert_eq!(busy.load(Ordering::Relaxed), 1);
        assert_eq!(idle.load(Ordering::Relaxed), 2);
        assert!(!runner.busy());
    }

    #[tokio::test]
    async fn start_shell_rejects_when_busy() {
        let runner = Runner::<String, String>::new(RunnerOptions {
            on_idle: None,
            on_busy: None,
            on_interrupt: None,
        });
        let started = Latch::closed();
        let work: Work<String, String> = {
            let started = started.clone();
            Arc::new(move || {
                let started = started.clone();
                Box::pin(async move {
                    started.open();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok("shell".to_string())
                })
            })
        };
        let second = {
            let runner = runner.clone();
            let started = started.clone();
            tokio::spawn(async move {
                started.wait().await;
                runner
                    .start_shell(box_work("second".to_string()), None)
                    .await
            })
        };
        let first = runner.start_shell(work, None).await;
        let second = second.await.unwrap();
        assert_eq!(first.unwrap(), "shell");
        assert!(matches!(second, Err(ShellError::Busy(_))));
        assert!(!runner.busy());
    }

    #[tokio::test]
    async fn ensure_running_waits_for_pending_run_after_shell() {
        let runner = Runner::<String, String>::new(RunnerOptions {
            on_idle: None,
            on_busy: None,
            on_interrupt: None,
        });
        let work: Work<String, String> = Arc::new(|| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok("shell".to_string())
            })
        });
        let first = runner.clone();
        let second = runner.clone();
        let (shell, run) = tokio::join!(
            first.start_shell(work, None),
            second.ensure_running(CancellationToken::new(), box_work("run".to_string())),
        );
        assert_eq!(shell.unwrap(), "shell");
        assert_eq!(run.unwrap(), "run");
        assert!(!runner.busy());
    }

    #[tokio::test]
    async fn cancel_resolves_on_interrupt() {
        let runner = Runner::<String, String>::new(RunnerOptions {
            on_idle: None,
            on_busy: None,
            on_interrupt: Some(box_work("interrupted".to_string())),
        });
        let started = Latch::closed();
        let work: Work<String, String> = {
            let started = started.clone();
            Arc::new(move || {
                let started = started.clone();
                Box::pin(async move {
                    started.open();
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Err("sleep finished".to_string())
                })
            })
        };
        let cancel_handle = {
            let runner = runner.clone();
            let started = started.clone();
            tokio::spawn(async move {
                started.wait().await;
                runner.cancel().await;
            })
        };
        let result = runner.ensure_running(CancellationToken::new(), work).await;
        cancel_handle.await.unwrap();
        assert_eq!(result.unwrap(), "interrupted");
        assert!(!runner.busy());
    }

    #[tokio::test]
    async fn cancel_idle_runner_is_noop() {
        let runner = Runner::<String, String>::new(RunnerOptions {
            on_idle: None,
            on_busy: None,
            on_interrupt: None,
        });
        runner.cancel().await;
        assert!(!runner.busy());
    }

    #[tokio::test]
    async fn cancel_with_no_on_interrupt_surfaces_cancelled() {
        let runner = Runner::<String, String>::new(RunnerOptions {
            on_idle: None,
            on_busy: None,
            on_interrupt: None,
        });
        let started = Latch::closed();
        let work: Work<String, String> = {
            let started = started.clone();
            Arc::new(move || {
                let started = started.clone();
                Box::pin(async move {
                    started.open();
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Err("sleep finished".to_string())
                })
            })
        };
        let handle = {
            let runner = runner.clone();
            let started = started.clone();
            tokio::spawn(async move {
                started.wait().await;
                runner.cancel().await;
            })
        };
        let result = runner.ensure_running(CancellationToken::new(), work).await;
        handle.await.unwrap();
        assert!(matches!(result, Err(RunnerError::Cancelled(_))));
    }

    // ------------------------------------------------------------ background jobs

    #[derive(Default)]
    struct MockJobs {
        jobs: Mutex<Vec<BackgroundJobInfo>>,
    }

    impl BackgroundJobs for MockJobs {
        fn list(&self) -> Result<Vec<BackgroundJobInfo>, CoreError> {
            Ok(self.jobs.lock().unwrap().clone())
        }

        fn cancel(&self, id: &str) -> Result<(), CoreError> {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(job) = jobs.iter_mut().find(|job| job.id == id) {
                job.status = BackgroundJobStatus::Cancelled;
            }
            Ok(())
        }
    }

    fn job(
        id: &str,
        metadata: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> BackgroundJobInfo {
        BackgroundJobInfo {
            id: id.to_string(),
            status: BackgroundJobStatus::Running,
            metadata,
        }
    }

    fn metadata_json(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_value(value).unwrap()
    }

    #[tokio::test]
    async fn cancel_background_jobs_cancels_chain() {
        let jobs = Arc::new(MockJobs::default());
        *jobs.jobs.lock().unwrap() = vec![
            job(
                "job_1",
                Some(metadata_json(serde_json::json!({ "sessionId": "ses_01" }))),
            ),
            job(
                "job_2",
                Some(metadata_json(serde_json::json!({ "sessionId": "job_1" }))),
            ),
            job(
                "job_3",
                Some(metadata_json(
                    serde_json::json!({ "parentSessionId": "job_2" }),
                )),
            ),
            job("job_4", None),
        ];
        cancel_background_jobs(&*jobs, "ses_01").unwrap();
        let jobs = jobs.jobs.lock().unwrap().clone();
        assert_eq!(jobs[0].status, BackgroundJobStatus::Cancelled);
        assert_eq!(jobs[1].status, BackgroundJobStatus::Cancelled);
        assert_eq!(jobs[2].status, BackgroundJobStatus::Cancelled);
        assert_eq!(jobs[3].status, BackgroundJobStatus::Running);
    }

    #[tokio::test]
    async fn cancel_background_jobs_ignores_finished_jobs() {
        let jobs = Arc::new(MockJobs::default());
        let mut finished = job(
            "job_1",
            Some(metadata_json(serde_json::json!({ "sessionId": "ses_01" }))),
        );
        finished.status = BackgroundJobStatus::Completed;
        *jobs.jobs.lock().unwrap() = vec![finished];
        cancel_background_jobs(&*jobs, "ses_01").unwrap();
        let jobs = jobs.jobs.lock().unwrap().clone();
        assert_eq!(jobs[0].status, BackgroundJobStatus::Completed);
    }

    // -------------------------------------------------------------- SessionRunState

    struct NoJobs;

    impl BackgroundJobs for NoJobs {
        fn list(&self) -> Result<Vec<BackgroundJobInfo>, CoreError> {
            Ok(Vec::new())
        }

        fn cancel(&self, _: &str) -> Result<(), CoreError> {
            Ok(())
        }
    }

    fn run_state() -> (Arc<SessionRunState>, Arc<SessionStatusService>) {
        let storage = crate::storage::Storage::open_in_memory().unwrap();
        let events = Arc::new(EventBus::new(storage, None));
        let status = Arc::new(SessionStatusService::new(events));
        let run_state = SessionRunState::new(Arc::new(NoJobs), status.clone());
        (run_state, status)
    }

    fn user_message(id: &str) -> WithParts {
        WithParts {
            info: V1Message::User {
                id: id.to_string(),
                session_id: "ses_01".to_string(),
                time: UserTime { created: 1.0 },
                format: None,
                summary: None,
                agent: "build".to_string(),
                model: V1UserModel {
                    provider_id: "anthropic".to_string(),
                    model_id: "claude".to_string(),
                    variant: None,
                },
                system: None,
                tools: None,
            },
            parts: Vec::new(),
        }
    }

    fn on_interrupt_never() -> Work<WithParts, SessionError> {
        Arc::new(|| Box::pin(async { unreachable!() }))
    }

    #[tokio::test]
    async fn assert_not_busy_tracks_runner_state() {
        let (run_state, _status) = run_state();
        run_state
            .assert_not_busy("ses_01")
            .expect("no runner yet, never busy");
        let started = Latch::closed();
        let work: Work<WithParts, SessionError> = {
            let started = started.clone();
            Arc::new(move || {
                let started = started.clone();
                Box::pin(async move {
                    started.open();
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Ok(user_message("msg_01"))
                })
            })
        };
        let handle = {
            let run_state = run_state.clone();
            let on_interrupt = on_interrupt_never();
            tokio::spawn(async move {
                run_state
                    .ensure_running("ses_01", CancellationToken::new(), on_interrupt, work)
                    .await
                    .unwrap()
            })
        };
        started.wait().await;
        let err = run_state.assert_not_busy("ses_01").expect_err("busy");
        assert_eq!(err.session_id, "ses_01");
        handle.await.unwrap();
        run_state
            .assert_not_busy("ses_01")
            .expect("idle after the run");
    }

    #[tokio::test]
    async fn ensure_running_sets_idle_status_when_done() {
        let (run_state, status) = run_state();
        run_state
            .ensure_running(
                "ses_01",
                CancellationToken::new(),
                on_interrupt_never(),
                box_work(user_message("msg_01")),
            )
            .await
            .unwrap();
        assert_eq!(status.get("ses_01"), SessionStatusInfo::Idle);
    }

    #[tokio::test]
    async fn start_shell_sets_busy_status_and_rejects_concurrent() {
        let (run_state, status) = run_state();
        let started = Latch::closed();
        let work: Work<WithParts, SessionError> = {
            let started = started.clone();
            Arc::new(move || {
                let started = started.clone();
                Box::pin(async move {
                    started.open();
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Ok(user_message("msg_01"))
                })
            })
        };
        let second = {
            let run_state = run_state.clone();
            let started = started.clone();
            tokio::spawn(async move {
                started.wait().await;
                run_state
                    .start_shell(
                        "ses_01",
                        on_interrupt_never(),
                        box_work(user_message("msg_02")),
                        None,
                    )
                    .await
            })
        };
        let first = run_state
            .start_shell("ses_01", on_interrupt_never(), work, None)
            .await;
        let second = second.await.unwrap();
        assert!(first.is_ok());
        assert!(matches!(second, Err(SessionError::Busy(_))));
        assert_eq!(status.get("ses_01"), SessionStatusInfo::Idle);
    }

    #[tokio::test]
    async fn cancel_without_runner_sets_idle() {
        let (run_state, status) = run_state();
        run_state.cancel("ses_02").await.unwrap();
        assert_eq!(status.get("ses_02"), SessionStatusInfo::Idle);
    }
}
