//! Wire DTOs for `schema-src/pty.ts` — openapi `Pty`, `PtyCreated`, `PtyUpdated`,
//! `PtyExited`, `PtyDeleted`.

use serde::{Deserialize, Serialize};

use crate::ids::PtyId;

/// `Pty.Info` — openapi `Pty`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyInfo {
    pub id: PtyId,
    pub title: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub status: PtyStatus,
    /// NonNegativeInt.
    pub pid: u64,
    /// NonNegativeInt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u64>,
}

/// PTY status — openapi `Pty.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PtyStatus {
    Running,
    Exited,
}

/// `pty.created` payload — openapi `PtyCreated.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyCreatedData {
    pub info: PtyInfo,
}

/// `pty.updated` payload — openapi `PtyUpdated.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyUpdatedData {
    pub info: PtyInfo,
}

/// `pty.exited` payload — openapi `PtyExited.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyExitedData {
    pub id: PtyId,
    /// NonNegativeInt.
    pub exit_code: u64,
}

/// `pty.deleted` payload — openapi `PtyDeleted.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyDeletedData {
    pub id: PtyId,
}

/// `Pty.CreateInput` — input DTO for creating a PTY.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyCreateInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<std::collections::BTreeMap<String, String>>,
}

/// `Pty.UpdateInput` — input DTO for updating a PTY.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyUpdateInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<PtySize>,
}

/// Terminal size — `{ rows, cols }` (PositiveInt both).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtySize {
    pub rows: u64,
    pub cols: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{PtyCreateInput, PtyInfo, PtySize, PtyStatus, PtyUpdateInput};

    #[test]
    fn pty_info_has_camelcase_exit_code() {
        let info = PtyInfo {
            id: "pty_1".to_string(),
            title: "shell".to_string(),
            command: "bash".to_string(),
            args: vec![],
            cwd: "/repo".to_string(),
            status: PtyStatus::Running,
            pid: 42,
            exit_code: Some(0),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json.get("exitCode"), Some(&json!(0)));
        let back: PtyInfo = serde_json::from_value(json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn pty_info_omits_exit_code_when_absent() {
        let info = PtyInfo {
            id: "pty_1".to_string(),
            title: "shell".to_string(),
            command: "bash".to_string(),
            args: vec![],
            cwd: "/repo".to_string(),
            status: PtyStatus::Running,
            pid: 42,
            exit_code: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert!(json.get("exitCode").is_none());
        let back: PtyInfo = serde_json::from_value(json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn pty_inputs_omit_optional_keys() {
        let create = PtyCreateInput {
            command: None,
            args: None,
            cwd: None,
            title: None,
            env: None,
        };
        let json = serde_json::to_value(&create).unwrap();
        assert_eq!(json, json!({}));
        let update = PtyUpdateInput {
            title: None,
            size: Some(PtySize { rows: 24, cols: 80 }),
        };
        let json = serde_json::to_value(&update).unwrap();
        assert_eq!(json, json!({"size": {"rows": 24, "cols": 80}}));
    }
}
