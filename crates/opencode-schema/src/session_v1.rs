//! Wire DTOs for `schema-src/v1/session.ts` — the v1 session, message, and part
//! schemas (openapi `Session`, `UserMessage`, `AssistantMessage`, `TextPart`,
//! `ToolPart`, `CompactionPart`, …).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::file_diff::SnapshotFileDiff;
use crate::ids::{MessageId, ModelId, PartId, ProjectId, ProviderId, SessionId, WorkspaceId};
use crate::permission_v1::PermissionV1Ruleset;
use crate::schema::JsonMap;

/// v1 message ID prefix (`MessageID` in `schema-src/v1/session.ts`).
pub const V1_MESSAGE_PREFIX: &str = "msg";

/// v1 part ID prefix (`PartID` in `schema-src/v1/session.ts`).
pub const V1_PART_PREFIX: &str = "prt";

/// v1 assistant error union — `{"name": "<PascalCase>", "data": {…}}`.
///
/// openapi components: `ProviderAuthError`, `UnknownError`, `MessageOutputLengthError`,
/// `MessageAbortedError`, `StructuredOutputError`, `ContextOverflowError`,
/// `ContentFilterError`, `APIError`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "data")]
#[serde(rename_all_fields = "camelCase")]
pub enum AssistantError {
    /// `ProviderAuthError`
    #[serde(rename = "ProviderAuthError")]
    Auth {
        #[serde(rename = "providerID")]
        provider_id: String,
        message: String,
    },
    /// `UnknownError`
    #[serde(rename = "UnknownError")]
    Unknown {
        message: String,
        #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
        r#ref: Option<String>,
    },
    /// `MessageOutputLengthError`
    #[serde(rename = "MessageOutputLengthError")]
    OutputLength {},
    /// `MessageAbortedError`
    #[serde(rename = "MessageAbortedError")]
    Aborted { message: String },
    /// `StructuredOutputError`
    #[serde(rename = "StructuredOutputError")]
    StructuredOutput { message: String, retries: u64 },
    /// `ContextOverflowError`
    #[serde(rename = "ContextOverflowError")]
    ContextOverflow {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_body: Option<String>,
    },
    /// `ContentFilterError`
    #[serde(rename = "ContentFilterError")]
    ContentFilter { message: String },
    /// `APIError`
    #[serde(rename = "APIError")]
    Api {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_code: Option<u64>,
        is_retryable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_headers: Option<BTreeMap<String, String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_body: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<BTreeMap<String, String>>,
    },
}

/// v1 `OutputFormat` union — openapi `OutputFormat` (`OutputFormatText`,
/// `OutputFormatJsonSchema`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum OutputFormat {
    Text {},
    #[serde(rename = "json_schema")]
    JsonSchema {
        schema: JsonMap,
        /// TS decode default is `2` (`withDecodingDefault`); the key is simply
        /// absent here — consumers must apply the default (spec STOP S2).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_count: Option<u64>,
    },
}

/// openapi `FilePartSourceText`: `{ value, start, end }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1FilePartSourceText {
    pub value: String,
    pub start: f64,
    pub end: f64,
}

/// openapi `Range` position sub-object: `{ line, character }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1Position {
    pub line: u64,
    pub character: u64,
}

/// openapi `Range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1Range {
    pub start: V1Position,
    pub end: V1Position,
}

/// v1 `FilePartSource` union — openapi `FilePartSource` (`FileSource`,
/// `SymbolSource`, `ResourceSource`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum V1FilePartSource {
    File {
        text: V1FilePartSourceText,
        path: String,
    },
    Symbol {
        text: V1FilePartSourceText,
        path: String,
        range: V1Range,
        name: String,
        kind: u64,
    },
    Resource {
        text: V1FilePartSourceText,
        client_name: String,
        uri: String,
    },
}

/// v1 file part — openapi `FilePart`.
///
/// A standalone (non-union) file part, used e.g. in `ToolStateCompleted.attachments`.
/// Modeled as a single-variant enum so the required literal tag `"type": "file"`
/// is produced and validated by serde's tagged-enum machinery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum V1FilePart {
    File {
        id: PartId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        #[serde(rename = "messageID")]
        message_id: MessageId,
        mime: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<V1FilePartSource>,
    },
}

/// openapi `TextPart.time` sub-object: `{ start, end? }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextPartTime {
    pub start: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u64>,
}

/// openapi `ReasoningPart.time` sub-object: `{ start, end? }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningTime {
    pub start: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u64>,
}

/// openapi `AgentPart.source` sub-object: `{ value, start, end }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPartSource {
    pub value: String,
    pub start: u64,
    pub end: u64,
}

/// openapi `SubtaskPart.model` sub-object: `{ providerID, modelID }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1SubtaskModel {
    #[serde(rename = "providerID")]
    pub provider_id: ProviderId,
    #[serde(rename = "modelID")]
    pub model_id: ModelId,
}

/// Token cache counters for `V1StepTokens`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1TokenCache {
    pub read: f64,
    pub write: f64,
}

/// openapi `StepFinishPart.tokens` / `AssistantMessage.tokens`:
/// `{ total?, input, output, reasoning, cache }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1StepTokens {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    pub input: f64,
    pub output: f64,
    pub reasoning: f64,
    pub cache: V1TokenCache,
}

/// openapi `RetryPart.time` sub-object: `{ created }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryTime {
    pub created: u64,
}

/// openapi `ToolStateRunning.time` sub-object: `{ start }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStateRunningTime {
    pub start: u64,
}

/// openapi `ToolStateCompleted.time` sub-object: `{ start, end, compacted? }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStateCompletedTime {
    pub start: u64,
    pub end: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted: Option<u64>,
}

/// openapi `ToolStateError.time` sub-object: `{ start, end }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStateErrorTime {
    pub start: u64,
    pub end: u64,
}

/// v1 tool state union — openapi `ToolState` (`ToolStatePending`,
/// `ToolStateRunning`, `ToolStateCompleted`, `ToolStateError`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum V1ToolState {
    Pending {
        input: JsonMap,
        raw: String,
    },
    Running {
        input: JsonMap,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        time: ToolStateRunningTime,
    },
    Completed {
        input: JsonMap,
        output: String,
        title: String,
        metadata: JsonMap,
        time: ToolStateCompletedTime,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachments: Option<Vec<V1FilePart>>,
    },
    Error {
        input: JsonMap,
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        time: ToolStateErrorTime,
    },
}

/// v1 part union — openapi `Part` (payload of `message.part.updated`).
///
/// Every variant repeats the part base fields: `id`, `sessionID`, `messageID`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum V1Part {
    /// `TextPart`
    Text {
        id: PartId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        #[serde(rename = "messageID")]
        message_id: MessageId,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        synthetic: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ignored: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        time: Option<TextPartTime>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
    },
    /// `SubtaskPart`
    Subtask {
        id: PartId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        #[serde(rename = "messageID")]
        message_id: MessageId,
        prompt: String,
        description: String,
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<V1SubtaskModel>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },
    /// `ReasoningPart`
    Reasoning {
        id: PartId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        #[serde(rename = "messageID")]
        message_id: MessageId,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        time: ReasoningTime,
    },
    /// `FilePart`
    File {
        id: PartId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        #[serde(rename = "messageID")]
        message_id: MessageId,
        mime: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<V1FilePartSource>,
    },
    /// `ToolPart`
    Tool {
        id: PartId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        #[serde(rename = "messageID")]
        message_id: MessageId,
        #[serde(rename = "callID")]
        call_id: String,
        tool: String,
        state: V1ToolState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
    },
    /// `StepStartPart`
    #[serde(rename = "step-start")]
    StepStart {
        id: PartId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        #[serde(rename = "messageID")]
        message_id: MessageId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot: Option<String>,
    },
    /// `StepFinishPart`
    #[serde(rename = "step-finish")]
    StepFinish {
        id: PartId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        #[serde(rename = "messageID")]
        message_id: MessageId,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        snapshot: Option<String>,
        cost: f64,
        tokens: V1StepTokens,
    },
    /// `SnapshotPart`
    Snapshot {
        id: PartId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        #[serde(rename = "messageID")]
        message_id: MessageId,
        snapshot: String,
    },
    /// `PatchPart`
    Patch {
        id: PartId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        #[serde(rename = "messageID")]
        message_id: MessageId,
        hash: String,
        files: Vec<String>,
    },
    /// `AgentPart`
    Agent {
        id: PartId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        #[serde(rename = "messageID")]
        message_id: MessageId,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<AgentPartSource>,
    },
    /// `RetryPart` — `error` is always the `APIError`-shaped member.
    Retry {
        id: PartId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        #[serde(rename = "messageID")]
        message_id: MessageId,
        attempt: u64,
        error: AssistantError,
        time: RetryTime,
    },
    /// `CompactionPart` — `tail_start_id` is snake_case on the wire.
    Compaction {
        id: PartId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        #[serde(rename = "messageID")]
        message_id: MessageId,
        auto: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overflow: Option<bool>,
        #[serde(
            rename = "tail_start_id",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        tail_start_id: Option<MessageId>,
    },
}

/// openapi `UserMessage.time` sub-object: `{ created }` (`Schema.Finite`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserTime {
    pub created: f64,
}

/// openapi `AssistantMessage.time` sub-object: `{ created, completed? }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantTime {
    pub created: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
}

/// openapi `UserMessage.summary` sub-object: `{ title?, body?, diffs }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1Summary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub diffs: Vec<SnapshotFileDiff>,
}

/// openapi `UserMessage.model` sub-object: `{ providerID, modelID, variant? }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1UserModel {
    #[serde(rename = "providerID")]
    pub provider_id: ProviderId,
    #[serde(rename = "modelID")]
    pub model_id: ModelId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// openapi `AssistantMessage.path` sub-object: `{ cwd, root }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1Path {
    pub cwd: String,
    pub root: String,
}

/// v1 message union — openapi `Message` (`UserMessage`, `AssistantMessage`).
///
/// The variants are large but flat on the wire; boxing would deviate from the
/// spec sketch, so the clippy size lint is allowed instead.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum V1Message {
    User {
        id: MessageId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        time: UserTime,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<OutputFormat>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<V1Summary>,
        agent: String,
        model: V1UserModel,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tools: Option<BTreeMap<String, bool>>,
    },
    Assistant {
        id: MessageId,
        #[serde(rename = "sessionID")]
        session_id: SessionId,
        time: AssistantTime,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<AssistantError>,
        #[serde(rename = "parentID")]
        parent_id: MessageId,
        #[serde(rename = "modelID")]
        model_id: ModelId,
        #[serde(rename = "providerID")]
        provider_id: ProviderId,
        mode: String,
        agent: String,
        path: V1Path,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<bool>,
        cost: f64,
        tokens: V1StepTokens,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        variant: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish: Option<String>,
    },
}

/// openapi `Session.summary` sub-object: `{ additions, deletions, files, diffs? }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1SessionSummary {
    pub additions: f64,
    pub deletions: f64,
    pub files: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diffs: Option<Vec<SnapshotFileDiff>>,
}

/// openapi `Session.share` sub-object: `{ url }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1SessionShare {
    pub url: String,
}

/// openapi `Session.model` sub-object: `{ id, providerID, variant? }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1SessionModel {
    pub id: ModelId,
    #[serde(rename = "providerID")]
    pub provider_id: ProviderId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// openapi `Session.time` sub-object:
/// `{ created, updated, compacting?, archived? }`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1SessionTime {
    pub created: u64,
    pub updated: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacting: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<f64>,
}

/// openapi `Session.revert` sub-object:
/// `{ messageID, partID?, snapshot?, diff? }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1SessionRevert {
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    #[serde(rename = "partID", default, skip_serializing_if = "Option::is_none")]
    pub part_id: Option<PartId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// v1 session info — openapi `Session`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1SessionInfo {
    pub id: SessionId,
    pub slug: String,
    #[serde(rename = "projectID")]
    pub project_id: ProjectId,
    #[serde(
        rename = "workspaceID",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_id: Option<WorkspaceId>,
    pub directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(rename = "parentID", default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<V1SessionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<crate::session::SessionTokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<V1SessionShare>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<V1SessionModel>,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    pub time: V1SessionTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionV1Ruleset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revert: Option<V1SessionRevert>,
}

/// `session.created` payload — openapi `EventSessionCreated.properties`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreatedData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub info: V1SessionInfo,
}

/// `session.updated` payload — openapi `EventSessionUpdated.properties`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdatedData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub info: V1SessionInfo,
}

/// `session.deleted` payload — openapi `EventSessionDeleted.properties`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDeletedData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub info: V1SessionInfo,
}

/// `message.updated` payload — openapi `EventMessageUpdated.properties`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageUpdatedData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub info: V1Message,
}

/// `message.removed` payload — openapi `EventMessageRemoved.properties`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRemovedData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
}

/// `message.part.updated` payload — openapi `EventMessagePartUpdated.properties`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePartUpdatedData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub part: V1Part,
    pub time: f64,
}

/// `message.part.removed` payload — openapi `EventMessagePartRemoved.properties`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePartRemovedData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    #[serde(rename = "partID")]
    pub part_id: PartId,
}

/// `message.part.delta` payload — openapi `EventMessagePartDelta.properties`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePartDeltaData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    #[serde(rename = "partID")]
    pub part_id: PartId,
    pub field: String,
    pub delta: String,
}

/// `session.diff` payload — openapi `EventSessionDiff.properties`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDiffData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub diff: Vec<SnapshotFileDiff>,
}

/// `session.error` payload — openapi `EventSessionError.properties`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionErrorData {
    #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub error: AssistantError,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip<T>(value: &T)
    where
        T: Serialize + std::fmt::Debug + PartialEq,
        for<'de> T: Deserialize<'de>,
    {
        let serialized = serde_json::to_value(value).unwrap();
        let deserialized: T = serde_json::from_value(serialized.clone()).unwrap();
        assert_eq!(&deserialized, value);
        assert_eq!(serde_json::to_value(&deserialized).unwrap(), serialized);
    }

    #[test]
    fn compaction_part_serializes_tail_start_id() {
        // §6 vector V10 — openapi `CompactionPart`.
        let part = V1Part::Compaction {
            id: "prt_01J".to_string(),
            session_id: "ses_01JDY".to_string(),
            message_id: "msg_01JDY".to_string(),
            auto: true,
            overflow: Some(false),
            tail_start_id: Some("msg_01JDX".to_string()),
        };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(
            json,
            json!({
                "type": "compaction",
                "id": "prt_01J",
                "sessionID": "ses_01JDY",
                "messageID": "msg_01JDY",
                "auto": true,
                "overflow": false,
                "tail_start_id": "msg_01JDX"
            })
        );
        round_trip(&part);
    }

    #[test]
    fn compaction_part_omits_optional_keys() {
        let part = V1Part::Compaction {
            id: "prt_01J".to_string(),
            session_id: "ses_01JDY".to_string(),
            message_id: "msg_01JDY".to_string(),
            auto: false,
            overflow: None,
            tail_start_id: None,
        };
        let json = serde_json::to_value(&part).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.get("overflow").is_none(), "overflow must be absent");
        assert!(
            obj.get("tail_start_id").is_none(),
            "tail_start_id must be absent"
        );
    }

    #[test]
    fn assistant_error_api_produces_name_data() {
        // §6 vector V8 error shape — openapi `APIError`.
        let error = AssistantError::Api {
            message: "upstream 500".to_string(),
            status_code: Some(500),
            is_retryable: true,
            response_headers: None,
            response_body: None,
            metadata: None,
        };
        let json = serde_json::to_value(&error).unwrap();
        assert_eq!(
            json,
            json!({
                "name": "APIError",
                "data": {
                    "message": "upstream 500",
                    "statusCode": 500,
                    "isRetryable": true
                }
            })
        );
        round_trip(&error);
    }

    #[test]
    fn assistant_error_variants_wire_names() {
        let auth = AssistantError::Auth {
            provider_id: "anthropic".to_string(),
            message: "no key".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&auth).unwrap(),
            json!({"name": "ProviderAuthError", "data": {"providerID": "anthropic", "message": "no key"}})
        );

        let unknown = AssistantError::Unknown {
            message: "boom".to_string(),
            r#ref: Some("ref_1".to_string()),
        };
        assert_eq!(
            serde_json::to_value(&unknown).unwrap(),
            json!({"name": "UnknownError", "data": {"message": "boom", "ref": "ref_1"}})
        );

        let output_length = AssistantError::OutputLength {};
        assert_eq!(
            serde_json::to_value(&output_length).unwrap(),
            json!({"name": "MessageOutputLengthError", "data": {}})
        );

        let aborted = AssistantError::Aborted {
            message: "aborted".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&aborted).unwrap(),
            json!({"name": "MessageAbortedError", "data": {"message": "aborted"}})
        );

        let structured = AssistantError::StructuredOutput {
            message: "bad json".to_string(),
            retries: 1,
        };
        assert_eq!(
            serde_json::to_value(&structured).unwrap(),
            json!({"name": "StructuredOutputError", "data": {"message": "bad json", "retries": 1}})
        );

        let overflow = AssistantError::ContextOverflow {
            message: "too big".to_string(),
            response_body: Some("body".to_string()),
        };
        assert_eq!(
            serde_json::to_value(&overflow).unwrap(),
            json!({"name": "ContextOverflowError", "data": {"message": "too big", "responseBody": "body"}})
        );

        let content_filter = AssistantError::ContentFilter {
            message: "blocked".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&content_filter).unwrap(),
            json!({"name": "ContentFilterError", "data": {"message": "blocked"}})
        );
    }

    #[test]
    fn user_message_has_role_tag() {
        // openapi `UserMessage` — required: id, sessionID, role, time, agent, model.
        let user = V1Message::User {
            id: "msg_1".to_string(),
            session_id: "ses_1".to_string(),
            time: UserTime {
                created: 1778031210000.0,
            },
            format: None,
            summary: None,
            agent: "build".to_string(),
            model: V1UserModel {
                provider_id: "anthropic".to_string(),
                model_id: "claude-sonnet-4-5".to_string(),
                variant: None,
            },
            system: None,
            tools: None,
        };
        let json = serde_json::to_value(&user).unwrap();
        assert_eq!(
            json,
            json!({
                "role": "user",
                "id": "msg_1",
                "sessionID": "ses_1",
                "time": {"created": 1778031210000.0},
                "agent": "build",
                "model": {
                    "providerID": "anthropic",
                    "modelID": "claude-sonnet-4-5"
                }
            })
        );
        round_trip(&user);
    }

    #[test]
    fn assistant_message_with_api_error_round_trip() {
        // §6 vector V8 — openapi `AssistantMessage` + `APIError`.
        let json = json!({
            "id": "msg_01JDY",
            "sessionID": "ses_01JDY",
            "role": "assistant",
            "time": {"created": 1778031210000i64},
            "error": {
                "name": "APIError",
                "data": {"message": "upstream 500", "statusCode": 500, "isRetryable": true}
            },
            "parentID": "msg_01JDX",
            "modelID": "claude-sonnet-4-5",
            "providerID": "anthropic",
            "mode": "primary",
            "agent": "build",
            "path": {"cwd": "/repo", "root": "/repo"},
            "cost": 0.001,
            "tokens": {
                "input": 10,
                "output": 5,
                "reasoning": 0,
                "cache": {"read": 0, "write": 0}
            }
        });
        let message: V1Message = serde_json::from_value(json).unwrap();
        match &message {
            V1Message::Assistant { error, .. } => {
                assert_eq!(
                    error.as_ref().map(|e| serde_json::to_value(e).unwrap()),
                    Some(json!({
                        "name": "APIError",
                        "data": {"message": "upstream 500", "statusCode": 500, "isRetryable": true}
                    }))
                );
            }
            other => panic!("expected assistant message, got {other:?}"),
        }
        round_trip(&message);
    }

    #[test]
    fn tool_part_completed_state_round_trip() {
        // §6 vector V9 — openapi `ToolPart` + `ToolStateCompleted`.
        let json = json!({
            "id": "prt_01J",
            "sessionID": "ses_01JDY",
            "messageID": "msg_01JDY",
            "type": "tool",
            "callID": "call_1",
            "tool": "bash",
            "state": {
                "status": "completed",
                "input": {"command": "ls"},
                "output": "file.txt",
                "title": "ls",
                "metadata": {},
                "time": {"start": 1778031210000i64, "end": 1778031211000i64}
            }
        });
        let part: V1Part = serde_json::from_value(json).unwrap();
        assert!(matches!(
            part,
            V1Part::Tool {
                state: V1ToolState::Completed { .. },
                ..
            }
        ));
        round_trip(&part);
    }

    #[test]
    fn tool_part_error_state_omits_optional_metadata() {
        let part = V1Part::Tool {
            id: "prt_01J".to_string(),
            session_id: "ses_01JDY".to_string(),
            message_id: "msg_01JDY".to_string(),
            call_id: "call_1".to_string(),
            tool: "bash".to_string(),
            state: V1ToolState::Error {
                input: serde_json::Map::new(),
                error: "boom".to_string(),
                metadata: None,
                time: ToolStateErrorTime {
                    start: 1778031210000,
                    end: 1778031211000,
                },
            },
            metadata: None,
        };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(
            json,
            json!({
                "type": "tool",
                "id": "prt_01J",
                "sessionID": "ses_01JDY",
                "messageID": "msg_01JDY",
                "callID": "call_1",
                "tool": "bash",
                "state": {
                    "status": "error",
                    "input": {},
                    "error": "boom",
                    "time": {"start": 1778031210000i64, "end": 1778031211000i64}
                }
            })
        );
        round_trip(&part);
    }

    #[test]
    fn step_start_and_step_finish_wire_tags() {
        let step_start = V1Part::StepStart {
            id: "prt_01J".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_1".to_string(),
            snapshot: None,
        };
        assert_eq!(
            serde_json::to_value(&step_start).unwrap(),
            json!({
                "type": "step-start",
                "id": "prt_01J",
                "sessionID": "ses_1",
                "messageID": "msg_1"
            })
        );

        let step_finish = V1Part::StepFinish {
            id: "prt_01J".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_1".to_string(),
            reason: "stop".to_string(),
            snapshot: None,
            cost: 0.001,
            tokens: V1StepTokens {
                total: Some(15.0),
                input: 10.0,
                output: 5.0,
                reasoning: 0.0,
                cache: V1TokenCache {
                    read: 0.0,
                    write: 0.0,
                },
            },
        };
        let json = serde_json::to_value(&step_finish).unwrap();
        assert_eq!(json["type"], "step-finish");
        assert_eq!(json["tokens"]["total"], 15.0);
        round_trip(&step_finish);
    }

    #[test]
    fn output_format_wire_shapes() {
        let text = OutputFormat::Text {};
        assert_eq!(
            serde_json::to_value(&text).unwrap(),
            json!({"type": "text"})
        );

        let mut schema = serde_json::Map::new();
        schema.insert("type".to_string(), json!("object"));
        let json_schema = OutputFormat::JsonSchema {
            schema,
            retry_count: Some(2),
        };
        assert_eq!(
            serde_json::to_value(&json_schema).unwrap(),
            json!({"type": "json_schema", "schema": {"type": "object"}, "retryCount": 2})
        );
        round_trip(&json_schema);
    }

    #[test]
    fn file_part_source_variants() {
        let text = V1FilePartSourceText {
            value: "v".to_string(),
            start: 0.0,
            end: 1.0,
        };
        let file = V1FilePartSource::File {
            text,
            path: "/tmp/a".to_string(),
        };
        let symbol = V1FilePartSource::Symbol {
            text: V1FilePartSourceText {
                value: "v".to_string(),
                start: 0.0,
                end: 1.0,
            },
            path: "/tmp/a".to_string(),
            range: V1Range {
                start: V1Position {
                    line: 1,
                    character: 2,
                },
                end: V1Position {
                    line: 3,
                    character: 4,
                },
            },
            name: "sym".to_string(),
            kind: 5,
        };
        let resource = V1FilePartSource::Resource {
            text: V1FilePartSourceText {
                value: "v".to_string(),
                start: 0.0,
                end: 1.0,
            },
            client_name: "client".to_string(),
            uri: "file:///tmp/a".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&file).unwrap(),
            json!({"type": "file", "text": {"value": "v", "start": 0.0, "end": 1.0}, "path": "/tmp/a"})
        );
        assert_eq!(
            serde_json::to_value(&symbol).unwrap()["type"],
            json!("symbol")
        );
        assert_eq!(
            serde_json::to_value(&resource).unwrap(),
            json!({
                "type": "resource",
                "text": {"value": "v", "start": 0.0, "end": 1.0},
                "clientName": "client",
                "uri": "file:///tmp/a"
            })
        );
    }

    #[test]
    fn file_part_has_literal_type_tag() {
        let part = V1FilePart::File {
            id: "prt_01J".to_string(),
            session_id: "ses_01JDY".to_string(),
            message_id: "msg_01JDY".to_string(),
            mime: "text/plain".to_string(),
            filename: None,
            url: "file:///tmp/a.txt".to_string(),
            source: None,
        };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(
            json,
            json!({
                "type": "file",
                "id": "prt_01J",
                "sessionID": "ses_01JDY",
                "messageID": "msg_01JDY",
                "mime": "text/plain",
                "url": "file:///tmp/a.txt"
            })
        );
        round_trip(&part);
    }

    #[test]
    fn session_info_minimal_round_trip() {
        // openapi `Session` — required: id, slug, projectID, directory, title,
        // version, time.
        let info = V1SessionInfo {
            id: "ses_1".to_string(),
            slug: "my-session".to_string(),
            project_id: "global".to_string(),
            workspace_id: None,
            directory: "/repo".to_string(),
            path: None,
            parent_id: None,
            summary: None,
            cost: None,
            tokens: None,
            share: None,
            title: "My session".to_string(),
            agent: None,
            model: None,
            version: "1".to_string(),
            metadata: None,
            time: V1SessionTime {
                created: 1778031210000,
                updated: 1778031210000,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(
            json,
            json!({
                "id": "ses_1",
                "slug": "my-session",
                "projectID": "global",
                "directory": "/repo",
                "title": "My session",
                "version": "1",
                "time": {"created": 1778031210000i64, "updated": 1778031210000i64}
            })
        );
        round_trip(&info);
    }

    #[test]
    fn session_created_data_round_trip() {
        let info = V1SessionInfo {
            id: "ses_1".to_string(),
            slug: "s".to_string(),
            project_id: "global".to_string(),
            workspace_id: None,
            directory: "/repo".to_string(),
            path: None,
            parent_id: None,
            summary: None,
            cost: None,
            tokens: None,
            share: None,
            title: "t".to_string(),
            agent: None,
            model: None,
            version: "1".to_string(),
            metadata: None,
            time: V1SessionTime {
                created: 1778031210000,
                updated: 1778031210000,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert: None,
        };
        let data = SessionCreatedData {
            session_id: "ses_1".to_string(),
            info,
        };
        round_trip(&data);
    }

    #[test]
    fn message_part_updated_data_round_trip() {
        // §6 vector V7 payload — openapi `EventMessagePartUpdated.properties`.
        let json = json!({
            "sessionID": "ses_01JDY",
            "time": 1778031210000i64,
            "part": {
                "id": "prt_01J",
                "sessionID": "ses_01JDY",
                "messageID": "msg_01JDY",
                "type": "text",
                "text": "hi"
            }
        });
        let data: MessagePartUpdatedData = serde_json::from_value(json).unwrap();
        assert!(matches!(data.part, V1Part::Text { .. }));
        round_trip(&data);
    }

    #[test]
    fn message_part_delta_data_round_trip() {
        let data = MessagePartDeltaData {
            session_id: "ses_01JDY".to_string(),
            message_id: "msg_01JDY".to_string(),
            part_id: "prt_01J".to_string(),
            field: "text".to_string(),
            delta: " hello".to_string(),
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(
            json,
            json!({
                "sessionID": "ses_01JDY",
                "messageID": "msg_01JDY",
                "partID": "prt_01J",
                "field": "text",
                "delta": " hello"
            })
        );
        round_trip(&data);
    }

    #[test]
    fn session_error_data_omits_optional_session_id() {
        let data = SessionErrorData {
            session_id: None,
            error: AssistantError::OutputLength {},
        };
        let json = serde_json::to_value(&data).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.get("sessionID").is_none(), "sessionID must be absent");
        assert_eq!(
            json["error"],
            json!({"name": "MessageOutputLengthError", "data": {}})
        );
        round_trip(&data);
    }
}
