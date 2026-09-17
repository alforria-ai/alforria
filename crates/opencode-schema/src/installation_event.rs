//! Wire DTOs for `schema-src/installation-event.ts` — openapi
//! `InstallationUpdated`, `InstallationUpdate-available`.

use serde::{Deserialize, Serialize};

/// `installation.updated` payload — openapi `InstallationUpdated.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallationUpdatedData {
    pub version: String,
}

/// `installation.update-available` payload — openapi
/// `InstallationUpdate-available.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallationUpdateAvailableData {
    pub version: String,
}
