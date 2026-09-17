//! `workspace.ts` + `workspace-id.ts` + `workspace-event.ts` + `worktree-event.ts`
//! — openapi `WorkspaceEventConnectionStatus`, `EventWorkspace*`, `EventWorktree*`.

use serde::{Deserialize, Serialize};

use crate::ids::WorkspaceId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceReadyData {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFailedData {
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatusData {
    #[serde(rename = "workspaceID")]
    pub workspace_id: WorkspaceId,
    pub status: WorkspaceConnectionStatus,
}

/// openapi `WorkspaceEventConnectionStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceConnectionStatus {
    Connected,
    Connecting,
    Disconnected,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeReadyData {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeFailedData {
    pub message: String,
}
