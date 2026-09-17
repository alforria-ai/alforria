//! Wire DTOs for `schema-src/vcs-event.ts` — openapi `VcsBranchUpdated`.

use serde::{Deserialize, Serialize};

/// `vcs.branch.updated` payload — openapi `VcsBranchUpdated.data`.
///
/// `branch` is `optional(Schema.String)`: omitted when the current branch is
/// unknown (e.g. detached HEAD).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsBranchUpdatedData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::VcsBranchUpdatedData;

    #[test]
    fn vcs_branch_updated_omits_branch_when_absent() {
        let data = VcsBranchUpdatedData { branch: None };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json, json!({}));
        assert!(json.get("branch").is_none());
    }

    #[test]
    fn vcs_branch_updated_roundtrip() {
        let data = VcsBranchUpdatedData {
            branch: Some("main".to_string()),
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json, json!({"branch": "main"}));
        let back: VcsBranchUpdatedData =
            serde_json::from_value(serde_json::to_value(&data).unwrap()).unwrap();
        assert_eq!(data, back);
    }
}
