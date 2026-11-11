//! Config keys that only alforria has (the always-on assistant features).
//!
//! They live apart from `schema.rs`, which mirrors the upstream config, so
//! the two can change without touching each other.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `triggers` — the scheduler's switches. The triggers themselves are the
/// `trigger` map ([`crate::trigger::TriggerSpec`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TriggersInfo {
    /// `false` turns scheduling off: nothing fires and the `schedule` tool
    /// is not offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// How many trigger runs go at once (default 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u64>,
    /// The most files and directories one `watch` may cover (default 10000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_watched: Option<u64>,
    /// The least time between two deliveries one webhook trigger accepts
    /// (default `"1s"`; `"0s"` is no limit). A delivery that comes sooner
    /// is answered 429.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_min_interval: Option<crate::config::host::DurationSetting>,
}

/// `memory.scope` — whose records an instance reads and writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    /// One memory per project (the default).
    #[default]
    Project,
    /// One memory shared by every project.
    Global,
}

/// `memory.inject` — how much memory the system context carries.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryInjectInfo {
    /// The most records (default 20; 0 turns injection off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent: Option<u64>,
    /// The most characters of record content (default 4000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<u64>,
    /// Also inject records written by a compaction flush or from a session
    /// that carried outside text (default false: those are only searched).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unverified: Option<bool>,
}

/// `memory` — long-term memory records and transcript search.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryInfo {
    /// Off unless `true`: no tools, no injection, no index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<MemoryScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject: Option<MemoryInjectInfo>,
    /// Give the model one bounded chance to save memories before a
    /// compaction (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flush_before_compaction: Option<bool>,
}

/// `prompt.environment`: `true` (the default) keeps the `<env>` block,
/// `false` drops it, a string replaces it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PromptEnvironment {
    Switch(bool),
    Template(String),
}

/// `prompt` — the fixed additions to the system prompt, for a product whose
/// agent is not a coding agent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptInfo {
    /// The `<env>` block (working directory, git, platform, date). A string
    /// is used in its place, with `{{directory}}`, `{{worktree}}`,
    /// `{{git}}`, `{{platform}}`, `{{date}}`, `{{model}}` and
    /// `{{provider}}` filled in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<PromptEnvironment>,
    /// The "You are powered by the model named…" line (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_identity: Option<bool>,
    /// The skills paragraph when no skill exists (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_skills: Option<bool>,
}

/// `tool.<name>` — the wording of one tool's description.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Replaces the built-in description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Added after the description (the built-in one, or `description`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_append: Option<String>,
}

/// `websearch` — where the `websearch` tool sends its queries.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WebsearchInfo {
    /// `"libertai"` (LibertAI search, with the libertai provider's key) or
    /// `"http"` (`url` and `headers` below). Absent: upstream's choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// For `"http"`: the endpoint. It receives
    /// `POST { "query": …, "max_results": n }` and answers
    /// `{ "results": [{ "title", "url", "snippet" }] }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
}
