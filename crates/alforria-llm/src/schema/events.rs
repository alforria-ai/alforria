//! Usage, the `LlmEvent` stream union, and the `LlmResponse` reducer
//! (from `schema/events.ts`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::errors::ProviderFailureClassification;
use super::ids::{ContentBlockId, FinishReason, ProviderMetadata, ToolCallId};
use super::messages::{ContentPart, Message, ToolOutput, ToolResultValue};

/// Token usage reported by an LLM provider.
///
/// **Inclusive totals** (match AI SDK / OpenAI / LangChain convention — a
/// reader from any of those ecosystems sees the number they expect):
///
/// - `inputTokens` — total prompt tokens, *including* cached reads/writes.
/// - `outputTokens` — total output tokens, *including* reasoning.
/// - `totalTokens` — provider-supplied total, or `inputTokens + outputTokens`.
///
/// **Non-overlapping breakdown** (every field is independently meaningful;
/// consumers never have to subtract):
///
/// - `nonCachedInputTokens` — the "fresh" portion of the prompt.
/// - `cacheReadInputTokens` — input tokens served from cache.
/// - `cacheWriteInputTokens` — input tokens written to cache.
/// - `reasoningTokens` — subset of `outputTokens` spent on hidden reasoning.
///
/// **Invariant**: `nonCachedInputTokens + cacheReadInputTokens +
/// cacheWriteInputTokens = inputTokens`, and `reasoningTokens <= outputTokens`.
/// Each protocol mapper computes whichever side it doesn't get natively,
/// with `max(0, …)` clamping for defense against provider bugs. Because
/// every breakdown field is stored independently, downstream consumers can
/// read whatever they need (cost-by-category, context-pressure, AI-SDK-style
/// inclusive total) without ever subtracting — eliminating the underflow
/// class of bug where a clamped difference would silently store the wrong
/// value.
///
/// **Semantics by provider**:
///
/// - OpenAI Chat / Responses / Gemini / Bedrock: provider reports inclusive
///   `inputTokens` and an inclusive `outputTokens`; mapper subtracts to
///   derive the breakdown.
/// - Anthropic: provider reports the breakdown natively (`input_tokens` is
///   non-cached only); mapper sums to derive the inclusive `inputTokens`.
///   Anthropic does *not* break extended-thinking out of `output_tokens`, so
///   `reasoningTokens` is `undefined` and `outputTokens` carries the
///   combined total — a documented limitation of the Anthropic API.
///
/// `providerMetadata` always carries the provider's raw usage payload —
/// keyed by provider name (`{ openai: ... }`, `{ anthropic: ... }`, etc.)
/// — for fields we don't normalize and for billing-level audit trails.
/// Matches the same escape-hatch field on `LlmEvent`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Total prompt tokens, *including* cached reads/writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<f64>,
    /// Total output tokens, *including* reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<f64>,
    /// The "fresh" portion of the prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_cached_input_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<f64>,
    /// Subset of `output_tokens` spent on hidden reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<f64>,
    /// Provider-supplied total, or `input_tokens + output_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<f64>,
    /// Raw provider usage payload, keyed by provider name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<ProviderMetadata>,
}

impl Usage {
    /// Visible output tokens — `outputTokens` minus `reasoningTokens`, clamped
    /// to zero. The one place subtraction happens in this contract; the clamp
    /// means a provider reporting `reasoningTokens > outputTokens` produces a
    /// harmless zero rather than a negative that crashes downstream schemas.
    pub fn visible_output_tokens(&self) -> f64 {
        (self.output_tokens.unwrap_or(0.0) - self.reasoning_tokens.unwrap_or(0.0)).max(0.0)
    }
}

/// Provider-neutral LLM stream event (TS `LLM.LLMEvent` tagged union).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[serde(rename_all_fields = "camelCase")]
pub enum LlmEvent {
    StepStart {
        index: f64,
    },
    TextStart {
        id: ContentBlockId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    TextDelta {
        id: ContentBlockId,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    TextEnd {
        id: ContentBlockId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ReasoningStart {
        id: ContentBlockId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ReasoningDelta {
        id: ContentBlockId,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ReasoningEnd {
        id: ContentBlockId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolInputStart {
        id: ToolCallId,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolInputDelta {
        id: ToolCallId,
        name: String,
        text: String,
    },
    ToolInputEnd {
        id: ToolCallId,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolCall {
        id: ToolCallId,
        name: String,
        input: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolResult {
        id: ToolCallId,
        name: String,
        result: ToolResultValue,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<ToolOutput>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolError {
        id: ToolCallId,
        name: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    StepFinish {
        index: f64,
        reason: FinishReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    Finish {
        reason: FinishReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ProviderError {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        classification: Option<ProviderFailureClassification>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retryable: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
}

/// Bookkeeping for an assembled text or reasoning content part.
#[derive(Debug, Clone, PartialEq)]
pub struct ContentAssembly {
    pub content_index: usize,
    pub text: String,
    pub provider_metadata: Option<ProviderMetadata>,
}

/// Bookkeeping for a tool input being streamed in.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInputAssembly {
    pub name: String,
    pub text: String,
    pub provider_metadata: Option<ProviderMetadata>,
}

/// TS `LLMResponse.State` — reducer state for assembling one provider
/// attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseState {
    pub events: Vec<LlmEvent>,
    pub message: Message,
    pub usage: Option<Usage>,
    pub finish_reason: Option<FinishReason>,
    pub text_parts: BTreeMap<String, ContentAssembly>,
    pub reasoning_parts: BTreeMap<String, ContentAssembly>,
    pub tool_inputs: BTreeMap<String, ToolInputAssembly>,
}

/// TS `LLM.Response` — a completed provider turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmResponse {
    pub message: Message,
    pub events: Vec<LlmEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub finish_reason: FinishReason,
}

impl LlmResponse {
    /// Initial reducer state for assembling one provider attempt.
    pub fn empty() -> ResponseState {
        ResponseState {
            events: Vec::new(),
            message: Message::assistant(Vec::new()),
            usage: None,
            finish_reason: None,
            text_parts: BTreeMap::new(),
            reasoning_parts: BTreeMap::new(),
            tool_inputs: BTreeMap::new(),
        }
    }

    /// Purely fold one provider-neutral event into the attempt assembly
    /// state.
    pub fn reduce(mut state: ResponseState, event: LlmEvent) -> ResponseState {
        append_event(&mut state, &event);
        match event {
            LlmEvent::TextStart {
                id,
                provider_metadata,
            } => ensure_text(&mut state, &id, provider_metadata),
            LlmEvent::TextDelta {
                id,
                text,
                provider_metadata,
            } => reduce_text_delta(&mut state, &id, &text, provider_metadata),
            LlmEvent::TextEnd {
                id,
                provider_metadata,
            } => reduce_text_end(&mut state, &id, provider_metadata),
            LlmEvent::ReasoningStart {
                id,
                provider_metadata,
            } => ensure_reasoning(&mut state, &id, provider_metadata),
            LlmEvent::ReasoningDelta {
                id,
                text,
                provider_metadata,
            } => reduce_reasoning_delta(&mut state, &id, &text, provider_metadata),
            LlmEvent::ReasoningEnd {
                id,
                provider_metadata,
            } => reduce_reasoning_end(&mut state, &id, provider_metadata),
            LlmEvent::ToolInputStart {
                id,
                name,
                provider_metadata,
            } => reduce_tool_input_start(&mut state, &id, &name, provider_metadata),
            LlmEvent::ToolInputDelta { id, name, text } => {
                reduce_tool_input_delta(&mut state, &id, &name, &text)
            }
            LlmEvent::ToolInputEnd {
                id,
                name,
                provider_metadata,
            } => reduce_tool_input_end(&mut state, &id, &name, provider_metadata),
            LlmEvent::ToolCall {
                id,
                name,
                input,
                provider_executed,
                provider_metadata,
            } => {
                state.tool_inputs.remove(&id);
                state.message.content.push(ContentPart::ToolCall {
                    id,
                    name,
                    input,
                    provider_executed,
                    metadata: None,
                    provider_metadata,
                });
            }
            LlmEvent::ToolResult {
                id,
                name,
                result,
                provider_executed,
                provider_metadata,
                ..
            } => {
                state.message.content.push(ContentPart::ToolResult {
                    id,
                    name,
                    result,
                    provider_executed,
                    cache: None,
                    metadata: None,
                    provider_metadata,
                });
            }
            _ => {}
        }
        state
    }

    /// Return a completed response only after a terminal finish or provider
    /// error.
    pub fn complete(state: &ResponseState) -> Option<LlmResponse> {
        let finish_reason = state.finish_reason?;
        Some(LlmResponse {
            message: state.message.clone(),
            events: state.events.clone(),
            usage: state.usage.clone(),
            finish_reason,
        })
    }

    /// Convenience reducer for callers that already have a collected event
    /// list.
    pub fn from_events(events: &[LlmEvent]) -> Option<LlmResponse> {
        let state = events.iter().cloned().fold(Self::empty(), Self::reduce);
        Self::complete(&state)
    }

    /// Concatenated assistant text assembled from streamed `text-delta`
    /// events.
    pub fn text(&self) -> String {
        response_text(&self.events)
    }

    /// Concatenated reasoning text assembled from streamed `reasoning-delta`
    /// events.
    pub fn reasoning(&self) -> String {
        response_reasoning(&self.events)
    }

    /// Completed tool calls emitted by the provider.
    pub fn tool_calls(&self) -> Vec<&LlmEvent> {
        response_tool_calls(&self.events)
    }

    /// Response usage, falling back to the latest usage-bearing event.
    pub fn usage(&self) -> Option<Usage> {
        self.usage.clone().or_else(|| response_usage(&self.events))
    }
}

/// Concatenate assistant text from a collected event list.
pub fn response_text(events: &[LlmEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Concatenate reasoning text from a collected event list.
pub fn response_reasoning(events: &[LlmEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::ReasoningDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Latest usage across the given events (TS internal `responseUsage`).
fn latest_usage(events: &[LlmEvent]) -> Option<Usage> {
    events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::StepFinish {
                usage: Some(usage), ..
            }
            | LlmEvent::Finish {
                usage: Some(usage), ..
            } => Some(usage),
            _ => None,
        })
        .next_back()
        .cloned()
}

/// Return response usage, falling back to the latest usage-bearing event
/// (TS `LLMResponse.usage`, accepting a completed response or an collected
/// event list).
pub fn response_usage(events: &[LlmEvent]) -> Option<Usage> {
    latest_usage(events)
}

/// Return completed tool calls from a collected event list.
pub fn response_tool_calls(events: &[LlmEvent]) -> Vec<&LlmEvent> {
    events
        .iter()
        .filter(|event| matches!(event, LlmEvent::ToolCall { .. }))
        .collect()
}

fn append_event(state: &mut ResponseState, event: &LlmEvent) {
    match event {
        LlmEvent::Finish { reason, usage, .. } => {
            if let Some(usage) = usage {
                state.usage = Some(usage.clone());
            }
            state.finish_reason = Some(*reason);
        }
        LlmEvent::ProviderError { .. } if state.finish_reason.is_none() => {
            state.finish_reason = Some(FinishReason::Error);
        }
        LlmEvent::StepFinish {
            usage: Some(usage), ..
        } => {
            state.usage = Some(usage.clone());
        }
        _ => {}
    }
    state.events.push(event.clone());
}

fn text_content(text: String, provider_metadata: Option<ProviderMetadata>) -> ContentPart {
    ContentPart::Text {
        text,
        cache: None,
        metadata: None,
        provider_metadata,
    }
}

fn reasoning_content(text: String, provider_metadata: Option<ProviderMetadata>) -> ContentPart {
    ContentPart::Reasoning {
        text,
        encrypted: None,
        metadata: None,
        provider_metadata,
    }
}

fn replace_content(state: &mut ResponseState, index: usize, part: ContentPart) {
    if let Some(existing) = state.message.content.get_mut(index) {
        *existing = part;
    }
}

fn ensure_text(state: &mut ResponseState, id: &str, provider_metadata: Option<ProviderMetadata>) {
    if state.text_parts.contains_key(id) {
        return;
    }
    let content_index = state.message.content.len();
    state
        .message
        .content
        .push(text_content(String::new(), provider_metadata.clone()));
    state.text_parts.insert(
        id.to_string(),
        ContentAssembly {
            content_index,
            text: String::new(),
            provider_metadata,
        },
    );
}

fn reduce_text_delta(
    state: &mut ResponseState,
    id: &str,
    text: &str,
    event_metadata: Option<ProviderMetadata>,
) {
    ensure_text(state, id, event_metadata.clone());
    let Some(mut current) = state.text_parts.get(id).cloned() else {
        return;
    };
    current.text.push_str(text);
    if let Some(metadata) = event_metadata {
        current.provider_metadata = Some(metadata);
    }
    replace_content(
        state,
        current.content_index,
        text_content(current.text.clone(), current.provider_metadata.clone()),
    );
    state.text_parts.insert(id.to_string(), current);
}

fn reduce_text_end(state: &mut ResponseState, id: &str, event_metadata: Option<ProviderMetadata>) {
    let Some(mut current) = state.text_parts.get(id).cloned() else {
        return;
    };
    if let Some(metadata) = event_metadata {
        current.provider_metadata = Some(metadata);
    }
    replace_content(
        state,
        current.content_index,
        text_content(current.text.clone(), current.provider_metadata.clone()),
    );
    state.text_parts.insert(id.to_string(), current);
}

fn ensure_reasoning(
    state: &mut ResponseState,
    id: &str,
    provider_metadata: Option<ProviderMetadata>,
) {
    if state.reasoning_parts.contains_key(id) {
        return;
    }
    let content_index = state.message.content.len();
    state
        .message
        .content
        .push(reasoning_content(String::new(), provider_metadata.clone()));
    state.reasoning_parts.insert(
        id.to_string(),
        ContentAssembly {
            content_index,
            text: String::new(),
            provider_metadata,
        },
    );
}

fn reduce_reasoning_delta(
    state: &mut ResponseState,
    id: &str,
    text: &str,
    event_metadata: Option<ProviderMetadata>,
) {
    ensure_reasoning(state, id, event_metadata.clone());
    let Some(mut current) = state.reasoning_parts.get(id).cloned() else {
        return;
    };
    current.text.push_str(text);
    if let Some(metadata) = event_metadata {
        current.provider_metadata = Some(metadata);
    }
    replace_content(
        state,
        current.content_index,
        reasoning_content(current.text.clone(), current.provider_metadata.clone()),
    );
    state.reasoning_parts.insert(id.to_string(), current);
}

fn reduce_reasoning_end(
    state: &mut ResponseState,
    id: &str,
    event_metadata: Option<ProviderMetadata>,
) {
    let Some(mut current) = state.reasoning_parts.get(id).cloned() else {
        return;
    };
    if let Some(metadata) = event_metadata {
        current.provider_metadata = Some(metadata);
    }
    replace_content(
        state,
        current.content_index,
        reasoning_content(current.text.clone(), current.provider_metadata.clone()),
    );
    state.reasoning_parts.insert(id.to_string(), current);
}

fn reduce_tool_input_start(
    state: &mut ResponseState,
    id: &str,
    name: &str,
    provider_metadata: Option<ProviderMetadata>,
) {
    state.tool_inputs.insert(
        id.to_string(),
        ToolInputAssembly {
            name: name.to_string(),
            text: String::new(),
            provider_metadata,
        },
    );
}

fn reduce_tool_input_delta(state: &mut ResponseState, id: &str, name: &str, text: &str) {
    let current = state
        .tool_inputs
        .entry(id.to_string())
        .or_insert(ToolInputAssembly {
            name: name.to_string(),
            text: String::new(),
            provider_metadata: None,
        });
    current.text.push_str(text);
}

fn reduce_tool_input_end(
    state: &mut ResponseState,
    id: &str,
    name: &str,
    provider_metadata: Option<ProviderMetadata>,
) {
    let current = state
        .tool_inputs
        .entry(id.to_string())
        .or_insert(ToolInputAssembly {
            name: name.to_string(),
            text: String::new(),
            provider_metadata: None,
        });
    current.name = name.to_string();
    if let Some(metadata) = provider_metadata {
        current.provider_metadata = Some(metadata);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::schema::ids::FinishReason;

    fn fold(events: &[LlmEvent]) -> ResponseState {
        events
            .iter()
            .cloned()
            .fold(LlmResponse::empty(), LlmResponse::reduce)
    }

    fn usage_with(fields: Vec<(&str, f64)>) -> Usage {
        let mut usage = Usage::default();
        for (field, value) in fields {
            match field {
                "input" => usage.input_tokens = Some(value),
                "output" => usage.output_tokens = Some(value),
                "nonCached" => usage.non_cached_input_tokens = Some(value),
                "cacheRead" => usage.cache_read_input_tokens = Some(value),
                "cacheWrite" => usage.cache_write_input_tokens = Some(value),
                "reasoning" => usage.reasoning_tokens = Some(value),
                "total" => usage.total_tokens = Some(value),
                _ => panic!("unknown field: {field}"),
            }
        }
        usage
    }

    #[test]
    fn usage_serializes_only_present_camel_case_fields() {
        let usage = Usage {
            input_tokens: Some(18.0),
            output_tokens: Some(5.0),
            non_cached_input_tokens: Some(18.0),
            cache_read_input_tokens: Some(0.0),
            cache_write_input_tokens: Some(0.0),
            total_tokens: Some(23.0),
            reasoning_tokens: None,
            provider_metadata: None,
        };
        let wire = serde_json::to_value(&usage).unwrap();
        assert_eq!(
            wire,
            json!({
                "inputTokens": 18.0,
                "outputTokens": 5.0,
                "nonCachedInputTokens": 18.0,
                "cacheReadInputTokens": 0.0,
                "cacheWriteInputTokens": 0.0,
                "totalTokens": 23.0,
            }),
        );
        let back: Usage = serde_json::from_value(wire).unwrap();
        assert_eq!(back, usage);
    }

    #[test]
    fn usage_visible_output_tokens_clamps_at_zero() {
        assert_eq!(
            usage_with(vec![("output", 5.0), ("reasoning", 4.0)]).visible_output_tokens(),
            1.0,
        );
        assert_eq!(
            usage_with(vec![("output", 2.0), ("reasoning", 4.0)]).visible_output_tokens(),
            0.0,
        );
        assert_eq!(Usage::default().visible_output_tokens(), 0.0);
    }

    #[test]
    fn event_wire_tags_are_kebab_case_with_camel_case_fields() {
        let event = LlmEvent::TextDelta {
            id: "text-0".to_string(),
            text: "Hi".to_string(),
            provider_metadata: None,
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(
            wire,
            json!({"type": "text-delta", "id": "text-0", "text": "Hi"}),
        );
        let back: LlmEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(back, event);

        let event = LlmEvent::ToolCall {
            id: "call_1".to_string(),
            name: "lookup".to_string(),
            input: json!({"query": "weather"}),
            provider_executed: Some(false),
            provider_metadata: None,
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(
            wire,
            json!({
                "type": "tool-call",
                "id": "call_1",
                "name": "lookup",
                "input": {"query": "weather"},
                "providerExecuted": false,
            }),
        );
        let back: LlmEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(back, event);

        let event = LlmEvent::StepFinish {
            index: 0.0,
            reason: FinishReason::ToolCalls,
            usage: Some(usage_with(vec![("input", 677.0)])),
            provider_metadata: None,
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(
            wire,
            json!({
                "type": "step-finish",
                "index": 0.0,
                "reason": "tool-calls",
                "usage": {"inputTokens": 677.0},
            }),
        );
        let back: LlmEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(back, event);

        let event = LlmEvent::ProviderError {
            message: "boom".to_string(),
            classification: Some(ProviderFailureClassification::ContextOverflow),
            retryable: Some(true),
            provider_metadata: None,
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(
            wire,
            json!({
                "type": "provider-error",
                "message": "boom",
                "classification": "context-overflow",
                "retryable": true,
            }),
        );
        let back: LlmEvent = serde_json::from_value(wire).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn reducer_folds_a_text_stream_into_a_message() {
        let events = vec![
            LlmEvent::StepStart { index: 0.0 },
            LlmEvent::TextStart {
                id: "text-0".to_string(),
                provider_metadata: None,
            },
            LlmEvent::TextDelta {
                id: "text-0".to_string(),
                text: "Hel".to_string(),
                provider_metadata: None,
            },
            LlmEvent::TextDelta {
                id: "text-0".to_string(),
                text: "lo!".to_string(),
                provider_metadata: None,
            },
            LlmEvent::TextEnd {
                id: "text-0".to_string(),
                provider_metadata: None,
            },
            LlmEvent::StepFinish {
                index: 0.0,
                reason: FinishReason::Stop,
                usage: Some(usage_with(vec![("input", 18.0), ("output", 5.0)])),
                provider_metadata: None,
            },
            LlmEvent::Finish {
                reason: FinishReason::Stop,
                usage: Some(usage_with(vec![("input", 18.0), ("output", 5.0)])),
                provider_metadata: None,
            },
        ];
        let response = LlmResponse::from_events(&events).unwrap();
        assert_eq!(response.message.content, vec![Message::text("Hello!")],);
        assert_eq!(response.finish_reason, FinishReason::Stop);
        assert_eq!(response.events, events);
        assert_eq!(response.text(), "Hello!");
        assert_eq!(response.reasoning(), "");
        assert!(response.tool_calls().is_empty());
    }

    #[test]
    fn complete_is_none_until_a_terminal_finish_or_provider_error() {
        let events = vec![
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
            LlmEvent::TextEnd {
                id: "text-0".to_string(),
                provider_metadata: None,
            },
        ];
        let state = fold(&events);
        assert!(LlmResponse::complete(&state).is_none());
        assert_eq!(state.message.content, vec![Message::text("partial")],);

        let terminal = vec![LlmEvent::ProviderError {
            message: "boom".to_string(),
            classification: None,
            retryable: None,
            provider_metadata: None,
        }];
        let response = LlmResponse::from_events(&terminal).unwrap();
        assert_eq!(response.finish_reason, FinishReason::Error);
        assert_eq!(response.events, terminal);
    }

    #[test]
    fn usage_falls_back_to_the_latest_usage_bearing_event() {
        let step_finish = LlmEvent::StepFinish {
            index: 0.0,
            reason: FinishReason::Stop,
            usage: Some(usage_with(vec![("input", 3.0)])),
            provider_metadata: None,
        };
        let finish_without_usage = LlmEvent::Finish {
            reason: FinishReason::Stop,
            usage: None,
            provider_metadata: None,
        };

        let response =
            LlmResponse::from_events(&[step_finish.clone(), finish_without_usage.clone()]).unwrap();
        assert_eq!(response.usage, Some(usage_with(vec![("input", 3.0)])));
        assert_eq!(response.usage().map(|u| u.input_tokens), Some(Some(3.0)));

        let finish_with_usage = LlmEvent::Finish {
            reason: FinishReason::Stop,
            usage: Some(usage_with(vec![("output", 2.0)])),
            provider_metadata: None,
        };
        let response = LlmResponse::from_events(&[step_finish, finish_with_usage]).unwrap();
        assert_eq!(response.usage, Some(usage_with(vec![("output", 2.0)])));

        // Raw event-list fallback (TS `LLMResponse.usage({ events })`).
        let events = vec![
            LlmEvent::StepFinish {
                index: 0.0,
                reason: FinishReason::Stop,
                usage: Some(usage_with(vec![("input", 3.0)])),
                provider_metadata: None,
            },
            finish_without_usage,
        ];
        assert_eq!(
            response_usage(&events),
            Some(usage_with(vec![("input", 3.0)]))
        );
    }

    #[test]
    fn assembles_interleaved_reasoning_and_text_with_end_metadata() {
        let signature: ProviderMetadata = [(
            "anthropic".to_string(),
            serde_json::from_value(json!({"signature": "sig"})).unwrap(),
        )]
        .into_iter()
        .collect();
        let events = vec![
            LlmEvent::ReasoningStart {
                id: "r1".to_string(),
                provider_metadata: None,
            },
            LlmEvent::ReasoningDelta {
                id: "r1".to_string(),
                text: "I should ".to_string(),
                provider_metadata: None,
            },
            LlmEvent::TextStart {
                id: "t1".to_string(),
                provider_metadata: None,
            },
            LlmEvent::ReasoningDelta {
                id: "r1".to_string(),
                text: "compare...".to_string(),
                provider_metadata: None,
            },
            LlmEvent::ReasoningEnd {
                id: "r1".to_string(),
                provider_metadata: Some(signature.clone()),
            },
            LlmEvent::TextDelta {
                id: "t1".to_string(),
                text: "Answer".to_string(),
                provider_metadata: None,
            },
            LlmEvent::TextEnd {
                id: "t1".to_string(),
                provider_metadata: None,
            },
            LlmEvent::Finish {
                reason: FinishReason::Stop,
                usage: Some(usage_with(vec![("output", 5.0)])),
                provider_metadata: None,
            },
        ];
        let response = LlmResponse::from_events(&events).unwrap();

        assert_eq!(response.finish_reason, FinishReason::Stop);
        assert_eq!(response.usage, Some(usage_with(vec![("output", 5.0)])));
        assert_eq!(response.events, events);
        assert_eq!(
            response.message.content,
            vec![
                ContentPart::Reasoning {
                    text: "I should compare...".to_string(),
                    encrypted: None,
                    metadata: None,
                    provider_metadata: Some(signature),
                },
                Message::text("Answer"),
            ],
        );
        assert_eq!(response.text(), "Answer");
        assert_eq!(response.reasoning(), "I should compare...");
    }

    #[test]
    fn assembles_tool_call_content_only_after_the_completed_tool_call_event() {
        let events = vec![
            LlmEvent::ToolInputStart {
                id: "call_1".to_string(),
                name: "lookup".to_string(),
                provider_metadata: None,
            },
            LlmEvent::ToolInputDelta {
                id: "call_1".to_string(),
                name: "lookup".to_string(),
                text: "{\"query\"".to_string(),
            },
        ];
        let state = fold(&events);
        assert!(state.message.content.is_empty());
        assert_eq!(state.tool_inputs["call_1"].text, "{\"query\"");

        let mut all_events = events;
        all_events.extend([
            LlmEvent::ToolInputDelta {
                id: "call_1".to_string(),
                name: "lookup".to_string(),
                text: ":\"weather\"}".to_string(),
            },
            LlmEvent::ToolInputEnd {
                id: "call_1".to_string(),
                name: "lookup".to_string(),
                provider_metadata: None,
            },
            LlmEvent::ToolCall {
                id: "call_1".to_string(),
                name: "lookup".to_string(),
                input: json!({"query": "weather"}),
                provider_executed: None,
                provider_metadata: None,
            },
            LlmEvent::Finish {
                reason: FinishReason::ToolCalls,
                usage: None,
                provider_metadata: None,
            },
        ]);
        let response = LlmResponse::from_events(&all_events).unwrap();

        assert_eq!(
            response.message.content,
            vec![ContentPart::ToolCall {
                id: "call_1".to_string(),
                name: "lookup".to_string(),
                input: json!({"query": "weather"}),
                provider_executed: None,
                metadata: None,
                provider_metadata: None,
            }],
        );
        assert_eq!(response.tool_calls().len(), 1);
        assert!(state.tool_inputs["call_1"].text == "{\"query\"");
    }

    #[test]
    fn assembles_tool_result_content() {
        let events = vec![
            LlmEvent::ToolResult {
                id: "call_1".to_string(),
                name: "lookup".to_string(),
                result: ToolResultValue::Json {
                    value: json!({"ok": true}),
                },
                output: None,
                provider_executed: Some(true),
                provider_metadata: None,
            },
            LlmEvent::Finish {
                reason: FinishReason::Stop,
                usage: None,
                provider_metadata: None,
            },
        ];
        let response = LlmResponse::from_events(&events).unwrap();
        assert_eq!(
            response.message.content,
            vec![ContentPart::ToolResult {
                id: "call_1".to_string(),
                name: "lookup".to_string(),
                result: ToolResultValue::Json {
                    value: json!({"ok": true}),
                },
                provider_executed: Some(true),
                cache: None,
                metadata: None,
                provider_metadata: None,
            }],
        );
    }

    #[test]
    fn tool_error_is_not_a_terminal_event() {
        let events = vec![LlmEvent::ToolError {
            id: "call_1".to_string(),
            name: "lookup".to_string(),
            message: "boom".to_string(),
            error: Some(json!({"code": 1})),
            provider_metadata: None,
        }];
        let state = fold(&events);
        assert!(LlmResponse::complete(&state).is_none());
        assert!(state.message.content.is_empty());
    }
}
