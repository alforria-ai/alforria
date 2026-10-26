//! Workflow wire DTOs — an alforria extension (no TS reference).
//!
//! A workflow is a saved JavaScript orchestration of subagents. The runtime
//! records one [`WorkflowRun`] per execution, with a [`WorkflowPhase`] per
//! `phase()` group and a [`WorkflowAgent`] per `agent()` call. Timestamps are
//! milliseconds since the Unix epoch, matching the rest of the wire surface.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How a saved workflow was found: the project, the user's config, or a
/// Claude Code-compatible external directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowSource {
    Project,
    Global,
    External,
    Builtin,
}

/// One saved workflow script, as listed by `GET /workflow`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub path: String,
    pub source: WorkflowSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<String>,
    pub script: String,
}

/// The lifecycle shared by runs, phases, and agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl WorkflowState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            WorkflowState::Completed | WorkflowState::Failed | WorkflowState::Cancelled
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            WorkflowState::Queued => "queued",
            WorkflowState::Running => "running",
            WorkflowState::Completed => "completed",
            WorkflowState::Failed => "failed",
            WorkflowState::Cancelled => "cancelled",
        }
    }
}

/// One `phase()` group. Agents started before the first `phase()` call live
/// in the implicit `Workflow` phase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPhase {
    pub title: String,
    pub state: WorkflowState,
    pub started: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<f64>,
}

/// One `agent()` call. `sessionID` appears once the child session is created,
/// so the web client can open the agent's transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAgent {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub phase: String,
    pub label: String,
    pub agent: String,
    pub prompt: String,
    pub state: WorkflowState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<f64>,
}

/// A `log()` line, kept with the run for the progress view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowLog {
    pub time: f64,
    pub message: String,
}

/// One workflow execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRun {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub directory: String,
    /// The session the run belongs to. Agent sessions are its children.
    #[serde(rename = "sessionID")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub state: WorkflowState,
    #[serde(default)]
    pub args: Value,
    pub script: String,
    #[serde(default)]
    pub phases: Vec<WorkflowPhase>,
    #[serde(default)]
    pub agents: Vec<WorkflowAgent>,
    #[serde(default)]
    pub logs: Vec<WorkflowLog>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<f64>,
}

impl WorkflowRun {
    pub fn is_active(&self) -> bool {
        !self.state.is_terminal()
    }
}

/// `POST /workflow/run` body.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowStartRequest {
    /// A saved workflow name. Either this or `script` is required.
    #[serde(default)]
    pub name: Option<String>,
    /// An inline script, used when `name` is absent.
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub args: Value,
    /// Run as a child of this session instead of a new workflow session.
    #[serde(rename = "sessionID")]
    #[serde(default)]
    pub session_id: Option<String>,
}

/// `POST /workflow` body — save a script to the project or user directory.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSaveRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub script: String,
    /// `project` (default) or `global`.
    #[serde(default)]
    pub scope: Option<String>,
}
