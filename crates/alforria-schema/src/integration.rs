//! `schema-src/integration.ts` + `schema-src/integration-id.ts`.

use serde::{Deserialize, Serialize};

use crate::connection::ConnectionInfo;
use crate::ids::{IntegrationAttemptId, IntegrationId, IntegrationMethodId};

/// Comparison operator of `Integration.When` (openapi `IntegrationWhen`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationWhenOp {
    Eq,
    Neq,
}

/// `Integration.When` (openapi `IntegrationWhen`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationWhen {
    pub key: String,
    pub op: IntegrationWhenOp,
    pub value: String,
}

/// Option entry of `Integration.SelectPrompt`'s `options` array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationSelectOption {
    pub label: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// `Integration.Prompt` (openapi `IntegrationTextPrompt` / `IntegrationSelectPrompt`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum IntegrationPrompt {
    /// `Integration.TextPrompt` (openapi `IntegrationTextPrompt`).
    Text {
        key: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        when: Option<IntegrationWhen>,
    },
    /// `Integration.SelectPrompt` (openapi `IntegrationSelectPrompt`).
    Select {
        key: String,
        message: String,
        options: Vec<IntegrationSelectOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        when: Option<IntegrationWhen>,
    },
}

/// `Integration.Method` (openapi `IntegrationMethod`): tagged union over
/// `Integration.OAuthMethod`, `Integration.KeyMethod` and `Integration.EnvMethod`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum IntegrationMethod {
    /// `Integration.OAuthMethod` (openapi `IntegrationOAuthMethod`).
    Oauth {
        id: IntegrationMethodId,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompts: Option<Vec<IntegrationPrompt>>,
    },
    /// `Integration.KeyMethod` (openapi `IntegrationKeyMethod`).
    Key {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// `Integration.EnvMethod` (openapi `IntegrationEnvMethod`).
    Env { names: Vec<String> },
}

/// `Integration.Info` (openapi `IntegrationInfo`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationInfo {
    pub id: IntegrationId,
    pub name: String,
    pub methods: Vec<IntegrationMethod>,
    pub connections: Vec<ConnectionInfo>,
}

/// `Integration.Ref` (openapi `IntegrationRef`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationRef {
    pub id: IntegrationId,
    pub name: String,
}

/// `Integration.Inputs` (openapi `IntegrationInputs`): `Record<String, String>`.
pub type IntegrationInputs = std::collections::BTreeMap<String, String>;

/// Mode of `Integration.Attempt` (openapi `IntegrationAttempt`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationAttemptMode {
    Auto,
    Code,
}

/// Time struct of `Integration.Attempt` and `Integration.AttemptStatus`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationAttemptTime {
    #[serde(serialize_with = "crate::js_number::js_f64")]
    pub created: f64,
    #[serde(serialize_with = "crate::js_number::js_f64")]
    pub expires: f64,
}

/// `Integration.Attempt` (openapi `IntegrationAttempt`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationAttempt {
    #[serde(rename = "attemptID")]
    pub attempt_id: IntegrationAttemptId,
    pub url: String,
    pub instructions: String,
    pub mode: IntegrationAttemptMode,
    pub time: IntegrationAttemptTime,
}

/// `Integration.AttemptStatus` (openapi `IntegrationAttemptStatus`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum IntegrationAttemptStatus {
    Pending {
        time: IntegrationAttemptTime,
    },
    Complete {
        time: IntegrationAttemptTime,
    },
    Failed {
        message: String,
        time: IntegrationAttemptTime,
    },
    Expired {
        time: IntegrationAttemptTime,
    },
}

/// Payload of the `integration.updated` event (openapi `IntegrationUpdated` data).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationUpdatedData {}

/// Payload of the `integration.connection.updated` event
/// (openapi `IntegrationConnectionUpdated` data).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationConnectionUpdatedData {
    #[serde(rename = "integrationID")]
    pub integration_id: IntegrationId,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        IntegrationAttempt, IntegrationAttemptMode, IntegrationAttemptStatus,
        IntegrationAttemptTime, IntegrationMethod, IntegrationPrompt,
    };

    #[test]
    fn oauth_method_with_select_prompt_roundtrips() {
        let value = json!({
            "type": "oauth",
            "id": "github",
            "label": "GitHub",
            "prompts": [
                {
                    "type": "select",
                    "key": "account",
                    "message": "Which account?",
                    "options": [
                        {"label": "Personal", "value": "personal"},
                        {"label": "Work", "value": "work", "hint": "Work account"},
                    ],
                    "when": {"key": "kind", "op": "eq", "value": "cloud"},
                },
            ],
        });
        let method: IntegrationMethod = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(method, IntegrationMethod::Oauth { .. }));
        assert_eq!(serde_json::to_value(&method).unwrap(), value);
    }

    #[test]
    fn attempt_serializes_attempt_id_casing() {
        let attempt = IntegrationAttempt {
            attempt_id: "con_123".into(),
            url: "https://example.com/oauth/authorize".into(),
            instructions: "Visit the URL".into(),
            mode: IntegrationAttemptMode::Auto,
            time: IntegrationAttemptTime {
                created: 1.0,
                expires: 2.0,
            },
        };
        let roundtrip = serde_json::to_value(&attempt).unwrap();
        assert_eq!(
            roundtrip,
            json!({
                "attemptID": "con_123",
                "url": "https://example.com/oauth/authorize",
                "instructions": "Visit the URL",
                "mode": "auto",
                "time": {"created": 1, "expires": 2},
            }),
        );
        assert!(roundtrip.get("attemptID").is_some());
        assert!(roundtrip.get("attemptId").is_none());
    }

    #[test]
    fn attempt_status_failed_roundtrips() {
        let value = json!({
            "status": "failed",
            "message": "code expired",
            "time": {"created": 1, "expires": 2},
        });
        let status: IntegrationAttemptStatus = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(status, IntegrationAttemptStatus::Failed { .. }));
        assert_eq!(serde_json::to_value(&status).unwrap(), value);
    }

    #[test]
    fn prompt_text_roundtrips_and_omits_optional_keys() {
        let value = json!({
            "type": "text",
            "key": "region",
            "message": "Which region?",
        });
        let prompt: IntegrationPrompt = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(prompt, IntegrationPrompt::Text { .. }));
        let roundtrip = serde_json::to_value(&prompt).unwrap();
        assert_eq!(roundtrip, value);
        assert!(roundtrip.get("placeholder").is_none());
        assert!(roundtrip.get("when").is_none());
    }
}
