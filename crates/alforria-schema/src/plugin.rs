//! Wire DTOs for `schema-src/plugin.ts` — openapi `PluginAdded`.

use serde::{Deserialize, Serialize};

use crate::ids::PluginId;

/// `plugin.added` payload — openapi `PluginAdded.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginAddedData {
    pub id: PluginId,
}
