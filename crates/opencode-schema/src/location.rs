//! `schema-src/location.ts`.

use serde::{Deserialize, Serialize};

use crate::ids::{ProjectId, WorkspaceId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationRef {
    pub directory: String, // AbsolutePath
    #[serde(
        rename = "workspaceID",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_id: Option<WorkspaceId>,
}

/// `Location.Info` — internal wrapper; wire-visible via the `{location, data}` response helper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationInfo {
    pub directory: String,
    #[serde(
        rename = "workspaceID",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_id: Option<WorkspaceId>,
    pub project: LocationProject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationProject {
    pub id: ProjectId,
    pub directory: String,
}
