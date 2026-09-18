//! Background jobs — port of `packages/core/src/background-job.ts` (M7.2).
//!
//! A scoped, process-local registry. Entries are intentionally not durable:
//! process restart loses status and interrupts live work (background-job.ts
//! module comment). The service tracks in-flight jobs in memory keyed by id;
//! `extend`/`settle` keep the token guard so settlements from a replaced
//! job generation are ignored.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;

use crate::session::ids::JobId;
use crate::session::run_state::Deferred;
use crate::session::run_state::{BackgroundJobInfo, BackgroundJobStatus, BackgroundJobs};
use crate::{Clock, CoreError};

/// `BackgroundJob.Status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Completed,
    Error,
    Cancelled,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Completed => "completed",
            Status::Error => "error",
            Status::Cancelled => "cancelled",
        }
    }

    fn reduced(self) -> BackgroundJobStatus {
        match self {
            Status::Running => BackgroundJobStatus::Running,
            Status::Completed => BackgroundJobStatus::Completed,
            Status::Error => BackgroundJobStatus::Error,
            Status::Cancelled => BackgroundJobStatus::Cancelled,
        }
    }
}

/// `BackgroundJob.Info` — the full registry snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    pub id: String,
    pub r#type: String,
    pub title: Option<String>,
    pub status: Status,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub metadata: Option<serde_json::Map<String, Value>>,
}

/// `StartInput.run` — the job body produces its output text; the failure
/// string is the error text (`errorText(Cause.squash(cause))`).
pub type JobFuture = BoxFuture<'static, Result<String, String>>;

/// `StartInput` (background-job.ts:64-71).
pub struct StartInput {
    pub id: Option<String>,
    pub r#type: String,
    pub title: Option<String>,
    pub metadata: Option<serde_json::Map<String, Value>>,
    pub run: JobFuture,
    /// `onPromote` — `Effect.Effect<void>`, awaited by `promote`.
    pub on_promote: Option<BoxFuture<'static, ()>>,
}

/// `ExtendInput` (background-job.ts:73-76).
pub struct ExtendInput {
    pub id: String,
    pub run: JobFuture,
}

/// `WaitInput` (background-job.ts:78-81).
pub struct WaitInput {
    pub id: String,
    pub timeout: Option<Duration>,
}

/// `WaitResult` (background-job.ts:83-86).
pub struct WaitResult {
    pub info: Option<Info>,
    pub timed_out: bool,
}

/// One in-flight registry entry — the TS `Active` record.
struct Active {
    info: Info,
    done: Arc<Deferred<Info>>,
    token: u64,
    pending: usize,
    next: u64,
    output: Option<(u64, String)>,
    tail: Arc<Deferred<()>>,
    promoted: Arc<Deferred<Info>>,
    on_promote: Option<BoxFuture<'static, ()>>,
    /// `Scope` — aborting these tasks is `Scope.close`.
    tasks: Vec<tokio::task::AbortHandle>,
}

impl Active {
    fn snapshot(&self) -> Info {
        Info {
            metadata: self.info.metadata.clone(),
            ..self.info.clone()
        }
    }
}

enum Exit {
    Success(String),
    Failure(String),
}

/// `BackgroundJob.Service` (background-job.ts:120-361).
pub struct BackgroundJobService {
    clock: Arc<dyn Clock>,
    jobs: Mutex<BTreeMap<String, Active>>,
    tokens: AtomicU64,
}

impl BackgroundJobService {
    pub fn new(clock: Arc<dyn Clock>) -> Arc<BackgroundJobService> {
        Arc::new(BackgroundJobService {
            clock,
            jobs: Mutex::new(BTreeMap::new()),
            tokens: AtomicU64::new(0),
        })
    }

    fn lock_jobs(&self) -> MutexGuard<'_, BTreeMap<String, Active>> {
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `settle` (background-job.ts:126-171): complete one run generation.
    fn settle(self: &Arc<Self>, id: &str, token: u64, sequence: u64, exit: Exit) -> Option<Info> {
        let completed_at = self.clock.now_ms();
        let mut jobs = self.lock_jobs();
        let job = jobs.get_mut(id)?;
        if job.token != token {
            return None;
        }
        if job.info.status != Status::Running {
            return Some(job.snapshot());
        }
        let pending = job.pending - 1;
        let output = match (&exit, &job.output) {
            (Exit::Success(text), Some((sequence_, _))) if sequence > *sequence_ => {
                Some((sequence, text.clone()))
            }
            (Exit::Success(text), None) => Some((sequence, text.clone())),
            _ => job.output.clone(),
        };
        if matches!(exit, Exit::Success(_)) && pending > 0 {
            job.pending = pending;
            job.output = output;
            return Some(job.snapshot());
        }
        let (status, error) = match &exit {
            Exit::Success(_) => (Status::Completed, None),
            Exit::Failure(message) => (Status::Error, Some(message.clone())),
        };
        job.on_promote = None;
        job.pending = 0;
        job.output = output.clone();
        job.info.status = status;
        job.info.completed_at = Some(completed_at);
        job.info.output = output.map(|(_, text)| text);
        job.info.error = error;
        let info = job.snapshot();
        let done = job.done.clone();
        drop(jobs);
        done.complete(info.clone());
        Some(info)
    }

    /// `fork` (background-job.ts:173-188) — run to completion, then settle.
    /// `run.pipe(Effect.ensuring(Deferred.succeed(tail)))`: the tail
    /// completes before the settlement. `previous` is the awaited chain
    /// tail (`extend`), if any.
    fn spawn_run(
        self: &Arc<Self>,
        id: String,
        token: u64,
        sequence: u64,
        previous: Option<Arc<Deferred<()>>>,
        run: JobFuture,
        tail: Arc<Deferred<()>>,
    ) -> tokio::task::AbortHandle {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            if let Some(previous) = previous {
                previous.get().await;
            }
            let exit = match run.await {
                Ok(output) => Exit::Success(output),
                Err(error) => Exit::Failure(error),
            };
            tail.complete(());
            this.settle(&id, token, sequence, exit);
        })
        .abort_handle()
    }

    /// `list` (background-job.ts:190-194) — `started_at` ascending, stable.
    pub fn list(&self) -> Vec<Info> {
        let mut jobs: Vec<Info> = self.lock_jobs().values().map(Active::snapshot).collect();
        jobs.sort_by_key(|job| job.started_at);
        jobs
    }

    /// `get` (background-job.ts:196-200).
    pub fn get(&self, id: &str) -> Option<Info> {
        self.lock_jobs().get(id).map(Active::snapshot)
    }

    /// `start` (background-job.ts:202-254) — returns the running job's
    /// info without restarting an already-running job.
    pub fn start(self: &Arc<Self>, input: StartInput) -> Result<Info, CoreError> {
        let id = JobId::ascending(input.id.as_deref())?;
        let started_at = self.clock.now_ms();
        let mut jobs = self.lock_jobs();
        if let Some(existing) = jobs.get(&id) {
            if existing.info.status == Status::Running {
                return Ok(existing.snapshot());
            }
        }
        let token = self.tokens.fetch_add(1, Ordering::Relaxed);
        let tail = Arc::new(Deferred::new());
        let job = Active {
            info: Info {
                id: id.clone(),
                r#type: input.r#type,
                title: input.title,
                status: Status::Running,
                started_at,
                completed_at: None,
                output: None,
                error: None,
                metadata: input.metadata,
            },
            done: Arc::new(Deferred::new()),
            token,
            pending: 1,
            next: 1,
            output: None,
            tail: tail.clone(),
            promoted: Arc::new(Deferred::new()),
            on_promote: input.on_promote,
            tasks: Vec::new(),
        };
        let info = job.snapshot();
        jobs.insert(id.clone(), job);
        let handle = self.spawn_run(id.clone(), token, 0, None, input.run, tail);
        if let Some(job) = jobs.get_mut(&id) {
            job.tasks.push(handle);
        }
        Ok(info)
    }

    /// `extend` (background-job.ts:256-290) — queue a chained run after the
    /// current tail.
    pub fn extend(self: &Arc<Self>, input: ExtendInput) -> Result<bool, CoreError> {
        let tail = Arc::new(Deferred::new());
        let (previous, token, sequence) = {
            let mut jobs = self.lock_jobs();
            let Some(job) = jobs.get_mut(&input.id) else {
                return Ok(false);
            };
            if job.info.status != Status::Running {
                return Ok(false);
            }
            let previous = job.tail.clone();
            let token = job.token;
            let sequence = job.next;
            job.pending += 1;
            job.next += 1;
            job.tail = tail.clone();
            (previous, token, sequence)
        };
        let handle = self.spawn_run(
            input.id.clone(),
            token,
            sequence,
            Some(previous),
            input.run,
            tail,
        );
        let mut jobs = self.lock_jobs();
        if let Some(job) = jobs.get_mut(&input.id) {
            job.tasks.push(handle);
        }
        Ok(true)
    }

    /// `wait` (background-job.ts:292-301).
    pub async fn wait(&self, input: WaitInput) -> WaitResult {
        let (done, snapshot) = {
            let jobs = self.lock_jobs();
            let Some(job) = jobs.get(&input.id) else {
                return WaitResult {
                    info: None,
                    timed_out: false,
                };
            };
            if job.info.status != Status::Running {
                return WaitResult {
                    info: Some(job.snapshot()),
                    timed_out: false,
                };
            }
            (job.done.clone(), job.snapshot())
        };
        let Some(timeout) = input.timeout else {
            let info = done.get().await;
            return WaitResult {
                info,
                timed_out: false,
            };
        };
        if timeout <= Duration::ZERO {
            return WaitResult {
                info: Some(snapshot),
                timed_out: true,
            };
        }
        match tokio::time::timeout(timeout, done.get()).await {
            Ok(info) => WaitResult {
                info,
                timed_out: false,
            },
            Err(_) => WaitResult {
                info: Some(snapshot),
                timed_out: true,
            },
        }
    }

    /// `waitForPromotion` (background-job.ts:303-308) — never resolves for
    /// missing/finished jobs.
    pub async fn wait_for_promotion(self: &Arc<Self>, id: &str) -> Info {
        enum Pending {
            Ready(Info),
            Await(Arc<Deferred<Info>>),
            Never,
        }
        let pending = {
            let jobs = self.lock_jobs();
            let pending = match jobs.get(id) {
                None => Pending::Never,
                Some(job) if job.info.status != Status::Running => Pending::Never,
                Some(job)
                    if job
                        .info
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get("background"))
                        == Some(&Value::Bool(true)) =>
                {
                    Pending::Ready(job.snapshot())
                }
                Some(job) => Pending::Await(job.promoted.clone()),
            };
            pending
        };
        match pending {
            Pending::Ready(info) => info,
            Pending::Await(promoted) => promoted
                .get()
                .await
                .expect("promoted deferred completes once"),
            Pending::Never => futures::future::pending().await,
        }
    }

    /// `promote` (background-job.ts:310-335) — flip `metadata.background`,
    /// resolve the promotion waiters and run the `onPromote` effect.
    pub async fn promote(self: &Arc<Self>, id: &str) -> Option<Info> {
        let (info, promoted, on_promote) = {
            let mut jobs = self.lock_jobs();
            let job = jobs.get_mut(id)?;
            if job.info.status != Status::Running {
                return None;
            }
            if job
                .info
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("background"))
                == Some(&Value::Bool(true))
            {
                return Some(job.snapshot());
            }
            job.info
                .metadata
                .get_or_insert_with(serde_json::Map::new)
                .insert("background".to_string(), Value::Bool(true));
            let on_promote = job.on_promote.take();
            (job.snapshot(), job.promoted.clone(), on_promote)
        };
        promoted.complete(info.clone());
        if let Some(on_promote) = on_promote {
            on_promote.await;
        }
        Some(info)
    }

    /// `cancel` (background-job.ts:337-358) — settle the job as cancelled
    /// and close its scope (aborting in-flight work).
    pub fn cancel(&self, id: &str) -> Option<Info> {
        let completed_at = self.clock.now_ms();
        let (info, done, tasks) = {
            let mut jobs = self.lock_jobs();
            let job = jobs.get_mut(id)?;
            if job.info.status != Status::Running {
                return Some(job.snapshot());
            }
            job.on_promote = None;
            job.pending = 0;
            job.info.status = Status::Cancelled;
            job.info.completed_at = Some(completed_at);
            (
                job.snapshot(),
                job.done.clone(),
                std::mem::take(&mut job.tasks),
            )
        };
        done.complete(info.clone());
        for task in tasks {
            task.abort();
        }
        Some(info)
    }
}

impl BackgroundJobs for BackgroundJobService {
    fn list(&self) -> Result<Vec<BackgroundJobInfo>, CoreError> {
        Ok(self
            .lock_jobs()
            .values()
            .map(|job| BackgroundJobInfo {
                id: job.info.id.clone(),
                status: job.info.status.reduced(),
                metadata: job.info.metadata.clone(),
            })
            .collect())
    }

    fn cancel(&self, id: &str) -> Result<(), CoreError> {
        let _ = self.cancel(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedClock;

    impl Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            1_000
        }
    }

    fn service() -> Arc<BackgroundJobService> {
        BackgroundJobService::new(Arc::new(FixedClock))
    }

    fn immediate(text: &str) -> JobFuture {
        let text = text.to_string();
        Box::pin(async move { Ok(text) })
    }

    fn running(run: JobFuture) -> StartInput {
        StartInput {
            id: None,
            r#type: "task".to_string(),
            title: None,
            metadata: None,
            run,
            on_promote: None,
        }
    }

    fn task_metadata(session: &str) -> Option<serde_json::Map<String, Value>> {
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "parentSessionId".to_string(),
            Value::String(session.to_string()),
        );
        Some(metadata)
    }

    #[tokio::test]
    async fn start_settles_to_completed() {
        let jobs = service();
        let info = jobs
            .start(StartInput {
                title: Some("A job".to_string()),
                metadata: task_metadata("ses_1"),
                ..running(immediate("done"))
            })
            .unwrap();
        assert_eq!(info.status, Status::Running);
        assert!(info.id.starts_with("job_"));
        let result = jobs
            .wait(WaitInput {
                id: info.id.clone(),
                timeout: None,
            })
            .await;
        assert!(!result.timed_out);
        let info = result.info.unwrap();
        assert_eq!(info.status, Status::Completed);
        assert_eq!(info.output.as_deref(), Some("done"));
        assert_eq!(info.completed_at, Some(1_000));
        assert_eq!(jobs.get(&info.id).unwrap().status, Status::Completed);
    }

    #[tokio::test]
    async fn start_failure_sets_error_status() {
        let jobs = service();
        let info = jobs
            .start(running(Box::pin(async { Err("boom".to_string()) })))
            .unwrap();
        let result = jobs
            .wait(WaitInput {
                id: info.id,
                timeout: None,
            })
            .await;
        assert_eq!(result.info.unwrap().status, Status::Error);
    }

    #[tokio::test]
    async fn start_with_same_id_does_not_restart_running_job() {
        let jobs = service();
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let held = gate.clone();
        let first = jobs
            .start(StartInput {
                id: Some("job_1".to_string()),
                run: Box::pin(async move {
                    let _guard = held.lock().await;
                    Ok("first".to_string())
                }),
                ..running(immediate("second"))
            })
            .unwrap();
        let second = jobs
            .start(StartInput {
                id: Some("job_1".to_string()),
                ..running(immediate("second"))
            })
            .unwrap();
        assert_eq!(first.id, "job_1");
        assert_eq!(second.started_at, first.started_at);
        drop(gate);
        let result = jobs
            .wait(WaitInput {
                id: "job_1".to_string(),
                timeout: None,
            })
            .await;
        assert_eq!(result.info.unwrap().output.as_deref(), Some("first"));
    }

    #[tokio::test]
    async fn extend_chains_after_the_tail() {
        let jobs = service();
        let info = jobs
            .start(StartInput {
                id: Some("job_1".to_string()),
                ..running(immediate("first"))
            })
            .unwrap();
        assert!(jobs
            .extend(ExtendInput {
                id: info.id.clone(),
                run: immediate("second"),
            })
            .unwrap());
        let result = jobs
            .wait(WaitInput {
                id: info.id,
                timeout: None,
            })
            .await;
        assert_eq!(result.info.unwrap().output.as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn extend_missing_or_finished_job_returns_false() {
        let jobs = service();
        assert!(!jobs
            .extend(ExtendInput {
                id: "job_none".to_string(),
                run: immediate("x"),
            })
            .unwrap());
        let info = jobs
            .start(StartInput {
                id: Some("job_1".to_string()),
                ..running(immediate("x"))
            })
            .unwrap();
        let _ = jobs
            .wait(WaitInput {
                id: info.id.clone(),
                timeout: None,
            })
            .await;
        assert!(!jobs
            .extend(ExtendInput {
                id: info.id,
                run: immediate("y"),
            })
            .unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn wait_times_out() {
        let jobs = service();
        let info = jobs
            .start(StartInput {
                id: Some("job_1".to_string()),
                ..running(Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok("late".to_string())
                }))
            })
            .unwrap();
        let result = jobs
            .wait(WaitInput {
                id: info.id.clone(),
                timeout: Some(Duration::ZERO),
            })
            .await;
        assert!(result.timed_out);
        assert_eq!(result.info.unwrap().status, Status::Running);
        let result = jobs
            .wait(WaitInput {
                id: info.id.clone(),
                timeout: Some(Duration::from_millis(10)),
            })
            .await;
        assert!(result.timed_out);
        let result = jobs
            .wait(WaitInput {
                id: info.id,
                timeout: None,
            })
            .await;
        assert!(!result.timed_out);
        // Unbounded waits see the settlement.
        assert_eq!(result.info.unwrap().status, Status::Completed);
    }

    #[tokio::test]
    async fn promote_flips_metadata_and_resolves_waiters() {
        let jobs = service();
        let info = jobs
            .start(StartInput {
                id: Some("job_1".to_string()),
                metadata: task_metadata("ses_1"),
                ..running(Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok("late".to_string())
                }))
            })
            .unwrap();
        let promoted = jobs.promote(&info.id).await.unwrap();
        assert_eq!(
            promoted.metadata.as_ref().unwrap().get("background"),
            Some(&Value::Bool(true)),
        );
        // Already promoted jobs promote again idempotently.
        assert!(jobs.promote(&info.id).await.is_some());
        // waitForPromotion resolves for background jobs.
        let waiter = jobs.wait_for_promotion(&info.id);
        assert!(tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .is_ok());
        jobs.cancel(&info.id);
    }

    #[tokio::test]
    async fn promote_runs_on_promote_once() {
        let jobs = service();
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ran_clone = ran.clone();
        let info = jobs
            .start(StartInput {
                id: Some("job_1".to_string()),
                on_promote: Some(Box::pin(async move {
                    ran_clone.fetch_add(1, Ordering::Relaxed);
                })),
                ..running(Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok("late".to_string())
                }))
            })
            .unwrap();
        jobs.promote(&info.id).await.unwrap();
        assert_eq!(ran.load(Ordering::Relaxed), 1);
        // onPromote is cleared — the second promote doesn't re-run it.
        jobs.promote(&info.id).await.unwrap();
        assert_eq!(ran.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cancel_settles_as_cancelled() {
        let jobs = service();
        let info = jobs
            .start(StartInput {
                id: Some("job_1".to_string()),
                ..running(Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok("late".to_string())
                }))
            })
            .unwrap();
        let cancelled = jobs.cancel(&info.id).unwrap();
        assert_eq!(cancelled.status, Status::Cancelled);
        assert_eq!(cancelled.completed_at, Some(1_000));
        // A finished job returns its info; missing jobs return None.
        assert_eq!(jobs.cancel(&info.id).unwrap().status, Status::Cancelled);
        assert!(jobs.cancel("job_none").is_none());
        // The done deferred resolves with the cancelled info.
        let result = jobs
            .wait(WaitInput {
                id: info.id,
                timeout: None,
            })
            .await;
        assert_eq!(result.info.unwrap().status, Status::Cancelled);
    }

    #[tokio::test]
    async fn background_jobs_trait_impl() {
        let jobs = service();
        let info = jobs
            .start(StartInput {
                id: Some("job_1".to_string()),
                metadata: task_metadata("ses_1"),
                ..running(Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok("late".to_string())
                }))
            })
            .unwrap();
        let reduced = BackgroundJobs::list(&*jobs).unwrap();
        assert_eq!(reduced.len(), 1);
        assert_eq!(reduced[0].id, info.id);
        assert_eq!(reduced[0].status, BackgroundJobStatus::Running);
        assert_eq!(reduced[0].metadata, info.metadata);
        BackgroundJobs::cancel(&*jobs, &info.id).unwrap();
        assert_eq!(
            BackgroundJobs::list(&*jobs).unwrap()[0].status,
            BackgroundJobStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn list_sorts_by_started_at() {
        struct SteppedClock(std::sync::atomic::AtomicU64);

        impl Clock for SteppedClock {
            fn now_ms(&self) -> u64 {
                1_000 + self.0.fetch_add(1, Ordering::Relaxed)
            }
        }

        let jobs =
            BackgroundJobService::new(Arc::new(SteppedClock(std::sync::atomic::AtomicU64::new(0))));
        for id in ["job_c", "job_a", "job_b"] {
            jobs.start(StartInput {
                id: Some(id.to_string()),
                ..running(Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok("late".to_string())
                }))
            })
            .unwrap();
        }
        assert_eq!(
            jobs.list()
                .iter()
                .map(|job| job.id.clone())
                .collect::<Vec<_>>(),
            vec!["job_c", "job_a", "job_b"],
        );
        assert_eq!(
            jobs.list()
                .iter()
                .map(|job| job.started_at)
                .collect::<Vec<_>>(),
            vec![1_000, 1_001, 1_002],
        );
    }
}
