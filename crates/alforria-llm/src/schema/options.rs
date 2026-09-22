//! Generation options, model metadata, and cache policy from `schema/options.ts`.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::route::client::RouteHandle;
use crate::schema::ids::{JsonMap, ModelId, ProviderId};

/// `LLM.ProviderOptions`: `Record<String, Record<String, Unknown>>`.
pub type ProviderOptions = BTreeMap<String, JsonMap>;

pub fn merge_json_records(items: &[Option<&JsonMap>]) -> Option<JsonMap> {
    let defined: Vec<&JsonMap> = items.iter().filter_map(|item| *item).collect();
    match defined.len() {
        0 => None,
        1 => Some(defined[0].clone()),
        _ => {
            let mut result = JsonMap::new();
            for record in defined {
                for (key, value) in record {
                    let merged = match result.get(key) {
                        Some(existing) if existing.is_object() && value.is_object() => {
                            deep_merge_pair(existing, value)
                        }
                        _ => value.clone(),
                    };
                    result.insert(key.clone(), merged);
                }
            }
            if result.is_empty() {
                None
            } else {
                Some(result)
            }
        }
    }
}

fn deep_merge_pair(a: &serde_json::Value, b: &serde_json::Value) -> serde_json::Value {
    match (a, b) {
        (serde_json::Value::Object(a_record), serde_json::Value::Object(b_record)) => {
            let mut merged = a_record.clone();
            for (key, value) in b_record {
                let merged_value = match merged.get(key) {
                    Some(existing) if existing.is_object() && value.is_object() => {
                        deep_merge_pair(existing, value)
                    }
                    _ => value.clone(),
                };
                merged.insert(key.clone(), merged_value);
            }
            serde_json::Value::Object(merged)
        }
        _ => b.clone(),
    }
}

fn merge_string_records(
    items: &[Option<&BTreeMap<String, String>>],
) -> Option<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for record in items.iter().filter_map(|item| *item) {
        for (key, value) in record {
            result.insert(key.clone(), value.clone());
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

pub fn merge_provider_options(items: &[Option<&ProviderOptions>]) -> Option<ProviderOptions> {
    let mut result: ProviderOptions = BTreeMap::new();
    for record in items.iter().filter_map(|item| *item) {
        for (provider, options) in record {
            let merged = merge_json_records(&[result.get(provider), Some(options)]);
            if let Some(merged) = merged {
                result.insert(provider.clone(), merged);
            }
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<JsonMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<BTreeMap<String, String>>,
}

pub fn merge_http_options(items: &[Option<&HttpOptions>]) -> Option<HttpOptions> {
    let defined: Vec<&HttpOptions> = items.iter().filter_map(|item| *item).collect();
    let body = merge_json_records(
        &defined
            .iter()
            .map(|options| options.body.as_ref())
            .collect::<Vec<_>>(),
    );
    let headers = merge_string_records(
        &defined
            .iter()
            .map(|options| options.headers.as_ref())
            .collect::<Vec<_>>(),
    );
    let query = merge_string_records(
        &defined
            .iter()
            .map(|options| options.query.as_ref())
            .collect::<Vec<_>>(),
    );
    if body.is_none() && headers.is_none() && query.is_none() {
        None
    } else {
        Some(HttpOptions {
            body,
            headers,
            query,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
}

fn latest_generation<V>(
    items: &[&GenerationOptions],
    get: impl Fn(&GenerationOptions) -> Option<V>,
) -> Option<V> {
    items.iter().rev().find_map(|options| get(options))
}

pub fn merge_generation_options(items: &[Option<&GenerationOptions>]) -> Option<GenerationOptions> {
    let defined: Vec<&GenerationOptions> = items.iter().filter_map(|item| *item).collect();
    let merged = GenerationOptions {
        max_tokens: latest_generation(&defined, |options| options.max_tokens),
        temperature: latest_generation(&defined, |options| options.temperature),
        top_p: latest_generation(&defined, |options| options.top_p),
        top_k: latest_generation(&defined, |options| options.top_k),
        frequency_penalty: latest_generation(&defined, |options| options.frequency_penalty),
        presence_penalty: latest_generation(&defined, |options| options.presence_penalty),
        seed: latest_generation(&defined, |options| options.seed),
        stop: latest_generation(&defined, |options| options.stop.clone()),
    };
    if merged.max_tokens.is_none()
        && merged.temperature.is_none()
        && merged.top_p.is_none()
        && merged.top_k.is_none()
        && merged.frequency_penalty.is_none()
        && merged.presence_penalty.is_none()
        && merged.seed.is_none()
        && merged.stop.is_none()
    {
        None
    } else {
        Some(merged)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<ModelLimits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_options: Option<ProviderOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpOptions>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelToolSchemaCompatibility {
    #[serde(rename = "gemini")]
    Gemini,
    #[serde(rename = "moonshot")]
    Moonshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCompatibility {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_schema: Option<ModelToolSchemaCompatibility>,
}

/// Process-local executable model value (TS `Model`). `route` is the deployment
/// concern: protocol + endpoint + auth + transport, erased behind RouteHandle.
#[derive(Clone)]
pub struct Model {
    pub id: ModelId,
    pub provider: ProviderId,
    pub route: Arc<RouteHandle>,
    pub defaults: Option<ModelDefaults>,
    pub compatibility: Option<ModelCompatibility>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheHintType {
    Ephemeral,
    Persistent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheHint {
    pub r#type: CacheHintType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CachePolicyMessages {
    /// `"latest-user-message"`
    LatestUserMessage,
    /// `"latest-assistant"`
    LatestAssistant,
    /// `{ tail: number }`
    Tail { tail: f64 },
}

impl serde::Serialize for CachePolicyMessages {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap as _;

        match self {
            Self::LatestUserMessage => serializer.serialize_str("latest-user-message"),
            Self::LatestAssistant => serializer.serialize_str("latest-assistant"),
            Self::Tail { tail } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("tail", tail)?;
                map.end()
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for CachePolicyMessages {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(literal) => match literal.as_str() {
                "latest-user-message" => Ok(Self::LatestUserMessage),
                "latest-assistant" => Ok(Self::LatestAssistant),
                _ => Err(serde::de::Error::custom(
                    "unknown cache policy messages literal",
                )),
            },
            serde_json::Value::Object(record) => {
                let tail = record
                    .get("tail")
                    .ok_or_else(|| serde::de::Error::missing_field("tail"))?;
                let tail = tail
                    .as_f64()
                    .ok_or_else(|| serde::de::Error::custom("`tail` must be a number"))?;
                Ok(Self::Tail { tail })
            }
            _ => Err(serde::de::Error::custom("invalid cache policy messages")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachePolicyObject {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<CachePolicyMessages>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CachePolicyLiteral {
    Auto,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CachePolicy {
    Literal(CachePolicyLiteral),
    Object(CachePolicyObject),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemPartType {
    #[serde(rename = "text")]
    Text,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemPart {
    pub r#type: SystemPartType,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn cache_policy_parses_literals_and_object() {
        let auto: CachePolicy = serde_json::from_value(json!("auto")).unwrap();
        assert_eq!(auto, CachePolicy::Literal(CachePolicyLiteral::Auto));
        let none: CachePolicy = serde_json::from_value(json!("none")).unwrap();
        assert_eq!(none, CachePolicy::Literal(CachePolicyLiteral::None));
        let object: CachePolicy = serde_json::from_value(json!({
            "messages": "latest-user-message",
        }))
        .unwrap();
        assert_eq!(
            object,
            CachePolicy::Object(CachePolicyObject {
                tools: None,
                system: None,
                messages: Some(CachePolicyMessages::LatestUserMessage),
                ttl_seconds: None,
            }),
        );
        let assistant: CachePolicy = serde_json::from_value(json!({
            "messages": "latest-assistant",
        }))
        .unwrap();
        assert_eq!(
            assistant,
            CachePolicy::Object(CachePolicyObject {
                tools: None,
                system: None,
                messages: Some(CachePolicyMessages::LatestAssistant),
                ttl_seconds: None,
            }),
        );
    }

    #[test]
    fn cache_policy_object_round_trips_tail() {
        let policy = CachePolicy::Object(CachePolicyObject {
            tools: Some(true),
            system: None,
            messages: Some(CachePolicyMessages::Tail { tail: 4.0 }),
            ttl_seconds: Some(3600.0),
        });
        let value = serde_json::to_value(&policy).unwrap();
        assert_eq!(
            value,
            json!({"tools": true, "messages": {"tail": 4.0}, "ttlSeconds": 3600.0}),
        );
        let roundtripped: CachePolicy = serde_json::from_value(value).unwrap();
        assert_eq!(roundtripped, policy);
    }

    #[test]
    fn merge_json_records_deep_merges_maps() {
        let first: JsonMap = serde_json::from_value(json!({
            "shared": {"a": 1, "b": 2},
            "only_first": true,
        }))
        .unwrap();
        let second: JsonMap = serde_json::from_value(json!({
            "shared": {"b": 20, "c": 3},
            "only_second": false,
        }))
        .unwrap();
        let merged = merge_json_records(&[Some(&first), Some(&second)]).unwrap();
        let expected: JsonMap = serde_json::from_value(json!({
            "shared": {"a": 1, "b": 20, "c": 3},
            "only_first": true,
            "only_second": false,
        }))
        .unwrap();
        assert_eq!(merged, expected);

        assert_eq!(merge_json_records(&[]), None);
        assert_eq!(merge_json_records(&[None, None]), None);
        let empty: JsonMap = JsonMap::new();
        assert_eq!(merge_json_records(&[Some(&empty), Some(&empty)]), None);
    }

    #[test]
    fn merge_provider_options_merges_per_provider() {
        let anthropic_only: ProviderOptions = serde_json::from_value(json!({
            "anthropic": {"alpha": 1},
        }))
        .unwrap();
        let both: ProviderOptions = serde_json::from_value(json!({
            "anthropic": {"beta": 2},
            "openai": {"gamma": 3},
        }))
        .unwrap();
        let merged = merge_provider_options(&[Some(&anthropic_only), Some(&both)]).unwrap();
        let expected: ProviderOptions = serde_json::from_value(json!({
            "anthropic": {"alpha": 1, "beta": 2},
            "openai": {"gamma": 3},
        }))
        .unwrap();
        assert_eq!(merged, expected);
        assert_eq!(merge_provider_options(&[]), None);
    }

    #[test]
    fn merge_http_options_merges_sections() {
        let first = HttpOptions {
            body: Some(serde_json::from_value(json!({"max_tokens": 20})).unwrap()),
            headers: Some(BTreeMap::from([("x-a".to_string(), "1".to_string())])),
            query: None,
        };
        let second = HttpOptions {
            body: None,
            headers: Some(BTreeMap::from([("x-b".to_string(), "2".to_string())])),
            query: Some(BTreeMap::from([("alt".to_string(), "sse".to_string())])),
        };
        let merged = merge_http_options(&[Some(&first), Some(&second)]).unwrap();
        let body = merged.body.unwrap();
        let expected: JsonMap = serde_json::from_value(json!({"max_tokens": 20})).unwrap();
        assert_eq!(body, expected);
        assert_eq!(merged.headers.unwrap().len(), 2);
        assert!(merged.query.is_some());

        assert_eq!(merge_http_options(&[]), None);
        assert_eq!(merge_http_options(&[None, None]), None);
    }

    #[test]
    fn merge_generation_options_last_defined_wins() {
        let base = GenerationOptions {
            max_tokens: Some(1000.0),
            temperature: Some(0.0),
            top_p: Some(0.9),
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            seed: Some(42.0),
            stop: Some(vec!["stop-a".to_string()]),
        };
        let override_ = GenerationOptions {
            max_tokens: None,
            temperature: Some(1.0),
            top_p: None,
            top_k: Some(50.0),
            frequency_penalty: None,
            presence_penalty: None,
            seed: None,
            stop: Some(vec!["stop-b".to_string()]),
        };
        let merged = merge_generation_options(&[Some(&base), Some(&override_)]).unwrap();
        assert_eq!(merged.max_tokens, Some(1000.0));
        assert_eq!(merged.temperature, Some(1.0));
        assert_eq!(merged.top_p, Some(0.9));
        assert_eq!(merged.top_k, Some(50.0));
        assert_eq!(merged.seed, Some(42.0));
        assert_eq!(merged.stop, Some(vec!["stop-b".to_string()]));

        assert_eq!(merge_generation_options(&[]), None);
        let empty = GenerationOptions {
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            seed: None,
            stop: None,
        };
        assert_eq!(merge_generation_options(&[Some(&empty)]), None);
    }

    #[test]
    fn system_part_serializes_with_text_literal() {
        let part = SystemPart {
            r#type: SystemPartType::Text,
            text: "You are helpful".to_string(),
            cache: None,
            metadata: None,
        };
        let value = serde_json::to_value(&part).unwrap();
        assert_eq!(value, json!({"type": "text", "text": "You are helpful"}),);
    }
}
