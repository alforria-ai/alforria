//! Bedrock Converse protocol (TS `protocols/bedrock-converse.ts`).
//!
//! Request body construction (`modelId` / `system` / `messages` / `toolConfig`
//! / `inferenceConfig` / `additionalModelRequestFields`) plus the streaming
//! state machine over the AWS event stream (`messageStart`, `contentBlock*`,
//! `messageStop`, `metadata`, …). The binary event-stream framing itself lives
//! in [`crate::protocols::bedrock_event_stream`].
//!
//! Cache markers are positional `cachePoint` blocks emitted immediately after
//! the content they mark, with the same tools → system → messages breakpoint
//! budget discipline as anthropic-messages.

#![allow(clippy::result_large_err)]
#![allow(clippy::arc_with_non_send_sync)]

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use alforria_schema::llm::ToolContent;

use crate::protocols::shared;
use crate::protocols::utils::bedrock_auth;
use crate::protocols::utils::bedrock_cache;
use crate::protocols::utils::bedrock_media::{self, BedrockMediaBlock};
use crate::protocols::utils::cache::Breakpoints;
use crate::protocols::utils::lifecycle;
use crate::protocols::utils::lifecycle::FinishInput;
use crate::protocols::utils::tool_schema::ToolSchemaProjection;
use crate::protocols::utils::tool_stream::{self, PendingTool};
use crate::provider_error::is_context_overflow;
use crate::route::auth::Auth;
use crate::route::client::{RouteDefaults, RouteHandle};
use crate::route::endpoint::{Endpoint, EndpointPart};
use crate::route::framing::Framing;
use crate::route::protocol::Protocol;
use crate::schema::errors::LlmError;
use crate::schema::errors::ProviderFailureClassification;
use crate::schema::events::{LlmEvent, Usage};
use crate::schema::ids::{FinishReason, JsonMap, MessageRole, ProviderMetadata};
use crate::schema::messages::{
    ContentPart, LlmRequest, ToolChoice, ToolChoiceType, ToolDefinition, ToolResultValue,
};
use crate::schema::options::{
    CacheHint, GenerationOptions, ModelToolSchemaCompatibility, SystemPart,
};

/// Stable protocol id (TS `ADAPTER`).
pub const ADAPTER: &str = "bedrock-converse";
/// Route name used in user-facing lowering errors (TS passes
/// `"Bedrock Converse"`).
const ROUTE: &str = "Bedrock Converse";

// =========================================================================
// Request body schema (native casing, TS `BedrockConverseBody`)
// =========================================================================

#[derive(Debug, Clone, PartialEq, Serialize)]
struct BedrockTextBlock {
    text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockToolUse {
    tool_use_id: String,
    name: String,
    input: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockToolUseBlock {
    tool_use: BedrockToolUse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum BedrockToolResultStatus {
    Success,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
enum BedrockToolResultContentItem {
    Text { text: String },
    Json { json: Value },
    Image(bedrock_media::ImageBlock),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockToolResult {
    tool_use_id: String,
    content: Vec<BedrockToolResultContentItem>,
    status: BedrockToolResultStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockToolResultBlock {
    tool_result: BedrockToolResult,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockReasoningBlock {
    reasoning_content: BedrockReasoningContent,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockReasoningContent {
    reasoning_text: BedrockReasoningText,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct BedrockReasoningText {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
enum BedrockUserBlock {
    Text(BedrockTextBlock),
    Image(bedrock_media::ImageBlock),
    Document(bedrock_media::DocumentBlock),
    ToolResult(BedrockToolResultBlock),
    CachePoint(bedrock_cache::CachePointBlock),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
enum BedrockAssistantBlock {
    Text(BedrockTextBlock),
    Reasoning(BedrockReasoningBlock),
    ToolUse(BedrockToolUseBlock),
    CachePoint(bedrock_cache::CachePointBlock),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum BedrockRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
enum BedrockMessage {
    User {
        role: BedrockRole,
        content: Vec<BedrockUserBlock>,
    },
    Assistant {
        role: BedrockRole,
        content: Vec<BedrockAssistantBlock>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
enum BedrockSystemBlock {
    Text(BedrockTextBlock),
    CachePoint(bedrock_cache::CachePointBlock),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockToolSpecBody {
    name: String,
    description: String,
    input_schema: BedrockInputSchema,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct BedrockInputSchema {
    json: JsonMap,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockToolSpec {
    tool_spec: BedrockToolSpecBody,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
enum BedrockTool {
    Spec(BedrockToolSpec),
    CachePoint(bedrock_cache::CachePointBlock),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct BedrockEmpty {}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct BedrockToolChoiceName {
    name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
enum BedrockToolChoice {
    Auto { auto: BedrockEmpty },
    Any { any: BedrockEmpty },
    Tool { tool: BedrockToolChoiceName },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockInferenceConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockToolConfig {
    tools: Vec<BedrockTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<BedrockToolChoice>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct BedrockConverseBody {
    model_id: String,
    messages: Vec<BedrockMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<BedrockSystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inference_config: Option<BedrockInferenceConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<BedrockToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    additional_model_request_fields: Option<JsonMap>,
}

// =========================================================================
// Request lowering (TS `lowerToolSpec` … `fromRequest`)
// =========================================================================

fn lower_tool_spec(tool: &ToolDefinition, input_schema: JsonMap) -> BedrockToolSpec {
    BedrockToolSpec {
        tool_spec: BedrockToolSpecBody {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: BedrockInputSchema { json: input_schema },
        },
    }
}

fn lower_tools(
    compatibility: Option<ModelToolSchemaCompatibility>,
    breakpoints: &mut Breakpoints,
    tools: &[ToolDefinition],
) -> Vec<BedrockTool> {
    let mut result = Vec::new();
    for tool in tools {
        let schema = ToolSchemaProjection::model_compatibility(&tool.input_schema, compatibility);
        result.push(BedrockTool::Spec(lower_tool_spec(tool, schema)));
        if let Some(cache_point) = bedrock_cache::block(breakpoints, tool.cache.as_ref()) {
            result.push(BedrockTool::CachePoint(cache_point));
        }
    }
    result
}

/// The text block plus its optional positional `cachePoint` follower
/// (TS `textWithCache`).
fn text_with_cache(
    breakpoints: &mut Breakpoints,
    text: &str,
    cache: Option<&CacheHint>,
) -> (String, Option<bedrock_cache::CachePointBlock>) {
    (text.to_string(), bedrock_cache::block(breakpoints, cache))
}

fn lower_tool_choice(tool_choice: &ToolChoice) -> Result<Option<BedrockToolChoice>, LlmError> {
    Ok(match shared::match_tool_choice(tool_choice, ROUTE)? {
        shared::MatchedToolChoice::Auto => Some(BedrockToolChoice::Auto {
            auto: BedrockEmpty {},
        }),
        shared::MatchedToolChoice::None => None,
        shared::MatchedToolChoice::Required => Some(BedrockToolChoice::Any {
            any: BedrockEmpty {},
        }),
        shared::MatchedToolChoice::Tool(name) => Some(BedrockToolChoice::Tool {
            tool: BedrockToolChoiceName { name },
        }),
    })
}

fn reasoning_signature(
    encrypted: &Option<String>,
    provider_metadata: &Option<ProviderMetadata>,
) -> Option<String> {
    if let Some(encrypted) = encrypted {
        return Some(encrypted.clone());
    }
    let metadata = provider_metadata.as_ref()?;
    let bedrock = metadata.get("bedrock")?;
    bedrock
        .get("signature")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn lower_tool_result_content(
    part: &ContentPart,
) -> Result<Vec<BedrockToolResultContentItem>, LlmError> {
    let ContentPart::ToolResult { .. } = part else {
        return Err(shared::invalid_request(
            "Bedrock Converse tool result lowering requires a tool result part",
        ));
    };
    match part {
        ContentPart::ToolResult { result, .. } => match result {
            ToolResultValue::Text { .. } | ToolResultValue::Error { .. } => {
                Ok(vec![BedrockToolResultContentItem::Text {
                    text: shared::tool_result_text(part),
                }])
            }
            ToolResultValue::Json { value } => Ok(vec![BedrockToolResultContentItem::Json {
                json: value.clone(),
            }]),
            ToolResultValue::Content { value } => {
                let mut content = Vec::new();
                for item in value {
                    let (media_type, data, filename) = match item {
                        ToolContent::Text { text } => {
                            content.push(BedrockToolResultContentItem::Text { text: text.clone() });
                            continue;
                        }
                        ToolContent::File { uri, mime, name } => {
                            (mime.clone(), uri.clone(), name.clone())
                        }
                    };
                    let media = bedrock_media::lower(&ContentPart::Media {
                        media_type,
                        data,
                        filename,
                        metadata: None,
                    })?;
                    if let BedrockMediaBlock::Image(image) = media {
                        content.push(BedrockToolResultContentItem::Image(image));
                    } else {
                        return Err(shared::invalid_request(
                            "Bedrock Converse only supports image media in tool results",
                        ));
                    }
                }
                Ok(content)
            }
        },
        _ => unreachable!("tool result part checked above"),
    }
}

fn lower_tool_result(part: &ContentPart) -> Result<BedrockUserBlock, LlmError> {
    let ContentPart::ToolResult { id, result, .. } = part else {
        return Err(shared::invalid_request(
            "Bedrock Converse tool result lowering requires a tool result part",
        ));
    };
    Ok(BedrockUserBlock::ToolResult(BedrockToolResultBlock {
        tool_result: BedrockToolResult {
            tool_use_id: id.clone(),
            content: lower_tool_result_content(part)?,
            status: if matches!(result, ToolResultValue::Error { .. }) {
                BedrockToolResultStatus::Error
            } else {
                BedrockToolResultStatus::Success
            },
        },
    }))
}

fn lower_user_part(
    part: &ContentPart,
    breakpoints: &mut Breakpoints,
) -> Result<Vec<BedrockUserBlock>, LlmError> {
    if !shared::supports_content(part, &["text", "media"]) {
        return Err(shared::unsupported_content(
            ROUTE,
            &MessageRole::User,
            &["text", "media"],
        ));
    }
    match part {
        ContentPart::Text { text, cache, .. } => {
            let (text, cache_point) = text_with_cache(breakpoints, text, cache.as_ref());
            let mut content = vec![BedrockUserBlock::Text(BedrockTextBlock { text })];
            if let Some(cache_point) = cache_point {
                content.push(BedrockUserBlock::CachePoint(cache_point));
            }
            Ok(content)
        }
        ContentPart::Media { .. } => {
            let media = bedrock_media::lower(part)?;
            Ok(vec![match media {
                BedrockMediaBlock::Image(image) => BedrockUserBlock::Image(image),
                BedrockMediaBlock::Document(document) => BedrockUserBlock::Document(document),
            }])
        }
        _ => Err(shared::unsupported_content(
            ROUTE,
            &MessageRole::User,
            &["text", "media"],
        )),
    }
}

fn lower_messages(
    request: &LlmRequest,
    breakpoints: &mut Breakpoints,
) -> Result<Vec<BedrockMessage>, LlmError> {
    let mut messages: Vec<BedrockMessage> = Vec::new();

    for message in &request.messages {
        match message.role {
            MessageRole::System => {
                let part = shared::wrapped_system_update(ROUTE, message)?;
                let ContentPart::Text { text, cache, .. } = &part else {
                    return Err(shared::invalid_request(
                        "wrapped system update must be text content",
                    ));
                };
                let (text, cache_point) = text_with_cache(breakpoints, text, cache.as_ref());
                let mut content = vec![BedrockUserBlock::Text(BedrockTextBlock { text })];
                if let Some(cache_point) = cache_point {
                    content.push(BedrockUserBlock::CachePoint(cache_point));
                }
                match messages.last_mut() {
                    // A chronological system update merges into the adjacent
                    // user message (TS `messages.at(-1)?.role === "user"`).
                    Some(BedrockMessage::User {
                        content: previous, ..
                    }) => {
                        previous.extend(content);
                    }
                    _ => {
                        messages.push(BedrockMessage::User {
                            role: BedrockRole::User,
                            content,
                        });
                    }
                }
            }
            MessageRole::User => {
                let mut content = Vec::new();
                for part in &message.content {
                    content.extend(lower_user_part(part, breakpoints)?);
                }
                messages.push(BedrockMessage::User {
                    role: BedrockRole::User,
                    content,
                });
            }
            MessageRole::Assistant => {
                let mut content = Vec::new();
                for part in &message.content {
                    if !shared::supports_content(part, &["text", "reasoning", "tool-call"]) {
                        return Err(shared::unsupported_content(
                            ROUTE,
                            &MessageRole::Assistant,
                            &["text", "reasoning", "tool-call"],
                        ));
                    }
                    match part {
                        ContentPart::Text { text, cache, .. } => {
                            let (text, cache_point) =
                                text_with_cache(breakpoints, text, cache.as_ref());
                            content.push(BedrockAssistantBlock::Text(BedrockTextBlock { text }));
                            if let Some(cache_point) = cache_point {
                                content.push(BedrockAssistantBlock::CachePoint(cache_point));
                            }
                        }
                        ContentPart::Reasoning {
                            text,
                            encrypted,
                            provider_metadata,
                            ..
                        } => {
                            content.push(BedrockAssistantBlock::Reasoning(BedrockReasoningBlock {
                                reasoning_content: BedrockReasoningContent {
                                    reasoning_text: BedrockReasoningText {
                                        text: text.clone(),
                                        signature: reasoning_signature(
                                            encrypted,
                                            provider_metadata,
                                        ),
                                    },
                                },
                            }));
                        }
                        ContentPart::ToolCall {
                            id, name, input, ..
                        } => {
                            content.push(BedrockAssistantBlock::ToolUse(BedrockToolUseBlock {
                                tool_use: BedrockToolUse {
                                    tool_use_id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                },
                            }));
                        }
                        _ => {
                            return Err(shared::unsupported_content(
                                ROUTE,
                                &MessageRole::Assistant,
                                &["text", "reasoning", "tool-call"],
                            ));
                        }
                    }
                }
                messages.push(BedrockMessage::Assistant {
                    role: BedrockRole::Assistant,
                    content,
                });
            }
            MessageRole::Tool => {
                let mut content = Vec::new();
                for part in &message.content {
                    if !shared::supports_content(part, &["tool-result"]) {
                        return Err(shared::unsupported_content(
                            ROUTE,
                            &MessageRole::Tool,
                            &["tool-result"],
                        ));
                    }
                    content.push(lower_tool_result(part)?);
                    let cache = match part {
                        ContentPart::ToolResult { cache, .. } => cache.as_ref(),
                        _ => None,
                    };
                    if let Some(cache_point) = bedrock_cache::block(breakpoints, cache) {
                        content.push(BedrockUserBlock::CachePoint(cache_point));
                    }
                }
                messages.push(BedrockMessage::User {
                    role: BedrockRole::User,
                    content,
                });
            }
        }
    }

    Ok(messages)
}

/// System prompts share the cache-point convention: emit the text block,
/// then optionally a positional `cachePoint` marker.
fn lower_system(breakpoints: &mut Breakpoints, system: &[SystemPart]) -> Vec<BedrockSystemBlock> {
    let mut blocks = Vec::new();
    for part in system {
        let (text, cache_point) = text_with_cache(breakpoints, &part.text, part.cache.as_ref());
        blocks.push(BedrockSystemBlock::Text(BedrockTextBlock { text }));
        if let Some(cache_point) = cache_point {
            blocks.push(BedrockSystemBlock::CachePoint(cache_point));
        }
    }
    blocks
}

fn inference_config(generation: Option<&GenerationOptions>) -> Option<BedrockInferenceConfig> {
    let generation = generation?;
    let stop_defined = generation
        .stop
        .as_ref()
        .is_some_and(|stop| !stop.is_empty());
    if generation.max_tokens.is_none()
        && generation.temperature.is_none()
        && generation.top_p.is_none()
        && !stop_defined
    {
        return None;
    }
    Some(BedrockInferenceConfig {
        max_tokens: generation.max_tokens,
        temperature: generation.temperature,
        top_p: generation.top_p,
        stop_sequences: generation.stop.clone(),
    })
}

fn from_request(request: &LlmRequest) -> Result<BedrockConverseBody, LlmError> {
    let tool_choice = match &request.tool_choice {
        Some(choice) => lower_tool_choice(choice)?,
        None => None,
    };
    let generation = request.generation.as_ref();
    // Bedrock-Claude shares Anthropic's 4-breakpoint cap. Spend the budget in
    // tools → system → messages order to favour the highest-impact prefixes.
    // TS logs a warning when the budget overflows; this crate has no logging
    // facility, and the `dropped` counter is observable on the breakpoints
    // value for callers that need it.
    let mut breakpoints = bedrock_cache::breakpoints();
    let tool_config = if !request.tools.is_empty()
        && request.tool_choice.as_ref().map(|choice| choice.r#type) != Some(ToolChoiceType::None)
    {
        Some(BedrockToolConfig {
            tools: lower_tools(
                request
                    .model
                    .compatibility
                    .as_ref()
                    .and_then(|compatibility| compatibility.tool_schema),
                &mut breakpoints,
                &request.tools,
            ),
            tool_choice,
        })
    } else {
        None
    };
    let system =
        (!request.system.is_empty()).then(|| lower_system(&mut breakpoints, &request.system));
    let messages = lower_messages(request, &mut breakpoints)?;
    Ok(BedrockConverseBody {
        model_id: request.model.id.clone(),
        messages,
        system,
        inference_config: inference_config(generation),
        tool_config,
        // Converse's base inferenceConfig has no topK; Anthropic/Nova accept
        // it as a model-specific field, so it goes through
        // additionalModelRequestFields.
        additional_model_request_fields: generation.and_then(|options| {
            options.top_k.map(|top_k| {
                let mut fields = JsonMap::new();
                fields.insert("top_k".to_string(), serde_json::json!(top_k));
                fields
            })
        }),
    })
}

// =========================================================================
// Stream event schema (TS `BedrockUsage` / `BedrockEvent`)
// =========================================================================

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BedrockUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_read_input_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_write_input_tokens: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireToolUseStart {
    tool_use_id: String,
    name: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireStart {
    #[serde(default)]
    tool_use: Option<WireToolUseStart>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireReasoningDelta {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireToolUseDelta {
    input: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireDelta {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    tool_use: Option<WireToolUseDelta>,
    #[serde(default)]
    reasoning_content: Option<WireReasoningDelta>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireContentBlockStart {
    content_block_index: f64,
    #[serde(default)]
    start: Option<WireStart>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireContentBlockDelta {
    content_block_index: f64,
    #[serde(default)]
    delta: Option<WireDelta>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireContentBlockStop {
    content_block_index: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMessageStop {
    stop_reason: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMetadata {
    #[serde(default)]
    usage: Option<BedrockUsage>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireException {
    message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct BedrockEvent {
    #[serde(default)]
    message_start: Option<WireMessageStart>,
    #[serde(default)]
    content_block_start: Option<WireContentBlockStart>,
    #[serde(default)]
    content_block_delta: Option<WireContentBlockDelta>,
    #[serde(default)]
    content_block_stop: Option<WireContentBlockStop>,
    #[serde(default)]
    message_stop: Option<WireMessageStop>,
    #[serde(default)]
    metadata: Option<WireMetadata>,
    #[serde(default)]
    internal_server_exception: Option<WireException>,
    #[serde(default)]
    model_stream_error_exception: Option<WireException>,
    #[serde(default)]
    validation_exception: Option<WireException>,
    #[serde(default)]
    throttling_exception: Option<WireException>,
    #[serde(default)]
    service_unavailable_exception: Option<WireException>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMessageStart {
    role: String,
}

// =========================================================================
// Parser state (TS `ParserState`)
// =========================================================================

/// Bedrock splits the finish into `messageStop` (carries `stopReason`) and
/// `metadata` (carries usage). Both halves are held here so `on_halt` can
/// emit exactly one finish after both chunks have had a chance to arrive.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingFinish {
    pub reason: FinishReason,
    pub usage: Option<Usage>,
}

/// Accumulator threaded through the Bedrock streaming state machine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    tools: tool_stream::State<u32>,
    pending_finish: Option<PendingFinish>,
    has_tool_calls: bool,
    lifecycle: lifecycle::State,
    reasoning_signatures: BTreeMap<u32, String>,
}

// =========================================================================
// Stream parsing (TS `mapFinishReason` / `mapUsage` / `step` / `onHalt`)
// =========================================================================

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "content_filtered" | "guardrail_intervened" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

fn bedrock_metadata(value: Value) -> ProviderMetadata {
    let mut metadata = ProviderMetadata::new();
    if let Some(record) = value.as_object() {
        metadata.insert("bedrock".to_string(), record.clone());
    }
    metadata
}

/// AWS Bedrock Converse reports `inputTokens` (inclusive total) with
/// `cacheReadInputTokens` and `cacheWriteInputTokens` as subsets. Pass the
/// total through and derive the non-cached breakdown. Bedrock does not break
/// reasoning out of `outputTokens` for any current model.
fn map_usage(usage: &BedrockUsage) -> Option<Usage> {
    let cache_total = usage.cache_read_input_tokens.unwrap_or(0.0)
        + usage.cache_write_input_tokens.unwrap_or(0.0);
    Some(Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        non_cached_input_tokens: shared::subtract_tokens(usage.input_tokens, Some(cache_total)),
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        reasoning_tokens: None,
        total_tokens: shared::total_tokens(
            usage.input_tokens,
            usage.output_tokens,
            usage.total_tokens,
        ),
        provider_metadata: Some(bedrock_metadata(
            serde_json::to_value(usage).unwrap_or(Value::Null),
        )),
    })
}

fn step(mut state: State, event: &BedrockEvent) -> Result<(State, Vec<LlmEvent>), LlmError> {
    if let (Some(block), Some(tool_use)) = (
        event.content_block_start.as_ref(),
        event
            .content_block_start
            .as_ref()
            .and_then(|block| block.start.as_ref())
            .and_then(|start| start.tool_use.as_ref()),
    ) {
        let index = block.content_block_index as u32;
        let mut events = Vec::new();
        state.lifecycle = lifecycle::step_start(state.lifecycle, &mut events);
        events.push(LlmEvent::ToolInputStart {
            id: tool_use.tool_use_id.clone(),
            name: tool_use.name.clone(),
            provider_metadata: None,
        });
        state.tools = tool_stream::start(
            state.tools,
            index,
            PendingTool {
                id: tool_use.tool_use_id.clone(),
                name: tool_use.name.clone(),
                input: String::new(),
                provider_executed: None,
                provider_metadata: None,
            },
        );
        return Ok((state, events));
    }

    if let Some(delta) = event
        .content_block_delta
        .as_ref()
        .and_then(|block| block.delta.as_ref())
    {
        if let Some(text) = delta.text.as_deref().filter(|text| !text.is_empty()) {
            let index = event
                .content_block_delta
                .as_ref()
                .unwrap()
                .content_block_index;
            let mut events = Vec::new();
            state.lifecycle = lifecycle::text_delta(
                state.lifecycle,
                &mut events,
                &format!("text-{}", index as u32),
                text,
            );
            return Ok((state, events));
        }

        if let Some(reasoning) = delta.reasoning_content.as_ref() {
            let index = event
                .content_block_delta
                .as_ref()
                .unwrap()
                .content_block_index as u32;
            let mut events = Vec::new();
            if let Some(text) = reasoning.text.as_deref().filter(|text| !text.is_empty()) {
                state.lifecycle = lifecycle::reasoning_delta(
                    state.lifecycle,
                    &mut events,
                    &format!("reasoning-{index}"),
                    text,
                    None,
                );
            }
            // `reasoning.signature ? …` (bedrock-converse.ts:520) — the
            // truthy check drops empty-string signatures.
            if let Some(signature) = reasoning
                .signature
                .clone()
                .filter(|signature| !signature.is_empty())
            {
                state.reasoning_signatures.insert(index, signature);
            }
            return Ok((state, events));
        }

        if let Some(tool_use) = delta.tool_use.as_ref() {
            let index = event
                .content_block_delta
                .as_ref()
                .unwrap()
                .content_block_index;
            let result = tool_stream::append_existing(
                ADAPTER,
                state.tools,
                index as u32,
                tool_use.input.clone(),
                "Bedrock Converse tool delta is missing its tool call",
            )?;
            let mut events = Vec::new();
            let lifecycle_state = if !result.events.is_empty() {
                lifecycle::step_start(state.lifecycle, &mut events)
            } else {
                state.lifecycle
            };
            events.extend(result.events);
            state.lifecycle = lifecycle_state;
            state.tools = result.tools;
            return Ok((state, events));
        }
    }

    if let Some(stop) = event.content_block_stop.as_ref() {
        let index = stop.content_block_index as u32;
        let result = tool_stream::finish(ADAPTER, state.tools, index)?;
        let mut events = Vec::new();
        let lifecycle_state = if !result.events.is_empty() {
            lifecycle::step_start(state.lifecycle, &mut events)
        } else {
            let lifecycle_state =
                lifecycle::text_end(state.lifecycle, &mut events, &format!("text-{index}"), None);
            let signature = state.reasoning_signatures.get(&index).cloned();
            lifecycle::reasoning_end(
                lifecycle_state,
                &mut events,
                &format!("reasoning-{index}"),
                signature.map(|signature| {
                    bedrock_metadata(serde_json::json!({ "signature": signature }))
                }),
            )
        };
        let has_tool_call = result
            .events
            .iter()
            .any(|event| matches!(event, LlmEvent::ToolCall { .. }));
        if has_tool_call {
            state.has_tool_calls = true;
        }
        events.extend(result.events);
        state.lifecycle = lifecycle_state;
        state.tools = result.tools;
        state.reasoning_signatures.remove(&index);
        return Ok((state, events));
    }

    if let Some(message_stop) = event.message_stop.as_ref() {
        state.pending_finish = Some(PendingFinish {
            reason: map_finish_reason(&message_stop.stop_reason),
            usage: state
                .pending_finish
                .as_ref()
                .and_then(|pending| pending.usage.clone()),
        });
        return Ok((state, Vec::new()));
    }

    if let Some(metadata) = event.metadata.as_ref() {
        let usage = metadata.usage.as_ref().and_then(map_usage);
        state.pending_finish = Some(PendingFinish {
            reason: state
                .pending_finish
                .as_ref()
                .map(|pending| pending.reason)
                .unwrap_or(FinishReason::Stop),
            usage,
        });
        return Ok((state, Vec::new()));
    }

    if event.internal_server_exception.is_some()
        || event.model_stream_error_exception.is_some()
        || event.service_unavailable_exception.is_some()
    {
        let message = event
            .internal_server_exception
            .as_ref()
            .or(event.model_stream_error_exception.as_ref())
            .or(event.service_unavailable_exception.as_ref())
            .map(|exception| exception.message.clone())
            .unwrap_or_else(|| "Bedrock Converse stream error".to_string());
        return Ok((
            state,
            vec![LlmEvent::ProviderError {
                message,
                classification: None,
                retryable: Some(true),
                provider_metadata: None,
            }],
        ));
    }

    if event.validation_exception.is_some() || event.throttling_exception.is_some() {
        let message = event
            .validation_exception
            .as_ref()
            .or(event.throttling_exception.as_ref())
            .map(|exception| exception.message.clone())
            .unwrap_or_else(|| "Bedrock Converse error".to_string());
        let classification =
            if event.validation_exception.is_some() && is_context_overflow(&message) {
                Some(ProviderFailureClassification::ContextOverflow)
            } else {
                None
            };
        return Ok((
            state,
            vec![LlmEvent::ProviderError {
                message,
                classification,
                retryable: Some(event.throttling_exception.is_some()),
                provider_metadata: None,
            }],
        ));
    }

    Ok((state, Vec::new()))
}

fn on_halt(state: State) -> Vec<LlmEvent> {
    let Some(pending) = state.pending_finish else {
        return Vec::new();
    };
    let reason = if pending.reason == FinishReason::Stop && state.has_tool_calls {
        FinishReason::ToolCalls
    } else {
        pending.reason
    };
    let mut events = Vec::new();
    lifecycle::finish(
        state.lifecycle,
        &mut events,
        FinishInput {
            reason,
            usage: pending.usage,
            provider_metadata: None,
        },
    );
    events
}

fn invalid_stream_event(frame: &Value) -> LlmError {
    shared::event_error(
        ADAPTER,
        format!("Invalid {ADAPTER} stream event"),
        Some(&frame.to_string()),
    )
}

// =========================================================================
// Protocol And Bedrock Route
// =========================================================================

/// The Bedrock Converse protocol — request body construction and the
/// streaming-event state machine over the AWS event stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct BedrockConverse;

impl Protocol for BedrockConverse {
    const ID: &'static str = ADAPTER;
    type State = State;

    fn lower_body(&self, request: &LlmRequest) -> Result<Value, LlmError> {
        serde_json::to_value(from_request(request)?).map_err(|error| {
            shared::invalid_request(format!(
                "Bedrock Converse request body serialization failed: {error}"
            ))
        })
    }

    fn decode_frame(&self, frame: &Value) -> Result<Option<Value>, LlmError> {
        serde_json::from_value::<BedrockEvent>(frame.clone())
            .map_err(|_| invalid_stream_event(frame))?;
        Ok(Some(frame.clone()))
    }

    fn initial(&self, _request: &LlmRequest) -> State {
        State::default()
    }

    fn step(&self, state: State, event: &Value) -> Result<(State, Vec<LlmEvent>), LlmError> {
        let event = serde_json::from_value::<BedrockEvent>(event.clone())
            .map_err(|_| invalid_stream_event(event))?;
        step(state, &event)
    }

    fn on_halt(&self, state: State) -> Vec<LlmEvent> {
        on_halt(state)
    }
}

/// Bedrock route auth defaults to SigV4 (TS `BedrockAuth.auth` =
/// `sigV4(undefined)`): credentials come from route configuration, so at the
/// protocol level signing fails with the missing-credentials request error.
pub fn auth() -> Auth {
    Auth::custom(Arc::new(|input: &crate::route::auth::AuthInput<'_>| {
        let headers: BTreeMap<String, String> = input.headers.iter().cloned().collect();
        bedrock_auth::auth(bedrock_auth::AuthInput {
            url: input.url,
            body: input.body,
            headers: &headers,
        })
        .map(|signed| signed.into_iter().collect())
    }))
}

/// JS `encodeURIComponent`: Bedrock's URL embeds the validated `modelId` in
/// the path, so the URL matches the body that gets signed.
fn encode_uri_component(value: &str) -> String {
    let mut out = String::new();
    for &byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Bedrock's URL embeds the validated `modelId` in the path. The route has
/// no default base URL — provider facades configure the regional
/// `bedrock-runtime.<region>.amazonaws.com` host before model selection.
pub fn endpoint() -> Endpoint {
    Endpoint {
        base_url: None,
        path: EndpointPart::Function(|input| {
            format!(
                "/model/{}/converse-stream",
                encode_uri_component(
                    input
                        .body
                        .get("modelId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                )
            )
        }),
        query: None,
    }
}

/// The canonical Bedrock route handle (TS `BedrockConverse.route`).
pub fn route_handle() -> RouteHandle {
    RouteHandle {
        id: ADAPTER.to_string(),
        protocol_id: ADAPTER.to_string(),
        endpoint: endpoint(),
        auth: auth(),
        framing: Framing::AwsEventStream,
        defaults: RouteDefaults::default(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]

    use super::*;
    use crate::schema::messages::{Message, ModelRef, ToolResultInput};
    use crate::schema::options::{GenerationOptions, SystemPart, SystemPartType};
    use serde_json::json;
    use std::sync::Arc;

    fn model_ref() -> ModelRef {
        ModelRef::new(
            "us.amazon.nova-micro-v1:0",
            "amazon-bedrock",
            Arc::new(RouteHandle::empty()),
        )
    }

    fn system_part(text: &str) -> SystemPart {
        SystemPart {
            r#type: SystemPartType::Text,
            text: text.to_string(),
            cache: None,
            metadata: None,
        }
    }

    fn request() -> LlmRequest {
        let mut value = LlmRequest::new(model_ref());
        value.system = vec![system_part("Reply with the single word 'Hello'.")];
        value.messages = vec![Message::user("Say hello.")];
        value.generation = Some(GenerationOptions {
            max_tokens: Some(16.0),
            temperature: Some(0.0),
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            seed: None,
            stop: None,
        });
        value
    }

    fn lower(request: &LlmRequest) -> Value {
        BedrockConverse.lower_body(request).unwrap()
    }

    /// Numeric-tolerant JSON equality (spec §2.3): Rust lowering emits
    /// `"max_tokens": 16.0` where the recording has `16`.
    fn assert_json_equal(expected: &Value, actual: &Value) {
        match (expected, actual) {
            (Value::Object(expected), Value::Object(actual)) => {
                assert_eq!(expected.len(), actual.len(), "{expected:?} vs {actual:?}");
                for (key, value) in expected {
                    let Some(other) = actual.get(key) else {
                        panic!("missing key {key}: {actual:?}");
                    };
                    assert_json_equal(value, other);
                }
            }
            (Value::Array(expected), Value::Array(actual)) => {
                assert_eq!(expected.len(), actual.len(), "{expected:?} vs {actual:?}");
                for (value, other) in expected.iter().zip(actual) {
                    assert_json_equal(value, other);
                }
            }
            (Value::Number(expected), Value::Number(actual)) => {
                assert_eq!(expected.as_f64(), actual.as_f64(), "{expected} vs {actual}");
            }
            _ => assert_eq!(expected, actual),
        }
    }

    fn stream(frames: Vec<Value>) -> Vec<LlmEvent> {
        let request = request();
        let mut state = BedrockConverse.initial(&request);
        let mut events = Vec::new();
        for frame in &frames {
            let decoded = BedrockConverse.decode_frame(frame).unwrap().unwrap();
            let (next, mut emitted) = BedrockConverse.step(state, &decoded).unwrap();
            state = next;
            events.append(&mut emitted);
        }
        events.extend(BedrockConverse.on_halt(state));
        events
    }

    fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get current weather for a city.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"],
                "additionalProperties": false
            })
            .as_object()
            .unwrap()
            .clone(),
            output_schema: None,
            cache: None,
            metadata: None,
            native: None,
        }
    }

    #[test]
    fn lowers_text_requests_like_the_streams_text_recording() {
        assert_json_equal(
            &json!({
                "modelId": "us.amazon.nova-micro-v1:0",
                "messages": [{"role": "user", "content": [{"text": "Say hello."}]}],
                "system": [{"text": "Reply with the single word 'Hello'."}],
                "inferenceConfig": {"maxTokens": 16, "temperature": 0},
            }),
            &lower(&request()),
        );
    }

    #[test]
    fn lowers_tool_definitions_and_choice_like_the_streams_tool_call_recording() {
        let mut value = request();
        value.messages = vec![Message::user("Call get_weather with city exactly Paris.")];
        value.system = vec![system_part("Call tools exactly as requested.")];
        value.tools = vec![tool_definition()];
        value.tool_choice = Some(ToolChoice::named("get_weather"));
        assert_json_equal(
            &json!({
                "modelId": "us.amazon.nova-micro-v1:0",
                "messages": [
                    {"role": "user", "content": [{"text": "Call get_weather with city exactly Paris."}]}
                ],
                "system": [{"text": "Call tools exactly as requested."}],
                "inferenceConfig": {"maxTokens": 16, "temperature": 0},
                "toolConfig": {
                    "tools": [
                        {
                            "toolSpec": {
                                "name": "get_weather",
                                "description": "Get current weather for a city.",
                                "inputSchema": {
                                    "json": {
                                        "type": "object",
                                        "properties": {"city": {"type": "string"}},
                                        "required": ["city"],
                                        "additionalProperties": false
                                    }
                                }
                            }
                        }
                    ],
                    "toolChoice": {"tool": {"name": "get_weather"}}
                }
            }),
            &lower(&value),
        );
    }

    #[test]
    fn lowers_tool_history_like_the_tool_loop_recording() {
        let mut value = request();
        value.messages = vec![
            Message::user("What is the weather in Paris?"),
            Message::assistant(vec![
                ContentPart::text(
                    "<thinking> To determine the weather in Paris, I will use the get_weather tool and provide the city as \"Paris\". </thinking>\n",
                ),
                ContentPart::tool_call(
                    "tooluse_a8nlf2bqGLcZvaSoBpQ1sH",
                    "get_weather",
                    json!({"city": "Paris"}),
                ),
            ]),
            Message::tool(ContentPart::tool_result(ToolResultInput {
                id: "tooluse_a8nlf2bqGLcZvaSoBpQ1sH".to_string(),
                name: "get_weather".to_string(),
                result: json!({"temperature": 22, "condition": "sunny"}),
                ..ToolResultInput::default()
            })),
        ];
        value.tools = vec![tool_definition()];
        assert_json_equal(
            &json!({
                "modelId": "us.amazon.nova-micro-v1:0",
                "messages": [
                    {"role": "user", "content": [{"text": "What is the weather in Paris?"}]},
                    {
                        "role": "assistant",
                        "content": [
                            {"text": "<thinking> To determine the weather in Paris, I will use the get_weather tool and provide the city as \"Paris\". </thinking>\n"},
                            {
                                "toolUse": {
                                    "toolUseId": "tooluse_a8nlf2bqGLcZvaSoBpQ1sH",
                                    "name": "get_weather",
                                    "input": {"city": "Paris"}
                                }
                            }
                        ]
                    },
                    {
                        "role": "user",
                        "content": [
                            {
                                "toolResult": {
                                    "toolUseId": "tooluse_a8nlf2bqGLcZvaSoBpQ1sH",
                                    "content": [{"json": {"temperature": 22, "condition": "sunny"}}],
                                    "status": "success"
                                }
                            }
                        ]
                    }
                ],
                "system": [{"text": "Reply with the single word 'Hello'."}],
                "inferenceConfig": {"maxTokens": 16, "temperature": 0},
                "toolConfig": {
                    "tools": [
                        {
                            "toolSpec": {
                                "name": "get_weather",
                                "description": "Get current weather for a city.",
                                "inputSchema": {
                                    "json": {
                                        "type": "object",
                                        "properties": {"city": {"type": "string"}},
                                        "required": ["city"],
                                        "additionalProperties": false
                                    }
                                }
                            }
                        }
                    ]
                }
            }),
            &lower(&value),
        );
    }

    #[test]
    fn omits_tools_when_tool_choice_is_none() {
        let mut value = request();
        value.tools = vec![tool_definition()];
        value.tool_choice = Some(ToolChoice {
            r#type: ToolChoiceType::None,
            name: None,
        });
        let body = lower(&value);
        assert!(body.get("toolConfig").is_none());
    }

    #[test]
    fn lowers_cache_hints_as_positional_cache_point_blocks() {
        let mut value = request();
        let mut hinted = system_part("You are concise.");
        hinted.cache = Some(CacheHint {
            r#type: crate::schema::options::CacheHintType::Ephemeral,
            ttl_seconds: Some(3600.0),
        });
        value.system = vec![system_part("First."), hinted];
        let body = lower(&value);
        assert_json_equal(
            &json!([
                {"text": "First."},
                {"text": "You are concise."},
                {"cachePoint": {"type": "default", "ttl": "1h"}}
            ]),
            &body["system"],
        );
    }

    #[test]
    fn moves_top_k_into_additional_model_request_fields() {
        let mut value = request();
        value.generation = Some(GenerationOptions {
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: Some(5.0),
            frequency_penalty: None,
            presence_penalty: None,
            seed: None,
            stop: None,
        });
        let body = lower(&value);
        assert!(
            body.get("inferenceConfig").is_none(),
            "top_k alone must not emit inferenceConfig"
        );
        assert_eq!(
            body["additionalModelRequestFields"]["top_k"].as_f64(),
            Some(5.0)
        );
    }

    #[test]
    fn encode_uri_component_matches_js() {
        assert_eq!(
            encode_uri_component("us.amazon.nova-micro-v1:0"),
            "us.amazon.nova-micro-v1%3A0"
        );
        assert_eq!(
            encode_uri_component("a.b-c_d(e)~f*g!'h"),
            "a.b-c_d(e)~f*g!'h"
        );
    }

    #[test]
    fn endpoint_path_embeds_the_body_model_id() {
        let request = request();
        let body = lower(&request);
        let mut with_host = endpoint();
        with_host.base_url = Some("https://bedrock-runtime.us-east-1.amazonaws.com".to_string());
        let url = crate::route::endpoint::render(
            &with_host,
            &crate::route::endpoint::EndpointInput {
                request: &request,
                body: &body,
            },
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/us.amazon.nova-micro-v1%3A0/converse-stream"
        );
    }

    #[test]
    fn parses_text_streams_and_emits_one_finish_in_on_halt() {
        // The frames of fixtures/llm-recordings/bedrock-converse/streams-text
        // after the event-stream decoder stripped the `p` padding.
        let events = stream(vec![
            json!({"messageStart": {"role": "assistant"}}),
            json!({"contentBlockDelta": {"contentBlockIndex": 0, "delta": {"text": "Hello"}}}),
            // Empty text deltas are dropped (TS truthiness check).
            json!({"contentBlockDelta": {"contentBlockIndex": 0, "delta": {"text": ""}}}),
            json!({"contentBlockStop": {"contentBlockIndex": 0}}),
            json!({"messageStop": {"stopReason": "end_turn"}}),
            json!({
                "metadata": {
                    "metrics": {"latencyMs": 306},
                    "usage": {"inputTokens": 12, "outputTokens": 2, "totalTokens": 14}
                }
            }),
        ]);

        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart { index: 0.0 },
                LlmEvent::TextStart {
                    id: "text-0".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::TextDelta {
                    id: "text-0".to_string(),
                    text: "Hello".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::TextEnd {
                    id: "text-0".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::StepFinish {
                    index: 0.0,
                    reason: FinishReason::Stop,
                    usage: Some(expected_recorded_usage(
                        Some(12.0),
                        Some(2.0),
                        Some(12.0),
                        Some(14.0),
                    )),
                    provider_metadata: None,
                },
                LlmEvent::Finish {
                    reason: FinishReason::Stop,
                    usage: Some(expected_recorded_usage(
                        Some(12.0),
                        Some(2.0),
                        Some(12.0),
                        Some(14.0),
                    )),
                    provider_metadata: None,
                },
            ],
        );
    }

    fn expected_usage(
        input: Option<f64>,
        output: Option<f64>,
        non_cached: Option<f64>,
        total: Option<f64>,
    ) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            non_cached_input_tokens: non_cached,
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            reasoning_tokens: None,
            total_tokens: total,
            provider_metadata: None,
        }
    }

    /// [`expected_usage`] plus the raw `{bedrock: usage}` provider metadata.
    fn expected_recorded_usage(
        input: Option<f64>,
        output: Option<f64>,
        non_cached: Option<f64>,
        total: Option<f64>,
    ) -> Usage {
        let usage = expected_usage(input, output, non_cached, total);
        let raw = json!({
            "inputTokens": input,
            "outputTokens": output,
            "totalTokens": total,
        });
        let mut provider_metadata = ProviderMetadata::new();
        provider_metadata.insert(
            "bedrock".to_string(),
            raw.as_object().cloned().unwrap_or_default(),
        );
        Usage {
            provider_metadata: Some(provider_metadata),
            ..usage
        }
    }

    #[test]
    fn parses_tool_call_streams_and_maps_the_finish_reason() {
        // The frames of streams-a-tool-call after event-stream decoding.
        let events = stream(vec![
            json!({"messageStart": {"role": "assistant"}}),
            json!({"contentBlockStart": {"contentBlockIndex": 0, "start": {"toolUse": {"name": "get_weather", "toolUseId": "tooluse_6a1pPvnc99GLKO3KGkUA2N"}}}}),
            json!({"contentBlockDelta": {"contentBlockIndex": 0, "delta": {"toolUse": {"input": "{\"city\":\"Paris\"}"}}}}),
            json!({"contentBlockStop": {"contentBlockIndex": 0}}),
            json!({"messageStop": {"stopReason": "tool_use"}}),
            json!({
                "metadata": {
                    "metrics": {"latencyMs": 355},
                    "usage": {"inputTokens": 419, "outputTokens": 16, "totalTokens": 435}
                }
            }),
        ]);

        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart { index: 0.0 },
                LlmEvent::ToolInputStart {
                    id: "tooluse_6a1pPvnc99GLKO3KGkUA2N".to_string(),
                    name: "get_weather".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::ToolInputDelta {
                    id: "tooluse_6a1pPvnc99GLKO3KGkUA2N".to_string(),
                    name: "get_weather".to_string(),
                    text: "{\"city\":\"Paris\"}".to_string(),
                },
                LlmEvent::ToolInputEnd {
                    id: "tooluse_6a1pPvnc99GLKO3KGkUA2N".to_string(),
                    name: "get_weather".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::ToolCall {
                    id: "tooluse_6a1pPvnc99GLKO3KGkUA2N".to_string(),
                    name: "get_weather".to_string(),
                    input: json!({"city": "Paris"}),
                    provider_executed: None,
                    provider_metadata: None,
                },
                LlmEvent::StepFinish {
                    index: 0.0,
                    reason: FinishReason::ToolCalls,
                    usage: Some(expected_recorded_usage(
                        Some(419.0),
                        Some(16.0),
                        Some(419.0),
                        Some(435.0),
                    )),
                    provider_metadata: None,
                },
                LlmEvent::Finish {
                    reason: FinishReason::ToolCalls,
                    usage: Some(expected_recorded_usage(
                        Some(419.0),
                        Some(16.0),
                        Some(419.0),
                        Some(435.0),
                    )),
                    provider_metadata: None,
                },
            ],
        );
    }

    #[test]
    fn closes_reasoning_blocks_with_their_signatures() {
        let events = stream(vec![
            json!({"contentBlockDelta": {"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "thinking", "signature": "sig"}}}}),
            json!({"contentBlockStop": {"contentBlockIndex": 0}}),
            json!({"messageStop": {"stopReason": "end_turn"}}),
        ]);
        let reasoning_end = events
            .iter()
            .find(|event| matches!(event, LlmEvent::ReasoningEnd { .. }))
            .unwrap();
        assert_eq!(
            reasoning_end,
            &LlmEvent::ReasoningEnd {
                id: "reasoning-0".to_string(),
                provider_metadata: Some(bedrock_metadata(json!({"signature": "sig"}))),
            },
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LlmEvent::TextStart { .. })),
            "a reasoning-only block must not open a text block"
        );
    }

    #[test]
    fn upgrades_stop_reasons_to_tool_calls_in_on_halt() {
        let events = stream(vec![
            json!({"contentBlockStart": {"contentBlockIndex": 0, "start": {"toolUse": {"name": "get_weather", "toolUseId": "tooluse_1"}}}}),
            json!({"contentBlockDelta": {"contentBlockIndex": 0, "delta": {"toolUse": {"input": "{}"}}}}),
            json!({"contentBlockStop": {"contentBlockIndex": 0}}),
            json!({"messageStop": {"stopReason": "end_turn"}}),
        ]);
        let finish = events
            .iter()
            .find(|event| matches!(event, LlmEvent::Finish { .. }))
            .unwrap();
        match finish {
            LlmEvent::Finish { reason, .. } => assert_eq!(*reason, FinishReason::ToolCalls),
            event => panic!("expected a finish, got {event:?}"),
        }
    }

    #[test]
    fn maps_exception_frames_to_provider_errors() {
        let validation = stream(vec![json!({
            "validationException": {"message": "prompt is too long: 12000 tokens"}
        })]);
        match &validation[0] {
            LlmEvent::ProviderError {
                message,
                classification,
                retryable,
                ..
            } => {
                assert_eq!(message, "prompt is too long: 12000 tokens");
                assert_eq!(
                    *classification,
                    Some(ProviderFailureClassification::ContextOverflow)
                );
                assert_eq!(*retryable, Some(false));
            }
            event => panic!("expected a provider error, got {event:?}"),
        }

        let throttling = stream(vec![
            json!({"throttlingException": {"message": "slow down"}}),
        ]);
        match &throttling[0] {
            LlmEvent::ProviderError {
                classification,
                retryable,
                ..
            } => {
                assert!(classification.is_none());
                assert_eq!(*retryable, Some(true));
            }
            event => panic!("expected a provider error, got {event:?}"),
        }

        let internal = stream(vec![
            json!({"internalServerException": {"message": "boom"}}),
        ]);
        match &internal[0] {
            LlmEvent::ProviderError { retryable, .. } => assert_eq!(*retryable, Some(true)),
            event => panic!("expected a provider error, got {event:?}"),
        }
    }
}
