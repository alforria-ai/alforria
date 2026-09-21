//! Gemini `generateContent` protocol (TS `protocols/gemini.ts`).
//!
//! Gemini embeds the model id in the request URL and pins SSE framing at the
//! URL level (`?alt=sse`). The protocol owns request-body construction
//! (`contents` / `systemInstruction` / `tools` / `toolConfig` /
//! `generationConfig`) and the streaming state machine that turns
//! `candidates[0]` parts into the neutral [`LlmEvent`] stream.
//!
//! Tool-schema conversion applies the sanitize-then-project Gemini dialect on
//! every declaration (`utils/gemini_tool_schema`); the model's `toolSchema`
//! compatibility projection runs first, mirroring the TS
//! `ToolSchemaProjection.modelCompatibility` composition in `lowerTool`.

#![allow(clippy::result_large_err)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

use opencode_schema::llm::ToolContent;

use crate::protocols::shared;
use crate::protocols::utils::gemini_tool_schema;
use crate::protocols::utils::lifecycle;
use crate::protocols::utils::lifecycle::FinishInput;
use crate::protocols::utils::tool_schema::ToolSchemaProjection;
use crate::route::auth::Auth;
use crate::route::client::{RouteDefaults, RouteHandle};
use crate::route::endpoint::{Endpoint, EndpointPart};
use crate::route::framing::Framing;
use crate::route::protocol::Protocol;
use crate::schema::errors::LlmError;
use crate::schema::events::{LlmEvent, Usage};
use crate::schema::ids::{FinishReason, JsonMap, MessageRole, ProviderMetadata};
use crate::schema::messages::{
    ContentPart, LlmRequest, ToolChoice, ToolChoiceType, ToolResultValue,
};
use crate::schema::options::ModelToolSchemaCompatibility;

/// Stable protocol id (TS `ADAPTER`).
const ADAPTER: &str = "gemini";
/// Route name used in user-facing lowering errors (TS passes `"Gemini"`).
const ROUTE: &str = "Gemini";

const REASONING_ID: &str = "reasoning-0";
const TEXT_ID: &str = "text-0";

pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

// =========================================================================
// Request body schema (native casing, TS `GeminiBody`)
// =========================================================================

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiTextPart {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    thought: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thought_signature: Option<String>,
}

impl GeminiTextPart {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            thought: None,
            thought_signature: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiInlineData {
    mime_type: String,
    data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiInlineDataPart {
    inline_data: GeminiInlineData,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct GeminiFunctionCall {
    name: String,
    args: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionCallPart {
    function_call: GeminiFunctionCall,
    #[serde(skip_serializing_if = "Option::is_none")]
    thought_signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct GeminiFunctionResponse {
    name: String,
    response: GeminiFunctionResponsePayload,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct GeminiFunctionResponsePayload {
    name: String,
    content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionResponsePart {
    function_response: GeminiFunctionResponse,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
enum GeminiPart {
    Text(GeminiTextPart),
    InlineData(GeminiInlineDataPart),
    FunctionCall(GeminiFunctionCallPart),
    FunctionResponse(GeminiFunctionResponsePart),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

impl GeminiContent {
    fn new(role: &str, parts: Vec<GeminiPart>) -> Self {
        Self {
            role: role.to_string(),
            parts,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct GeminiSystemInstruction {
    parts: Vec<GeminiSystemText>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct GeminiSystemText {
    text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<JsonMap>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiTool {
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
enum GeminiFunctionCallingMode {
    Auto,
    None,
    Any,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionCallingConfig {
    mode: GeminiFunctionCallingMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_function_names: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiToolConfig {
    function_calling_config: GeminiFunctionCallingConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiThinkingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_budget: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include_thoughts: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_config: Option<GeminiThinkingConfig>,
}

impl GeminiGenerationConfig {
    fn is_empty(&self) -> bool {
        self.max_output_tokens.is_none()
            && self.temperature.is_none()
            && self.top_p.is_none()
            && self.top_k.is_none()
            && self.stop_sequences.is_none()
            && self.thinking_config.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiBody {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<GeminiToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

// =========================================================================
// Stream event schema (TS `GeminiUsage` / `GeminiEvent`)
// =========================================================================

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached_content_token_count: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thoughts_token_count: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt_token_count: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    candidates_token_count: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total_token_count: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiPartWire {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
    #[serde(default)]
    thought_signature: Option<String>,
    #[serde(default)]
    function_call: Option<GeminiFunctionCallWire>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct GeminiFunctionCallWire {
    name: String,
    // `args: Schema.Unknown` (gemini.ts:47) — the field must be present.
    args: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct GeminiContentWire {
    role: String,
    parts: Vec<GeminiPartWire>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidateWire {
    #[serde(default)]
    content: Option<GeminiContentWire>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiEventWire {
    #[serde(default)]
    candidates: Vec<GeminiCandidateWire>,
    #[serde(default)]
    usage_metadata: Option<GeminiUsage>,
}

// =========================================================================
// Parser state (TS `ParserState`)
// =========================================================================

/// Accumulator threaded through the Gemini streaming state machine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    finish_reason: Option<String>,
    has_tool_calls: bool,
    next_tool_call_id: usize,
    usage: Option<Usage>,
    lifecycle: lifecycle::State,
    reasoning_signature: Option<String>,
}

// =========================================================================
// Request lowering (TS `lowerTool` … `fromRequest`)
// =========================================================================

fn lower_tool(
    tool: &crate::schema::messages::ToolDefinition,
    compatibility: Option<ModelToolSchemaCompatibility>,
) -> GeminiFunctionDeclaration {
    let schema = ToolSchemaProjection::model_compatibility(&tool.input_schema, compatibility);
    GeminiFunctionDeclaration {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters: gemini_tool_schema::convert(&Value::Object(schema))
            .and_then(|projected| projected.as_object().cloned()),
    }
}

fn lower_tool_config(tool_choice: &ToolChoice) -> Result<GeminiToolConfig, LlmError> {
    let (mode, allowed_function_names) = match shared::match_tool_choice(tool_choice, ROUTE)? {
        shared::MatchedToolChoice::Auto => (GeminiFunctionCallingMode::Auto, None),
        shared::MatchedToolChoice::None => (GeminiFunctionCallingMode::None, None),
        shared::MatchedToolChoice::Required => (GeminiFunctionCallingMode::Any, None),
        shared::MatchedToolChoice::Tool(name) => (GeminiFunctionCallingMode::Any, Some(vec![name])),
    };
    Ok(GeminiToolConfig {
        function_calling_config: GeminiFunctionCallingConfig {
            mode,
            allowed_function_names,
        },
    })
}

fn lower_user_part(part: &ContentPart) -> Result<GeminiPart, LlmError> {
    match part {
        ContentPart::Text { text, .. } => Ok(GeminiPart::Text(GeminiTextPart::text(text))),
        ContentPart::Media { .. } => {
            let media = shared::validate_media(ROUTE, part, &shared::MEDIA_MIMES)?;
            Ok(GeminiPart::InlineData(GeminiInlineDataPart {
                inline_data: GeminiInlineData {
                    mime_type: media.mime,
                    data: media.base64,
                },
            }))
        }
        _ => Err(shared::unsupported_content(
            ROUTE,
            &MessageRole::User,
            &["text", "media"],
        )),
    }
}

fn thought_signature(provider_metadata: &Option<ProviderMetadata>) -> Option<String> {
    let metadata = provider_metadata.as_ref()?;
    let google = metadata.get("google")?;
    google
        .get("thoughtSignature")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn lower_messages(request: &LlmRequest) -> Result<Vec<GeminiContent>, LlmError> {
    let mut contents: Vec<GeminiContent> = Vec::new();

    for message in &request.messages {
        match message.role {
            MessageRole::System => {
                let ContentPart::Text { text: wrapped, .. } =
                    shared::wrapped_system_update(ROUTE, message)?
                else {
                    return Err(shared::invalid_request(
                        "wrapped system update must be text content",
                    ));
                };
                match contents.last_mut() {
                    Some(previous) if previous.role == "user" => {
                        previous
                            .parts
                            .push(GeminiPart::Text(GeminiTextPart::text(wrapped)));
                    }
                    _ => {
                        contents.push(GeminiContent::new(
                            "user",
                            vec![GeminiPart::Text(GeminiTextPart::text(wrapped))],
                        ));
                    }
                }
            }
            MessageRole::User => {
                let mut parts = Vec::new();
                for part in &message.content {
                    if !shared::supports_content(part, &["text", "media"]) {
                        return Err(shared::unsupported_content(
                            ROUTE,
                            &message.role,
                            &["text", "media"],
                        ));
                    }
                    parts.push(lower_user_part(part)?);
                }
                contents.push(GeminiContent::new("user", parts));
            }
            MessageRole::Assistant => {
                let mut parts = Vec::new();
                for part in &message.content {
                    if !shared::supports_content(part, &["text", "reasoning", "tool-call"]) {
                        return Err(shared::unsupported_content(
                            ROUTE,
                            &message.role,
                            &["text", "reasoning", "tool-call"],
                        ));
                    }
                    match part {
                        ContentPart::Text { text, .. } => {
                            parts.push(GeminiPart::Text(GeminiTextPart::text(text)));
                        }
                        ContentPart::Reasoning {
                            text,
                            provider_metadata,
                            ..
                        } => {
                            parts.push(GeminiPart::Text(GeminiTextPart {
                                text: text.clone(),
                                thought: Some(true),
                                thought_signature: thought_signature(provider_metadata),
                            }));
                        }
                        ContentPart::ToolCall {
                            name,
                            input,
                            provider_metadata,
                            ..
                        } => {
                            parts.push(GeminiPart::FunctionCall(GeminiFunctionCallPart {
                                function_call: GeminiFunctionCall {
                                    name: name.clone(),
                                    args: input.clone(),
                                },
                                thought_signature: thought_signature(provider_metadata),
                            }));
                        }
                        _ => {}
                    }
                }
                contents.push(GeminiContent::new("model", parts));
            }
            MessageRole::Tool => {
                let mut parts = Vec::new();
                for part in &message.content {
                    if !shared::supports_content(part, &["tool-result"]) {
                        return Err(shared::unsupported_content(
                            ROUTE,
                            &message.role,
                            &["tool-result"],
                        ));
                    }
                    let ContentPart::ToolResult { name, result, .. } = part else {
                        continue;
                    };
                    let response = match result {
                        ToolResultValue::Content { value } => {
                            let text = value
                                .iter()
                                .filter_map(|item| match item {
                                    ToolContent::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            parts.push(GeminiPart::FunctionResponse(GeminiFunctionResponsePart {
                                function_response: GeminiFunctionResponse {
                                    name: name.clone(),
                                    response: GeminiFunctionResponsePayload {
                                        name: name.clone(),
                                        content: text,
                                    },
                                },
                            }));
                            for item in value {
                                if let ToolContent::File { .. } = item {
                                    let media = shared::validate_tool_file(
                                        ROUTE,
                                        item,
                                        &shared::MEDIA_MIMES,
                                    )?;
                                    parts.push(GeminiPart::InlineData(GeminiInlineDataPart {
                                        inline_data: GeminiInlineData {
                                            mime_type: media.mime,
                                            data: media.base64,
                                        },
                                    }));
                                }
                            }
                            continue;
                        }
                        _ => GeminiFunctionResponsePayload {
                            name: name.clone(),
                            content: shared::tool_result_text(part),
                        },
                    };
                    parts.push(GeminiPart::FunctionResponse(GeminiFunctionResponsePart {
                        function_response: GeminiFunctionResponse {
                            name: name.clone(),
                            response,
                        },
                    }));
                }
                contents.push(GeminiContent::new("user", parts));
            }
        }
    }

    Ok(contents)
}

fn thinking_config(request: &LlmRequest) -> Option<GeminiThinkingConfig> {
    let options = request.provider_options.as_ref()?;
    let gemini = options.get(ADAPTER)?;
    let value = gemini.get("thinkingConfig")?;
    if !value.is_object() {
        return None;
    }
    let config = GeminiThinkingConfig {
        thinking_budget: value.get("thinkingBudget").and_then(Value::as_f64),
        include_thoughts: value.get("includeThoughts").and_then(Value::as_bool),
    };
    (config.thinking_budget.is_some() || config.include_thoughts.is_some()).then_some(config)
}

fn from_request(request: &LlmRequest) -> Result<GeminiBody, LlmError> {
    let tools_enabled = !request.tools.is_empty()
        && request.tool_choice.as_ref().map(|choice| choice.r#type) != Some(ToolChoiceType::None);
    let generation = request.generation.as_ref();
    let generation_config = GeminiGenerationConfig {
        max_output_tokens: generation.and_then(|options| options.max_tokens),
        temperature: generation.and_then(|options| options.temperature),
        top_p: generation.and_then(|options| options.top_p),
        top_k: generation.and_then(|options| options.top_k),
        stop_sequences: generation.and_then(|options| options.stop.clone()),
        thinking_config: thinking_config(request),
    };
    let compatibility = request
        .model
        .compatibility
        .as_ref()
        .and_then(|compatibility| compatibility.tool_schema);

    Ok(GeminiBody {
        contents: lower_messages(request)?,
        system_instruction: (!request.system.is_empty()).then(|| GeminiSystemInstruction {
            parts: vec![GeminiSystemText {
                text: shared::join_text(request.system.iter().map(|part| part.text.as_str())),
            }],
        }),
        tools: tools_enabled.then(|| {
            vec![GeminiTool {
                function_declarations: request
                    .tools
                    .iter()
                    .map(|tool| lower_tool(tool, compatibility))
                    .collect(),
            }]
        }),
        tool_config: match &request.tool_choice {
            Some(choice) if tools_enabled => Some(lower_tool_config(choice)?),
            _ => None,
        },
        generation_config: (!generation_config.is_empty()).then_some(generation_config),
    })
}

// =========================================================================
// Stream parsing (TS `mapUsage` / `mapFinishReason` / `step` / `finish`)
// =========================================================================

fn map_usage(usage: &GeminiUsage) -> Usage {
    let cached = usage.cached_content_token_count;
    // `candidatesTokenCount` is visible-only; sum with thoughts to produce
    // the inclusive `outputTokens` the contract expects.
    let output_tokens = usage
        .candidates_token_count
        .map(|candidates| candidates + usage.thoughts_token_count.unwrap_or(0.0));
    Usage {
        input_tokens: usage.prompt_token_count,
        output_tokens,
        non_cached_input_tokens: shared::subtract_tokens(usage.prompt_token_count, cached),
        cache_read_input_tokens: cached,
        cache_write_input_tokens: None,
        reasoning_tokens: usage.thoughts_token_count,
        total_tokens: shared::total_tokens(
            usage.prompt_token_count,
            output_tokens,
            usage.total_token_count,
        ),
        provider_metadata: Some(google_metadata(
            serde_json::to_value(usage).unwrap_or(Value::Null),
        )),
    }
}

fn map_finish_reason(finish_reason: Option<&str>, has_tool_calls: bool) -> FinishReason {
    match finish_reason {
        Some("STOP") => {
            if has_tool_calls {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            }
        }
        Some("MAX_TOKENS") => FinishReason::Length,
        Some(
            "IMAGE_SAFETY" | "RECITATION" | "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII",
        ) => FinishReason::ContentFilter,
        Some("MALFORMED_FUNCTION_CALL") => FinishReason::Error,
        _ => FinishReason::Unknown,
    }
}

fn google_metadata(value: Value) -> ProviderMetadata {
    let mut metadata = ProviderMetadata::new();
    if let Some(record) = value.as_object() {
        metadata.insert("google".to_string(), record.clone());
    }
    metadata
}

fn signature_metadata(signature: &str) -> ProviderMetadata {
    google_metadata(serde_json::json!({ "thoughtSignature": signature }))
}

fn signature_from(state: &State) -> Option<ProviderMetadata> {
    state
        .reasoning_signature
        .as_deref()
        .filter(|signature| !signature.is_empty())
        .map(signature_metadata)
}

fn step(mut state: State, event: &GeminiEventWire) -> (State, Vec<LlmEvent>) {
    if let Some(usage) = &event.usage_metadata {
        state.usage = Some(map_usage(usage));
    }

    let Some(candidate) = event.candidates.first() else {
        return (state, Vec::new());
    };

    let Some(content) = &candidate.content else {
        if candidate.finish_reason.is_some() {
            state.finish_reason.clone_from(&candidate.finish_reason);
        }
        return (state, Vec::new());
    };

    let mut events = Vec::new();
    for part in &content.parts {
        let thought = part.thought.unwrap_or(false);
        let part_signature = part
            .thought_signature
            .as_deref()
            .filter(|signature| !signature.is_empty());
        if part_signature.is_some() && thought {
            state
                .reasoning_signature
                .clone_from(&part.thought_signature);
        }
        if part.text.as_deref().is_some_and(|text| !text.is_empty()) {
            let text = part.text.clone().unwrap_or_default();
            if thought {
                state.lifecycle = lifecycle::reasoning_delta(
                    state.lifecycle,
                    &mut events,
                    REASONING_ID,
                    &text,
                    part_signature.map(signature_metadata),
                );
                continue;
            }
            let metadata = signature_from(&state);
            state.lifecycle =
                lifecycle::reasoning_end(state.lifecycle, &mut events, REASONING_ID, metadata);
            state.lifecycle = lifecycle::text_delta(state.lifecycle, &mut events, TEXT_ID, &text);
            continue;
        }

        if let Some(function_call) = &part.function_call {
            let id = format!("tool_{}", state.next_tool_call_id);
            state.next_tool_call_id += 1;
            let metadata = signature_from(&state);
            state.lifecycle =
                lifecycle::reasoning_end(state.lifecycle, &mut events, REASONING_ID, metadata);
            state.lifecycle = lifecycle::step_start(state.lifecycle, &mut events);
            events.push(LlmEvent::ToolCall {
                id,
                name: function_call.name.clone(),
                input: function_call.args.clone(),
                provider_executed: None,
                provider_metadata: part_signature.map(signature_metadata),
            });
            state.has_tool_calls = true;
        }
    }

    if candidate.finish_reason.is_some() {
        state.finish_reason.clone_from(&candidate.finish_reason);
    }
    (state, events)
}

fn finish(state: State) -> Vec<LlmEvent> {
    let State {
        finish_reason,
        has_tool_calls,
        usage,
        lifecycle: lifecycle_state,
        reasoning_signature,
        ..
    } = state;
    let has_reason = finish_reason
        .as_deref()
        .is_some_and(|reason| !reason.is_empty());
    if !has_reason && usage.is_none() {
        return Vec::new();
    }

    let mut events = Vec::new();
    let lifecycle_state = match reasoning_signature
        .as_deref()
        .filter(|signature| !signature.is_empty())
    {
        Some(signature) => lifecycle::reasoning_end(
            lifecycle_state,
            &mut events,
            REASONING_ID,
            Some(signature_metadata(signature)),
        ),
        _ => lifecycle_state,
    };
    lifecycle::finish(
        lifecycle_state,
        &mut events,
        FinishInput {
            reason: map_finish_reason(finish_reason.as_deref(), has_tool_calls),
            usage,
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
// Protocol and Gemini route
// =========================================================================

/// The Gemini protocol — request body construction and the streaming-event
/// state machine. Used by Google AI Studio Gemini and Vertex Gemini.
pub struct Gemini;

impl Protocol for Gemini {
    const ID: &'static str = ADAPTER;
    type State = State;

    fn lower_body(&self, request: &LlmRequest) -> Result<Value, LlmError> {
        serde_json::to_value(from_request(request)?)
            .map_err(|error| shared::invalid_request(format!("Gemini.lowerBody: {error}")))
    }

    fn decode_frame(&self, frame: &Value) -> Result<Option<Value>, LlmError> {
        let event: GeminiEventWire =
            serde_json::from_value(frame.clone()).map_err(|_| invalid_stream_event(frame))?;
        for candidate in &event.candidates {
            if let Some(content) = &candidate.content {
                if !matches!(content.role.as_str(), "user" | "model") {
                    return Err(invalid_stream_event(frame));
                }
            }
        }
        Ok(Some(frame.clone()))
    }

    fn initial(&self, _request: &LlmRequest) -> State {
        State::default()
    }

    fn step(&self, state: State, event: &Value) -> Result<(State, Vec<LlmEvent>), LlmError> {
        let event: GeminiEventWire =
            serde_json::from_value(event.clone()).map_err(|_| invalid_stream_event(event))?;
        Ok(step(state, &event))
    }

    fn on_halt(&self, state: State) -> Vec<LlmEvent> {
        finish(state)
    }
}

/// Gemini's path embeds the model id and pins SSE framing at the URL level.
pub fn endpoint() -> Endpoint {
    Endpoint {
        base_url: Some(DEFAULT_BASE_URL.to_string()),
        path: EndpointPart::Function(|input| {
            format!(
                "/models/{}:streamGenerateContent?alt=sse",
                input.request.model.id
            )
        }),
        query: None,
    }
}

/// The canonical Gemini route handle (TS `Gemini.route`).
pub fn route_handle() -> RouteHandle {
    RouteHandle {
        id: ADAPTER.to_string(),
        protocol_id: ADAPTER.to_string(),
        endpoint: endpoint(),
        auth: Auth::none(),
        framing: Framing::Sse,
        defaults: RouteDefaults::default(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]

    use super::*;
    use crate::schema::messages::{ModelRef, ToolDefinition, ToolResultInput, ToolResultType};
    use crate::schema::options::{GenerationOptions, SystemPart, SystemPartType};
    use serde_json::json;
    use std::sync::Arc;

    use crate::schema::messages::Message;

    fn model_ref() -> ModelRef {
        ModelRef::new("gemini-2.5-flash", "google", Arc::new(RouteHandle::empty()))
    }

    fn request() -> LlmRequest {
        let mut value = LlmRequest::new(model_ref());
        value.system = vec![system_part("You are concise.")];
        value.messages = vec![Message::user("Say hello.")];
        value.generation = Some(GenerationOptions {
            max_tokens: Some(20.0),
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

    fn system_part(text: &str) -> SystemPart {
        SystemPart {
            r#type: SystemPartType::Text,
            text: text.to_string(),
            cache: None,
            metadata: None,
        }
    }

    fn lower(request: &LlmRequest) -> Value {
        Gemini.lower_body(request).unwrap()
    }

    /// Numeric-tolerant JSON equality (spec §2.3): Rust lowering emits
    /// `"temperature": 0.0` where the recording has `0`.
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
        let mut state = Gemini.initial(&request);
        let mut events = Vec::new();
        for frame in &frames {
            let decoded = Gemini.decode_frame(frame).unwrap().unwrap();
            let (next, mut emitted) = Gemini.step(state, &decoded).unwrap();
            state = next;
            events.append(&mut emitted);
        }
        events.extend(Gemini.on_halt(state));
        events
    }

    fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "lookup".to_string(),
            description: "Lookup data".to_string(),
            input_schema: json!({"type": "object", "properties": {"query": {"type": "string"}}})
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
        let mut value = request();
        value.messages = vec![Message::user("Reply with exactly: Hello!")];
        value.generation = Some(GenerationOptions {
            max_tokens: Some(80.0),
            temperature: Some(0.0),
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            seed: None,
            stop: None,
        });
        let body = lower(&value);
        assert_json_equal(
            &json!({
                "contents": [{"role": "user", "parts": [{"text": "Reply with exactly: Hello!"}]}],
                "systemInstruction": {"parts": [{"text": "You are concise."}]},
                "generationConfig": {"maxOutputTokens": 80, "temperature": 0},
            }),
            &body,
        );
    }

    #[test]
    fn lowers_chronological_system_updates_to_wrapped_user_text() {
        let mut value = request();
        value.system = Vec::new();
        value.generation = None;
        value.messages = vec![
            Message::user("Before."),
            Message::system("Update."),
            Message::assistant("After."),
        ];
        assert_json_equal(
            &json!({
                "contents": [
                    {
                        "role": "user",
                        "parts": [
                            {"text": "Before."},
                            {"text": "<system-update>\nUpdate.\n</system-update>"},
                        ],
                    },
                    {"role": "model", "parts": [{"text": "After."}]},
                ],
            }),
            &lower(&value),
        );
    }

    #[test]
    fn prepares_multimodal_user_input_and_tool_history() {
        let mut value = request();
        value.system = Vec::new();
        value.generation = None;
        value.tools = vec![tool_definition()];
        value.tool_choice = Some(ToolChoice::named("lookup"));
        value.messages = vec![
            Message::user(vec![
                ContentPart::text("What is in this image?"),
                ContentPart::media("image/png", "AAECAw=="),
            ]),
            Message::assistant(vec![ContentPart::tool_call(
                "call_1",
                "lookup",
                json!({"query": "weather"}),
            )]),
            Message::tool(ContentPart::tool_result(ToolResultInput {
                id: "call_1".to_string(),
                name: "lookup".to_string(),
                result: json!({"forecast": "sunny"}),
                ..ToolResultInput::default()
            })),
        ];
        assert_json_equal(
            &json!({
                "contents": [
                    {
                        "role": "user",
                        "parts": [
                            {"text": "What is in this image?"},
                            {"inlineData": {"mimeType": "image/png", "data": "AAECAw=="}},
                        ],
                    },
                    {
                        "role": "model",
                        "parts": [{"functionCall": {"name": "lookup", "args": {"query": "weather"}}}],
                    },
                    {
                        "role": "user",
                        "parts": [
                            {
                                "functionResponse": {
                                    "name": "lookup",
                                    "response": {"name": "lookup", "content": "{\"forecast\":\"sunny\"}"},
                                },
                            },
                        ],
                    },
                ],
                "tools": [
                    {
                        "functionDeclarations": [
                            {
                                "name": "lookup",
                                "description": "Lookup data",
                                "parameters": {
                                    "type": "object",
                                    "properties": {"query": {"type": "string"}},
                                },
                            },
                        ],
                    },
                ],
                "toolConfig": {
                    "functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["lookup"]}
                },
            }),
            &lower(&value),
        );
    }

    #[test]
    fn continues_image_tool_results_as_inline_data() {
        let mut value = request();
        value.system = Vec::new();
        value.generation = None;
        value.messages = vec![
            Message::assistant(vec![ContentPart::tool_call(
                "call_image",
                "read",
                json!({"path": "pixel.png"}),
            )]),
            Message::tool(ContentPart::tool_result(ToolResultInput {
                id: "call_image".to_string(),
                name: "read".to_string(),
                result: json!([
                    {"type": "text", "text": "Image read successfully"},
                    {
                        "type": "file",
                        "uri": "data:image/png;base64,AAECAw==",
                        "mime": "image/png",
                        "name": "pixel.png",
                    },
                ]),
                result_type: Some(ToolResultType::Content),
                ..ToolResultInput::default()
            })),
        ];
        assert_json_equal(
            &json!({
                "contents": [
                    {
                        "role": "model",
                        "parts": [{"functionCall": {"name": "read", "args": {"path": "pixel.png"}}}],
                    },
                    {
                        "role": "user",
                        "parts": [
                            {
                                "functionResponse": {
                                    "name": "read",
                                    "response": {"name": "read", "content": "Image read successfully"},
                                },
                            },
                            {"inlineData": {"mimeType": "image/png", "data": "AAECAw=="}},
                        ],
                    },
                ],
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
        assert_json_equal(
            &json!({
                "contents": [{"role": "user", "parts": [{"text": "Say hello."}]}],
                "systemInstruction": {"parts": [{"text": "You are concise."}]},
                "generationConfig": {"maxOutputTokens": 20, "temperature": 0},
            }),
            &lower(&value),
        );
    }

    #[test]
    fn sanitizes_tool_schemas_for_gemini() {
        let mut value = request();
        value.system = Vec::new();
        value.generation = None;
        value.tools = vec![ToolDefinition {
            name: "lookup".to_string(),
            description: "Lookup data".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["status", "missing"],
                "properties": {
                    "status": {"type": "integer", "enum": [1, 2]},
                    "tags": {"type": "array"},
                    "name": {"type": "string", "properties": {"ignored": {"type": "string"}}, "required": ["ignored"]},
                },
            })
            .as_object()
            .unwrap()
            .clone(),
            output_schema: None,
            cache: None,
            metadata: None,
            native: None,
        }];
        let body = lower(&value);
        assert_json_equal(
            &json!({
                "type": "object",
                "required": ["status"],
                "properties": {
                    "status": {"type": "string", "enum": ["1", "2"]},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "name": {"type": "string"},
                },
            }),
            &body["tools"][0]["functionDeclarations"][0]["parameters"],
        );
    }

    #[test]
    fn lowers_thinking_config_from_provider_options() {
        let mut value = request();
        value.provider_options = Some(
            json!({
                "gemini": {"thinkingConfig": {"thinkingBudget": 0, "includeThoughts": true}},
            })
            .as_object()
            .unwrap()
            .iter()
            .map(|(key, entry)| (key.clone(), entry.as_object().cloned().unwrap_or_default()))
            .collect(),
        );
        let body = lower(&value);
        assert_json_equal(
            &json!({
                "maxOutputTokens": 20,
                "temperature": 0,
                "thinkingConfig": {"thinkingBudget": 0, "includeThoughts": true},
            }),
            &body["generationConfig"],
        );
    }

    #[test]
    fn parses_text_reasoning_and_usage_streams() {
        let events = stream(vec![
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "thinking", "thought": true}]}}]}),
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Hello"}]}}]}),
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "!"}]}, "finishReason": "STOP"}]}),
            json!({"usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2, "totalTokenCount": 7, "thoughtsTokenCount": 1, "cachedContentTokenCount": 1}}),
        ]);

        let usage = expected_usage(
            5.0,
            Some(3.0),
            Some(4.0),
            Some(1.0),
            Some(1.0),
            Some(7.0),
            json!({
                "cachedContentTokenCount": 1,
                "thoughtsTokenCount": 1,
                "promptTokenCount": 5,
                "candidatesTokenCount": 2,
                "totalTokenCount": 7,
            }),
        );
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
                    text: "thinking".to_string(),
                    provider_metadata: None,
                },
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
                LlmEvent::TextDelta {
                    id: "text-0".to_string(),
                    text: "!".to_string(),
                    provider_metadata: None,
                },
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
                    usage: Some(usage),
                    provider_metadata: None,
                },
            ]
        );
    }

    /// Gemini usage numbers are TS `Number` (f64) on this side, so the raw
    /// payload serializes `2.0` where `json!` literals are integers.
    fn float_numbers(value: Value) -> Value {
        match value {
            Value::Number(number) => {
                match serde_json::Number::from_f64(number.as_f64().unwrap_or(0.0)) {
                    Some(number) => Value::Number(number),
                    None => Value::Number(number),
                }
            }
            Value::Array(items) => Value::Array(items.into_iter().map(float_numbers).collect()),
            Value::Object(record) => Value::Object(
                record
                    .into_iter()
                    .map(|(key, item)| (key, float_numbers(item)))
                    .collect(),
            ),
            other => other,
        }
    }

    fn expected_usage(
        input: f64,
        output: Option<f64>,
        non_cached: Option<f64>,
        cache_read: Option<f64>,
        reasoning: Option<f64>,
        total: Option<f64>,
        google: Value,
    ) -> Usage {
        Usage {
            input_tokens: Some(input),
            output_tokens: output,
            non_cached_input_tokens: non_cached,
            cache_read_input_tokens: cache_read,
            cache_write_input_tokens: None,
            reasoning_tokens: reasoning,
            total_tokens: total,
            provider_metadata: Some(
                [(
                    "google".to_string(),
                    float_numbers(google)
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                )]
                .into_iter()
                .collect(),
            ),
        }
    }

    #[test]
    fn preserves_thought_signature_for_reasoning_and_tool_call_continuation() {
        let events = stream(vec![json!({
            "candidates": [
                {
                    "content": {
                        "role": "model",
                        "parts": [
                            {"text": "thinking", "thought": true},
                            {"text": "", "thought": true, "thoughtSignature": "thought_sig"},
                            {"functionCall": {"name": "lookup", "args": {"query": "weather"}}, "thoughtSignature": "tool_sig"},
                        ],
                    },
                    "finishReason": "STOP",
                },
            ],
        })]);
        let reasoning_start = events
            .iter()
            .find(|event| matches!(event, LlmEvent::ReasoningStart { .. }))
            .unwrap();
        assert_eq!(
            reasoning_start,
            &LlmEvent::ReasoningStart {
                id: "reasoning-0".to_string(),
                provider_metadata: None,
            },
        );
        let reasoning_end = events
            .iter()
            .find(|event| matches!(event, LlmEvent::ReasoningEnd { .. }))
            .unwrap();
        assert_eq!(
            reasoning_end,
            &LlmEvent::ReasoningEnd {
                id: "reasoning-0".to_string(),
                provider_metadata: Some(signature_metadata("thought_sig")),
            },
        );
        let tool_call = events
            .iter()
            .find(|event| matches!(event, LlmEvent::ToolCall { .. }))
            .unwrap();
        assert_eq!(
            tool_call,
            &LlmEvent::ToolCall {
                id: "tool_0".to_string(),
                name: "lookup".to_string(),
                input: json!({"query": "weather"}),
                provider_executed: None,
                provider_metadata: Some(signature_metadata("tool_sig")),
            },
        );
        assert!(
            events
                .iter()
                .position(|event| matches!(event, LlmEvent::ReasoningEnd { .. }))
                .unwrap()
                < events
                    .iter()
                    .position(|event| matches!(event, LlmEvent::ToolCall { .. }))
                    .unwrap()
        );

        let mut value = request();
        value.system = Vec::new();
        value.generation = None;
        value.messages = vec![Message::assistant(vec![
            ContentPart::Reasoning {
                text: "thinking".to_string(),
                encrypted: None,
                metadata: None,
                provider_metadata: Some(signature_metadata("thought_sig")),
            },
            ContentPart::ToolCall {
                id: "tool_0".to_string(),
                name: "lookup".to_string(),
                input: json!({"query": "weather"}),
                provider_executed: None,
                metadata: None,
                provider_metadata: Some(signature_metadata("tool_sig")),
            },
        ])];
        assert_json_equal(
            &json!({
                "contents": [
                    {
                        "role": "model",
                        "parts": [
                            {"text": "thinking", "thought": true, "thoughtSignature": "thought_sig"},
                            {"functionCall": {"name": "lookup", "args": {"query": "weather"}}, "thoughtSignature": "tool_sig"},
                        ],
                    },
                ],
            }),
            &lower(&value),
        );
    }

    #[test]
    fn emits_streamed_tool_calls_and_maps_finish_reason() {
        let events = stream(vec![json!({
            "candidates": [
                {
                    "content": {"role": "model", "parts": [{"functionCall": {"name": "lookup", "args": {"query": "weather"}}}]},
                    "finishReason": "STOP",
                },
            ],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 1},
        })]);
        let usage = expected_usage(
            5.0,
            Some(1.0),
            Some(5.0),
            None,
            None,
            Some(6.0),
            json!({"promptTokenCount": 5, "candidatesTokenCount": 1}),
        );
        assert_eq!(
            events,
            vec![
                LlmEvent::StepStart { index: 0.0 },
                LlmEvent::ToolCall {
                    id: "tool_0".to_string(),
                    name: "lookup".to_string(),
                    input: json!({"query": "weather"}),
                    provider_executed: None,
                    provider_metadata: None,
                },
                LlmEvent::StepFinish {
                    index: 0.0,
                    reason: FinishReason::ToolCalls,
                    usage: Some(usage.clone()),
                    provider_metadata: None,
                },
                LlmEvent::Finish {
                    reason: FinishReason::ToolCalls,
                    usage: Some(usage),
                    provider_metadata: None,
                },
            ]
        );
    }

    #[test]
    fn assigns_unique_ids_to_multiple_streamed_tool_calls() {
        let events = stream(vec![json!({
            "candidates": [
                {
                    "content": {
                        "role": "model",
                        "parts": [
                            {"functionCall": {"name": "lookup", "args": {"query": "weather"}}},
                            {"functionCall": {"name": "lookup", "args": {"query": "news"}}},
                        ],
                    },
                    "finishReason": "STOP",
                },
            ],
        })]);
        let names: Vec<(String, String)> = events
            .iter()
            .filter_map(|event| match event {
                LlmEvent::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            names,
            vec![
                ("tool_0".to_string(), "lookup".to_string()),
                ("tool_1".to_string(), "lookup".to_string()),
            ]
        );
        assert!(matches!(
            events.last(),
            Some(LlmEvent::Finish {
                reason: FinishReason::ToolCalls,
                ..
            })
        ));
    }

    #[test]
    fn maps_length_and_content_filter_finish_reasons() {
        let length = stream(vec![json!({
            "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "MAX_TOKENS"}],
        })]);
        assert_eq!(
            length
                .iter()
                .map(std::mem::discriminant)
                .collect::<Vec<_>>(),
            vec![
                std::mem::discriminant(&LlmEvent::StepStart { index: 0.0 }),
                std::mem::discriminant(&LlmEvent::StepFinish {
                    index: 0.0,
                    reason: FinishReason::Length,
                    usage: None,
                    provider_metadata: None,
                }),
                std::mem::discriminant(&LlmEvent::Finish {
                    reason: FinishReason::Length,
                    usage: None,
                    provider_metadata: None,
                }),
            ]
        );
        assert!(matches!(
            length.last(),
            Some(LlmEvent::Finish {
                reason: FinishReason::Length,
                ..
            })
        ));

        let filtered = stream(vec![json!({
            "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": "SAFETY"}],
        })]);
        assert!(matches!(
            filtered.last(),
            Some(LlmEvent::Finish {
                reason: FinishReason::ContentFilter,
                ..
            })
        ));
    }

    #[test]
    fn leaves_total_usage_undefined_when_component_counts_are_missing() {
        let events = stream(vec![json!({"usageMetadata": {"thoughtsTokenCount": 1}})]);
        let Some(LlmEvent::Finish { usage, .. }) = events.last() else {
            panic!("expected a finish event: {events:?}");
        };
        let usage = usage.as_ref().unwrap();
        assert_eq!(usage.reasoning_tokens, Some(1.0));
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.total_tokens, None);
    }

    #[test]
    fn reports_cached_content_token_count_as_cache_read() {
        let events = stream(vec![json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "Hi."}]}, "finishReason": "STOP"}],
            "usageMetadata": {
                "cachedContentTokenCount": 1100,
                "promptTokenCount": 1200,
                "candidatesTokenCount": 2,
                "totalTokenCount": 1202,
            },
        })]);
        let Some(LlmEvent::Finish { usage, .. }) = events.last() else {
            panic!("expected a finish event: {events:?}");
        };
        let usage = usage.as_ref().unwrap();
        assert_eq!(usage.input_tokens, Some(1200.0));
        assert_eq!(usage.non_cached_input_tokens, Some(100.0));
        assert_eq!(usage.cache_read_input_tokens, Some(1100.0));
        assert_eq!(usage.output_tokens, Some(2.0));
        assert_eq!(usage.total_tokens, Some(1202.0));
    }

    #[test]
    fn decodes_stream_usage_like_the_streams_text_recording() {
        let events = stream(vec![json!({
            "candidates": [
                {
                    "content": {"parts": [{"text": "Hello!"}], "role": "model"},
                    "finishReason": "STOP",
                    "index": 0,
                },
            ],
            "usageMetadata": {
                "promptTokenCount": 11,
                "candidatesTokenCount": 2,
                "totalTokenCount": 29,
                "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 11}],
                "thoughtsTokenCount": 16,
            },
            "modelVersion": "gemini-2.5-flash",
        })]);
        let Some(LlmEvent::Finish { usage, .. }) = events.last() else {
            panic!("expected a finish event: {events:?}");
        };
        let usage = usage.as_ref().unwrap();
        assert_eq!(usage.input_tokens, Some(11.0));
        assert_eq!(usage.non_cached_input_tokens, Some(11.0));
        assert_eq!(usage.output_tokens, Some(18.0));
        assert_eq!(usage.reasoning_tokens, Some(16.0));
        assert_eq!(usage.total_tokens, Some(29.0));
    }

    #[test]
    fn rejects_invalid_stream_events() {
        for invalid in [
            json!({"candidates": "nope"}),
            json!({"candidates": [{"content": {"role": "evil", "parts": []}}]}),
            json!({"usageMetadata": {"promptTokenCount": "many"}}),
        ] {
            assert!(Gemini.decode_frame(&invalid).is_err());
        }
        let valid = json!({"candidates": [{"content": {"role": "model", "parts": []}}]});
        assert_eq!(Gemini.decode_frame(&valid).unwrap(), Some(valid));
    }

    #[test]
    fn on_halt_emits_nothing_without_finish_reason_or_usage() {
        let events = stream(vec![json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "partial"}]}}],
        })]);
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
                    text: "partial".to_string(),
                    provider_metadata: None,
                },
            ]
        );
    }

    #[test]
    fn renders_the_gemini_endpoint_url() {
        let request = request();
        let url = crate::route::endpoint::render(
            &endpoint(),
            &crate::route::endpoint::EndpointInput {
                request: &request,
                body: &json!({}),
            },
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn replays_the_streams_text_recording_fixture() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/llm-recordings/gemini/streams-text.json"
        );
        let recording = serde_json::from_str::<Value>(&std::fs::read_to_string(path).unwrap())
            .expect("frozen fixture must exist");
        let body = recording["interactions"][0]["response"]["body"]
            .as_str()
            .unwrap();
        let frames: Vec<Value> = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
        let events = stream(frames);

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
                LlmEvent::TextEnd {
                    id: "text-0".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::StepFinish {
                    index: 0.0,
                    reason: FinishReason::Stop,
                    usage: Some(expected_usage(
                        11.0,
                        Some(18.0),
                        Some(11.0),
                        None,
                        Some(16.0),
                        Some(29.0),
                        json!({
                            "promptTokenCount": 11,
                            "candidatesTokenCount": 2,
                            "totalTokenCount": 29,
                            "thoughtsTokenCount": 16,
                        }),
                    )),
                    provider_metadata: None,
                },
                LlmEvent::Finish {
                    reason: FinishReason::Stop,
                    usage: Some(expected_usage(
                        11.0,
                        Some(18.0),
                        Some(11.0),
                        None,
                        Some(16.0),
                        Some(29.0),
                        json!({
                            "promptTokenCount": 11,
                            "candidatesTokenCount": 2,
                            "totalTokenCount": 29,
                            "thoughtsTokenCount": 16,
                        }),
                    )),
                    provider_metadata: None,
                },
            ]
        );
    }
}
