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

use crate::tool::def::BoxFuture;

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

    /// Prompt an agent session to completion.
    fn prompt_agent<'a>(
        &'a self,
        session_id: &'a str,
        agent: &'a str,
        prompt: &'a str,
    ) -> BoxFuture<'a, Result<WorkflowAgentOutcome, String>>;

    /// Cancel a running agent session.
    fn cancel_agent<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ()>;
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
