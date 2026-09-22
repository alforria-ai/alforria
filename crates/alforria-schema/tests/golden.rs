//! Golden round-trip harness — spec M1 §6 vectors (V1–V16) and the §2.5
//! per-variant union samples.
//!
//! Every vector is deserialized into its target type, serialized back, and
//! compared with `serde_json::Value` equality (never string comparison —
//! f64 formatting and key order are documented deviations). Optional keys
//! must be ABSENT (not `null`) when the source omitted them.
//!
//! Vector authoring deviations from the spec text (recorded, minimal):
//!
//! - f64-typed fields carry an explicit `.0` in the literals (e.g. `10.0`
//!   where §6 writes `10`): serde_json `Value` equality distinguishes
//!   integer and float numbers, so the literal must match the Rust type.
//! - V6's attachment gains the required `mime` key (openapi
//!   `PromptFileAttachment` requires `uri` + `mime`).

use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use alforria_schema::connection::ConnectionInfo;
use alforria_schema::credential::CredentialValue;
use alforria_schema::event::LegacyEnvelope;
use alforria_schema::event::V2Envelope;
use alforria_schema::event_manifest::Event;
use alforria_schema::event_manifest::V2Event;
use alforria_schema::integration::{
    IntegrationAttemptStatus, IntegrationMethod, IntegrationPrompt,
};
use alforria_schema::llm::ToolContent;
use alforria_schema::location::LocationRef;
use alforria_schema::model::ModelApi;
use alforria_schema::permission::PermissionRequest;
use alforria_schema::provider::ProviderApi;
use alforria_schema::pty_ticket::PtyTicketConnectToken;
use alforria_schema::reference::ReferenceSource;
use alforria_schema::session::SessionInfo;
use alforria_schema::session_message::{AssistantContent, SessionMessage, ToolState};
use alforria_schema::session_status::SessionStatusInfo;
use alforria_schema::session_v1::{
    AssistantError, OutputFormat, V1FilePartSource, V1Message, V1Part, V1ToolState,
};
use alforria_schema::skill::SkillSource;

/// Deserializes `value` into `T`, serializes back, and asserts
/// `serde_json::Value` equality with the input.
fn round_trip<T>(value: Value)
where
    T: DeserializeOwned + serde::Serialize,
{
    let parsed: T =
        serde_json::from_value(value.clone()).unwrap_or_else(|e| panic!("deserialize failed: {e}"));
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(back, value);
}

/// Asserts a key is absent (omitted), never `null`.
fn assert_absent(value: &Value, key: &str) {
    assert!(
        value.get(key).is_none(),
        "key `{key}` must be absent, not null (got {:?})",
        value.get(key)
    );
}

// ---------------------------------------------------------------- V1

/// V1 — `LocationRef` (component `LocationRef`).
#[test]
fn v1_location_ref() {
    let value = json!({ "directory": "/home/jon/repo", "workspaceID": "wrk_abc" });
    round_trip::<LocationRef>(value);

    let minimal = json!({ "directory": "/home/jon/repo" });
    let parsed: LocationRef = serde_json::from_value(minimal.clone()).unwrap();
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(back, minimal);
    assert_absent(&back, "workspaceID");
}

// ---------------------------------------------------------------- V2

/// V2 — v2 envelope, `session.next.text.started` (component
/// `SessionNextTextStarted`).
#[test]
fn v2_v2_envelope_text_started() {
    let value = json!({
        "id": "evt_01JDY",
        "type": "session.next.text.started",
        "data": {
            "timestamp": 1778031210000i64,
            "sessionID": "ses_01JDY",
            "assistantMessageID": "msg_01JDY",
            "textID": "txt_1",
        },
    });
    let envelope: V2Envelope<V2Event> = serde_json::from_value(value.clone()).unwrap();
    let back = serde_json::to_value(&envelope).unwrap();
    assert_eq!(back, value);
    for key in ["metadata", "durable", "location"] {
        assert_absent(&back, key);
    }
}

// ---------------------------------------------------------------- V3

/// V3 — v2 envelope with metadata/durable/location, `session.next.tool.failed`
/// (components `SessionNextToolFailed` + `SessionNextToolFailed` data).
#[test]
fn v3_v2_envelope_tool_failed() {
    let value = json!({
        "id": "evt_01JDZ",
        "metadata": { "origin": "test" },
        "durable": { "aggregateID": "ses_01JDY", "seq": 12, "version": 1 },
        "location": { "directory": "/repo", "workspaceID": "wrk_1" },
        "type": "session.next.tool.failed",
        "data": {
            "timestamp": 1778031210000i64,
            "sessionID": "ses_01JDY",
            "assistantMessageID": "msg_01JDY",
            "callID": "call_1",
            "error": { "type": "unknown", "message": "boom" },
            "provider": { "executed": true, "metadata": { "a": { "b": "c" } } },
        },
    });
    let envelope: V2Envelope<V2Event> = serde_json::from_value(value.clone()).unwrap();
    let back = serde_json::to_value(&envelope).unwrap();
    assert_eq!(back, value);
    // `result` is optional and must be OMITTED, proving omit-vs-null.
    assert_absent(&back["data"], "result");
}

// ---------------------------------------------------------------- V4

/// V4 — legacy envelope, `session.status` with retry status
/// (component `EventSessionStatus`).
#[test]
fn v4_legacy_envelope_session_status() {
    let value = json!({
        "id": "evt_01JE0",
        "type": "session.status",
        "properties": {
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
        },
    });
    round_trip::<LegacyEnvelope<Event>>(value);
}

// ---------------------------------------------------------------- V5

/// V5 — v2 `user` message (component `SessionMessageUser`).
#[test]
fn v5_session_message_user() {
    let value = json!({
        "id": "msg_01JDY",
        "time": { "created": 1778031210000i64 },
        "text": "hello",
        "files": [
            {
                "uri": "file:///tmp/a.txt",
                "mime": "text/plain",
                "name": "a.txt",
                "source": { "start": 0, "end": 5, "text": "hello" },
            },
        ],
        "type": "user",
    });
    round_trip::<SessionMessage>(value);
}

// ---------------------------------------------------------------- V6

/// V6 — v2 `assistant` message with tool content (components
/// `SessionMessageAssistant`, `SessionMessageAssistantTool`,
/// `SessionMessageToolStateCompleted`).
#[test]
fn v6_session_message_assistant_tool() {
    let value = json!({
        "id": "msg_01JDY",
        "time": { "created": 1778031210000i64 },
        "type": "assistant",
        "agent": "build",
        "model": { "id": "claude-sonnet-4-5", "providerID": "anthropic" },
        "content": [
            {
                "type": "tool",
                "id": "tool_1",
                "name": "bash",
                "state": {
                    "status": "completed",
                    "input": { "command": "ls" },
                    "content": [ { "type": "text", "text": "file.txt" } ],
                    "structured": {},
                    "attachments": [
                        {
                            "uri": "file:///tmp/out",
                            "mime": "text/plain",
                            "name": "out",
                        },
                    ],
                },
                "time": { "created": 1778031210000i64 },
            },
        ],
    });
    let message: SessionMessage = serde_json::from_value(value.clone()).unwrap();
    let back = serde_json::to_value(&message).unwrap();
    assert_eq!(back, value);
    for key in ["metadata", "snapshot", "finish", "cost", "tokens", "error"] {
        assert_absent(&back, key);
    }
    match &message {
        SessionMessage::Assistant { content, .. } => match content.as_slice() {
            [AssistantContent::Tool {
                state: ToolState::Completed { .. },
                ..
            }] => {}
            other => panic!("expected completed tool content, got {other:?}"),
        },
        other => panic!("expected assistant message, got {other:?}"),
    }
}

// ---------------------------------------------------------------- V7

/// V7 — legacy envelope, `message.part.updated` (component
/// `EventMessagePartUpdated`).
#[test]
fn v7_legacy_envelope_message_part_updated() {
    let value = json!({
        "id": "evt_01JE1",
        "type": "message.part.updated",
        "properties": {
            "sessionID": "ses_01JDY",
            "time": 1778031210000i64,
            "part": {
                "id": "prt_01J",
                "sessionID": "ses_01JDY",
                "messageID": "msg_01JDY",
                "type": "text",
                "text": "hi",
            },
        },
    });
    round_trip::<LegacyEnvelope<Event>>(value);
}

// ---------------------------------------------------------------- V8

/// V8 — v1 assistant message with APIError (components `AssistantMessage`,
/// `APIError`).
#[test]
fn v8_v1_assistant_message_api_error() {
    let value = json!({
        "id": "msg_01JDY",
        "sessionID": "ses_01JDY",
        "role": "assistant",
        "time": { "created": 1778031210000i64 },
        "error": {
            "name": "APIError",
            "data": { "message": "upstream 500", "statusCode": 500, "isRetryable": true },
        },
        "parentID": "msg_01JDX",
        "modelID": "claude-sonnet-4-5",
        "providerID": "anthropic",
        "mode": "primary",
        "agent": "build",
        "path": { "cwd": "/repo", "root": "/repo" },
        "cost": 0.001,
        "tokens": {
            "input": 10,
            "output": 5,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 },
        },
    });
    round_trip::<V1Message>(value);
}

// ---------------------------------------------------------------- V9

/// V9 — v1 ToolPart, completed state (components `ToolPart`,
/// `ToolStateCompleted`).
#[test]
fn v9_v1_tool_part_completed() {
    let value = json!({
        "id": "prt_01J",
        "sessionID": "ses_01JDY",
        "messageID": "msg_01JDY",
        "type": "tool",
        "callID": "call_1",
        "tool": "bash",
        "state": {
            "status": "completed",
            "input": { "command": "ls" },
            "output": "file.txt",
            "title": "ls",
            "metadata": {},
            "time": { "start": 1778031210000i64, "end": 1778031211000i64 },
        },
    });
    round_trip::<V1Part>(value);
}

// ---------------------------------------------------------------- V10

/// V10 — v1 CompactionPart, snake_case `tail_start_id` (component
/// `CompactionPart`).
#[test]
fn v10_v1_compaction_part_tail_start_id() {
    let value = json!({
        "id": "prt_01J",
        "sessionID": "ses_01JDY",
        "messageID": "msg_01JDY",
        "type": "compaction",
        "auto": true,
        "overflow": false,
        "tail_start_id": "msg_01JDX",
    });
    round_trip::<V1Part>(value);
}

// ---------------------------------------------------------------- V11

/// V11 — PTY connect token, snake_case `expires_in` (component
/// `PtyTicketConnectToken`).
#[test]
fn v11_pty_ticket_connect_token() {
    let value = json!({ "ticket": "abc", "expires_in": 300 });
    round_trip::<PtyTicketConnectToken>(value);
}

// ---------------------------------------------------------------- V12

/// V12 — legacy envelope, `permission.v2.asked` (component
/// `EventPermissionV2Asked`).
#[test]
fn v12_legacy_envelope_permission_v2_asked() {
    let value = json!({
        "id": "evt_01JE2",
        "type": "permission.v2.asked",
        "properties": {
            "id": "per_01J",
            "sessionID": "ses_01JDY",
            "action": "bash",
            "resources": [ "rm -rf /tmp/x" ],
            "save": [ "always" ],
            "source": { "type": "tool", "messageID": "msg_01JDY", "callID": "call_1" },
        },
    });
    round_trip::<LegacyEnvelope<Event>>(value);
}

// ---------------------------------------------------------------- V13

/// V13 — v2 envelope, `tui.toast.show` without `duration` (component
/// `TuiToastShow`).
#[test]
fn v13_v2_envelope_tui_toast_show() {
    let value = json!({
        "id": "evt_01JE3",
        "type": "tui.toast.show",
        "data": { "message": "done", "variant": "success" },
    });
    let envelope: V2Envelope<V2Event> = serde_json::from_value(value.clone()).unwrap();
    let back = serde_json::to_value(&envelope).unwrap();
    assert_eq!(back, value);
    assert_absent(&back["data"], "title");
    assert_absent(&back["data"], "duration");
}

// ---------------------------------------------------------------- V14

/// V14 — Integration OAuth method (components `IntegrationOAuthMethod`,
/// `IntegrationTextPrompt`).
#[test]
fn v14_integration_oauth_method() {
    let value = json!({
        "id": "github",
        "type": "oauth",
        "label": "GitHub",
        "prompts": [
            {
                "type": "text",
                "key": "pat",
                "message": "Paste token",
                "placeholder": "token",
            },
        ],
    });
    round_trip::<IntegrationMethod>(value);
}

// ---------------------------------------------------------------- V15

/// V15 — Model native api variant (component `ModelApi`).
#[test]
fn v15_model_api_native() {
    let value = json!({
        "id": "claude-sonnet-4-5",
        "type": "native",
        "settings": {},
    });
    round_trip::<ModelApi>(value);
}

// ---------------------------------------------------------------- V16

/// V16 — legacy envelope, `session.next.prompted` (component
/// `EventSessionNextPrompted`).
#[test]
fn v16_legacy_envelope_session_next_prompted() {
    let value = json!({
        "id": "evt_01JE4",
        "type": "session.next.prompted",
        "properties": {
            "timestamp": 1778031210000i64,
            "sessionID": "ses_01JDY",
            "messageID": "msg_01JDY",
            "prompt": { "text": "hi" },
            "delivery": "steer",
        },
    });
    round_trip::<LegacyEnvelope<Event>>(value);
}

// ---------------------------------------------------------------------
// §2.5 per-variant samples — LLM.ToolContent
// ---------------------------------------------------------------------

#[test]
fn union_tool_content() {
    let text = json!({ "type": "text", "text": "hello" });
    round_trip::<ToolContent>(text);

    let file = json!({
        "type": "file",
        "uri": "file:///tmp/a.txt",
        "mime": "text/plain",
        "name": "a.txt",
    });
    round_trip::<ToolContent>(file);

    // `name` is optional and must be omitted when absent.
    let bare = json!({ "type": "file", "uri": "file:///tmp/a.txt", "mime": "text/plain" });
    let parsed: ToolContent = serde_json::from_value(bare.clone()).unwrap();
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(back, bare);
    assert_absent(&back, "name");
}

// ---------------------------------------------------------------------
// §2.5 per-variant samples — Connection.Info / Credential.Value
// ---------------------------------------------------------------------

#[test]
fn union_connection_info() {
    let credential = json!({ "type": "credential", "id": "cred_123", "label": "my-key" });
    round_trip::<ConnectionInfo>(credential);

    let env = json!({ "type": "env", "name": "ANTHROPIC_API_KEY" });
    round_trip::<ConnectionInfo>(env);
}

#[test]
fn union_credential_value() {
    let oauth = json!({
        "type": "oauth",
        "methodID": "github",
        "refresh": "r",
        "access": "a",
        "expires": 123,
    });
    let parsed: CredentialValue = serde_json::from_value(oauth.clone()).unwrap();
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(back, oauth);
    assert_absent(&back, "metadata");

    let key = json!({
        "type": "key",
        "key": "sk-abc",
        "metadata": { "hint": "value" },
    });
    round_trip::<CredentialValue>(key);
}

// ---------------------------------------------------------------------
// §2.5 per-variant samples — Provider.Api / Model.Api
// ---------------------------------------------------------------------

#[test]
fn union_provider_api() {
    let aisdk = json!({ "type": "aisdk", "package": "@ai-sdk/openai" });
    round_trip::<ProviderApi>(aisdk);

    let native = json!({
        "type": "native",
        "url": "https://api.example.com",
        "settings": { "baseURL": "https://api.example.com" },
    });
    round_trip::<ProviderApi>(native);
}

#[test]
fn union_model_api() {
    let aisdk = json!({
        "type": "aisdk",
        "id": "gpt-4o",
        "package": "@ai-sdk/openai",
    });
    round_trip::<ModelApi>(aisdk);

    // V15, kept here too — openapi `ModelApi` variant 2 requires id/type/settings.
    let native = json!({ "id": "claude-sonnet-4-5", "type": "native", "settings": {} });
    round_trip::<ModelApi>(native);
}

// ---------------------------------------------------------------------
// §2.5 per-variant samples — Integration.Prompt / Method / AttemptStatus
// ---------------------------------------------------------------------

#[test]
fn union_integration_prompt() {
    let text = json!({ "type": "text", "key": "region", "message": "Which region?" });
    round_trip::<IntegrationPrompt>(text);

    let select = json!({
        "type": "select",
        "key": "account",
        "message": "Which account?",
        "options": [ { "label": "Personal", "value": "personal", "hint": "Work" } ],
        "when": { "key": "kind", "op": "eq", "value": "cloud" },
    });
    round_trip::<IntegrationPrompt>(select);
}

#[test]
fn union_integration_method() {
    let oauth = json!({
        "id": "github",
        "type": "oauth",
        "label": "GitHub",
        "prompts": [
            {
                "type": "text",
                "key": "pat",
                "message": "Paste token",
                "placeholder": "token",
            },
        ],
    });
    round_trip::<IntegrationMethod>(oauth);

    let key = json!({ "type": "key", "label": "API key" });
    round_trip::<IntegrationMethod>(key);

    let env = json!({ "type": "env", "names": [ "ANTHROPIC_API_KEY" ] });
    round_trip::<IntegrationMethod>(env);
}

#[test]
fn union_integration_attempt_status() {
    let pending = json!({
        "status": "pending",
        "time": { "created": 1, "expires": 2 },
    });
    round_trip::<IntegrationAttemptStatus>(pending);

    let complete = json!({
        "status": "complete",
        "time": { "created": 1, "expires": 2 },
    });
    round_trip::<IntegrationAttemptStatus>(complete);

    let failed = json!({
        "status": "failed",
        "message": "code expired",
        "time": { "created": 1, "expires": 2 },
    });
    round_trip::<IntegrationAttemptStatus>(failed);

    let expired = json!({
        "status": "expired",
        "time": { "created": 1, "expires": 2 },
    });
    round_trip::<IntegrationAttemptStatus>(expired);
}

// ---------------------------------------------------------------------
// §2.5 per-variant samples — Reference.Source / Skill.Source
// ---------------------------------------------------------------------

#[test]
fn union_reference_source() {
    let local = json!({
        "type": "local",
        "path": "/repo/src",
        "description": "The repo",
        "hidden": false,
    });
    round_trip::<ReferenceSource>(local);

    let git = json!({
        "type": "git",
        "repository": "https://github.com/anomalyco/opencode",
        "branch": "main",
        "description": "Upstream",
        "hidden": false,
    });
    round_trip::<ReferenceSource>(git);
}

#[test]
fn union_skill_source() {
    let directory = json!({ "type": "directory", "path": "/repo/.opencode/skills" });
    round_trip::<SkillSource>(directory);

    let url = json!({ "type": "url", "url": "https://example.com/skill.md" });
    round_trip::<SkillSource>(url);

    // Embedded carries a nested SkillInfo — recursion check.
    let embedded = json!({
        "type": "embedded",
        "skill": {
            "name": "nested",
            "description": "A nested skill",
            "slash": true,
            "location": "/repo/.opencode/skills/nested",
            "content": "# Nested",
        },
    });
    round_trip::<SkillSource>(embedded);
}

// ---------------------------------------------------------------------
// §2.5 per-variant samples — SessionStatus.Info
// ---------------------------------------------------------------------

#[test]
fn union_session_status_info() {
    let idle = json!({ "type": "idle" });
    round_trip::<SessionStatusInfo>(idle);

    let busy = json!({ "type": "busy" });
    round_trip::<SessionStatusInfo>(busy);

    // V4's retry status, minus the envelope.
    let retry = json!({
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
    });
    round_trip::<SessionStatusInfo>(retry);
}

// ---------------------------------------------------------------------
// §2.5 per-variant samples — Session.Message (v2, 8 variants)
// ---------------------------------------------------------------------

#[test]
fn union_session_message_all_variants() {
    let agent_switched = json!({
        "id": "msg_01JDY",
        "time": { "created": 1778031210000i64 },
        "agent": "build",
        "type": "agent-switched",
    });
    round_trip::<SessionMessage>(agent_switched);

    let model_switched = json!({
        "id": "msg_01JDY",
        "time": { "created": 1778031210000i64 },
        "model": { "id": "claude-sonnet-4-5", "providerID": "anthropic" },
        "type": "model-switched",
    });
    round_trip::<SessionMessage>(model_switched);

    let synthetic = json!({
        "id": "msg_01JDY",
        "time": { "created": 1778031210000i64 },
        "sessionID": "ses_01JDY",
        "text": "synthetic",
        "type": "synthetic",
    });
    round_trip::<SessionMessage>(synthetic);

    let system = json!({
        "id": "msg_01JDY",
        "time": { "created": 1778031210000i64 },
        "text": "system prompt",
        "type": "system",
    });
    round_trip::<SessionMessage>(system);

    let shell = json!({
        "id": "msg_01JDY",
        "time": { "created": 1778031210000i64, "completed": 1778031211000i64 },
        "callID": "call_1",
        "command": "ls",
        "output": "file.txt",
        "type": "shell",
    });
    round_trip::<SessionMessage>(shell);

    let compaction = json!({
        "id": "msg_01JDY",
        "time": { "created": 1778031210000i64 },
        "reason": "auto",
        "summary": "summary text",
        "recent": "recent text",
        "type": "compaction",
    });
    round_trip::<SessionMessage>(compaction);
}

// ---------------------------------------------------------------------
// §2.5 per-variant samples — AssistantContent (3 variants)
// ---------------------------------------------------------------------

#[test]
fn union_assistant_content() {
    let text = json!({ "type": "text", "id": "text_1", "text": "hi" });
    round_trip::<AssistantContent>(text);

    let reasoning = json!({
        "type": "reasoning",
        "id": "reason_1",
        "text": "thinking",
        "providerMetadata": { "anthropic": { "c": "d" } },
        "time": { "created": 1778031210000i64 },
    });
    round_trip::<AssistantContent>(reasoning);
}

// ---------------------------------------------------------------------
// §2.5 per-variant samples — ToolState (v2, 4 variants)
// ---------------------------------------------------------------------

#[test]
fn union_tool_state_v2() {
    let pending = json!({
        "status": "pending",
        "input": "raw string",
    });
    round_trip::<ToolState>(pending);

    let running = json!({
        "status": "running",
        "input": { "command": "ls" },
        "structured": {},
        "content": [ { "type": "text", "text": "…" } ],
    });
    round_trip::<ToolState>(running);

    let error = json!({
        "status": "error",
        "input": { "command": "ls" },
        "content": [ { "type": "text", "text": "partial" } ],
        "structured": {},
        "error": { "type": "unknown", "message": "boom" },
    });
    let parsed: ToolState = serde_json::from_value(error.clone()).unwrap();
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(back, error);
    assert_absent(&back, "result");
}

// ---------------------------------------------------------------------
// §2.5 per-variant samples — v1 Part (12 variants)
// ---------------------------------------------------------------------

#[test]
fn union_v1_part_all_variants() {
    let text = json!({
        "id": "prt_01J",
        "sessionID": "ses_01JDY",
        "messageID": "msg_01JDY",
        "type": "text",
        "text": "hi",
    });
    round_trip::<V1Part>(text);

    let subtask = json!({
        "id": "prt_01J",
        "sessionID": "ses_01JDY",
        "messageID": "msg_01JDY",
        "type": "subtask",
        "prompt": "do it",
        "description": "subtask",
        "agent": "build",
        "model": { "providerID": "anthropic", "modelID": "claude-sonnet-4-5" },
        "command": "opencode build",
    });
    round_trip::<V1Part>(subtask);

    let reasoning = json!({
        "id": "prt_01J",
        "sessionID": "ses_01JDY",
        "messageID": "msg_01JDY",
        "type": "reasoning",
        "text": "thinking",
        "time": { "start": 1778031210000i64, "end": 1778031211000i64 },
    });
    round_trip::<V1Part>(reasoning);

    let file = json!({
        "id": "prt_01J",
        "sessionID": "ses_01JDY",
        "messageID": "msg_01JDY",
        "type": "file",
        "mime": "text/plain",
        "filename": "a.txt",
        "url": "file:///tmp/a.txt",
        "source": {
            "type": "file",
            "text": { "value": "hi", "start": 0, "end": 2 },
            "path": "/tmp/a.txt",
        },
    });
    round_trip::<V1Part>(file);

    let step_start = json!({
        "id": "prt_01J",
        "sessionID": "ses_01JDY",
        "messageID": "msg_01JDY",
        "type": "step-start",
        "snapshot": "snap",
    });
    round_trip::<V1Part>(step_start);

    let step_finish = json!({
        "id": "prt_01J",
        "sessionID": "ses_01JDY",
        "messageID": "msg_01JDY",
        "type": "step-finish",
        "reason": "stop",
        "snapshot": "snap",
        "cost": 0.001,
        "tokens": {
            "input": 10,
            "output": 5,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 },
        },
    });
    round_trip::<V1Part>(step_finish);

    let snapshot = json!({
        "id": "prt_01J",
        "sessionID": "ses_01JDY",
        "messageID": "msg_01JDY",
        "type": "snapshot",
        "snapshot": "snap",
    });
    round_trip::<V1Part>(snapshot);

    let patch = json!({
        "id": "prt_01J",
        "sessionID": "ses_01JDY",
        "messageID": "msg_01JDY",
        "type": "patch",
        "hash": "abc123",
        "files": [ "src/lib.rs" ],
    });
    round_trip::<V1Part>(patch);

    let agent = json!({
        "id": "prt_01J",
        "sessionID": "ses_01JDY",
        "messageID": "msg_01JDY",
        "type": "agent",
        "name": "build",
        "source": { "value": "build it", "start": 1, "end": 2 },
    });
    round_trip::<V1Part>(agent);

    let retry = json!({
        "id": "prt_01J",
        "sessionID": "ses_01JDY",
        "messageID": "msg_01JDY",
        "type": "retry",
        "attempt": 1,
        "error": {
            "name": "APIError",
            "data": { "message": "upstream 500", "statusCode": 500, "isRetryable": true },
        },
        "time": { "created": 1778031210000i64 },
    });
    round_trip::<V1Part>(retry);
}

// ---------------------------------------------------------------------
// §2.5 per-variant samples — v1 ToolState (4 variants)
// ---------------------------------------------------------------------

#[test]
fn union_v1_tool_state() {
    let pending = json!({
        "status": "pending",
        "input": { "command": "ls" },
        "raw": "{\"command\":\"ls\"}",
    });
    round_trip::<V1ToolState>(pending);

    let running = json!({
        "status": "running",
        "input": { "command": "ls" },
        "title": "ls",
        "metadata": {},
        "time": { "start": 1778031210000i64 },
    });
    round_trip::<V1ToolState>(running);

    let error = json!({
        "status": "error",
        "input": { "command": "ls" },
        "error": "boom",
        "time": { "start": 1778031210000i64, "end": 1778031211000i64 },
    });
    let parsed: V1ToolState = serde_json::from_value(error.clone()).unwrap();
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(back, error);
    assert_absent(&back, "metadata");
}

// ---------------------------------------------------------------------
// §2.5 per-variant samples — v1 FilePartSource (3 variants)
// ---------------------------------------------------------------------

#[test]
fn union_v1_file_part_source() {
    let file = json!({
        "type": "file",
        "text": { "value": "hi", "start": 0, "end": 2 },
        "path": "/tmp/a.txt",
    });
    round_trip::<V1FilePartSource>(file);

    let symbol = json!({
        "type": "symbol",
        "text": { "value": "hi", "start": 0, "end": 2 },
        "path": "/tmp/a.txt",
        "range": {
            "start": { "line": 1, "character": 2 },
            "end": { "line": 3, "character": 4 },
        },
        "name": "someFunction",
        "kind": 12,
    });
    round_trip::<V1FilePartSource>(symbol);

    let resource = json!({
        "type": "resource",
        "text": { "value": "hi", "start": 0, "end": 2 },
        "clientName": "client",
        "uri": "file:///tmp/a.txt",
    });
    round_trip::<V1FilePartSource>(resource);
}

// ---------------------------------------------------------------------
// §2.5 per-variant samples — v1 Message.Info / OutputFormat / AssistantError
// ---------------------------------------------------------------------

#[test]
fn union_v1_message_user() {
    let user = json!({
        "id": "msg_01JDY",
        "sessionID": "ses_01JDY",
        "role": "user",
        "time": { "created": 1778031210000i64 },
        "agent": "build",
        "model": { "providerID": "anthropic", "modelID": "claude-sonnet-4-5" },
    });
    round_trip::<V1Message>(user);
}

#[test]
fn union_output_format() {
    let text = json!({ "type": "text" });
    round_trip::<OutputFormat>(text);

    let json_schema = json!({
        "type": "json_schema",
        "schema": { "type": "object" },
        "retryCount": 2,
    });
    round_trip::<OutputFormat>(json_schema);
}

#[test]
fn union_assistant_error_all_variants() {
    let vectors: Vec<(Value, AssistantError)> = vec![
        (
            json!({ "name": "ProviderAuthError", "data": { "providerID": "anthropic", "message": "no key" } }),
            AssistantError::Auth {
                provider_id: "anthropic".to_string(),
                message: "no key".to_string(),
            },
        ),
        (
            json!({ "name": "UnknownError", "data": { "message": "boom", "ref": "r1" } }),
            AssistantError::Unknown {
                message: "boom".to_string(),
                r#ref: Some("r1".to_string()),
            },
        ),
        // Regression: `ref` is optional and must be omitted when absent
        // (and must deserialize when the key is missing entirely).
        (
            json!({ "name": "UnknownError", "data": { "message": "boom" } }),
            AssistantError::Unknown {
                message: "boom".to_string(),
                r#ref: None,
            },
        ),
        (
            json!({ "name": "MessageOutputLengthError", "data": {} }),
            AssistantError::OutputLength {},
        ),
        (
            json!({ "name": "MessageAbortedError", "data": { "message": "aborted" } }),
            AssistantError::Aborted {
                message: "aborted".to_string(),
            },
        ),
        (
            json!({ "name": "StructuredOutputError", "data": { "message": "bad", "retries": 1 } }),
            AssistantError::StructuredOutput {
                message: "bad".to_string(),
                retries: 1,
            },
        ),
        (
            json!({ "name": "ContextOverflowError", "data": { "message": "too big", "responseBody": "…" } }),
            AssistantError::ContextOverflow {
                message: "too big".to_string(),
                response_body: Some("…".to_string()),
            },
        ),
        (
            json!({ "name": "ContentFilterError", "data": { "message": "blocked" } }),
            AssistantError::ContentFilter {
                message: "blocked".to_string(),
            },
        ),
        (
            json!({ "name": "APIError", "data": { "message": "upstream 500", "statusCode": 500, "isRetryable": true } }),
            AssistantError::Api {
                message: "upstream 500".to_string(),
                status_code: Some(500),
                is_retryable: true,
                response_headers: None,
                response_body: None,
                metadata: None,
            },
        ),
    ];
    for (value, expected) in vectors {
        let parsed: AssistantError = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(parsed, expected, "mismatch for {value}");
        let back = serde_json::to_value(&parsed).unwrap();
        assert_eq!(back, value);
    }
}

// ---------------------------------------------------------------------
// Serde unit-test rules (spec M1.12)
// ---------------------------------------------------------------------

/// Negative test: a JSON with `"type": "tool"` payload for
/// `SessionMessage::User` must error.
#[test]
fn negative_tool_type_is_not_a_session_message_variant() {
    let bogus = json!({
        "id": "msg_01JDY",
        "type": "tool",
        "time": { "created": 1778031210000i64 },
        "text": "hello",
    });
    assert!(
        serde_json::from_value::<SessionMessage>(bogus).is_err(),
        "`tool` is not a SessionMessage variant and must be rejected"
    );

    let missing_text = json!({
        "id": "msg_01JDY",
        "type": "user",
        "time": { "created": 1778031210000i64 },
    });
    assert!(
        serde_json::from_value::<SessionMessage>(missing_text).is_err(),
        "user message without required `text` must be rejected"
    );
}

/// Optional-key test: deserialize a minimal object, re-serialize, assert the
/// optional key is absent (not null).
#[test]
fn optional_key_save_is_absent_not_null() {
    let minimal = json!({
        "id": "per_01J",
        "sessionID": "ses_01JDY",
        "action": "bash",
        "resources": [ "rm -rf /tmp/x" ],
    });
    let parsed: PermissionRequest = serde_json::from_value(minimal.clone()).unwrap();
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(back, minimal);
    for key in ["save", "metadata", "source"] {
        assert_absent(&back, key);
    }
}

/// Numeric test: `SessionV2Info.time.created` accepts integral epoch millis.
/// (Do NOT assert rejection of fractional millis — documented tolerance gap.)
#[test]
fn numeric_epoch_millis_accepts_integral_values() {
    let value = json!({
        "id": "ses_01JDY",
        "projectID": "global",
        "cost": 0.0,
        "tokens": {
            "input": 10,
            "output": 5,
            "reasoning": 0,
            "cache": { "read": 0, "write": 0 },
        },
        "time": { "created": 1778031210000i64, "updated": 1778031211000i64 },
        "title": "hello",
        "location": { "directory": "/repo" },
    });
    let info: SessionInfo = serde_json::from_value(value).unwrap();
    assert_eq!(info.time.created, 1778031210000);
}
