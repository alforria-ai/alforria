//! `schema-src/prompt-input.ts`.

use serde::{Deserialize, Serialize};

use crate::prompt::PromptAgentAttachment;
use crate::prompt::PromptSource;

/// openapi `PromptInputFileAttachment`; required `uri`. No `mime` (unlike `PromptFileAttachment`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptInputFileAttachment {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PromptSource>,
}

/// openapi `PromptInput`; required `text`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptInput {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<PromptInputFileAttachment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<PromptAgentAttachment>>,
}
