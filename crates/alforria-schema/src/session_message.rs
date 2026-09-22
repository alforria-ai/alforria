//! `schema-src/session-message.ts` — openapi `SessionMessage*`, `SessionErrorUnknown`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{MessageId, SessionId};
use crate::llm::{ProviderMetadata, ToolContent};
use crate::model::ModelRef;
use crate::prompt::{PromptAgentAttachment, PromptFileAttachment};
use crate::schema::{EpochMillis, JsonMap};
use crate::session::SessionTokens;

/// `Session.Message.Compaction.reason` — `"auto"` | `"manual"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    Auto,
    Manual,
}

/// `Session.Error.Unknown.type` — literal `"unknown"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionUnknownErrorType {
    Unknown,
}

/// openapi `SessionErrorUnknown`; required `type`, `message`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUnknownError {
    #[serde(rename = "type")]
    pub type_: SessionUnknownErrorType,
    pub message: String,
}

/// `Base.time` for most message variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageTime {
    pub created: EpochMillis,
}

/// `Session.Message.Shell.time`; required only `created`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellTime {
    pub created: EpochMillis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<EpochMillis>,
}

/// `Session.Message.Assistant.time`; required only `created`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantTime {
    pub created: EpochMillis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<EpochMillis>,
}

/// `Session.Message.Assistant.Reasoning.time`; required only `created`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningTime {
    pub created: EpochMillis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<EpochMillis>,
}

/// `Session.Message.Assistant.Tool.time`; required only `created`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolTime {
    pub created: EpochMillis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ran: Option<EpochMillis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<EpochMillis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pruned: Option<EpochMillis>,
}

/// `Session.Message.Assistant.snapshot`; all fields optional (openapi
/// `SessionMessageAssistant.snapshot` has no required properties).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>, // RelativePath
}

/// `Session.Message.Assistant.Tool.provider`; required only `executed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolProvider {
    pub executed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ProviderMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_metadata: Option<ProviderMetadata>,
}

/// `Session.Message.ToolState` union (openapi `SessionMessageToolState*`;
/// tag `status`: `pending` | `running` | `completed` | `error`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ToolState {
    /// openapi `SessionMessageToolStatePending` — v2 pending input is a
    /// plain string, not a map.
    Pending { input: String },
    Running {
        input: JsonMap,
        structured: JsonMap,
        content: Vec<ToolContent>,
    },
    Completed {
        input: JsonMap,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachments: Option<Vec<PromptFileAttachment>>,
        content: Vec<ToolContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_paths: Option<Vec<String>>,
        structured: JsonMap,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
    Error {
        input: JsonMap,
        content: Vec<ToolContent>,
        structured: JsonMap,
        error: SessionUnknownError,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
}

/// `Session.Message.AssistantContent` union (openapi `SessionMessageAssistantText`,
/// `SessionMessageAssistantReasoning`, `SessionMessageAssistantTool`;
/// tag `type`: `text` | `reasoning` | `tool`).
#[allow(clippy::large_enum_variant)] // wire shapes per spec; no benefit to boxing here
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AssistantContent {
    Text {
        id: String,
        text: String,
    },
    Reasoning {
        id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        time: Option<ReasoningTime>,
    },
    Tool {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<ToolProvider>,
        state: ToolState,
        time: ToolTime,
    },
}

/// `Session.Message` union (openapi `SessionMessage`; tag `type`).
///
/// Every variant carries the `Base` fields `id`, optional `metadata`, and
/// `time` — `Shell` and `Assistant` have their own `time` shapes
/// (`completed?` is optional).
#[allow(clippy::large_enum_variant)] // wire shapes per spec; no benefit to boxing here
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum SessionMessage {
    /// openapi `SessionMessageAgentSwitched`.
    AgentSwitched {
        id: MessageId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        time: MessageTime,
        agent: String,
    },
    /// openapi `SessionMessageModelSwitched`.
    ModelSwitched {
        id: MessageId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        time: MessageTime,
        model: ModelRef,
    },
    /// openapi `SessionMessageUser`.
    User {
        id: MessageId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        time: MessageTime,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        files: Option<Vec<PromptFileAttachment>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agents: Option<Vec<PromptAgentAttachment>>,
    },
    /// openapi `SessionMessageSynthetic`.
    Synthetic {
        id: MessageId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        time: MessageTime,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        text: String,
    },
    /// openapi `SessionMessageSystem`.
    System {
        id: MessageId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        time: MessageTime,
        text: String,
    },
    /// openapi `SessionMessageShell`.
    Shell {
        id: MessageId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        time: ShellTime,
        #[serde(rename = "callID")]
        call_id: String,
        command: String,
        output: String,
    },
    /// openapi `SessionMessageAssistant`.
    Assistant {
        id: MessageId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        time: AssistantTime,
        agent: String,
        model: ModelRef,
        content: Vec<AssistantContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot: Option<AssistantSnapshot>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(serialize_with = "crate::js_number::js_opt_f64")]
        cost: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens: Option<SessionTokens>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<SessionUnknownError>,
    },
    /// openapi `SessionMessageCompaction`.
    Compaction {
        id: MessageId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        time: MessageTime,
        reason: CompactionReason,
        summary: String,
        recent: String,
    },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        AssistantContent, CompactionReason, MessageTime, SessionMessage, SessionUnknownError,
        SessionUnknownErrorType, ShellTime, ToolState,
    };

    #[test]
    fn user_message_roundtrip() {
        let value = json!({
            "id": "msg_01JDY",
            "type": "user",
            "time": { "created": 1778031210000i64 },
            "text": "hello world",
        });
        let message: SessionMessage = serde_json::from_value(value.clone()).unwrap();
        let roundtrip = serde_json::to_value(&message).unwrap();
        assert_eq!(roundtrip, value);
        for key in ["metadata", "files", "agents"] {
            assert!(roundtrip.get(key).is_none(), "{key} must be absent");
        }
    }

    #[test]
    fn user_message_with_optionals_roundtrip() {
        let value = json!({
            "id": "msg_01JDY",
            "type": "user",
            "metadata": { "origin": "test" },
            "time": { "created": 1778031210000i64 },
            "text": "hello",
            "files": [{ "uri": "file:///tmp/a.txt", "mime": "text/plain" }],
            "agents": [{ "name": "build" }],
        });
        let message: SessionMessage = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&message).unwrap(), value);
    }

    #[test]
    fn tool_assistant_content_with_completed_state_roundtrip() {
        let value = json!({
            "type": "tool",
            "id": "prt_01",
            "name": "read",
            "provider": { "executed": true },
            "state": {
                "status": "completed",
                "input": { "path": "/tmp/a.txt" },
                "attachments": [{ "uri": "file:///tmp/a.txt", "mime": "text/plain" }],
                "content": [{ "type": "text", "text": "ok" }],
                "outputPaths": ["/tmp/a.txt"],
                "structured": {},
                "result": { "ok": true },
            },
            "time": {
                "created": 1778031210000i64,
                "ran": 1778031210100i64,
                "completed": 1778031210200i64,
            },
        });
        let content: AssistantContent = serde_json::from_value(value.clone()).unwrap();
        let roundtrip = serde_json::to_value(&content).unwrap();
        assert_eq!(roundtrip, value);
        let state = match &content {
            AssistantContent::Tool { state, .. } => state,
            _ => panic!("expected tool content"),
        };
        assert_eq!(state.status(), "completed");
    }

    #[test]
    fn error_state_omits_result_key() {
        let value = json!({
            "status": "error",
            "input": { "path": "/tmp/a.txt" },
            "content": [{ "type": "text", "text": "partial" }],
            "structured": {},
            "error": { "type": "unknown", "message": "boom" },
        });
        let state: ToolState = serde_json::from_value(value.clone()).unwrap();
        let roundtrip = serde_json::to_value(&state).unwrap();
        assert_eq!(roundtrip, value);
        // Optional `result` must be omitted, not serialized as null.
        assert!(roundtrip.get("result").is_none());
        assert!(roundtrip["error"].get("result").is_none());
    }

    #[test]
    fn kebab_case_tags_and_id_casings() {
        let message = SessionMessage::AgentSwitched {
            id: "msg_01JDY".to_string(),
            metadata: None,
            time: MessageTime {
                created: 1778031210000i64,
            },
            agent: "build".to_string(),
        };
        let roundtrip = serde_json::to_value(&message).unwrap();
        assert_eq!(roundtrip["type"], json!("agent-switched"));

        let message = SessionMessage::ModelSwitched {
            id: "msg_01JDY".to_string(),
            metadata: None,
            time: MessageTime {
                created: 1778031210000i64,
            },
            model: serde_json::from_value(json!({
                "id": "claude-sonnet-4-5",
                "providerID": "anthropic",
            }))
            .unwrap(),
        };
        let roundtrip = serde_json::to_value(&message).unwrap();
        assert_eq!(roundtrip["type"], json!("model-switched"));

        let message = SessionMessage::Synthetic {
            id: "msg_01JDY".to_string(),
            metadata: None,
            time: MessageTime {
                created: 1778031210000i64,
            },
            session_id: "ses_01JDY".to_string(),
            text: "synthetic".to_string(),
        };
        let roundtrip = serde_json::to_value(&message).unwrap();
        assert!(roundtrip.get("sessionID").is_some());
        assert!(roundtrip.get("sessionId").is_none());

        let message = SessionMessage::Shell {
            id: "msg_01JDY".to_string(),
            metadata: None,
            time: ShellTime {
                created: 1778031210000i64,
                completed: Some(1778031212000i64),
            },
            call_id: "call_01".to_string(),
            command: "ls".to_string(),
            output: "a.txt".to_string(),
        };
        let roundtrip = serde_json::to_value(&message).unwrap();
        assert!(roundtrip.get("callID").is_some());
        assert!(roundtrip.get("callId").is_none());
    }

    #[test]
    fn compaction_and_assistant_roundtrip() {
        let value = json!({
            "id": "msg_01JDY",
            "type": "compaction",
            "time": { "created": 1778031210000i64 },
            "reason": "manual",
            "summary": "summary text",
            "recent": "recent text",
        });
        let message: SessionMessage = serde_json::from_value(value.clone()).unwrap();
        let roundtrip = serde_json::to_value(&message).unwrap();
        assert_eq!(roundtrip, value);
        match &message {
            SessionMessage::Compaction { reason, .. } => {
                assert_eq!(*reason, CompactionReason::Manual);
            }
            _ => panic!("expected compaction message"),
        }

        let value = json!({
            "id": "msg_01JDZ",
            "type": "assistant",
            "time": { "created": 1778031210000i64, "completed": 1778031215000i64 },
            "agent": "build",
            "model": { "id": "claude-sonnet-4-5", "providerID": "anthropic" },
            "content": [
                { "type": "reasoning", "id": "reasoning_01", "text": "thinking..." },
            ],
            "finish": "stop",
            "cost": 0.001,
            "tokens": {
                "input": 10,
                "output": 5,
                "reasoning": 0,
                "cache": { "read": 0, "write": 0 },
            },
            "error": { "type": "unknown", "message": "boom" },
        });
        let message: SessionMessage = serde_json::from_value(value.clone()).unwrap();
        let roundtrip = serde_json::to_value(&message).unwrap();
        assert_eq!(roundtrip, value);
        for key in ["metadata", "snapshot"] {
            assert!(roundtrip.get(key).is_none(), "{key} must be absent");
        }
    }

    #[test]
    fn session_unknown_error_wire_shape() {
        let error = SessionUnknownError {
            type_: SessionUnknownErrorType::Unknown,
            message: "boom".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            json!({ "type": "unknown", "message": "boom" })
        );
    }

    impl ToolState {
        fn status(&self) -> &'static str {
            match self {
                ToolState::Pending { .. } => "pending",
                ToolState::Running { .. } => "running",
                ToolState::Completed { .. } => "completed",
                ToolState::Error { .. } => "error",
            }
        }
    }
}
