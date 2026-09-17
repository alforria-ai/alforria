//! `schema-src/prompt.ts`.

use serde::{Deserialize, Serialize};

/// openapi `PromptSource`; required `start`, `end`, `text`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptSource {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// openapi `PromptFileAttachment`; required `uri`, `mime`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptFileAttachment {
    pub uri: String,
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PromptSource>,
}

/// openapi `PromptAgentAttachment`; required `name`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptAgentAttachment {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PromptSource>,
}

/// openapi `Prompt`; required `text`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prompt {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<PromptFileAttachment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<PromptAgentAttachment>>,
}
