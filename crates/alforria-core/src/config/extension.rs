//! Config keys that only alforria has (the always-on assistant features).
//!
//! They live apart from `schema.rs`, which mirrors the upstream config, so
//! the two can change without touching each other.

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
