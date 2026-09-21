//! OpenAI Responses protocol (TS `protocols/openai-responses.ts`).
//!
//! Request lowering builds the provider-native `POST /responses` body (flat
//! `{type: "function", …}` tools, heterogeneous `input` items including
//! reasoning replay with `encrypted_content`), and the streaming parser turns
//! `response.*` SSE events into common [`LlmEvent`]s.
//!
//! Not ported: the WebSocket transport (`webSocketRoute`) is out of M2 scope
//! (spec STOP S2). The native OpenAI route's static default is
//! `providerOptions: {openai: {store: false}}` — callers wire that through
//! [`crate::route::client::RouteDefaults`].

#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::protocols::shared::{
    self, event_error, invalid_request, join_text, match_tool_choice, subtract_tokens,
    supports_content, tool_result_text, total_tokens, unsupported_content, validate_media,
    validate_tool_file, wrapped_system_update,
};
use crate::protocols::utils::lifecycle;
use crate::protocols::utils::openai_options;
use crate::protocols::utils::tool_schema::ToolSchemaProjection;
use crate::protocols::utils::tool_stream::{self, PendingTool};
use crate::provider_error::is_context_overflow;
use crate::route::auth::Auth;
use crate::route::framing::Framing;
use crate::route::protocol::Protocol;
use crate::schema::errors::{LlmError, ProviderFailureClassification};
use crate::schema::events::{LlmEvent, Usage};
use crate::schema::ids::{FinishReason, JsonMap, MessageRole, ProviderMetadata};
use crate::schema::messages::{ContentPart, LlmRequest, ToolChoice, ToolResultValue};
use opencode_schema::llm::ToolContent;

pub const ADAPTER: &str = "openai-responses";
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const PATH: &str = "/responses";

const ROUTE: &str = "OpenAI Responses";
const MISSING_TOOL_MESSAGE: &str = "OpenAI Responses tool argument delta is missing its tool call";

// =============================================================================
// Request Lowering
// =============================================================================

/// `encrypted_content` on a reasoning replay item is `string | null | undefined`
/// — the distinction matters on the wire (`null` serializes, `undefined` is
/// omitted).
enum ReasoningEncrypted {
    Text(String),
    Null,
    Undefined,
}

struct LoweredReasoning {
    id: String,
    summary: Vec<Value>,
    encrypted_content: ReasoningEncrypted,
}

fn lower_tool(tool: &crate::schema::messages::ToolDefinition, input_schema: &JsonMap) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": ToolSchemaProjection::open_ai(input_schema),
        "strict": false,
    })
}

fn lower_tool_choice(tool_choice: &ToolChoice) -> Result<Value, LlmError> {
    match match_tool_choice(tool_choice, ROUTE)? {
        shared::MatchedToolChoice::Auto => Ok(json!("auto")),
        shared::MatchedToolChoice::None => Ok(json!("none")),
        shared::MatchedToolChoice::Required => Ok(json!("required")),
        shared::MatchedToolChoice::Tool(name) => Ok(json!({"type": "function", "name": name})),
    }
}

fn lower_tool_call(part: &ContentPart) -> Value {
    let ContentPart::ToolCall {
        id, name, input, ..
    } = part
    else {
        return json!({});
    };
    json!({
        "type": "function_call",
        "call_id": id,
        "name": name,
        "arguments": serde_json::to_string(input).unwrap_or_default(),
    })
}

fn lower_reasoning(part: &ContentPart) -> Option<LoweredReasoning> {
    let ContentPart::Reasoning {
        text,
        provider_metadata,
        ..
    } = part
    else {
        return None;
    };
    let openai = provider_metadata.as_ref()?.get("openai")?;
    let id = openai
        .get("itemId")?
        .as_str()
        .filter(|id| !id.is_empty())?
        .to_string();
    let encrypted_content = match openai.get("reasoningEncryptedContent") {
        Some(Value::String(value)) => ReasoningEncrypted::Text(value.clone()),
        Some(Value::Null) => ReasoningEncrypted::Null,
        _ => ReasoningEncrypted::Undefined,
    };
    Some(LoweredReasoning {
        id,
        summary: if text.is_empty() {
            Vec::new()
        } else {
            vec![json!({"type": "summary_text", "text": text})]
        },
        encrypted_content,
    })
}

fn hosted_tool_item_id(part: &ContentPart) -> Option<String> {
    let ContentPart::ToolResult {
        provider_metadata, ..
    } = part
    else {
        return None;
    };
    let id = provider_metadata
        .as_ref()?
        .get("openai")?
        .get("itemId")?
        .as_str()?
        .to_string();
    (!id.is_empty()).then_some(id)
}

fn lower_user_content(part: &ContentPart) -> Result<Value, LlmError> {
    match part {
        ContentPart::Text { text, .. } => Ok(json!({"type": "input_text", "text": text})),
        ContentPart::Media { .. } => {
            let media = validate_media(ROUTE, part, &shared::IMAGE_MIMES)?;
            Ok(json!({"type": "input_image", "image_url": media.data_url}))
        }
        _ => Err(unsupported_content(
            ROUTE,
            &MessageRole::User,
            &["text", "media"],
        )),
    }
}

/// Tool results may carry structured text/images. Keep media as provider-native
/// content instead of JSON-stringifying base64 into a prompt string.
fn lower_tool_result_content_item(item: &ToolContent) -> Result<Value, LlmError> {
    match item {
        ToolContent::Text { text } => Ok(json!({"type": "input_text", "text": text})),
        ToolContent::File { .. } => {
            let media = validate_tool_file(ROUTE, item, &shared::IMAGE_MIMES)?;
            Ok(json!({"type": "input_image", "image_url": media.data_url}))
        }
    }
}

fn lower_tool_result_output(part: &ContentPart) -> Result<Value, LlmError> {
    let ContentPart::ToolResult { result, .. } = part else {
        return Err(invalid_request("tool result content required"));
    };
    match result {
        ToolResultValue::Content { value } => {
            let mut items = Vec::with_capacity(value.len());
            for item in value {
                items.push(lower_tool_result_content_item(item)?);
            }
            Ok(Value::Array(items))
        }
        _ => Ok(json!(tool_result_text(part))),
    }
}

fn flush_assistant_text(texts: &mut Vec<String>, input: &mut Vec<Value>) {
    if texts.is_empty() {
        return;
    }
    let content: Vec<Value> = texts
        .drain(..)
        .map(|text| json!({"type": "output_text", "text": text}))
        .collect();
    input.push(json!({"role": "assistant", "content": content}));
}

fn lower_messages(request: &LlmRequest) -> Result<Vec<Value>, LlmError> {
    let mut input: Vec<Value> = Vec::new();
    if !request.system.is_empty() {
        input.push(json!({
            "role": "system",
            "content": join_text(request.system.iter().map(|part| part.text.as_str())),
        }));
    }
    let store = openai_options::store(request);

    for message in &request.messages {
        match message.role {
            MessageRole::System => {
                let part = wrapped_system_update(ROUTE, message)?;
                let previous_is_user = input
                    .last()
                    .is_some_and(|item| item.get("role").and_then(Value::as_str) == Some("user"));
                if previous_is_user {
                    let previous = input.last_mut().expect("checked above");
                    let mut content = previous
                        .get_mut("content")
                        .and_then(Value::as_array_mut)
                        .cloned()
                        .unwrap_or_default();
                    content.push(json!({"type": "input_text", "text": part_text(&part)}));
                    previous["content"] = Value::Array(content);
                } else {
                    input.push(json!({
                        "role": "user",
                        "content": [{"type": "input_text", "text": part_text(&part)}],
                    }));
                }
            }
            MessageRole::User => {
                let mut content = Vec::with_capacity(message.content.len());
                for part in &message.content {
                    content.push(lower_user_content(part)?);
                }
                input.push(json!({"role": "user", "content": content}));
            }
            MessageRole::Assistant => {
                let mut texts: Vec<String> = Vec::new();
                // `reasoningItems` mirrors the TS record of replay items; the
                // map stores the index of the item inside `input` so replay
                // folding can mutate it in place.
                let mut reasoning_items: BTreeMap<String, usize> = BTreeMap::new();
                let mut reasoning_references: BTreeSet<String> = BTreeSet::new();
                let mut hosted_tool_references: BTreeSet<String> = BTreeSet::new();

                for part in &message.content {
                    match part {
                        ContentPart::Text { text, .. } => texts.push(text.clone()),
                        ContentPart::Reasoning { .. } => {
                            flush_assistant_text(&mut texts, &mut input);
                            let Some(reasoning) = lower_reasoning(part) else {
                                continue;
                            };
                            if store != Some(false) {
                                if !reasoning_references.contains(&reasoning.id) {
                                    input.push(
                                        json!({"type": "item_reference", "id": reasoning.id}),
                                    );
                                }
                                reasoning_references.insert(reasoning.id.clone());
                                continue;
                            }
                            if let Some(index) = reasoning_items.get(&reasoning.id).copied() {
                                let existing = &mut input[index];
                                if let Some(summary) =
                                    existing.get_mut("summary").and_then(Value::as_array_mut)
                                {
                                    summary.extend(reasoning.summary.iter().cloned());
                                }
                                if let ReasoningEncrypted::Text(value) =
                                    &reasoning.encrypted_content
                                {
                                    if let Some(existing_object) = existing.as_object_mut() {
                                        existing_object
                                            .insert("encrypted_content".into(), json!(value));
                                    }
                                }
                                continue;
                            }
                            let mut replay = JsonMap::new();
                            replay.insert("type".into(), json!("reasoning"));
                            replay
                                .insert("summary".into(), Value::Array(reasoning.summary.clone()));
                            match &reasoning.encrypted_content {
                                ReasoningEncrypted::Text(value) => {
                                    replay.insert("encrypted_content".into(), json!(value));
                                }
                                ReasoningEncrypted::Null => {
                                    replay.insert("encrypted_content".into(), Value::Null);
                                }
                                ReasoningEncrypted::Undefined => {}
                            }
                            reasoning_items.insert(reasoning.id.clone(), input.len());
                            input.push(Value::Object(replay));
                        }
                        ContentPart::ToolCall {
                            provider_executed, ..
                        } => {
                            flush_assistant_text(&mut texts, &mut input);
                            if *provider_executed == Some(true) {
                                continue;
                            }
                            input.push(lower_tool_call(part));
                        }
                        ContentPart::ToolResult {
                            provider_executed: Some(true),
                            ..
                        } => {
                            flush_assistant_text(&mut texts, &mut input);
                            let item_id = hosted_tool_item_id(part);
                            if store != Some(false) {
                                if let Some(id) = &item_id {
                                    if !hosted_tool_references.contains(id) {
                                        input.push(json!({"type": "item_reference", "id": id}));
                                    }
                                }
                            }
                            if let Some(id) = item_id {
                                hosted_tool_references.insert(id);
                            }
                        }
                        _ => {
                            return Err(unsupported_content(
                                ROUTE,
                                &MessageRole::Assistant,
                                &["text", "reasoning", "tool-call", "tool-result"],
                            ));
                        }
                    }
                }
                flush_assistant_text(&mut texts, &mut input);
            }
            MessageRole::Tool => {
                for part in &message.content {
                    if !supports_content(part, &["tool-result"]) {
                        return Err(unsupported_content(ROUTE, &message.role, &["tool-result"]));
                    }
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_result_id(part),
                        "output": lower_tool_result_output(part)?,
                    }));
                }
            }
        }
    }

    // With store:false, OpenAI only accepts previous reasoning items when the
    // complete item has encrypted state. Summary blocks for one item may carry
    // that state only on the last block, so filter after they have been joined.
    if store == Some(false) {
        input.retain(|item| {
            item.get("type").and_then(Value::as_str) != Some("reasoning")
                || item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_some()
        });
    }
    Ok(input)
}

fn part_text(part: &ContentPart) -> &str {
    match part {
        ContentPart::Text { text, .. } => text,
        _ => "",
    }
}

fn tool_result_id(part: &ContentPart) -> &str {
    match part {
        ContentPart::ToolResult { id, .. } => id,
        _ => "",
    }
}

/// Fields shared between the HTTP body and the WebSocket `response.create`
/// message (TS `lowerOptions`).
fn lower_options(request: &LlmRequest) -> Result<JsonMap, LlmError> {
    let store = openai_options::store(request);
    let prompt_cache_key = openai_options::prompt_cache_key(request);
    let effort = openai_options::reasoning_effort(request);
    if let Some(effort) = effort {
        if !openai_options::is_reasoning_effort(effort) {
            return Err(invalid_request(format!(
                "OpenAI Responses does not support reasoning effort {effort}"
            )));
        }
    }
    let summary = openai_options::reasoning_summary(request);
    let include = openai_options::include(request);
    let verbosity = openai_options::text_verbosity(request);
    let instructions = openai_options::instructions(request);
    let service_tier = openai_options::service_tier(request);

    let mut options = JsonMap::new();
    if let Some(value) = instructions {
        options.insert("instructions".into(), json!(value));
    }
    if let Some(value) = store {
        options.insert("store".into(), json!(value));
    }
    if let Some(value) = prompt_cache_key {
        options.insert("prompt_cache_key".into(), json!(value));
    }
    if let Some(value) = include {
        options.insert("include".into(), json!(value));
    }
    if effort.is_some() || summary.is_some() {
        let mut reasoning = JsonMap::new();
        if let Some(value) = effort {
            reasoning.insert("effort".into(), json!(value));
        }
        if let Some(value) = summary {
            reasoning.insert("summary".into(), json!(value));
        }
        options.insert("reasoning".into(), Value::Object(reasoning));
    }
    if let Some(value) = verbosity {
        options.insert("text".into(), json!({"verbosity": value}));
    }
    if let Some(value) = service_tier {
        options.insert("service_tier".into(), json!(value));
    }
    Ok(options)
}

/// TS `fromRequest` — lower a common request into the OpenAI Responses body.
pub fn from_request(request: &LlmRequest) -> Result<Value, LlmError> {
    let generation = request.generation.as_ref();
    let compatibility = request
        .model
        .compatibility
        .as_ref()
        .and_then(|c| c.tool_schema);

    let mut body = JsonMap::new();
    body.insert("model".into(), json!(request.model.id));
    body.insert("input".into(), Value::Array(lower_messages(request)?));
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                lower_tool(
                    tool,
                    &ToolSchemaProjection::model_compatibility(&tool.input_schema, compatibility),
                )
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(tool_choice) = &request.tool_choice {
        body.insert("tool_choice".into(), lower_tool_choice(tool_choice)?);
    }
    body.insert("stream".into(), json!(true));
    if let Some(max_tokens) = generation.and_then(|options| options.max_tokens) {
        body.insert("max_output_tokens".into(), json!(max_tokens));
    }
    if let Some(temperature) = generation.and_then(|options| options.temperature) {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = generation.and_then(|options| options.top_p) {
        body.insert("top_p".into(), json!(top_p));
    }
    body.extend(lower_options(request)?);
    Ok(Value::Object(body))
}

// =============================================================================
// Stream Parsing
// =============================================================================

/// OpenAI Responses reports `input_tokens` (inclusive total) with a
/// `cached_tokens` subset, and `output_tokens` (inclusive total) with a
/// `reasoning_tokens` subset. Pass the totals through and derive the
/// non-cached breakdown.
fn map_usage(usage: Option<&Value>) -> Option<Usage> {
    let usage = usage.filter(|usage| !usage.is_null())?;
    let cached = usage
        .get("input_tokens_details")
        .filter(|details| !details.is_null())
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_f64);
    let reasoning = usage
        .get("output_tokens_details")
        .filter(|details| !details.is_null())
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_f64);
    let input_tokens = usage.get("input_tokens").and_then(Value::as_f64);
    let output_tokens = usage.get("output_tokens").and_then(Value::as_f64);
    let total = usage.get("total_tokens").and_then(Value::as_f64);
    Some(Usage {
        input_tokens,
        output_tokens,
        non_cached_input_tokens: subtract_tokens(input_tokens, cached),
        cache_read_input_tokens: cached,
        cache_write_input_tokens: None,
        reasoning_tokens: reasoning,
        total_tokens: total_tokens(input_tokens, output_tokens, total),
        provider_metadata: Some(openai_metadata(usage.clone())),
    })
}

fn map_finish_reason(event: &Value, has_function_call: bool) -> FinishReason {
    let reason = event
        .get("response")
        .filter(|response| !response.is_null())
        .and_then(|response| response.get("incomplete_details"))
        .filter(|details| !details.is_null())
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str);
    match reason {
        None => {
            if has_function_call {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            }
        }
        Some("max_output_tokens") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        Some(_) => {
            if has_function_call {
                FinishReason::ToolCalls
            } else {
                FinishReason::Unknown
            }
        }
    }
}

fn openai_metadata(metadata: Value) -> ProviderMetadata {
    let mut provider_metadata = ProviderMetadata::new();
    if let Value::Object(metadata) = metadata {
        provider_metadata.insert("openai".to_string(), metadata);
    }
    provider_metadata
}

/// `response.failed` carries the failure details under `response.error`; the
/// streaming `error` event carries them at the top level. Both paths funnel
/// through here.
fn provider_error_message(event: &Value, fallback: &str) -> String {
    let nested = event
        .get("response")
        .filter(|response| !response.is_null())
        .and_then(|response| response.get("error"))
        .filter(|error| !error.is_null());
    let message = event
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .or_else(|| {
            nested
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .filter(|message| !message.is_empty())
        });
    let code = event
        .get("code")
        .and_then(Value::as_str)
        .filter(|code| !code.is_empty())
        .or_else(|| {
            nested
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
                .filter(|code| !code.is_empty())
        });
    match (message, code) {
        (Some(message), Some(code)) => format!("{code}: {message}"),
        (Some(message), None) => message.to_string(),
        (None, Some(code)) => code.to_string(),
        (None, None) => fallback.to_string(),
    }
}

fn provider_error_event(event: &Value, fallback: &str) -> LlmEvent {
    let message = provider_error_message(event, fallback);
    let code = event
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .get("response")
                .filter(|response| !response.is_null())
                .and_then(|response| response.get("error"))
                .filter(|error| !error.is_null())
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
        })
        .filter(|code| !code.is_empty());
    let classification = if code == Some("context_length_exceeded") || is_context_overflow(&message)
    {
        Some(ProviderFailureClassification::ContextOverflow)
    } else {
        None
    };
    LlmEvent::ProviderError {
        message,
        classification,
        retryable: None,
        provider_metadata: None,
    }
}

/// One record per OpenAI Responses item type that represents a hosted
/// (provider-executed) tool call: the common name we surface. The typed input
/// extraction lives in [`hosted_tool_input`].
fn hosted_tool_name(item_type: &str) -> Option<&'static str> {
    match item_type {
        "web_search_call" => Some("web_search"),
        "web_search_preview_call" => Some("web_search_preview"),
        "file_search_call" => Some("file_search"),
        "code_interpreter_call" => Some("code_interpreter"),
        "computer_use_call" => Some("computer_use"),
        "image_generation_call" => Some("image_generation"),
        "mcp_call" => Some("mcp"),
        "local_shell_call" => Some("local_shell"),
        _ => None,
    }
}

fn or_empty_object(value: Option<&Value>) -> Value {
    value
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or(json!({}))
}

fn hosted_tool_input(item_type: &str, item: &Value) -> Value {
    match item_type {
        "web_search_call"
        | "web_search_preview_call"
        | "computer_use_call"
        | "local_shell_call" => or_empty_object(item.get("action")),
        "file_search_call" => {
            let mut input = JsonMap::new();
            input.insert(
                "queries".into(),
                item.get("queries")
                    .filter(|queries| !queries.is_null())
                    .cloned()
                    .unwrap_or(json!([])),
            );
            Value::Object(input)
        }
        "code_interpreter_call" => {
            let mut input = JsonMap::new();
            for field in ["code", "container_id"] {
                if let Some(value) = item.get(field).and_then(Value::as_str) {
                    input.insert(field.into(), json!(value));
                }
            }
            Value::Object(input)
        }
        "image_generation_call" => Value::Object(JsonMap::new()),
        "mcp_call" => {
            let mut input = JsonMap::new();
            for field in ["server_label", "name", "arguments"] {
                if let Some(value) = item.get(field).and_then(Value::as_str) {
                    input.insert(field.into(), json!(value));
                }
            }
            Value::Object(input)
        }
        _ => Value::Object(JsonMap::new()),
    }
}

/// Round-trip the full item as the structured result so consumers can extract
/// outputs / sources / status without re-decoding.
fn hosted_tool_result(item: &Value) -> crate::schema::messages::ToolResultValue {
    let is_error = item.get("error").is_some_and(|error| !error.is_null());
    if is_error {
        ToolResultValue::Error {
            value: item.get("error").cloned().unwrap_or(Value::Null),
        }
    } else {
        ToolResultValue::Json {
            value: item.clone(),
        }
    }
}

fn is_reasoning_item(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("reasoning") && item_id(item).is_some()
}

fn item_id(item: &Value) -> Option<&str> {
    item.get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

fn str_field<'a>(record: &'a Value, key: &str) -> Option<&'a str> {
    record.get(key).and_then(Value::as_str)
}

fn summary_index(event: &Value) -> Option<u64> {
    event.get("summary_index").and_then(Value::as_u64)
}

/// Status of one streamed reasoning summary part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryStatus {
    Active,
    CanConclude,
    Concluded,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReasoningStreamItem {
    pub encrypted_content: Option<String>,
    pub summary_parts: BTreeMap<u64, SummaryStatus>,
}

/// TS `ParserState` — accumulator threaded through the streaming state
/// machine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParserState {
    pub tools: tool_stream::State<String>,
    pub has_function_call: bool,
    pub lifecycle: lifecycle::State,
    pub reasoning_items: BTreeMap<String, ReasoningStreamItem>,
    pub store: Option<bool>,
}

fn no_events(state: ParserState) -> (ParserState, Vec<LlmEvent>) {
    (state, Vec::new())
}

fn reasoning_metadata(item: &Value) -> Option<ProviderMetadata> {
    let id = item_id(item)?;
    Some(openai_metadata(json!({
        "itemId": id,
        "reasoningEncryptedContent": item.get("encrypted_content").and_then(Value::as_str),
    })))
}

fn on_output_text_delta(mut state: ParserState, event: &Value) -> (ParserState, Vec<LlmEvent>) {
    let Some(delta) = str_field(event, "delta").filter(|delta| !delta.is_empty()) else {
        return no_events(state);
    };
    let id = str_field(event, "item_id").unwrap_or("text-0");
    let mut events = Vec::new();
    state.lifecycle = lifecycle::text_delta(state.lifecycle, &mut events, id, delta);
    (state, events)
}

fn on_reasoning_delta(mut state: ParserState, event: &Value) -> (ParserState, Vec<LlmEvent>) {
    let Some(delta) = str_field(event, "delta").filter(|delta| !delta.is_empty()) else {
        return no_events(state);
    };
    let item_id = str_field(event, "item_id").unwrap_or("reasoning-0");
    let id = match (summary_index(event), state.reasoning_items.get(item_id)) {
        (Some(index), _) => format!("{item_id}:{index}"),
        (None, Some(_)) => format!("{item_id}:0"),
        (None, None) => item_id.to_string(),
    };
    let mut events = Vec::new();
    state.lifecycle = lifecycle::reasoning_delta(state.lifecycle, &mut events, &id, delta, None);
    (state, events)
}

fn on_output_item_added(mut state: ParserState, event: &Value) -> (ParserState, Vec<LlmEvent>) {
    let Some(item) = event.get("item") else {
        return no_events(state);
    };
    if is_reasoning_item(item) {
        let item_id = item_id(item).expect("checked above");
        let mut events = Vec::new();
        state.lifecycle = lifecycle::reasoning_start(
            state.lifecycle,
            &mut events,
            &format!("{item_id}:0"),
            reasoning_metadata(item),
        );
        state.reasoning_items.insert(
            item_id.to_string(),
            ReasoningStreamItem {
                encrypted_content: item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                summary_parts: [(0, SummaryStatus::Active)].into(),
            },
        );
        return (state, events);
    }
    if str_field(item, "type") != Some("function_call") {
        return no_events(state);
    }
    let Some(item_id) = item_id(item) else {
        return no_events(state);
    };
    let provider_metadata = openai_metadata(json!({"itemId": item_id}));
    let mut events = Vec::new();
    let next_lifecycle = lifecycle::step_start(state.lifecycle.clone(), &mut events);
    let call_id = str_field(item, "call_id").unwrap_or(item_id);
    let name = str_field(item, "name").unwrap_or("");
    state.tools = tool_stream::start(
        state.tools,
        item_id.to_string(),
        PendingTool {
            id: call_id.to_string(),
            name: name.to_string(),
            input: str_field(item, "arguments").unwrap_or("").to_string(),
            provider_executed: None,
            provider_metadata: Some(provider_metadata.clone()),
        },
    );
    state.lifecycle = next_lifecycle;
    events.push(LlmEvent::ToolInputStart {
        id: call_id.to_string(),
        name: name.to_string(),
        provider_metadata: Some(provider_metadata),
    });
    (state, events)
}

fn on_reasoning_summary_part_added(
    mut state: ParserState,
    event: &Value,
) -> (ParserState, Vec<LlmEvent>) {
    let Some(item_id) = str_field(event, "item_id").filter(|item_id| !item_id.is_empty()) else {
        return no_events(state);
    };
    let Some(index) = summary_index(event) else {
        return no_events(state);
    };
    if index == 0 {
        if state.reasoning_items.contains_key(item_id) {
            return no_events(state);
        }
        let mut events = Vec::new();
        let existing = state.reasoning_items.get(item_id).cloned();
        state.lifecycle = lifecycle::reasoning_start(
            state.lifecycle,
            &mut events,
            &format!("{item_id}:0"),
            Some(openai_metadata(json!({
                "itemId": item_id,
                "reasoningEncryptedContent": null,
            }))),
        );
        state.reasoning_items.insert(
            item_id.to_string(),
            ReasoningStreamItem {
                encrypted_content: existing.and_then(|item| item.encrypted_content),
                summary_parts: [(0, SummaryStatus::Active)].into(),
            },
        );
        return (state, events);
    }

    let item = state
        .reasoning_items
        .get(item_id)
        .cloned()
        .unwrap_or_default();
    let mut events = Vec::new();
    let mut lifecycle_state = state.lifecycle.clone();
    for (key, status) in &item.summary_parts {
        if *status == SummaryStatus::CanConclude {
            lifecycle_state = lifecycle::reasoning_end(
                lifecycle_state,
                &mut events,
                &format!("{item_id}:{key}"),
                Some(openai_metadata(json!({"itemId": item_id}))),
            );
        }
    }
    state.lifecycle = lifecycle::reasoning_start(
        lifecycle_state,
        &mut events,
        &format!("{item_id}:{index}"),
        Some(openai_metadata(json!({
            "itemId": item_id,
            "reasoningEncryptedContent": item.encrypted_content,
        }))),
    );
    let mut summary_parts = item.summary_parts;
    for status in summary_parts.values_mut() {
        if *status == SummaryStatus::CanConclude {
            *status = SummaryStatus::Concluded;
        }
    }
    summary_parts.insert(index, SummaryStatus::Active);
    state.reasoning_items.insert(
        item_id.to_string(),
        ReasoningStreamItem {
            encrypted_content: item.encrypted_content,
            summary_parts,
        },
    );
    (state, events)
}

fn on_reasoning_summary_part_done(
    mut state: ParserState,
    event: &Value,
) -> (ParserState, Vec<LlmEvent>) {
    let Some(item_id) = str_field(event, "item_id").filter(|item_id| !item_id.is_empty()) else {
        return no_events(state);
    };
    let Some(index) = summary_index(event) else {
        return no_events(state);
    };
    let Some(item) = state.reasoning_items.get(item_id).cloned() else {
        return no_events(state);
    };
    let mut events = Vec::new();
    let concluded = state.store != Some(false);
    if concluded {
        state.lifecycle = lifecycle::reasoning_end(
            state.lifecycle,
            &mut events,
            &format!("{item_id}:{index}"),
            Some(openai_metadata(json!({"itemId": item_id}))),
        );
    }
    let mut summary_parts = item.summary_parts;
    summary_parts.insert(
        index,
        if concluded {
            SummaryStatus::Concluded
        } else {
            SummaryStatus::CanConclude
        },
    );
    state.reasoning_items.insert(
        item_id.to_string(),
        ReasoningStreamItem {
            encrypted_content: item.encrypted_content,
            summary_parts,
        },
    );
    (state, events)
}

fn on_function_call_arguments_delta(
    mut state: ParserState,
    event: &Value,
) -> Result<(ParserState, Vec<LlmEvent>), LlmError> {
    let Some(item_id) = str_field(event, "item_id").filter(|item_id| !item_id.is_empty()) else {
        return Ok(no_events(state));
    };
    let Some(delta) = str_field(event, "delta").filter(|delta| !delta.is_empty()) else {
        return Ok(no_events(state));
    };
    let outcome = tool_stream::append_existing(
        ADAPTER,
        state.tools,
        item_id.to_string(),
        delta.to_string(),
        MISSING_TOOL_MESSAGE,
    )?;
    let mut events = Vec::new();
    let lifecycle_state = if outcome.events.is_empty() {
        state.lifecycle
    } else {
        lifecycle::step_start(state.lifecycle, &mut events)
    };
    events.extend(outcome.events);
    state.lifecycle = lifecycle_state;
    state.tools = outcome.tools;
    Ok((state, events))
}

fn hosted_tool_events(item: &Value, item_type: &str, item_id: &str) -> Vec<LlmEvent> {
    let name = hosted_tool_name(item_type).expect("hosted tool checked above");
    let provider_metadata = openai_metadata(json!({"itemId": item_id}));
    vec![
        LlmEvent::ToolCall {
            id: item_id.to_string(),
            name: name.to_string(),
            input: hosted_tool_input(item_type, item),
            provider_executed: Some(true),
            provider_metadata: Some(provider_metadata.clone()),
        },
        LlmEvent::ToolResult {
            id: item_id.to_string(),
            name: name.to_string(),
            result: hosted_tool_result(item),
            output: None,
            provider_executed: Some(true),
            provider_metadata: Some(provider_metadata),
        },
    ]
}

fn on_output_item_done(
    mut state: ParserState,
    event: &Value,
) -> Result<(ParserState, Vec<LlmEvent>), LlmError> {
    let Some(item) = event.get("item") else {
        return Ok(no_events(state));
    };

    if str_field(item, "type") == Some("function_call") {
        // `!item.id || !item.call_id || !item.name` (openai-responses.ts:816)
        // — empty strings drop the item like any other missing field.
        let (Some(item_id), Some(call_id), Some(name)) = (
            item_id(item).filter(|value| !value.is_empty()),
            str_field(item, "call_id").filter(|value| !value.is_empty()),
            str_field(item, "name").filter(|value| !value.is_empty()),
        ) else {
            return Ok(no_events(state));
        };
        let tools = if state.tools.contains_key(item_id) {
            state.tools.clone()
        } else {
            tool_stream::start(
                state.tools.clone(),
                item_id.to_string(),
                PendingTool {
                    id: call_id.to_string(),
                    name: name.to_string(),
                    input: String::new(),
                    provider_executed: None,
                    provider_metadata: None,
                },
            )
        };
        let outcome = match str_field(item, "arguments") {
            Some(arguments) => {
                tool_stream::finish_with_input(ADAPTER, tools, item_id.to_string(), arguments)
            }
            None => tool_stream::finish(ADAPTER, tools, item_id.to_string()),
        }?;
        let mut events = Vec::new();
        let lifecycle_state = if outcome.events.is_empty() {
            state.lifecycle
        } else {
            lifecycle::step_start(state.lifecycle, &mut events)
        };
        let has_function_call = if outcome
            .events
            .iter()
            .any(|event| matches!(event, LlmEvent::ToolCall { .. }))
        {
            true
        } else {
            state.has_function_call
        };
        events.extend(outcome.events);
        state.lifecycle = lifecycle_state;
        state.has_function_call = has_function_call;
        state.tools = outcome.tools;
        return Ok((state, events));
    }

    if let Some(item_type) = str_field(item, "type") {
        if let Some(item_id) = item_id(item) {
            if hosted_tool_name(item_type).is_some() {
                let mut events = Vec::new();
                let lifecycle_state = lifecycle::step_start(state.lifecycle, &mut events);
                events.extend(hosted_tool_events(item, item_type, item_id));
                state.lifecycle = lifecycle_state;
                return Ok((state, events));
            }
        }
    }

    if is_reasoning_item(item) {
        let item_id = item_id(item).expect("checked above");
        let provider_metadata = reasoning_metadata(item);
        let mut events = Vec::new();
        if let Some(reasoning_item) = state.reasoning_items.get(item_id).cloned() {
            let mut lifecycle_state = state.lifecycle;
            for (key, status) in &reasoning_item.summary_parts {
                if matches!(status, SummaryStatus::Active | SummaryStatus::CanConclude) {
                    lifecycle_state = lifecycle::reasoning_end(
                        lifecycle_state,
                        &mut events,
                        &format!("{item_id}:{key}"),
                        provider_metadata.clone(),
                    );
                }
            }
            state.reasoning_items.remove(item_id);
            state.lifecycle = lifecycle_state;
            return Ok((state, events));
        }
        if !state.lifecycle.reasoning.contains(item_id) {
            let lifecycle_state = lifecycle::step_start(state.lifecycle, &mut events);
            events.push(LlmEvent::ReasoningStart {
                id: item_id.to_string(),
                provider_metadata: provider_metadata.clone(),
            });
            events.push(LlmEvent::ReasoningEnd {
                id: item_id.to_string(),
                provider_metadata,
            });
            state.lifecycle = lifecycle_state;
            return Ok((state, events));
        }
        state.lifecycle =
            lifecycle::reasoning_end(state.lifecycle, &mut events, item_id, provider_metadata);
        return Ok((state, events));
    }

    Ok(no_events(state))
}

fn on_response_finish(mut state: ParserState, event: &Value) -> (ParserState, Vec<LlmEvent>) {
    let response = event.get("response").filter(|response| !response.is_null());
    let usage = map_usage(response.and_then(|response| response.get("usage")));
    let provider_metadata = response
        .filter(|response| {
            response
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .is_some()
                || response
                    .get("service_tier")
                    .and_then(Value::as_str)
                    .filter(|tier| !tier.is_empty())
                    .is_some()
        })
        .map(|response| {
            // TS spreads `{responseId, serviceTier}` — absent values stay
            // `undefined` and are dropped from the encoded JSON.
            let mut metadata = JsonMap::new();
            if let Some(id) = response.get("id").and_then(Value::as_str) {
                metadata.insert("responseId".into(), json!(id));
            }
            match response.get("service_tier") {
                Some(Value::String(service_tier)) => {
                    metadata.insert("serviceTier".into(), json!(service_tier));
                }
                Some(Value::Null) => {
                    metadata.insert("serviceTier".into(), Value::Null);
                }
                _ => {}
            }
            openai_metadata(Value::Object(metadata))
        });
    let mut events = Vec::new();
    state.lifecycle = lifecycle::finish(
        state.lifecycle,
        &mut events,
        lifecycle::FinishInput {
            reason: map_finish_reason(event, state.has_function_call),
            usage,
            provider_metadata,
        },
    );
    (state, events)
}

fn on_response_failed(state: ParserState, event: &Value) -> (ParserState, Vec<LlmEvent>) {
    let error = provider_error_event(event, "OpenAI Responses response failed");
    (state, vec![error])
}

fn on_error(state: ParserState, event: &Value) -> (ParserState, Vec<LlmEvent>) {
    let error = provider_error_event(event, "OpenAI Responses stream error");
    (state, vec![error])
}

fn step(state: ParserState, event: &Value) -> Result<(ParserState, Vec<LlmEvent>), LlmError> {
    match str_field(event, "type") {
        Some("response.output_text.delta") => Ok(on_output_text_delta(state, event)),
        Some("response.reasoning_text.delta")
        | Some("response.reasoning_summary.delta")
        | Some("response.reasoning_summary_text.delta") => Ok(on_reasoning_delta(state, event)),
        Some("response.reasoning_summary_part.added") => {
            Ok(on_reasoning_summary_part_added(state, event))
        }
        Some("response.reasoning_summary_part.done") => {
            Ok(on_reasoning_summary_part_done(state, event))
        }
        Some("response.output_item.added") => Ok(on_output_item_added(state, event)),
        Some("response.function_call_arguments.delta") => {
            on_function_call_arguments_delta(state, event)
        }
        Some("response.output_item.done") => on_output_item_done(state, event),
        Some("response.completed") | Some("response.incomplete") => {
            Ok(on_response_finish(state, event))
        }
        Some("response.failed") => Ok(on_response_failed(state, event)),
        Some("error") => Ok(on_error(state, event)),
        _ => Ok(no_events(state)),
    }
}

// =============================================================================
// Protocol And OpenAI Route
// =============================================================================

/// The OpenAI Responses protocol — request body construction and the
/// streaming-event state machine.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiResponses;

impl Protocol for OpenAiResponses {
    const ID: &'static str = ADAPTER;

    type State = ParserState;

    fn lower_body(&self, request: &LlmRequest) -> Result<Value, LlmError> {
        from_request(request)
    }

    fn decode_frame(&self, frame: &Value) -> Result<Option<Value>, LlmError> {
        if frame.get("type").and_then(Value::as_str).is_some() {
            Ok(Some(frame.clone()))
        } else {
            Err(event_error(
                ADAPTER,
                format!("Invalid {ADAPTER} stream event"),
                Some(&frame.to_string()),
            ))
        }
    }

    fn initial(&self, request: &LlmRequest) -> Self::State {
        ParserState {
            tools: tool_stream::empty(),
            has_function_call: false,
            lifecycle: lifecycle::initial(),
            reasoning_items: BTreeMap::new(),
            store: openai_options::store(request),
        }
    }

    fn step(
        &self,
        state: Self::State,
        event: &Value,
    ) -> Result<(Self::State, Vec<LlmEvent>), LlmError> {
        step(state, event)
    }

    /// `response.completed` / `response.incomplete` are clean finishes;
    /// `response.failed` is a hard failure. All three end the stream.
    fn terminal(&self, event: &Value) -> bool {
        matches!(
            str_field(event, "type"),
            Some("response.completed" | "response.incomplete" | "response.failed")
        )
    }
}

/// Route constants for the native OpenAI deployment (TS `OpenAIResponses.route`).
pub fn route_handle() -> crate::route::client::RouteHandle {
    crate::route::client::RouteHandle {
        id: ADAPTER.to_string(),
        protocol_id: ADAPTER.to_string(),
        endpoint: {
            let mut endpoint = crate::route::endpoint::Endpoint::path(PATH);
            endpoint.base_url = Some(DEFAULT_BASE_URL.to_string());
            endpoint
        },
        auth: Auth::none(),
        framing: Framing::Sse,
        defaults: crate::route::client::RouteDefaults::default(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]

    use std::sync::Arc;

    use crate::route::client::RouteHandle;
    use crate::schema::errors::ProviderFailureClassification;
    use crate::schema::messages::{Message, ModelRef, ToolResultInput};
    use crate::schema::options::{SystemPart, SystemPartType};

    use super::*;

    fn model_ref() -> ModelRef {
        ModelRef::new("gpt-5.5", "openai", Arc::new(RouteHandle::empty()))
    }

    fn make_request() -> LlmRequest {
        LlmRequest::new(model_ref())
    }

    fn openai_pm(value: Value) -> Option<ProviderMetadata> {
        let mut metadata = ProviderMetadata::new();
        if let Value::Object(object) = value {
            metadata.insert("openai".to_string(), object);
        }
        Some(metadata)
    }

    fn replay(request: &LlmRequest, events: &[Value]) -> Vec<LlmEvent> {
        let protocol = OpenAiResponses;
        let mut state = protocol.initial(request);
        let mut out = Vec::new();
        for event in events {
            let (next, mut emitted) = protocol.step(state, event).unwrap();
            state = next;
            out.append(&mut emitted);
        }
        out
    }

    #[test]
    fn lower_body_matches_the_recorded_tool_call_request() {
        let mut request = make_request();
        request.system = vec![SystemPart {
            r#type: SystemPartType::Text,
            text: "Call tools exactly as requested.".to_string(),
            cache: None,
            metadata: None,
        }];
        request.messages = vec![Message::user("Call get_weather with city exactly Paris.")];
        request.tools = vec![crate::schema::messages::ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get current weather for a city.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"],
                "additionalProperties": false,
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
            output_schema: None,
            cache: None,
            metadata: None,
            native: None,
        }];
        request.tool_choice = Some(ToolChoice::named("get_weather"));
        request.generation = Some(crate::schema::options::GenerationOptions {
            max_tokens: Some(80.0),
            temperature: None,
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            seed: None,
            stop: None,
        });

        let body = OpenAiResponses.lower_body(&request).unwrap();
        assert_eq!(
            body,
            json!({
                "model": "gpt-5.5",
                "input": [
                    {"role": "system", "content": "Call tools exactly as requested."},
                    {"role": "user", "content": [{"type": "input_text", "text": "Call get_weather with city exactly Paris."}]},
                ],
                "tools": [{
                    "type": "function",
                    "name": "get_weather",
                    "description": "Get current weather for a city.",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false},
                    "strict": false,
                }],
                "tool_choice": {"type": "function", "name": "get_weather"},
                "stream": true,
                "max_output_tokens": 80.0,
            }),
        );
    }

    #[test]
    fn lower_body_rejects_max_reasoning_effort() {
        let mut request = make_request();
        request.provider_options =
            Some(serde_json::from_value(json!({"openai": {"reasoningEffort": "max"}})).unwrap());
        let error = OpenAiResponses.lower_body(&request).unwrap_err();
        assert!(
            error
                .reason
                .to_string()
                .contains("OpenAI Responses does not support reasoning effort max"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn reasoning_replay_lowering_with_store_false() {
        let mut request = make_request();
        request.provider_options =
            Some(serde_json::from_value(json!({"openai": {"store": false}})).unwrap());
        request.messages = vec![Message::assistant(vec![ContentPart::Reasoning {
            text: String::new(),
            encrypted: None,
            metadata: None,
            provider_metadata: openai_pm(json!({
                "itemId": "rs_1",
                "reasoningEncryptedContent": "gAAAA",
            })),
        }])];
        request.messages[0].content.push(Message::text("Hello!"));

        let body = from_request(&request).unwrap();
        assert_eq!(
            body["input"],
            json!([
                {"type": "reasoning", "summary": [], "encrypted_content": "gAAAA"},
                {"role": "assistant", "content": [{"type": "output_text", "text": "Hello!"}]},
            ]),
        );

        // A reasoning part without encrypted state is dropped under store:false
        // (OpenAI rejects it), but kept when the item id is absent.
        request.messages[0].content[0] = ContentPart::Reasoning {
            text: String::new(),
            encrypted: None,
            metadata: None,
            provider_metadata: openai_pm(json!({"reasoningEncryptedContent": null})),
        };
        let body = from_request(&request).unwrap();
        assert_eq!(body["input"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            body["input"][0],
            json!({"role": "assistant", "content": [{"type": "output_text", "text": "Hello!"}]}),
        );
    }

    #[test]
    fn reasoning_replay_uses_item_references_when_store_is_not_false() {
        let mut request = make_request();
        request.messages = vec![Message::assistant(vec![ContentPart::Reasoning {
            text: "thinking".to_string(),
            encrypted: None,
            metadata: None,
            provider_metadata: openai_pm(json!({
                "itemId": "rs_1",
                "reasoningEncryptedContent": "gAAAA",
            })),
        }])];
        let body = from_request(&request).unwrap();
        assert_eq!(
            body["input"],
            json!([{"type": "item_reference", "id": "rs_1"}]),
        );
    }

    #[test]
    fn tool_results_lower_as_function_call_output() {
        let request = &mut make_request();
        request.messages = vec![Message::tool(ContentPart::tool_result(ToolResultInput {
            id: "call_1".to_string(),
            name: "get_weather".to_string(),
            result: json!({"temperature": 22, "condition": "sunny"}),
            ..ToolResultInput::default()
        }))];
        let body = from_request(request).unwrap();
        assert_eq!(
            body["input"],
            json!([{
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "{\"condition\":\"sunny\",\"temperature\":22}",
            }]),
        );
    }

    #[test]
    fn content_tool_results_keep_provider_native_images() {
        let mut request = make_request();
        request.messages = vec![Message::tool(ContentPart::tool_result(ToolResultInput {
            id: "call_screenshot_1".to_string(),
            name: "read_screenshot".to_string(),
            result: json!([
                {"type": "text", "text": "Image read successfully"},
                {"type": "file", "uri": "data:image/png;base64,aGVsbG8=", "mime": "image/png"},
            ]),
            result_type: Some(crate::schema::messages::ToolResultType::Content),
            ..ToolResultInput::default()
        }))];
        let body = from_request(&request).unwrap();
        assert_eq!(
            body["input"],
            json!([{
                "type": "function_call_output",
                "call_id": "call_screenshot_1",
                "output": [
                    {"type": "input_text", "text": "Image read successfully"},
                    {"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="},
                ],
            }]),
        );
    }

    #[test]
    fn mid_conversation_system_updates_merge_into_the_previous_user_item() {
        let mut request = make_request();
        request.messages = vec![
            Message::user("Hello!"),
            Message::system("New instructions."),
        ];
        let body = from_request(&request).unwrap();
        assert_eq!(
            body["input"],
            json!([{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Hello!"},
                    {"type": "input_text", "text": "<system-update>\nNew instructions.\n</system-update>"},
                ],
            }]),
        );
    }

    #[test]
    fn text_and_reasoning_stream_produces_the_recorded_event_sequence() {
        let reasoning_added = json!({
            "type": "response.output_item.added",
            "item": {"id": "rs_1", "type": "reasoning", "encrypted_content": "enc-added", "summary": []},
        });
        let reasoning_done = json!({
            "type": "response.output_item.done",
            "item": {"id": "rs_1", "type": "reasoning", "encrypted_content": "enc-done", "summary": []},
        });
        let message_added = json!({
            "type": "response.output_item.added",
            "item": {"id": "msg_1", "type": "message", "status": "in_progress", "content": []},
        });
        let text_delta = |delta: &str| {
            json!({
                "type": "response.output_text.delta",
                "delta": delta,
                "item_id": "msg_1",
            })
        };
        let completed = json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "service_tier": "default",
                "incomplete_details": None::<String>,
                "usage": {
                    "input_tokens": 31,
                    "input_tokens_details": {"cached_tokens": 0},
                    "output_tokens": 21,
                    "output_tokens_details": {"reasoning_tokens": 13},
                    "total_tokens": 52,
                },
            },
        });

        let events = replay(
            &make_request(),
            &[
                json!({"type": "response.created", "response": {}}),
                json!({"type": "response.in_progress", "response": {}}),
                reasoning_added,
                reasoning_done,
                message_added,
                text_delta("Hello"),
                text_delta("!"),
                completed,
            ],
        );

        let usage = Usage {
            input_tokens: Some(31.0),
            output_tokens: Some(21.0),
            non_cached_input_tokens: Some(31.0),
            cache_read_input_tokens: Some(0.0),
            cache_write_input_tokens: None,
            reasoning_tokens: Some(13.0),
            total_tokens: Some(52.0),
            provider_metadata: openai_pm(json!({
                "input_tokens": 31,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens": 21,
                "output_tokens_details": {"reasoning_tokens": 13},
                "total_tokens": 52,
            })),
        };
        let finish_metadata = openai_pm(json!({"responseId": "resp_1", "serviceTier": "default"}));
        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart { index: 0.0 },
                LlmEvent::ReasoningStart {
                    id: "rs_1:0".to_string(),
                    provider_metadata: openai_pm(json!({
                        "itemId": "rs_1",
                        "reasoningEncryptedContent": "enc-added",
                    })),
                },
                LlmEvent::ReasoningEnd {
                    id: "rs_1:0".to_string(),
                    provider_metadata: openai_pm(json!({
                        "itemId": "rs_1",
                        "reasoningEncryptedContent": "enc-done",
                    })),
                },
                LlmEvent::TextStart {
                    id: "msg_1".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::TextDelta {
                    id: "msg_1".to_string(),
                    text: "Hello".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::TextDelta {
                    id: "msg_1".to_string(),
                    text: "!".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::TextEnd {
                    id: "msg_1".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::StepFinish {
                    index: 0.0,
                    reason: FinishReason::Stop,
                    usage: Some(usage.clone()),
                    provider_metadata: finish_metadata.clone(),
                },
                LlmEvent::Finish {
                    reason: FinishReason::Stop,
                    usage: Some(usage),
                    provider_metadata: finish_metadata,
                },
            ],
        );
    }

    #[test]
    fn tool_call_stream_produces_tool_input_events_then_a_tool_call() {
        let deltas = ["{\"", "city", "\":\"", "Paris", "\"}"];
        let mut tool_events = vec![json!({
            "type": "response.output_item.added",
            "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": ""},
        })];
        tool_events.extend(deltas.iter().map(|delta| {
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc_1",
                "delta": delta,
            })
        }));
        tool_events.push(json!({
            "type": "response.function_call_arguments.done",
            "item_id": "fc_1",
            "arguments": "{\"city\":\"Paris\"}",
        }));
        tool_events.push(json!({
            "type": "response.output_item.done",
            "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
        }));
        tool_events.push(json!({
            "type": "response.completed",
            "response": {"id": "resp_1", "usage": {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12}},
        }));

        let events = replay(&make_request(), &tool_events);

        let tool_input_metadata = openai_pm(json!({"itemId": "fc_1"}));
        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart { index: 0.0 },
                LlmEvent::ToolInputStart {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    provider_metadata: tool_input_metadata.clone(),
                },
                LlmEvent::ToolInputDelta {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    text: "{\"".to_string(),
                },
                LlmEvent::ToolInputDelta {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    text: "city".to_string(),
                },
                LlmEvent::ToolInputDelta {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    text: "\":\"".to_string(),
                },
                LlmEvent::ToolInputDelta {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    text: "Paris".to_string(),
                },
                LlmEvent::ToolInputDelta {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    text: "\"}".to_string(),
                },
                LlmEvent::ToolInputEnd {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    provider_metadata: tool_input_metadata.clone(),
                },
                LlmEvent::ToolCall {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    input: json!({"city": "Paris"}),
                    provider_executed: None,
                    provider_metadata: tool_input_metadata,
                },
                LlmEvent::StepFinish {
                    index: 0.0,
                    reason: FinishReason::ToolCalls,
                    usage: Some(Usage {
                        input_tokens: Some(10.0),
                        output_tokens: Some(2.0),
                        non_cached_input_tokens: Some(10.0),
                        cache_read_input_tokens: None,
                        cache_write_input_tokens: None,
                        reasoning_tokens: None,
                        total_tokens: Some(12.0),
                        provider_metadata: openai_pm(json!({
                            "input_tokens": 10, "output_tokens": 2, "total_tokens": 12,
                        })),
                    }),
                    provider_metadata: openai_pm(json!({"responseId": "resp_1"})),
                },
                LlmEvent::Finish {
                    reason: FinishReason::ToolCalls,
                    usage: Some(Usage {
                        input_tokens: Some(10.0),
                        output_tokens: Some(2.0),
                        non_cached_input_tokens: Some(10.0),
                        cache_read_input_tokens: None,
                        cache_write_input_tokens: None,
                        reasoning_tokens: None,
                        total_tokens: Some(12.0),
                        provider_metadata: openai_pm(json!({
                            "input_tokens": 10, "output_tokens": 2, "total_tokens": 12,
                        })),
                    }),
                    provider_metadata: openai_pm(json!({"responseId": "resp_1"})),
                },
            ],
        );
    }

    #[test]
    fn reasoning_item_done_emits_reasoning_end_with_encrypted_content() {
        let mut request = make_request();
        request.provider_options =
            Some(serde_json::from_value(json!({"openai": {"store": false}})).unwrap());
        request.messages.push(Message::user(
            "Think briefly, then reply exactly with: Hello!",
        ));

        let events = replay(
            &request,
            &[
                json!({
                    "type": "response.reasoning_summary_part.added",
                    "item_id": "rs_1",
                    "summary_index": 0,
                }),
                json!({
                    "type": "response.reasoning_summary_text.delta",
                    "item_id": "rs_1",
                    "summary_index": 0,
                    "delta": "Think",
                }),
                json!({
                    "type": "response.reasoning_summary_part.done",
                    "item_id": "rs_1",
                    "summary_index": 0,
                }),
                json!({
                    "type": "response.output_item.done",
                    "item": {"id": "rs_1", "type": "reasoning", "encrypted_content": "gAAAA", "summary": []},
                }),
            ],
        );

        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart { index: 0.0 },
                LlmEvent::ReasoningStart {
                    id: "rs_1:0".to_string(),
                    provider_metadata: openai_pm(json!({
                        "itemId": "rs_1",
                        "reasoningEncryptedContent": null,
                    })),
                },
                LlmEvent::ReasoningDelta {
                    id: "rs_1:0".to_string(),
                    text: "Think".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::ReasoningEnd {
                    id: "rs_1:0".to_string(),
                    provider_metadata: openai_pm(json!({
                        "itemId": "rs_1",
                        "reasoningEncryptedContent": "gAAAA",
                    })),
                },
            ],
        );
    }

    #[test]
    fn hosted_tool_items_pair_tool_call_and_tool_result() {
        let events = replay(
            &make_request(),
            &[json!({
                "type": "response.output_item.done",
                "item": {"id": "ws_1", "type": "web_search_call", "action": {"query": "weather"}},
            })],
        );
        let expected = vec![
            LlmEvent::StepStart { index: 0.0 },
            LlmEvent::ToolCall {
                id: "ws_1".to_string(),
                name: "web_search".to_string(),
                input: json!({"query": "weather"}),
                provider_executed: Some(true),
                provider_metadata: openai_pm(json!({"itemId": "ws_1"})),
            },
            LlmEvent::ToolResult {
                id: "ws_1".to_string(),
                name: "web_search".to_string(),
                result: ToolResultValue::Json {
                    value: json!({"id": "ws_1", "type": "web_search_call", "action": {"query": "weather"}}),
                },
                output: None,
                provider_executed: Some(true),
                provider_metadata: openai_pm(json!({"itemId": "ws_1"})),
            },
        ];
        assert_eq!(events, expected);
    }

    #[test]
    fn response_failed_emits_a_provider_error_with_code_prefix() {
        let events = replay(
            &make_request(),
            &[json!({
                "type": "response.failed",
                "response": {"error": {"code": "rate_limit_exceeded", "message": "Slow down"}},
            })],
        );
        assert_eq!(
            events,
            vec![LlmEvent::ProviderError {
                message: "rate_limit_exceeded: Slow down".to_string(),
                classification: None,
                retryable: None,
                provider_metadata: None,
            }],
        );
    }

    #[test]
    fn stream_error_classifies_context_overflow() {
        let events = replay(
            &make_request(),
            &[json!({
                "type": "error",
                "code": "context_length_exceeded",
                "message": "This model's maximum context length is 4096 tokens.",
            })],
        );
        assert_eq!(
            events,
            vec![LlmEvent::ProviderError {
                message:
                    "context_length_exceeded: This model's maximum context length is 4096 tokens."
                        .to_string(),
                classification: Some(ProviderFailureClassification::ContextOverflow),
                retryable: None,
                provider_metadata: None,
            }],
        );
    }

    #[test]
    fn finish_reason_mapping() {
        let finish = |details: Option<&str>| {
            let response = match details {
                Some(reason) => json!({"incomplete_details": {"reason": reason}}),
                None => json!({"incomplete_details": null}),
            };
            map_finish_reason(&json!({"response": response}), false)
        };
        assert_eq!(finish(None), FinishReason::Stop);
        assert_eq!(finish(Some("max_output_tokens")), FinishReason::Length);
        assert_eq!(finish(Some("content_filter")), FinishReason::ContentFilter);
        assert_eq!(finish(Some("other")), FinishReason::Unknown);
        assert_eq!(
            map_finish_reason(
                &json!({"response": {"incomplete_details": {"reason": "other"}}}),
                true
            ),
            FinishReason::ToolCalls
        );
    }

    #[test]
    fn decode_frame_requires_a_type_and_terminal_matches_the_three_finishes() {
        let protocol = OpenAiResponses;
        let event = protocol
            .decode_frame(&json!({"type": "response.created"}))
            .unwrap();
        assert_eq!(event, Some(json!({"type": "response.created"})));
        assert!(protocol.decode_frame(&json!({"kind": "unknown"})).is_err());

        assert!(protocol.terminal(&json!({"type": "response.completed"})));
        assert!(protocol.terminal(&json!({"type": "response.incomplete"})));
        assert!(protocol.terminal(&json!({"type": "response.failed"})));
        assert!(!protocol.terminal(&json!({"type": "response.created"})));
    }
}
