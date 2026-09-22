//! The Anthropic Messages protocol (TS `protocols/anthropic-messages.ts`).
//!
//! Request lowering maps the common [`LlmRequest`] onto the Anthropic
//! Messages wire format: a `system` text-block array, `user`/`assistant`
//! message arrays of typed content blocks, and Anthropic-native tool
//! definitions. Mid-conversation system messages use the wrapped-user
//! `<system-update>` fallback everywhere except `claude-opus-4-8` (STOP S7).
//!
//! Parser notes:
//!
//! - Anthropic accepts at most 4 explicit `cache_control` breakpoints per
//!   request, so the lowering counts emitted markers and silently drops any
//!   that exceed it.
//! - Anthropic reports the *non-overlapping* usage breakdown natively (its
//!   `input_tokens` is the non-cached count); the mapper sums the breakdown
//!   to derive the inclusive `inputTokens`.

#![allow(clippy::result_large_err)]

use alforria_schema::llm::ToolContent;

use crate::protocols::shared::{
    self, match_tool_choice, sum_tokens, supports_content, total_tokens, unsupported_content,
    validate_media, validate_tool_file, wrapped_system_update, MatchedToolChoice, IMAGE_MIMES,
};
use crate::protocols::utils::cache::{ttl_bucket, Breakpoints};
use crate::protocols::utils::lifecycle::{self, FinishInput};
use crate::protocols::utils::tool_schema::ToolSchemaProjection;
use crate::protocols::utils::tool_stream;
use crate::provider_error::is_context_overflow;
use crate::route::auth::Auth;
use crate::route::client::{RouteDefaults, RouteHandle};
use crate::route::endpoint::Endpoint;
use crate::route::framing::Framing;
use crate::route::protocol::Protocol;
use crate::schema::errors::{LlmError, ProviderFailureClassification};
use crate::schema::events::{LlmEvent, Usage};
use crate::schema::ids::{FinishReason, MessageRole, ProviderMetadata};
use crate::schema::messages::{
    ContentPart, LlmRequest, Message, ToolChoice, ToolChoiceType, ToolDefinition, ToolResultValue,
};
use crate::schema::options::CacheHint;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Route id and protocol id (TS `ADAPTER`).
pub const ADAPTER: &str = "anthropic-messages";
/// Human-facing route name used in error messages (TS `"Anthropic Messages"`).
pub const NAME: &str = "Anthropic Messages";
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
pub const PATH: &str = "/messages";

/// Anthropic accepts at most 4 explicit `cache_control` breakpoints per
/// request, across `tools`, `system`, and `messages`. Beyond the cap the API
/// returns a 400 — so the lowering layer counts emitted markers and silently
/// drops any that exceed it.
const ANTHROPIC_BREAKPOINT_CAP: i64 = 4;

const MISSING_TOOL_MESSAGE: &str =
    "Anthropic Messages tool argument delta is missing its tool call";

// =============================================================================
// Request Body Schema
// =============================================================================
// The body schema is the provider-native JSON body. `from_request` below
// builds this shape from the common `LlmRequest`; serializing the typed
// structs is the validation (spec §2.6).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum EphemeralTag {
    #[serde(rename = "ephemeral")]
    Ephemeral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TextTag {
    #[serde(rename = "text")]
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ImageTag {
    #[serde(rename = "image")]
    Image,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Base64Tag {
    #[serde(rename = "base64")]
    Base64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ThinkingTag {
    #[serde(rename = "thinking")]
    Thinking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ToolUseTag {
    #[serde(rename = "tool_use")]
    ToolUse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ServerToolUseTag {
    #[serde(rename = "server_tool_use")]
    ServerToolUse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ToolResultTag {
    #[serde(rename = "tool_result")]
    ToolResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum EnabledTag {
    #[serde(rename = "enabled")]
    Enabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ToolChoiceTag {
    #[serde(rename = "tool")]
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ToolChoiceMode {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "any")]
    Any,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnthropicCacheControl {
    pub r#type: EphemeralTag,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnthropicTextBlock {
    pub r#type: TextTag,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnthropicImageSource {
    pub r#type: Base64Tag,
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnthropicImageBlock {
    pub r#type: ImageTag,
    pub source: AnthropicImageSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnthropicThinkingBlock {
    pub r#type: ThinkingTag,
    pub thinking: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnthropicToolUseBlock {
    pub r#type: ToolUseTag,
    pub id: String,
    pub name: String,
    pub input: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnthropicServerToolUseBlock {
    pub r#type: ServerToolUseTag,
    pub id: String,
    pub name: String,
    pub input: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

/// Server tool result blocks (`web_search_tool_result`,
/// `code_execution_tool_result`, `web_fetch_tool_result`) round-trip the
/// structured payload returned by the provider as opaque JSON so the next
/// request can echo it back when continuing the conversation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnthropicServerToolResultBlock {
    pub r#type: String,
    pub tool_use_id: String,
    pub content: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

/// Anthropic accepts either a plain string or an ordered array of text/image
/// blocks inside `tool_result.content`. The array form is required when a
/// tool returns image bytes (screenshot, image search, etc.) so they can be
/// passed to the model as proper image inputs instead of being
/// JSON-stringified into the prompt.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AnthropicToolResultContent {
    Text(String),
    Blocks(Vec<AnthropicToolResultContentBlock>),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AnthropicToolResultContentBlock {
    Text(AnthropicTextBlock),
    Image(AnthropicImageBlock),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnthropicToolResultBlock {
    pub r#type: ToolResultTag,
    pub tool_use_id: String,
    pub content: AnthropicToolResultContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AnthropicUserBlock {
    Text(AnthropicTextBlock),
    Image(AnthropicImageBlock),
    ToolResult(AnthropicToolResultBlock),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AnthropicAssistantBlock {
    Text(AnthropicTextBlock),
    Thinking(AnthropicThinkingBlock),
    ToolUse(AnthropicToolUseBlock),
    ServerToolUse(AnthropicServerToolUseBlock),
    ServerToolResult(AnthropicServerToolResultBlock),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "role")]
pub enum AnthropicMessage {
    #[serde(rename = "user")]
    User { content: Vec<AnthropicUserBlock> },
    #[serde(rename = "assistant")]
    Assistant {
        content: Vec<AnthropicAssistantBlock>,
    },
    #[serde(rename = "system")]
    System { content: Vec<AnthropicTextBlock> },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AnthropicToolChoice {
    Mode { r#type: ToolChoiceMode },
    Tool { r#type: ToolChoiceTag, name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnthropicThinking {
    pub r#type: EnabledTag,
    pub budget_tokens: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnthropicMessagesBody {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<AnthropicTextBlock>>,
    pub messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<AnthropicToolChoice>,
    pub stream: bool,
    pub max_tokens: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<AnthropicThinking>,
}

// =============================================================================
// Streaming Event Schema
// =============================================================================
// The event schema is one decoded SSE `data:` payload.

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct MessageStart {
    usage: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct StreamBlock {
    r#type: Option<String>,
    id: Option<String>,
    name: Option<String>,
    text: Option<String>,
    thinking: Option<String>,
    // Present on the wire (thinking signatures / initial tool input) but not
    // consumed by the parser.
    #[allow(dead_code)]
    signature: Option<String>,
    #[allow(dead_code)]
    input: Option<Value>,
    tool_use_id: Option<String>,
    content: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct StreamDelta {
    r#type: Option<String>,
    text: Option<String>,
    thinking: Option<String>,
    partial_json: Option<String>,
    signature: Option<String>,
    stop_reason: Option<String>,
    stop_sequence: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct ErrorPayload {
    r#type: Option<String>,
    message: Option<String>,
}

/// One decoded SSE `data:` payload. `type` and `message` are both required
/// per Anthropic's spec, but OpenAI-compatible proxies and gateway
/// translations occasionally drop one or the other; they are optional here so
/// a partial payload still parses and the parser can fall back to whichever
/// field is populated.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Event {
    r#type: Option<String>,
    index: Option<f64>,
    message: Option<MessageStart>,
    content_block: Option<StreamBlock>,
    delta: Option<StreamDelta>,
    usage: Option<Value>,
    error: Option<ErrorPayload>,
}

/// Streaming parser state (TS `ParserState`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    pub tools: tool_stream::State<u32>,
    pub usage: Option<Usage>,
    pub lifecycle: lifecycle::State,
}

fn parse_event(frame: &Value) -> Result<Event, LlmError> {
    serde_json::from_value(frame.clone()).map_err(|error| {
        shared::event_error(
            ADAPTER,
            format!("Invalid Anthropic Messages event: {error}"),
            Some(&frame.to_string()),
        )
    })
}

// =============================================================================
// Request Lowering
// =============================================================================

/// TS `cacheControl` — map a cache hint onto a wire `cache_control` marker,
/// spending one unit of the 4-breakpoint budget; markers beyond the cap are
/// silently dropped (counted).
fn cache_control(
    breakpoints: &mut Breakpoints,
    cache: Option<&CacheHint>,
) -> Option<AnthropicCacheControl> {
    let cache = cache?;
    if breakpoints.remaining <= 0 {
        breakpoints.dropped += 1;
        return None;
    }
    breakpoints.remaining -= 1;
    Some(AnthropicCacheControl {
        r#type: EphemeralTag::Ephemeral,
        ttl: ttl_bucket(cache.ttl_seconds),
    })
}

/// The thinking signature round-trips either as the part's `encrypted` field
/// or inside its provider metadata (TS `signatureFromMetadata`).
fn signature_from_metadata(metadata: &Option<ProviderMetadata>) -> Option<String> {
    let anthropic = metadata.as_ref()?.get("anthropic")?;
    anthropic
        .get("signature")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn lower_tool(
    breakpoints: &mut Breakpoints,
    tool: &ToolDefinition,
    input_schema: serde_json::Map<String, Value>,
) -> AnthropicTool {
    AnthropicTool {
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema,
        cache_control: cache_control(breakpoints, tool.cache.as_ref()),
    }
}

fn lower_tool_choice(tool_choice: &ToolChoice) -> Result<Option<AnthropicToolChoice>, LlmError> {
    Ok(match match_tool_choice(tool_choice, NAME)? {
        MatchedToolChoice::Auto => Some(AnthropicToolChoice::Mode {
            r#type: ToolChoiceMode::Auto,
        }),
        MatchedToolChoice::None => None,
        MatchedToolChoice::Required => Some(AnthropicToolChoice::Mode {
            r#type: ToolChoiceMode::Any,
        }),
        MatchedToolChoice::Tool(name) => Some(AnthropicToolChoice::Tool {
            r#type: ToolChoiceTag::Tool,
            name,
        }),
    })
}

fn tool_call_fields(part: &ContentPart) -> (String, String, Value) {
    let ContentPart::ToolCall {
        id, name, input, ..
    } = part
    else {
        unreachable!("tool-call content part");
    };
    (id.clone(), name.clone(), input.clone())
}

fn lower_tool_call(part: &ContentPart) -> AnthropicToolUseBlock {
    let (id, name, input) = tool_call_fields(part);
    AnthropicToolUseBlock {
        r#type: ToolUseTag::ToolUse,
        id,
        name,
        input,
        cache_control: None,
    }
}

fn lower_server_tool_call(part: &ContentPart) -> AnthropicServerToolUseBlock {
    let (id, name, input) = tool_call_fields(part);
    AnthropicServerToolUseBlock {
        r#type: ServerToolUseTag::ServerToolUse,
        id,
        name,
        input,
        cache_control: None,
    }
}

/// Server tool result blocks are typed by name. Anthropic ships three today;
/// the block content is the structured payload returned by the provider,
/// which we round-trip as-is.
fn server_tool_result_type(name: &str) -> Option<&'static str> {
    match name {
        "web_search" => Some("web_search_tool_result"),
        "code_execution" => Some("code_execution_tool_result"),
        "web_fetch" => Some("web_fetch_tool_result"),
        _ => None,
    }
}

fn server_tool_result_name(block_type: &str) -> Option<&'static str> {
    match block_type {
        "web_search_tool_result" => Some("web_search"),
        "code_execution_tool_result" => Some("code_execution"),
        "web_fetch_tool_result" => Some("web_fetch"),
        _ => None,
    }
}

fn lower_server_tool_result(
    part: &ContentPart,
) -> Result<AnthropicServerToolResultBlock, LlmError> {
    let ContentPart::ToolResult {
        id, name, result, ..
    } = part
    else {
        unreachable!("tool-result content part");
    };
    let Some(wire_type) = server_tool_result_type(name) else {
        return Err(shared::invalid_request(format!(
            "Anthropic Messages does not know how to round-trip server tool result for {name}"
        )));
    };
    let content = match result {
        ToolResultValue::Json { value }
        | ToolResultValue::Text { value }
        | ToolResultValue::Error { value } => value.clone(),
        ToolResultValue::Content { value } => serde_json::to_value(value).unwrap_or(Value::Null),
    };
    Ok(AnthropicServerToolResultBlock {
        r#type: wire_type.to_string(),
        tool_use_id: id.clone(),
        content,
        cache_control: None,
    })
}

fn lower_image(part: &ContentPart) -> Result<AnthropicImageBlock, LlmError> {
    let media = validate_media(NAME, part, &IMAGE_MIMES)?;
    Ok(AnthropicImageBlock {
        r#type: ImageTag::Image,
        source: AnthropicImageSource {
            r#type: Base64Tag::Base64,
            media_type: media.mime,
            data: media.base64,
        },
        cache_control: None,
    })
}

fn lower_tool_result_content_item(
    item: &ToolContent,
) -> Result<AnthropicToolResultContentBlock, LlmError> {
    match item {
        ToolContent::Text { text } => {
            Ok(AnthropicToolResultContentBlock::Text(AnthropicTextBlock {
                r#type: TextTag::Text,
                text: text.clone(),
                cache_control: None,
            }))
        }
        file @ ToolContent::File { .. } => {
            let media = validate_tool_file(NAME, file, &IMAGE_MIMES)?;
            Ok(AnthropicToolResultContentBlock::Image(
                AnthropicImageBlock {
                    r#type: ImageTag::Image,
                    source: AnthropicImageSource {
                        r#type: Base64Tag::Base64,
                        media_type: media.mime,
                        data: media.base64,
                    },
                    cache_control: None,
                },
            ))
        }
    }
}

/// Tool results may carry structured text/images. Keep media as
/// provider-native content instead of JSON-stringifying base64 into a prompt
/// string; text / json / error results stay a string for backward
/// compatibility with existing cassettes and provider expectations.
fn lower_tool_result_content(part: &ContentPart) -> Result<AnthropicToolResultContent, LlmError> {
    let ContentPart::ToolResult { result, .. } = part else {
        unreachable!("tool-result content part");
    };
    match result {
        ToolResultValue::Content { value } => {
            let mut blocks = Vec::new();
            for item in value {
                blocks.push(lower_tool_result_content_item(item)?);
            }
            Ok(AnthropicToolResultContent::Blocks(blocks))
        }
        _ => Ok(AnthropicToolResultContent::Text(shared::tool_result_text(
            part,
        ))),
    }
}

/// Mid-conversation system messages are a native Claude API feature only for
/// Opus 4.8. Other Anthropic models intentionally use the same visible
/// wrapped-user fallback as non-Anthropic routes rather than sending a role
/// they reject. (STOP S7: port the model-id check as written.)
fn supports_native_system_updates(request: &LlmRequest) -> bool {
    request.model.id == "claude-opus-4-8"
}

fn ends_in_server_tool_use(message: &Message) -> bool {
    message.role == MessageRole::Assistant
        && matches!(
            message.content.last(),
            Some(ContentPart::ToolCall {
                provider_executed: Some(true),
                ..
            })
        )
}

fn can_use_native_system_update(messages: &[Message], index: usize) -> bool {
    let previous = if index > 0 {
        messages.get(index - 1)
    } else {
        None
    };
    let next = messages.get(index + 1);
    previous.is_some_and(|previous| {
        previous.role != MessageRole::System
            && (previous.role == MessageRole::User
                || previous.role == MessageRole::Tool
                || ends_in_server_tool_use(previous))
            && next.is_none_or(|next| next.role != MessageRole::System)
            && next.is_none_or(|next| next.role == MessageRole::Assistant)
    })
}

fn splits_local_tool_results(messages: &[Message], index: usize) -> bool {
    let mut pending = std::collections::BTreeSet::new();
    for message in &messages[..index] {
        for part in &message.content {
            match part {
                ContentPart::ToolCall { id, .. }
                    if message.role == MessageRole::Assistant
                        && !matches!(
                            part,
                            ContentPart::ToolCall {
                                provider_executed: Some(true),
                                ..
                            }
                        ) =>
                {
                    pending.insert(id.clone());
                }
                ContentPart::ToolResult { id, .. } if message.role == MessageRole::Tool => {
                    pending.remove(id);
                }
                _ => {}
            }
        }
    }
    !pending.is_empty()
}

fn lower_native_system_update(
    message: &Message,
    breakpoints: &mut Breakpoints,
) -> Result<AnthropicMessage, LlmError> {
    let content = shared::system_update_text(NAME, message)?;
    let mut blocks = Vec::new();
    for part in &content {
        let ContentPart::Text { text, cache, .. } = part else {
            continue;
        };
        blocks.push(AnthropicTextBlock {
            r#type: TextTag::Text,
            text: text.clone(),
            cache_control: cache_control(breakpoints, cache.as_ref()),
        });
    }
    Ok(AnthropicMessage::System { content: blocks })
}

fn lower_messages(
    request: &LlmRequest,
    breakpoints: &mut Breakpoints,
) -> Result<Vec<AnthropicMessage>, LlmError> {
    let mut messages: Vec<AnthropicMessage> = Vec::new();

    for (index, message) in request.messages.iter().enumerate() {
        match message.role {
            MessageRole::System => {
                if splits_local_tool_results(&request.messages, index) {
                    return Err(shared::invalid_request(
                        "Anthropic Messages system updates cannot split a local tool call from its tool result",
                    ));
                }
                if supports_native_system_updates(request)
                    && can_use_native_system_update(&request.messages, index)
                {
                    messages.push(lower_native_system_update(message, breakpoints)?);
                    continue;
                }
                let part = wrapped_system_update(NAME, message)?;
                let ContentPart::Text { text, cache, .. } = &part else {
                    return Err(shared::invalid_request(
                        "wrapped system update must be text content",
                    ));
                };
                let block = AnthropicTextBlock {
                    r#type: TextTag::Text,
                    text: text.clone(),
                    cache_control: cache_control(breakpoints, cache.as_ref()),
                };
                match messages.last_mut() {
                    Some(AnthropicMessage::User { content }) => {
                        content.push(AnthropicUserBlock::Text(block));
                    }
                    _ => messages.push(AnthropicMessage::User {
                        content: vec![AnthropicUserBlock::Text(block)],
                    }),
                }
                continue;
            }
            MessageRole::User => {
                let mut content = Vec::new();
                for part in &message.content {
                    match part {
                        ContentPart::Text { text, cache, .. } => {
                            content.push(AnthropicUserBlock::Text(AnthropicTextBlock {
                                r#type: TextTag::Text,
                                text: text.clone(),
                                cache_control: cache_control(breakpoints, cache.as_ref()),
                            }));
                        }
                        ContentPart::Media { .. } => {
                            content.push(AnthropicUserBlock::Image(lower_image(part)?));
                        }
                        _ => {
                            return Err(unsupported_content(
                                NAME,
                                &message.role,
                                &["text", "media"],
                            ));
                        }
                    }
                }
                messages.push(AnthropicMessage::User { content });
            }
            MessageRole::Assistant => {
                let mut content: Vec<AnthropicAssistantBlock> = Vec::new();
                for part in &message.content {
                    match part {
                        ContentPart::Text { text, cache, .. } => {
                            content.push(AnthropicAssistantBlock::Text(AnthropicTextBlock {
                                r#type: TextTag::Text,
                                text: text.clone(),
                                cache_control: cache_control(breakpoints, cache.as_ref()),
                            }));
                        }
                        ContentPart::Reasoning {
                            text,
                            encrypted,
                            provider_metadata,
                            ..
                        } => {
                            content.push(AnthropicAssistantBlock::Thinking(
                                AnthropicThinkingBlock {
                                    r#type: ThinkingTag::Thinking,
                                    thinking: text.clone(),
                                    signature: encrypted
                                        .clone()
                                        .or_else(|| signature_from_metadata(provider_metadata)),
                                    cache_control: None,
                                },
                            ));
                        }
                        ContentPart::ToolCall { .. } => {
                            if matches!(
                                part,
                                ContentPart::ToolCall {
                                    provider_executed: Some(true),
                                    ..
                                }
                            ) {
                                content.push(AnthropicAssistantBlock::ServerToolUse(
                                    lower_server_tool_call(part),
                                ));
                            } else {
                                content
                                    .push(AnthropicAssistantBlock::ToolUse(lower_tool_call(part)));
                            }
                        }
                        ContentPart::ToolResult { .. }
                            if matches!(
                                part,
                                ContentPart::ToolResult {
                                    provider_executed: Some(true),
                                    ..
                                }
                            ) =>
                        {
                            content.push(AnthropicAssistantBlock::ServerToolResult(
                                lower_server_tool_result(part)?,
                            ));
                        }
                        _ => {
                            return Err(shared::invalid_request(
                                "Anthropic Messages assistant messages only support text, reasoning, and tool-call content for now",
                            ));
                        }
                    }
                }
                messages.push(AnthropicMessage::Assistant { content });
            }
            MessageRole::Tool => {
                let mut content = Vec::new();
                for part in &message.content {
                    if !supports_content(part, &["tool-result"]) {
                        return Err(unsupported_content(NAME, &message.role, &["tool-result"]));
                    }
                    let ContentPart::ToolResult {
                        id, result, cache, ..
                    } = part
                    else {
                        continue;
                    };
                    content.push(AnthropicUserBlock::ToolResult(AnthropicToolResultBlock {
                        r#type: ToolResultTag::ToolResult,
                        tool_use_id: id.clone(),
                        content: lower_tool_result_content(part)?,
                        is_error: matches!(result, ToolResultValue::Error { .. }).then_some(true),
                        cache_control: cache_control(breakpoints, cache.as_ref()),
                    }));
                }
                messages.push(AnthropicMessage::User { content });
            }
        }
    }

    Ok(messages)
}

/// TS `lowerThinking` — resolve `providerOptions.anthropic.thinking`.
fn lower_thinking(request: &LlmRequest) -> Result<Option<AnthropicThinking>, LlmError> {
    let Some(anthropic) = request
        .provider_options
        .as_ref()
        .and_then(|options| options.get("anthropic"))
    else {
        return Ok(None);
    };
    let Some(thinking) = anthropic
        .get("thinking")
        .filter(|thinking| thinking.is_object())
    else {
        return Ok(None);
    };
    if thinking.get("type").and_then(Value::as_str) != Some("enabled") {
        return Ok(None);
    }
    let budget = thinking
        .get("budgetTokens")
        .and_then(Value::as_f64)
        .or_else(|| thinking.get("budget_tokens").and_then(Value::as_f64));
    let Some(budget_tokens) = budget else {
        return Err(shared::invalid_request(
            "Anthropic thinking provider option requires budgetTokens",
        ));
    };
    Ok(Some(AnthropicThinking {
        r#type: EnabledTag::Enabled,
        budget_tokens,
    }))
}

fn from_request(request: &LlmRequest) -> Result<AnthropicMessagesBody, LlmError> {
    let tool_choice = match &request.tool_choice {
        Some(tool_choice) => lower_tool_choice(tool_choice)?,
        None => None,
    };
    let generation = request.generation.as_ref();
    let tool_schema_compatibility = request
        .model
        .compatibility
        .as_ref()
        .and_then(|compatibility| compatibility.tool_schema);
    let output_limit = request
        .model
        .defaults
        .as_ref()
        .and_then(|defaults| defaults.limits.as_ref())
        .and_then(|limits| limits.output)
        .or_else(|| {
            request
                .model
                .route
                .defaults
                .limits
                .as_ref()
                .and_then(|limits| limits.output)
        })
        .unwrap_or(4096.0);

    // Allocate the 4-breakpoint budget in invalidation order: tools →
    // system → messages. Tools live highest in the cache hierarchy, so when
    // callers over-mark we keep their tool hints and shed the message-tail
    // ones first.
    let mut breakpoints = crate::protocols::utils::cache::new_breakpoints(ANTHROPIC_BREAKPOINT_CAP);
    let tools = if request.tools.is_empty()
        || request
            .tool_choice
            .as_ref()
            .is_some_and(|choice| choice.r#type == ToolChoiceType::None)
    {
        None
    } else {
        Some(
            request
                .tools
                .iter()
                .map(|tool| {
                    lower_tool(
                        &mut breakpoints,
                        tool,
                        ToolSchemaProjection::model_compatibility(
                            &tool.input_schema,
                            tool_schema_compatibility,
                        ),
                    )
                })
                .collect::<Vec<_>>(),
        )
    };
    let system = if request.system.is_empty() {
        None
    } else {
        Some(
            request
                .system
                .iter()
                .map(|part| AnthropicTextBlock {
                    r#type: TextTag::Text,
                    text: part.text.clone(),
                    cache_control: cache_control(&mut breakpoints, part.cache.as_ref()),
                })
                .collect::<Vec<_>>(),
        )
    };
    let messages = lower_messages(request, &mut breakpoints)?;
    if breakpoints.dropped > 0 {
        // TS `Effect.logWarning`; stderr is the crate's only warning channel.
        eprintln!(
            "Anthropic Messages: dropped {} cache breakpoint(s); the API allows at most {} per request.",
            breakpoints.dropped, ANTHROPIC_BREAKPOINT_CAP
        );
    }
    Ok(AnthropicMessagesBody {
        model: request.model.id.clone(),
        system,
        messages,
        tools,
        tool_choice,
        stream: true,
        max_tokens: generation
            .and_then(|generation| generation.max_tokens)
            .unwrap_or(output_limit),
        temperature: generation.and_then(|generation| generation.temperature),
        top_p: generation.and_then(|generation| generation.top_p),
        top_k: generation.and_then(|generation| generation.top_k),
        stop_sequences: generation.and_then(|generation| generation.stop.clone()),
        thinking: lower_thinking(request)?,
    })
}

// =============================================================================
// Stream Parsing
// =============================================================================
// The streaming parser is a small state machine: every event returns a new
// state plus the common `LlmEvent`s produced by that event.

/// TS `mapFinishReason`.
fn map_finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("end_turn") | Some("stop_sequence") | Some("pause_turn") => FinishReason::Stop,
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCalls,
        Some("refusal") => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

fn anthropic_metadata(value: Value) -> ProviderMetadata {
    let mut metadata = ProviderMetadata::new();
    if let Some(record) = value.as_object() {
        metadata.insert("anthropic".to_string(), record.clone());
    }
    metadata
}

/// Anthropic reports the non-overlapping breakdown natively — its
/// `input_tokens` is the *non-cached* count, with cache reads and writes as
/// separate fields. We sum them to derive the inclusive `inputTokens` the
/// rest of the contract expects. Extended thinking tokens are *not* broken
/// out by Anthropic — they're billed as part of `output_tokens`, so
/// `reasoningTokens` stays `None` and `outputTokens` carries the combined
/// total.
fn map_usage(usage: Option<&Value>) -> Option<Usage> {
    let usage = usage?.as_object()?;
    let field = |name: &str| usage.get(name).and_then(Value::as_f64);
    let non_cached = field("input_tokens");
    let cache_read = field("cache_read_input_tokens");
    let cache_write = field("cache_creation_input_tokens");
    let input_tokens = sum_tokens(&[non_cached, cache_read, cache_write]);
    let output_tokens = field("output_tokens");
    Some(Usage {
        input_tokens,
        output_tokens,
        non_cached_input_tokens: non_cached,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: cache_write,
        reasoning_tokens: None,
        total_tokens: total_tokens(input_tokens, output_tokens, None),
        provider_metadata: Some(anthropic_metadata(Value::Object(usage.clone()))),
    })
}

/// Anthropic emits usage on `message_start` and again on `message_delta` —
/// the final delta carries the authoritative totals. Right-biased merge:
/// each field prefers `right` when defined, falls back to `left`.
/// `inputTokens` is recomputed from the merged breakdown so the inclusive
/// total stays consistent with `nonCached + cacheRead + cacheWrite`.
fn merge_usage(left: Option<Usage>, right: Option<Usage>) -> Option<Usage> {
    let (left, right) = match (left, right) {
        (Some(left), Some(right)) => (left, right),
        (None, Some(right)) => return Some(right),
        (Some(left), None) => return Some(left),
        (None, None) => return None,
    };
    let non_cached_input_tokens = right
        .non_cached_input_tokens
        .or(left.non_cached_input_tokens);
    let cache_read_input_tokens = right
        .cache_read_input_tokens
        .or(left.cache_read_input_tokens);
    let cache_write_input_tokens = right
        .cache_write_input_tokens
        .or(left.cache_write_input_tokens);
    let input_tokens = sum_tokens(&[
        non_cached_input_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens,
    ]);
    let output_tokens = right.output_tokens.or(left.output_tokens);
    let mut anthropic = left
        .provider_metadata
        .as_ref()
        .and_then(|metadata| metadata.get("anthropic"))
        .cloned()
        .unwrap_or_default();
    if let Some(right_anthropic) = right
        .provider_metadata
        .as_ref()
        .and_then(|metadata| metadata.get("anthropic"))
    {
        for (key, value) in right_anthropic {
            anthropic.insert(key.clone(), value.clone());
        }
    }
    Some(Usage {
        input_tokens,
        output_tokens,
        non_cached_input_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens,
        reasoning_tokens: None,
        total_tokens: total_tokens(input_tokens, output_tokens, None),
        provider_metadata: Some(ProviderMetadata::from([(
            "anthropic".to_string(),
            anthropic,
        )])),
    })
}

/// TS `serverToolResultEvent` — server tool result blocks arrive whole in
/// `content_block_start` (no streaming delta sequence). We convert the
/// payload to a `tool-result` event with `providerExecuted: true`. The
/// runtime appends it to the assistant message for round-trip; downstream
/// consumers can inspect `result.value` for the structured payload.
fn server_tool_result_event(block: &StreamBlock) -> Option<LlmEvent> {
    let block_type = block.r#type.as_deref()?;
    let name = server_tool_result_name(block_type)?;
    let content = block.content.clone().unwrap_or(Value::Null);
    let error_payload = match content.get("type") {
        Some(value) => value
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string()),
        None => String::new(),
    };
    let is_error = error_payload.ends_with("_tool_result_error");
    Some(LlmEvent::ToolResult {
        id: block.tool_use_id.clone().unwrap_or_default(),
        name: name.to_string(),
        result: if is_error {
            ToolResultValue::Error { value: content }
        } else {
            ToolResultValue::Json { value: content }
        },
        output: None,
        provider_executed: Some(true),
        provider_metadata: Some(anthropic_metadata(json!({ "blockType": block_type }))),
    })
}

/// Anthropic keys pending tools by the numeric content block index.
fn tool_key(index: f64) -> Result<u32, LlmError> {
    if index < 0.0 || index.fract() != 0.0 || index > u32::MAX as f64 {
        return Err(shared::event_error(
            ADAPTER,
            format!("{NAME} tool argument delta has an invalid index {index}"),
            None,
        ));
    }
    Ok(index as u32)
}

fn on_message_start(state: State, event: &Event) -> State {
    match map_usage(
        event
            .message
            .as_ref()
            .and_then(|message| message.usage.as_ref()),
    ) {
        Some(usage) => {
            let mut state = state;
            state.usage = merge_usage(state.usage, Some(usage));
            state
        }
        None => state,
    }
}

fn on_content_block_start(state: State, event: &Event) -> Result<(State, Vec<LlmEvent>), LlmError> {
    let Some(block) = event.content_block.as_ref() else {
        return Ok((state, Vec::new()));
    };
    let block_type = block.r#type.as_deref();

    if matches!(block_type, Some("tool_use") | Some("server_tool_use")) {
        if let Some(index) = event.index {
            let key = tool_key(index)?;
            let id = block.id.clone().unwrap_or_else(|| key.to_string());
            let name = block.name.clone().unwrap_or_default();
            let mut events = Vec::new();
            let lifecycle = lifecycle::step_start(state.lifecycle, &mut events);
            let tools = tool_stream::start(
                state.tools,
                key,
                tool_stream::PendingTool {
                    id: id.clone(),
                    name: name.clone(),
                    input: String::new(),
                    provider_executed: Some(block_type == Some("server_tool_use")),
                    provider_metadata: None,
                },
            );
            events.push(LlmEvent::ToolInputStart {
                id,
                name,
                provider_metadata: None,
            });
            return Ok((
                State {
                    tools,
                    lifecycle,
                    usage: state.usage,
                },
                events,
            ));
        }
    }

    if block_type == Some("text") {
        if let Some(text) = block.text.as_deref().filter(|text| !text.is_empty()) {
            let mut events = Vec::new();
            let lifecycle = lifecycle::text_delta(
                state.lifecycle,
                &mut events,
                &format!("text-{}", event.index.unwrap_or(0.0)),
                text,
            );
            return Ok((State { lifecycle, ..state }, events));
        }
    }

    if block_type == Some("thinking") {
        if let Some(thinking) = block.thinking.as_deref().filter(|text| !text.is_empty()) {
            let mut events = Vec::new();
            let lifecycle = lifecycle::reasoning_delta(
                state.lifecycle,
                &mut events,
                &format!("reasoning-{}", event.index.unwrap_or(0.0)),
                thinking,
                None,
            );
            return Ok((State { lifecycle, ..state }, events));
        }
    }

    let Some(result) = server_tool_result_event(block) else {
        return Ok((state, Vec::new()));
    };
    let mut events = Vec::new();
    let lifecycle = lifecycle::step_start(state.lifecycle, &mut events);
    events.push(result);
    Ok((State { lifecycle, ..state }, events))
}

fn on_content_block_delta(state: State, event: &Event) -> Result<(State, Vec<LlmEvent>), LlmError> {
    let Some(delta) = event.delta.as_ref() else {
        return Ok((state, Vec::new()));
    };

    if delta.r#type.as_deref() == Some("text_delta") {
        if let Some(text) = delta.text.as_deref().filter(|text| !text.is_empty()) {
            let mut events = Vec::new();
            let lifecycle = lifecycle::text_delta(
                state.lifecycle,
                &mut events,
                &format!("text-{}", event.index.unwrap_or(0.0)),
                text,
            );
            return Ok((State { lifecycle, ..state }, events));
        }
    }

    if delta.r#type.as_deref() == Some("thinking_delta") {
        if let Some(thinking) = delta.thinking.as_deref().filter(|text| !text.is_empty()) {
            let mut events = Vec::new();
            let lifecycle = lifecycle::reasoning_delta(
                state.lifecycle,
                &mut events,
                &format!("reasoning-{}", event.index.unwrap_or(0.0)),
                thinking,
                None,
            );
            return Ok((State { lifecycle, ..state }, events));
        }
    }

    if delta.r#type.as_deref() == Some("signature_delta") {
        if let Some(signature) = delta.signature.as_deref().filter(|text| !text.is_empty()) {
            let mut events = Vec::new();
            let lifecycle = lifecycle::reasoning_end(
                state.lifecycle,
                &mut events,
                &format!("reasoning-{}", event.index.unwrap_or(0.0)),
                Some(anthropic_metadata(json!({ "signature": signature }))),
            );
            return Ok((State { lifecycle, ..state }, events));
        }
    }

    if delta.r#type.as_deref() == Some("input_json_delta") {
        if let Some(index) = event.index {
            let Some(partial_json) = delta.partial_json.as_deref() else {
                return Ok((state, Vec::new()));
            };
            if partial_json.is_empty() {
                return Ok((state, Vec::new()));
            }
            let key = tool_key(index)?;
            let result = tool_stream::append_existing(
                ADAPTER,
                state.tools,
                key,
                partial_json.to_string(),
                MISSING_TOOL_MESSAGE,
            )?;
            let mut events = Vec::new();
            let lifecycle = if result.events.is_empty() {
                state.lifecycle
            } else {
                lifecycle::step_start(state.lifecycle, &mut events)
            };
            events.extend(result.events);
            return Ok((
                State {
                    tools: result.tools,
                    lifecycle,
                    usage: state.usage,
                },
                events,
            ));
        }
    }

    Ok((state, Vec::new()))
}

fn on_content_block_stop(state: State, event: &Event) -> Result<(State, Vec<LlmEvent>), LlmError> {
    let Some(index) = event.index else {
        return Ok((state, Vec::new()));
    };
    let key = tool_key(index)?;
    let result = tool_stream::finish(ADAPTER, state.tools, key)?;
    let mut events = Vec::new();
    let lifecycle = if result.events.is_empty() {
        lifecycle::reasoning_end(
            lifecycle::text_end(state.lifecycle, &mut events, &format!("text-{index}"), None),
            &mut events,
            &format!("reasoning-{index}"),
            None,
        )
    } else {
        lifecycle::step_start(state.lifecycle, &mut events)
    };
    events.extend(result.events);
    Ok((
        State {
            tools: result.tools,
            lifecycle,
            usage: state.usage,
        },
        events,
    ))
}

fn on_message_delta(state: State, event: &Event) -> (State, Vec<LlmEvent>) {
    let usage = merge_usage(state.usage.clone(), map_usage(event.usage.as_ref()));
    let mut events = Vec::new();
    let provider_metadata = event
        .delta
        .as_ref()
        .and_then(|delta| delta.stop_sequence.as_deref())
        .filter(|sequence| !sequence.is_empty())
        .map(|sequence| anthropic_metadata(json!({ "stopSequence": sequence })));
    let lifecycle = lifecycle::finish(
        state.lifecycle,
        &mut events,
        FinishInput {
            reason: map_finish_reason(
                event
                    .delta
                    .as_ref()
                    .and_then(|delta| delta.stop_reason.as_deref()),
            ),
            usage: usage.clone(),
            provider_metadata,
        },
    );
    (
        State {
            tools: state.tools,
            usage,
            lifecycle,
        },
        events,
    )
}

/// Prefix `error.type` so overloads, rate limits, and quota errors are
/// visible even when the provider message is generic or empty.
fn provider_error_message(event: &Event) -> String {
    let error = event.error.as_ref();
    let error_type = error.and_then(|error| error.r#type.as_deref());
    let error_message = error.and_then(|error| error.message.as_deref());
    match (error_type, error_message) {
        (Some(error_type), Some(message)) if !error_type.is_empty() && !message.is_empty() => {
            format!("{error_type}: {message}")
        }
        (Some(error_type), _) if !error_type.is_empty() => error_type.to_string(),
        (_, Some(message)) => message.to_string(),
        _ => "Anthropic Messages stream error".to_string(),
    }
}

fn on_error(state: State, event: &Event) -> (State, Vec<LlmEvent>) {
    let message = event
        .error
        .as_ref()
        .and_then(|error| error.message.as_deref())
        .unwrap_or_default();
    (
        state,
        vec![LlmEvent::ProviderError {
            message: provider_error_message(event),
            classification: if is_context_overflow(message) {
                Some(ProviderFailureClassification::ContextOverflow)
            } else {
                None
            },
            retryable: None,
            provider_metadata: None,
        }],
    )
}

fn step(state: State, event: &Event) -> Result<(State, Vec<LlmEvent>), LlmError> {
    match event.r#type.as_deref() {
        Some("message_start") => Ok((on_message_start(state, event), Vec::new())),
        Some("content_block_start") => on_content_block_start(state, event),
        Some("content_block_delta") => on_content_block_delta(state, event),
        Some("content_block_stop") => on_content_block_stop(state, event),
        Some("message_delta") => {
            let (state, events) = on_message_delta(state, event);
            Ok((state, events))
        }
        Some("error") => {
            let (state, events) = on_error(state, event);
            Ok((state, events))
        }
        // `ping` / `message_stop` and unknown events carry no events.
        _ => Ok((state, Vec::new())),
    }
}

// =============================================================================
// Protocol And Anthropic Route
// =============================================================================

/// The Anthropic Messages protocol — request body construction and the
/// streaming-event state machine. Used by native Anthropic Cloud and (once
/// registered) Vertex Anthropic / Bedrock-hosted Anthropic passthrough.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicMessages;

impl Protocol for AnthropicMessages {
    const ID: &'static str = ADAPTER;
    type State = State;

    fn lower_body(&self, request: &LlmRequest) -> Result<Value, LlmError> {
        serde_json::to_value(from_request(request)?).map_err(|error| {
            shared::invalid_request(format!(
                "Anthropic Messages request body serialization failed: {error}"
            ))
        })
    }

    fn decode_frame(&self, frame: &Value) -> Result<Option<Value>, LlmError> {
        parse_event(frame)?;
        Ok(Some(frame.clone()))
    }

    fn initial(&self, _request: &LlmRequest) -> State {
        State {
            tools: tool_stream::empty(),
            usage: None,
            lifecycle: lifecycle::initial(),
        }
    }

    fn step(&self, state: State, event: &Value) -> Result<(State, Vec<LlmEvent>), LlmError> {
        step(state, &parse_event(event)?)
    }
}

/// Route constants for the native Anthropic deployment (TS
/// `AnthropicMessages.route`).
pub fn route_handle() -> RouteHandle {
    RouteHandle {
        id: ADAPTER.to_string(),
        protocol_id: ADAPTER.to_string(),
        endpoint: {
            let mut endpoint = Endpoint::path(PATH);
            endpoint.base_url = Some(DEFAULT_BASE_URL.to_string());
            endpoint
        },
        auth: Auth::none(),
        framing: Framing::Sse,
        defaults: RouteDefaults {
            headers: vec![("anthropic-version".to_string(), "2023-06-01".to_string())],
            ..RouteDefaults::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::messages::{ModelRef, ToolResultInput, ToolResultType};
    use crate::schema::options::{
        CacheHint, CacheHintType, GenerationOptions, ModelDefaults, ModelLimits, SystemPart,
        SystemPartType,
    };
    use std::sync::Arc;

    fn generation(max_tokens: f64, temperature: f64) -> GenerationOptions {
        GenerationOptions {
            max_tokens: Some(max_tokens),
            temperature: Some(temperature),
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            seed: None,
            stop: None,
        }
    }

    fn request() -> LlmRequest {
        LlmRequest::new(ModelRef::new(
            "claude-haiku-4-5-20251001",
            "anthropic",
            #[allow(clippy::arc_with_non_send_sync)]
            Arc::new(RouteHandle::empty()),
        ))
    }

    fn system(text: &str) -> SystemPart {
        SystemPart {
            r#type: SystemPartType::Text,
            text: text.to_string(),
            cache: None,
            metadata: None,
        }
    }

    fn weather_tool() -> ToolDefinition {
        ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get current weather for a city.".to_string(),
            input_schema: serde_json::from_value(json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"],
                "additionalProperties": false,
            }))
            .unwrap(),
            output_schema: None,
            cache: None,
            metadata: None,
            native: None,
        }
    }

    /// Numeric-tolerant JSON equality (spec §2.3): Rust lowering emits
    /// `"max_tokens": 20.0` where the TS recording has `20`.
    fn assert_json_equal(expected: &Value, actual: &Value) {
        match (expected, actual) {
            (Value::Object(expected), Value::Object(actual)) => {
                assert_eq!(
                    expected.keys().collect::<Vec<_>>(),
                    actual.keys().collect::<Vec<_>>(),
                    "key mismatch"
                );
                for (key, value) in expected {
                    assert_json_equal(value, &actual[key]);
                }
            }
            (Value::Array(expected), Value::Array(actual)) => {
                assert_eq!(expected.len(), actual.len(), "array length mismatch");
                for (expected, actual) in expected.iter().zip(actual) {
                    assert_json_equal(expected, actual);
                }
            }
            (Value::Number(expected), Value::Number(actual)) => {
                assert_eq!(expected.as_f64(), actual.as_f64(), "number mismatch");
            }
            _ => assert_eq!(expected, actual),
        }
    }

    #[test]
    fn lowered_body_matches_the_streams_text_recording() {
        let mut request = request();
        request.system = vec![system("You are concise.")];
        request.messages = vec![Message::user("Reply with exactly: Hello!")];
        request.generation = Some(generation(20.0, 0.0));

        let expected: Value = serde_json::from_str(r#"{"model":"claude-haiku-4-5-20251001","system":[{"type":"text","text":"You are concise."}],"messages":[{"role":"user","content":[{"type":"text","text":"Reply with exactly: Hello!"}]}],"stream":true,"max_tokens":20,"temperature":0}"#).unwrap();
        let body = AnthropicMessages.lower_body(&request).unwrap();
        assert_json_equal(&expected, &body);
    }

    #[test]
    fn lowered_tool_body_matches_the_streams_tool_call_recording() {
        let mut request = request();
        request.system = vec![system("Call tools exactly as requested.")];
        request.messages = vec![Message::user("Call get_weather with city exactly Paris.")];
        request.tools = vec![weather_tool()];
        request.tool_choice = Some(ToolChoice::named("get_weather"));
        request.generation = Some(generation(80.0, 0.0));

        let expected: Value = serde_json::from_str(r#"{"model":"claude-haiku-4-5-20251001","system":[{"type":"text","text":"Call tools exactly as requested."}],"messages":[{"role":"user","content":[{"type":"text","text":"Call get_weather with city exactly Paris."}]}],"tools":[{"name":"get_weather","description":"Get current weather for a city.","input_schema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}}],"tool_choice":{"type":"tool","name":"get_weather"},"stream":true,"max_tokens":80,"temperature":0}"#).unwrap();
        let body = AnthropicMessages.lower_body(&request).unwrap();
        assert_json_equal(&expected, &body);
    }

    #[test]
    fn lowered_body_matches_the_tool_loop_follow_up_recording() {
        let mut request = request();
        request.messages = vec![
            Message::user("What is the weather in Paris?"),
            Message::assistant(vec![ContentPart::tool_call(
                "toolu_01M8nJQQMxqpv1VaPYuJKT4j",
                "get_weather",
                json!({"city": "Paris"}),
            )]),
            Message::tool(ToolResultInput {
                id: "toolu_01M8nJQQMxqpv1VaPYuJKT4j".to_string(),
                name: "get_weather".to_string(),
                result: json!({"temperature": 22, "condition": "sunny"}),
                ..ToolResultInput::default()
            }),
        ];

        let body = AnthropicMessages.lower_body(&request).unwrap();
        let assistant: Value = serde_json::from_str(r#"{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01M8nJQQMxqpv1VaPYuJKT4j","name":"get_weather","input":{"city":"Paris"}}]}"#).unwrap();
        assert_json_equal(&assistant, &body["messages"][1]);
        let tool_result = &body["messages"][2];
        assert_eq!(tool_result["role"], json!("user"));
        let block = &tool_result["content"][0];
        assert_eq!(block["type"], json!("tool_result"));
        assert_eq!(
            block["tool_use_id"],
            json!("toolu_01M8nJQQMxqpv1VaPYuJKT4j")
        );
        // `tool_result.content` is a JSON-encoded string; serde_json maps sort
        // keys alphabetically, so compare the decoded payload.
        let content = serde_json::from_str::<Value>(block["content"].as_str().unwrap()).unwrap();
        assert_json_equal(&json!({"temperature": 22, "condition": "sunny"}), &content);
    }

    #[test]
    fn max_tokens_defaults_to_the_output_limit() {
        let mut request = request();
        request.messages = vec![Message::user("Hi")];
        let body = AnthropicMessages.lower_body(&request).unwrap();
        assert_eq!(body["max_tokens"], json!(4096.0));

        request.model.defaults = Some(ModelDefaults {
            limits: Some(ModelLimits {
                context: None,
                output: Some(64.0),
            }),
            generation: None,
            provider_options: None,
            http: None,
        });
        let body = AnthropicMessages.lower_body(&request).unwrap();
        assert_eq!(body["max_tokens"], json!(64.0));

        request.generation = Some(GenerationOptions {
            max_tokens: Some(8.0),
            temperature: None,
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            seed: None,
            stop: None,
        });
        let body = AnthropicMessages.lower_body(&request).unwrap();
        assert_eq!(body["max_tokens"], json!(8.0));
    }

    #[test]
    fn cache_control_follows_the_ttl_bucket_and_cap() {
        let mut request = request();
        request.system = (0..5)
            .map(|index| SystemPart {
                r#type: SystemPartType::Text,
                text: index.to_string(),
                cache: Some(CacheHint {
                    r#type: CacheHintType::Ephemeral,
                    ttl_seconds: if index == 1 { Some(3600.0) } else { None },
                }),
                metadata: None,
            })
            .collect();
        let body = AnthropicMessages.lower_body(&request).unwrap();
        let markers = body["system"]
            .as_array()
            .unwrap()
            .iter()
            .map(|part| part.get("cache_control"))
            .collect::<Vec<_>>();
        assert_eq!(markers[0], Some(&json!({"type": "ephemeral"})));
        assert_eq!(markers[1], Some(&json!({"type": "ephemeral", "ttl": "1h"})));
        assert_eq!(markers[2], Some(&json!({"type": "ephemeral"})));
        assert_eq!(markers[3], Some(&json!({"type": "ephemeral"})));
        // The 5th hint exceeds the 4-breakpoint cap and is dropped.
        assert_eq!(markers[4], None);
    }

    #[test]
    fn thinking_lowers_from_provider_options() {
        let mut request = request();
        request.provider_options = Some(
            serde_json::from_value(json!({
                "anthropic": {"thinking": {"type": "enabled", "budget_tokens": 1024}}
            }))
            .unwrap(),
        );
        let body = AnthropicMessages.lower_body(&request).unwrap();
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 1024.0})
        );

        request.provider_options = Some(
            serde_json::from_value(json!({
                "anthropic": {"thinking": {"type": "enabled"}}
            }))
            .unwrap(),
        );
        let error = AnthropicMessages.lower_body(&request).unwrap_err();
        match error.reason {
            crate::schema::errors::LlmErrorReason::InvalidRequest { message, .. } => {
                assert_eq!(
                    message,
                    "Anthropic thinking provider option requires budgetTokens"
                );
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
    }

    #[test]
    fn mid_conversation_system_updates_merge_into_the_previous_user_message() {
        let mut request = request();
        request.system = vec![system("You are concise.")];
        request.messages = vec![
            Message::user("Hello"),
            Message::system("Be briefer."),
            Message::user("Bye"),
        ];
        let body = AnthropicMessages.lower_body(&request).unwrap();
        assert_json_equal(
            &body["messages"],
            &json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "Hello"},
                    {"type": "text", "text": "<system-update>\nBe briefer.\n</system-update>"},
                ]},
                {"role": "user", "content": [{"type": "text", "text": "Bye"}]},
            ]),
        );
    }

    #[test]
    fn native_system_updates_apply_only_to_opus_4_8() {
        let mut request = request();
        request.messages = vec![
            Message::user("Hello"),
            Message::system("Be briefer."),
            Message::assistant("Sure."),
        ];
        let body = AnthropicMessages.lower_body(&request).unwrap();
        // Fallback: wrapped-user merged into the previous user message, not a
        // native system role.
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(
            body["messages"][0]["content"][1]["text"],
            json!("<system-update>\nBe briefer.\n</system-update>"),
        );

        let mut opus = request;
        opus.model.id = "claude-opus-4-8".to_string();
        let body = AnthropicMessages.lower_body(&opus).unwrap();
        assert_eq!(body["messages"][1]["role"], json!("system"));
    }

    #[test]
    fn assistant_reasoning_lowers_the_signature() {
        let mut request = request();
        request.messages = vec![Message::assistant(vec![
            ContentPart::reasoning("thinking"),
            Message::text("Hello!"),
        ])];
        let body = AnthropicMessages.lower_body(&request).unwrap();
        assert_eq!(
            body["messages"][0]["content"][0],
            json!({"type": "thinking", "thinking": "thinking"}),
        );
    }

    #[test]
    fn tool_choice_none_drops_tools() {
        let mut request = request();
        request.tools = vec![weather_tool()];
        request.tool_choice = Some(ToolChoice {
            r#type: ToolChoiceType::None,
            name: None,
        });
        let body = AnthropicMessages.lower_body(&request).unwrap();
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn tool_result_images_lower_to_native_content_blocks() {
        let mut request = request();
        request.messages = vec![Message::tool(ToolResultInput {
            id: "call_screenshot_1".to_string(),
            name: "read_screenshot".to_string(),
            result: json!([
                {"type": "text", "text": "Image read successfully"},
                {"type": "file", "uri": "aGVsbG8=", "mime": "image/png", "name": "chart.png"},
            ]),
            result_type: Some(ToolResultType::Content),
            ..ToolResultInput::default()
        })];
        let body = AnthropicMessages.lower_body(&request).unwrap();
        assert_eq!(
            body["messages"][0],
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_screenshot_1", "content": [
                {"type": "text", "text": "Image read successfully"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}},
            ]}]}),
        );
    }

    #[test]
    fn error_results_lower_with_is_error() {
        let mut request = request();
        request.messages = vec![Message::tool(ToolResultInput {
            id: "call_1".to_string(),
            name: "get_weather".to_string(),
            result: json!({"code": 500, "detail": "boom"}),
            result_type: Some(ToolResultType::Error),
            ..ToolResultInput::default()
        })];
        let body = AnthropicMessages.lower_body(&request).unwrap();
        assert_eq!(body["messages"][0]["content"][0]["is_error"], json!(true));
    }

    #[test]
    fn route_constants() {
        assert_eq!(ADAPTER, "anthropic-messages");
        assert_eq!(DEFAULT_BASE_URL, "https://api.anthropic.com/v1");
        assert_eq!(PATH, "/messages");
        let handle = route_handle();
        assert_eq!(handle.id, "anthropic-messages");
        assert_eq!(handle.protocol_id, "anthropic-messages");
        assert_eq!(handle.endpoint.base_url.as_deref(), Some(DEFAULT_BASE_URL));
        assert!(handle
            .defaults
            .headers
            .contains(&("anthropic-version".to_string(), "2023-06-01".to_string())));
    }

    // ========================================================================
    // Stream parsing
    // ========================================================================

    fn event(r#type: &str, fields: Value) -> Value {
        let mut value = json!({"type": r#type});
        if let (object, Some(fields)) = (value.as_object_mut().unwrap(), fields.as_object()) {
            for (key, field) in fields {
                object.insert(key.clone(), field.clone());
            }
        }
        value
    }

    #[test]
    fn text_stream_produces_the_full_lifecycle_sequence() {
        let protocol = AnthropicMessages;
        let state = protocol.initial(&request());

        let (state, events) = protocol
            .step(
                state,
                &event(
                    "message_start",
                    json!({"message": {"usage": {"input_tokens": 18, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 2}}}),
                ),
            )
            .unwrap();
        assert!(events.is_empty());
        let (state, events) = protocol
            .step(
                state,
                &event(
                    "content_block_start",
                    json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
                ),
            )
            .unwrap();
        assert!(events.is_empty());
        let (state, events) = protocol.step(state, &event("ping", json!({}))).unwrap();
        assert!(events.is_empty());
        let (state, events) = protocol
            .step(
                state,
                &event(
                    "content_block_delta",
                    json!({"index": 0, "delta": {"type": "text_delta", "text": "Hello!"}}),
                ),
            )
            .unwrap();
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
                    text: "Hello!".to_string(),
                    provider_metadata: None,
                },
            ],
        );
        let (state, events) = protocol
            .step(state, &event("content_block_stop", json!({"index": 0})))
            .unwrap();
        assert_eq!(
            events,
            vec![LlmEvent::TextEnd {
                id: "text-0".to_string(),
                provider_metadata: None,
            }],
        );
        let (state, events) = protocol
            .step(state, &event("message_stop", json!({})))
            .unwrap();
        assert!(events.is_empty());

        // Usage on `message_delta` supersedes the `message_start` numbers.
        let (_, events) = protocol
            .step(
                state,
                &event(
                    "message_delta",
                    json!({
                        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                        "usage": {"input_tokens": 18, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 5},
                    }),
                ),
            )
            .unwrap();
        let usage = Usage {
            input_tokens: Some(18.0),
            output_tokens: Some(5.0),
            non_cached_input_tokens: Some(18.0),
            cache_read_input_tokens: Some(0.0),
            cache_write_input_tokens: Some(0.0),
            reasoning_tokens: None,
            total_tokens: Some(23.0),
            provider_metadata: Some(
                serde_json::from_value(json!({
                    "anthropic": {"input_tokens": 18, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 5}
                }))
                .unwrap(),
            ),
        };
        assert_eq!(
            events,
            vec![
                LlmEvent::StepFinish {
                    index: 0.0,
                    reason: FinishReason::Stop,
                    usage: Some(usage.clone()),
                    provider_metadata: None,
                },
                LlmEvent::Finish {
                    reason: FinishReason::Stop,
                    usage: Some(usage),
                    provider_metadata: None,
                },
            ],
        );
    }

    #[test]
    fn tool_call_stream_accumulates_split_json_deltas() {
        let protocol = AnthropicMessages;
        let state = protocol.initial(&request());

        let (state, events) = protocol
            .step(
                state,
                &event(
                    "message_start",
                    json!({"message": {"usage": {"input_tokens": 677, "output_tokens": 16}}}),
                ),
            )
            .unwrap();
        assert!(events.is_empty());
        let (state, events) = protocol
            .step(
                state,
                &event(
                    "content_block_start",
                    json!({"index": 0, "content_block": {"type": "tool_use", "id": "toolu_012rmAruviySvUXSjgCPWVRu", "name": "get_weather", "input": {}}}),
                ),
            )
            .unwrap();
        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart { index: 0.0 },
                LlmEvent::ToolInputStart {
                    id: "toolu_012rmAruviySvUXSjgCPWVRu".to_string(),
                    name: "get_weather".to_string(),
                    provider_metadata: None,
                },
            ],
        );

        // The empty `partial_json: ""` delta is dropped.
        let (state, events) = protocol
            .step(
                state,
                &event(
                    "content_block_delta",
                    json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": ""}}),
                ),
            )
            .unwrap();
        assert!(events.is_empty());

        let (state, _) = protocol
            .step(
                state,
                &event(
                    "content_block_delta",
                    json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"city\":"}}),
                ),
            )
            .unwrap();
        let (state, events) = protocol
            .step(
                state,
                &event(
                    "content_block_delta",
                    json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": " \"Paris\"}"}}),
                ),
            )
            .unwrap();
        assert_eq!(
            events,
            vec![LlmEvent::ToolInputDelta {
                id: "toolu_012rmAruviySvUXSjgCPWVRu".to_string(),
                name: "get_weather".to_string(),
                text: " \"Paris\"}".to_string(),
            }],
        );

        let (state, events) = protocol
            .step(state, &event("content_block_stop", json!({"index": 0})))
            .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], LlmEvent::ToolInputEnd { .. }));
        match &events[1] {
            LlmEvent::ToolCall { input, .. } => assert_eq!(input, &json!({"city": "Paris"})),
            event => panic!("expected a tool-call, got {event:?}"),
        }

        let (_, events) = protocol
            .step(
                state,
                &event(
                    "message_delta",
                    json!({
                        "delta": {"stop_reason": "tool_use", "stop_sequence": null},
                        "usage": {"input_tokens": 677, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 33},
                    }),
                ),
            )
            .unwrap();
        match events.last() {
            Some(LlmEvent::Finish {
                reason,
                usage: Some(usage),
                ..
            }) => {
                assert_eq!(*reason, FinishReason::ToolCalls);
                assert_eq!(usage.input_tokens, Some(677.0));
                assert_eq!(usage.output_tokens, Some(33.0));
                assert_eq!(usage.total_tokens, Some(710.0));
            }
            event => panic!("expected a finish with usage, got {event:?}"),
        }
    }

    #[test]
    fn signature_delta_emits_reasoning_end_with_the_signature() {
        let protocol = AnthropicMessages;
        let state = protocol.initial(&request());
        let (state, _) = protocol
            .step(
                state,
                &event(
                    "content_block_start",
                    json!({"index": 0, "content_block": {"type": "thinking", "thinking": "hmm"}}),
                ),
            )
            .unwrap();
        let (state, events) = protocol
            .step(
                state,
                &event(
                    "content_block_delta",
                    json!({"index": 0, "delta": {"type": "thinking_delta", "thinking": " more"}}),
                ),
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        let (_, events) = protocol
            .step(
                state,
                &event(
                    "content_block_delta",
                    json!({"index": 0, "delta": {"type": "signature_delta", "signature": "sig-1"}}),
                ),
            )
            .unwrap();
        assert_eq!(
            events,
            vec![LlmEvent::ReasoningEnd {
                id: "reasoning-0".to_string(),
                provider_metadata: Some(anthropic_metadata(json!({"signature": "sig-1"}))),
            }],
        );
    }

    #[test]
    fn server_tool_result_blocks_arrive_as_whole_tool_result_events() {
        let protocol = AnthropicMessages;
        let state = protocol.initial(&request());
        let (state, events) = protocol
            .step(
                state,
                &event(
                    "content_block_start",
                    json!({
                        "index": 1,
                        "content_block": {
                            "type": "web_search_tool_result",
                            "tool_use_id": "toolu_server",
                            "content": {"type": "web_search_tool_result", "results": []},
                        },
                    }),
                ),
            )
            .unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], LlmEvent::StepStart { .. }));
        match &events[1] {
            LlmEvent::ToolResult {
                id,
                name,
                result,
                provider_executed,
                provider_metadata,
                ..
            } => {
                assert_eq!(id, "toolu_server");
                assert_eq!(name, "web_search");
                assert_eq!(*provider_executed, Some(true));
                assert_eq!(
                    result,
                    &ToolResultValue::Json {
                        value: json!({"type": "web_search_tool_result", "results": []}),
                    },
                );
                assert_eq!(
                    provider_metadata.as_ref().unwrap()["anthropic"]["blockType"],
                    json!("web_search_tool_result"),
                );
            }
            event => panic!("expected a tool-result, got {event:?}"),
        }

        // Error results carry the `…_tool_result_error` suffix. The lifecycle
        // step has already started, so only the tool-result is emitted.
        let (_, events) = protocol
            .step(
                state,
                &event(
                    "content_block_start",
                    json!({
                        "index": 2,
                        "content_block": {
                            "type": "code_execution_tool_result",
                            "tool_use_id": "toolu_server",
                            "content": {"type": "code_execution_tool_result_error", "error": "boom"},
                        },
                    }),
                ),
            )
            .unwrap();
        match events.last() {
            Some(LlmEvent::ToolResult { result, .. }) => {
                assert!(matches!(result, ToolResultValue::Error { .. }))
            }
            event => panic!("expected a tool-result, got {event:?}"),
        }
    }

    #[test]
    fn stream_errors_emit_a_provider_error_event() {
        let protocol = AnthropicMessages;
        let state = protocol.initial(&request());
        let (_, events) = protocol
            .step(
                state,
                &event(
                    "error",
                    json!({"error": {"type": "overloaded_error", "message": "Overloaded"}}),
                ),
            )
            .unwrap();
        assert_eq!(
            events,
            vec![LlmEvent::ProviderError {
                message: "overloaded_error: Overloaded".to_string(),
                classification: None,
                retryable: None,
                provider_metadata: None,
            }],
        );

        let state = protocol.initial(&request());
        let (_, events) = protocol
            .step(
                state,
                &event(
                    "error",
                    json!({"error": {"type": "invalid_request_error", "message": "prompt is too long: 200000 tokens > 199999 maximum"}}),
                ),
            )
            .unwrap();
        match &events[0] {
            LlmEvent::ProviderError { classification, .. } => assert_eq!(
                classification,
                &Some(ProviderFailureClassification::ContextOverflow),
            ),
            event => panic!("expected a provider-error, got {event:?}"),
        }
    }

    #[test]
    fn map_finish_reason_covers_every_provider_reason() {
        assert_eq!(map_finish_reason(Some("end_turn")), FinishReason::Stop);
        assert_eq!(map_finish_reason(Some("stop_sequence")), FinishReason::Stop);
        assert_eq!(map_finish_reason(Some("pause_turn")), FinishReason::Stop);
        assert_eq!(map_finish_reason(Some("max_tokens")), FinishReason::Length);
        assert_eq!(map_finish_reason(Some("tool_use")), FinishReason::ToolCalls);
        assert_eq!(
            map_finish_reason(Some("refusal")),
            FinishReason::ContentFilter
        );
        assert_eq!(map_finish_reason(None), FinishReason::Unknown);
        assert_eq!(map_finish_reason(Some("whatever")), FinishReason::Unknown);
    }

    #[test]
    fn usage_maps_the_native_breakdown() {
        let usage = map_usage(Some(&json!({
            "input_tokens": 9,
            "cache_read_input_tokens": 5752,
            "cache_creation_input_tokens": 0,
            "output_tokens": 3,
        })))
        .unwrap();
        assert_eq!(usage.input_tokens, Some(5761.0));
        assert_eq!(usage.non_cached_input_tokens, Some(9.0));
        assert_eq!(usage.cache_read_input_tokens, Some(5752.0));
        assert_eq!(usage.cache_write_input_tokens, Some(0.0));
        assert_eq!(usage.output_tokens, Some(3.0));
        assert_eq!(usage.total_tokens, Some(5764.0));
        assert_eq!(usage.reasoning_tokens, None);
    }
}
