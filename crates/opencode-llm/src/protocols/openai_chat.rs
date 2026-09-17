//! The OpenAI Chat protocol (TS `protocols/openai-chat.ts`).
//!
//! Request lowering is the only place that knows how common `LLMRequest`
//! messages map onto the OpenAI Chat wire format; provider quirks stay here
//! instead of leaking native fields into `LLMRequest`. Every route that
//! speaks OpenAI Chat over HTTP+SSE reuses this protocol end-to-end: native
//! OpenAI, DeepSeek, TogetherAI, Groq, OpenRouter, …
//!
//! Parser notes:
//!
//! - Tool calls are accumulated because OpenAI streams JSON arguments across
//!   multiple deltas keyed by `tool_calls[].index`.
//! - Tool-call events are produced when `finish_reason` arrives but emitted
//!   at `on_halt` (held in `State::tool_call_events`) so the trailing
//!   `include_usage` chunk's usage lands on the terminal finish events.

#![allow(clippy::result_large_err)]

use opencode_schema::llm::ToolContent;

use crate::protocols::shared::{
    self, join_text, match_tool_choice, subtract_tokens, supports_content, total_tokens,
    unsupported_content, validate_media, wrapped_system_update, MatchedToolChoice, IMAGE_MIMES,
};
use crate::protocols::utils::lifecycle;
use crate::protocols::utils::openai_options;
use crate::protocols::utils::openai_options::is_reasoning_effort;
use crate::protocols::utils::tool_schema::ToolSchemaProjection;
use crate::protocols::utils::tool_stream;
use crate::route::auth::Auth;
use crate::route::client::{RouteDefaults, RouteHandle};
use crate::route::endpoint::Endpoint;
use crate::route::framing::Framing;
use crate::route::protocol::Protocol;
use crate::schema::errors::LlmError;
use crate::schema::events::{LlmEvent, Usage};
use crate::schema::ids::{FinishReason, MessageRole, ProviderMetadata};
use crate::schema::messages::{ContentPart, LlmRequest, Message, ToolChoice, ToolResultValue};
use crate::schema::options::ModelToolSchemaCompatibility;

use serde::{Deserialize, Serialize};

/// Route id and protocol id (TS `ADAPTER`).
pub const ADAPTER: &str = "openai-chat";
/// Human-facing route name used in error messages (TS `"OpenAI Chat"`).
pub const NAME: &str = "OpenAI Chat";
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const PATH: &str = "/chat/completions";

const MISSING_TOOL_MESSAGE: &str = "OpenAI Chat tool call delta is missing id or name";

// =============================================================================
// Request Body Schema
// =============================================================================
// The body schema is the provider-native JSON body. `from_request` below
// builds this shape from the common `LlmRequest`; serializing the typed
// structs is the validation (spec §2.6).

/// Wire tag for OpenAI's `"function"` discriminating values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum FunctionTag {
    #[serde(rename = "function")]
    Function,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenAiChatFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenAiChatTool {
    pub r#type: FunctionTag,
    pub function: OpenAiChatFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenAiChatToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenAiChatAssistantToolCall {
    pub id: String,
    pub r#type: FunctionTag,
    pub function: OpenAiChatToolCallFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenAiChatImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpenAiChatUserContentPart {
    Text { text: String },
    ImageUrl { image_url: OpenAiChatImageUrl },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum OpenAiChatUserContent {
    Text(String),
    Parts(Vec<OpenAiChatUserContentPart>),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum OpenAiChatMessage {
    System {
        content: String,
    },
    User {
        content: OpenAiChatUserContent,
    },
    Assistant {
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<OpenAiChatAssistantToolCall>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenAiChatToolChoiceFunction {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum OpenAiChatToolChoice {
    Mode(String),
    Function {
        r#type: FunctionTag,
        function: OpenAiChatToolChoiceFunction,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenAiChatStreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenAiChatBody {
    pub model: String,
    pub messages: Vec<OpenAiChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OpenAiChatTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<OpenAiChatToolChoice>,
    pub stream: bool,
    pub stream_options: OpenAiChatStreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

// =============================================================================
// Streaming Event Schema
// =============================================================================
// The event schema is one decoded SSE `data:` payload.

#[derive(Debug, Clone, Default, Deserialize)]
struct ToolCallDeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ToolCallDelta {
    index: f64,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ToolCallDeltaFunction>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Clone, Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// One decoded SSE `data:` payload. `usage` stays raw so the mapped
/// [`Usage`] can carry the provider's payload verbatim in its
/// `providerMetadata` (`{openai: …}`).
#[derive(Debug, Clone, Deserialize)]
struct Event {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<serde_json::Value>,
}

/// Streaming parser state (TS `ParserState`).
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub tools: tool_stream::State<u32>,
    pub tool_call_events: Vec<LlmEvent>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<FinishReason>,
    pub lifecycle: lifecycle::State,
}

fn parse_event(frame: &serde_json::Value) -> Result<Event, LlmError> {
    let raw = frame.to_string();
    let event: Event = serde_json::from_value(frame.clone()).map_err(|error| {
        shared::event_error(
            ADAPTER,
            format!("Invalid OpenAI Chat event: {error}"),
            Some(raw.as_str()),
        )
    })?;
    if let Some(usage) = &event.usage {
        if !usage.is_object() {
            return Err(shared::event_error(
                ADAPTER,
                "Invalid OpenAI Chat event: usage must be an object",
                Some(raw.as_str()),
            ));
        }
    }
    Ok(event)
}

// =============================================================================
// Request Lowering
// =============================================================================

fn lower_tool(
    tool: &crate::schema::messages::ToolDefinition,
    input_schema: &serde_json::Map<String, serde_json::Value>,
) -> OpenAiChatTool {
    OpenAiChatTool {
        r#type: FunctionTag::Function,
        function: OpenAiChatFunction {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: serde_json::Value::Object(ToolSchemaProjection::open_ai(input_schema)),
        },
    }
}

fn lower_tool_choice(tool_choice: &ToolChoice) -> Result<OpenAiChatToolChoice, LlmError> {
    match match_tool_choice(tool_choice, NAME)? {
        MatchedToolChoice::Auto => Ok(OpenAiChatToolChoice::Mode("auto".to_string())),
        MatchedToolChoice::None => Ok(OpenAiChatToolChoice::Mode("none".to_string())),
        MatchedToolChoice::Required => Ok(OpenAiChatToolChoice::Mode("required".to_string())),
        MatchedToolChoice::Tool(name) => Ok(OpenAiChatToolChoice::Function {
            r#type: FunctionTag::Function,
            function: OpenAiChatToolChoiceFunction { name },
        }),
    }
}

fn lower_tool_call(part: &ContentPart) -> OpenAiChatAssistantToolCall {
    let ContentPart::ToolCall {
        id, name, input, ..
    } = part
    else {
        unreachable!("tool-call content part");
    };
    OpenAiChatAssistantToolCall {
        id: id.clone(),
        r#type: FunctionTag::Function,
        function: OpenAiChatToolCallFunction {
            name: name.clone(),
            arguments: input.to_string(),
        },
    }
}

fn lower_media(part: &ContentPart) -> Result<OpenAiChatUserContentPart, LlmError> {
    let media = validate_media(NAME, part, &IMAGE_MIMES)?;
    Ok(OpenAiChatUserContentPart::ImageUrl {
        image_url: OpenAiChatImageUrl {
            url: media.data_url,
        },
    })
}

/// openai-compatible providers round-trip assistant reasoning through
/// `message.native.openaiCompatible.reasoning_content`.
fn openai_compatible_reasoning_content(
    native: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    let native = native?.get("openaiCompatible")?.as_object()?;
    native
        .get("reasoning_content")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn lower_user_message(message: &Message) -> Result<OpenAiChatMessage, LlmError> {
    let mut content: Vec<OpenAiChatUserContentPart> = Vec::new();
    for part in message.content.iter() {
        match part {
            ContentPart::Text { text, .. } => {
                content.push(OpenAiChatUserContentPart::Text { text: text.clone() });
            }
            ContentPart::Media { .. } => content.push(lower_media(part)?),
            _ => {
                return Err(unsupported_content(NAME, &message.role, &["text", "media"]));
            }
        }
    }
    if content
        .iter()
        .all(|part| matches!(part, OpenAiChatUserContentPart::Text { .. }))
    {
        let text = content
            .iter()
            .map(|part| match part {
                OpenAiChatUserContentPart::Text { text } => text.as_str(),
                _ => "",
            })
            .collect::<String>();
        return Ok(OpenAiChatMessage::User {
            content: OpenAiChatUserContent::Text(text),
        });
    }
    Ok(OpenAiChatMessage::User {
        content: OpenAiChatUserContent::Parts(content),
    })
}

fn lower_assistant_message(message: &Message) -> Result<OpenAiChatMessage, LlmError> {
    let mut content = Vec::new();
    let mut reasoning = Vec::new();
    let mut tool_calls = Vec::new();
    for part in &message.content {
        if !supports_content(part, &["text", "reasoning", "tool-call"]) {
            return Err(unsupported_content(
                NAME,
                &MessageRole::Assistant,
                &["text", "reasoning", "tool-call"],
            ));
        }
        match part {
            ContentPart::Text { .. } => content.push(part.clone()),
            ContentPart::Reasoning { .. } => reasoning.push(part.clone()),
            ContentPart::ToolCall { .. } => tool_calls.push(lower_tool_call(part)),
            _ => {}
        }
    }
    let reasoning_content = if !reasoning.is_empty() {
        Some(
            reasoning
                .iter()
                .map(|part| match part {
                    ContentPart::Reasoning { text, .. } => text.as_str(),
                    _ => "",
                })
                .collect::<String>(),
        )
    } else {
        openai_compatible_reasoning_content(message.native.as_ref())
    };
    Ok(OpenAiChatMessage::Assistant {
        content: if content.is_empty() {
            None
        } else {
            Some(join_text(content.iter().map(|part| match part {
                ContentPart::Text { text, .. } => text.as_str(),
                _ => "",
            })))
        },
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        reasoning_content,
    })
}

fn lower_tool_messages(
    message: &Message,
) -> Result<(Vec<OpenAiChatMessage>, Vec<OpenAiChatUserContentPart>), LlmError> {
    let mut messages = Vec::new();
    let mut images = Vec::new();
    for part in &message.content {
        if !supports_content(part, &["tool-result"]) {
            return Err(unsupported_content(
                NAME,
                &MessageRole::Tool,
                &["tool-result"],
            ));
        }
        let ContentPart::ToolResult { id, result, .. } = part else {
            continue;
        };
        match result {
            ToolResultValue::Content { value } => {
                let text = value
                    .iter()
                    .filter_map(|item| match item {
                        ToolContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                messages.push(OpenAiChatMessage::Tool {
                    tool_call_id: id.clone(),
                    content: text,
                });
                for item in value {
                    if let ToolContent::File { uri, mime, .. } = item {
                        images.push(lower_media(&ContentPart::media(mime, uri))?);
                    }
                }
            }
            _ => {
                messages.push(OpenAiChatMessage::Tool {
                    tool_call_id: id.clone(),
                    content: shared::tool_result_text(part),
                });
            }
        }
    }
    Ok((messages, images))
}

fn lower_message(message: &Message) -> Result<Vec<OpenAiChatMessage>, LlmError> {
    match message.role {
        MessageRole::User => Ok(vec![lower_user_message(message)?]),
        MessageRole::Assistant => Ok(vec![lower_assistant_message(message)?]),
        _ => Ok(lower_tool_messages(message)?.0),
    }
}

fn lower_messages(request: &LlmRequest) -> Result<Vec<OpenAiChatMessage>, LlmError> {
    let mut messages: Vec<OpenAiChatMessage> = if request.system.is_empty() {
        Vec::new()
    } else {
        vec![OpenAiChatMessage::System {
            content: join_text(request.system.iter().map(|part| part.text.as_str())),
        }]
    };
    let mut pending_images: Vec<OpenAiChatUserContentPart> = Vec::new();
    let flush_images = |messages: &mut Vec<OpenAiChatMessage>,
                        pending: &mut Vec<OpenAiChatUserContentPart>| {
        if pending.is_empty() {
            return;
        }
        messages.push(OpenAiChatMessage::User {
            content: OpenAiChatUserContent::Parts(std::mem::take(pending)),
        });
    };
    for message in &request.messages {
        match message.role {
            MessageRole::System => {
                let part = wrapped_system_update(NAME, message)?;
                let text = match &part {
                    ContentPart::Text { text, .. } => text.clone(),
                    _ => String::new(),
                };
                if !pending_images.is_empty() {
                    let mut content = std::mem::take(&mut pending_images);
                    content.push(OpenAiChatUserContentPart::Text { text });
                    messages.push(OpenAiChatMessage::User {
                        content: OpenAiChatUserContent::Parts(content),
                    });
                    continue;
                }
                match messages.last_mut() {
                    Some(OpenAiChatMessage::User {
                        content: OpenAiChatUserContent::Text(previous),
                        ..
                    }) => {
                        *previous = format!("{previous}\n{text}");
                    }
                    Some(OpenAiChatMessage::User {
                        content: OpenAiChatUserContent::Parts(parts),
                        ..
                    }) => {
                        parts.push(OpenAiChatUserContentPart::Text { text });
                    }
                    _ => messages.push(OpenAiChatMessage::User {
                        content: OpenAiChatUserContent::Text(text),
                    }),
                }
                continue;
            }
            MessageRole::Tool => {
                let (mut lowered, mut images) = lower_tool_messages(message)?;
                messages.append(&mut lowered);
                pending_images.append(&mut images);
                continue;
            }
            _ => {}
        }
        flush_images(&mut messages, &mut pending_images);
        messages.extend(lower_message(message)?);
    }
    flush_images(&mut messages, &mut pending_images);
    Ok(messages)
}

/// TS `lowerOptions` — resolve `providerOptions.openai` readers, validating
/// that the chat-completions subset does not include `"max"`.
fn lower_options(request: &LlmRequest) -> Result<(Option<bool>, Option<String>), LlmError> {
    let store = openai_options::store(request);
    let reasoning_effort = openai_options::reasoning_effort(request);
    if let Some(effort) = reasoning_effort {
        if !is_reasoning_effort(effort) {
            return Err(shared::invalid_request(format!(
                "{NAME} does not support reasoning effort {effort}"
            )));
        }
    }
    Ok((store, reasoning_effort.map(str::to_string)))
}

fn from_request(request: &LlmRequest) -> Result<OpenAiChatBody, LlmError> {
    let generation = request.generation.as_ref();
    let tool_schema_compatibility: Option<ModelToolSchemaCompatibility> = request
        .model
        .compatibility
        .as_ref()
        .and_then(|compatibility| compatibility.tool_schema);
    let (store, reasoning_effort) = lower_options(request)?;
    Ok(OpenAiChatBody {
        model: request.model.id.clone(),
        messages: lower_messages(request)?,
        tools: if request.tools.is_empty() {
            None
        } else {
            Some(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        lower_tool(
                            tool,
                            &ToolSchemaProjection::model_compatibility(
                                &tool.input_schema,
                                tool_schema_compatibility,
                            ),
                        )
                    })
                    .collect(),
            )
        },
        tool_choice: match &request.tool_choice {
            Some(tool_choice) => Some(lower_tool_choice(tool_choice)?),
            None => None,
        },
        stream: true,
        stream_options: OpenAiChatStreamOptions {
            include_usage: true,
        },
        max_tokens: generation.and_then(|options| options.max_tokens),
        temperature: generation.and_then(|options| options.temperature),
        top_p: generation.and_then(|options| options.top_p),
        frequency_penalty: generation.and_then(|options| options.frequency_penalty),
        presence_penalty: generation.and_then(|options| options.presence_penalty),
        seed: generation.and_then(|options| options.seed),
        stop: generation.and_then(|options| options.stop.clone()),
        store,
        reasoning_effort,
    })
}

// =============================================================================
// Stream Parsing
// =============================================================================
// The streaming parser is a small state machine: every event returns a new
// state plus the common `LlmEvent`s produced by that event.

/// TS `mapFinishReason`.
fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        "function_call" | "tool_calls" => FinishReason::ToolCalls,
        _ => FinishReason::Unknown,
    }
}

/// OpenAI Chat reports `prompt_tokens` (inclusive total) with a
/// `cached_tokens` subset, and `completion_tokens` (inclusive total) with a
/// `reasoning_tokens` subset. We pass the inclusive totals through and derive
/// the non-cached breakdown so the `Usage` contract is satisfied on both
/// sides.
fn map_usage(usage: Option<&serde_json::Value>) -> Option<Usage> {
    let usage = usage?;
    let field = |name: &str| usage.get(name).and_then(serde_json::Value::as_f64);
    // TS uses `?.` chains — a missing details object means "no breakdown",
    // not "no usage".
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(serde_json::Value::as_f64);
    let reasoning = usage
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(serde_json::Value::as_f64);
    let prompt_tokens = field("prompt_tokens");
    let completion_tokens = field("completion_tokens");
    let raw = usage.as_object()?.clone();
    Some(Usage {
        input_tokens: prompt_tokens,
        output_tokens: completion_tokens,
        non_cached_input_tokens: subtract_tokens(prompt_tokens, cached),
        cache_read_input_tokens: cached,
        cache_write_input_tokens: None,
        reasoning_tokens: reasoning,
        total_tokens: total_tokens(prompt_tokens, completion_tokens, field("total_tokens")),
        provider_metadata: Some(ProviderMetadata::from([("openai".to_string(), raw)])),
    })
}

/// TS `tool_calls[].index` keys the stream accumulator. OpenAI emits
/// non-negative integers; anything else is invalid provider output.
fn tool_key(index: f64) -> Result<u32, LlmError> {
    if index < 0.0 || index.fract() != 0.0 || index > u32::MAX as f64 {
        return Err(shared::event_error(
            ADAPTER,
            format!("{NAME} tool call delta has an invalid index {index}"),
            None,
        ));
    }
    Ok(index as u32)
}

fn step(state: State, event: &serde_json::Value) -> Result<(State, Vec<LlmEvent>), LlmError> {
    let event = parse_event(event)?;
    let mut events = Vec::new();
    let usage = map_usage(event.usage.as_ref()).or(state.usage);
    let choice = event.choices.first();
    let finish_reason = match choice.and_then(|choice| choice.finish_reason.as_deref()) {
        Some(reason) if !reason.is_empty() => Some(map_finish_reason(reason)),
        _ => state.finish_reason,
    };
    let delta = choice.and_then(|choice| choice.delta.as_ref());
    let tool_deltas = delta
        .and_then(|delta| delta.tool_calls.as_deref())
        .unwrap_or_default();
    let mut tools = state.tools;
    let mut lifecycle = state.lifecycle;

    if let Some(reasoning) = delta
        .and_then(|delta| delta.reasoning_content.as_deref())
        .filter(|reasoning| !reasoning.is_empty())
    {
        lifecycle =
            lifecycle::reasoning_delta(lifecycle, &mut events, "reasoning-0", reasoning, None);
    }

    if let Some(content) = delta
        .and_then(|delta| delta.content.as_deref())
        .filter(|content| !content.is_empty())
    {
        lifecycle = lifecycle::reasoning_end(lifecycle, &mut events, "reasoning-0", None);
        lifecycle = lifecycle::text_delta(lifecycle, &mut events, "text-0", content);
    }

    if !tool_deltas.is_empty() {
        lifecycle = lifecycle::reasoning_end(lifecycle, &mut events, "reasoning-0", None);
    }

    for tool in tool_deltas {
        let outcome = tool_stream::append_or_start(
            ADAPTER,
            tools,
            tool_key(tool.index)?,
            tool_stream::ToolDelta {
                id: tool.id.clone(),
                name: tool
                    .function
                    .as_ref()
                    .and_then(|function| function.name.clone()),
                text: tool
                    .function
                    .as_ref()
                    .and_then(|function| function.arguments.clone())
                    .unwrap_or_default(),
            },
            MISSING_TOOL_MESSAGE,
        )?;
        tools = outcome.tools;
        if !outcome.events.is_empty() {
            lifecycle = lifecycle::step_start(lifecycle, &mut events);
        }
        events.extend(outcome.events);
    }

    // Finalize accumulated tool inputs eagerly when finish_reason arrives so
    // JSON parse failures fail the stream at the boundary rather than at
    // halt. The resulting events are held back for `on_halt` (STOP S11).
    let (tools, tool_call_events) =
        if finish_reason.is_some() && state.finish_reason.is_none() && !tools.is_empty() {
            let finished = tool_stream::finish_all(ADAPTER, tools)?;
            (finished.tools, finished.events)
        } else {
            (tools, state.tool_call_events)
        };

    Ok((
        State {
            tools,
            tool_call_events,
            usage,
            finish_reason,
            lifecycle,
        },
        events,
    ))
}

/// TS `finishEvents` — the `onHalt` flush: emit the buffered tool-call
/// events (with a leading `step-start` if any), then — when a finish reason
/// exists — `step-finish` + `finish`, upgrading `stop` → `tool-calls` when
/// tool calls were produced.
fn finish_events(state: State) -> Vec<LlmEvent> {
    let mut events = Vec::new();
    let has_tool_calls = !state.tool_call_events.is_empty();
    let reason = match (state.finish_reason, has_tool_calls) {
        (Some(FinishReason::Stop), true) => Some(FinishReason::ToolCalls),
        (Some(reason), _) => Some(reason),
        (None, _) => None,
    };
    let lifecycle = if has_tool_calls {
        lifecycle::step_start(state.lifecycle, &mut events)
    } else {
        state.lifecycle
    };
    events.extend(state.tool_call_events);
    if let Some(reason) = reason {
        lifecycle::finish(
            lifecycle,
            &mut events,
            lifecycle::FinishInput {
                reason,
                usage: state.usage,
                provider_metadata: None,
            },
        );
    }
    events
}

// =============================================================================
// Protocol And OpenAI Route
// =============================================================================

/// The OpenAI Chat protocol — request body construction and the
/// streaming-event state machine. Reused by every route that speaks OpenAI
/// Chat over HTTP+SSE: native OpenAI, DeepSeek, Groq, TogetherAI, OpenRouter,
/// … (see [`crate::protocols::openai_compatible_chat`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiChat;

impl Protocol for OpenAiChat {
    const ID: &'static str = ADAPTER;
    type State = State;

    fn lower_body(&self, request: &LlmRequest) -> Result<serde_json::Value, LlmError> {
        serde_json::to_value(from_request(request)?).map_err(|error| {
            shared::invalid_request(format!(
                "OpenAI Chat request body serialization failed: {error}"
            ))
        })
    }

    fn decode_frame(
        &self,
        frame: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, LlmError> {
        parse_event(frame)?;
        Ok(Some(frame.clone()))
    }

    fn initial(&self, _request: &LlmRequest) -> State {
        State {
            tools: tool_stream::empty(),
            tool_call_events: Vec::new(),
            usage: None,
            finish_reason: None,
            lifecycle: lifecycle::initial(),
        }
    }

    fn step(
        &self,
        state: State,
        event: &serde_json::Value,
    ) -> Result<(State, Vec<LlmEvent>), LlmError> {
        step(state, event)
    }

    fn on_halt(&self, state: State) -> Vec<LlmEvent> {
        finish_events(state)
    }
}

/// Route constants for the native OpenAI deployment (TS `OpenAIChat.route`).
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
        defaults: RouteDefaults::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::messages::ToolResultInput;
    use serde_json::json;
    use std::sync::Arc;

    fn generation(max_tokens: f64, temperature: f64) -> crate::schema::options::GenerationOptions {
        crate::schema::options::GenerationOptions {
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
        LlmRequest::new(crate::schema::messages::ModelRef::new(
            "gpt-4o-mini",
            "openai",
            #[allow(clippy::arc_with_non_send_sync)]
            Arc::new(RouteHandle::empty()),
        ))
    }

    /// Numeric-tolerant JSON equality (spec §2.3): Rust lowering emits
    /// `"max_tokens": 20.0` where the TS recording has `20`.
    fn assert_json_equal(expected: &serde_json::Value, actual: &serde_json::Value) {
        match (expected, actual) {
            (serde_json::Value::Object(expected), serde_json::Value::Object(actual)) => {
                assert_eq!(
                    expected.keys().collect::<Vec<_>>(),
                    actual.keys().collect::<Vec<_>>(),
                    "key mismatch"
                );
                for (key, value) in expected {
                    assert_json_equal(value, &actual[key]);
                }
            }
            (serde_json::Value::Array(expected), serde_json::Value::Array(actual)) => {
                assert_eq!(expected.len(), actual.len(), "array length mismatch");
                for (expected, actual) in expected.iter().zip(actual) {
                    assert_json_equal(expected, actual);
                }
            }
            (serde_json::Value::Number(expected), serde_json::Value::Number(actual)) => {
                assert_eq!(expected.as_f64(), actual.as_f64(), "number mismatch");
            }
            _ => assert_eq!(expected, actual),
        }
    }

    #[test]
    fn lowered_body_matches_the_streams_text_recording() {
        let mut request = request();
        request.system = vec![crate::schema::options::SystemPart {
            r#type: crate::schema::options::SystemPartType::Text,
            text: "You are concise.".to_string(),
            cache: None,
            metadata: None,
        }];
        request.messages = vec![Message::user("Say hello in one short sentence.")];
        request.generation = Some(generation(20.0, 0.0));

        let expected: serde_json::Value = serde_json::from_str(r#"{"model":"gpt-4o-mini","messages":[{"role":"system","content":"You are concise."},{"role":"user","content":"Say hello in one short sentence."}],"stream":true,"stream_options":{"include_usage":true},"max_tokens":20,"temperature":0}"#).unwrap();
        let body = OpenAiChat.lower_body(&request).unwrap();
        assert_json_equal(&expected, &body);
    }

    #[test]
    fn lowered_body_matches_the_continues_after_tool_result_recording() {
        let mut request = request();
        request.system = vec![crate::schema::options::SystemPart {
            r#type: crate::schema::options::SystemPartType::Text,
            text: "Answer using only the provided tool result.".to_string(),
            cache: None,
            metadata: None,
        }];
        request.messages = vec![
            Message::user("What is the weather in Paris?"),
            Message::assistant(vec![ContentPart::tool_call(
                "call_weather",
                "get_weather",
                json!({"city": "Paris"}),
            )]),
            Message::tool(ToolResultInput {
                id: "call_weather".to_string(),
                name: "get_weather".to_string(),
                result: json!({"forecast": "sunny", "temperature_c": 22}),
                ..ToolResultInput::default()
            }),
        ];
        request.generation = Some(generation(40.0, 0.0));

        let expected: serde_json::Value = serde_json::from_str(r#"{"model":"gpt-4o-mini","messages":[{"role":"system","content":"Answer using only the provided tool result."},{"role":"user","content":"What is the weather in Paris?"},{"role":"assistant","content":null,"tool_calls":[{"id":"call_weather","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},{"role":"tool","tool_call_id":"call_weather","content":"{\"forecast\":\"sunny\",\"temperature_c\":22}"}],"stream":true,"stream_options":{"include_usage":true},"max_tokens":40,"temperature":0}"#).unwrap();
        let body = OpenAiChat.lower_body(&request).unwrap();
        assert_json_equal(&expected, &body);
    }

    #[test]
    fn lowered_tool_body_matches_the_streams_tool_call_recording() {
        use crate::schema::messages::ToolDefinition;

        let mut request = request();
        request.system = vec![crate::schema::options::SystemPart {
            r#type: crate::schema::options::SystemPartType::Text,
            text: "Call tools exactly as requested.".to_string(),
            cache: None,
            metadata: None,
        }];
        request.messages = vec![Message::user("Call get_weather with city exactly Paris.")];
        request.tools = vec![ToolDefinition {
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
        }];
        request.tool_choice = Some(ToolChoice::named("get_weather"));
        request.generation = Some(generation(80.0, 0.0));

        let expected: serde_json::Value = serde_json::from_str(r#"{"model":"gpt-4o-mini","messages":[{"role":"system","content":"Call tools exactly as requested."},{"role":"user","content":"Call get_weather with city exactly Paris."}],"tools":[{"type":"function","function":{"name":"get_weather","description":"Get current weather for a city.","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}}}],"tool_choice":{"type":"function","function":{"name":"get_weather"}},"stream":true,"stream_options":{"include_usage":true},"max_tokens":80,"temperature":0}"#).unwrap();
        let body = OpenAiChat.lower_body(&request).unwrap();
        assert_json_equal(&expected, &body);
    }

    #[test]
    fn reasoning_effort_max_is_rejected() {
        let mut request = request();
        request.provider_options =
            Some(serde_json::from_value(json!({"openai": {"reasoningEffort": "max"}})).unwrap());
        let error = OpenAiChat.lower_body(&request).unwrap_err();
        match error.reason {
            crate::schema::errors::LlmErrorReason::InvalidRequest { message, .. } => {
                assert_eq!(message, "OpenAI Chat does not support reasoning effort max");
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
        request.provider_options = Some(
            serde_json::from_value(json!({"openai": {"reasoningEffort": "low", "store": true}}))
                .unwrap(),
        );
        let body = OpenAiChat.lower_body(&request).unwrap();
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["store"], true);
    }

    #[test]
    fn media_lowers_to_image_url_parts() {
        let mut request = request();
        request.messages = vec![Message::user(vec![
            Message::text("Look at this:"),
            ContentPart::media("image/png", "aGVsbG8="),
        ])];
        let body = OpenAiChat.lower_body(&request).unwrap();
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                {"type": "text", "text": "Look at this:"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGVsbG8="}},
            ]),
        );
    }

    #[test]
    fn assistant_reasoning_lowers_with_native_fallback() {
        let mut with_reasoning = request();
        with_reasoning.messages = vec![Message::assistant(vec![
            ContentPart::reasoning("thinking"),
            Message::text("Hello!"),
        ])];
        let body = OpenAiChat.lower_body(&with_reasoning).unwrap();
        assert_eq!(body["messages"][0]["reasoning_content"], "thinking");
        assert_eq!(body["messages"][0]["content"], "Hello!");

        let mut native = request();
        native.messages = vec![Message::assistant("Hello!")];
        native.messages[0].native = Some(
            serde_json::from_value(
                json!({"openaiCompatible": {"reasoning_content": "native reasoning"}}),
            )
            .unwrap(),
        );
        let body = OpenAiChat.lower_body(&native).unwrap();
        assert_eq!(body["messages"][0]["reasoning_content"], "native reasoning");

        let mut without_reasoning = request();
        without_reasoning.messages = vec![Message::assistant("Hello!")];
        let body = OpenAiChat.lower_body(&without_reasoning).unwrap();
        assert!(body["messages"][0].get("reasoning_content").is_none());
    }

    #[test]
    fn mid_conversation_system_updates_merge_into_the_previous_user_message() {
        let mut request = request();
        request.system = vec![crate::schema::options::SystemPart {
            r#type: crate::schema::options::SystemPartType::Text,
            text: "You are concise.".to_string(),
            cache: None,
            metadata: None,
        }];
        request.messages = vec![
            Message::user("Hello"),
            Message::system("Be briefer."),
            Message::user("Bye"),
        ];
        let body = OpenAiChat.lower_body(&request).unwrap();
        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "You are concise."},
                {"role": "user", "content": "Hello\n<system-update>\nBe briefer.\n</system-update>"},
                {"role": "user", "content": "Bye"},
            ]),
        );
    }

    #[test]
    fn tool_result_images_flush_into_the_next_user_message() {
        let mut request = request();
        request.messages = vec![
            Message::user("Check the chart."),
            Message::tool(ToolResultInput {
                id: "call_weather".to_string(),
                name: "get_weather".to_string(),
                result: json!([
                    {"type": "text", "text": "18 degrees"},
                    {"type": "file", "uri": "aGVsbG8=", "mime": "image/png", "name": "chart.png"},
                ]),
                result_type: Some(crate::schema::messages::ToolResultType::Content),
                ..ToolResultInput::default()
            }),
            Message::user("Thanks!"),
        ];
        let body = OpenAiChat.lower_body(&request).unwrap();
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "Check the chart."},
                {"role": "tool", "tool_call_id": "call_weather", "content": "18 degrees"},
                {"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGVsbG8="}},
                ]},
                {"role": "user", "content": "Thanks!"},
            ]),
        );
    }

    fn chunk(delta: serde_json::Value, finish_reason: Option<&str>) -> serde_json::Value {
        json!({
            "choices": [{"index": 0, "delta": delta, "logprobs": null, "finish_reason": finish_reason}],
            "usage": null,
        })
    }

    fn usage_chunk(usage: serde_json::Value) -> serde_json::Value {
        json!({"choices": [], "usage": usage})
    }

    #[test]
    fn tool_argument_deltas_accumulate_and_stop_upgrades_to_tool_calls() {
        let protocol = OpenAiChat;
        let state = protocol.initial(&request());

        let (state, first) = protocol
            .step(
                state,
                &chunk(
                    json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                        "function": {"name": "get_weather", "arguments": ""}}]}),
                    None,
                ),
            )
            .unwrap();
        assert_eq!(
            first,
            vec![
                LlmEvent::StepStart { index: 0.0 },
                LlmEvent::ToolInputStart {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    provider_metadata: None,
                },
            ],
        );

        let mut state = state;
        for arguments in ["{\"", "city", "\":\"", "Paris", "\"}"] {
            let (next, events) = protocol
                .step(
                    state,
                    &chunk(
                        json!({"tool_calls": [{"index": 0,
                            "function": {"arguments": arguments}}]}),
                        None,
                    ),
                )
                .unwrap();
            state = next;
            assert_eq!(
                events,
                vec![LlmEvent::ToolInputDelta {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    text: arguments.to_string(),
                }],
            );
        }

        let (state, events) = protocol
            .step(state, &chunk(json!({}), Some("stop")))
            .unwrap();
        assert!(events.is_empty());
        let (state, events) = protocol
            .step(
                state,
                &usage_chunk(json!({
                    "prompt_tokens": 67,
                    "completion_tokens": 5,
                    "total_tokens": 72,
                    "prompt_tokens_details": {"cached_tokens": 0},
                    "completion_tokens_details": {"reasoning_tokens": 0},
                })),
            )
            .unwrap();
        assert!(events.is_empty());

        let finish = protocol.on_halt(state);
        assert_eq!(
            finish[0],
            LlmEvent::ToolInputEnd {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                provider_metadata: None,
            },
        );
        match &finish[1] {
            LlmEvent::ToolCall { input, .. } => assert_eq!(input, &json!({"city": "Paris"})),
            event => panic!("expected a tool-call, got {event:?}"),
        }
        match &finish[2] {
            LlmEvent::StepFinish {
                index,
                reason,
                usage: Some(usage),
                provider_metadata,
            } => {
                assert_eq!(*index, 0.0);
                assert_eq!(*reason, FinishReason::ToolCalls);
                assert!(provider_metadata.is_none());
                assert_eq!(usage.input_tokens, Some(67.0));
                assert_eq!(usage.output_tokens, Some(5.0));
                assert_eq!(usage.non_cached_input_tokens, Some(67.0));
                assert_eq!(usage.total_tokens, Some(72.0));
            }
            event => panic!("expected a step-finish with usage, got {event:?}"),
        }
        match &finish[3] {
            LlmEvent::Finish {
                reason,
                usage: Some(usage),
                provider_metadata,
            } => {
                assert_eq!(*reason, FinishReason::ToolCalls);
                assert!(provider_metadata.is_none());
                assert_eq!(usage.input_tokens, Some(67.0));
            }
            event => panic!("expected a finish with usage, got {event:?}"),
        }
    }

    #[test]
    fn text_stream_produces_the_full_lifecycle_sequence() {
        let protocol = OpenAiChat;
        let state = protocol.initial(&request());

        // `content: ""` is falsy in TS — no events.
        let (state, events) = protocol
            .step(state, &chunk(json!({"content": ""}), None))
            .unwrap();
        assert!(events.is_empty());
        let (state, events) = protocol
            .step(state, &chunk(json!({"content": "Hello"}), None))
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
                    text: "Hello".to_string(),
                    provider_metadata: None,
                },
            ],
        );
        let (state, events) = protocol
            .step(state, &chunk(json!({"content": "!"}), None))
            .unwrap();
        assert_eq!(
            events,
            vec![LlmEvent::TextDelta {
                id: "text-0".to_string(),
                text: "!".to_string(),
                provider_metadata: None,
            }],
        );
        let (state, finish_events) = protocol
            .step(state, &chunk(json!({}), Some("stop")))
            .unwrap();
        let (state, _) = protocol
            .step(
                state,
                &usage_chunk(json!({
                    "prompt_tokens": 22,
                    "completion_tokens": 2,
                    "total_tokens": 24,
                })),
            )
            .unwrap();
        let _ = finish_events;

        let events = protocol.on_halt(state);
        let usage = Usage {
            input_tokens: Some(22.0),
            output_tokens: Some(2.0),
            non_cached_input_tokens: Some(22.0),
            cache_read_input_tokens: None,
            cache_write_input_tokens: None,
            reasoning_tokens: None,
            total_tokens: Some(24.0),
            provider_metadata: Some(
                serde_json::from_value(json!({
                    "openai": {
                        "prompt_tokens": 22,
                        "completion_tokens": 2,
                        "total_tokens": 24,
                    },
                }))
                .unwrap(),
            ),
        };
        assert_eq!(
            events,
            vec![
                LlmEvent::TextEnd {
                    id: "text-0".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::StepFinish {
                    index: 0.0,
                    reason: FinishReason::Stop,
                    usage: Some(usage.clone()),
                    provider_metadata: None,
                },
                LlmEvent::Finish {
                    reason: FinishReason::Stop,
                    usage: Some(usage.clone()),
                    provider_metadata: None,
                },
            ],
        );
    }

    #[test]
    fn reasoning_content_closes_reasoning_when_text_arrives() {
        let protocol = OpenAiChat;
        let state = protocol.initial(&request());
        let (state, events) = protocol
            .step(state, &chunk(json!({"reasoning_content": "hmm"}), None))
            .unwrap();
        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart { index: 0.0 },
                LlmEvent::ReasoningStart {
                    id: "reasoning-0".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::ReasoningDelta {
                    id: "reasoning-0".to_string(),
                    text: "hmm".to_string(),
                    provider_metadata: None,
                },
            ],
        );
        let (state, events) = protocol
            .step(state, &chunk(json!({"content": "Hello"}), None))
            .unwrap();
        assert_eq!(
            events,
            vec![
                LlmEvent::ReasoningEnd {
                    id: "reasoning-0".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::TextStart {
                    id: "text-0".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::TextDelta {
                    id: "text-0".to_string(),
                    text: "Hello".to_string(),
                    provider_metadata: None,
                },
            ],
        );
        let _ = state;
    }

    #[test]
    fn usage_maps_the_inclusive_provider_breakdown() {
        let usage = map_usage(Some(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "total_tokens": 110,
            "prompt_tokens_details": {"cached_tokens": 80},
            "completion_tokens_details": {"reasoning_tokens": 4},
        })))
        .unwrap();
        assert_eq!(usage.input_tokens, Some(100.0));
        assert_eq!(usage.output_tokens, Some(10.0));
        assert_eq!(usage.non_cached_input_tokens, Some(20.0));
        assert_eq!(usage.cache_read_input_tokens, Some(80.0));
        assert_eq!(usage.reasoning_tokens, Some(4.0));
        assert_eq!(usage.total_tokens, Some(110.0));
        assert_eq!(
            usage.provider_metadata.as_ref().unwrap()["openai"]["prompt_tokens"],
            json!(100),
        );

        // Defensive clamp: cached_tokens > prompt_tokens clamps at zero.
        let usage = map_usage(Some(&json!({
            "prompt_tokens": 100,
            "prompt_tokens_details": {"cached_tokens": 120},
        })))
        .unwrap();
        assert_eq!(usage.input_tokens, Some(100.0));
        assert_eq!(usage.non_cached_input_tokens, Some(0.0));
    }

    #[test]
    fn map_finish_reason_covers_every_provider_reason() {
        assert_eq!(map_finish_reason("stop"), FinishReason::Stop);
        assert_eq!(map_finish_reason("length"), FinishReason::Length);
        assert_eq!(
            map_finish_reason("content_filter"),
            FinishReason::ContentFilter,
        );
        assert_eq!(map_finish_reason("function_call"), FinishReason::ToolCalls);
        assert_eq!(map_finish_reason("tool_calls"), FinishReason::ToolCalls);
        assert_eq!(map_finish_reason("whatever"), FinishReason::Unknown);
    }

    #[test]
    fn route_constants() {
        assert_eq!(ADAPTER, "openai-chat");
        assert_eq!(DEFAULT_BASE_URL, "https://api.openai.com/v1");
        assert_eq!(PATH, "/chat/completions");
        let handle = route_handle();
        assert_eq!(handle.id, "openai-chat");
        assert_eq!(handle.protocol_id, "openai-chat");
        assert_eq!(handle.endpoint.base_url.as_deref(), Some(DEFAULT_BASE_URL));
    }
}
