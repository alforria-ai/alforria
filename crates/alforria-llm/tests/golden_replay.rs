//! Golden replay harness over `fixtures/llm-recordings/` (spec M2.12, §5).
//!
//! Every recording directory in the fixture tree is replayed here:
//!
//! 1. **Load** the cassette from `$(CARGO_MANIFEST_DIR)/../../fixtures/llm-recordings`.
//! 2. **Declare the driver request** in Rust (mirroring the TS scenario drivers
//!    in `recorded-scenarios.ts` / `provider/*.recorded.test.ts`). Recorded
//!    request bodies are authoritative where the pinned TS drivers drift.
//! 3. **Assert lowering parity** — `Route::compile` is compared against the
//!    recorded request (method, URL, pre-auth headers, body) with
//!    numeric-tolerant JSON equality (spec §2.3).
//! 4. **Replay the response** — SSE text through `Framing::Sse`, the bedrock
//!    base64 bodies through `Framing::AwsEventStream` — folding frames through
//!    `decode_frame` → `terminal` → `step`, then `on_halt` at stream end
//!    (the same pipeline as `Route::stream`).
//! 5. **Assert the golden invariants** (spec §5.4): exact event sequences for
//!    the canonical recordings, the tool-loop invariant set, exact usage
//!    numbers, one `finish` per turn, and tool-call shape.

#![allow(clippy::arc_with_non_send_sync)]

use std::path::Path;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::{json, Value};

use alforria_llm::protocols::anthropic_messages::{self, AnthropicMessages};
use alforria_llm::protocols::bedrock_converse::{self, BedrockConverse};
use alforria_llm::protocols::gemini::{self, Gemini};
use alforria_llm::protocols::openai_chat::{self, OpenAiChat};
use alforria_llm::protocols::openai_compatible_chat;
use alforria_llm::protocols::openai_responses::{self, OpenAiResponses};
use alforria_llm::route::auth::Auth;
use alforria_llm::route::client::Route;
use alforria_llm::route::endpoint::Endpoint;
use alforria_llm::route::framing::Framing;
use alforria_llm::route::protocol::Protocol;
use alforria_llm::schema::events::LlmEvent;
use alforria_llm::schema::events::Usage;
use alforria_llm::schema::ids::{FinishReason, JsonMap};
use alforria_llm::schema::messages::{
    ContentPart, LlmRequest, LlmRequestPatch, Message, ModelRef, ToolChoice, ToolDefinition,
    ToolResultInput, ToolResultType, ToolResultValue,
};
use alforria_llm::schema::options::{
    CacheHint, CacheHintType, CachePolicy, CachePolicyLiteral, CachePolicyMessages,
    CachePolicyObject, GenerationOptions, ProviderOptions, SystemPart, SystemPartType,
};

// =============================================================================
// Cassette plumbing
// =============================================================================

struct Interaction {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
    response_status: u16,
    response_body: String,
    response_body_encoding: Option<String>,
}

fn load_cassette(name: &str) -> Vec<Interaction> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/llm-recordings")
        .join(format!("{name}.json"));
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read cassette {name}: {error}"));
    let root: Value = serde_json::from_str(&raw).unwrap();
    root["interactions"]
        .as_array()
        .unwrap_or_else(|| panic!("cassette {name} has no interactions"))
        .iter()
        .map(|interaction| {
            let request = &interaction["request"];
            let headers = request["headers"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_string()))
                .collect();
            Interaction {
                method: request["method"].as_str().unwrap().to_string(),
                url: request["url"].as_str().unwrap().to_string(),
                headers,
                body: serde_json::from_str(request["body"].as_str().unwrap()).unwrap(),
                response_status: interaction["response"]["status"].as_u64().unwrap() as u16,
                response_body: interaction["response"]["body"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                response_body_encoding: interaction["response"]
                    .get("bodyEncoding")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }
        })
        .collect()
}

// =============================================================================
// Numeric-tolerant JSON equality (spec §2.3): f64 serializes as 20.0 where the
// TS recording has 20. Compare as parsed Values, treating integers and
// equal-valued floats as equal.
// =============================================================================

fn assert_json_equal(path: &str, expected: &Value, actual: &Value) {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            assert_eq!(
                expected.keys().collect::<Vec<_>>(),
                actual.keys().collect::<Vec<_>>(),
                "key mismatch at {path}"
            );
            for (key, value) in expected {
                assert_json_equal(
                    &format!("{path}.{key}"),
                    value,
                    actual.get(key).unwrap_or(&Value::Null),
                );
            }
        }
        (Value::Array(expected), Value::Array(actual)) => {
            assert_eq!(
                expected.len(),
                actual.len(),
                "array length mismatch at {path}"
            );
            for (index, (expected, actual)) in expected.iter().zip(actual).enumerate() {
                assert_json_equal(&format!("{path}[{index}]"), expected, actual);
            }
        }
        (Value::Number(_), Value::Number(_)) => {
            assert_eq!(
                expected.as_f64().map(f64::to_bits),
                actual.as_f64().map(f64::to_bits),
                "number mismatch at {path}"
            );
        }
        _ => assert_eq!(expected, actual, "value mismatch at {path}"),
    }
}

// =============================================================================
// Routes
// =============================================================================

fn anthropic_route() -> Route<AnthropicMessages> {
    Route::new(anthropic_messages::route_handle(), AnthropicMessages)
}

fn openai_chat_route() -> Route<OpenAiChat> {
    Route::new(openai_chat::route_handle(), OpenAiChat)
}

fn openai_compatible_route(base_url: &str) -> Route<OpenAiChat> {
    let mut handle = openai_compatible_chat::route_handle();
    handle.endpoint = Endpoint {
        base_url: Some(base_url.to_string()),
        path: endpoint_path("/chat/completions"),
        query: None,
    };
    Route::new(handle, OpenAiChat)
}

fn endpoint_path(path: &str) -> alforria_llm::route::endpoint::EndpointPart {
    alforria_llm::route::endpoint::EndpointPart::Path(path.to_string())
}

fn openai_responses_route() -> Route<OpenAiResponses> {
    Route::new(openai_responses::route_handle(), OpenAiResponses)
}

fn gemini_route() -> Route<Gemini> {
    Route::new(gemini::route_handle(), Gemini)
}

fn bedrock_route() -> Route<BedrockConverse> {
    let mut handle = bedrock_converse::route_handle();
    // Replay does not need SigV4 signing: the recorded headers are the
    // pre-auth set. The regional host comes from the provider facade (M3).
    handle.auth = Auth::none();
    handle.endpoint.base_url = Some("https://bedrock-runtime.us-east-1.amazonaws.com".to_string());
    Route::new(handle, BedrockConverse)
}

// =============================================================================
// Replay
// =============================================================================

fn base64_decode(input: &str) -> Vec<u8> {
    let alphabet = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("invalid base64 character: {}", c as char),
    };
    let filtered: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::new();
    for chunk in filtered.chunks(4) {
        let values: Vec<u8> = chunk
            .iter()
            .filter(|b| **b != b'=')
            .map(|b| alphabet(*b))
            .collect();
        match values.len() {
            4 => {
                out.push((values[0] << 2) | (values[1] >> 4));
                out.push((values[1] << 4) | (values[2] >> 2));
                out.push((values[2] << 6) | values[3]);
            }
            3 => {
                out.push((values[0] << 2) | (values[1] >> 4));
                out.push((values[1] << 4) | (values[2] >> 2));
            }
            2 => out.push((values[0] << 2) | (values[1] >> 4)),
            _ => panic!("invalid base64 chunk"),
        }
    }
    out
}

/// Replay one recorded interaction through the full pipeline:
/// request parity via `Route::compile`, then the response frames through
/// `decode_frame` → `terminal` → `step` → `on_halt` (the `Route::stream`
/// semantics, without the HTTP transport).
async fn replay_interaction<P: Protocol>(
    route: &Route<P>,
    request: &LlmRequest,
    interaction: &Interaction,
) -> Vec<LlmEvent> {
    let prepared = route.compile(request).unwrap();
    assert_eq!(prepared.method, interaction.method, "request method");
    // `{account}`-style placeholder segments in cloudflare URLs survive
    // `Url::parse` percent-encoded; normalize them back for comparison.
    assert_eq!(
        prepared
            .url
            .as_str()
            .replace("%7B", "{")
            .replace("%7D", "}"),
        interaction.url,
        "request url"
    );
    for (name, value) in &route.handle.defaults.headers {
        let recorded = interaction
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("recorded headers missing {name}"));
        assert_eq!(value, &recorded, "recorded header {name}");
    }
    let actual_body: Value = serde_json::from_str(&prepared.body).unwrap();
    assert_json_equal("body", &interaction.body, &actual_body);

    let bytes = match interaction.response_body_encoding.as_deref() {
        Some("base64") => base64_decode(&interaction.response_body),
        _ => interaction.response_body.clone().into_bytes(),
    };
    let framing = match interaction.response_body_encoding.as_deref() {
        Some("base64") => Framing::AwsEventStream,
        _ => Framing::Sse,
    };
    assert_eq!(interaction.response_status, 200, "replay expects HTTP 200");

    let mut frames = framing.frame(futures::stream::iter(vec![Ok(bytes)]));
    let protocol = &route.protocol;
    let mut state = Some(protocol.initial(request));
    let mut events = Vec::new();
    while let Some(frame) = frames.next().await {
        let frame = frame.unwrap();
        let Some(event) = protocol.decode_frame(&frame).unwrap() else {
            continue;
        };
        let is_terminal = protocol.terminal(&event);
        let current = state.take().unwrap();
        let (next, emitted) = protocol.step(current, &event).unwrap();
        state = Some(next);
        events.extend(emitted);
        if is_terminal {
            break;
        }
    }
    let final_state = state.take().unwrap();
    events.extend(protocol.on_halt(final_state));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::Finish { .. })),
        "replay produced no finish event"
    );
    events
}

// =============================================================================
// Driver request builders
// =============================================================================

fn large_cacheable_system() -> String {
    // recorded-scenarios.ts `LARGE_CACHEABLE_SYSTEM`: the fixed sentence × 250.
    "You are a concise, factual assistant. Answer precisely and avoid filler. Cite numbers when known. "
        .repeat(250)
}

fn weather_tool() -> ToolDefinition {
    ToolDefinition {
        name: "get_weather".to_string(),
        description: "Get current weather for a city.".to_string(),
        input_schema: json_map(json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
            "additionalProperties": false,
        })),
        output_schema: None,
        cache: None,
        metadata: None,
        native: None,
    }
}

fn read_screenshot_tool() -> ToolDefinition {
    ToolDefinition {
        name: "read_screenshot".to_string(),
        description: "Capture a screenshot of the current screen.".to_string(),
        input_schema: json_map(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })),
        output_schema: None,
        cache: None,
        metadata: None,
        native: None,
    }
}

fn json_map(value: Value) -> JsonMap {
    serde_json::from_value(value).unwrap()
}

fn system_part(text: &str) -> SystemPart {
    SystemPart {
        r#type: SystemPartType::Text,
        text: text.to_string(),
        cache: None,
        metadata: None,
    }
}

fn generation(max_tokens: f64, temperature: Option<f64>) -> GenerationOptions {
    GenerationOptions {
        max_tokens: Some(max_tokens),
        temperature,
        top_p: None,
        top_k: None,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        stop: None,
    }
}

fn openai_options(entries: Value) -> Option<ProviderOptions> {
    let mut options = ProviderOptions::new();
    options.insert("openai".to_string(), json_map(entries));
    Some(options)
}

fn base_request(model: ModelRef, system: Vec<SystemPart>, messages: Vec<Message>) -> LlmRequest {
    let mut request = LlmRequest::new(model);
    request.cache = Some(CachePolicy::Literal(CachePolicyLiteral::None));
    request.system = system;
    request.messages = messages;
    request
}

fn text_request(
    model: ModelRef,
    system: &str,
    prompt: &str,
    max_tokens: f64,
    temperature: Option<f64>,
) -> LlmRequest {
    let mut request = base_request(
        model,
        vec![system_part(system)],
        vec![Message::user(prompt)],
    );
    request.generation = Some(generation(max_tokens, temperature));
    request
}

fn weather_tool_call_request(
    model: ModelRef,
    max_tokens: f64,
    temperature: Option<f64>,
) -> LlmRequest {
    let mut request = base_request(
        model,
        vec![system_part("Call tools exactly as requested.")],
        vec![Message::user("Call get_weather with city exactly Paris.")],
    );
    request.tools = vec![weather_tool()];
    request.tool_choice = Some(ToolChoice::named("get_weather"));
    request.generation = Some(generation(max_tokens, temperature));
    request
}

fn weather_tool_loop_request(
    model: ModelRef,
    system: &str,
    max_tokens: f64,
    temperature: Option<f64>,
) -> LlmRequest {
    let mut request = base_request(
        model,
        vec![system_part(system)],
        vec![Message::user("What is the weather in Paris?")],
    );
    request.tools = vec![weather_tool()];
    request.generation = Some(generation(max_tokens, temperature));
    request
}

/// `assistantContent` from recorded-scenarios.ts — fold streamed events into
/// the assistant message content for the next turn.
fn assistant_content(events: &[LlmEvent]) -> Vec<ContentPart> {
    let mut content: Vec<ContentPart> = Vec::new();
    for event in events {
        match event {
            LlmEvent::TextDelta { text, .. } => match content.last_mut() {
                Some(ContentPart::Text { text: joined, .. }) => {
                    *joined = format!("{joined}{text}");
                }
                _ => content.push(ContentPart::text(text.clone())),
            },
            LlmEvent::ReasoningDelta { text, .. } => match content.last_mut() {
                Some(ContentPart::Reasoning { text: joined, .. }) => {
                    *joined = format!("{joined}{text}");
                }
                _ => content.push(ContentPart::reasoning(text.clone())),
            },
            LlmEvent::TextEnd {
                provider_metadata, ..
            } => {
                if let Some(ContentPart::Text {
                    provider_metadata: joined,
                    ..
                }) = content.last_mut()
                {
                    *joined = provider_metadata.clone();
                }
            }
            LlmEvent::ReasoningEnd {
                provider_metadata, ..
            } => {
                if let Some(ContentPart::Reasoning {
                    provider_metadata: joined,
                    ..
                }) = content.last_mut()
                {
                    *joined = provider_metadata.clone();
                }
            }
            LlmEvent::ToolCall {
                id,
                name,
                input,
                provider_executed,
                provider_metadata,
            } => content.push(ContentPart::ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
                provider_executed: *provider_executed,
                metadata: None,
                provider_metadata: provider_metadata.clone(),
            }),
            _ => {}
        }
    }
    content
}

/// Build the follow-up tool result message for the recorded weather loop.
///
/// TS builds a `Json` result (`{temperature: 22, condition: "sunny"}`) and
/// relies on `JSON.stringify` insertion order for the wire string
/// (`{"temperature":22,"condition":"sunny"}`). serde_json sorts object keys
/// (no `preserve_order`), so the harness passes the recorded JSON *string* as
/// a text result instead — the lowered bodies match byte-for-byte while the
/// neutral model stays representable.
/// Build the follow-up tool result message for the recorded weather loop.
///
/// TS builds a `Json` result (`{temperature: 22, condition: "sunny"}`) and
/// relies on `JSON.stringify` insertion order for the wire string
/// (`{"temperature":22,"condition":"sunny"}`). serde_json sorts object keys
/// (no `preserve_order`), so for stringified-result protocols the harness
/// passes the recorded JSON *string* as a text result instead — the lowered
/// bodies match byte-for-byte while the neutral model stays representable.
fn weather_tool_result_text(id: &str, name: &str) -> Message {
    Message::tool(ToolResultInput {
        id: id.to_string(),
        name: name.to_string(),
        result: json!("{\"temperature\":22,\"condition\":\"sunny\"}"),
        result_type: Some(ToolResultType::Text),
        provider_executed: None,
        cache: None,
        metadata: None,
        provider_metadata: None,
    })
}

/// Bedrock embeds the tool result as a JSON object (`{"json": {...}}`), so
/// key order never reaches the wire and a plain JSON result suffices.
fn weather_tool_result_json(id: &str, name: &str) -> Message {
    Message::tool(ToolResultInput {
        id: id.to_string(),
        name: name.to_string(),
        result: json!({"temperature": 22, "condition": "sunny"}),
        result_type: Some(ToolResultType::Json),
        provider_executed: None,
        cache: None,
        metadata: None,
        provider_metadata: None,
    })
}

/// `runWeatherToolLoop` from recorded-scenarios.ts — drive the recorded
/// interactions in order, appending the assistant turn + tool result for
/// each tool call the model emitted. Returns the events the TS golden
/// assertions observe (non-finish events from every turn, plus the final
/// turn's finish and the dispatched tool results).
async fn drive_weather_tool_loop<P: Protocol>(
    route: &Route<P>,
    cassette: &[Interaction],
    request: &LlmRequest,
    tool_result: &dyn Fn(&str, &str) -> Message,
) -> Vec<LlmEvent> {
    let mut next = request.clone();
    let mut events: Vec<LlmEvent> = Vec::new();
    for interaction in cassette {
        let turn = replay_interaction(route, &next, interaction).await;
        let tool_calls: Vec<&LlmEvent> = turn
            .iter()
            .filter(|event| matches!(event, LlmEvent::ToolCall { .. }))
            .collect();
        events.extend(
            turn.iter()
                .filter(|event| !matches!(event, LlmEvent::Finish { .. }))
                .cloned(),
        );
        if tool_calls.is_empty() {
            if let Some(finish) = turn
                .iter()
                .rev()
                .find(|event| matches!(event, LlmEvent::Finish { .. }))
            {
                events.push(finish.clone());
            }
            return events;
        }
        // ToolRuntime.dispatch result event (weatherRuntimeTool.execute).
        for call in &tool_calls {
            if let LlmEvent::ToolCall { id, name, .. } = *call {
                events.push(LlmEvent::ToolResult {
                    id: id.clone(),
                    name: name.clone(),
                    result: ToolResultValue::Json {
                        value: json!({"temperature": 22, "condition": "sunny"}),
                    },
                    output: None,
                    provider_executed: None,
                    provider_metadata: None,
                });
            }
        }
        let mut messages = next.messages.clone();
        messages.push(Message::assistant(assistant_content(&turn)));
        for call in &tool_calls {
            if let LlmEvent::ToolCall { id, name, .. } = *call {
                messages.push(tool_result(id, name));
            }
        }
        next = next.update(LlmRequestPatch {
            messages: Some(messages),
            ..LlmRequestPatch::default()
        });
    }
    panic!("weather tool loop exceeded the recorded interactions");
}

// =============================================================================
// Assertions
// =============================================================================

fn event_kind(event: &LlmEvent) -> &'static str {
    match event {
        LlmEvent::StepStart { .. } => "step-start",
        LlmEvent::TextStart { .. } => "text-start",
        LlmEvent::TextDelta { .. } => "text-delta",
        LlmEvent::TextEnd { .. } => "text-end",
        LlmEvent::ReasoningStart { .. } => "reasoning-start",
        LlmEvent::ReasoningDelta { .. } => "reasoning-delta",
        LlmEvent::ReasoningEnd { .. } => "reasoning-end",
        LlmEvent::ToolInputStart { .. } => "tool-input-start",
        LlmEvent::ToolInputDelta { .. } => "tool-input-delta",
        LlmEvent::ToolInputEnd { .. } => "tool-input-end",
        LlmEvent::ToolCall { .. } => "tool-call",
        LlmEvent::ToolResult { .. } => "tool-result",
        LlmEvent::ToolError { .. } => "tool-error",
        LlmEvent::StepFinish { .. } => "step-finish",
        LlmEvent::Finish { .. } => "finish",
        LlmEvent::ProviderError { .. } => "provider-error",
    }
}

fn kinds(events: &[LlmEvent]) -> Vec<&'static str> {
    events.iter().map(event_kind).collect()
}

fn finishes(events: &[LlmEvent]) -> Vec<&LlmEvent> {
    events
        .iter()
        .filter(|event| matches!(event, LlmEvent::Finish { .. }))
        .collect()
}

fn finish_reason(event: &LlmEvent) -> FinishReason {
    match event {
        LlmEvent::Finish { reason, .. } => *reason,
        _ => panic!("not a finish event"),
    }
}

fn finish_usage(event: &LlmEvent) -> Option<&Usage> {
    match event {
        LlmEvent::Finish { usage, .. } => usage.as_ref(),
        _ => panic!("not a finish event"),
    }
}

/// §8.4: in every golden, `nonCached + cacheRead + cacheWrite == inputTokens`
/// and `reasoningTokens <= outputTokens`.
fn assert_usage_invariants(usage: &Usage) {
    if let (Some(non_cached), Some(cache_read), Some(cache_write)) = (
        usage.non_cached_input_tokens,
        usage.cache_read_input_tokens,
        usage.cache_write_input_tokens,
    ) {
        if let Some(input) = usage.input_tokens {
            assert_eq!(
                non_cached + cache_read + cache_write,
                input,
                "nonCached + cacheRead + cacheWrite == inputTokens, got {usage:?}"
            );
        }
    }
    if let (Some(reasoning), Some(output)) = (usage.reasoning_tokens, usage.output_tokens) {
        assert!(
            reasoning <= output,
            "reasoningTokens {reasoning} > outputTokens {output}"
        );
    }
}

/// One turn of a healthy golden replay: exactly one finish, it is the last
/// event, and its usage satisfies the normalization invariants.
fn assert_turn(events: &[LlmEvent]) -> FinishReason {
    let finishes = finishes(events);
    assert_eq!(finishes.len(), 1, "exactly one finish, got {events:?}");
    assert!(
        matches!(events.last(), Some(event) if matches!(event, LlmEvent::Finish { .. })),
        "finish must be the last event, got {events:?}"
    );
    if let Some(usage) = finish_usage(finishes[0]) {
        assert_usage_invariants(usage);
    }
    finish_reason(finishes[0])
}

fn response_text(events: &[LlmEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn response_reasoning(events: &[LlmEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::ReasoningDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn assert_tool_call_shape(events: &[LlmEvent]) {
    let calls: Vec<&LlmEvent> = events
        .iter()
        .filter(|event| matches!(event, LlmEvent::ToolCall { .. }))
        .collect();
    assert_eq!(calls.len(), 1, "one tool-call, got {events:?}");
    match calls[0] {
        LlmEvent::ToolCall {
            id, name, input, ..
        } => {
            assert!(!id.is_empty(), "tool-call id");
            assert_eq!(name, "get_weather");
            assert_eq!(input, &json!({"city": "Paris"}));
        }
        _ => unreachable!(),
    }
}

/// `expectWeatherToolLoop` from recorded-scenarios.ts: one finish with reason
/// stop, step-finish reasons `["tool-calls", "stop"]`, exactly one
/// `get_weather` tool call and one dispatched tool result, and a final text
/// mentioning Paris.
fn assert_weather_tool_loop(events: &[LlmEvent]) {
    let finishes = finishes(events);
    assert_eq!(finishes.len(), 1);
    assert_eq!(finish_reason(finishes[0]), FinishReason::Stop);

    let step_reasons: Vec<FinishReason> = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::StepFinish { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect();
    assert_eq!(
        step_reasons,
        vec![FinishReason::ToolCalls, FinishReason::Stop],
        "step-finish reasons"
    );

    assert_tool_call_shape(events);

    let results: Vec<&LlmEvent> = events
        .iter()
        .filter(|event| matches!(event, LlmEvent::ToolResult { .. }))
        .collect();
    assert_eq!(results.len(), 1);
    match results[0] {
        LlmEvent::ToolResult { name, result, .. } => {
            assert_eq!(name, "get_weather");
            assert_eq!(
                result,
                &ToolResultValue::Json {
                    value: json!({"temperature": 22, "condition": "sunny"}),
                }
            );
        }
        _ => unreachable!(),
    }

    let text = response_text(events);
    assert!(!text.trim().is_empty(), "loop text must be non-empty");
    assert!(
        text.contains("Paris"),
        "loop text mentions Paris, got {text:?}"
    );
}

fn normalized_image_text(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphabetic() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// =============================================================================
// anthropic-messages/
// =============================================================================

const ANTHROPIC_HAIKU: (&str, &str) = ("claude-haiku-4-5-20251001", "anthropic");
const ANTHROPIC_OPUS: (&str, &str) = ("claude-opus-4-7", "anthropic");

fn anthropic_model(model: (&str, &str)) -> ModelRef {
    ModelRef::new(
        model.0,
        model.1,
        Arc::new(anthropic_messages::route_handle()),
    )
}

#[tokio::test]
async fn anthropic_messages_streams_text() {
    let cassette = load_cassette("anthropic-messages/streams-text");
    let route = anthropic_route();
    let request = text_request(
        anthropic_model(ANTHROPIC_HAIKU),
        "You are concise.",
        "Reply with exactly: Hello!",
        20.0,
        Some(0.0),
    );
    let events = replay_interaction(&route, &request, &cassette[0]).await;

    // §5.4 canonical sequence.
    assert_eq!(
        kinds(&events),
        vec![
            "step-start",
            "text-start",
            "text-delta",
            "text-end",
            "step-finish",
            "finish",
        ]
    );
    match &events[1] {
        LlmEvent::TextStart { id, .. } => assert_eq!(id, "text-0"),
        _ => unreachable!(),
    }
    match &events[2] {
        LlmEvent::TextDelta { id, text, .. } => {
            assert_eq!(id, "text-0");
            assert_eq!(text, "Hello!");
        }
        _ => unreachable!(),
    }
    match &events[3] {
        LlmEvent::TextEnd { id, .. } => assert_eq!(id, "text-0"),
        _ => unreachable!(),
    }
    match &events[4] {
        LlmEvent::StepFinish { index, reason, .. } => {
            assert_eq!(*index, 0.0);
            assert_eq!(*reason, FinishReason::Stop);
        }
        _ => unreachable!(),
    }
    let usage = finish_usage(&events[5]).unwrap();
    assert_eq!(usage.input_tokens, Some(18.0));
    assert_eq!(usage.non_cached_input_tokens, Some(18.0));
    assert_eq!(usage.cache_read_input_tokens, Some(0.0));
    assert_eq!(usage.cache_write_input_tokens, Some(0.0));
    assert_eq!(usage.output_tokens, Some(5.0));
    assert_eq!(usage.reasoning_tokens, None);
    assert_eq!(usage.total_tokens, Some(23.0));
    assert_usage_invariants(usage);
    assert_eq!(finish_reason(&events[5]), FinishReason::Stop);
    assert_eq!(response_text(&events), "Hello!");
}

#[tokio::test]
async fn anthropic_messages_streams_tool_call() {
    let cassette = load_cassette("anthropic-messages/streams-tool-call");
    let route = anthropic_route();
    let request = weather_tool_call_request(anthropic_model(ANTHROPIC_HAIKU), 80.0, Some(0.0));
    let events = replay_interaction(&route, &request, &cassette[0]).await;

    // §5.4 canonical sequence: the empty `partial_json: ""` delta is dropped.
    assert_eq!(
        kinds(&events),
        vec![
            "step-start",
            "tool-input-start",
            "tool-input-delta",
            "tool-input-delta",
            "tool-input-end",
            "tool-call",
            "step-finish",
            "finish",
        ]
    );
    match &events[1] {
        LlmEvent::ToolInputStart { id, name, .. } => {
            assert_eq!(id, "toolu_012rmAruviySvUXSjgCPWVRu");
            assert_eq!(name, "get_weather");
        }
        _ => unreachable!(),
    }
    match &events[2] {
        LlmEvent::ToolInputDelta { text, .. } => assert_eq!(text, "{\"city\":"),
        _ => unreachable!(),
    }
    match &events[3] {
        LlmEvent::ToolInputDelta { text, .. } => assert_eq!(text, " \"Paris\"}"),
        _ => unreachable!(),
    }
    match &events[5] {
        LlmEvent::ToolCall { name, input, .. } => {
            assert_eq!(name, "get_weather");
            assert_eq!(input, &json!({"city": "Paris"}));
        }
        _ => unreachable!(),
    }
    let usage = finish_usage(&events[7]).unwrap();
    assert_eq!(usage.input_tokens, Some(677.0));
    assert_eq!(usage.non_cached_input_tokens, Some(677.0));
    assert_eq!(usage.cache_read_input_tokens, Some(0.0));
    assert_eq!(usage.cache_write_input_tokens, Some(0.0));
    assert_eq!(usage.output_tokens, Some(33.0));
    assert_eq!(usage.reasoning_tokens, None);
    assert_eq!(usage.total_tokens, Some(710.0));
    assert_usage_invariants(usage);
    assert_eq!(finish_reason(&events[7]), FinishReason::ToolCalls);
}

#[tokio::test]
async fn anthropic_messages_drives_a_tool_loop() {
    let cassette = load_cassette("anthropic-messages/claude-opus-4-7-drives-a-tool-loop");
    let route = anthropic_route();
    let request = weather_tool_loop_request(
        anthropic_model(ANTHROPIC_OPUS),
        "Use the get_weather tool, then answer in one short sentence.",
        80.0,
        None,
    );
    let events =
        drive_weather_tool_loop(&route, &cassette, &request, &weather_tool_result_text).await;
    assert_weather_tool_loop(&events);
}

#[tokio::test]
async fn anthropic_messages_image_tool_result() {
    let cassette = load_cassette("anthropic-messages/anthropic-opus-4-7-image-tool-result");
    let route = anthropic_route();
    let image = include_str!("support/restroom.png.b64");
    let mut request = base_request(
        anthropic_model(ANTHROPIC_OPUS),
        vec![system_part(
            "Read images carefully. Reply only with the visible text, lowercase, no punctuation.",
        )],
        vec![
            Message::user("Use the read_screenshot tool, then reply with the words shown."),
            Message::assistant(vec![ContentPart::tool_call(
                "call_screenshot_1",
                "read_screenshot",
                json!({}),
            )]),
            Message::tool(ToolResultInput {
                id: "call_screenshot_1".to_string(),
                name: "read_screenshot".to_string(),
                result: json!([
                    {"type": "text", "text": "Image read successfully"},
                    {"type": "file", "uri": format!("data:image/png;base64,{image}"), "mime": "image/png"},
                ]),
                result_type: Some(ToolResultType::Content),
                provider_executed: None,
                cache: None,
                metadata: None,
                provider_metadata: None,
            }),
        ],
    );
    request.tools = vec![read_screenshot_tool()];
    request.generation = Some(generation(40.0, None));
    let events = replay_interaction(&route, &request, &cassette[0]).await;

    assert_eq!(assert_turn(&events), FinishReason::Stop);
    assert_eq!(
        normalized_image_text(&response_text(&events)),
        "jiggling restroom prison"
    );
}

#[tokio::test]
async fn anthropic_messages_accepts_malformed_assistant_tool_order() {
    let cassette = load_cassette(
        "anthropic-messages/accepts-malformed-assistant-tool-order-with-default-patch",
    );
    let route = anthropic_route();
    let tool = ToolDefinition {
        name: "get_weather".to_string(),
        description: "Get weather".to_string(),
        input_schema: json_map(json!({"type": "object", "properties": {}})),
        output_schema: None,
        cache: None,
        metadata: None,
        native: None,
    };
    // The recorded request carries the assistant text and tool_use as two
    // consecutive assistant messages (the "default patch" shape).
    let mut request = base_request(
        anthropic_model(ANTHROPIC_HAIKU),
        Vec::new(),
        vec![
            Message::assistant(vec![ContentPart::text("I will check the weather.")]),
            Message::assistant(vec![ContentPart::tool_call(
                "call_1",
                "get_weather",
                json!({"city": "Paris"}),
            )]),
            Message::tool(ToolResultInput {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                result: json!({"temperature": "72F"}),
                result_type: None,
                provider_executed: None,
                cache: None,
                metadata: None,
                provider_metadata: None,
            }),
            Message::user("Use that result to answer briefly."),
        ],
    );
    request.tools = vec![tool.clone()];
    request.cache = Some(CachePolicy::Object(CachePolicyObject {
        tools: None,
        system: None,
        messages: Some(CachePolicyMessages::LatestUserMessage),
        ttl_seconds: None,
    }));

    let events = replay_interaction(&route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&events), FinishReason::Stop);
    assert!(response_text(&events).contains("72"));
}

#[tokio::test]
async fn anthropic_messages_rejects_malformed_assistant_tool_order() {
    let cassette =
        load_cassette("anthropic-messages/rejects-malformed-assistant-tool-order-without-patch");
    let route = anthropic_route();
    let tool = ToolDefinition {
        name: "get_weather".to_string(),
        description: "Get weather".to_string(),
        input_schema: json_map(json!({"type": "object", "properties": {}})),
        output_schema: None,
        cache: None,
        metadata: None,
        native: None,
    };
    let mut request = base_request(
        anthropic_model(ANTHROPIC_HAIKU),
        Vec::new(),
        vec![
            Message::assistant(vec![
                ContentPart::tool_call("call_1", "get_weather", json!({"city": "Paris"})),
                ContentPart::text("I will check the weather."),
            ]),
            Message::tool(ToolResultInput {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                result: json!({"temperature": "72F"}),
                result_type: None,
                provider_executed: None,
                cache: None,
                metadata: None,
                provider_metadata: None,
            }),
            Message::user("Use that result to answer briefly."),
        ],
    );
    request.tools = vec![tool];
    request.cache = Some(CachePolicy::Literal(CachePolicyLiteral::None));

    // Request parity only: the 400 response is a JSON error body, not a
    // stream. The executor classifies it as InvalidRequest (HTTP 400).
    let prepared = route.compile(&request).unwrap();
    let body: Value = serde_json::from_str(&prepared.body).unwrap();
    assert_json_equal("body", &cassette[0].body, &body);
    assert_eq!(cassette[0].response_status, 400);

    use alforria_llm::route::executor::{status_reason, StatusReasonInput};
    use alforria_llm::schema::errors::{HttpContext, HttpRequestDetails, HttpResponseDetails};
    let http = HttpContext {
        request: HttpRequestDetails {
            method: "POST".to_string(),
            url: prepared.url.to_string(),
            headers: std::collections::BTreeMap::new(),
        },
        response: Some(HttpResponseDetails {
            status: 400.0,
            headers: std::collections::BTreeMap::new(),
        }),
        body: Some(cassette[0].response_body.clone()),
        body_truncated: None,
        request_id: None,
        rate_limit: None,
    };
    let reason = status_reason(StatusReasonInput {
        status: 400,
        message: "Bad request".to_string(),
        retry_after_ms: None,
        rate_limit: None,
        http,
    });
    assert!(matches!(
        reason,
        alforria_llm::schema::errors::LlmErrorReason::InvalidRequest { .. }
    ));
}

// =============================================================================
// anthropic-messages-cache/
// =============================================================================

#[tokio::test]
async fn anthropic_messages_cache_writes_then_reads_cache_control() {
    let cassette = load_cassette(
        "anthropic-messages-cache/writes-then-reads-cache-control-on-identical-second-call",
    );
    let route = anthropic_route();
    let mut request = base_request(
        anthropic_model(ANTHROPIC_HAIKU),
        vec![SystemPart {
            r#type: SystemPartType::Text,
            text: large_cacheable_system(),
            cache: Some(CacheHint {
                r#type: CacheHintType::Ephemeral,
                ttl_seconds: None,
            }),
            metadata: None,
        }],
        vec![Message::user("Say hi.")],
    );
    request.generation = Some(generation(16.0, Some(0.0)));

    let first = replay_interaction(&route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&first), FinishReason::Stop);
    assert_eq!(response_text(&first), "Hi.");
    let first_usage = finish_usage(finishes(&first)[0]).unwrap();
    assert_eq!(first_usage.cache_read_input_tokens, Some(0.0));
    assert_eq!(first_usage.cache_write_input_tokens, Some(5752.0));

    let second = replay_interaction(&route, &request, &cassette[1]).await;
    assert_eq!(assert_turn(&second), FinishReason::Stop);
    let second_usage = finish_usage(finishes(&second)[0]).unwrap();
    assert_eq!(second_usage.cache_read_input_tokens, Some(5752.0));
    assert_eq!(second_usage.cache_write_input_tokens, Some(0.0));
}

// =============================================================================
// bedrock-converse/
// =============================================================================

fn bedrock_model() -> ModelRef {
    ModelRef::new(
        "us.amazon.nova-micro-v1:0",
        "amazon-bedrock",
        Arc::new(bedrock_converse::route_handle()),
    )
}

#[tokio::test]
async fn bedrock_converse_streams_text() {
    let cassette = load_cassette("bedrock-converse/streams-text");
    let route = bedrock_route();
    let request = text_request(
        bedrock_model(),
        "Reply with the single word 'Hello'.",
        "Say hello.",
        16.0,
        Some(0.0),
    );
    let events = replay_interaction(&route, &request, &cassette[0]).await;

    assert_eq!(assert_turn(&events), FinishReason::Stop);
    assert_eq!(response_text(&events), "Hello");
    let usage = finish_usage(finishes(&events)[0]).unwrap();
    assert_eq!(usage.input_tokens, Some(12.0));
    assert_eq!(usage.output_tokens, Some(2.0));
    assert_eq!(usage.total_tokens, Some(14.0));
}

#[tokio::test]
async fn bedrock_converse_streams_a_tool_call() {
    let cassette = load_cassette("bedrock-converse/streams-a-tool-call");
    let route = bedrock_route();
    let request = weather_tool_call_request(bedrock_model(), 80.0, Some(0.0));
    let events = replay_interaction(&route, &request, &cassette[0]).await;

    assert_eq!(assert_turn(&events), FinishReason::ToolCalls);
    assert_tool_call_shape(&events);
    let usage = finish_usage(finishes(&events)[0]).unwrap();
    assert_eq!(usage.input_tokens, Some(419.0));
    assert_eq!(usage.output_tokens, Some(16.0));
    assert_eq!(usage.total_tokens, Some(435.0));
}

#[tokio::test]
async fn bedrock_converse_drives_a_tool_loop() {
    let cassette = load_cassette("bedrock-converse/drives-a-tool-loop");
    let route = bedrock_route();
    let request = weather_tool_loop_request(
        bedrock_model(),
        "Use the get_weather tool, then answer in one short sentence.",
        80.0,
        Some(0.0),
    );
    let events =
        drive_weather_tool_loop(&route, &cassette, &request, &weather_tool_result_json).await;
    assert_weather_tool_loop(&events);
}

// =============================================================================
// cloudflare-* / openai-compatible-chat/ (all openai-chat protocol)
// =============================================================================

fn compatible_model(model: &str, base_url: &str) -> (ModelRef, Route<OpenAiChat>) {
    let route = openai_compatible_route(base_url);
    let mut handle = openai_compatible_chat::route_handle();
    (
        ModelRef::new(model, "compatible", {
            handle.endpoint = Endpoint {
                base_url: Some(base_url.to_string()),
                path: endpoint_path("/chat/completions"),
                query: None,
            };
            Arc::new(handle)
        }),
        route,
    )
}

async fn assert_streams_text(model: ModelRef, route: &Route<OpenAiChat>, name: &str) {
    let cassette = load_cassette(name);
    let request = text_request(
        model,
        "You are concise.",
        "Reply exactly with: Hello!",
        40.0,
        Some(0.0),
    );
    let events = replay_interaction(route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&events), FinishReason::Stop, "{name}");
    assert_eq!(response_text(&events), "Hello!", "{name}");
}

async fn assert_streams_tool_call(
    model: ModelRef,
    route: &Route<OpenAiChat>,
    name: &str,
    max_tokens: f64,
) {
    let cassette = load_cassette(name);
    let request = weather_tool_call_request(model, max_tokens, Some(0.0));
    let events = replay_interaction(route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&events), FinishReason::ToolCalls, "{name}");
    assert_tool_call_shape(&events);
}

#[tokio::test]
async fn cloudflare_ai_gateway_streams_text() {
    let (model, route) = compatible_model(
        "workers-ai/@cf/meta/llama-3.1-8b-instruct",
        "https://gateway.ai.cloudflare.com/v1/{account}/{gateway}/compat",
    );
    assert_streams_text(
        model,
        &route,
        "cloudflare-ai-gateway/cloudflare-ai-gateway-workers-ai-llama-3-1-8b-text",
    )
    .await;
}

#[tokio::test]
async fn cloudflare_ai_gateway_streams_tool_call() {
    let (model, route) = compatible_model(
        "workers-ai/@cf/openai/gpt-oss-20b",
        "https://gateway.ai.cloudflare.com/v1/{account}/{gateway}/compat",
    );
    assert_streams_tool_call(
        model,
        &route,
        "cloudflare-ai-gateway/cloudflare-ai-gateway-workers-ai-gpt-oss-20b-tools-tool-call",
        120.0,
    )
    .await;
}

#[tokio::test]
async fn cloudflare_workers_ai_streams_text() {
    let (model, route) = compatible_model(
        "@cf/meta/llama-3.1-8b-instruct",
        "https://api.cloudflare.com/client/v4/accounts/{account}/ai/v1",
    );
    assert_streams_text(
        model,
        &route,
        "cloudflare-workers-ai/cloudflare-workers-ai-llama-3-1-8b-text",
    )
    .await;
}

#[tokio::test]
async fn cloudflare_workers_ai_streams_tool_call() {
    let (model, route) = compatible_model(
        "@cf/openai/gpt-oss-20b",
        "https://api.cloudflare.com/client/v4/accounts/{account}/ai/v1",
    );
    assert_streams_tool_call(
        model,
        &route,
        "cloudflare-workers-ai/cloudflare-workers-ai-gpt-oss-20b-tools-tool-call",
        120.0,
    )
    .await;
}

#[tokio::test]
async fn deepseek_streams_text() {
    let (model, route) = compatible_model("deepseek-chat", "https://api.deepseek.com/v1");
    let cassette = load_cassette("openai-compatible-chat/deepseek-streams-text");
    let request = text_request(
        model,
        "You are concise.",
        "Reply with exactly: Hello!",
        20.0,
        Some(0.0),
    );
    let events = replay_interaction(&route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&events), FinishReason::Stop);
    assert_eq!(response_text(&events), "Hello!");
}

#[tokio::test]
async fn togetherai_streams_text() {
    let (model, route) = compatible_model(
        "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        "https://api.together.xyz/v1",
    );
    let cassette = load_cassette("openai-compatible-chat/togetherai-streams-text");
    let request = text_request(
        model,
        "You are concise.",
        "Reply with exactly: Hello!",
        20.0,
        Some(0.0),
    );
    let events = replay_interaction(&route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&events), FinishReason::Stop);
    assert_eq!(response_text(&events), "Hello!");
}

#[tokio::test]
async fn togetherai_streams_tool_call() {
    let (model, route) = compatible_model(
        "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        "https://api.together.xyz/v1",
    );
    assert_streams_tool_call(
        model,
        &route,
        "openai-compatible-chat/togetherai-streams-tool-call",
        80.0,
    )
    .await;
}

#[tokio::test]
async fn groq_streams_text() {
    let (model, route) =
        compatible_model("llama-3.3-70b-versatile", "https://api.groq.com/openai/v1");
    let cassette = load_cassette("openai-compatible-chat/groq-streams-text");
    let request = text_request(
        model,
        "You are concise.",
        "Reply with exactly: Hello!",
        20.0,
        Some(0.0),
    );
    let events = replay_interaction(&route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&events), FinishReason::Stop);
    assert_eq!(response_text(&events), "Hello!");
}

#[tokio::test]
async fn groq_streams_tool_call() {
    let (model, route) =
        compatible_model("llama-3.3-70b-versatile", "https://api.groq.com/openai/v1");
    assert_streams_tool_call(
        model,
        &route,
        "openai-compatible-chat/groq-streams-tool-call",
        80.0,
    )
    .await;
}

#[tokio::test]
async fn groq_drives_a_tool_loop() {
    let (model, route) =
        compatible_model("llama-3.3-70b-versatile", "https://api.groq.com/openai/v1");
    let cassette = load_cassette("openai-compatible-chat/groq-llama-3-3-70b-drives-a-tool-loop");
    let request = weather_tool_loop_request(
        model,
        "Use the get_weather tool, then answer in one short sentence.",
        80.0,
        Some(0.0),
    );
    let events =
        drive_weather_tool_loop(&route, &cassette, &request, &weather_tool_result_text).await;
    assert_weather_tool_loop(&events);
}

fn openrouter_model(model: &str) -> (ModelRef, Route<OpenAiChat>) {
    compatible_model(model, "https://openrouter.ai/api/v1")
}

#[tokio::test]
async fn openrouter_streams_text() {
    let (model, route) = openrouter_model("openai/gpt-4o-mini");
    let cassette = load_cassette("openai-compatible-chat/openrouter-streams-text");
    let request = text_request(
        model,
        "You are concise.",
        "Reply with exactly: Hello!",
        20.0,
        Some(0.0),
    );
    let events = replay_interaction(&route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&events), FinishReason::Stop);
    assert_eq!(response_text(&events), "Hello!");
}

#[tokio::test]
async fn openrouter_streams_tool_call() {
    let (model, route) = openrouter_model("openai/gpt-4o-mini");
    assert_streams_tool_call(
        model,
        &route,
        "openai-compatible-chat/openrouter-streams-tool-call",
        80.0,
    )
    .await;
}

const OPENROUTER_LOOP_SYSTEM: &str =
    "Use the get_weather tool exactly once, then answer in one short sentence.";

#[tokio::test]
async fn openrouter_gpt_4o_mini_drives_a_tool_loop() {
    let (model, route) = openrouter_model("openai/gpt-4o-mini");
    let cassette =
        load_cassette("openai-compatible-chat/openrouter-gpt-4o-mini-drives-a-tool-loop");
    let request = weather_tool_loop_request(model, OPENROUTER_LOOP_SYSTEM, 80.0, Some(0.0));
    let events =
        drive_weather_tool_loop(&route, &cassette, &request, &weather_tool_result_text).await;
    assert_weather_tool_loop(&events);
}

#[tokio::test]
async fn openrouter_gpt_5_5_drives_a_tool_loop() {
    let (model, route) = openrouter_model("openai/gpt-5.5");
    let cassette = load_cassette("openai-compatible-chat/openrouter-gpt-5-5-drives-a-tool-loop");
    let request = weather_tool_loop_request(model, OPENROUTER_LOOP_SYSTEM, 80.0, Some(0.0));
    let events =
        drive_weather_tool_loop(&route, &cassette, &request, &weather_tool_result_text).await;
    assert_weather_tool_loop(&events);
}

#[tokio::test]
async fn openrouter_claude_opus_4_7_drives_a_tool_loop() {
    let (model, route) = openrouter_model("anthropic/claude-opus-4.7");
    let cassette =
        load_cassette("openai-compatible-chat/openrouter-claude-opus-4-7-drives-a-tool-loop");
    let request = weather_tool_loop_request(model, OPENROUTER_LOOP_SYSTEM, 80.0, Some(0.0));
    let events =
        drive_weather_tool_loop(&route, &cassette, &request, &weather_tool_result_text).await;
    assert_weather_tool_loop(&events);
}

// =============================================================================
// gemini/
// =============================================================================

#[tokio::test]
async fn gemini_streams_text() {
    let cassette = load_cassette("gemini/streams-text");
    let route = gemini_route();
    let model = ModelRef::new(
        "gemini-2.5-flash",
        "google",
        Arc::new(gemini::route_handle()),
    );
    let request = text_request(
        model,
        "You are concise.",
        "Reply with exactly: Hello!",
        80.0,
        Some(0.0),
    );
    let events = replay_interaction(&route, &request, &cassette[0]).await;

    // §5.4 canonical sequence: one SSE frame, full text in one part.
    assert_eq!(
        kinds(&events),
        vec![
            "step-start",
            "text-start",
            "text-delta",
            "text-end",
            "step-finish",
            "finish",
        ]
    );
    match &events[2] {
        LlmEvent::TextDelta { id, text, .. } => {
            assert_eq!(id, "text-0");
            assert_eq!(text, "Hello!");
        }
        _ => unreachable!(),
    }
    let usage = finish_usage(&events[5]).unwrap();
    assert_eq!(usage.input_tokens, Some(11.0));
    assert_eq!(usage.non_cached_input_tokens, Some(11.0));
    assert_eq!(usage.cache_read_input_tokens, None);
    assert_eq!(usage.output_tokens, Some(18.0));
    assert_eq!(usage.reasoning_tokens, Some(16.0));
    assert_eq!(usage.total_tokens, Some(29.0));
    assert_usage_invariants(usage);
    assert_eq!(finish_reason(&events[5]), FinishReason::Stop);
}

#[tokio::test]
async fn gemini_streams_tool_call() {
    let cassette = load_cassette("gemini/streams-tool-call");
    let route = gemini_route();
    let model = ModelRef::new(
        "gemini-2.5-flash",
        "google",
        Arc::new(gemini::route_handle()),
    );
    let request = weather_tool_call_request(model, 80.0, Some(0.0));
    let events = replay_interaction(&route, &request, &cassette[0]).await;

    assert_eq!(assert_turn(&events), FinishReason::ToolCalls);
    assert_tool_call_shape(&events);
}

#[tokio::test]
async fn gemini_reads_image_text() {
    let cassette = load_cassette("gemini/gemini-2-5-flash-image");
    let route = gemini_route();
    let image = include_str!("support/restroom.png.b64");
    let model = ModelRef::new(
        "gemini-2.5-flash",
        "google",
        Arc::new(gemini::route_handle()),
    );
    let mut request = base_request(
        model,
        vec![system_part("Read images carefully. Reply only with the visible text.")],
        vec![Message::user(vec![
            ContentPart::text(
                "The image contains exactly three lowercase English words. Read them left to right and reply with only those words.",
            ),
            ContentPart::media("image/png", image),
        ])],
    );
    request.generation = Some(generation(160.0, Some(0.0)));
    let events = replay_interaction(&route, &request, &cassette[0]).await;

    assert_eq!(assert_turn(&events), FinishReason::Stop);
    assert_eq!(
        normalized_image_text(&response_text(&events)),
        "jiggling restroom prison"
    );
}

// =============================================================================
// gemini-cache/
// =============================================================================

#[tokio::test]
async fn gemini_cache_reports_cachedcontenttokencount() {
    let cassette =
        load_cassette("gemini-cache/reports-cachedcontenttokencount-on-identical-second-call");
    let route = gemini_route();
    let model = ModelRef::new(
        "gemini-2.5-flash",
        "google",
        Arc::new(gemini::route_handle()),
    );
    let mut request = base_request(
        model,
        vec![system_part(&large_cacheable_system())],
        vec![Message::user("Say hi.")],
    );
    request.generation = Some(generation(16.0, Some(0.0)));

    let first = replay_interaction(&route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&first), FinishReason::Stop);
    assert_eq!(response_text(&first), "Hi.");

    let second = replay_interaction(&route, &request, &cassette[1]).await;
    assert_eq!(assert_turn(&second), FinishReason::Stop);
    let usage = finish_usage(finishes(&second)[0]).unwrap();
    assert_eq!(usage.cache_read_input_tokens, Some(1100.0));
    assert_eq!(usage.input_tokens, Some(1200.0));
    assert_eq!(usage.non_cached_input_tokens, Some(100.0));
}

// =============================================================================
// openai-chat/
// =============================================================================

#[tokio::test]
async fn openai_chat_streams_text() {
    let cassette = load_cassette("openai-chat/streams-text");
    let route = openai_chat_route();
    let model = ModelRef::new(
        "gpt-4o-mini",
        "openai",
        Arc::new(openai_chat::route_handle()),
    );
    let request = text_request(
        model,
        "You are concise.",
        "Say hello in one short sentence.",
        20.0,
        Some(0.0),
    );
    let events = replay_interaction(&route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&events), FinishReason::Stop);
    assert_eq!(response_text(&events), "Hello!");
}

#[tokio::test]
async fn openai_chat_streams_tool_call() {
    let cassette = load_cassette("openai-chat/streams-tool-call");
    let route = openai_chat_route();
    let model = ModelRef::new(
        "gpt-4o-mini",
        "openai",
        Arc::new(openai_chat::route_handle()),
    );
    let request = weather_tool_call_request(model, 80.0, Some(0.0));
    let events = replay_interaction(&route, &request, &cassette[0]).await;
    // The AI SDK runtime reports the provider's finish_reason verbatim
    // (the stop → tool-calls upgrade was the experimental native
    // runtime's behavior; the tool call finalized eagerly above).
    assert_eq!(assert_turn(&events), FinishReason::Stop);
    assert_tool_call_shape(&events);
}

#[tokio::test]
async fn openai_chat_drives_a_tool_loop_end_to_end() {
    let cassette = load_cassette("openai-chat/drives-a-tool-loop-end-to-end");
    let route = openai_chat_route();
    let model = ModelRef::new(
        "gpt-4o-mini",
        "openai",
        Arc::new(openai_chat::route_handle()),
    );
    let request = weather_tool_loop_request(
        model,
        "Use the get_weather tool, then answer in one short sentence.",
        80.0,
        Some(0.0),
    );
    let events =
        drive_weather_tool_loop(&route, &cassette, &request, &weather_tool_result_text).await;
    assert_weather_tool_loop(&events);
}

#[tokio::test]
async fn openai_chat_continues_after_tool_result() {
    let cassette = load_cassette("openai-chat/continues-after-tool-result");
    let route = openai_chat_route();
    let model = ModelRef::new(
        "gpt-4o-mini",
        "openai",
        Arc::new(openai_chat::route_handle()),
    );
    let mut request = base_request(
        model,
        vec![system_part("Answer using only the provided tool result.")],
        vec![
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
                result_type: None,
                provider_executed: None,
                cache: None,
                metadata: None,
                provider_metadata: None,
            }),
        ],
    );
    request.generation = Some(generation(40.0, Some(0.0)));
    let events = replay_interaction(&route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&events), FinishReason::Stop);
    let text = response_text(&events);
    assert!(text.contains("Paris"), "text mentions Paris, got {text:?}");
    assert!(text.contains("sunny"), "text mentions sunny, got {text:?}");
}

// =============================================================================
// openai-responses/
// =============================================================================

fn responses_model(model: &str) -> ModelRef {
    ModelRef::new(model, "openai", Arc::new(openai_responses::route_handle()))
}

#[tokio::test]
async fn openai_responses_streams_text() {
    let cassette = load_cassette("openai-responses/gpt-5-5-streams-text");
    let route = openai_responses_route();
    let request = text_request(
        responses_model("gpt-5.5"),
        "You are concise.",
        "Reply with exactly: Hello!",
        80.0,
        None,
    );
    let events = replay_interaction(&route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&events), FinishReason::Stop);
    assert_eq!(response_text(&events), "Hello!");
}

#[tokio::test]
async fn openai_responses_streams_tool_call() {
    let cassette = load_cassette("openai-responses/gpt-5-5-streams-tool-call");
    let route = openai_responses_route();
    let request = weather_tool_call_request(responses_model("gpt-5.5"), 80.0, None);
    let events = replay_interaction(&route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&events), FinishReason::ToolCalls);
    assert_tool_call_shape(&events);
}

#[tokio::test]
async fn openai_responses_drives_a_tool_loop() {
    let cassette = load_cassette("openai-responses/gpt-5-5-drives-a-tool-loop");
    let route = openai_responses_route();
    let request = weather_tool_loop_request(
        responses_model("gpt-5.5"),
        "Use the get_weather tool, then answer in one short sentence.",
        80.0,
        None,
    );
    let events =
        drive_weather_tool_loop(&route, &cassette, &request, &weather_tool_result_text).await;
    assert_weather_tool_loop(&events);
}

fn encrypted_reasoning_options() -> Option<ProviderOptions> {
    openai_options(json!({
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "reasoningEffort": "low",
        "reasoningSummary": "auto",
        "textVerbosity": "low",
    }))
}

#[tokio::test]
async fn openai_responses_image_tool_result() {
    let cassette = load_cassette("openai-responses/openai-responses-gpt-5-5-image-tool-result");
    let route = openai_responses_route();
    let image = include_str!("support/restroom.png.b64");
    let mut request = base_request(
        responses_model("gpt-5.5"),
        vec![system_part(
            "Read images carefully. Reply only with the visible text, lowercase, no punctuation.",
        )],
        vec![
            Message::user("Use the read_screenshot tool, then reply with the words shown."),
            Message::assistant(vec![ContentPart::tool_call(
                "call_screenshot_1",
                "read_screenshot",
                json!({}),
            )]),
            Message::tool(ToolResultInput {
                id: "call_screenshot_1".to_string(),
                name: "read_screenshot".to_string(),
                result: json!([
                    {"type": "text", "text": "Image read successfully"},
                    {"type": "file", "uri": format!("data:image/png;base64,{image}"), "mime": "image/png"},
                ]),
                result_type: Some(ToolResultType::Content),
                provider_executed: None,
                cache: None,
                metadata: None,
                provider_metadata: None,
            }),
        ],
    );
    request.tools = vec![read_screenshot_tool()];
    request.generation = Some(generation(40.0, None));
    request.provider_options = openai_options(json!({
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "reasoningEffort": "medium",
        "reasoningSummary": "auto",
        "textVerbosity": "low",
    }));
    let events = replay_interaction(&route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&events), FinishReason::Stop);
    assert_eq!(
        normalized_image_text(&response_text(&events)),
        "jiggling restroom prison"
    );
}

#[tokio::test]
async fn openai_responses_uses_reasoning() {
    let cassette = load_cassette("openai-responses/openai-responses-gpt-5-5-reasoning");
    let route = openai_responses_route();
    let mut request = base_request(
        responses_model("gpt-5.5"),
        vec![system_part(
            "Show concise reasoning when the provider supports visible reasoning summaries.",
        )],
        vec![Message::user(
            "Think briefly, then reply exactly with: Hello!",
        )],
    );
    request.generation = Some(generation(120.0, None));
    request.provider_options = encrypted_reasoning_options();
    let events = replay_interaction(&route, &request, &cassette[0]).await;

    assert_eq!(assert_turn(&events), FinishReason::Stop);
    assert_eq!(response_text(&events), "Hello!");
    let usage = finish_usage(finishes(&events)[0]).unwrap();
    assert!(
        usage.reasoning_tokens.unwrap_or(0.0) > 0.0,
        "reasoningTokens > 0"
    );
}

#[tokio::test]
async fn openai_responses_continues_encrypted_reasoning() {
    let cassette =
        load_cassette("openai-responses/openai-responses-gpt-5-5-reasoning-continuation");
    let route = openai_responses_route();

    let mut first_request = base_request(
        responses_model("gpt-5.5"),
        vec![system_part(
            "Show concise reasoning when the provider supports visible reasoning summaries.",
        )],
        vec![Message::user(
            "Think briefly, then reply exactly with: Hello!",
        )],
    );
    first_request.generation = Some(generation(120.0, None));
    first_request.provider_options = encrypted_reasoning_options();

    let first = replay_interaction(&route, &first_request, &cassette[0]).await;
    assert_eq!(assert_turn(&first), FinishReason::Stop);
    assert_eq!(response_text(&first), "Hello!");

    // assistantMessageFromResponse: replay the encrypted reasoning state.
    let reasoning_end = first.iter().rev().find(|event| match event {
        LlmEvent::ReasoningEnd {
            provider_metadata: Some(metadata),
            ..
        } => metadata
            .get("openai")
            .and_then(|openai| openai.get("itemId"))
            .and_then(Value::as_str)
            .is_some(),
        _ => false,
    });
    let reasoning_metadata = match reasoning_end {
        Some(LlmEvent::ReasoningEnd {
            provider_metadata: Some(metadata),
            ..
        }) => metadata.clone(),
        _ => panic!("reasoning-end with openai.itemId missing: {first:?}"),
    };
    match reasoning_metadata.get("openai") {
        Some(openai) => assert!(openai.get("reasoningEncryptedContent").is_some()),
        _ => panic!("missing openai metadata"),
    }
    let assistant = Message::assistant(vec![
        ContentPart::Reasoning {
            text: response_reasoning(&first),
            encrypted: None,
            metadata: None,
            provider_metadata: Some(reasoning_metadata),
        },
        ContentPart::text(response_text(&first)),
    ]);

    let mut second_request = base_request(
        responses_model("gpt-5.5"),
        Vec::new(),
        vec![
            Message::user("Think briefly, then reply exactly with: Hello!"),
            assistant,
            Message::user("Now reply exactly with: Done."),
        ],
    );
    second_request.generation = Some(generation(40.0, None));
    second_request.provider_options = encrypted_reasoning_options();

    let second = replay_interaction(&route, &second_request, &cassette[1]).await;
    assert_eq!(assert_turn(&second), FinishReason::Stop);
    assert_eq!(response_text(&second), "Done.");
}

// =============================================================================
// openai-responses-cache/
// =============================================================================

#[tokio::test]
async fn openai_responses_cache_reports_cached_tokens() {
    let cassette =
        load_cassette("openai-responses-cache/reports-cached-tokens-on-identical-second-call");
    let route = openai_responses_route();
    let mut request = base_request(
        responses_model("gpt-4.1-mini"),
        vec![system_part(&large_cacheable_system())],
        vec![Message::user("Say hi.")],
    );
    request.generation = Some(generation(16.0, Some(0.0)));
    request.provider_options =
        openai_options(json!({"promptCacheKey": "recorded-cache-test", "store": false}));

    let first = replay_interaction(&route, &request, &cassette[0]).await;
    assert_eq!(assert_turn(&first), FinishReason::Stop);
    assert_eq!(response_text(&first), "Hi.");

    let second = replay_interaction(&route, &request, &cassette[1]).await;
    assert_eq!(assert_turn(&second), FinishReason::Stop);
    let usage = finish_usage(finishes(&second)[0]).unwrap();
    assert_eq!(usage.cache_read_input_tokens, Some(4608.0));
    assert_eq!(usage.input_tokens, Some(4765.0));
    assert_eq!(usage.non_cached_input_tokens, Some(157.0));
}
