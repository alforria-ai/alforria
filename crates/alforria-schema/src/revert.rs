//! `schema-src/revert.ts`.

use serde::{Deserialize, Serialize};

use crate::ids::{MessageId, PartId};

/// openapi `FileDiff` (the revert file diff).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevertFileDiff {
    pub path: String, // RelativePath
    pub status: RevertFileStatus,
    pub additions: u64,
    pub deletions: u64,
    pub patch: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevertFileStatus {
    Added,
    Modified,
    Deleted,
}

/// openapi `RevertState`; required only `messageID`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevertState {
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    #[serde(rename = "partID", default, skip_serializing_if = "Option::is_none")]
    pub part_id: Option<PartId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<RevertFileDiff>>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::RevertState;

    #[test]
    fn revert_state_roundtrip_omits_optional_keys() {
        let value = json!({
            "messageID": "msg_01JDY",
        });
        let state: RevertState = serde_json::from_value(value.clone()).unwrap();
        let roundtrip = serde_json::to_value(&state).unwrap();
        assert_eq!(roundtrip, value);
        for key in ["partID", "snapshot", "diff", "files"] {
            assert!(roundtrip.get(key).is_none(), "{key} must be absent");
        }
    }

    #[test]
    fn revert_state_roundtrip_with_optionals() {
        let value = json!({
            "messageID": "msg_01JDY",
            "partID": "prt_01JDY",
            "snapshot": "snap",
            "diff": "@@ -1 +1 @@",
            "files": [
                {
                    "path": "src/lib.rs",
                    "status": "modified",
                    "additions": 1,
                    "deletions": 2,
                    "patch": "@@ -1 +1 @@",
                },
            ],
        });
        let state: RevertState = serde_json::from_value(value.clone()).unwrap();
        let roundtrip = serde_json::to_value(&state).unwrap();
        assert_eq!(roundtrip, value);
    }
}
