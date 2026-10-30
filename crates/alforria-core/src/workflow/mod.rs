//! Dynamic workflows — an alforria extension inspired by Claude Code.
//!
//! A workflow is a JavaScript script (`.opencode/workflows/*.js`, the user
//! config directory, or a read-only `.claude/workflows` directory) that
//! orchestrates subagents with `agent()`, `pipeline()`, `parallel()`,
//! `phase()`, `log()`, and an `args` global. The script runs in a confined
//! QuickJS context with no filesystem or shell access; agents do the work.
//!
//! The runtime seam is [`WorkflowOps`]: production creates child sessions and
//! prompts them through the session engine, while tests use a fake.

pub mod definition;
pub mod runtime;
pub mod service;

use std::path::PathBuf;

use alforria_schema::workflow::WorkflowDefinition;
use serde_json::Value;

use crate::tool::def::BoxFuture;

/// Why a workflow request failed, so callers can answer with the right status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowError {
    Disabled,
    Invalid(String),
    NotFound(String),
    Conflict(String),
    Failed(String),
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowError::Disabled => f.write_str("workflows are disabled"),
            WorkflowError::Invalid(message)
            | WorkflowError::NotFound(message)
            | WorkflowError::Conflict(message)
            | WorkflowError::Failed(message) => f.write_str(message),
        }
    }
}

/// What one `agent()` call needs from the session engine.
#[derive(Debug, Clone)]
pub struct WorkflowAgentRequest {
    pub run_id: String,
    pub parent_session_id: String,
    pub title: String,
    pub agent: String,
    pub prompt: String,
}

/// The result of prompting one workflow agent.
#[derive(Debug, Clone, Default)]
pub struct WorkflowAgentOutcome {
    pub text: String,
    pub error: Option<String>,
    /// Whether prompting again may get past `error`.
    pub recoverable: bool,
}

/// The session-engine seam behind the workflow runtime.
pub trait WorkflowOps: Send + Sync {
    /// Create the parent session for a run that was not started from a
    /// session (a UI run). It is the parent of every agent session.
    fn create_run_session<'a>(
        &'a self,
        run_id: &'a str,
        title: &'a str,
    ) -> BoxFuture<'a, Result<String, String>>;

    /// Create one child session for an agent.
    fn create_agent_session<'a>(
        &'a self,
        request: &'a WorkflowAgentRequest,
    ) -> BoxFuture<'a, Result<String, String>>;

    /// Prompt an agent session to completion. With a `schema` the outcome's
    /// text is the model's structured output, encoded as JSON.
    fn prompt_agent<'a>(
        &'a self,
        session_id: &'a str,
        agent: &'a str,
        prompt: &'a str,
        schema: Option<&'a Value>,
    ) -> BoxFuture<'a, Result<WorkflowAgentOutcome, String>>;

    /// [`Self::prompt_agent`] with a note from the system rather than from a
    /// person: the model reads it, and clients show it as what it is.
    fn resume_agent<'a>(
        &'a self,
        session_id: &'a str,
        agent: &'a str,
        note: &'a str,
        schema: Option<&'a Value>,
    ) -> BoxFuture<'a, Result<WorkflowAgentOutcome, String>> {
        self.prompt_agent(session_id, agent, note, schema)
    }

    /// Cancel a running agent session.
    fn cancel_agent<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ()>;

    /// How many cancels the session has been asked for.
    fn cancel_count(&self, _session_id: &str) -> u64 {
        0
    }

    /// Whether a session is a workflow agent or runs under one. Those hold a
    /// concurrency slot, so a run they started and waited for could never get
    /// one.
    fn is_workflow_agent(&self, _session_id: &str) -> bool {
        false
    }
}

/// Where to discover workflows and how the runtime behaves.
#[derive(Debug, Clone)]
pub struct WorkflowSettings {
    pub directory: PathBuf,
    pub worktree: PathBuf,
    /// `$XDG_CONFIG_HOME/opencode` (the global config directory).
    pub config: PathBuf,
    pub home: PathBuf,
    /// The config directories the engine booted with.
    pub config_dirs: Vec<PathBuf>,
    /// `workflows.paths` from the config.
    pub paths: Vec<String>,
    /// Where run snapshots are persisted.
    pub state: PathBuf,
    pub enabled: bool,
    pub max_concurrent: usize,
    pub max_agents: usize,
    /// Read `.claude/workflows` directories too (Claude Code compatibility).
    pub claude_compat: bool,
    /// False under `OPENCODE_DISABLE_PROJECT_CONFIG`: skip project directories.
    pub project_config: bool,
}

impl WorkflowSettings {
    pub const DEFAULT_MAX_CONCURRENT: usize = 8;
    pub const DEFAULT_MAX_AGENTS: usize = 1000;

    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent.max(1)
    }

    pub fn max_agents(&self) -> usize {
        self.max_agents.max(1)
    }
}

/// A workflow command the runtime can start.
#[derive(Debug, Clone)]
pub struct ResolvedWorkflow {
    pub name: String,
    pub description: Option<String>,
    pub script: String,
}

impl From<&WorkflowDefinition> for ResolvedWorkflow {
    fn from(definition: &WorkflowDefinition) -> Self {
        ResolvedWorkflow {
            name: definition.name.clone(),
            description: definition.description.clone(),
            script: definition.script.clone(),
        }
    }
}
