//! Usage math + `usage_update` emission (`acp/usage.ts`).

use serde_json::{json, Value};

use crate::acp::server::ServerClient;

/// `contextTokens` (usage.ts:86-88): input + cache.read + cache.write.
pub fn context_tokens(message: &Value) -> f64 {
    let tokens = message.get("tokens");
    let field = |path: &str| {
        tokens
            .and_then(|tokens| tokens.pointer(path))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    };
    field("/input") + field("/cache/read") + field("/cache/write")
}

/// `buildUsage` (usage.ts:90-103).
pub fn build_usage(message: &Value) -> Value {
    let tokens = message.get("tokens");
    let field = |path: &str| {
        tokens
            .and_then(|tokens| tokens.pointer(path))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    };
    let input_tokens = field("/input");
    let output_tokens = field("/output");
    let thought_tokens = field("/reasoning");
    let cached_read_tokens = field("/cache/read");
    let cached_write_tokens = field("/cache/write");

    let mut usage = json!({
        "inputTokens": input_tokens,
        "outputTokens": output_tokens,
        "totalTokens": input_tokens
            + output_tokens
            + thought_tokens
            + cached_read_tokens
            + cached_write_tokens,
    });
    if thought_tokens > 0.0 {
        usage["thoughtTokens"] = json!(thought_tokens);
    }
    if cached_read_tokens > 0.0 {
        usage["cachedReadTokens"] = json!(cached_read_tokens);
    }
    if cached_write_tokens > 0.0 {
        usage["cachedWriteTokens"] = json!(cached_write_tokens);
    }
    usage
}

/// `latestAssistantMessage` (usage.ts:105-109).
pub fn latest_assistant_message(messages: &[Value]) -> Option<Value> {
    messages
        .iter()
        .rev()
        .find(|message| message["info"]["role"] == json!("assistant"))
        .and_then(|message| message.get("info"))
        .cloned()
}

/// `totalSessionCost` (usage.ts:111-115).
pub fn total_session_cost(messages: &[Value]) -> f64 {
    messages
        .iter()
        .filter(|message| message["info"]["role"] == json!("assistant"))
        .map(|message| message["info"]["cost"].as_f64().unwrap_or_default())
        .sum()
}

/// `findContextLimit` (usage.ts:117-123) — accepts both the raw list
/// (`{providers: [...]}`) and the id-keyed map.
pub fn find_context_limit(providers: &Value, provider_id: &str, model_id: &str) -> Option<f64> {
    let provider = providers.get(provider_id).or_else(|| {
        match providers.get("providers").and_then(Value::as_array) {
            Some(list) => list
                .iter()
                .find(|provider| provider.get("id").and_then(Value::as_str) == Some(provider_id)),
            None => None,
        }
    })?;
    provider
        .get("models")?
        .get(model_id)?
        .get("limit")?
        .get("context")?
        .as_f64()
}

/// `sendUpdate` (usage.ts:183-221): best-effort — every failure is
/// logged and swallowed.
/// The TS memoizes context-limit lookups per (directory, providerID,
/// modelID) via `Effect.cached` inside a `SynchronizedRef` (usage.ts:148-169).
static CONTEXT_LIMIT_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, Option<f64>>>,
> = std::sync::OnceLock::new();

fn context_limit_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, Option<f64>>>
{
    CONTEXT_LIMIT_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub async fn send_update(
    connection: &crate::acp::jsonrpc::Connection,
    server: &ServerClient,
    directory: &str,
    session_id: &str,
) {
    let messages = match server.session_messages(directory, session_id, None).await {
        Ok(messages) => messages,
        Err(error) => {
            eprintln!("failed to fetch messages for usage update: {error}");
            return;
        }
    };
    let Some(message) = latest_assistant_message(&messages) else {
        return;
    };
    let (Some(provider_id), Some(model_id)) = (
        message.get("providerID").and_then(Value::as_str),
        message.get("modelID").and_then(Value::as_str),
    ) else {
        return;
    };
    let cache_key = format!("{directory}\x00{provider_id}\x00{model_id}");
    let size = {
        let cached = context_limit_cache()
            .lock()
            .unwrap()
            .get(&cache_key)
            .cloned();
        match cached {
            Some(size) => size,
            None => {
                let size = match server.config_providers(directory).await {
                    Ok(providers) => find_context_limit(&providers, provider_id, model_id),
                    Err(_) => None,
                };
                context_limit_cache()
                    .lock()
                    .unwrap()
                    .insert(cache_key, size);
                size
            }
        }
    };
    let Some(size) = size else {
        return;
    };

    let _ = connection
        .send_notification(
            "session/update",
            json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "usage_update",
                    "used": context_tokens(&message),
                    "size": size,
                    "cost": { "amount": total_session_cost(&messages), "currency": "USD" },
                },
            }),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn usage_carries_all_token_kinds() {
        let usage = build_usage(&json!({
            "tokens": {
                "input": 10.0,
                "output": 5.0,
                "reasoning": 2.0,
                "cache": { "read": 3.0, "write": 4.0 }
            }
        }));
        assert_eq!(usage["inputTokens"], json!(10.0));
        assert_eq!(usage["outputTokens"], json!(5.0));
        assert_eq!(usage["totalTokens"], json!(24.0));
        assert_eq!(usage["thoughtTokens"], json!(2.0));
        assert_eq!(usage["cachedReadTokens"], json!(3.0));
        assert_eq!(usage["cachedWriteTokens"], json!(4.0));
    }

    #[test]
    fn usage_omits_zero_optionals() {
        let usage = build_usage(&json!({
            "tokens": { "input": 1.0, "output": 1.0 }
        }));
        assert!(usage.get("thoughtTokens").is_none());
        assert!(usage.get("cachedReadTokens").is_none());
    }

    #[test]
    fn latest_assistant_picks_last() {
        let messages = vec![
            json!({ "info": { "role": "user" } }),
            json!({ "info": { "role": "assistant", "cost": 1.0 } }),
            json!({ "info": { "role": "assistant", "cost": 2.0 } }),
        ];
        assert_eq!(
            latest_assistant_message(&messages).unwrap()["cost"],
            json!(2.0)
        );
        assert_eq!(total_session_cost(&messages), 3.0);
    }

    #[test]
    fn context_limit_walks_the_provider_map() {
        let providers = json!({
            "mock": { "models": { "m": { "limit": { "context": 4096.0 } } } }
        });
        assert_eq!(find_context_limit(&providers, "mock", "m"), Some(4096.0));
        assert_eq!(find_context_limit(&providers, "mock", "gone"), None);
    }
}
