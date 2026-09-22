//! Streaming tool-call JSON accumulator (TS `protocols/utils/tool-stream.ts`).
//!
//! `State` is keyed by the provider's *stream-local* tool identifier — numeric
//! content index for anthropic/bedrock/openai-chat, string item id for
//! openai-responses — never the final tool-call id.

// `LlmError` (spec §2.5) is this crate's error channel; boxing it would
// deviate from the schema type, so the size lint is allowed instead.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;

use crate::protocols::shared::{event_error, parse_tool_input};
use crate::schema::errors::LlmError;
use crate::schema::events::LlmEvent;
use crate::schema::ids::ProviderMetadata;

/// One pending streamed tool call. Providers emit the tool identity and JSON
/// argument text across separate chunks; `input` is the raw JSON string
/// collected so far, not the parsed object.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingTool {
    /// Final tool-call id (`"toolu_…"`, `"call_…"`, `"tool_0"`, …).
    pub id: String,
    pub name: String,
    /// Accumulated raw JSON argument text — NOT parsed.
    pub input: String,
    pub provider_executed: Option<bool>,
    pub provider_metadata: Option<ProviderMetadata>,
}

/// Sparse parser state keyed by the provider's stream-local tool identifier.
///
/// This key is not the final tool-call id (`call_...`). It is the id/index the
/// provider uses while streaming a partial call: OpenAI Chat / Anthropic /
/// Bedrock use numeric content indexes, while OpenAI Responses uses string
/// `item_id`s. The generic keeps each protocol internally consistent.
pub type State<K> = BTreeMap<K, PendingTool>;

/// Result of adding argument text to one pending tool call. It returns both
/// the next `tools` state and the updated `tool` because parsers often need
/// the current id/name immediately. `events` contains lifecycle and delta
/// events produced by the append; metadata-only deltas update identity
/// without output.
#[derive(Debug, Clone, PartialEq)]
pub struct AppendOutcome<K> {
    pub tools: State<K>,
    pub tool: PendingTool,
    pub events: Vec<LlmEvent>,
}

/// Result of a `finish*` call: the next state plus its emitted events
/// (empty when the key was not pending).
#[derive(Debug, Clone, PartialEq)]
pub struct FinishOutcome<K> {
    pub tools: State<K>,
    pub events: Vec<LlmEvent>,
}

/// Identity + argument text for [`append_or_start`]: the id/name may only
/// appear on the first delta for a given key.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDelta {
    pub id: Option<String>,
    pub name: Option<String>,
    pub text: String,
}

/// Create empty accumulator state for one provider stream.
pub fn empty<K>() -> State<K> {
    BTreeMap::new()
}

/// Register a tool call whose start event arrived before any argument deltas.
/// Used by Anthropic `content_block_start`, Bedrock `contentBlockStart`, and
/// OpenAI Responses `response.output_item.added`.
///
/// TS types the input as `Omit<PendingTool, "input">` with optional `input`;
/// Rust callers defaulting to streamed arguments pass `input: String::new()`.
pub fn start<K: Ord>(mut tools: State<K>, key: K, tool: PendingTool) -> State<K> {
    tools.insert(key, tool);
    tools
}

/// Append a streamed argument delta, starting the tool if this provider
/// encodes identity on the first delta instead of a separate start event.
/// OpenAI Chat has this shape: `tool_calls[].index` is the stream key, and
/// `id` / `name` may only appear on the first delta for that index.
pub fn append_or_start<K: Ord>(
    route: &str,
    tools: State<K>,
    key: K,
    delta: ToolDelta,
    missing_tool_message: &str,
) -> Result<AppendOutcome<K>, LlmError> {
    let current = tools.get(&key).cloned();
    let id = delta
        .id
        .clone()
        .or_else(|| current.as_ref().map(|tool| tool.id.clone()));
    let name = delta
        .name
        .clone()
        .or_else(|| current.as_ref().map(|tool| tool.name.clone()));
    let (Some(id), Some(name)) = (id, name) else {
        return Err(event_error(route, missing_tool_message, None));
    };
    if id.is_empty() || name.is_empty() {
        return Err(event_error(route, missing_tool_message, None));
    }

    let tool = PendingTool {
        id: id.clone(),
        name: name.clone(),
        input: format!(
            "{}{}",
            current
                .as_ref()
                .map(|tool| tool.input.clone())
                .unwrap_or_default(),
            delta.text
        ),
        provider_executed: current.as_ref().and_then(|tool| tool.provider_executed),
        provider_metadata: current
            .as_ref()
            .and_then(|tool| tool.provider_metadata.clone()),
    };
    if let Some(current) = &current {
        if delta.text.is_empty() && current.id == id && current.name == name {
            return Ok(AppendOutcome {
                tools,
                tool: current.clone(),
                events: Vec::new(),
            });
        }
    }
    Ok(append_tool(tools, key, tool, &delta.text))
}

/// Append argument text to a tool that must already have been started. This
/// keeps protocols honest when their stream grammar promises a start event
/// before any argument delta.
pub fn append_existing<K: Ord>(
    route: &str,
    tools: State<K>,
    key: K,
    text: String,
    missing_tool_message: &str,
) -> Result<AppendOutcome<K>, LlmError> {
    let Some(current) = tools.get(&key).cloned() else {
        return Err(event_error(route, missing_tool_message, None));
    };
    if text.is_empty() {
        return Ok(AppendOutcome {
            tools,
            tool: current,
            events: Vec::new(),
        });
    }
    let mut tool = current.clone();
    tool.input.push_str(&text);
    Ok(append_tool(tools, key, tool, &text))
}

/// Finalize one pending tool call: parse the accumulated raw JSON, remove it
/// from state, and return the optional public `tool-call` event. Missing keys
/// are a no-op because some providers emit stop events for non-tool content
/// blocks.
pub fn finish<K: Ord>(
    route: &str,
    mut tools: State<K>,
    key: K,
) -> Result<FinishOutcome<K>, LlmError> {
    let Some(tool) = tools.remove(&key) else {
        return Ok(FinishOutcome {
            tools,
            events: Vec::new(),
        });
    };
    let events = vec![input_end(&tool), tool_call_event(route, &tool, None)?];
    Ok(FinishOutcome { tools, events })
}

/// Finalize one pending tool call with an authoritative final input string.
/// OpenAI Responses can send accumulated deltas and then repeat the completed
/// arguments on `response.output_item.done`; the final value wins.
pub fn finish_with_input<K: Ord>(
    route: &str,
    mut tools: State<K>,
    key: K,
    input: &str,
) -> Result<FinishOutcome<K>, LlmError> {
    let Some(tool) = tools.remove(&key) else {
        return Ok(FinishOutcome {
            tools,
            events: Vec::new(),
        });
    };
    let events = vec![
        input_end(&tool),
        tool_call_event(route, &tool, Some(input))?,
    ];
    Ok(FinishOutcome { tools, events })
}

/// Finalize every pending tool call at once. OpenAI Chat has this shape: it
/// does not emit per-tool stop events, so all accumulated calls finish when
/// the choice receives a terminal `finish_reason`.
///
/// The backing `BTreeMap` iterates in ascending key order, which matches the
/// TS object spread's numeric-ascending iteration for the numeric keys this
/// path serves.
pub fn finish_all<K: Ord>(route: &str, tools: State<K>) -> Result<FinishOutcome<K>, LlmError> {
    let mut events = Vec::new();
    for tool in tools.values() {
        events.push(input_end(tool));
        events.push(tool_call_event(route, tool, None)?);
    }
    Ok(FinishOutcome {
        tools: BTreeMap::new(),
        events,
    })
}

/// Store the updated tool and produce the optional public delta event.
fn append_tool<K: Ord>(
    mut tools: State<K>,
    key: K,
    tool: PendingTool,
    text: &str,
) -> AppendOutcome<K> {
    let mut events = Vec::new();
    if !tools.contains_key(&key) {
        events.push(input_start(&tool));
    }
    if !text.is_empty() {
        events.push(input_delta(&tool, text));
    }
    tools.insert(key, tool.clone());
    AppendOutcome {
        tools,
        tool,
        events,
    }
}

fn input_start(tool: &PendingTool) -> LlmEvent {
    LlmEvent::ToolInputStart {
        id: tool.id.clone(),
        name: tool.name.clone(),
        provider_metadata: tool.provider_metadata.clone(),
    }
}

fn input_delta(tool: &PendingTool, text: &str) -> LlmEvent {
    LlmEvent::ToolInputDelta {
        id: tool.id.clone(),
        name: tool.name.clone(),
        text: text.to_string(),
    }
}

fn input_end(tool: &PendingTool) -> LlmEvent {
    LlmEvent::ToolInputEnd {
        id: tool.id.clone(),
        name: tool.name.clone(),
        provider_metadata: tool.provider_metadata.clone(),
    }
}

fn tool_call_event(
    route: &str,
    tool: &PendingTool,
    input_override: Option<&str>,
) -> Result<LlmEvent, LlmError> {
    let raw = input_override.unwrap_or(&tool.input);
    let input = parse_tool_input(route, &tool.name, raw)?;
    Ok(LlmEvent::ToolCall {
        id: tool.id.clone(),
        name: tool.name.clone(),
        input,
        // TS: `providerExecuted ? true : undefined` — falsy values normalize
        // away.
        provider_executed: tool.provider_executed.filter(|executed| *executed),
        provider_metadata: tool.provider_metadata.clone(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::schema::errors::LlmErrorReason;

    use super::*;

    fn pending(id: &str, name: &str) -> PendingTool {
        PendingTool {
            id: id.to_string(),
            name: name.to_string(),
            input: String::new(),
            provider_executed: None,
            provider_metadata: None,
        }
    }

    #[test]
    fn start_append_finish_emits_the_full_tool_input_sequence() {
        // OpenAI-chat shape: identity arrives on the first argument delta, so
        // the first append emits `tool-input-start` + `tool-input-delta`.
        let outcome = append_or_start(
            "anthropic-messages",
            empty(),
            0u32,
            ToolDelta {
                id: Some("toolu_1".to_string()),
                name: Some("get_weather".to_string()),
                text: "{\"city\":\"Paris\"}".to_string(),
            },
            "missing tool",
        )
        .unwrap();
        let mut events = outcome.events;
        let finished = finish("anthropic-messages", outcome.tools, 0u32).unwrap();
        events.extend(finished.events);

        assert_eq!(
            events,
            vec![
                LlmEvent::ToolInputStart {
                    id: "toolu_1".to_string(),
                    name: "get_weather".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::ToolInputDelta {
                    id: "toolu_1".to_string(),
                    name: "get_weather".to_string(),
                    text: "{\"city\":\"Paris\"}".to_string(),
                },
                LlmEvent::ToolInputEnd {
                    id: "toolu_1".to_string(),
                    name: "get_weather".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::ToolCall {
                    id: "toolu_1".to_string(),
                    name: "get_weather".to_string(),
                    input: json!({"city": "Paris"}),
                    provider_executed: None,
                    provider_metadata: None,
                },
            ],
        );
    }

    #[test]
    fn append_existing_after_start_emits_only_the_delta() {
        // Anthropic shape: `content_block_start` pre-registers the tool and
        // emits `tool-input-start` itself, so appendExisting only emits
        // `tool-input-delta`.
        let tools = start(empty(), 0u32, pending("toolu_1", "get_weather"));
        let outcome = append_existing(
            "anthropic-messages",
            tools,
            0u32,
            "{\"city\":\"Paris\"}".to_string(),
            "missing tool",
        )
        .unwrap();
        assert_eq!(
            outcome.events,
            vec![LlmEvent::ToolInputDelta {
                id: "toolu_1".to_string(),
                name: "get_weather".to_string(),
                text: "{\"city\":\"Paris\"}".to_string(),
            }],
        );
    }

    #[test]
    fn empty_input_parses_to_an_empty_object() {
        let tools = start(empty(), "item_1", pending("tool_0", "ping"));
        let finished = finish("openai-responses", tools, "item_1").unwrap();
        assert!(finished.tools.is_empty());
        assert_eq!(
            finished.events,
            vec![
                LlmEvent::ToolInputEnd {
                    id: "tool_0".to_string(),
                    name: "ping".to_string(),
                    provider_metadata: None,
                },
                LlmEvent::ToolCall {
                    id: "tool_0".to_string(),
                    name: "ping".to_string(),
                    input: json!({}),
                    provider_executed: None,
                    provider_metadata: None,
                },
            ],
        );
    }

    #[test]
    fn append_or_start_accumulates_split_json_deltas() {
        let outcome = append_or_start(
            "openai-chat",
            empty(),
            0u32,
            ToolDelta {
                id: Some("call_1".to_string()),
                name: Some("lookup".to_string()),
                text: "{\"query\":".to_string(),
            },
            "missing tool",
        )
        .unwrap();
        // First delta for a key: start + delta.
        assert_eq!(outcome.events.len(), 2);
        assert!(matches!(outcome.events[0], LlmEvent::ToolInputStart { .. }));
        assert!(matches!(outcome.events[1], LlmEvent::ToolInputDelta { .. }));

        let outcome = append_or_start(
            "openai-chat",
            outcome.tools,
            0u32,
            ToolDelta {
                id: None,
                name: None,
                text: "\"weather\"}".to_string(),
            },
            "missing tool",
        )
        .unwrap();
        // Identity may only appear on the first delta; no duplicate start.
        assert_eq!(outcome.events.len(), 1);
        assert!(matches!(outcome.events[0], LlmEvent::ToolInputDelta { .. }));

        let finished = finish("openai-chat", outcome.tools, 0u32).unwrap();
        match &finished.events[1] {
            LlmEvent::ToolCall { input, .. } => assert_eq!(input, &json!({"query": "weather"})),
            event => panic!("expected a tool-call, got {event:?}"),
        }
    }

    #[test]
    fn append_or_start_with_a_blank_name_is_an_error() {
        let tools = start(empty(), 0u32, pending("call_1", "lookup"));
        let error = append_or_start(
            "openai-chat",
            tools,
            1u32,
            ToolDelta {
                id: Some("call_2".to_string()),
                name: Some(String::new()),
                text: String::new(),
            },
            "missing tool",
        )
        .unwrap_err();
        assert!(matches!(
            error.reason,
            LlmErrorReason::InvalidProviderOutput { .. }
        ));
    }

    #[test]
    fn append_or_start_metadata_only_delta_is_a_no_op() {
        let tools = start(empty(), 0u32, pending("call_1", "lookup"));
        let outcome = append_or_start(
            "openai-chat",
            tools,
            0u32,
            ToolDelta {
                id: Some("call_1".to_string()),
                name: Some("lookup".to_string()),
                text: String::new(),
            },
            "missing tool",
        )
        .unwrap();
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.tool.input, "");
        assert_eq!(outcome.tools.get(&0).unwrap().input, "");
    }

    #[test]
    fn append_or_start_without_identity_is_an_error() {
        let error = append_or_start(
            "openai-chat",
            empty(),
            0u32,
            ToolDelta {
                id: None,
                name: None,
                text: "{}".to_string(),
            },
            "tool call id and name are required",
        )
        .unwrap_err();
        assert_eq!(error.module, "ProviderShared");
        assert_eq!(error.method, "stream");
        match error.reason {
            LlmErrorReason::InvalidProviderOutput { message, route, .. } => {
                assert_eq!(message, "tool call id and name are required");
                assert_eq!(route.as_deref(), Some("openai-chat"));
            }
            reason => panic!("expected an InvalidProviderOutput reason, got {reason:?}"),
        }
    }

    #[test]
    fn append_existing_for_an_unknown_key_is_an_error() {
        let error = append_or_start(
            "anthropic-messages",
            empty(),
            0u32,
            ToolDelta {
                id: Some("toolu_1".to_string()),
                name: Some("get_weather".to_string()),
                text: String::new(),
            },
            "missing tool",
        );
        // start-first shape: the tool exists, so this appends fine.
        assert!(error.is_ok());

        let error = append_existing(
            "anthropic-messages",
            empty(),
            7u32,
            "{}".to_string(),
            "tool call not started",
        )
        .unwrap_err();
        match error.reason {
            LlmErrorReason::InvalidProviderOutput { message, .. } => {
                assert_eq!(message, "tool call not started");
            }
            reason => panic!("expected an InvalidProviderOutput reason, got {reason:?}"),
        }
    }

    #[test]
    fn empty_text_append_existing_is_a_no_op() {
        let tools = start(empty(), 0u32, pending("toolu_1", "get_weather"));
        let outcome = append_existing(
            "anthropic-messages",
            tools,
            0u32,
            String::new(),
            "missing tool",
        )
        .unwrap();
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.tool.input, "");
    }

    #[test]
    fn finish_with_a_missing_key_is_a_no_op() {
        let tools = start(empty(), 0u32, pending("toolu_1", "get_weather"));
        let finished = finish("anthropic-messages", tools, 1u32).unwrap();
        assert!(finished.events.is_empty());
        // State is untouched.
        assert!(finished.tools.contains_key(&0));
    }

    #[test]
    fn finish_with_input_overrides_accumulated_deltas() {
        let outcome = append_or_start(
            "openai-responses",
            empty(),
            "item_1",
            ToolDelta {
                id: Some("call_1".to_string()),
                name: Some("lookup".to_string()),
                text: "accumulated-but-stal".to_string(),
            },
            "missing tool",
        )
        .unwrap();
        let finished =
            finish_with_input("openai-responses", outcome.tools, "item_1", "{\"ok\":true}")
                .unwrap();
        match &finished.events[1] {
            LlmEvent::ToolCall { input, .. } => assert_eq!(input, &json!({"ok": true})),
            event => panic!("expected a tool-call, got {event:?}"),
        }
    }

    #[test]
    fn finish_all_finalizes_every_pending_tool_in_key_order() {
        let first = append_or_start(
            "openai-chat",
            empty(),
            0u32,
            ToolDelta {
                id: Some("call_1".to_string()),
                name: Some("first".to_string()),
                text: "{\"a\":".to_string(),
            },
            "missing tool",
        )
        .unwrap();
        let second = append_or_start(
            "openai-chat",
            first.tools,
            1u32,
            ToolDelta {
                id: Some("call_2".to_string()),
                name: Some("second".to_string()),
                text: "{\"ok\":true}".to_string(),
            },
            "missing tool",
        )
        .unwrap();
        let second = append_or_start(
            "openai-chat",
            second.tools,
            0u32,
            ToolDelta {
                id: None,
                name: None,
                text: "1}".to_string(),
            },
            "missing tool",
        )
        .unwrap();

        let finished = finish_all("openai-chat", second.tools).unwrap();
        assert!(finished.tools.is_empty());
        assert_eq!(finished.events.len(), 4);
        assert!(matches!(finished.events[0], LlmEvent::ToolInputEnd { .. }));
        assert!(matches!(finished.events[1], LlmEvent::ToolCall { .. }));
        assert!(matches!(finished.events[2], LlmEvent::ToolInputEnd { .. }));
        assert!(matches!(finished.events[3], LlmEvent::ToolCall { .. }));
        match &finished.events[1] {
            LlmEvent::ToolCall { input, .. } => assert_eq!(input, &json!({"a": 1})),
            event => panic!("expected a tool-call, got {event:?}"),
        }
    }

    #[test]
    fn provider_executed_normalizes_falsy_away() {
        let mut tools = empty();
        tools.insert(
            0u32,
            PendingTool {
                id: "toolu_1".to_string(),
                name: "web_search".to_string(),
                input: "{}".to_string(),
                provider_executed: Some(false),
                provider_metadata: None,
            },
        );
        let finished = finish("anthropic-messages", tools, 0u32).unwrap();
        match &finished.events[1] {
            LlmEvent::ToolCall {
                provider_executed, ..
            } => assert_eq!(*provider_executed, None),
            event => panic!("expected a tool-call, got {event:?}"),
        }
    }

    #[test]
    fn invalid_json_input_is_an_invalid_provider_output_error() {
        let tools = start(empty(), 0u32, pending("toolu_1", "get_weather"));
        let outcome = append_existing(
            "anthropic-messages",
            tools,
            0u32,
            "{\"city\"".to_string(),
            "missing tool",
        )
        .unwrap();
        let error = finish("anthropic-messages", outcome.tools, 0u32).unwrap_err();
        match error.reason {
            LlmErrorReason::InvalidProviderOutput {
                message,
                route,
                raw,
                ..
            } => {
                assert_eq!(
                    message,
                    "Invalid JSON input for anthropic-messages tool call get_weather"
                );
                assert_eq!(route.as_deref(), Some("anthropic-messages"));
                assert_eq!(raw.as_deref(), Some("{\"city\""));
            }
            reason => panic!("expected an InvalidProviderOutput reason, got {reason:?}"),
        }
    }
}
