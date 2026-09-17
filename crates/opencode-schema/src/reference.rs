//! `reference.ts` — openapi `ReferenceLocalSource`, `ReferenceGitSource`,
//! `ReferenceInfo`, `ReferenceSource`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum ReferenceSource {
    Local {
        /// AbsolutePath
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hidden: Option<bool>,
    },
    Git {
        repository: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hidden: Option<bool>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceInfo {
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    pub source: ReferenceSource,
}

/// `reference.updated` payload — empty object (openapi `EventReferenceUpdated.properties`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceUpdatedData {}
