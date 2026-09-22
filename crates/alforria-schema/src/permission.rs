//! Wire DTOs for `schema-src/permission.ts` (`PermissionV2.*` in openapi).

use serde::{Deserialize, Serialize};

use crate::ids::{PermissionId, SessionId};
use crate::schema::JsonMap;

/// `PermissionV2.Source` — openapi `PermissionV2Source`.
///
/// Wire: `{"type":"tool","messageID":"…","callID":"…"}` (all three required).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum PermissionSource {
    #[serde(rename_all = "camelCase")]
    Tool {
        #[serde(rename = "messageID")]
        message_id: String,
        #[serde(rename = "callID")]
        call_id: String,
    },
}

/// `PermissionV2.Request` — openapi `PermissionV2Request`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub id: PermissionId,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub action: String,
    pub resources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PermissionSource>,
}

/// `PermissionV2.Reply` — openapi `PermissionV2Reply`: `"once" | "always" | "reject"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionReply {
    Once,
    Always,
    Reject,
}

/// `PermissionV2.Effect` — openapi `PermissionV2Effect`: `"allow" | "deny" | "ask"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionEffect {
    Allow,
    Deny,
    Ask,
}

/// `PermissionV2.Rule` — openapi `PermissionV2Rule`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    pub action: String,
    pub resource: String,
    pub effect: PermissionEffect,
}

/// `PermissionV2.Ruleset` — openapi `PermissionV2Ruleset`.
pub type PermissionRuleset = Vec<PermissionRule>;

/// `permission.v2.asked` payload — openapi `PermissionV2Asked.data`
/// (same fields as `PermissionRequest`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionV2AskedData {
    pub id: PermissionId,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub action: String,
    pub resources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PermissionSource>,
}

/// `permission.v2.replied` payload — openapi `PermissionV2Replied.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionV2RepliedData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "requestID")]
    pub request_id: PermissionId,
    pub reply: PermissionReply,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn permission_source_tool_wire_object() {
        let source = PermissionSource::Tool {
            message_id: "msg_1".to_string(),
            call_id: "call_1".to_string(),
        };
        let json = serde_json::to_value(&source).unwrap();
        assert_eq!(
            json,
            json!({"type": "tool", "messageID": "msg_1", "callID": "call_1"})
        );
        let back: PermissionSource =
            serde_json::from_value(serde_json::to_value(&source).unwrap()).unwrap();
        assert_eq!(source, back);
    }

    #[test]
    fn permission_request_omits_optional_keys() {
        let request = PermissionRequest {
            id: "per_1".to_string(),
            session_id: "ses_1".to_string(),
            action: "bash".to_string(),
            resources: vec!["rm -rf /".to_string()],
            save: None,
            metadata: None,
            source: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(
            json,
            json!({
                "id": "per_1",
                "sessionID": "ses_1",
                "action": "bash",
                "resources": ["rm -rf /"],
            })
        );
        let back: PermissionRequest =
            serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
        assert_eq!(request, back);
    }

    #[test]
    fn permission_reply_and_effect_values() {
        assert_eq!(
            serde_json::to_value(PermissionReply::Once).unwrap(),
            json!("once")
        );
        assert_eq!(
            serde_json::to_value(PermissionReply::Always).unwrap(),
            json!("always")
        );
        assert_eq!(
            serde_json::to_value(PermissionReply::Reject).unwrap(),
            json!("reject")
        );
        assert_eq!(
            serde_json::to_value(PermissionEffect::Allow).unwrap(),
            json!("allow")
        );
        assert_eq!(
            serde_json::to_value(PermissionEffect::Deny).unwrap(),
            json!("deny")
        );
        assert_eq!(
            serde_json::to_value(PermissionEffect::Ask).unwrap(),
            json!("ask")
        );
    }

    #[test]
    fn permission_replied_data_wire_shape() {
        let data = PermissionV2RepliedData {
            session_id: "ses_1".to_string(),
            request_id: "per_1".to_string(),
            reply: PermissionReply::Always,
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(
            json,
            json!({"sessionID": "ses_1", "requestID": "per_1", "reply": "always"})
        );
        let back: PermissionV2RepliedData =
            serde_json::from_value(serde_json::to_value(&data).unwrap()).unwrap();
        assert_eq!(data, back);
    }
}
