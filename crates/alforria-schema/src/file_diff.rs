//! `schema-src/file-diff.ts`.

use serde::{Deserialize, Serialize};

/// openapi `SnapshotFileDiff`; required only `additions`, `deletions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotFileDiff {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    #[serde(serialize_with = "crate::js_number::js_f64")]
    pub additions: f64,
    #[serde(serialize_with = "crate::js_number::js_f64")]
    pub deletions: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<FileDiffStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDiffStatus {
    Added,
    Deleted,
    Modified,
}
