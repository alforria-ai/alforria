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
