//! Message rendering — port of `toModelMessagesEffect`/`toModelMessages`
//! (message-v2.ts:131-423) plus the `convertToModelMessages` normalization of
//! `ai@6.0.168` (`ui/convert-to-model-messages.ts`) that it invokes.
//!
//! The TS function first builds `UIMessage[]` from the stored parts, then
//! hands them to the ai SDK's `convertToModelMessages`, which produces
//! `ModelMessage[]`. The Rust port replicates both passes and emits
//! `opencode-llm` request messages directly.

use std::collections::BTreeSet;

use opencode_llm::schema::ids::MessageRole;
use opencode_llm::schema::messages::{ContentPart, Message, ToolResultValue};
use opencode_schema::llm::{ProviderMetadata, ToolContent};
use opencode_schema::schema::JsonMap;
use opencode_schema::session_v1::{AssistantError, V1FilePart, V1Part, V1ToolState};
use serde_json::{json, Value};

use crate::session::message::{message_id, WithParts, SYNTHETIC_ATTACHMENT_PROMPT};

/// The `Provider.Model` fields `toModelMessagesEffect` reads:
/// `providerID`/`id` (model identity) and `api.npm`/`api.id`
/// (media-in-tool-result support table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderModel {
    pub provider_id: String,
    pub id: String,
    /// e.g. `"@ai-sdk/anthropic"`.
    pub api_npm: String,
    /// e.g. `"claude-sonnet-4-5"`.
    pub api_id: String,
}

/// `options` of `toModelMessagesEffect` (message-v2.ts:134).
#[derive(Debug, Clone, Default)]
pub struct RenderOptions {
    pub strip_media: bool,
    pub tool_output_max_chars: Option<usize>,
}

/// `truncateToolOutput` (message-v2.ts:49-53). `text.length` counts UTF-16
/// code units in JS; `!maxChars` treats `0` like `undefined`.
pub fn truncate_tool_output(text: &str, max_chars: Option<usize>) -> String {
    let Some(max) = max_chars.filter(|max| *max > 0) else {
        return text.to_string();
    };
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max {
        return text.to_string();
    }
    let omitted = units.len() - max;
    // `slice` can split a surrogate pair; `from_utf16_lossy` drops the tail.
    let slice = String::from_utf16_lossy(&units[..max]);
    format!("{slice}\n[Tool output truncated for compaction: omitted {omitted} chars]")
}

/// `isMedia` (util/media.ts): `image/*` or `application/pdf`.
fn is_media(mime: &str) -> bool {
    mime.starts_with("image/") || mime == "application/pdf"
}

/// `supportsMediaInToolResult` (message-v2.ts:147-159).
fn supports_media_in_tool_result(model: &RenderModel, mime: &str) -> bool {
    match model.api_npm.as_str() {
        "@ai-sdk/anthropic"
        | "@ai-sdk/openai"
        | "@ai-sdk/amazon-bedrock/mantle"
        | "@ai-sdk/google-vertex/anthropic" => true,
        "@ai-sdk/amazon-bedrock" | "@ai-sdk/xai" => mime.starts_with("image/"),
        "@ai-sdk/google" => {
            let id = model.api_id.to_lowercase();
            id.contains("gemini-3") && !id.contains("gemini-2")
        }
        _ => false,
    }
}

/// `providerMeta` (message-v2.ts:125-129): strips `providerExecuted`, then
/// `undefined` when nothing remains.
fn provider_meta(metadata: Option<&JsonMap>) -> Option<JsonMap> {
    let mut rest = metadata.cloned().unwrap_or_default();
    rest.remove("providerExecuted");
    (!rest.is_empty()).then_some(rest)
}

/// Part metadata is `Record<string, unknown>` on the stored part but
/// `Record<string, Record<string, unknown>>` (`ProviderMetadata`) on the
/// request wire. TS passes it through untyped; the typed Rust wire cannot
/// carry non-object entries, so they are dropped — providers only read their
/// own namespace key either way.
fn render_provider_metadata(metadata: &Option<JsonMap>) -> Option<ProviderMetadata> {
    let mut out = ProviderMetadata::new();
    for (key, value) in metadata.as_ref()? {
        if let Value::Object(fields) = value {
            out.insert(key.clone(), fields.clone());
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// UIMessage — the intermediate shape of message-v2.ts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum UiPart {
    Text {
        text: String,
        provider_metadata: Option<JsonMap>,
    },
    File {
        url: String,
        media_type: String,
        filename: Option<String>,
    },
    StepStart,
    Reasoning {
        text: String,
        provider_metadata: Option<JsonMap>,
    },
    Tool(UiToolPart),
}

#[derive(Debug, Clone)]
enum UiToolState {
    /// `state: "output-available"`, carrying the raw `output` value.
    Available { output: Value },
    /// `state: "output-error"` with its `errorText`.
    Error { error_text: String },
}

#[derive(Debug, Clone)]
struct UiToolPart {
    tool: String,
    call_id: String,
    input: Value,
    state: UiToolState,
    provider_executed: Option<bool>,
    call_provider_metadata: Option<JsonMap>,
}

#[derive(Debug, Clone)]
struct UiMessage {
    #[allow(dead_code)]
    id: String,
    role: MessageRole,
    parts: Vec<UiPart>,
}

/// `toModelOutput` (message-v2.ts:161-193): the output adapter TS installs
/// for every tool name it saw (`tools` map at 404).
fn to_model_output(output: &Value) -> ToolResultValue {
    if let Some(text) = output.as_str() {
        return ToolResultValue::Text {
            value: Value::String(text.to_string()),
        };
    }
    if let Some(object) = output.as_object() {
        let mut value = Vec::new();
        if let Some(text) = object.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                value.push(ToolContent::Text {
                    text: text.to_string(),
                });
            }
        }
        let empty = Vec::new();
        let attachments = object
            .get("attachments")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        for attachment in attachments {
            let Some(url) = attachment.get("url").and_then(Value::as_str) else {
                continue;
            };
            if !url.starts_with("data:") || !url.contains(',') {
                continue;
            }
            let data = match url.find(',') {
                Some(index) => &url[index + 1..],
                None => url,
            };
            value.push(ToolContent::File {
                uri: data.to_string(),
                mime: attachment
                    .get("mime")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: None,
            });
        }
        return ToolResultValue::Content { value };
    }
    ToolResultValue::Json {
        value: output.clone(),
    }
}

/// The user branch of `toModelMessagesEffect` (message-v2.ts:198-242).
fn build_user_message(msg: &WithParts, options: &RenderOptions, result: &mut Vec<UiMessage>) {
    let mut parts: Vec<UiPart> = Vec::new();
    for part in &msg.parts {
        match part {
            V1Part::Text { text, ignored, .. } => {
                // "User message parts should never be empty"
                if ignored.unwrap_or(false) || text.is_empty() {
                    continue;
                }
                parts.push(UiPart::Text {
                    text: text.clone(),
                    provider_metadata: None,
                });
            }
            V1Part::File {
                mime,
                filename,
                url,
                ..
            } => {
                // text/plain and directory files are converted into text
                // parts elsewhere — ignore them here.
                if mime == "text/plain" || mime == "application/x-directory" {
                    continue;
                }
                if options.strip_media && is_media(mime) {
                    parts.push(UiPart::Text {
                        text: format!(
                            "[Attached {}: {}]",
                            mime,
                            filename.clone().unwrap_or_else(|| "file".to_string())
                        ),
                        provider_metadata: None,
                    });
                } else {
                    parts.push(UiPart::File {
                        url: url.clone(),
                        media_type: mime.clone(),
                        filename: filename.clone(),
                    });
                }
            }
            V1Part::Compaction { .. } => parts.push(UiPart::Text {
                text: "What did we do so far?".to_string(),
                provider_metadata: None,
            }),
            V1Part::Subtask { .. } => parts.push(UiPart::Text {
                text: "The following tool was executed by the user".to_string(),
                provider_metadata: None,
            }),
            _ => {}
        }
    }
    if !parts.is_empty() {
        result.push(UiMessage {
            id: message_id(&msg.info).to_string(),
            role: MessageRole::User,
            parts,
        });
    }
}

/// The assistant branch of `toModelMessagesEffect` (message-v2.ts:244-401).
fn build_assistant_message(
    msg: &WithParts,
    model: &RenderModel,
    options: &RenderOptions,
    different_model: bool,
    result: &mut Vec<UiMessage>,
) {
    // hasSignedReasoning (message-v2.ts:273-279)
    let has_signed_reasoning = msg.parts.iter().any(|part| match part {
        V1Part::Reasoning { metadata, .. } => metadata
            .as_ref()
            .and_then(|m| m.get("anthropic"))
            .and_then(|anthropic| anthropic.get("signature"))
            .map(|signature| !signature.is_null())
            .unwrap_or(false),
        _ => false,
    });
    let mut ui_parts: Vec<UiPart> = Vec::new();
    // Media from tool results extracted for providers that don't support
    // that media type in tool results.
    let mut media: Vec<(String, String, Option<String>)> = Vec::new();

    for part in &msg.parts {
        match part {
            V1Part::Text { text, metadata, .. } => {
                let text = if text.is_empty() && has_signed_reasoning {
                    " ".to_string()
                } else {
                    text.clone()
                };
                ui_parts.push(UiPart::Text {
                    text,
                    provider_metadata: if different_model {
                        None
                    } else {
                        metadata.clone()
                    },
                });
            }
            V1Part::StepStart { .. } => ui_parts.push(UiPart::StepStart),
            V1Part::Tool {
                tool,
                call_id,
                state,
                metadata,
                ..
            } => {
                ui_parts.push(UiPart::Tool(UiToolPart {
                    tool: tool.clone(),
                    call_id: call_id.clone(),
                    input: tool_state_input(state),
                    state: tool_ui_state(state, model, options, &mut media),
                    provider_executed: metadata
                        .as_ref()
                        .and_then(|m| m.get("providerExecuted"))
                        .filter(|v| js_truthy(v))
                        .map(|_| true),
                    call_provider_metadata: if different_model {
                        None
                    } else {
                        provider_meta(metadata.as_ref())
                    },
                }));
            }
            V1Part::Reasoning { text, metadata, .. } => {
                if different_model {
                    // Demote reasoning to plain text when replaying history
                    // under a different model (message-v2.ts:362-376).
                    if !text.trim().is_empty() {
                        ui_parts.push(UiPart::Text {
                            text: text.clone(),
                            provider_metadata: None,
                        });
                    }
                    continue;
                }
                ui_parts.push(UiPart::Reasoning {
                    text: text.clone(),
                    provider_metadata: metadata.clone(),
                });
            }
            _ => {}
        }
    }

    if ui_parts.is_empty() {
        return;
    }
    result.push(UiMessage {
        id: message_id(&msg.info).to_string(),
        role: MessageRole::Assistant,
        parts: ui_parts,
    });
    // Inject extracted media as a user message for providers that don't
    // support media in tool results (message-v2.ts:382-399).
    if !media.is_empty() {
        let mut parts = vec![UiPart::Text {
            text: SYNTHETIC_ATTACHMENT_PROMPT.to_string(),
            provider_metadata: None,
        }];
        for (mime, url, filename) in media {
            parts.push(UiPart::File {
                url,
                media_type: mime,
                filename,
            });
        }
        result.push(UiMessage {
            id: crate::session::ids::MessageId::ascending(None).expect("valid id"),
            role: MessageRole::User,
            parts,
        });
    }
}

/// JS truthiness for JSON values (`part.metadata?.providerExecuted ? ...`,
/// message-v2.ts:321-358).
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(_) => true,
    }
}

/// The tool part state machine of message-v2.ts:292-360.
fn tool_ui_state(
    state: &V1ToolState,
    model: &RenderModel,
    options: &RenderOptions,
    media: &mut Vec<(String, String, Option<String>)>,
) -> UiToolState {
    match state {
        V1ToolState::Completed {
            output,
            time,
            attachments,
            ..
        } => {
            let compacted = time.compacted.is_some();
            let output_text = if compacted {
                "[Old tool result content cleared]".to_string()
            } else {
                truncate_tool_output(output, options.tool_output_max_chars)
            };
            let attachments: Vec<&V1FilePart> = if compacted || options.strip_media {
                Vec::new()
            } else {
                attachments.iter().flatten().collect()
            };
            for attachment in attachments.iter() {
                let V1FilePart::File {
                    mime,
                    url,
                    filename,
                    ..
                } = attachment;
                if is_media(mime) && !supports_media_in_tool_result(model, mime) {
                    media.push((mime.clone(), url.clone(), filename.clone()));
                }
            }
            let final_attachments: Vec<&V1FilePart> = attachments
                .iter()
                .copied()
                .filter(|a| match a {
                    V1FilePart::File { mime, .. } => {
                        !is_media(mime) || supports_media_in_tool_result(model, mime)
                    }
                })
                .collect();
            let output = if final_attachments.is_empty() {
                Value::String(output_text)
            } else {
                json!({
                    "text": output_text,
                    "attachments": final_attachments
                        .iter()
                        .map(|a| serde_json::to_value(a).unwrap_or(Value::Null))
                        .collect::<Vec<_>>(),
                })
            };
            UiToolState::Available { output }
        }
        V1ToolState::Error {
            metadata, error, ..
        } => {
            // `metadata.interrupted === true` carries the (string) output as
            // an `output-available` part (message-v2.ts:325-347).
            let interrupted = metadata
                .as_ref()
                .and_then(|m| m.get("interrupted"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let output = metadata
                .as_ref()
                .and_then(|m| m.get("output"))
                .and_then(Value::as_str);
            if interrupted {
                if let Some(output) = output {
                    return UiToolState::Available {
                        output: Value::String(output.to_string()),
                    };
                }
            }
            UiToolState::Error {
                error_text: error.clone(),
            }
        }
        // Pending/running tool calls become `output-error` parts to prevent
        // dangling tool_use blocks (message-v2.ts:349-360).
        V1ToolState::Pending { .. } | V1ToolState::Running { .. } => UiToolState::Error {
            error_text: "[Tool execution was interrupted]".to_string(),
        },
    }
}

fn tool_state_input(state: &V1ToolState) -> Value {
    let input = match state {
        V1ToolState::Pending { input, .. }
        | V1ToolState::Running { input, .. }
        | V1ToolState::Completed { input, .. }
        | V1ToolState::Error { input, .. } => input.clone(),
    };
    Value::Object(input)
}

/// `toModelMessagesEffect` (message-v2.ts:131-402): builds the
/// `UIMessage[]`.
fn to_ui_messages(
    input: &[WithParts],
    model: &RenderModel,
    options: &RenderOptions,
) -> Vec<UiMessage> {
    let mut result: Vec<UiMessage> = Vec::new();
    for msg in input {
        if msg.parts.is_empty() {
            continue;
        }
        match &msg.info {
            opencode_schema::session_v1::V1Message::User { .. } => {
                build_user_message(msg, options, &mut result);
            }
            opencode_schema::session_v1::V1Message::Assistant {
                error,
                provider_id,
                model_id,
                ..
            } => {
                let different_model = format!("{}/{}", model.provider_id, model.id)
                    != format!("{}/{}", provider_id, model_id);
                // Errored assistant turns are dropped from model input
                // entirely unless they are aborted *and* still carry a
                // visible (non step-start/reasoning) part (249-255).
                if let Some(error) = error {
                    let aborted = matches!(error, AssistantError::Aborted { .. });
                    let visible = msg.parts.iter().any(|part| {
                        !matches!(part, V1Part::StepStart { .. } | V1Part::Reasoning { .. })
                    });
                    if !(aborted && visible) {
                        continue;
                    }
                }
                build_assistant_message(msg, model, options, different_model, &mut result);
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// convertToModelMessages (ai@6.0.168 ui/convert-to-model-messages.ts)
// ---------------------------------------------------------------------------

/// `convertToModelMessages` reduced to the shapes opencode produces:
/// assistant blocks are split on `step-start`; tool results of
/// non-provider-executed calls move into a following `tool` role message.
fn convert_to_model_messages(messages: Vec<UiMessage>) -> Vec<Message> {
    let mut model_messages = Vec::new();
    for message in messages {
        match message.role {
            MessageRole::User => {
                let content = message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        UiPart::Text {
                            text,
                            provider_metadata,
                        } => Some(ContentPart::Text {
                            text: text.clone(),
                            cache: None,
                            metadata: None,
                            provider_metadata: render_provider_metadata(provider_metadata),
                        }),
                        UiPart::File {
                            url,
                            media_type,
                            filename,
                        } => Some(ContentPart::Media {
                            media_type: media_type.clone(),
                            data: url.clone(),
                            filename: filename.clone(),
                            metadata: None,
                        }),
                        _ => None,
                    })
                    .collect();
                model_messages.push(message_of(MessageRole::User, content));
            }
            MessageRole::Assistant => {
                let mut block: Vec<&UiPart> = Vec::new();
                for part in &message.parts {
                    match part {
                        UiPart::StepStart => {
                            flush_assistant_block(&mut block, &mut model_messages);
                        }
                        _ => block.push(part),
                    }
                }
                flush_assistant_block(&mut block, &mut model_messages);
            }
            _ => {}
        }
    }
    model_messages
}

fn message_of(role: MessageRole, content: Vec<ContentPart>) -> Message {
    Message {
        id: None,
        role,
        content,
        metadata: None,
        native: None,
    }
}

fn tool_result_part(tool: &UiToolPart) -> ContentPart {
    let result = match &tool.state {
        UiToolState::Available { output } => to_model_output(output),
        UiToolState::Error { error_text } => ToolResultValue::Error {
            value: Value::String(error_text.clone()),
        },
    };
    ContentPart::ToolResult {
        id: tool.call_id.clone(),
        name: tool.tool.clone(),
        result,
        provider_executed: None,
        cache: None,
        metadata: None,
        provider_metadata: render_provider_metadata(&tool.call_provider_metadata),
    }
}

fn flush_assistant_block(block: &mut Vec<&UiPart>, out: &mut Vec<Message>) {
    if block.is_empty() {
        return;
    }
    let mut content: Vec<ContentPart> = Vec::new();
    for part in block.iter() {
        match part {
            UiPart::Text {
                text,
                provider_metadata,
            } => content.push(ContentPart::Text {
                text: text.clone(),
                cache: None,
                metadata: None,
                provider_metadata: render_provider_metadata(provider_metadata),
            }),
            UiPart::File {
                url,
                media_type,
                filename,
            } => content.push(ContentPart::Media {
                media_type: media_type.clone(),
                data: url.clone(),
                filename: filename.clone(),
                metadata: None,
            }),
            UiPart::Reasoning {
                text,
                provider_metadata,
            } => content.push(ContentPart::Reasoning {
                text: text.clone(),
                encrypted: None,
                metadata: None,
                provider_metadata: render_provider_metadata(provider_metadata),
            }),
            UiPart::Tool(tool) => {
                content.push(ContentPart::ToolCall {
                    id: tool.call_id.clone(),
                    name: tool.tool.clone(),
                    input: tool.input.clone(),
                    provider_executed: tool.provider_executed,
                    metadata: None,
                    provider_metadata: render_provider_metadata(&tool.call_provider_metadata),
                });
                // `providerExecuted === true` tool results stay inside the
                // assistant message content.
                if tool.provider_executed == Some(true) {
                    content.push(tool_result_part(tool));
                }
            }
            UiPart::StepStart => {}
        }
    }
    out.push(message_of(MessageRole::Assistant, content));

    // Non-provider-executed tool calls get their results in a following
    // `tool` role message.
    let mut tool_content = Vec::new();
    for part in block.iter() {
        if let UiPart::Tool(tool) = part {
            if tool.provider_executed != Some(true) {
                tool_content.push(tool_result_part(tool));
            }
        }
    }
    if !tool_content.is_empty() {
        out.push(message_of(MessageRole::Tool, tool_content));
    }
    block.clear();
}

/// `toModelMessages` (message-v2.ts:131-423): render stored messages into
/// provider request messages over the `opencode-llm` request model.
pub fn to_model_messages(
    input: &[WithParts],
    model: &RenderModel,
    options: Option<RenderOptions>,
) -> Vec<Message> {
    let options = options.unwrap_or_default();
    let messages = to_ui_messages(input, model, &options);
    // Final pass drops messages that only contain `step-start` (408).
    let messages: Vec<UiMessage> = messages
        .into_iter()
        .filter(|msg| {
            msg.parts
                .iter()
                .any(|part| !matches!(part, UiPart::StepStart))
        })
        .collect();
    convert_to_model_messages(messages)
}

/// The `toolNames` set (message-v2.ts:137) — every tool seen installs the
/// same [`to_model_output`] adapter, so this is only observable as "the set
/// of tools whose outputs use that adapter".
pub fn tool_names(input: &[WithParts]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for msg in input {
        for part in &msg.parts {
            if let V1Part::Tool { tool, .. } = part {
                names.insert(tool.clone());
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencode_schema::session_v1::{
        AssistantTime, TextPartTime, ToolStateCompletedTime, ToolStateErrorTime,
        ToolStateRunningTime, UserTime, V1Path, V1StepTokens, V1TokenCache, V1UserModel,
    };

    fn anthropic_model() -> RenderModel {
        RenderModel {
            provider_id: "anthropic".to_string(),
            id: "claude-sonnet-4-5".to_string(),
            api_npm: "@ai-sdk/anthropic".to_string(),
            api_id: "claude-sonnet-4-5".to_string(),
        }
    }

    fn user(id: &str, parts: Vec<V1Part>) -> WithParts {
        WithParts {
            info: opencode_schema::session_v1::V1Message::User {
                id: id.to_string(),
                session_id: "ses_1".to_string(),
                time: UserTime { created: 1.0 },
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
            },
            parts,
        }
    }

    fn assistant(id: &str, parts: Vec<V1Part>) -> WithParts {
        WithParts {
            info: opencode_schema::session_v1::V1Message::Assistant {
                id: id.to_string(),
                session_id: "ses_1".to_string(),
                time: AssistantTime {
                    created: 1,
                    completed: None,
                },
                error: None,
                parent_id: "msg_0".to_string(),
                model_id: "claude-sonnet-4-5".to_string(),
                provider_id: "anthropic".to_string(),
                mode: "primary".to_string(),
                agent: "build".to_string(),
                path: V1Path {
                    cwd: "/repo".to_string(),
                    root: "/repo".to_string(),
                },
                summary: None,
                cost: 0.0,
                tokens: V1StepTokens {
                    total: None,
                    input: 0.0,
                    output: 0.0,
                    reasoning: 0.0,
                    cache: V1TokenCache {
                        read: 0.0,
                        write: 0.0,
                    },
                },
                structured: None,
                variant: None,
                finish: None,
            },
            parts,
        }
    }

    fn text_part(id: &str, message: &str) -> V1Part {
        V1Part::Text {
            id: id.to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_1".to_string(),
            text: message.to_string(),
            synthetic: None,
            ignored: None,
            time: Some(TextPartTime {
                start: 1,
                end: Some(2),
            }),
            metadata: None,
        }
    }

    fn tool_part(id: &str, call: &str, state: V1ToolState) -> V1Part {
        V1Part::Tool {
            id: id.to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_1".to_string(),
            call_id: call.to_string(),
            tool: "bash".to_string(),
            state,
            metadata: None,
        }
    }

    fn completed_tool_state(output: &str) -> V1ToolState {
        V1ToolState::Completed {
            input: serde_json::Map::new(),
            output: output.to_string(),
            title: String::new(),
            metadata: serde_json::Map::new(),
            time: ToolStateCompletedTime {
                start: 1,
                end: 2,
                compacted: None,
            },
            attachments: None,
        }
    }

    fn content_json(message: &Message) -> Value {
        serde_json::to_value(&message.content).unwrap()
    }

    #[test]
    fn truncate_tool_output_matches_js_length_semantics() {
        assert_eq!(truncate_tool_output("hello", None), "hello");
        assert_eq!(truncate_tool_output("hello", Some(0)), "hello");
        assert_eq!(truncate_tool_output("hello", Some(10)), "hello");
        assert_eq!(
            truncate_tool_output("hello", Some(3)),
            "hel\n[Tool output truncated for compaction: omitted 2 chars]"
        );
    }

    #[test]
    fn renders_plain_user_text() {
        let input = vec![user("msg_u", vec![text_part("prt_1", "hello world")])];
        let out = to_model_messages(&input, &anthropic_model(), None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, MessageRole::User);
        assert_eq!(
            content_json(&out[0]),
            json!([{"type": "text", "text": "hello world"}])
        );
    }

    #[test]
    fn skips_ignored_and_empty_user_text() {
        let mut ignored = text_part("prt_1", "");
        if let V1Part::Text { ignored, .. } = &mut ignored {
            *ignored = Some(true);
        }
        let input = vec![user("msg_u", vec![ignored, text_part("prt_2", "")])];
        let out = to_model_messages(&input, &anthropic_model(), None);
        assert!(out.is_empty(), "empty and ignored parts are skipped");
    }

    #[test]
    fn compaction_and_subtask_render_synthetic_text() {
        let compaction = V1Part::Compaction {
            id: "prt_c".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_u".to_string(),
            auto: true,
            overflow: None,
            tail_start_id: None,
        };
        let subtask = V1Part::Subtask {
            id: "prt_s".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_u".to_string(),
            prompt: "do it".to_string(),
            description: String::new(),
            agent: "general".to_string(),
            model: None,
            command: None,
        };
        let input = vec![user("msg_u", vec![compaction, subtask])];
        let out = to_model_messages(&input, &anthropic_model(), None);
        assert_eq!(
            content_json(&out[0]),
            json!([
                {"type": "text", "text": "What did we do so far?"},
                {"type": "text", "text": "The following tool was executed by the user"},
            ])
        );
    }

    #[test]
    fn file_parts_skip_text_plain_and_directories() {
        let file_part = |mime: &str| V1Part::File {
            id: "prt_f".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_u".to_string(),
            mime: mime.to_string(),
            filename: Some("a.png".to_string()),
            url: "data:image/png;base64,aGk=".to_string(),
            source: None,
        };
        let input = vec![user(
            "msg_u",
            vec![
                file_part("text/plain"),
                file_part("application/x-directory"),
                file_part("image/png"),
            ],
        )];
        let out = to_model_messages(&input, &anthropic_model(), None);
        assert_eq!(
            content_json(&out[0]),
            json!([
                {"type": "media", "mediaType": "image/png", "data": "data:image/png;base64,aGk=", "filename": "a.png"}
            ])
        );
    }

    #[test]
    fn strip_media_replaces_media_with_placeholder() {
        let file_part = V1Part::File {
            id: "prt_f".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_u".to_string(),
            mime: "image/png".to_string(),
            filename: Some("screenshot.png".to_string()),
            url: "data:image/png;base64,aGk=".to_string(),
            source: None,
        };
        let input = vec![user("msg_u", vec![file_part])];
        let out = to_model_messages(
            &input,
            &anthropic_model(),
            Some(RenderOptions {
                strip_media: true,
                tool_output_max_chars: None,
            }),
        );
        assert_eq!(
            content_json(&out[0]),
            json!([{"type": "text", "text": "[Attached image/png: screenshot.png]"}])
        );
    }

    #[test]
    fn errored_assistant_turns_are_dropped() {
        let mut msg = assistant("msg_a", vec![text_part("prt_1", "hi")]);
        if let opencode_schema::session_v1::V1Message::Assistant { error, .. } = &mut msg.info {
            *error = Some(AssistantError::Api {
                message: "500".to_string(),
                status_code: Some(500),
                is_retryable: true,
                response_headers: None,
                response_body: None,
                metadata: None,
            });
        }
        let out = to_model_messages(&[msg], &anthropic_model(), None);
        assert!(out.is_empty());
    }

    #[test]
    fn aborted_assistant_with_visible_parts_is_kept() {
        let mut msg = assistant("msg_a", vec![text_part("prt_1", "hi")]);
        if let opencode_schema::session_v1::V1Message::Assistant { error, .. } = &mut msg.info {
            *error = Some(AssistantError::Aborted {
                message: "Aborted".to_string(),
            });
        }
        let out = to_model_messages(&[msg], &anthropic_model(), None);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn aborted_assistant_with_only_reasoning_is_dropped() {
        let reasoning = V1Part::Reasoning {
            id: "prt_r".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_1".to_string(),
            text: "thinking".to_string(),
            metadata: None,
            time: opencode_schema::session_v1::ReasoningTime {
                start: 1,
                end: None,
            },
        };
        let mut msg = assistant("msg_a", vec![reasoning]);
        if let opencode_schema::session_v1::V1Message::Assistant { error, .. } = &mut msg.info {
            *error = Some(AssistantError::Aborted {
                message: "Aborted".to_string(),
            });
        }
        let out = to_model_messages(&[msg], &anthropic_model(), None);
        assert!(out.is_empty());
    }

    #[test]
    fn empty_text_separator_fix_with_signed_reasoning() {
        let reasoning = V1Part::Reasoning {
            id: "prt_r".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_1".to_string(),
            text: "thinking".to_string(),
            metadata: None,
            time: opencode_schema::session_v1::ReasoningTime {
                start: 1,
                end: None,
            },
        };
        // hasSignedReasoning requires metadata.anthropic.signature != null
        let mut signed = reasoning.clone();
        if let V1Part::Reasoning { metadata, .. } = &mut signed {
            *metadata =
                Some(serde_json::from_value(json!({"anthropic": {"signature": "sig"}})).unwrap());
        }
        let msg = assistant("msg_a", vec![signed, text_part("prt_t", "")]);
        let out = to_model_messages(&[msg], &anthropic_model(), None);
        assert_eq!(out.len(), 1);
        assert_eq!(
            content_json(&out[0]),
            json!([
                {"type": "reasoning", "text": "thinking", "providerMetadata": {"anthropic": {"signature": "sig"}}},
                {"type": "text", "text": " "},
            ])
        );
    }

    #[test]
    fn pending_and_running_tools_render_interrupted() {
        let pending = tool_part(
            "prt_p",
            "call_p",
            V1ToolState::Pending {
                input: serde_json::Map::new(),
                raw: String::new(),
            },
        );
        let running = tool_part(
            "prt_r",
            "call_r",
            V1ToolState::Running {
                input: serde_json::Map::new(),
                title: None,
                metadata: None,
                time: ToolStateRunningTime { start: 1 },
            },
        );
        let msg = assistant("msg_a", vec![pending, running]);
        let out = to_model_messages(&[msg], &anthropic_model(), None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, MessageRole::Assistant);
        assert_eq!(out[1].role, MessageRole::Tool);
        let assistant_calls: Vec<&ContentPart> = out[0]
            .content
            .iter()
            .filter(|p| matches!(p, ContentPart::ToolCall { .. }))
            .collect();
        assert_eq!(assistant_calls.len(), 2);
        match (&out[1].content[0], &out[1].content[1]) {
            (
                ContentPart::ToolResult { result, .. },
                ContentPart::ToolResult {
                    result: result2, ..
                },
            ) => {
                assert_eq!(
                    result,
                    &ToolResultValue::Error {
                        value: json!("[Tool execution was interrupted]")
                    }
                );
                assert_eq!(result, result2);
            }
            _ => panic!("expected two tool results"),
        }
    }

    #[test]
    fn error_tool_with_interrupted_string_output_renders_available() {
        let state = V1ToolState::Error {
            input: serde_json::Map::new(),
            error: "boom".to_string(),
            metadata: Some(
                serde_json::from_value(json!({"interrupted": true, "output": "partial output"}))
                    .unwrap(),
            ),
            time: ToolStateErrorTime { start: 1, end: 2 },
        };
        let msg = assistant("msg_a", vec![tool_part("prt_e", "call_e", state)]);
        let out = to_model_messages(&[msg], &anthropic_model(), None);
        match &out[1].content[0] {
            ContentPart::ToolResult { result, .. } => assert_eq!(
                result,
                &ToolResultValue::Text {
                    value: json!("partial output")
                }
            ),
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn error_tool_renders_error_text() {
        let state = V1ToolState::Error {
            input: serde_json::Map::new(),
            error: "boom".to_string(),
            metadata: None,
            time: ToolStateErrorTime { start: 1, end: 2 },
        };
        let msg = assistant("msg_a", vec![tool_part("prt_e", "call_e", state)]);
        let out = to_model_messages(&[msg], &anthropic_model(), None);
        match &out[1].content[0] {
            ContentPart::ToolResult { result, .. } => assert_eq!(
                result,
                &ToolResultValue::Error {
                    value: json!("boom")
                }
            ),
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn compacted_tool_output_is_cleared() {
        let mut state = completed_tool_state("long output");
        if let V1ToolState::Completed { time, .. } = &mut state {
            time.compacted = Some(1234);
        }
        let msg = assistant("msg_a", vec![tool_part("prt_c", "call_c", state)]);
        let out = to_model_messages(&[msg], &anthropic_model(), None);
        match &out[1].content[0] {
            ContentPart::ToolResult { result, .. } => assert_eq!(
                result,
                &ToolResultValue::Text {
                    value: json!("[Old tool result content cleared]")
                }
            ),
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn provider_executed_tool_results_stay_in_assistant_message() {
        let mut part = tool_part("prt_c", "call_c", completed_tool_state("done"));
        if let V1Part::Tool { metadata, .. } = &mut part {
            *metadata = Some(serde_json::from_value(json!({"providerExecuted": true})).unwrap());
        }
        let msg = assistant("msg_a", vec![part]);
        let out = to_model_messages(&[msg], &anthropic_model(), None);
        assert_eq!(out.len(), 1, "no separate tool message expected");
        assert_eq!(out[0].content.len(), 2);
        assert!(matches!(
            &out[0].content[0],
            ContentPart::ToolCall {
                provider_executed: Some(true),
                ..
            }
        ));
        match &out[0].content[1] {
            ContentPart::ToolResult {
                provider_executed, ..
            } => assert_eq!(*provider_executed, None),
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn call_provider_metadata_strips_provider_executed() {
        let mut part = tool_part("prt_c", "call_c", completed_tool_state("done"));
        if let V1Part::Tool { metadata, .. } = &mut part {
            *metadata = Some(
                serde_json::from_value(
                    json!({"providerExecuted": true, "anthropic": {"signature": "s"}}),
                )
                .unwrap(),
            );
        }
        let msg = assistant("msg_a", vec![part]);
        let out = to_model_messages(&[msg], &anthropic_model(), None);
        match &out[0].content[0] {
            ContentPart::ToolCall {
                provider_metadata: Some(metadata),
                ..
            } => {
                let keys: Vec<&str> = metadata.keys().map(|k| k.as_str()).collect();
                assert_eq!(keys, vec!["anthropic"]);
            }
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn different_model_demotes_reasoning_and_strips_metadata() {
        let mut part = text_part("prt_t", "hi");
        if let V1Part::Text { metadata, .. } = &mut part {
            *metadata =
                Some(serde_json::from_value(json!({"anthropic": {"signature": "s"}})).unwrap());
        }
        let reasoning = V1Part::Reasoning {
            id: "prt_r".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_1".to_string(),
            text: "  deep thought  ".to_string(),
            metadata: None,
            time: opencode_schema::session_v1::ReasoningTime {
                start: 1,
                end: None,
            },
        };
        let msg = assistant("msg_a", vec![part, reasoning]);
        // Different model: anthropic/claude-sonnet-4-5 vs openai/gpt-4o.
        let mut model = anthropic_model();
        model.provider_id = "openai".to_string();
        model.id = "gpt-4o".to_string();
        model.api_npm = "@ai-sdk/openai".to_string();
        model.api_id = "gpt-4o".to_string();
        let out = to_model_messages(&[msg], &model, None);
        assert_eq!(
            content_json(&out[0]),
            json!([
                {"type": "text", "text": "hi"},
                {"type": "text", "text": "  deep thought  "},
            ])
        );
    }

    #[test]
    fn different_model_drops_whitespace_only_reasoning() {
        let reasoning = V1Part::Reasoning {
            id: "prt_r".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_1".to_string(),
            text: "   ".to_string(),
            metadata: None,
            time: opencode_schema::session_v1::ReasoningTime {
                start: 1,
                end: None,
            },
        };
        let msg = assistant("msg_a", vec![reasoning]);
        let mut model = anthropic_model();
        model.provider_id = "openai".to_string();
        model.id = "gpt-4o".to_string();
        let out = to_model_messages(&[msg], &model, None);
        assert!(out.is_empty());
    }

    #[test]
    fn step_start_only_messages_are_dropped() {
        let step = V1Part::StepStart {
            id: "prt_s".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_a".to_string(),
            snapshot: None,
        };
        let msg = assistant("msg_a", vec![step]);
        let out = to_model_messages(&[msg], &anthropic_model(), None);
        assert!(out.is_empty());
    }

    #[test]
    fn step_start_splits_assistant_blocks() {
        let step = V1Part::StepStart {
            id: "prt_s".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_a".to_string(),
            snapshot: None,
        };
        let msg = assistant(
            "msg_a",
            vec![
                step.clone(),
                tool_part("prt_c", "call_c", completed_tool_state("done")),
                step,
                text_part("prt_t", "after"),
            ],
        );
        let out = to_model_messages(&[msg], &anthropic_model(), None);
        // [assistant(tool-call), tool(result), assistant(text)]
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].role, MessageRole::Assistant);
        assert_eq!(out[1].role, MessageRole::Tool);
        assert_eq!(out[2].role, MessageRole::Assistant);
        assert_eq!(
            content_json(&out[2]),
            json!([{"type": "text", "text": "after"}])
        );
    }

    #[test]
    fn extracted_media_becomes_synthetic_user_message() {
        // openai chat supports media in tool results; a "plain" api npm
        // (unknown) does not.
        let attachment = V1FilePart::File {
            id: "prt_f".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_a".to_string(),
            mime: "image/png".to_string(),
            filename: Some("shot.png".to_string()),
            url: "data:image/png;base64,aGk=".to_string(),
            source: None,
        };
        let mut state = completed_tool_state("done");
        if let V1ToolState::Completed {
            attachments, time, ..
        } = &mut state
        {
            time.compacted = None;
            *attachments = Some(vec![attachment]);
        }
        let msg = assistant("msg_a", vec![tool_part("prt_c", "call_c", state)]);
        let mut model = anthropic_model();
        model.api_npm = "@ai-sdk/unknown".to_string();
        let out = to_model_messages(&[msg], &model, None);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].role, MessageRole::Assistant);
        assert_eq!(out[1].role, MessageRole::Tool);
        assert_eq!(out[2].role, MessageRole::User);
        // The synthetic user message: prompt text + media file part.
        assert_eq!(
            content_json(&out[2]),
            json!([
                {"type": "text", "text": "Attached media from tool result:"},
                {"type": "media", "mediaType": "image/png", "data": "data:image/png;base64,aGk=", "filename": "shot.png"},
            ])
        );
        // The extracted media is no longer in the tool result.
        match &out[1].content[0] {
            ContentPart::ToolResult { result, .. } => assert_eq!(
                result,
                &ToolResultValue::Text {
                    value: json!("done")
                }
            ),
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn supported_media_stays_in_tool_result() {
        // anthropic supports media in tool results.
        let attachment = V1FilePart::File {
            id: "prt_f".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_a".to_string(),
            mime: "image/png".to_string(),
            filename: Some("shot.png".to_string()),
            url: "data:image/png;base64,aGk=".to_string(),
            source: None,
        };
        let mut state = completed_tool_state("done");
        if let V1ToolState::Completed {
            attachments, time, ..
        } = &mut state
        {
            time.compacted = None;
            *attachments = Some(vec![attachment]);
        }
        let msg = assistant("msg_a", vec![tool_part("prt_c", "call_c", state)]);
        let out = to_model_messages(&[msg], &anthropic_model(), None);
        assert_eq!(out.len(), 2, "no synthetic user message expected");
        match &out[1].content[0] {
            ContentPart::ToolResult { result, .. } => assert_eq!(
                result,
                &ToolResultValue::Content {
                    value: vec![
                        ToolContent::Text {
                            text: "done".to_string()
                        },
                        ToolContent::File {
                            uri: "aGk=".to_string(),
                            mime: "image/png".to_string(),
                            name: None,
                        },
                    ]
                }
            ),
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn google_gemini3_supports_media() {
        let mut model = anthropic_model();
        model.api_npm = "@ai-sdk/google".to_string();
        model.api_id = "gemini-3-pro".to_string();
        assert!(supports_media_in_tool_result(&model, "application/pdf"));
        model.api_id = "gemini-2.5-pro".to_string();
        assert!(!supports_media_in_tool_result(&model, "image/png"));
    }

    #[test]
    fn bedrock_mantle_supports_all_bedrock_supports_images() {
        let mut model = anthropic_model();
        model.api_npm = "@ai-sdk/amazon-bedrock".to_string();
        assert!(supports_media_in_tool_result(&model, "image/png"));
        assert!(!supports_media_in_tool_result(&model, "application/pdf"));
        model.api_npm = "@ai-sdk/amazon-bedrock/mantle".to_string();
        assert!(supports_media_in_tool_result(&model, "application/pdf"));
    }

    #[test]
    fn messages_with_no_parts_are_skipped() {
        let input = vec![user("msg_u", vec![])];
        let out = to_model_messages(&input, &anthropic_model(), None);
        assert!(out.is_empty());
    }

    #[test]
    fn tool_output_truncation_applies() {
        let state = completed_tool_state("a super long tool output");
        let msg = assistant("msg_a", vec![tool_part("prt_c", "call_c", state)]);
        let out = to_model_messages(
            &[msg],
            &anthropic_model(),
            Some(RenderOptions {
                strip_media: false,
                tool_output_max_chars: Some(9),
            }),
        );
        match &out[1].content[0] {
            ContentPart::ToolResult { result, .. } => assert_eq!(
                result,
                &ToolResultValue::Text {
                    value: json!(
                        "a super l\n[Tool output truncated for compaction: omitted 15 chars]"
                    )
                }
            ),
            other => panic!("expected tool result, got {other:?}"),
        }
    }
}
