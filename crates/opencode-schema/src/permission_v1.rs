//! Wire DTOs for `schema-src/v1/permission.ts` (openapi `Permission*`).

use serde::{Deserialize, Serialize};

use crate::ids::{PermissionId, ProjectId, SessionId};
use crate::schema::JsonMap;

/// openapi `PermissionAction`: `"allow" | "deny" | "ask"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionV1Action {
    Allow,
    Deny,
    Ask,
}

/// openapi `PermissionRule`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionV1Rule {
    pub permission: String,
    pub pattern: String,
    pub action: PermissionV1Action,
}

/// openapi `PermissionRuleset`.
pub type PermissionV1Ruleset = Vec<PermissionV1Rule>;

/// Inline `tool` sub-object of openapi `PermissionRequest`:
/// `{ messageID, callID }` (both required).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionV1Tool {
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "callID")]
    pub call_id: String,
}

/// openapi `PermissionRequest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionV1Request {
    pub id: PermissionId,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub permission: String,
    pub patterns: Vec<String>,
    pub metadata: JsonMap,
    pub always: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<PermissionV1Tool>,
}

/// `PermissionV1.Reply`: `"once" | "always" | "reject"`
/// (openapi `EventPermissionReplied.properties.reply`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionV1Reply {
    Once,
    Always,
    Reject,
}

/// `PermissionV1.ReplyBody` — NOT in openapi (schema-src only, spec STOP S5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionV1ReplyBody {
    pub reply: PermissionV1Reply,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `PermissionV1.Approval` — NOT in openapi (schema-src only, spec STOP S5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionV1Approval {
    #[serde(rename = "projectID")]
    pub project_id: ProjectId,
    pub patterns: Vec<String>,
}

/// `PermissionV1.AskInput` — NOT in openapi (schema-src only, spec STOP S5):
/// `Request` fields with an optional `id`, plus a required `ruleset`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionV1AskInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<PermissionId>,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub permission: String,
    pub patterns: Vec<String>,
    pub metadata: JsonMap,
    pub always: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<PermissionV1Tool>,
    pub ruleset: PermissionV1Ruleset,
}

/// `PermissionV1.ReplyInput` — NOT in openapi (schema-src only, spec STOP S5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionV1ReplyInput {
    #[serde(rename = "requestID")]
    pub request_id: PermissionId,
    pub reply: PermissionV1Reply,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `permission.asked` payload — openapi `EventPermissionAsked.properties`
/// (same fields as `PermissionV1Request`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionAskedData {
    pub id: PermissionId,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub permission: String,
    pub patterns: Vec<String>,
    pub metadata: JsonMap,
    pub always: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<PermissionV1Tool>,
}

/// `permission.replied` payload — openapi `EventPermissionReplied.properties`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRepliedData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "requestID")]
    pub request_id: PermissionId,
    pub reply: PermissionV1Reply,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn permission_request_round_trip() {
        let request = PermissionV1Request {
            id: "per_01J".to_string(),
            session_id: "ses_01JDY".to_string(),
            permission: "bash".to_string(),
            patterns: vec!["rm -rf /tmp/x".to_string()],
            metadata: json!({"command": "rm -rf /tmp/x"})
                .as_object()
                .unwrap()
                .clone(),
            always: vec!["allow".to_string()],
            tool: Some(PermissionV1Tool {
                message_id: "msg_01JDY".to_string(),
                call_id: "call_1".to_string(),
            }),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(
            json,
            json!({
                "id": "per_01J",
                "sessionID": "ses_01JDY",
                "permission": "bash",
                "patterns": ["rm -rf /tmp/x"],
                "metadata": {"command": "rm -rf /tmp/x"},
                "always": ["allow"],
                "tool": {"messageID": "msg_01JDY", "callID": "call_1"}
            })
        );
        let back: PermissionV1Request = serde_json::from_value(json).unwrap();
        assert_eq!(request, back);
    }

    #[test]
    fn permission_request_omits_optional_tool() {
        let request = PermissionV1Request {
            id: "per_01J".to_string(),
            session_id: "ses_01JDY".to_string(),
            permission: "bash".to_string(),
            patterns: vec![],
            metadata: serde_json::Map::new(),
            always: vec![],
            tool: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.get("tool").is_none(), "tool must be absent");
    }

    #[test]
    fn permission_action_and_reply_values() {
        assert_eq!(
            serde_json::to_value(PermissionV1Action::Allow).unwrap(),
            json!("allow")
        );
        assert_eq!(
            serde_json::to_value(PermissionV1Action::Deny).unwrap(),
            json!("deny")
        );
        assert_eq!(
            serde_json::to_value(PermissionV1Action::Ask).unwrap(),
            json!("ask")
        );
        assert_eq!(
            serde_json::to_value(PermissionV1Reply::Once).unwrap(),
            json!("once")
        );
        assert_eq!(
            serde_json::to_value(PermissionV1Reply::Always).unwrap(),
            json!("always")
        );
        assert_eq!(
            serde_json::to_value(PermissionV1Reply::Reject).unwrap(),
            json!("reject")
        );
    }

    #[test]
    fn permission_rule_wire_shape() {
        // openapi `PermissionRule` — spec STOP S3: permission stays a free string.
        let rule = PermissionV1Rule {
            permission: "bash".to_string(),
            pattern: "rm *".to_string(),
            action: PermissionV1Action::Allow,
        };
        let json = serde_json::to_value(&rule).unwrap();
        assert_eq!(
            json,
            json!({"permission": "bash", "pattern": "rm *", "action": "allow"})
        );
        let back: PermissionV1Rule = serde_json::from_value(json).unwrap();
        assert_eq!(rule, back);
    }

    #[test]
    fn permission_asked_and_replied_data_wire_shapes() {
        // openapi `EventPermissionAsked.properties` / `EventPermissionReplied.properties`.
        let asked = PermissionAskedData {
            id: "per_01J".to_string(),
            session_id: "ses_01JDY".to_string(),
            permission: "bash".to_string(),
            patterns: vec!["rm -rf /tmp/x".to_string()],
            metadata: serde_json::Map::new(),
            always: vec![],
            tool: None,
        };
        let json = serde_json::to_value(&asked).unwrap();
        assert_eq!(
            json,
            json!({
                "id": "per_01J",
                "sessionID": "ses_01JDY",
                "permission": "bash",
                "patterns": ["rm -rf /tmp/x"],
                "metadata": {},
                "always": []
            })
        );
        let back: PermissionAskedData = serde_json::from_value(json).unwrap();
        assert_eq!(asked, back);

        let replied = PermissionRepliedData {
            session_id: "ses_01JDY".to_string(),
            request_id: "per_01J".to_string(),
            reply: PermissionV1Reply::Reject,
        };
        assert_eq!(
            serde_json::to_value(&replied).unwrap(),
            json!({"sessionID": "ses_01JDY", "requestID": "per_01J", "reply": "reject"})
        );
        let back: PermissionRepliedData =
            serde_json::from_value(serde_json::to_value(&replied).unwrap()).unwrap();
        assert_eq!(replied, back);
    }

    #[test]
    fn permission_v1_input_dto_wire_shapes() {
        // NOT in openapi (schema-src only, spec STOP S5) — self-authored vectors.
        let ask_input = PermissionV1AskInput {
            id: None,
            session_id: "ses_01JDY".to_string(),
            permission: "bash".to_string(),
            patterns: vec!["rm *".to_string()],
            metadata: serde_json::Map::new(),
            always: vec![],
            tool: None,
            ruleset: vec![PermissionV1Rule {
                permission: "bash".to_string(),
                pattern: "rm *".to_string(),
                action: PermissionV1Action::Deny,
            }],
        };
        let json = serde_json::to_value(&ask_input).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.get("id").is_none(), "id must be absent when None");
        assert_eq!(json["ruleset"][0]["action"], json!("deny"));
        let back: PermissionV1AskInput = serde_json::from_value(json).unwrap();
        assert_eq!(ask_input, back);

        let reply_body = PermissionV1ReplyBody {
            reply: PermissionV1Reply::Always,
            message: Some("ok".to_string()),
        };
        assert_eq!(
            serde_json::to_value(&reply_body).unwrap(),
            json!({"reply": "always", "message": "ok"})
        );

        let reply_input = PermissionV1ReplyInput {
            request_id: "per_01J".to_string(),
            reply: PermissionV1Reply::Once,
            message: None,
        };
        assert_eq!(
            serde_json::to_value(&reply_input).unwrap(),
            json!({"requestID": "per_01J", "reply": "once"})
        );

        let approval = PermissionV1Approval {
            project_id: "global".to_string(),
            patterns: vec!["rm *".to_string()],
        };
        assert_eq!(
            serde_json::to_value(&approval).unwrap(),
            json!({"projectID": "global", "patterns": ["rm *"]})
        );
    }
}
