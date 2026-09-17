//! `project.ts` + `project-copy.ts` + `project-directories.ts` — openapi
//! `Project`, `ProjectVcs`, `ProjectIcon`, `ProjectCommands`, `ProjectTime`,
//! `ProjectCopy*`, `ProjectDirectoriesUpdated`.

use serde::{Deserialize, Serialize};

use crate::ids::{ProjectCopyStrategyId, ProjectId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectInfo {
    pub id: ProjectId,
    pub worktree: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcs: Option<ProjectVcs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<ProjectIcon>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commands: Option<ProjectCommands>,
    pub time: ProjectTime,
    pub sandboxes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectVcs {
    Git,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectIcon {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#override: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCommands {
    /// Startup script to run when creating a new workspace (worktree).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectTime {
    pub created: u64,
    pub updated: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialized: Option<u64>,
}

/// `project.updated` payload — identical to `ProjectInfo` on the wire
/// (openapi `EventProjectUpdated.properties`).
pub type ProjectUpdatedData = ProjectInfo;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDirectoriesUpdatedData {
    #[serde(rename = "projectID")]
    pub project_id: ProjectId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCopyCreateInput {
    #[serde(rename = "projectID")]
    pub project_id: ProjectId,
    pub strategy: ProjectCopyStrategyId,
    /// AbsolutePath
    pub source_directory: String,
    /// AbsolutePath
    pub directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCopyRemoveInput {
    #[serde(rename = "projectID")]
    pub project_id: ProjectId,
    /// AbsolutePath
    pub directory: String,
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCopyCopy {
    /// AbsolutePath
    pub directory: String,
}
