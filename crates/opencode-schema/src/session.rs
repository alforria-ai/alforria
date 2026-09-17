//! `schema-src/session.ts` — openapi `SessionV2Info`.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, ProjectId, SessionId};
use crate::location::LocationRef;
use crate::model::ModelRef;
use crate::revert::RevertState;
use crate::schema::EpochMillis;

/// `Session.Info.tokens` (openapi `SessionV2Info.tokens`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTokens {
    pub input: f64,
    pub output: f64,
    pub reasoning: f64,
    pub cache: SessionTokensCache,
}

/// `Session.Info.tokens.cache` (openapi `SessionV2Info.tokens.cache`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTokensCache {
    pub read: f64,
    pub write: f64,
}

/// `Session.Info.time` (openapi `SessionV2Info.time`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTime {
    pub created: EpochMillis,
    pub updated: EpochMillis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<EpochMillis>,
}

/// `Session.Info` (openapi `SessionV2Info`; required: `id`, `projectID`, `cost`,
/// `tokens`, `time`, `title`, `location`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: SessionId,
    #[serde(rename = "parentID", default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<SessionId>,
    #[serde(rename = "projectID")]
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    pub cost: f64,
    pub tokens: SessionTokens,
    pub time: SessionTime,
    pub title: String,
    pub location: LocationRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>, // RelativePath
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revert: Option<RevertState>,
}

/// `Session.ListAnchor.direction` — `"previous"` | `"next"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionListDirection {
    Previous,
    Next,
}

/// `Session.ListAnchor` — absent from openapi.json (spec S5); implemented from
/// `fixtures/schema-src/session.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListAnchor {
    pub id: SessionId,
    pub time: f64, // Schema.Finite in session.ts
    pub direction: SessionListDirection,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{SessionInfo, SessionListAnchor, SessionListDirection};

    #[test]
    fn session_info_roundtrip_omits_optional_keys() {
        let value = json!({
            "id": "ses_01JDY",
            "projectID": "global",
            "cost": 0.001,
            "tokens": {
                "input": 10.0,
                "output": 5.0,
                "reasoning": 0.0,
                "cache": { "read": 0.0, "write": 0.0 },
            },
            "time": { "created": 1778031210000i64, "updated": 1778031211000i64 },
            "title": "hello",
            "location": { "directory": "/repo", "workspaceID": "wrk_1" },
        });
        let info: SessionInfo = serde_json::from_value(value.clone()).unwrap();
        let roundtrip = serde_json::to_value(&info).unwrap();
        assert_eq!(roundtrip, value);
        for key in ["parentID", "agent", "model", "subpath", "revert"] {
            assert!(roundtrip.get(key).is_none(), "{key} must be absent");
        }
    }

    #[test]
    fn session_info_roundtrip_with_optionals() {
        let value = json!({
            "id": "ses_01JDY",
            "parentID": "ses_01JDX",
            "projectID": "global",
            "agent": "build",
            "model": { "id": "claude-sonnet-4-5", "providerID": "anthropic" },
            "cost": 0.0,
            "tokens": {
                "input": 10.0,
                "output": 5.0,
                "reasoning": 0.0,
                "cache": { "read": 0.0, "write": 0.0 },
            },
            "time": {
                "created": 1778031210000i64,
                "updated": 1778031211000i64,
                "archived": 1778031212000i64,
            },
            "title": "hello",
            "location": { "directory": "/repo" },
            "subpath": "src",
            "revert": {
                "messageID": "msg_01JDY",
                "partID": "prt_01JDY",
                "snapshot": "snap",
                "diff": "@@ -1 +1 @@",
            },
        });
        let info: SessionInfo = serde_json::from_value(value.clone()).unwrap();
        let roundtrip = serde_json::to_value(&info).unwrap();
        assert_eq!(roundtrip, value);
    }

    #[test]
    fn session_list_anchor_roundtrip() {
        // Not present in openapi.json — self-authored vector (spec S5).
        let value = json!({
            "id": "ses_01JDY",
            "time": 1778031210000.0,
            "direction": "previous",
        });
        let anchor: SessionListAnchor = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(anchor.direction, SessionListDirection::Previous);
        assert_eq!(serde_json::to_value(&anchor).unwrap(), value);
    }
}
