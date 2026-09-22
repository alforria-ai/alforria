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
    /// `Location.Info`'s project — present on locations routed through the
    /// event bridge's ambient-`InstanceRef` publish path
    /// (`event-v2-bridge.ts:27-31`); plain `Location.Ref`s omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<LocationProject>,
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
