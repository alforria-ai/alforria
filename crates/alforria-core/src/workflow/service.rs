//! Workflow runs: lifecycle, fan-out, persistence, and the progress model.
//!
//! Runs execute on a dedicated current-thread Tokio runtime so the QuickJS
//! context (which is not `Send`) never leaves its thread. The host bridges
//! each `agent()` call back to the session engine.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use alforria_schema::workflow::{
    WorkflowAgent, WorkflowDefinition, WorkflowLog, WorkflowPhase, WorkflowRun,
    WorkflowSaveRequest, WorkflowSource, WorkflowStartRequest, WorkflowState,
};
use serde_json::{json, Value};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::tool::def::BoxFuture;

use super::definition::{self, parse_meta, rewrite_meta, valid_name};
use super::runtime::{self, WorkflowHost};
use super::{ResolvedWorkflow, WorkflowAgentRequest, WorkflowOps, WorkflowSettings};

const MAX_LOGS: usize = 500;

struct RunState {
    run: Mutex<WorkflowRun>,
    abort: CancellationToken,
    notify: Notify,
}

impl RunState {
    fn new(run: WorkflowRun) -> Self {
        RunState {
            run: Mutex::new(run),
            abort: CancellationToken::new(),
            notify: Notify::new(),
        }
    }
}

/// The workflow service for one instance directory.
pub struct WorkflowService {
    ops: Arc<dyn WorkflowOps>,
    settings: WorkflowSettings,
    runs: Mutex<HashMap<String, Arc<RunState>>>,
    semaphore: Arc<Semaphore>,
    run_dir: PathBuf,
}

impl WorkflowService {
    pub fn new(ops: Arc<dyn WorkflowOps>, settings: WorkflowSettings) -> Arc<Self> {
        let run_dir = settings.state.join("workflows").join("runs");
        let service = Arc::new(WorkflowService {
            ops,
            semaphore: Arc::new(Semaphore::new(settings.max_concurrent())),
            settings,
            runs: Mutex::new(HashMap::new()),
            run_dir,
        });
        service.load_runs();
        service
    }

    pub fn enabled(&self) -> bool {
        self.settings.enabled
    }

    /// Saved workflows visible from this instance.
    pub fn definitions(&self) -> Vec<WorkflowDefinition> {
        definition::discover(&self.settings)
    }

    /// Save a workflow script to the project or user directory.
    pub fn save(&self, request: WorkflowSaveRequest) -> Result<WorkflowDefinition, String> {
        if !valid_name(&request.name) {
            return Err("invalid workflow name: use letters, digits, '-', '_' or '.'".to_string());
        }
        let script = rewrite_meta(
            &request.script,
            &request.name,
            request.description.as_deref(),
        );
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
            other => return Err(format!("unknown workflow scope: {other}")),
        };
        std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        let path = dir.join(format!("{}.js", request.name));
        let temp = dir.join(format!(".{}.js.tmp", request.name));
        std::fs::write(&temp, &script).map_err(|err| err.to_string())?;
        std::fs::rename(&temp, &path).map_err(|err| err.to_string())?;
        let meta = parse_meta(&script);
        Ok(WorkflowDefinition {
            name: request.name,
            description: meta.description.or(request.description),
            path: path.to_string_lossy().to_string(),
            source,
            phases: meta.phases,
            script,
        })
    }

    /// Delete a project workflow by name.
    pub fn delete(&self, name: &str) -> Result<bool, String> {
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
            return Err("only saved workflows can be deleted".to_string());
        }
        std::fs::remove_file(&workflow.path).map_err(|err| err.to_string())?;
        Ok(true)
    }

    /// Start a run and return its first snapshot. The run continues in the
    /// background on its own thread.
    pub fn start(self: &Arc<Self>, request: WorkflowStartRequest) -> Result<WorkflowRun, String> {
        if !self.settings.enabled {
            return Err("workflows are disabled".to_string());
        }
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
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(run_id.clone(), state.clone());
        self.persist(&state);
        self.prune_runs();

        let service = Arc::clone(self);
        let script = run.script.clone();
        let args = run.args.clone();
        let parent = request.session_id.clone();
        let title = run.name.clone();
        let thread_name = format!("workflow-{run_id}");
        std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let Ok(runtime) = runtime else {
                    let error = "workflow runtime could not start".to_string();
                    service.finish(&state, Err(error));
                    return;
                };
                let runner = Arc::clone(&service);
                let result =
                    runtime.block_on(runner.run(state.clone(), script, args, parent, title));
                service.finish(&state, result);
            })
            .map_err(|err| err.to_string())?;
        Ok(run)
    }

    /// Every run in this directory, newest first.
    pub fn runs(&self) -> Vec<WorkflowRun> {
        let mut runs: Vec<WorkflowRun> = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(snapshot)
            .collect();
        runs.sort_by(|a, b| b.started.total_cmp(&a.started));
        runs
    }

    pub fn get(&self, run_id: &str) -> Option<WorkflowRun> {
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(run_id)
            .map(snapshot)
    }

    /// The newest run that belongs to a session (for the tool's `status`
    /// action without an explicit run id).
    pub fn latest_for_session(&self, session_id: &str) -> Option<WorkflowRun> {
        self.runs()
            .into_iter()
            .find(|run| run.session_id.as_deref() == Some(session_id))
    }

    /// Stop a run and cancel its running agents.
    pub async fn abort(&self, run_id: &str) -> Result<WorkflowRun, String> {
        let state = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(run_id)
            .cloned()
            .ok_or_else(|| format!("workflow run not found: {run_id}"))?;
        state.abort.cancel();
        let sessions: Vec<String> = {
            let mut run = state
                .run
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut sessions = Vec::new();
            for agent in &mut run.agents {
                if agent.state == WorkflowState::Running {
                    agent.state = WorkflowState::Cancelled;
                    agent.finished = Some(now_ms());
                    if let Some(session) = &agent.session_id {
                        sessions.push(session.clone());
                    }
                }
            }
            for phase in &mut run.phases {
                if phase.state == WorkflowState::Running {
                    phase.state = WorkflowState::Cancelled;
                    phase.finished = Some(now_ms());
                }
            }
            run.state = WorkflowState::Cancelled;
            run.finished = Some(now_ms());
            sessions
        };
        for session in sessions {
            self.ops.cancel_agent(&session).await;
        }
        self.persist(&state);
        state.notify.notify_waiters();
        Ok(snapshot(&state))
    }

    /// Wait for a run to reach a terminal state.
    pub async fn wait(&self, run_id: &str) -> Result<WorkflowRun, String> {
        let state = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(run_id)
            .cloned()
            .ok_or_else(|| format!("workflow run not found: {run_id}"))?;
        loop {
            if let Some(run) = self.get(run_id) {
                if run.state.is_terminal() {
                    return Ok(run);
                }
            }
            let notified = state.notify.notified();
            if let Some(run) = self.get(run_id) {
                if run.state.is_terminal() {
                    return Ok(run);
                }
            }
            notified.await;
        }
    }

    fn resolve(&self, request: &WorkflowStartRequest) -> Result<ResolvedWorkflow, String> {
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
                Some(name) => format!("workflow not found: {name}"),
                None => "a workflow name or script is required".to_string(),
            });
        };
        let meta = parse_meta(&script);
        let name = request
            .name
            .clone()
            .or(meta.name)
            .unwrap_or_else(|| "inline".to_string());
        if !valid_name(&name) {
            return Err("invalid workflow name".to_string());
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
            let mut run = state
                .run
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            run.session_id = Some(parent.clone());
            run.state = WorkflowState::Running;
        }
        self.persist(&state);

        let host = Arc::new(RunHost {
            service: Arc::clone(&self),
            state: Arc::clone(&state),
            parent,
            phase: Mutex::new("Workflow".to_string()),
        });
        let result = tokio::select! {
            _ = state.abort.cancelled() => Err("workflow cancelled".to_string()),
            result = runtime::execute(host, &script, args) => result,
        };
        result
    }

    fn finish(&self, state: &Arc<RunState>, result: Result<Value, String>) {
        let mut run = state
            .run
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let finished = now_ms();
        if state.abort.is_cancelled() || run.state == WorkflowState::Cancelled {
            run.state = WorkflowState::Cancelled;
            if run.error.is_none() {
                run.error = Some("workflow cancelled".to_string());
            }
        } else {
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
                agent.state = final_state;
                agent.finished = Some(finished);
            }
        }
        run.finished = Some(finished);
        drop(run);
        self.persist(state);
        state.notify.notify_waiters();
    }

    fn ensure_phase(&self, state: &Arc<RunState>, title: &str) {
        let mut run = state
            .run
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        self.persist(state);
    }

    fn set_phase(&self, state: &Arc<RunState>, title: &str) {
        {
            let mut run = state
                .run
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for phase in &mut run.phases {
                if phase.state == WorkflowState::Running {
                    phase.state = WorkflowState::Completed;
                    phase.finished = Some(now_ms());
                }
            }
        }
        self.ensure_phase(state, title);
    }

    fn add_log(&self, state: &Arc<RunState>, message: String) {
        {
            let mut run = state
                .run
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            run.logs.push(WorkflowLog {
                time: now_ms(),
                message,
            });
            if run.logs.len() > MAX_LOGS {
                let drop = run.logs.len() - MAX_LOGS;
                run.logs.drain(0..drop);
            }
        }
        self.persist(state);
    }

    async fn run_agent(
        &self,
        state: &Arc<RunState>,
        parent_session_id: &str,
        phase: &str,
        prompt: String,
        options: Value,
    ) -> Result<String, String> {
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
        let schema = options.get("schema").cloned().unwrap_or(Value::Null);
        let prompt = if schema.is_null() {
            prompt
        } else {
            format!(
                "{prompt}\n\nReturn only one JSON value matching this JSON Schema. Do not wrap it in markdown fences.\nSchema: {schema}"
            )
        };
        let agent_id = Ulid::new().to_string();
        {
            let mut run = state
                .run
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if run.agents.len() >= self.settings.max_agents() {
                return Err(format!(
                    "workflow agent limit reached ({} agents)",
                    self.settings.max_agents()
                ));
            }
            run.agents.push(WorkflowAgent {
                id: agent_id.clone(),
                session_id: None,
                phase: phase.to_string(),
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
        self.ensure_phase(state, phase);
        self.persist(state);

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
            run_id: snapshot(state).id,
            parent_session_id: parent_session_id.to_string(),
            title: format!("{} (@{agent} workflow agent)", label),
            agent: agent.clone(),
            prompt: prompt.clone(),
        };
        let session_id = match self.ops.create_agent_session(&request).await {
            Ok(session) => session,
            Err(error) => {
                self.update_agent(state, &agent_id, |agent| {
                    agent.state = WorkflowState::Failed;
                    agent.error = Some(error.clone());
                    agent.finished = Some(now_ms());
                });
                return Err(error);
            }
        };
        self.update_agent(state, &agent_id, |agent| {
            agent.session_id = Some(session_id.clone());
        });

        let outcome = self.ops.prompt_agent(&session_id, &agent, &prompt).await;
        drop(permit);
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

    fn update_agent(
        &self,
        state: &Arc<RunState>,
        agent_id: &str,
        update: impl FnOnce(&mut WorkflowAgent),
    ) {
        {
            let mut run = state
                .run
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(agent) = run.agents.iter_mut().find(|agent| agent.id == agent_id) {
                update(agent);
            }
        }
        self.persist(state);
    }

    fn persist(&self, state: &Arc<RunState>) {
        if std::fs::create_dir_all(&self.run_dir).is_err() {
            return;
        }
        let run = state
            .run
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Ok(bytes) = serde_json::to_vec_pretty(&run) else {
            return;
        };
        let path = self.run_dir.join(format!("{}.json", run.id));
        let temp = self.run_dir.join(format!(".{}.json.tmp", run.id));
        if std::fs::write(&temp, bytes).is_ok() {
            let _ = std::fs::rename(&temp, &path);
        }
    }

    fn load_runs(&self) {
        let Ok(entries) = std::fs::read_dir(&self.run_dir) else {
            return;
        };
        let directory = self.settings.directory.to_string_lossy().to_string();
        let mut runs: Vec<WorkflowRun> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(mut run) = serde_json::from_slice::<WorkflowRun>(&bytes) else {
                continue;
            };
            if run.directory != directory {
                continue;
            }
            if run.is_active() {
                run.state = WorkflowState::Failed;
                run.error = Some("server restarted before the workflow finished".to_string());
                run.finished = Some(now_ms());
            }
            runs.push(run);
        }
        runs.sort_by(|a, b| b.started.total_cmp(&a.started));
        if runs.len() > 100 {
            for old in &runs[100..] {
                let _ = std::fs::remove_file(self.run_dir.join(format!("{}.json", old.id)));
            }
            runs.truncate(100);
        }
        let mut map = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for run in runs {
            let state = Arc::new(RunState::new(run));
            self.persist(&state);
            map.insert(snapshot(&state).id, state);
        }
    }

    fn prune_runs(&self) {
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if runs.len() <= 100 {
            return;
        }
        let mut terminal: Vec<(String, f64)> = runs
            .iter()
            .filter(|(_, state)| !snapshot(state).is_active())
            .map(|(id, state)| (id.clone(), snapshot(state).started))
            .collect();
        terminal.sort_by(|a, b| a.1.total_cmp(&b.1));
        let excess = runs.len().saturating_sub(100);
        for (id, _) in terminal.into_iter().take(excess) {
            runs.remove(&id);
            let _ = std::fs::remove_file(self.run_dir.join(format!("{id}.json")));
        }
    }
}

struct RunHost {
    service: Arc<WorkflowService>,
    state: Arc<RunState>,
    parent: String,
    phase: Mutex<String>,
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
        Box::pin(async move {
            service
                .run_agent(&state, &parent, &phase, prompt, options)
                .await
        })
    }

    fn state(&self) -> WorkflowState {
        snapshot(&self.state).state
    }
}

fn snapshot(state: &Arc<RunState>) -> WorkflowRun {
    state
        .run
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
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
        ) -> BoxFuture<'a, Result<super::super::WorkflowAgentOutcome, String>> {
            Box::pin(async move {
                let text = if prompt.contains("Break this research question") {
                    "{\"angles\":[{\"title\":\"one\",\"brief\":\"first\"}]}".to_string()
                } else {
                    format!("{session_id}:{prompt}")
                };
                Ok(super::super::WorkflowAgentOutcome { text, error: None })
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
}
