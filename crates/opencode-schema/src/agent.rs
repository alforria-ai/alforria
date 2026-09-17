//! `agent.ts` — openapi `AgentV2Info`.

use serde::{Deserialize, Serialize};

use crate::ids::AgentId;
use crate::model::ModelRef;
use crate::permission::PermissionRuleset;
use crate::provider::ProviderRequest;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    pub request: ProviderRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub mode: AgentMode,
    pub hidden: bool,
    /// `Agent.Color`: `#rrggbb` regex OR one of the preset words. Both branches
    /// are strings on the wire — modeled as a plain string, not validated
    /// (spec STOP S1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u64>,
    pub permissions: PermissionRuleset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    Subagent,
    Primary,
    All,
}
