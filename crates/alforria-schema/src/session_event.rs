//! `schema-src/session-event.ts` — openapi `SessionNext*` family.
//!
//! One payload struct per `session.next.*` event type. Every payload repeats
//! the common `Base` fields (`timestamp`, `sessionID`); there is no struct
//! inheritance in serde. `SessionEvent` is the full 32-variant union,
//! `SessionDurableEvent` the durable subset (deltas excluded).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::{MessageId, SessionId};
use crate::llm::{ProviderMetadata, ToolContent};
use crate::location::LocationRef;
use crate::model::ModelRef;
use crate::prompt::Prompt;
use crate::revert::RevertState;
use crate::schema::{EpochMillis, JsonMap};
use crate::session::SessionTokens;
use crate::session_delivery::SessionDelivery;
use crate::session_message::{CompactionReason, SessionUnknownError};

/// `session.next.agent.switched` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSwitched {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    pub agent: String,
}

/// `session.next.model.switched` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSwitched {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    pub model: ModelRef,
}

/// `session.next.moved` payload; required only `location`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Moved {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub location: LocationRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdirectory: Option<String>, // RelativePath
}

/// `session.next.prompted` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prompted {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    pub prompt: Prompt,
    pub delivery: SessionDelivery,
}

/// `session.next.prompt.admitted` payload; same shape as `Prompted`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptAdmitted {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    pub prompt: Prompt,
    pub delivery: SessionDelivery,
}

/// `session.next.context.updated` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUpdated {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    pub text: String,
}

/// `session.next.synthetic` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Synthetic {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    pub text: String,
}

/// `session.next.shell.started` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellStarted {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    #[serde(rename = "callID")]
    pub call_id: String,
    pub command: String,
}

/// `session.next.shell.ended` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellEnded {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "callID")]
    pub call_id: String,
    pub output: String,
}

/// `session.next.step.started` payload; required all but `snapshot`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepStarted {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub agent: String,
    pub model: ModelRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
}

/// `session.next.step.ended` payload; required all but `snapshot`, `files`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepEnded {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub finish: String,
    #[serde(serialize_with = "crate::js_number::js_f64")]
    pub cost: f64,
    pub tokens: SessionTokens,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>, // RelativePath
}

/// `session.next.step.failed` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepFailed {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub error: SessionUnknownError,
}

/// `session.next.text.started` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextStarted {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    #[serde(rename = "textID")]
    pub text_id: String,
}

/// `session.next.text.delta` payload (live-only, not durable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDelta {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    #[serde(rename = "textID")]
    pub text_id: String,
    pub delta: String,
}

/// `session.next.text.ended` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEnded {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    #[serde(rename = "textID")]
    pub text_id: String,
    pub text: String,
}

/// `session.next.reasoning.started` payload; required all but `providerMetadata`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningStarted {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    #[serde(rename = "reasoningID")]
    pub reasoning_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
}

/// `session.next.reasoning.delta` payload (live-only, not durable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningDelta {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    #[serde(rename = "reasoningID")]
    pub reasoning_id: String,
    pub delta: String,
}

/// `session.next.reasoning.ended` payload; required all but `providerMetadata`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEnded {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    #[serde(rename = "reasoningID")]
    pub reasoning_id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
}

/// `session.next.tool.input.started` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInputStarted {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    #[serde(rename = "callID")]
    pub call_id: String,
    pub name: String,
}

/// `session.next.tool.input.delta` payload (live-only, not durable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInputDelta {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    #[serde(rename = "callID")]
    pub call_id: String,
    pub delta: String,
}

/// `session.next.tool.input.ended` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInputEnded {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    #[serde(rename = "callID")]
    pub call_id: String,
    pub text: String,
}

/// Tool-event `provider` sub-object (openapi `SessionNextToolCalled.data.provider`;
/// required only `executed`). Distinct from `session_message::ToolProvider`, which
/// additionally carries `resultMetadata`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolEventProvider {
    pub executed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ProviderMetadata>,
}

/// `session.next.tool.called` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCalled {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    #[serde(rename = "callID")]
    pub call_id: String,
    pub tool: String,
    pub input: JsonMap,
    pub provider: ToolEventProvider,
}

/// `session.next.tool.progress` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolProgress {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    #[serde(rename = "callID")]
    pub call_id: String,
    pub structured: JsonMap,
    pub content: Vec<ToolContent>,
}

/// `session.next.tool.success` payload; required all but `outputPaths`, `result`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSuccess {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    #[serde(rename = "callID")]
    pub call_id: String,
    pub structured: JsonMap,
    pub content: Vec<ToolContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    pub provider: ToolEventProvider,
}

/// `session.next.tool.failed` payload; `result` omitted when absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolFailed {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    #[serde(rename = "callID")]
    pub call_id: String,
    pub error: SessionUnknownError,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    pub provider: ToolEventProvider,
}

/// `session.next.retry_error` (openapi `SessionNextRetry_error`); required
/// `message`, `isRetryable`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(serialize_with = "crate::js_number::js_opt_f64")]
    pub status_code: Option<f64>,
    pub is_retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<std::collections::BTreeMap<String, String>>,
}

/// `session.next.retried` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Retried {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(serialize_with = "crate::js_number::js_f64")]
    pub attempt: f64,
    pub error: RetryError,
}

/// `session.next.compaction.started` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionStarted {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    pub reason: CompactionReason,
}

/// `session.next.compaction.delta` payload (live-only, not durable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDelta {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    pub text: String,
}

/// `session.next.compaction.ended` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEnded {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    pub reason: CompactionReason,
    pub text: String,
    pub recent: String,
}

/// `session.next.revert.staged` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevertStaged {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub revert: RevertState,
}

/// `session.next.revert.cleared` payload (base fields only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevertCleared {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
}

/// `session.next.revert.committed` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevertCommitted {
    pub timestamp: EpochMillis,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
}

/// All 32 `session.next.*` event types (durable + live-only deltas).
///
/// Wire shape matches TS `Schema.Union(...).pipe(Schema.toTaggedUnion("type"))`:
/// internally tagged — the discriminant sits alongside the payload fields
/// (`{"type": "session.next.moved", "timestamp": ..., "sessionID": ...}`).
/// The envelope unions (`Event`/`V2Event`) are the ones that wrap payloads in
/// `properties`/`data`; this union serializes inline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionEvent {
    #[serde(rename = "session.next.agent.switched")]
    AgentSwitched(AgentSwitched),
    #[serde(rename = "session.next.model.switched")]
    ModelSwitched(ModelSwitched),
    #[serde(rename = "session.next.moved")]
    Moved(Moved),
    #[serde(rename = "session.next.prompted")]
    Prompted(Prompted),
    #[serde(rename = "session.next.prompt.admitted")]
    PromptAdmitted(PromptAdmitted),
    #[serde(rename = "session.next.context.updated")]
    ContextUpdated(ContextUpdated),
    #[serde(rename = "session.next.synthetic")]
    Synthetic(Synthetic),
    #[serde(rename = "session.next.shell.started")]
    ShellStarted(ShellStarted),
    #[serde(rename = "session.next.shell.ended")]
    ShellEnded(ShellEnded),
    #[serde(rename = "session.next.step.started")]
    StepStarted(StepStarted),
    #[serde(rename = "session.next.step.ended")]
    StepEnded(StepEnded),
    #[serde(rename = "session.next.step.failed")]
    StepFailed(StepFailed),
    #[serde(rename = "session.next.text.started")]
    TextStarted(TextStarted),
    #[serde(rename = "session.next.text.delta")]
    TextDelta(TextDelta),
    #[serde(rename = "session.next.text.ended")]
    TextEnded(TextEnded),
    #[serde(rename = "session.next.reasoning.started")]
    ReasoningStarted(ReasoningStarted),
    #[serde(rename = "session.next.reasoning.delta")]
    ReasoningDelta(ReasoningDelta),
    #[serde(rename = "session.next.reasoning.ended")]
    ReasoningEnded(ReasoningEnded),
    #[serde(rename = "session.next.tool.input.started")]
    ToolInputStarted(ToolInputStarted),
    #[serde(rename = "session.next.tool.input.delta")]
    ToolInputDelta(ToolInputDelta),
    #[serde(rename = "session.next.tool.input.ended")]
    ToolInputEnded(ToolInputEnded),
    #[serde(rename = "session.next.tool.called")]
    ToolCalled(ToolCalled),
    #[serde(rename = "session.next.tool.progress")]
    ToolProgress(ToolProgress),
    #[serde(rename = "session.next.tool.success")]
    ToolSuccess(ToolSuccess),
    #[serde(rename = "session.next.tool.failed")]
    ToolFailed(ToolFailed),
    #[serde(rename = "session.next.retried")]
    Retried(Retried),
    #[serde(rename = "session.next.compaction.started")]
    CompactionStarted(CompactionStarted),
    #[serde(rename = "session.next.compaction.delta")]
    CompactionDelta(CompactionDelta),
    #[serde(rename = "session.next.compaction.ended")]
    CompactionEnded(CompactionEnded),
    #[serde(rename = "session.next.revert.staged")]
    RevertStaged(RevertStaged),
    #[serde(rename = "session.next.revert.cleared")]
    RevertCleared(RevertCleared),
    #[serde(rename = "session.next.revert.committed")]
    RevertCommitted(RevertCommitted),
}

/// The durable subset (28 variants): no `TextDelta`, `ReasoningDelta`,
/// `ToolInputDelta`, `CompactionDelta` (openapi `SessionDurableEvent`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionDurableEvent {
    #[serde(rename = "session.next.agent.switched")]
    AgentSwitched(AgentSwitched),
    #[serde(rename = "session.next.model.switched")]
    ModelSwitched(ModelSwitched),
    #[serde(rename = "session.next.moved")]
    Moved(Moved),
    #[serde(rename = "session.next.prompted")]
    Prompted(Prompted),
    #[serde(rename = "session.next.prompt.admitted")]
    PromptAdmitted(PromptAdmitted),
    #[serde(rename = "session.next.context.updated")]
    ContextUpdated(ContextUpdated),
    #[serde(rename = "session.next.synthetic")]
    Synthetic(Synthetic),
    #[serde(rename = "session.next.shell.started")]
    ShellStarted(ShellStarted),
    #[serde(rename = "session.next.shell.ended")]
    ShellEnded(ShellEnded),
    #[serde(rename = "session.next.step.started")]
    StepStarted(StepStarted),
    #[serde(rename = "session.next.step.ended")]
    StepEnded(StepEnded),
    #[serde(rename = "session.next.step.failed")]
    StepFailed(StepFailed),
    #[serde(rename = "session.next.text.started")]
    TextStarted(TextStarted),
    #[serde(rename = "session.next.text.ended")]
    TextEnded(TextEnded),
    #[serde(rename = "session.next.reasoning.started")]
    ReasoningStarted(ReasoningStarted),
    #[serde(rename = "session.next.reasoning.ended")]
    ReasoningEnded(ReasoningEnded),
    #[serde(rename = "session.next.tool.input.started")]
    ToolInputStarted(ToolInputStarted),
    #[serde(rename = "session.next.tool.input.ended")]
    ToolInputEnded(ToolInputEnded),
    #[serde(rename = "session.next.tool.called")]
    ToolCalled(ToolCalled),
    #[serde(rename = "session.next.tool.progress")]
    ToolProgress(ToolProgress),
    #[serde(rename = "session.next.tool.success")]
    ToolSuccess(ToolSuccess),
    #[serde(rename = "session.next.tool.failed")]
    ToolFailed(ToolFailed),
    #[serde(rename = "session.next.retried")]
    Retried(Retried),
    #[serde(rename = "session.next.compaction.started")]
    CompactionStarted(CompactionStarted),
    #[serde(rename = "session.next.compaction.ended")]
    CompactionEnded(CompactionEnded),
    #[serde(rename = "session.next.revert.staged")]
    RevertStaged(RevertStaged),
    #[serde(rename = "session.next.revert.cleared")]
    RevertCleared(RevertCleared),
    #[serde(rename = "session.next.revert.committed")]
    RevertCommitted(RevertCommitted),
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::{json, Value};

    use super::*;

    /// Minimal stand-in for the M1.11 v2 envelope: the union is flattened
    /// next to the envelope's own keys.
    #[derive(Debug, Serialize, Deserialize)]
    struct V2Envelope {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        #[serde(flatten)]
        event: SessionEvent,
    }

    #[test]
    fn retried_round_trips_in_v2_envelope() {
        let retried = Retried {
            timestamp: 1_762_000_000_000,
            session_id: "ses_test".to_string(),
            attempt: 2.0,
            error: RetryError {
                message: "rate limited".to_string(),
                status_code: Some(429.0),
                is_retryable: true,
                response_headers: None,
                response_body: None,
                metadata: None,
            },
        };
        let envelope = V2Envelope {
            id: "evt_test".to_string(),
            metadata: None,
            event: SessionEvent::Retried(retried),
        };

        let value = serde_json::to_value(&envelope).unwrap();
        let expected = json!({
                "id": "evt_test",
                "type": "session.next.retried",
                "timestamp": 1_762_000_000_000i64,
                "sessionID": "ses_test",
                "attempt": 2,
                "error": {
                    "message": "rate limited",
                    "statusCode": 429,
                    "isRetryable": true,
                },
        });
        assert_eq!(value, expected);

        let back: V2Envelope = serde_json::from_value(expected).unwrap();
        match back.event {
            SessionEvent::Retried(Retried { attempt, error, .. }) => {
                assert_eq!(attempt, 2.0);
                assert!(error.is_retryable);
                assert_eq!(error.status_code, Some(429.0));
            }
            _ => panic!("expected Retried"),
        }
    }

    #[test]
    fn moved_omits_optional_subdirectory() {
        let moved = Moved {
            timestamp: 1,
            session_id: "ses_test".to_string(),
            location: crate::location::LocationRef {
                directory: "/tmp".to_string(),
                workspace_id: None,
                project: None,
            },
            subdirectory: None,
        };
        let value = serde_json::to_value(&moved).unwrap();
        assert_eq!(
            value,
            json!({
                "timestamp": 1,
                "sessionID": "ses_test",
                "location": {"directory": "/tmp"},
            })
        );
    }

    #[test]
    fn tool_failed_omits_result_and_durable_excludes_deltas() {
        let failed = ToolFailed {
            timestamp: 1,
            session_id: "ses_test".to_string(),
            assistant_message_id: "msg_test".to_string(),
            call_id: "call_1".to_string(),
            error: SessionUnknownError {
                type_: crate::session_message::SessionUnknownErrorType::Unknown,
                message: "boom".to_string(),
            },
            result: None,
            provider: ToolEventProvider {
                executed: false,
                metadata: None,
            },
        };
        let value = serde_json::to_value(&failed).unwrap();
        assert!(value.get("result").is_none());
        assert_eq!(value["provider"], json!({"executed": false}));

        // The full union accepts the delta type…
        let delta = json!({
            "type": "session.next.text.delta",
            "timestamp": 1,
            "sessionID": "ses_test",
            "assistantMessageID": "msg_test",
            "textID": "text_1",
            "delta": "hi",
        });
        assert!(serde_json::from_value::<SessionEvent>(delta.clone()).is_ok());
        // …but the durable union does not.
        assert!(serde_json::from_value::<SessionDurableEvent>(delta).is_err());
    }

    #[test]
    fn tool_success_round_trips() {
        let success = ToolSuccess {
            timestamp: 1,
            session_id: "ses_test".to_string(),
            assistant_message_id: "msg_test".to_string(),
            call_id: "call_1".to_string(),
            structured: JsonMap::new(),
            content: vec![ToolContent::Text {
                text: "ok".to_string(),
            }],
            output_paths: Some(vec!["a.txt".to_string()]),
            result: Some(Value::Bool(true)),
            provider: ToolEventProvider {
                executed: true,
                metadata: None,
            },
        };
        let value = serde_json::to_value(&success).unwrap();
        assert_eq!(value["outputPaths"], json!(["a.txt"]));
        assert_eq!(value["result"], json!(true));
        let back: ToolSuccess = serde_json::from_value(value).unwrap();
        assert_eq!(back, success);
    }
}
