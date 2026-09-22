//! `Provider.Service` (provider.ts) — the provider state machinery:
//! models-dev → `Info` mapping with `ProviderTransform` variants, config
//! provider merging, env/api credential `source`/`key` merging, and the
//! runtime model resolution the engine prompts resolve through (M7.7).

use std::collections::BTreeMap;

use serde_json::{json, Value};

use alforria_core::catalog::{self, Cost, Interleaved, Model, Provider as CatalogProvider};
use alforria_core::config::schema::Config;
use alforria_core::merge::merge_deep;
use alforria_core::provider::{reasoning_variants, variants, RuntimeModel};

use crate::error::ServerError;
use crate::state::{AuthStore, CatalogSource};

// ---------------------------------------------------------------------------
// models-dev → provider.Info mapping (provider/provider.ts:1265-1352)
// ---------------------------------------------------------------------------

/// `cloudflareGatewayNpm` (provider.ts:1258-1262).
pub(crate) fn cloudflare_gateway_npm(provider_id: &str, model_id: &str) -> Option<&'static str> {
    if provider_id != "cloudflare-ai-gateway" {
        return None;
    }
    if model_id.starts_with("openai/") {
        return Some("@ai-sdk/openai");
    }
    if model_id.starts_with("anthropic/") {
        return Some("@ai-sdk/anthropic");
    }
    None
}

/// `cost` (provider.ts:1221-1251).
pub(crate) fn cost_value(c: Option<&Cost>) -> Value {
    let empty: Option<&Cost> = None;
    let c = c.or(empty);
    let mut result = json!({
        "input": c.map(|c| c.input).unwrap_or(0.0),
        "output": c.map(|c| c.output).unwrap_or(0.0),
        "cache": {
            "read": c.and_then(|c| c.cache_read).unwrap_or(0.0),
            "write": c.and_then(|c| c.cache_write).unwrap_or(0.0),
        },
    });
    if let Some(c) = c {
        if let Some(tiers) = &c.tiers {
            result["tiers"] = json!(tiers
                .iter()
                .map(|item| json!({
                    "input": item.input,
                    "output": item.output,
                    "cache": {
                        "read": item.cache_read.unwrap_or(0.0),
                        "write": item.cache_write.unwrap_or(0.0),
                    },
                    "tier": item.tier,
                }))
                .collect::<Vec<_>>());
        }
        if let Some(over) = &c.context_over_200k {
            result["experimentalOver200K"] = json!({
                "cache": {
                    "read": over.cache_read.unwrap_or(0.0),
                    "write": over.cache_write.unwrap_or(0.0),
                },
                "input": over.input,
                "output": over.output,
            });
        }
    }
    result
}

fn has_modality(model: &Model, output: bool, name: &str) -> bool {
    let Some(modalities) = &model.modalities else {
        return false;
    };
    let list = if output {
        &modalities.output
    } else {
        &modalities.input
    };
    list.iter().any(|m| {
        matches!(
            (m, name),
            (catalog::Modality::Text, "text")
                | (catalog::Modality::Audio, "audio")
                | (catalog::Modality::Image, "image")
                | (catalog::Modality::Video, "video")
                | (catalog::Modality::Pdf, "pdf")
        )
    })
}

fn get_str(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// `modeOptions` (provider.ts:1349-1358) — snake_case body keys become
/// camelCase, and openai's `reasoning.mode` folds into `reasoningMode`.
fn mode_options(model: &Value, body: Option<&BTreeMap<String, Value>>) -> Value {
    let Some(body) = body else {
        return model["options"].clone();
    };
    let mut options = serde_json::Map::new();
    for (key, value) in body {
        let mut transformed = String::new();
        let mut chars = key.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '_' && chars.peek().is_some_and(|n| n.is_ascii_lowercase()) {
                transformed.push(chars.next().unwrap().to_ascii_uppercase());
            } else {
                transformed.push(c);
            }
        }
        options.insert(transformed, value.clone());
    }
    if model["api"]["npm"] != "@ai-sdk/openai" {
        return Value::Object(options);
    }
    let Some(reasoning) = body.get("reasoning").and_then(Value::as_object) else {
        return Value::Object(options);
    };
    let Some(mode) = reasoning.get("mode").and_then(Value::as_str) else {
        return Value::Object(options);
    };
    options.remove("reasoning");
    options.insert("reasoningMode".to_string(), json!(mode));
    Value::Object(options)
}

/// The model slice of the runtime `Provider.Model` the variant machinery
/// reads (alforria-core `provider::RuntimeModel`).
pub(crate) fn runtime_model(provider_id: &str, model: &Value) -> RuntimeModel {
    RuntimeModel {
        id: get_str(model, "id"),
        provider_id: provider_id.to_string(),
        api_id: get_str(&model["api"], "id"),
        api_npm: get_str(&model["api"], "npm"),
        api_url: get_str(&model["api"], "url"),
        release_date: get_str(model, "release_date"),
        family: model
            .get("family")
            .and_then(Value::as_str)
            .map(String::from),
        limit_output: model["limit"]["output"].as_f64().unwrap_or(0.0),
        reasoning: model["capabilities"]["reasoning"]
            .as_bool()
            .unwrap_or(false),
    }
}

fn variants_value(computed: alforria_core::provider::Variants) -> Value {
    Value::Object(computed.into_iter().collect())
}

/// `fromModelsDevModel` (provider.ts:1265-1320) including the
/// `reasoningVariants(model, base) ?? variants(base)` computation (M7.7).
fn from_models_dev_model(provider: &CatalogProvider, model: &Model) -> Value {
    let mut value = json!({
        "id": model.id,
        "providerID": provider.id,
        "name": model.name,
        "api": {
            "id": model.id,
            "url": model.provider.as_ref().and_then(|p| p.api.clone())
                .or(provider.api.clone())
                .unwrap_or_default(),
            "npm": cloudflare_gateway_npm(&provider.id, &model.id)
                .map(String::from)
                .or_else(|| model.provider.as_ref().and_then(|p| p.npm.clone()))
                .or_else(|| provider.npm.clone())
                .unwrap_or_else(|| "@ai-sdk/openai-compatible".to_string()),
        },
        "status": match model.status {
            Some(catalog::CatalogModelStatus::Alpha) => "alpha",
            Some(catalog::CatalogModelStatus::Beta) => "beta",
            Some(catalog::CatalogModelStatus::Deprecated) => "deprecated",
            None => "active",
        },
        "headers": {},
        "options": {},
        "cost": cost_value(model.cost.as_ref()),
        "limit": {
            "context": model.limit.context,
            "output": model.limit.output,
        },
        "capabilities": {
            "temperature": model.temperature,
            "reasoning": model.reasoning,
            "attachment": model.attachment,
            "toolcall": model.tool_call,
            "input": {
                "text": has_modality(model, false, "text"),
                "audio": has_modality(model, false, "audio"),
                "image": has_modality(model, false, "image"),
                "video": has_modality(model, false, "video"),
                "pdf": has_modality(model, false, "pdf"),
            },
            "output": {
                "text": has_modality(model, true, "text"),
                "audio": has_modality(model, true, "audio"),
                "image": has_modality(model, true, "image"),
                "video": has_modality(model, true, "video"),
                "pdf": has_modality(model, true, "pdf"),
            },
            "interleaved": match model.interleaved.clone().unwrap_or(Interleaved::Boolean(false)) {
                Interleaved::Field(field) => json!({ "field": field }),
                Interleaved::Boolean(value) => Value::from(value),
                Interleaved::Struct { field } => json!({ "field": field }),
            },
        },
        "release_date": model.release_date,
        "variants": {},
    });
    if let Some(family) = &model.family {
        value["family"] = json!(family);
    }
    if let Some(input) = model.limit.input {
        value["limit"]["input"] = json!(input);
    }
    let runtime = runtime_model(&provider.id, &value);
    let computed = reasoning_variants(model, &runtime).unwrap_or_else(|| variants(&runtime));
    value["variants"] = variants_value(computed);
    value
}

/// `fromModelsDevProvider` (provider.ts:1322-1347) — mode expansion keeps the
/// base model's variants.
pub fn from_models_dev_provider(provider: &CatalogProvider) -> Value {
    let mut models = serde_json::Map::new();
    for (key, model) in &provider.models {
        let base = from_models_dev_model(provider, model);
        models.insert(key.clone(), base.clone());
        let Some(modes) = model.experimental.as_ref().and_then(|e| e.modes.as_ref()) else {
            continue;
        };
        for (mode, opts) in modes {
            let id = format!("{}-{}", model.id, mode);
            let mut variant = base.clone();
            variant["id"] = json!(id);
            variant["name"] = json!(format!(
                "{} {}{}",
                model.name,
                mode[..1].to_uppercase(),
                &mode[1..]
            ));
            if let Some(cost) = opts.cost.as_ref().map(|c| cost_value(Some(c))) {
                variant["cost"] = cost;
            }
            variant["options"] = mode_options(
                &variant,
                opts.provider.as_ref().and_then(|p| p.body.as_ref()),
            );
            if let Some(headers) = opts.provider.as_ref().and_then(|p| p.headers.as_ref()) {
                variant["headers"] = json!(headers);
            }
            models.insert(id, variant);
        }
    }
    json!({
        "id": provider.id,
        "source": "custom",
        "name": provider.name,
        "env": provider.env,
        "options": {},
        "models": Value::Object(models),
    })
}

// ---------------------------------------------------------------------------
// State init (provider.ts:1400-1719)
// ---------------------------------------------------------------------------

/// The `Provider.Service` state: `database` (catalog) and `providers`
/// (the connected subset, provider.ts:1408).
#[derive(Default)]
pub struct ProviderState {
    pub database: BTreeMap<String, Value>,
    pub providers: BTreeMap<String, Value>,
}

/// `mergeProvider` (provider.ts:1427-1438) — deep-merge into the connected
/// providers map, falling back to the catalog entry.
fn merge_provider(state: &mut ProviderState, provider_id: &str, patch: Value) {
    let Some(mut existing) = state
        .providers
        .get(provider_id)
        .or_else(|| state.database.get(provider_id))
        .cloned()
    else {
        return;
    };
    merge_deep(&mut existing, &patch);
    state.providers.insert(provider_id.to_string(), existing);
}

/// `parseModel` (provider.ts:2058-2064).
pub fn parse_model(model: &str) -> (String, String) {
    let (provider, rest) = model.split_once('/').unwrap_or((model, ""));
    (provider.to_string(), rest.to_string())
}

fn modality_includes(list: Option<&[String]>, name: &str) -> Option<bool> {
    list.map(|input| input.iter().any(|item| item == name))
}

fn modality_option(
    list: Option<&[String]>,
    name: &str,
    existing: Option<bool>,
    fallback: bool,
) -> bool {
    modality_includes(list, name)
        .or(existing)
        .unwrap_or(fallback)
}

fn json_map(map: BTreeMap<String, Value>) -> Value {
    Value::Object(map.into_iter().collect())
}

/// The config-provider model merge (provider.ts:1493-1577).
fn config_models(
    provider_id: &str,
    config_provider: &alforria_core::config::schema::ProviderInfo,
    catalog_provider: Option<&CatalogProvider>,
    parsed_models: &mut serde_json::Map<String, Value>,
) {
    for (model_id, model) in config_provider.models.clone().unwrap_or_default() {
        let lookup = model.id.clone().unwrap_or_else(|| model_id.clone());
        let existing_model = parsed_models.get(&lookup).cloned();
        let existing_api_id = existing_model.as_ref().map(|m| get_str(&m["api"], "id"));
        let api_id = model
            .id
            .clone()
            .or(existing_api_id)
            .unwrap_or_else(|| model_id.clone());
        let api_npm = model
            .provider
            .as_ref()
            .and_then(|p| p.npm.clone())
            .or_else(|| config_provider.npm.clone())
            .or_else(|| existing_model.as_ref().map(|m| get_str(&m["api"], "npm")))
            .or_else(|| cloudflare_gateway_npm(provider_id, &api_id).map(String::from))
            .or_else(|| catalog_provider.and_then(|c| c.npm.clone()))
            .unwrap_or_else(|| "@ai-sdk/openai-compatible".to_string());
        let name = if let Some(name) = &model.name {
            name.clone()
        } else if model.id.is_some_and(|id| id != model_id) {
            model_id.clone()
        } else {
            existing_model
                .as_ref()
                .map(|m| get_str(m, "name"))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| model_id.clone())
        };
        let caps = existing_model
            .as_ref()
            .and_then(|m| m.get("capabilities"))
            .cloned()
            .unwrap_or(Value::Null);
        let cap_bool = |key: &str| caps.get(key).and_then(Value::as_bool).unwrap_or(false);
        let existing_limit_input = existing_model
            .as_ref()
            .and_then(|m| m["limit"].get("input"))
            .cloned();
        let status = model
            .status
            .map(|s| match s {
                alforria_core::config::schema::ProviderModelStatus::Alpha => "alpha",
                alforria_core::config::schema::ProviderModelStatus::Beta => "beta",
                alforria_core::config::schema::ProviderModelStatus::Deprecated => "deprecated",
                alforria_core::config::schema::ProviderModelStatus::Active => "active",
            })
            .map(String::from)
            .or_else(|| {
                existing_model
                    .as_ref()
                    .map(|m| get_str(m, "status"))
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "active".to_string());
        let interleaved = match model.interleaved.clone() {
            Some(alforria_core::config::schema::ProviderModelInterleaved::Field(field)) => {
                json!({"field": field})
            }
            Some(alforria_core::config::schema::ProviderModelInterleaved::Struct { field }) => {
                json!({"field": field})
            }
            Some(alforria_core::config::schema::ProviderModelInterleaved::Boolean(value)) => {
                Value::from(value)
            }
            None => existing_model
                .as_ref()
                .and_then(|m| m["capabilities"].get("interleaved"))
                .cloned()
                .unwrap_or_else(|| {
                    if existing_model.is_none()
                        && api_npm == "@ai-sdk/openai-compatible"
                        && api_id.contains("deepseek")
                    {
                        json!({"field": "reasoning_content"})
                    } else {
                        Value::Bool(false)
                    }
                }),
        };
        let mut parsed_model = json!({
            "id": model_id,
            "api": {
                "id": api_id,
                "npm": api_npm,
                "url": model.provider.as_ref().and_then(|p| p.api.clone())
                    .or_else(|| config_provider.api.clone())
                    .or_else(|| existing_model.as_ref().map(|m| get_str(&m["api"], "url")).filter(|url| !url.is_empty()))
                    .or_else(|| catalog_provider.and_then(|c| c.api.clone()))
                    .unwrap_or_default(),
            },
            "status": status,
            "name": name,
            "providerID": provider_id,
            "capabilities": {
                "temperature": model.temperature.unwrap_or_else(|| cap_bool("temperature")),
                "reasoning": model.reasoning.unwrap_or_else(|| cap_bool("reasoning")),
                "attachment": model.attachment.unwrap_or_else(|| cap_bool("attachment")),
                "toolcall": model.tool_call.unwrap_or_else(|| cap_bool("toolcall")),
                "input": {
                    "text": modality_option(model.modalities.as_ref().and_then(|m| m.input.as_deref()), "text", caps["input"]["text"].as_bool(), true),
                    "audio": modality_option(model.modalities.as_ref().and_then(|m| m.input.as_deref()), "audio", caps["input"]["audio"].as_bool(), false),
                    "image": modality_option(model.modalities.as_ref().and_then(|m| m.input.as_deref()), "image", caps["input"]["image"].as_bool(), false),
                    "video": modality_option(model.modalities.as_ref().and_then(|m| m.input.as_deref()), "video", caps["input"]["video"].as_bool(), false),
                    "pdf": modality_option(model.modalities.as_ref().and_then(|m| m.input.as_deref()), "pdf", caps["input"]["pdf"].as_bool(), false),
                },
                "output": {
                    "text": modality_option(model.modalities.as_ref().and_then(|m| m.output.as_deref()), "text", caps["output"]["text"].as_bool(), true),
                    "audio": modality_option(model.modalities.as_ref().and_then(|m| m.output.as_deref()), "audio", caps["output"]["audio"].as_bool(), false),
                    "image": modality_option(model.modalities.as_ref().and_then(|m| m.output.as_deref()), "image", caps["output"]["image"].as_bool(), false),
                    "video": modality_option(model.modalities.as_ref().and_then(|m| m.output.as_deref()), "video", caps["output"]["video"].as_bool(), false),
                    "pdf": modality_option(model.modalities.as_ref().and_then(|m| m.output.as_deref()), "pdf", caps["output"]["pdf"].as_bool(), false),
                },
                "interleaved": interleaved,
            },
            "cost": {
                "input": model.cost.as_ref().map(|c| c.input)
                    .or_else(|| existing_model.as_ref().and_then(|m| m["cost"]["input"].as_f64()))
                    .unwrap_or(0.0),
                "output": model.cost.as_ref().map(|c| c.output)
                    .or_else(|| existing_model.as_ref().and_then(|m| m["cost"]["output"].as_f64()))
                    .unwrap_or(0.0),
                "cache": {
                    "read": model.cost.as_ref().and_then(|c| c.cache_read)
                        .or_else(|| existing_model.as_ref().and_then(|m| m["cost"]["cache"]["read"].as_f64()))
                        .unwrap_or(0.0),
                    "write": model.cost.as_ref().and_then(|c| c.cache_write)
                        .or_else(|| existing_model.as_ref().and_then(|m| m["cost"]["cache"]["write"].as_f64()))
                        .unwrap_or(0.0),
                },
            },
            "options": merge_deep_values(
                existing_model
                    .as_ref()
                    .and_then(|m| m.get("options"))
                    .cloned()
                    .unwrap_or(json!({})),
                model
                    .options
                    .clone()
                    .map(json_map)
                    .unwrap_or(json!({})),
            ),
            "limit": {
                "context": model.limit.as_ref().map(|l| l.context)
                    .or_else(|| existing_model.as_ref().and_then(|m| m["limit"]["context"].as_f64()))
                    .unwrap_or(0.0),
                "output": model.limit.as_ref().map(|l| l.output)
                    .or_else(|| existing_model.as_ref().and_then(|m| m["limit"]["output"].as_f64()))
                    .unwrap_or(0.0),
            },
            "headers": merge_deep_values(
                existing_model.as_ref().and_then(|m| m.get("headers")).cloned().unwrap_or(json!({})),
                model.headers.clone().map(|h| {
                    Value::Object(h.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect())
                }).unwrap_or(json!({})),
            ),
            "family": model.family.clone()
                .or_else(|| existing_model.as_ref().and_then(|m| m.get("family")).and_then(Value::as_str).map(String::from))
                .unwrap_or_default(),
            "release_date": model.release_date.clone()
                .or_else(|| existing_model.as_ref().map(|m| get_str(m, "release_date")))
                .unwrap_or_default(),
            "variants": {},
        });
        if let Some(input) = model.limit.as_ref().and_then(|l| l.input) {
            parsed_model["limit"]["input"] = json!(input);
        } else if let Some(input) = existing_limit_input {
            parsed_model["limit"]["input"] = input;
        }
        let runtime = runtime_model(provider_id, &parsed_model);
        let same_npm = existing_model
            .as_ref()
            .is_some_and(|m| get_str(&m["api"], "npm") == api_npm);
        let base = if same_npm {
            existing_model
                .as_ref()
                .and_then(|m| m.get("variants"))
                .cloned()
                .unwrap_or_else(|| variants_value(variants(&runtime)))
        } else {
            variants_value(variants(&runtime))
        };
        let mut merged = base;
        if let Some(config_variants) = model.variants.clone() {
            let converted = Value::Object(
                config_variants
                    .iter()
                    .map(|(k, v)| (k.clone(), json_map(v.clone())))
                    .collect(),
            );
            merge_deep(&mut merged, &converted);
        }
        parsed_model["variants"] = filter_disabled_variants(merged);
        parsed_models.insert(model_id, parsed_model);
    }
}

fn merge_deep_values(target: Value, patch: Value) -> Value {
    let mut target = target;
    merge_deep(&mut target, &patch);
    target
}

/// `pickBy(merged, v => !v.disabled)` + `omit(v, ["disabled"])`
/// (provider.ts:1573-1576, 1708-1711).
fn filter_disabled_variants(merged: Value) -> Value {
    let Some(map) = merged.as_object() else {
        return json!({});
    };
    let mut out = serde_json::Map::new();
    for (id, value) in map {
        let Some(entry) = value.as_object() else {
            continue;
        };
        if entry.get("disabled").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let mut cleaned = entry.clone();
        cleaned.remove("disabled");
        out.insert(id.clone(), Value::Object(cleaned));
    }
    Value::Object(out)
}

/// The `Provider.Service` state init (provider.ts:1400-1719): catalog →
/// config extension → env/api credential merging → config re-apply →
/// model filtering + variant merging.
pub fn build_state(
    catalog: &alforria_core::catalog::Providers,
    config: &Config,
    auth: &dyn AuthStore,
) -> Result<ProviderState, ServerError> {
    let mut state = ProviderState::default();
    for (id, provider) in catalog {
        state
            .database
            .insert(id.clone(), from_models_dev_provider(provider));
    }

    let disabled = |id: &str| {
        config
            .disabled_providers
            .iter()
            .flatten()
            .any(|item| item == id)
    };
    let config_providers = config.provider.clone().unwrap_or_default();

    // extend database from config (provider.ts:1482-1580)
    for (provider_id, provider) in &config_providers {
        let catalog_provider = catalog.get(provider_id);
        let existing = state.database.get(provider_id);
        let mut parsed = json!({
            "id": provider_id,
            "name": provider.name.clone()
                .or_else(|| {
                    existing.map(|e| get_str(e, "name")).filter(|name| !name.is_empty())
                })
                .unwrap_or_else(|| provider_id.clone()),
            "env": provider
                .env
                .clone()
                .map(|env| json!(env))
                .or_else(|| {
                    existing
                        .map(|e| e["env"].clone())
                        .filter(|v| !v.is_null())
                })
                .unwrap_or_else(|| json!([])),
            "options": merge_deep_values(
                existing.and_then(|e| e.get("options")).cloned().unwrap_or(json!({})),
                provider.options.clone().map(|o| serde_json::to_value(o).unwrap_or_default()).unwrap_or(json!({})),
            ),
            "source": "config",
            "models": existing.map(|e| e["models"].clone()).unwrap_or(json!({})),
        });
        if let Value::Object(models) = &mut parsed["models"] {
            config_models(provider_id, provider, catalog_provider, models);
        }
        state.database.insert(provider_id.clone(), parsed);
    }

    // load env (provider.ts:1583-1593)
    let env_entries: Vec<(String, Value)> = state
        .database
        .iter()
        .filter(|(id, _)| !disabled(id))
        .map(|(id, provider)| (id.clone(), provider.clone()))
        .collect();
    for (id, provider) in env_entries {
        let env: Vec<String> = provider
            .get("env")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let api_key = env
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .find(|value| !value.is_empty());
        let Some(api_key) = api_key else {
            continue;
        };
        let mut patch = json!({"source": "env"});
        if env.len() == 1 {
            patch["key"] = Value::String(api_key);
        }
        merge_provider(&mut state, &id, patch);
    }

    // load apikeys (provider.ts:1595-1606)
    let auths = auth.all()?;
    for (id, provider) in &auths {
        if disabled(id) {
            continue;
        }
        if provider.get("type").and_then(Value::as_str) == Some("api") {
            let mut patch = json!({"source": "api"});
            if let Some(key) = provider.get("key").and_then(Value::as_str) {
                patch["key"] = Value::String(key.to_string());
            }
            merge_provider(&mut state, id, patch);
        }
    }

    // load config - re-apply with updated data (provider.ts:1647-1655)
    for (provider_id, provider) in &config_providers {
        let mut patch = json!({"source": "config"});
        if let Some(env) = &provider.env {
            patch["env"] = json!(env);
        }
        if let Some(name) = &provider.name {
            patch["name"] = json!(name);
        }
        if provider.options.is_some() {
            patch["options"] = serde_json::to_value(provider.options.clone()).unwrap_or_default();
        }
        merge_provider(&mut state, provider_id, patch);
    }

    filter_providers(&mut state, config, &config_providers);
    Ok(state)
}

/// The final model loop (provider.ts:1671-1719): allowed filtering, model
/// deletion rules and the config variant merge.
fn filter_providers(
    state: &mut ProviderState,
    config: &Config,
    config_providers: &BTreeMap<String, alforria_core::config::schema::ProviderInfo>,
) {
    let enabled = config.enabled_providers.clone();
    let allowed = |id: &str| -> bool {
        if let Some(enabled) = &enabled {
            if !enabled.iter().any(|item| item == id) {
                return false;
            }
        }
        !config
            .disabled_providers
            .iter()
            .flatten()
            .any(|item| item == id)
    };
    let enable_experimental = std::env::var("OPENCODE_ENABLE_EXPERIMENTAL_MODELS")
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let ids: Vec<String> = state.providers.keys().cloned().collect();
    for provider_id in ids {
        let Some(mut provider) = state.providers.get(&provider_id).cloned() else {
            continue;
        };
        if !allowed(&provider_id) {
            state.providers.remove(&provider_id);
            continue;
        }
        let config_provider = config_providers.get(&provider_id);
        let blacklist = config_provider.and_then(|p| p.blacklist.clone());
        let whitelist = config_provider.and_then(|p| p.whitelist.clone());
        let models = provider
            .get("models")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut filtered = serde_json::Map::new();
        for (model_id, mut model) in models {
            if model_id == "gpt-5-chat-latest"
                && (provider_id == "openai"
                    || provider_id == "github-copilot"
                    || provider_id == "openrouter")
            {
                continue;
            }
            if provider_id == "openrouter" && model_id == "openai/gpt-5-chat" {
                continue;
            }
            if model.get("status").and_then(Value::as_str) == Some("alpha") && !enable_experimental
            {
                continue;
            }
            if model.get("status").and_then(Value::as_str) == Some("deprecated") {
                continue;
            }
            if blacklist
                .as_ref()
                .is_some_and(|list| list.contains(&model_id))
            {
                continue;
            }
            if whitelist
                .as_ref()
                .is_some_and(|list| !list.contains(&model_id))
            {
                continue;
            }
            if model.get("variants").is_none() {
                let runtime = runtime_model(&provider_id, &model);
                model["variants"] = variants_value(variants(&runtime));
            }
            if let Some(config_variants) = config_provider
                .and_then(|p| p.models.as_ref())
                .and_then(|models| models.get(&model_id))
                .and_then(|m| m.variants.clone())
            {
                let converted = Value::Object(
                    config_variants
                        .iter()
                        .map(|(k, v)| (k.clone(), json_map(v.clone())))
                        .collect(),
                );
                if let Some(current) = model.get_mut("variants") {
                    merge_deep(current, &converted);
                }
                if let Some(current) = model.get_mut("variants") {
                    *current = filter_disabled_variants(current.clone());
                }
            }
            filtered.insert(model_id, model);
        }
        if filtered.is_empty() {
            state.providers.remove(&provider_id);
            continue;
        }
        provider["models"] = Value::Object(filtered);
        state.providers.insert(provider_id, provider);
    }
}

/// Load the catalog through the source, mapping failures onto the defect
/// error surface.
pub fn load_catalog(
    catalog: &CatalogSource,
) -> Result<alforria_core::catalog::Providers, ServerError> {
    catalog().map_err(|err| ServerError::Core(alforria_core::CoreError::Catalog(err.to_string())))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_provider() -> CatalogProvider {
        serde_json::from_value(json!({
            "api": "https://api.example",
            "name": "Example",
            "env": ["EXAMPLE_API_KEY"],
            "id": "example",
            "npm": "@example/sdk",
            "models": {
                "model-a": {
                    "id": "model-a", "name": "Model A", "release_date": "2025-01-01",
                    "attachment": true, "reasoning": false, "temperature": true, "tool_call": true,
                    "cost": {"input": 1, "output": 2, "cache_read": 0.1},
                    "limit": {"context": 100, "output": 10},
                    "modalities": {"input": ["text"], "output": ["text"]}
                }
            }
        }))
        .unwrap()
    }

    struct EmptyAuth;

    impl AuthStore for EmptyAuth {
        fn set(&self, _provider_id: &str, _info: Value) -> Result<(), ServerError> {
            Ok(())
        }
        fn remove(&self, _provider_id: &str) -> Result<(), ServerError> {
            Ok(())
        }
        fn has(&self, _provider_id: &str) -> bool {
            false
        }
        fn ids(&self) -> Vec<String> {
            Vec::new()
        }
        fn all(&self) -> Result<BTreeMap<String, Value>, ServerError> {
            Ok(BTreeMap::new())
        }
    }

    #[test]
    fn from_models_dev_provider_shape() {
        let info = from_models_dev_provider(&catalog_provider());
        assert_eq!(info["id"], "example");
        assert_eq!(info["source"], "custom");
        let model = &info["models"]["model-a"];
        assert_eq!(model["providerID"], "example");
        assert_eq!(model["api"]["url"], "https://api.example");
        assert_eq!(model["api"]["npm"], "@example/sdk");
        assert_eq!(model["status"], "active");
        assert_eq!(model["cost"]["input"], 1.0);
        assert_eq!(model["cost"]["cache"]["read"], 0.1);
        assert_eq!(model["capabilities"]["input"]["text"], true);
        assert_eq!(model["capabilities"]["input"]["image"], false);
        assert_eq!(model["capabilities"]["toolcall"], true);
        // non-reasoning models keep `variants: {}`
        assert_eq!(model["variants"], json!({}));
    }

    #[test]
    fn build_state_env_and_api_source_key_merging() {
        let catalog = BTreeMap::from([("example".to_string(), catalog_provider())]);
        let config = serde_json::from_value::<Config>(json!({})).unwrap();

        // No credentials → not connected.
        let state = build_state(&catalog, &config, &EmptyAuth).unwrap();
        assert!(state.providers.is_empty());

        // Env var → source "env" + key (single env var).
        std::env::set_var("EXAMPLE_API_KEY", "sk-env");
        let state = build_state(&catalog, &config, &EmptyAuth).unwrap();
        let provider = &state.providers["example"];
        assert_eq!(provider["source"], "env");
        assert_eq!(provider["key"], "sk-env");

        // auth.json api entry wins (applied after the env leg).
        let state = build_state(&catalog, &config, &ApiKeyAuth).unwrap();
        let provider = &state.providers["example"];
        assert_eq!(provider["source"], "api");
        assert_eq!(provider["key"], "sk-api");
        std::env::remove_var("EXAMPLE_API_KEY");

        // No env var → the api entry still connects.
        let state = build_state(&catalog, &config, &ApiKeyAuth).unwrap();
        let provider = &state.providers["example"];
        assert_eq!(provider["source"], "api");
        assert_eq!(provider["key"], "sk-api");
    }

    struct ApiKeyAuth;

    impl AuthStore for ApiKeyAuth {
        fn set(&self, _provider_id: &str, _info: Value) -> Result<(), ServerError> {
            Ok(())
        }
        fn remove(&self, _provider_id: &str) -> Result<(), ServerError> {
            Ok(())
        }
        fn has(&self, _provider_id: &str) -> bool {
            true
        }
        fn ids(&self) -> Vec<String> {
            vec!["example".to_string()]
        }
        fn all(&self) -> Result<BTreeMap<String, Value>, ServerError> {
            Ok(BTreeMap::from([(
                "example".to_string(),
                json!({"type": "api", "key": "sk-api"}),
            )]))
        }
    }
}
