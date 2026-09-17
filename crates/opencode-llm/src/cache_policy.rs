//! Apply an `LLMRequest.cache` policy by injecting `CacheHint`s onto the parts
//! the policy designates. Runs once at compile time, before the per-protocol
//! body builder, so the existing inline-hint lowering path handles the rest.
//!
//! The default `"auto"` shape places one breakpoint at the last tool
//! definition, one at the last system part, and one at the latest user
//! message. This matches what production agent harnesses (LangChain's
//! caching middleware, kern-ai's 10x cost-reduction playbook) converge on
//! for tool-use loops: the latest user message stays put while a single turn
//! explodes into many assistant/tool round-trips, so caching at that boundary
//! lets every intra-turn API call hit the prefix.
//!
//! Manual `cache: CacheHint` placements on individual parts are preserved —
//! this module only fills gap the caller left empty.

#![allow(clippy::arc_with_non_send_sync)]
use crate::schema::ids::MessageRole;
use crate::schema::messages::{ContentPart, LlmRequest, LlmRequestPatch, Message, ToolDefinition};
use crate::schema::options::{
    CacheHint, CacheHintType, CachePolicy, CachePolicyLiteral, CachePolicyMessages,
    CachePolicyObject, SystemPart,
};

const AUTO: CachePolicyObject = CachePolicyObject {
    tools: Some(true),
    system: Some(true),
    messages: Some(CachePolicyMessages::LatestUserMessage),
    ttl_seconds: None,
};

const NONE: CachePolicyObject = CachePolicyObject {
    tools: None,
    system: None,
    messages: None,
    ttl_seconds: None,
};

// Resolution rules:
//   - undefined   → "auto" — caching is on by default. The math favors it:
//                   Anthropic 5m-cache write is 1.25x base, read is 0.1x,
//                   so a single reuse within 5 minutes already wins.
//   - "auto"      → tools + system + latest user msg.
//   - "none"      → no auto placement; manual `CacheHint`s still flow.
//   - object form → exactly what the caller asked for.
fn resolve(policy: Option<&CachePolicy>) -> CachePolicyObject {
    match policy {
        None | Some(CachePolicy::Literal(CachePolicyLiteral::Auto)) => AUTO,
        Some(CachePolicy::Literal(CachePolicyLiteral::None)) => NONE,
        Some(CachePolicy::Object(object)) => object.clone(),
    }
}

// Protocols whose wire format ignores inline cache markers (OpenAI's implicit
// prefix caching, Gemini's implicit + out-of-band CachedContent). Skip the
// whole policy pass for these — emitting hints would be harmless but pointless.
const RESPECTS_INLINE_HINTS: [&str; 2] = ["anthropic-messages", "bedrock-converse"];

fn make_hint(ttl_seconds: Option<f64>) -> CacheHint {
    CacheHint {
        r#type: CacheHintType::Ephemeral,
        ttl_seconds,
    }
}

fn mark_last_tool(tools: &[ToolDefinition], hint: &CacheHint) -> Vec<ToolDefinition> {
    let mut tools = tools.to_vec();
    let Some(last) = tools.last_mut() else {
        return tools;
    };
    if last.cache.is_none() {
        last.cache = Some(hint.clone());
    }
    tools
}

fn mark_last_system(system: &[SystemPart], hint: &CacheHint) -> Vec<SystemPart> {
    let mut system = system.to_vec();
    let Some(last) = system.last_mut() else {
        return system;
    };
    if last.cache.is_none() {
        last.cache = Some(hint.clone());
    }
    system
}

fn last_index_of_role(messages: &[Message], role: MessageRole) -> Option<usize> {
    messages.iter().rposition(|message| message.role == role)
}

// Mark the last text part of `messages[index]`. If no text part exists, mark
// the last content part regardless of type — that's the breakpoint position
// in tool-result-only messages too.
fn mark_message_at(messages: &[Message], index: Option<usize>, hint: &CacheHint) -> Vec<Message> {
    let Some(index) = index else {
        return messages.to_vec();
    };
    let Some(target) = messages.get(index) else {
        return messages.to_vec();
    };
    if target.content.is_empty() {
        return messages.to_vec();
    }
    let last_text_index = target
        .content
        .iter()
        .rposition(|part| matches!(part, ContentPart::Text { .. }));
    let mark_at = last_text_index.unwrap_or(target.content.len() - 1);
    let Some(marked) = part_with_cache(&target.content[mark_at], hint) else {
        return messages.to_vec();
    };
    let mut next = target.clone();
    next.content[mark_at] = marked;
    let mut messages = messages.to_vec();
    messages[index] = next;
    messages
}

/// Set `cache` on a part that carries one, preserving existing manual hints.
/// Text and tool-result parts carry `cache`; the other variants have no
/// `cache` field, so the hint cannot be represented on them (the TS spread
/// adds the key, but the Effect-Schema encode of those parts strips it).
fn part_with_cache(part: &ContentPart, hint: &CacheHint) -> Option<ContentPart> {
    let mut part = part.clone();
    match &mut part {
        ContentPart::Text { cache, .. } if cache.is_none() => *cache = Some(hint.clone()),
        ContentPart::ToolResult { cache, .. } if cache.is_none() => *cache = Some(hint.clone()),
        _ => return None,
    }
    Some(part)
}

fn mark_messages(
    messages: &[Message],
    strategy: &CachePolicyMessages,
    hint: &CacheHint,
) -> Vec<Message> {
    if messages.is_empty() {
        return messages.to_vec();
    }
    match strategy {
        CachePolicyMessages::LatestUserMessage => mark_message_at(
            messages,
            last_index_of_role(messages, MessageRole::User),
            hint,
        ),
        CachePolicyMessages::LatestAssistant => mark_message_at(
            messages,
            last_index_of_role(messages, MessageRole::Assistant),
            hint,
        ),
        CachePolicyMessages::Tail { tail } => {
            let start = ((messages.len() as f64) - tail).max(0.0) as usize;
            let mut next = messages.to_vec();
            for index in start..messages.len() {
                next = mark_message_at(&next, Some(index), hint);
            }
            next
        }
    }
}

/// Apply the request's cache policy. Placement is scoped to routes whose wire
/// format respects inline cache markers (`anthropic-messages`,
/// `bedrock-converse`); every other route is returned unchanged.
///
/// TS reads the route id off `request.model.route.id`; the Rust `RouteHandle`
/// lands in M2.6, so the id is threaded in explicitly by the compile pipeline.
pub fn apply_cache_policy(route_id: &str, request: &LlmRequest) -> LlmRequest {
    if !RESPECTS_INLINE_HINTS.contains(&route_id) {
        return request.clone();
    }
    let policy = resolve(request.cache.as_ref());
    let marks_tools = policy.tools == Some(true);
    let marks_system = policy.system == Some(true);
    if !marks_tools && !marks_system && policy.messages.is_none() {
        return request.clone();
    }

    let hint = make_hint(policy.ttl_seconds);
    let tools = if marks_tools {
        mark_last_tool(&request.tools, &hint)
    } else {
        request.tools.clone()
    };
    let system = if marks_system {
        mark_last_system(&request.system, &hint)
    } else {
        request.system.clone()
    };
    let messages = match &policy.messages {
        Some(strategy) => mark_messages(&request.messages, strategy, &hint),
        None => request.messages.clone(),
    };

    if tools == request.tools && system == request.system && messages == request.messages {
        return request.clone();
    }
    request.update(LlmRequestPatch {
        tools: Some(tools),
        system: Some(system),
        messages: Some(messages),
        ..LlmRequestPatch::default()
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::route::client::RouteHandle;
    use crate::schema::ids::JsonMap;
    use crate::schema::messages::{ModelRef, ToolResultInput};

    fn model_ref() -> ModelRef {
        ModelRef::new(
            "claude-sonnet-4-5",
            "anthropic",
            Arc::new(RouteHandle::empty()),
        )
    }

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("Call {name}"),
            input_schema: JsonMap::new(),
            output_schema: None,
            cache: None,
            metadata: None,
            native: None,
        }
    }

    fn system_part(text: &str) -> SystemPart {
        SystemPart {
            r#type: crate::schema::options::SystemPartType::Text,
            text: text.to_string(),
            cache: None,
            metadata: None,
        }
    }

    #[test]
    fn auto_marks_last_tool_last_system_part_and_latest_user_message() {
        let request = LlmRequest {
            tools: vec![tool("get_weather"), tool("get_time")],
            system: vec![system_part("first"), system_part("last")],
            messages: vec![
                Message::user("Hi"),
                Message::assistant("Hello!"),
                Message::user("What is the weather?"),
            ],
            cache: Some(CachePolicy::Literal(CachePolicyLiteral::Auto)),
            ..LlmRequest::new(model_ref())
        };

        let applied = apply_cache_policy("anthropic-messages", &request);

        assert_eq!(applied.tools[0].cache, None);
        assert_eq!(applied.tools[1].cache, Some(make_hint(None)));
        assert_eq!(applied.system[0].cache, None);
        assert_eq!(applied.system[1].cache, Some(make_hint(None)));

        let last_user = &applied.messages[2];
        match &last_user.content[0] {
            ContentPart::Text { text, cache, .. } => {
                assert_eq!(text, "What is the weather?");
                assert_eq!(cache.as_ref(), Some(&make_hint(None)));
            }
            other => panic!("expected text part, got {other:?}"),
        }
        // Nothing else was touched.
        assert_eq!(applied.messages[0], request.messages[0]);
        assert_eq!(applied.messages[1], request.messages[1]);
    }

    #[test]
    fn unspecified_policy_defaults_to_auto() {
        let request = LlmRequest {
            tools: vec![tool("get_weather")],
            system: vec![system_part("last")],
            messages: vec![Message::user("Hi")],
            ..LlmRequest::new(model_ref())
        };
        let unspecified = apply_cache_policy("anthropic-messages", &request);
        let explicit = apply_cache_policy(
            "anthropic-messages",
            &request.clone().update(LlmRequestPatch {
                cache: Some(CachePolicy::Literal(CachePolicyLiteral::Auto)),
                ..LlmRequestPatch::default()
            }),
        );
        // The `cache` field itself differs (None vs "auto"); every placement
        // decision is identical.
        assert_eq!(unspecified.tools, explicit.tools);
        assert_eq!(unspecified.system, explicit.system);
        assert_eq!(unspecified.messages, explicit.messages);
    }

    #[test]
    fn skips_routes_that_ignore_inline_hints() {
        let request = LlmRequest {
            tools: vec![tool("get_weather")],
            system: vec![system_part("last")],
            messages: vec![Message::user("Hi")],
            ..LlmRequest::new(model_ref())
        };
        assert_eq!(apply_cache_policy("gemini", &request), request);
        assert_eq!(apply_cache_policy("openai-chat", &request), request);
    }

    #[test]
    fn none_policy_places_no_hints() {
        let request = LlmRequest {
            tools: vec![tool("get_weather")],
            system: vec![system_part("last")],
            messages: vec![Message::user("Hi")],
            cache: Some(CachePolicy::Literal(CachePolicyLiteral::None)),
            ..LlmRequest::new(model_ref())
        };
        assert_eq!(apply_cache_policy("anthropic-messages", &request), request);
    }

    #[test]
    fn manual_hints_are_preserved() {
        let mut manual_tool = tool("get_weather");
        manual_tool.cache = Some(CacheHint {
            r#type: CacheHintType::Persistent,
            ttl_seconds: Some(60.0),
        });
        let request = LlmRequest {
            tools: vec![manual_tool.clone(), tool("get_time")],
            system: vec![system_part("last")],
            messages: vec![Message::user("Hi")],
            ..LlmRequest::new(model_ref())
        };
        let applied = apply_cache_policy("anthropic-messages", &request);
        // The manual tool hint wins over the auto hint.
        assert_eq!(applied.tools[0], manual_tool);
        assert_eq!(applied.tools[1].cache, Some(make_hint(None)));

        // A manual hint on the latest user message's part is preserved.
        let manual_cache = CacheHint {
            r#type: CacheHintType::Ephemeral,
            ttl_seconds: Some(120.0),
        };
        let manual_part = ContentPart::Text {
            text: "Hi".to_string(),
            cache: Some(manual_cache.clone()),
            metadata: None,
            provider_metadata: None,
        };
        let request = LlmRequest {
            messages: vec![Message::user(vec![manual_part])],
            ..LlmRequest::new(model_ref())
        };
        let applied = apply_cache_policy("anthropic-messages", &request);
        match &applied.messages[0].content[0] {
            ContentPart::Text { cache, .. } => {
                assert_eq!(cache.as_ref(), Some(&manual_cache));
            }
            other => panic!("expected text part, got {other:?}"),
        }
    }

    #[test]
    fn ttl_seconds_flow_into_the_hint() {
        let request = LlmRequest {
            system: vec![system_part("last")],
            cache: Some(CachePolicy::Object(CachePolicyObject {
                tools: None,
                system: Some(true),
                messages: None,
                ttl_seconds: Some(3600.0),
            })),
            ..LlmRequest::new(model_ref())
        };
        let applied = apply_cache_policy("bedrock-converse", &request);
        assert_eq!(applied.system[0].cache, Some(make_hint(Some(3600.0))));
    }

    #[test]
    fn latest_user_message_without_text_marks_the_last_part() {
        // No text part exists, so the last content part of any type is the
        // breakpoint position — a tool result in this case.
        let request = LlmRequest {
            messages: vec![Message::user(vec![ContentPart::tool_result(
                ToolResultInput {
                    id: "toolu_1".to_string(),
                    name: "get_weather".to_string(),
                    result: json!({"temp": "18"}),
                    ..ToolResultInput::default()
                },
            )])],
            ..LlmRequest::new(model_ref())
        };
        let applied = apply_cache_policy("anthropic-messages", &request);
        match &applied.messages[0].content[0] {
            ContentPart::ToolResult { cache, .. } => {
                assert_eq!(cache.as_ref(), Some(&make_hint(None)));
            }
            other => panic!("expected tool result part, got {other:?}"),
        }
    }

    #[test]
    fn latest_assistant_strategy_marks_the_last_assistant_message() {
        let request = LlmRequest {
            messages: vec![
                Message::user("Hi"),
                Message::assistant("I can help."),
                Message::user("Thanks"),
            ],
            cache: Some(CachePolicy::Object(CachePolicyObject {
                tools: None,
                system: None,
                messages: Some(CachePolicyMessages::LatestAssistant),
                ttl_seconds: None,
            })),
            ..LlmRequest::new(model_ref())
        };
        let applied = apply_cache_policy("anthropic-messages", &request);
        match &applied.messages[1].content[0] {
            ContentPart::Text { cache, .. } => assert_eq!(cache.as_ref(), Some(&make_hint(None))),
            other => panic!("expected text part, got {other:?}"),
        }
        match &applied.messages[0].content[0] {
            ContentPart::Text { cache, .. } => assert_eq!(cache, &None),
            other => panic!("expected text part, got {other:?}"),
        }
    }

    #[test]
    fn tail_strategy_marks_each_of_the_last_n_messages() {
        let request = LlmRequest {
            messages: vec![
                Message::user("one"),
                Message::assistant("two"),
                Message::user("three"),
            ],
            cache: Some(CachePolicy::Object(CachePolicyObject {
                tools: None,
                system: None,
                messages: Some(CachePolicyMessages::Tail { tail: 2.0 }),
                ttl_seconds: None,
            })),
            ..LlmRequest::new(model_ref())
        };
        let applied = apply_cache_policy("anthropic-messages", &request);
        for (index, message) in applied.messages.iter().enumerate() {
            match &message.content[0] {
                ContentPart::Text { cache, .. } => {
                    let expected = if index >= 1 {
                        Some(make_hint(None))
                    } else {
                        None
                    };
                    assert_eq!(cache, &expected, "message {index}");
                }
                other => panic!("expected text part, got {other:?}"),
            }
        }
    }

    #[test]
    fn tail_strategy_larger_than_the_conversation_marks_everything() {
        let request = LlmRequest {
            messages: vec![Message::user("one"), Message::assistant("two")],
            cache: Some(CachePolicy::Object(CachePolicyObject {
                tools: None,
                system: None,
                messages: Some(CachePolicyMessages::Tail { tail: 10.0 }),
                ttl_seconds: None,
            })),
            ..LlmRequest::new(model_ref())
        };
        let applied = apply_cache_policy("anthropic-messages", &request);
        for message in &applied.messages {
            match &message.content[0] {
                ContentPart::Text { cache, .. } => {
                    assert_eq!(cache.as_ref(), Some(&make_hint(None)))
                }
                other => panic!("expected text part, got {other:?}"),
            }
        }
    }

    #[test]
    fn auto_without_a_user_message_is_a_no_op() {
        let request = LlmRequest {
            messages: vec![Message::assistant("Hi")],
            ..LlmRequest::new(model_ref())
        };
        assert_eq!(apply_cache_policy("anthropic-messages", &request), request);
    }
}
