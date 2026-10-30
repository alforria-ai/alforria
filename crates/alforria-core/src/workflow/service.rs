//! Workflow runs: lifecycle, fan-out, persistence, and the progress model.
//!
//! The script runs on a dedicated current-thread Tokio runtime so the QuickJS
//! context (which is not `Send`) never leaves its thread. Agent work runs on
//! the runtime that started the run: session loops are spawned where they are
//! prompted, and must not die with the script's runtime when the script ends.
//!
//! Runs outlive the service that started them, because an instance re-boot
//! builds a new service while run threads are still alive. So the run table
//! belongs to the state directory, once per process. A lock file marks a run
//! as owned by a live process for any other process sharing the directory.

use std::collections::HashMap;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alforria_schema::workflow::{
    WorkflowAgent, WorkflowDefinition, WorkflowLog, WorkflowPhase, WorkflowRun,
    WorkflowSaveRequest, WorkflowSource, WorkflowStartRequest, WorkflowState,
};
use serde_json::{json, Value};
use tokio::runtime::Handle;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::session::retry::{resume_note, wait_to_resume, AUTO_RESUME_DELAYS};
use crate::tool::def::BoxFuture;

use super::definition::{self, parse_meta, rewrite_meta, valid_name};
use super::runtime::{self, WorkflowHost};
use super::{ResolvedWorkflow, WorkflowAgentRequest, WorkflowError, WorkflowOps, WorkflowSettings};

const MAX_LOGS: usize = 500;
/// Finished runs kept per directory.
const MAX_RUNS: usize = 100;
/// Progress reaches disk at most this often; terminal states always do.
const PERSIST_INTERVAL: Duration = Duration::from_millis(500);

struct RunState {
    run: Mutex<WorkflowRun>,
    abort: CancellationToken,
    notify: Notify,
    /// Held while this process runs the workflow; see the module docs.
    owner: Mutex<Option<File>>,
    /// Another live process runs this workflow; this copy only mirrors its file.
    foreign: AtomicBool,
    /// Serializes writers of the run's file, and paces progress writes.
    persisted: Mutex<Option<Instant>>,
}

impl RunState {
    fn new(run: WorkflowRun) -> Self {
        RunState {
            run: Mutex::new(run),
            abort: CancellationToken::new(),
            notify: Notify::new(),
            owner: Mutex::new(None),
            foreign: AtomicBool::new(false),
            persisted: Mutex::new(None),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WorkflowRun> {
        self.run
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

type RunTable = Arc<Mutex<HashMap<String, Arc<RunState>>>>;

static RUN_TABLES: LazyLock<Mutex<HashMap<PathBuf, RunTable>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The workflow service for one instance directory.
pub struct WorkflowService {
    ops: Arc<dyn WorkflowOps>,
    settings: WorkflowSettings,
    runs: RunTable,
    semaphore: Arc<Semaphore>,
    run_dir: PathBuf,
}

impl WorkflowService {
    pub fn new(ops: Arc<dyn WorkflowOps>, settings: WorkflowSettings) -> Arc<Self> {
        let run_dir = settings.state.join("workflows").join("runs");
        let (runs, first) = {
            let mut tables = RUN_TABLES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match tables.get(&run_dir) {
                Some(runs) => (runs.clone(), false),
                None => {
                    let runs = RunTable::default();
                    tables.insert(run_dir.clone(), runs.clone());
                    (runs, true)
                }
            }
        };
        let service = Arc::new(WorkflowService {
            ops,
            semaphore: Arc::new(Semaphore::new(settings.max_concurrent())),
            settings,
            runs,
            run_dir,
        });
        if first {
            service.load_runs();
        }
        service
    }

    pub fn enabled(&self) -> bool {
        self.settings.enabled
    }

    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<RunState>>> {
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Saved workflows visible from this instance.
    pub fn definitions(&self) -> Vec<WorkflowDefinition> {
        definition::discover(&self.settings)
    }

    /// Save a workflow script to the project or user directory. Editing a
    /// workflow writes back to the file it came from, wherever that is, so
    /// the edit is what runs next. Anything else that would replace a saved
    /// workflow needs `overwrite`.
    pub fn save(&self, request: WorkflowSaveRequest) -> Result<WorkflowDefinition, WorkflowError> {
        if !self.settings.enabled {
            return Err(WorkflowError::Disabled);
        }
        if !valid_name(&request.name) {
            return Err(WorkflowError::Invalid(
                "invalid workflow name: use letters, digits, '-', '_' or '.'".to_string(),
            ));
        }
        let scope = request.scope.as_deref().unwrap_or("project");
        let (dir, source) = match scope {
            "global" => (
                self.settings.config.join("workflows"),
                WorkflowSource::Global,
            ),
            "project" => (
                self.settings.directory.join(".opencode").join("workflows"),
                WorkflowSource::Project,
            ),
            other => {
                return Err(WorkflowError::Invalid(format!(
                    "unknown workflow scope: {other}"
                )))
            }
        };
        let existing = self.definitions().into_iter().find(|workflow| {
            workflow.name == request.name
                && matches!(
                    workflow.source,
                    WorkflowSource::Project | WorkflowSource::Global
                )
        });
        if let Some(existing) = &existing {
            if !request.overwrite {
                return Err(WorkflowError::Conflict(format!(
                    "a workflow named {} already exists",
                    request.name
                )));
            }
            if existing.source == WorkflowSource::Project && source == WorkflowSource::Global {
                return Err(WorkflowError::Conflict(format!(
                    "the project workflow {} would still override a user one",
                    request.name
                )));
            }
        }
        let path = match existing {
            Some(existing) if existing.source == source => PathBuf::from(existing.path),
            _ => dir.join(format!("{}.js", request.name)),
        };
        let script = rewrite_meta(
            &request.script,
            &request.name,
            request.description.as_deref(),
        )
        .map_err(WorkflowError::Invalid)?;
        let failed = |err: std::io::Error| WorkflowError::Failed(err.to_string());
        let parent = path.parent().unwrap_or(&dir);
        std::fs::create_dir_all(parent).map_err(failed)?;
        let temp = parent.join(format!(".{}.js.tmp", request.name));
        std::fs::write(&temp, &script).map_err(failed)?;
        std::fs::rename(&temp, &path).map_err(failed)?;
        let meta = parse_meta(&script);
        Ok(WorkflowDefinition {
            name: request.name,
            description: meta.description,
            path: path.to_string_lossy().to_string(),
            source,
            phases: meta.phases,
            script,
        })
    }

    /// Delete a project workflow by name.
    pub fn delete(&self, name: &str) -> Result<bool, WorkflowError> {
        let Some(workflow) = self
            .definitions()
            .into_iter()
            .find(|workflow| workflow.name == name)
        else {
            return Ok(false);
        };
        if !matches!(
            workflow.source,
            WorkflowSource::Project | WorkflowSource::Global
        ) {
            return Err(WorkflowError::Invalid(
                "only saved workflows can be deleted".to_string(),
            ));
        }
        std::fs::remove_file(&workflow.path)
            .map_err(|err| WorkflowError::Failed(err.to_string()))?;
        Ok(true)
    }

    /// Start a run and return its first snapshot. The run continues in the
    /// background on its own thread.
    pub fn start(
        self: &Arc<Self>,
        request: WorkflowStartRequest,
    ) -> Result<WorkflowRun, WorkflowError> {
        if !self.settings.enabled {
            return Err(WorkflowError::Disabled);
        }
        if let Some(session) = &request.session_id {
            if self.ops.is_workflow_agent(session) {
                return Err(WorkflowError::Invalid(
                    "workflow agents cannot start workflows".to_string(),
                ));
            }
        }
        let handle = Handle::try_current().map_err(|err| WorkflowError::Failed(err.to_string()))?;
        let resolved = self.resolve(&request)?;
        let run_id = Ulid::new().to_string();
        let now = now_ms();
        let run = WorkflowRun {
            id: run_id.clone(),
            name: resolved.name,
            description: resolved.description,
            directory: self.settings.directory.to_string_lossy().to_string(),
            session_id: request.session_id.clone(),
            state: WorkflowState::Queued,
            args: normalize_args(request.args),
            script: resolved.script,
            phases: Vec::new(),
            agents: Vec::new(),
            logs: Vec::new(),
            result: None,
            error: None,
            started: now,
            finished: None,
        };
        let state = Arc::new(RunState::new(run.clone()));
        *state
            .owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = self.take_ownership(&run_id);
        self.table().insert(run_id.clone(), state.clone());
        self.persist(&state);
        self.prune_runs();

        let service = Arc::clone(self);
        let thread_state = state.clone();
        let script = run.script.clone();
        let args = run.args.clone();
        let parent = request.session_id.clone();
        let title = run.name.clone();
        let thread_name = format!("workflow-{run_id}");
        let spawned = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let state = thread_state;
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let Ok(runtime) = runtime else {
                    let error = "workflow runtime could not start".to_string();
                    service.finish(&state, Err(error));
                    return;
                };
                let runner = Arc::clone(&service);
                let result = runtime.block_on(runner.run(
                    state.clone(),
                    script,
                    args,
                    parent,
                    title,
                    handle,
                ));
                service.finish(&state, result);
            });
        if let Err(err) = spawned {
            self.finish(&state, Err(err.to_string()));
            return Err(WorkflowError::Failed(err.to_string()));
        }
        Ok(run)
    }

    fn lock_path(&self, run_id: &str) -> PathBuf {
        self.run_dir.join(format!("{run_id}.lock"))
    }

    /// Hold the run's lock file. Failing to is not fatal: the lock only tells
    /// other processes the run is alive.
    fn take_ownership(&self, run_id: &str) -> Option<File> {
        std::fs::create_dir_all(&self.run_dir).ok()?;
        let file = File::create(self.lock_path(run_id)).ok()?;
        file.try_lock().ok()?;
        Some(file)
    }

    /// Whether another live process holds this run's lock.
    fn owned_elsewhere(&self, run_id: &str) -> bool {
        let Ok(file) = File::open(self.lock_path(run_id)) else {
            return false;
        };
        matches!(file.try_lock(), Err(TryLockError::WouldBlock))
    }

    fn release_ownership(&self, state: &RunState, run_id: &str) {
        let owner = state
            .owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if owner.is_some() {
            let _ = std::fs::remove_file(self.lock_path(run_id));
        }
    }

    /// Every run in this directory or below it, newest first. The UI lists
    /// a project by its worktree, and a session may run in a subdirectory.
    pub fn runs(&self) -> Vec<WorkflowRun> {
        self.refresh_foreign();
        let root = &self.settings.directory;
        let mut runs: Vec<WorkflowRun> = self
            .table()
            .values()
            .map(snapshot)
            .filter(|run| {
                let directory = Path::new(&run.directory);
                directory == root || (root.parent().is_some() && directory.starts_with(root))
            })
            .collect();
        runs.sort_by(|a, b| b.started.total_cmp(&a.started));
        runs
    }

    pub fn get(&self, run_id: &str) -> Option<WorkflowRun> {
        self.refresh_foreign();
        self.table().get(run_id).map(snapshot)
    }

    /// The newest run that belongs to a session (for the tool's `status`
    /// action without an explicit run id).
    pub fn latest_for_session(&self, session_id: &str) -> Option<WorkflowRun> {
        self.runs()
            .into_iter()
            .find(|run| run.session_id.as_deref() == Some(session_id))
    }

    fn state_of(&self, run_id: &str) -> Result<Arc<RunState>, WorkflowError> {
        let state =
            self.table().get(run_id).cloned().ok_or_else(|| {
                WorkflowError::NotFound(format!("workflow run not found: {run_id}"))
            })?;
        if state.foreign.load(Ordering::Relaxed) {
            return Err(WorkflowError::Conflict(format!(
                "workflow run {run_id} belongs to another alforria process"
            )));
        }
        Ok(state)
    }

    /// Stop a run and cancel its running agents. A finished run stays as it
    /// finished.
    pub async fn abort(&self, run_id: &str) -> Result<WorkflowRun, WorkflowError> {
        let state = self.state_of(run_id)?;
        {
            let mut run = state.lock();
            if run.state.is_terminal() {
                return Ok(run.clone());
            }
            let finished = now_ms();
            for agent in &mut run.agents {
                if !agent.state.is_terminal() {
                    agent.state = WorkflowState::Cancelled;
                    agent.finished = Some(finished);
                }
            }
            for phase in &mut run.phases {
                if !phase.state.is_terminal() {
                    phase.state = WorkflowState::Cancelled;
                    phase.finished = Some(finished);
                }
            }
            run.state = WorkflowState::Cancelled;
            run.error = Some("workflow cancelled".to_string());
            run.finished = Some(finished);
        }
        // The agent tasks watch this token and cancel their own sessions.
        state.abort.cancel();
        self.persist(&state);
        state.notify.notify_waiters();
        Ok(snapshot(&state))
    }

    /// Wait for a run to reach a terminal state.
    pub async fn wait(&self, run_id: &str) -> Result<WorkflowRun, WorkflowError> {
        let state = self.state_of(run_id)?;
        loop {
            let notified = state.notify.notified();
            let run = snapshot(&state);
            if run.state.is_terminal() {
                return Ok(run);
            }
            notified.await;
        }
    }

    fn resolve(&self, request: &WorkflowStartRequest) -> Result<ResolvedWorkflow, WorkflowError> {
        if let Some(name) = request.name.as_deref() {
            if let Some(found) = self
                .definitions()
                .into_iter()
                .find(|workflow| workflow.name == name)
            {
                return Ok(ResolvedWorkflow::from(&found));
            }
        }
        let Some(script) = request.script.clone() else {
            return Err(match request.name.as_deref() {
                Some(name) => WorkflowError::NotFound(format!("workflow not found: {name}")),
                None => WorkflowError::Invalid("a workflow name or script is required".to_string()),
            });
        };
        let meta = parse_meta(&script);
        let name = request
            .name
            .clone()
            .or(meta.name)
            .unwrap_or_else(|| "inline".to_string());
        if !valid_name(&name) {
            return Err(WorkflowError::Invalid("invalid workflow name".to_string()));
        }
        Ok(ResolvedWorkflow {
            name,
            description: request.description.clone().or(meta.description),
            script,
        })
    }

    async fn run(
        self: Arc<Self>,
        state: Arc<RunState>,
        script: String,
        args: Value,
        parent_session: Option<String>,
        title: String,
        handle: Handle,
    ) -> Result<Value, String> {
        let run_id = snapshot(&state).id;
        let parent = match parent_session {
            Some(session) => session,
            None => self
                .ops
                .create_run_session(&run_id, &format!("workflow: {title}"))
                .await
                .map_err(|error| format!("could not create the workflow session: {error}"))?,
        };
        {
            let mut run = state.lock();
            if run.state.is_terminal() {
                return Err("workflow cancelled".to_string());
            }
            run.session_id = Some(parent.clone());
            run.state = WorkflowState::Running;
        }
        self.persist(&state);

        let host = Arc::new(RunHost {
            service: Arc::clone(&self),
            state: Arc::clone(&state),
            parent,
            phase: Mutex::new("Workflow".to_string()),
            handle,
        });
        tokio::select! {
            _ = state.abort.cancelled() => Err("workflow cancelled".to_string()),
            result = runtime::execute(host, &script, args) => result,
        }
    }

    /// Record the script's end. Agents still in flight no longer have anyone
    /// to report to, so they are stopped.
    fn finish(&self, state: &Arc<RunState>, result: Result<Value, String>) {
        let run_id = {
            let mut run = state.lock();
            if !run.state.is_terminal() {
                let finished = now_ms();
                match result {
                    Ok(value) => {
                        run.state = WorkflowState::Completed;
                        run.result = Some(value);
                    }
                    Err(error) => {
                        run.state = WorkflowState::Failed;
                        run.error = Some(error);
                    }
                }
                let final_state = run.state;
                for phase in &mut run.phases {
                    if !phase.state.is_terminal() {
                        phase.state = final_state;
                        phase.finished = Some(finished);
                    }
                }
                for agent in &mut run.agents {
                    if !agent.state.is_terminal() {
                        agent.state = WorkflowState::Cancelled;
                        agent.error = Some("the workflow ended before this agent".to_string());
                        agent.finished = Some(finished);
                    }
                }
                run.finished = Some(finished);
            }
            run.id.clone()
        };
        state.abort.cancel();
        self.persist(state);
        self.release_ownership(state, &run_id);
        state.notify.notify_waiters();
    }

    fn ensure_phase(&self, state: &Arc<RunState>, title: &str) {
        let mut run = state.lock();
        if run.phases.iter().any(|phase| phase.title == title) {
            return;
        }
        run.phases.push(WorkflowPhase {
            title: title.to_string(),
            state: WorkflowState::Running,
            started: now_ms(),
            finished: None,
        });
        drop(run);
        self.persist_soon(state);
    }

    fn set_phase(&self, state: &Arc<RunState>, title: &str) {
        {
            let mut run = state.lock();
            for phase in &mut run.phases {
                if phase.state == WorkflowState::Running {
                    phase.state = WorkflowState::Completed;
                    phase.finished = Some(now_ms());
                }
            }
            // Coming back to a phase reopens it.
            if let Some(phase) = run.phases.iter_mut().find(|phase| phase.title == title) {
                phase.state = WorkflowState::Running;
                phase.finished = None;
            }
        }
        self.ensure_phase(state, title);
    }

    fn add_log(&self, state: &Arc<RunState>, message: String) {
        {
            let mut run = state.lock();
            run.logs.push(WorkflowLog {
                time: now_ms(),
                message,
            });
            if run.logs.len() > MAX_LOGS {
                let drop = run.logs.len() - MAX_LOGS;
                run.logs.drain(0..drop);
            }
        }
        self.persist_soon(state);
    }

    async fn run_agent(
        self: Arc<Self>,
        state: Arc<RunState>,
        handle: Handle,
        parent_session_id: String,
        phase: String,
        prompt: String,
        options: Value,
    ) -> Result<String, String> {
        let state = &state;
        if state.abort.is_cancelled() {
            return Err("workflow cancelled".to_string());
        }
        let label = options
            .get("label")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| short_label(&prompt));
        let agent = options
            .get("agent")
            .or_else(|| options.get("subagent_type"))
            .or_else(|| options.get("subagentType"))
            .and_then(Value::as_str)
            .unwrap_or("general")
            .to_string();
        let schema = options
            .get("schema")
            .cloned()
            .filter(|schema| !schema.is_null());
        let agent_id = Ulid::new().to_string();
        {
            let mut run = state.lock();
            if run.agents.len() >= self.settings.max_agents() {
                return Err(format!(
                    "workflow agent limit reached ({} agents)",
                    self.settings.max_agents()
                ));
            }
            run.agents.push(WorkflowAgent {
                id: agent_id.clone(),
                session_id: None,
                phase: phase.clone(),
                label: label.clone(),
                agent: agent.clone(),
                prompt: prompt.clone(),
                state: WorkflowState::Queued,
                result: None,
                error: None,
                started: now_ms(),
                finished: None,
            });
        }
        self.ensure_phase(state, &phase);

        let permit = tokio::select! {
            _ = state.abort.cancelled() => {
                self.update_agent(state, &agent_id, |agent| {
                    agent.state = WorkflowState::Cancelled;
                    agent.finished = Some(now_ms());
                });
                return Err("workflow cancelled".to_string());
            }
            permit = Arc::clone(&self.semaphore).acquire_owned() => {
                permit.map_err(|_| "workflow runtime stopped".to_string())?
            }
        };
        if state.abort.is_cancelled() {
            self.update_agent(state, &agent_id, |agent| {
                agent.state = WorkflowState::Cancelled;
                agent.finished = Some(now_ms());
            });
            return Err("workflow cancelled".to_string());
        }

        self.update_agent(state, &agent_id, |agent| {
            agent.state = WorkflowState::Running;
            agent.started = now_ms();
        });
        let request = WorkflowAgentRequest {
            run_id: state.lock().id.clone(),
            parent_session_id,
            title: format!("{} (@{agent} workflow agent)", label),
            agent: agent.clone(),
            prompt: prompt.clone(),
        };
        let worker = handle.spawn(self.clone().agent_task(
            state.clone(),
            agent_id.clone(),
            request,
            permit,
            schema,
        ));
        let outcome = worker
            .await
            .unwrap_or_else(|_| Err("the server stopped before this agent finished".to_string()));
        if state.abort.is_cancelled() {
            self.update_agent(state, &agent_id, |agent| {
                agent.state = WorkflowState::Cancelled;
                agent.finished = Some(now_ms());
            });
            return Err("workflow cancelled".to_string());
        }
        match outcome {
            Ok(outcome) => {
                if let Some(error) = outcome.error {
                    self.update_agent(state, &agent_id, |agent| {
                        agent.state = WorkflowState::Failed;
                        agent.error = Some(error.clone());
                        agent.finished = Some(now_ms());
                    });
                    Err(error)
                } else {
                    self.update_agent(state, &agent_id, |agent| {
                        agent.state = WorkflowState::Completed;
                        agent.result = Some(outcome.text.clone());
                        agent.finished = Some(now_ms());
                    });
                    Ok(outcome.text)
                }
            }
            Err(error) => {
                self.update_agent(state, &agent_id, |agent| {
                    agent.state = WorkflowState::Failed;
                    agent.error = Some(error.clone());
                    agent.finished = Some(now_ms());
                });
                Err(error)
            }
        }
    }

    /// One agent's session work, on the runtime that started the run. It holds
    /// its concurrency slot until its session has really stopped.
    async fn agent_task(
        self: Arc<Self>,
        state: Arc<RunState>,
        agent_id: String,
        request: WorkflowAgentRequest,
        permit: OwnedSemaphorePermit,
        schema: Option<Value>,
    ) -> Result<super::WorkflowAgentOutcome, String> {
        let _permit = permit;
        let session_id = self.ops.create_agent_session(&request).await?;
        self.update_agent(&state, &agent_id, |agent| {
            agent.session_id = Some(session_id.clone());
        });
        tokio::select! {
            biased;
            _ = state.abort.cancelled() => {
                self.ops.cancel_agent(&session_id).await;
                Err("workflow cancelled".to_string())
            }
            outcome = self.prompt_recovering(&session_id, &request, schema.as_ref()) => outcome,
        }
    }

    /// Prompt the agent, resuming it while it stops on recoverable errors, so
    /// one bad response doesn't fail it (and, often, the whole script).
    async fn prompt_recovering(
        &self,
        session_id: &str,
        request: &WorkflowAgentRequest,
        schema: Option<&Value>,
    ) -> Result<super::WorkflowAgentOutcome, String> {
        let agent = &request.agent;
        let mut cancels = self.ops.cancel_count(session_id);
        let mut outcome = self
            .ops
            .prompt_agent(session_id, agent, &request.prompt, schema)
            .await?;
        for delay in AUTO_RESUME_DELAYS {
            let Some(error) = outcome.error.clone().filter(|_| outcome.recoverable) else {
                break;
            };
            // The session is idle now, so a stop has nothing to interrupt: it
            // only shows here.
            if !wait_to_resume(delay, || self.ops.cancel_count(session_id) != cancels).await {
                outcome.error = Some(format!(
                    "Stopped while waiting to resume after an error: {error}"
                ));
                outcome.recoverable = false;
                break;
            }
            cancels = self.ops.cancel_count(session_id);
            outcome = self
                .ops
                .resume_agent(session_id, agent, &resume_note(&error), schema)
                .await?;
        }
        Ok(outcome)
    }

    fn update_agent(
        &self,
        state: &Arc<RunState>,
        agent_id: &str,
        update: impl FnOnce(&mut WorkflowAgent),
    ) {
        {
            let mut run = state.lock();
            if let Some(agent) = run.agents.iter_mut().find(|agent| agent.id == agent_id) {
                // A stopped run has already settled its agents.
                if !agent.state.is_terminal() {
                    update(agent);
                }
            }
        }
        self.persist_soon(state);
    }

    /// [`Self::persist`], at most once per [`PERSIST_INTERVAL`] while running.
    fn persist_soon(&self, state: &Arc<RunState>) {
        let due = state
            .persisted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_none_or(|last| last.elapsed() >= PERSIST_INTERVAL);
        if due {
            self.persist(state);
        }
    }

    fn persist(&self, state: &Arc<RunState>) {
        if std::fs::create_dir_all(&self.run_dir).is_err() {
            return;
        }
        let mut persisted = state
            .persisted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (id, bytes) = {
            let run = state.lock();
            (run.id.clone(), serde_json::to_vec(&*run))
        };
        let Ok(bytes) = bytes else {
            return;
        };
        let path = self.run_dir.join(format!("{id}.json"));
        let temp = self.run_dir.join(format!(".{id}.json.tmp"));
        if std::fs::write(&temp, bytes).is_ok() {
            let _ = std::fs::rename(&temp, &path);
        }
        *persisted = Some(Instant::now());
    }

    /// Fail a run whose process is gone, everything inside it included.
    fn mark_dead(run: &mut WorkflowRun) {
        let finished = now_ms();
        run.state = WorkflowState::Failed;
        run.error = Some("server restarted before the workflow finished".to_string());
        run.finished = Some(finished);
        for phase in &mut run.phases {
            if !phase.state.is_terminal() {
                phase.state = WorkflowState::Failed;
                phase.finished = Some(finished);
            }
        }
        for agent in &mut run.agents {
            if !agent.state.is_terminal() {
                agent.state = WorkflowState::Failed;
                agent.finished = Some(finished);
            }
        }
    }

    fn read_run(&self, run_id: &str) -> Option<WorkflowRun> {
        let bytes = std::fs::read(self.run_dir.join(format!("{run_id}.json"))).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Runs persisted by earlier processes, every directory's, once per
    /// process. Only this process's first service gets here, so an active run
    /// is either held by another live process or dead.
    fn load_runs(&self) {
        let Ok(entries) = std::fs::read_dir(&self.run_dir) else {
            return;
        };
        let mut runs: Vec<WorkflowRun> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(run) = serde_json::from_slice::<WorkflowRun>(&bytes) else {
                continue;
            };
            runs.push(run);
        }
        let mut table = self.table();
        for mut run in runs {
            let id = run.id.clone();
            let active = run.is_active();
            let foreign = active && self.owned_elsewhere(&id);
            if active && !foreign {
                Self::mark_dead(&mut run);
                let _ = std::fs::remove_file(self.lock_path(&id));
            }
            let state = Arc::new(RunState::new(run));
            state.foreign.store(foreign, Ordering::Relaxed);
            if active && !foreign {
                self.persist(&state);
            }
            table.insert(id, state);
        }
        drop(table);
        self.prune_runs();
    }

    /// Mirror the runs another process owns, and take over the ones it left
    /// unfinished when it died.
    fn refresh_foreign(&self) {
        let foreign: Vec<Arc<RunState>> = self
            .table()
            .values()
            .filter(|state| state.foreign.load(Ordering::Relaxed))
            .cloned()
            .collect();
        for state in foreign {
            let id = state.lock().id.clone();
            let alive = self.owned_elsewhere(&id);
            // Read after checking the lock: a run that just ended is on disk.
            if let Some(mut run) = self.read_run(&id) {
                let dead = !alive && run.is_active();
                if dead {
                    Self::mark_dead(&mut run);
                }
                let settled = run.state.is_terminal();
                *state.lock() = run;
                if dead {
                    self.persist(&state);
                }
                if settled {
                    state.foreign.store(false, Ordering::Relaxed);
                    let _ = std::fs::remove_file(self.lock_path(&id));
                }
            }
        }
    }

    /// Keep each directory's newest finished runs.
    fn prune_runs(&self) {
        let mut table = self.table();
        let mut finished: HashMap<String, Vec<(f64, String)>> = HashMap::new();
        for (id, state) in table.iter() {
            let run = state.lock();
            if run.state.is_terminal() {
                finished
                    .entry(run.directory.clone())
                    .or_default()
                    .push((run.started, id.clone()));
            }
        }
        for mut runs in finished.into_values() {
            runs.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (_, id) in runs.into_iter().skip(MAX_RUNS) {
                table.remove(&id);
                let _ = std::fs::remove_file(self.run_dir.join(format!("{id}.json")));
            }
        }
    }
}

struct RunHost {
    service: Arc<WorkflowService>,
    state: Arc<RunState>,
    parent: String,
    phase: Mutex<String>,
    handle: Handle,
}

impl WorkflowHost for RunHost {
    fn phase(&self, title: String) {
        self.service.set_phase(&self.state, &title);
        *self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = title;
    }

    fn log(&self, message: String) {
        self.service.add_log(&self.state, message);
    }

    fn agent(&self, prompt: String, options: Value) -> BoxFuture<'static, Result<String, String>> {
        let service = Arc::clone(&self.service);
        let state = Arc::clone(&self.state);
        let parent = self.parent.clone();
        let phase = self
            .phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Box::pin(service.run_agent(state, self.handle.clone(), parent, phase, prompt, options))
    }

    fn state(&self) -> WorkflowState {
        self.state.lock().state
    }
}

fn snapshot(state: &Arc<RunState>) -> WorkflowRun {
    state.lock().clone()
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or_default()
}

fn normalize_args(args: Value) -> Value {
    if args.is_null() {
        json!({})
    } else {
        args
    }
}

fn short_label(prompt: &str) -> String {
    let line = prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let mut label: String = line.trim().chars().take(60).collect();
    if line.trim().chars().count() > 60 {
        label.push('…');
    }
    if label.is_empty() {
        "agent".to_string()
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn settings(dir: &std::path::Path) -> WorkflowSettings {
        WorkflowSettings {
            directory: dir.to_path_buf(),
            worktree: dir.to_path_buf(),
            config: dir.join("config"),
            home: dir.join("home"),
            config_dirs: Vec::new(),
            paths: Vec::new(),
            state: dir.join("state"),
            enabled: true,
            max_concurrent: 2,
            max_agents: 10,
            claude_compat: false,
            project_config: true,
        }
    }

    struct FakeOps;

    impl WorkflowOps for FakeOps {
        fn create_run_session<'a>(
            &'a self,
            _run_id: &'a str,
            _title: &'a str,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async { Ok("ses_root".to_string()) })
        }

        fn create_agent_session<'a>(
            &'a self,
            request: &'a WorkflowAgentRequest,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move { Ok(format!("ses_{}", request.title.len())) })
        }

        fn prompt_agent<'a>(
            &'a self,
            session_id: &'a str,
            _agent: &'a str,
            prompt: &'a str,
            _schema: Option<&'a Value>,
        ) -> BoxFuture<'a, Result<super::super::WorkflowAgentOutcome, String>> {
            Box::pin(async move {
                let text = if prompt.contains("Break this research question") {
                    "{\"angles\":[{\"title\":\"one\",\"brief\":\"first\"}]}".to_string()
                } else {
                    format!("{session_id}:{prompt}")
                };
                Ok(super::super::WorkflowAgentOutcome {
                    text,
                    ..Default::default()
                })
            })
        }

        fn cancel_agent<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn starts_waits_and_persists_a_run() {
        let temp = crate::storage::test_support::TempDir::new("workflow-service");
        let service = WorkflowService::new(Arc::new(FakeOps), settings(temp.path()));
        let run = service
            .start(WorkflowStartRequest {
                name: None,
                script: Some(
                    "export const meta = { name: 'inline' }\nphase('P')\nreturn await agent('hi')"
                        .to_string(),
                ),
                description: None,
                args: json!({ "a": 1 }),
                session_id: None,
            })
            .unwrap();
        assert_eq!(run.state, WorkflowState::Queued);
        let done = service.wait(&run.id).await.unwrap();
        assert_eq!(done.state, WorkflowState::Completed);
        assert_eq!(done.session_id.as_deref(), Some("ses_root"));
        assert_eq!(done.phases[0].title, "P");
        assert!(done.agents[0].result.as_deref().unwrap().contains("hi"));
        assert!(service.get(&run.id).is_some());
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn builtin_deep_research_runs_to_completion() {
        let temp = crate::storage::test_support::TempDir::new("workflow-deep-research");
        let service = WorkflowService::new(Arc::new(FakeOps), settings(temp.path()));
        let run = service
            .start(WorkflowStartRequest {
                name: Some("deep-research".to_string()),
                script: None,
                description: None,
                args: json!({ "question": "what changed" }),
                session_id: None,
            })
            .unwrap();
        let done = service.wait(&run.id).await.unwrap();
        assert_eq!(done.state, WorkflowState::Completed, "{:?}", done.error);
        assert_eq!(done.phases.len(), 4);
        assert!(done.agents.len() >= 4);
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn abort_marks_the_run_cancelled() {
        let temp = crate::storage::test_support::TempDir::new("workflow-abort");
        let service = WorkflowService::new(Arc::new(FakeOps), settings(temp.path()));
        let run = service
            .start(WorkflowStartRequest {
                name: None,
                script: Some("await new Promise(() => {})".to_string()),
                description: None,
                args: json!(null),
                session_id: None,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let cancelled = service.abort(&run.id).await.unwrap();
        assert_eq!(cancelled.state, WorkflowState::Cancelled);
        std::fs::remove_dir_all(temp.path()).ok();
    }

    fn inline(script: &str) -> WorkflowStartRequest {
        WorkflowStartRequest {
            name: None,
            script: Some(script.to_string()),
            description: None,
            args: json!(null),
            session_id: None,
        }
    }

    /// Prompts that contain `hang` run until cancelled.
    #[derive(Default)]
    struct HangingOps {
        cancelled: Mutex<Vec<String>>,
        cancels: Notify,
    }

    impl WorkflowOps for HangingOps {
        fn create_run_session<'a>(
            &'a self,
            _run_id: &'a str,
            _title: &'a str,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async { Ok("ses_root".to_string()) })
        }

        fn create_agent_session<'a>(
            &'a self,
            request: &'a WorkflowAgentRequest,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move { Ok(format!("ses_{}", request.prompt)) })
        }

        fn prompt_agent<'a>(
            &'a self,
            _session_id: &'a str,
            _agent: &'a str,
            prompt: &'a str,
            _schema: Option<&'a Value>,
        ) -> BoxFuture<'a, Result<super::super::WorkflowAgentOutcome, String>> {
            Box::pin(async move {
                if prompt.contains("hang") {
                    std::future::pending::<()>().await;
                }
                if prompt.contains("fail") {
                    return Err("exploded".to_string());
                }
                Ok(super::super::WorkflowAgentOutcome {
                    text: "ok".to_string(),
                    ..Default::default()
                })
            })
        }

        fn cancel_agent<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.cancelled.lock().unwrap().push(session_id.to_string());
                self.cancels.notify_waiters();
            })
        }

        fn is_workflow_agent(&self, session_id: &str) -> bool {
            session_id == "ses_agent"
        }
    }

    async fn wait_for_cancel(ops: &HangingOps, session: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = ops.cancels.notified();
                if ops.cancelled.lock().unwrap().iter().any(|id| id == session) {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("the agent session was cancelled");
    }

    /// Fails with the given errors in turn, then succeeds.
    struct FlakyOps {
        errors: Mutex<Vec<(&'static str, bool)>>,
        prompts: Mutex<Vec<String>>,
        /// Whether a stop arrives once the first prompt has ended.
        stopped: bool,
    }

    impl WorkflowOps for FlakyOps {
        fn create_run_session<'a>(
            &'a self,
            _run_id: &'a str,
            _title: &'a str,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async { Ok("ses_root".to_string()) })
        }

        fn create_agent_session<'a>(
            &'a self,
            _request: &'a WorkflowAgentRequest,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async { Ok("ses_agent".to_string()) })
        }

        fn prompt_agent<'a>(
            &'a self,
            _session_id: &'a str,
            _agent: &'a str,
            prompt: &'a str,
            _schema: Option<&'a Value>,
        ) -> BoxFuture<'a, Result<super::super::WorkflowAgentOutcome, String>> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            let next = self.errors.lock().unwrap().pop();
            Box::pin(async move {
                Ok(match next {
                    Some((error, recoverable)) => super::super::WorkflowAgentOutcome {
                        error: Some(error.to_string()),
                        recoverable,
                        ..Default::default()
                    },
                    None => super::super::WorkflowAgentOutcome {
                        text: "recovered".to_string(),
                        ..Default::default()
                    },
                })
            })
        }

        fn cancel_agent<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }

        fn cancel_count(&self, _session_id: &str) -> u64 {
            u64::from(self.stopped && !self.prompts.lock().unwrap().is_empty())
        }
    }

    #[tokio::test]
    async fn a_stop_during_an_agents_resume_wait_is_honoured() {
        let temp = crate::storage::test_support::TempDir::new("workflow-stop-resume");
        let ops = Arc::new(FlakyOps {
            errors: Mutex::new(vec![("boom", true)]),
            prompts: Mutex::new(Vec::new()),
            stopped: true,
        });
        let service = WorkflowService::new(ops.clone(), settings(temp.path()));
        let run = service.start(inline("return await agent('job')")).unwrap();
        let done = tokio::time::timeout(Duration::from_secs(1), service.wait(&run.id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.state, WorkflowState::Failed);
        assert_eq!(ops.prompts.lock().unwrap().len(), 1, "not resumed");
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn an_agent_that_stops_on_a_recoverable_error_is_resumed() {
        let temp = crate::storage::test_support::TempDir::new("workflow-resume");
        let ops = Arc::new(FlakyOps {
            errors: Mutex::new(vec![("boom", true)]),
            prompts: Mutex::new(Vec::new()),
            stopped: false,
        });
        let service = WorkflowService::new(ops.clone(), settings(temp.path()));
        let run = service.start(inline("return await agent('job')")).unwrap();
        let done = tokio::time::timeout(Duration::from_secs(10), service.wait(&run.id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.state, WorkflowState::Completed, "{:?}", done.error);
        assert_eq!(done.result, Some(json!("recovered")));
        assert_eq!(done.agents[0].state, WorkflowState::Completed);
        let prompts = ops.prompts.lock().unwrap();
        assert_eq!(prompts[0], "job");
        assert!(prompts[1].starts_with("<auto_resume>") && prompts[1].contains("boom"));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn an_unrecoverable_agent_error_fails_at_once() {
        let temp = crate::storage::test_support::TempDir::new("workflow-noresume");
        let ops = Arc::new(FlakyOps {
            errors: Mutex::new(vec![("bad credentials", false)]),
            prompts: Mutex::new(Vec::new()),
            stopped: false,
        });
        let service = WorkflowService::new(ops.clone(), settings(temp.path()));
        let run = service.start(inline("return await agent('job')")).unwrap();
        let done = tokio::time::timeout(Duration::from_secs(1), service.wait(&run.id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.state, WorkflowState::Failed);
        assert_eq!(ops.prompts.lock().unwrap().len(), 1);
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn agents_left_running_are_cancelled_when_the_script_ends() {
        let temp = crate::storage::test_support::TempDir::new("workflow-leftovers");
        let ops = Arc::new(HangingOps::default());
        let service = WorkflowService::new(ops.clone(), settings(temp.path()));
        let run = service
            .start(inline(
                "agent('hang').catch(() => {})\nawait agent('quick')\nreturn 1",
            ))
            .unwrap();
        let done = service.wait(&run.id).await.unwrap();
        assert_eq!(done.state, WorkflowState::Completed);
        let left = done
            .agents
            .iter()
            .find(|agent| agent.prompt == "hang")
            .unwrap();
        assert_eq!(left.state, WorkflowState::Cancelled);
        assert!(left.result.is_none());
        wait_for_cancel(&ops, "ses_hang").await;
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn abort_cancels_running_agents_and_keeps_a_finished_run() {
        let temp = crate::storage::test_support::TempDir::new("workflow-abort-agents");
        let ops = Arc::new(HangingOps::default());
        let service = WorkflowService::new(ops.clone(), settings(temp.path()));
        let run = service.start(inline("return await agent('hang')")).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while service
                .get(&run.id)
                .unwrap()
                .agents
                .iter()
                .all(|a| a.session_id.is_none())
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            service.abort(&run.id).await.unwrap().state,
            WorkflowState::Cancelled
        );
        wait_for_cancel(&ops, "ses_hang").await;

        let finished = service.start(inline("return 1")).unwrap();
        let done = service.wait(&finished.id).await.unwrap();
        let again = service.abort(&finished.id).await.unwrap();
        assert_eq!(again.state, WorkflowState::Completed);
        assert_eq!(again.finished, done.finished);
        assert_eq!(again.result, Some(json!(1)));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn a_new_service_adopts_live_runs() {
        let temp = crate::storage::test_support::TempDir::new("workflow-reboot");
        let ops = Arc::new(HangingOps::default());
        let first = WorkflowService::new(ops.clone(), settings(temp.path()));
        let run = first.start(inline("return await agent('hang')")).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let second = WorkflowService::new(ops.clone(), settings(temp.path()));
        let live = second.get(&run.id).unwrap();
        assert!(live.is_active(), "{:?}", live.error);
        assert_eq!(
            second.abort(&run.id).await.unwrap().state,
            WorkflowState::Cancelled
        );
        wait_for_cancel(&ops, "ses_hang").await;
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn dead_runs_fail_with_their_agents_and_phases() {
        let temp = crate::storage::test_support::TempDir::new("workflow-dead");
        let settings = settings(temp.path());
        let run_dir = settings.state.join("workflows/runs");
        std::fs::create_dir_all(&run_dir).unwrap();
        let run = json!({
            "id": "dead",
            "name": "x",
            "directory": settings.directory,
            "state": "running",
            "script": "",
            "started": 1.0,
            "phases": [{ "title": "P", "state": "running", "started": 1.0 }],
            "agents": [{
                "id": "a", "phase": "P", "label": "l", "agent": "general",
                "prompt": "p", "state": "running", "started": 1.0,
            }],
        });
        std::fs::write(run_dir.join("dead.json"), run.to_string()).unwrap();
        let service = WorkflowService::new(Arc::new(FakeOps), settings);
        let run = service.get("dead").unwrap();
        assert_eq!(run.state, WorkflowState::Failed);
        assert_eq!(run.phases[0].state, WorkflowState::Failed);
        assert_eq!(run.agents[0].state, WorkflowState::Failed);
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn a_run_locked_by_another_process_is_left_alone() {
        let temp = crate::storage::test_support::TempDir::new("workflow-foreign");
        let settings = settings(temp.path());
        let run_dir = settings.state.join("workflows/runs");
        std::fs::create_dir_all(&run_dir).unwrap();
        let run = json!({
            "id": "theirs",
            "name": "x",
            "directory": settings.directory,
            "state": "running",
            "script": "",
            "started": 1.0,
        });
        let path = run_dir.join("theirs.json");
        std::fs::write(&path, run.to_string()).unwrap();
        // Separately opened, so this stands in for the other process.
        let lock = File::create(run_dir.join("theirs.lock")).unwrap();
        lock.try_lock().unwrap();

        let service = WorkflowService::new(Arc::new(FakeOps), settings);
        assert_eq!(service.get("theirs").unwrap().state, WorkflowState::Running);
        assert!(matches!(
            service.abort("theirs").await,
            Err(WorkflowError::Conflict(_))
        ));
        assert!(std::fs::read_to_string(&path).unwrap().contains("running"));

        // It died without finishing.
        drop(lock);
        assert_eq!(service.get("theirs").unwrap().state, WorkflowState::Failed);
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn workflow_agents_cannot_start_workflows() {
        let temp = crate::storage::test_support::TempDir::new("workflow-nested");
        let service = WorkflowService::new(Arc::new(HangingOps::default()), settings(temp.path()));
        let request = WorkflowStartRequest {
            session_id: Some("ses_agent".to_string()),
            ..inline("return 1")
        };
        assert!(matches!(
            service.start(request),
            Err(WorkflowError::Invalid(_))
        ));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn runs_from_subdirectories_are_listed_under_the_project() {
        let temp = crate::storage::test_support::TempDir::new("workflow-subdir");
        let root = WorkflowService::new(Arc::new(FakeOps), settings(temp.path()));
        let sub = temp.path().join("pkg");
        let subdir = WorkflowService::new(
            Arc::new(FakeOps),
            WorkflowSettings {
                directory: sub,
                ..settings(temp.path())
            },
        );
        let run = subdir.start(inline("return 1")).unwrap();
        subdir.wait(&run.id).await.unwrap();
        assert!(root.runs().iter().any(|listed| listed.id == run.id));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn returning_to_a_phase_reopens_it() {
        let temp = crate::storage::test_support::TempDir::new("workflow-phase");
        let service = WorkflowService::new(Arc::new(FakeOps), settings(temp.path()));
        let run = service
            .start(inline(
                "phase('A')\nphase('B')\nphase('A')\nawait new Promise(() => {})",
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let live = service.get(&run.id).unwrap();
        let state = |title: &str| {
            live.phases
                .iter()
                .find(|phase| phase.title == title)
                .map(|phase| phase.state)
        };
        assert_eq!(state("A"), Some(WorkflowState::Running));
        assert_eq!(state("B"), Some(WorkflowState::Completed));
        service.abort(&run.id).await.unwrap();
        std::fs::remove_dir_all(temp.path()).ok();
    }

    fn save_request(name: &str, script: &str, overwrite: bool) -> WorkflowSaveRequest {
        WorkflowSaveRequest {
            name: name.to_string(),
            description: None,
            script: script.to_string(),
            scope: None,
            overwrite,
        }
    }

    #[tokio::test]
    async fn saving_writes_back_to_the_file_it_came_from() {
        let temp = crate::storage::test_support::TempDir::new("workflow-save");
        let service = WorkflowService::new(Arc::new(FakeOps), settings(temp.path()));
        let dir = temp.path().join(".opencode/workflow");
        std::fs::create_dir_all(&dir).unwrap();
        let original = dir.join("zeta.js");
        std::fs::write(&original, "export const meta = { name: 'alpha' }\nreturn 1").unwrap();

        let saved = service
            .save(save_request(
                "alpha",
                "export const meta = { name: 'alpha' }\nreturn 2",
                true,
            ))
            .unwrap();
        assert_eq!(Path::new(&saved.path), original);
        assert!(std::fs::read_to_string(&original)
            .unwrap()
            .contains("return 2"));
        assert!(!temp.path().join(".opencode/workflows/alpha.js").exists());

        let found = service.definitions();
        let alpha = found
            .iter()
            .find(|workflow| workflow.name == "alpha")
            .unwrap();
        assert!(alpha.script.contains("return 2"));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn saving_over_an_existing_workflow_needs_overwrite() {
        let temp = crate::storage::test_support::TempDir::new("workflow-conflict");
        let service = WorkflowService::new(Arc::new(FakeOps), settings(temp.path()));
        service
            .save(save_request("mine", "return 1", false))
            .unwrap();
        assert!(matches!(
            service.save(save_request("mine", "return 2", false)),
            Err(WorkflowError::Conflict(_))
        ));
        let list = service.definitions();
        let mine = list
            .iter()
            .find(|workflow| workflow.name == "mine")
            .unwrap();
        assert!(mine.script.contains("return 1"));
        // Built-ins are shadowed, not replaced.
        service
            .save(save_request("deep-research", "return 3", false))
            .unwrap();
        std::fs::remove_dir_all(temp.path()).ok();
    }
}
