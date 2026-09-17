//! Typed readers for `providerOptions.openai`
//! (from `protocols/utils/openai-options.ts`).

use crate::schema::ids::{JsonMap, REASONING_EFFORTS, TEXT_VERBOSITIES};
use crate::schema::messages::LlmRequest;

/// `OpenAIReasoningEfforts` — `ReasoningEfforts` without `"max"`
/// (chat completions reject it; responses accept it via `reasoning_effort`).
pub const OPENAI_REASONING_EFFORTS: [&str; 6] =
    ["none", "minimal", "low", "medium", "high", "xhigh"];

/// Mirrors OpenAI's `ResponseIncludable` union from the official SDK. Keep this
/// in lockstep with `openai-node/src/resources/responses/responses.ts`.
pub const OPENAI_RESPONSE_INCLUDABLES: [&str; 8] = [
    "file_search_call.results",
    "web_search_call.results",
    "web_search_call.action.sources",
    "message.input_image.image_url",
    "computer_call_output.output.image_url",
    "code_interpreter_call.outputs",
    "reasoning.encrypted_content",
    "message.output_text.logprobs",
];

pub const OPENAI_SERVICE_TIERS: [&str; 4] = ["auto", "default", "flex", "priority"];

/// TS `isReasoningEffort` — the chat-completions subset (no `"max"`).
pub fn is_reasoning_effort(effort: &str) -> bool {
    OPENAI_REASONING_EFFORTS.contains(&effort)
}

fn options(request: &LlmRequest) -> Option<&JsonMap> {
    request.provider_options.as_ref()?.get("openai")
}

pub fn store(request: &LlmRequest) -> Option<bool> {
    options(request)?
        .get("store")
        .and_then(serde_json::Value::as_bool)
}

/// Accepts any reasoning effort; the chat protocol validates the subset
/// without `"max"` separately.
pub fn reasoning_effort(request: &LlmRequest) -> Option<&str> {
    let value = options(request)?.get("reasoningEffort")?.as_str()?;
    REASONING_EFFORTS.contains(&value).then_some(value)
}

pub fn reasoning_summary(request: &LlmRequest) -> Option<&'static str> {
    match options(request)?.get("reasoningSummary")?.as_str()? {
        "auto" => Some("auto"),
        _ => None,
    }
}

/// Resolve the OpenAI Responses `include` field. Filters out unknown
/// includable values defensively so a typo in upstream config drops the
/// invalid entry instead of poisoning the wire body. An empty array (either
/// passed directly or produced by filtering) is treated as "no include" and
/// returns `None` so the request body omits the field entirely.
pub fn include(request: &LlmRequest) -> Option<Vec<&str>> {
    let value = options(request)?.get("include")?.as_array()?;
    let filtered: Vec<&str> = value
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter(|entry| OPENAI_RESPONSE_INCLUDABLES.contains(entry))
        .collect();
    (!filtered.is_empty()).then_some(filtered)
}

pub fn prompt_cache_key(request: &LlmRequest) -> Option<&str> {
    options(request)?.get("promptCacheKey")?.as_str()
}

pub fn text_verbosity(request: &LlmRequest) -> Option<&str> {
    let value = options(request)?.get("textVerbosity")?.as_str()?;
    TEXT_VERBOSITIES.contains(&value).then_some(value)
}

pub fn service_tier(request: &LlmRequest) -> Option<&str> {
    let value = options(request)?.get("serviceTier")?.as_str()?;
    OPENAI_SERVICE_TIERS.contains(&value).then_some(value)
}

pub fn instructions(request: &LlmRequest) -> Option<&str> {
    options(request)?.get("instructions")?.as_str()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use crate::route::client::RouteHandle;
    use crate::schema::messages::{LlmRequest, ModelRef};

    use super::*;

    fn make_request(provider_options: serde_json::Value) -> LlmRequest {
        let mut request = LlmRequest::new(ModelRef::new(
            "gpt-5.2",
            "openai",
            #[allow(clippy::arc_with_non_send_sync)]
            Arc::new(RouteHandle::empty()),
        ));
        request.provider_options = Some(serde_json::from_value(provider_options).unwrap());
        request
    }

    #[test]
    fn openai_reasoning_efforts_exclude_max() {
        let expected: Vec<&str> = REASONING_EFFORTS
            .iter()
            .copied()
            .filter(|effort| *effort != "max")
            .collect();
        assert_eq!(OPENAI_REASONING_EFFORTS.len(), expected.len());
        for effort in expected {
            assert!(OPENAI_REASONING_EFFORTS.contains(&effort));
        }
    }

    #[test]
    fn readers_pull_typed_values_from_provider_options() {
        let request = make_request(json!({
            "openai": {
                "store": true,
                "reasoningEffort": "high",
                "reasoningSummary": "auto",
                "include": [
                    "reasoning.encrypted_content",
                    "not-an-includable",
                ],
                "promptCacheKey": "cache-key",
                "textVerbosity": "medium",
                "serviceTier": "flex",
                "instructions": "Be terse.",
            },
        }));
        assert_eq!(store(&request), Some(true));
        assert_eq!(reasoning_effort(&request), Some("high"));
        assert_eq!(reasoning_summary(&request), Some("auto"));
        assert_eq!(include(&request), Some(vec!["reasoning.encrypted_content"]),);
        assert_eq!(prompt_cache_key(&request), Some("cache-key"));
        assert_eq!(text_verbosity(&request), Some("medium"));
        assert_eq!(service_tier(&request), Some("flex"));
        assert_eq!(instructions(&request), Some("Be terse."));
    }

    #[test]
    fn reasoning_effort_accepts_max() {
        let request = make_request(json!({"openai": {"reasoningEffort": "max"}}));
        assert_eq!(reasoning_effort(&request), Some("max"));
        assert!(!is_reasoning_effort("max"));
        assert!(is_reasoning_effort("high"));
    }

    #[test]
    fn include_returns_none_when_everything_filters_out() {
        let request = make_request(json!({"openai": {"include": ["bogus"]}}));
        assert_eq!(include(&request), None);
        let request = make_request(json!({"openai": {"include": []}}));
        assert_eq!(include(&request), None);
        let request = make_request(json!({"openai": {}}));
        assert_eq!(include(&request), None);
        let request = make_request(json!({}));
        assert_eq!(include(&request), None);
    }

    #[test]
    fn readers_reject_wrong_types_and_unknown_values() {
        let request = make_request(json!({
            "openai": {
                "store": "yes",
                "reasoningEffort": "ultra",
                "reasoningSummary": "detailed",
                "textVerbosity": "verbose",
                "serviceTier": "edge",
                "promptCacheKey": 42,
                "instructions": null,
            },
        }));
        assert_eq!(store(&request), None);
        assert_eq!(reasoning_effort(&request), None);
        assert_eq!(reasoning_summary(&request), None);
        assert_eq!(text_verbosity(&request), None);
        assert_eq!(service_tier(&request), None);
        assert_eq!(prompt_cache_key(&request), None);
        assert_eq!(instructions(&request), None);
    }
}
