//! `command.ts` — openapi `CommandV2Info`.

use serde::{Deserialize, Serialize};

use crate::model::ModelRef;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandInfo {
    pub name: String,
    pub template: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtask: Option<bool>,
}
