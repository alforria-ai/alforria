//! `schema-src/session-status-event.ts` — openapi `SessionStatus`, `EventSessionStatus`,
//! `EventSessionIdle`.

use serde::{Deserialize, Serialize};

use crate::ids::SessionId;

/// `SessionStatusEvent.Info` `action` sub-object (openapi `SessionStatus`
/// `retry.action`; required all but `link`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryAction {
    pub reason: String,
    pub provider: String,
    pub title: String,
    pub message: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// `SessionStatusEvent.Info` (openapi `SessionStatus`): `idle` | `retry` | `busy`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SessionStatusInfo {
    Idle,
    #[serde(rename_all = "camelCase")]
    Retry {
        attempt: u64, // NonNegativeInt
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<RetryAction>,
        next: u64, // NonNegativeInt
    },
    Busy,
}

/// `"session.status"` event payload (openapi `EventSessionStatus.properties`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub status: SessionStatusInfo,
}

/// `"session.idle"` event payload (openapi `EventSessionIdle.properties`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdleData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{RetryAction, SessionIdleData, SessionStatusData, SessionStatusInfo};

    #[test]
    fn retry_status_with_action_roundtrip() {
        let value = json!({
            "sessionID": "ses_01JDY",
            "status": {
                "type": "retry",
                "attempt": 1,
                "message": "rate limited",
                "action": {
                    "reason": "429",
                    "provider": "anthropic",
                    "title": "Rate limited",
                    "message": "Backing off",
                    "label": "Retry",
                    "link": "https://docs",
                },
                "next": 30,
            },
        });
        let data: SessionStatusData = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&data).unwrap(), value);
    }

    #[test]
    fn retry_status_omits_optional_action_and_link() {
        let value = json!({
            "sessionID": "ses_01JDY",
            "status": { "type": "retry", "attempt": 1, "message": "rate limited", "next": 30 },
        });
        let data: SessionStatusData = serde_json::from_value(value.clone()).unwrap();
        let roundtrip = serde_json::to_value(&data).unwrap();
        assert_eq!(roundtrip, value);
        assert!(roundtrip["status"].get("action").is_none());

        let value = json!({
            "sessionID": "ses_01JDY",
            "status": {
                "type": "retry",
                "attempt": 1,
                "message": "rate limited",
                "action": {
                    "reason": "429",
                    "provider": "anthropic",
                    "title": "Rate limited",
                    "message": "Backing off",
                    "label": "Retry",
                },
                "next": 30,
            },
        });
        let data: SessionStatusData = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            data.status,
            SessionStatusInfo::Retry {
                attempt: 1,
                message: "rate limited".to_string(),
                action: Some(RetryAction {
                    reason: "429".to_string(),
                    provider: "anthropic".to_string(),
                    title: "Rate limited".to_string(),
                    message: "Backing off".to_string(),
                    label: "Retry".to_string(),
                    link: None,
                }),
                next: 30,
            }
        );
        let roundtrip = serde_json::to_value(&data).unwrap();
        assert_eq!(roundtrip, value);
        assert!(roundtrip["status"]["action"].get("link").is_none());
    }

    #[test]
    fn idle_and_busy_status_roundtrip() {
        assert_eq!(
            serde_json::to_value(SessionStatusInfo::Idle).unwrap(),
            json!({ "type": "idle" })
        );
        assert_eq!(
            serde_json::to_value(SessionStatusInfo::Busy).unwrap(),
            json!({ "type": "busy" })
        );
    }

    #[test]
    fn session_idle_data_roundtrip() {
        let value = json!({ "sessionID": "ses_01JDY" });
        let data: SessionIdleData = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&data).unwrap(), value);
    }
}
