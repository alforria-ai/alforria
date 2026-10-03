//! M7.7 production provider runtime — `Provider.Service` over the M7.7
//! provider state (`crate::provider`): model resolution for the session
//! loop (`ModelSource`), V2 model info for prompt input (`InputModels`),
//! the share model seam and the alforria-llm route sender (`LlmStream`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

use crate::provider::{build_state, ProviderState};
use alforria_core::session::llm::{LlmModel, LlmRequestSender};
use alforria_core::session::overflow::ModelLimits;
use alforria_core::session::prompt_input::Models;
use alforria_core::session::r#loop::{LoopError, ModelSource, ResolvedModel};
use alforria_core::session::usage::{CacheCost, CostTier, ModelCost, Over200k};
use alforria_core::tool::def::BoxFuture;
use alforria_llm::route::client::{Route, RouteHandle};
use alforria_llm::schema::messages::ModelRef;
use alforria_schema::model::{
    ModelApi, ModelCapabilities, ModelCost as SchemaModelCost, ModelCostCache, ModelCostTier,
    ModelCostTierType, ModelInfo, ModelLimit, ModelRequest, ModelStatus, ModelTime, ModelVariant,
};
use serde_json::{json, Value};

use crate::error::ServerError;
use crate::state::AuthStore;

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn get_str(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

/// `parseModel` (provider.ts:2061-2068) — split at the first `/`.
pub fn parse_model(model: &str) -> (String, String) {
    match model.split_once('/') {
        Some((provider_id, model_id)) => (provider_id.to_string(), model_id.to_string()),
        None => (model.to_string(), String::new()),
    }
}

/// `RuntimeFlags.outputTokenMax` (runtime-flags.ts) —
/// `OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX`.
pub fn output_token_max() -> Option<f64> {
    std::env::var("OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
}

/// `sort` (provider.ts:2050-2058) — the priority index, `latest` first,
/// id desc.
fn sort_model_ids(mut models: Vec<String>) -> Vec<String> {
    // Deviation from provider.ts:2050-2058 — "glm-5.3-thinking" appended so
    // the built-in libertai provider wins when nothing earlier is connected.
    const PRIORITY: [&str; 5] = [
        "gpt-5",
        "claude-sonnet-4",
        "big-pickle",
        "gemini-3-pro",
        "glm-5.3-thinking",
    ];
    let priority = |id: &str| -> i64 {
        PRIORITY
            .iter()
            .position(|f| id.contains(f))
            .map(|i| i as i64)
            .unwrap_or(-1)
    };
    models.sort_by(|a, b| {
        priority(b)
            .cmp(&priority(a))
            .then_with(|| (!a.contains("latest")).cmp(&!b.contains("latest")))
            .then_with(|| b.cmp(a))
    });
    models
}

/// `modelSuggestions` (provider.ts:1359-1378) — the fuzzysort leg over the
/// non-deprecated (and non-alpha, without the experimental flag) model ids,
/// then the split-query fallback.
fn model_suggestions(provider: Option<&Value>, model_id: &str) -> Vec<String> {
    let Some(models) = provider.map(|p| &p["models"]) else {
        return Vec::new();
    };
    let Some(models) = models.as_object() else {
        return Vec::new();
    };
    let enable_experimental = crate::engine::bool_env("OPENCODE_ENABLE_EXPERIMENTAL_MODELS");
    let available = models
        .iter()
        .filter(|(_, model)| {
            let status = model["status"].as_str();
            if status == Some("deprecated") {
                return false;
            }
            if status == Some("alpha") && !enable_experimental {
                return false;
            }
            true
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<String>>();
    let fuzzy = alforria_core::fuzzysort::go(
        model_id,
        &available,
        &alforria_core::fuzzysort::Options {
            limit: Some(3),
            threshold: Some(-10000.0),
        },
    );
    if !fuzzy.is_empty() {
        return fuzzy
            .into_iter()
            .map(|index| available[index].clone())
            .collect();
    }
    let query: Vec<String> = model_id
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| part.len() > 1)
        .map(String::from)
        .collect();
    let mut scored: Vec<(String, usize)> = available
        .iter()
        .map(|id| {
            let lower = id.to_lowercase();
            (
                id.clone(),
                query
                    .iter()
                    .filter(|part| lower.contains(part.as_str()))
                    .count(),
            )
        })
        .filter(|(_, score)| *score > 0)
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.into_iter().take(3).map(|(id, _)| id).collect()
}

/// Provider suggestions when the provider itself is missing
/// (provider.ts:1880-1885) — `fuzzysort.go(providerID, keys({...catalog,
/// ...providers}), {limit: 3, threshold: -10000})`.
fn provider_suggestions(state: &ProviderState, provider_id: &str) -> Vec<String> {
    let mut keys = state.database.keys().cloned().collect::<Vec<String>>();
    keys.extend(state.providers.keys().cloned());
    let indices = alforria_core::fuzzysort::go(
        provider_id,
        &keys,
        &alforria_core::fuzzysort::Options {
            limit: Some(3),
            threshold: Some(-10000.0),
        },
    );
    indices
        .into_iter()
        .map(|index| keys[index].clone())
        .collect()
}

// ---------------------------------------------------------------------------
// RuntimeModels
// ---------------------------------------------------------------------------

struct Inner {
    /// The inputs the provider state is rebuilt from when credentials
    /// change.
    catalog: alforria_core::catalog::Providers,
    config: alforria_core::config::schema::Config,
    auth: Arc<dyn AuthStore>,
    /// The provider state and the auth generation it was built against.
    state: RwLock<(u64, Arc<ProviderState>)>,
    /// `cfg.model` (the `defaultModel` first leg).
    config_model: Option<String>,
    /// `cfg.small_model` (the `getSmallModel` first leg).
    config_small_model: Option<String>,
    /// Keys of `cfg.provider` — the `defaultModel` provider filter.
    config_provider_ids: Vec<String>,
    /// `Global.Path.state` — `<state>/model.json` recent models.
    state_dir: PathBuf,
    output_token_max: Option<f64>,
}

/// The production `Provider.Service` runtime (provider.ts:1501-2043): the
/// per-instance provider state with the env/api credentials merged, plus
/// the model-resolution surface the session engine consumes.
///
/// Deliberate improvement over TS: there the state is built once per
/// instance, so a new credential only reaches open projects after an
/// instance dispose (which interrupts running sessions). Here the state is
/// rebuilt at the next model lookup whenever the auth store's generation
/// moves — a sign-in, `PUT`/`DELETE /auth`, or a CLI login noticed through
/// auth.json's mtime. Lookups take an `Arc` snapshot of the state and copy
/// the credentials into the resolved model, so a rebuild never touches a
/// request already in flight.
#[derive(Clone)]
pub struct RuntimeModels(Arc<Inner>);

impl RuntimeModels {
    pub fn new(
        catalog: alforria_core::catalog::Providers,
        config: &alforria_core::config::schema::Config,
        auth: Arc<dyn AuthStore>,
        paths: &alforria_core::GlobalPaths,
    ) -> Result<RuntimeModels, ServerError> {
        // Read the generation first: a change racing the build moves it
        // past the recorded one, and the next lookup rebuilds.
        let generation = auth.generation();
        let state = build_state(&catalog, config, auth.as_ref())?;
        Ok(RuntimeModels(Arc::new(Inner {
            catalog,
            config: config.clone(),
            auth,
            state: RwLock::new((generation, Arc::new(state))),
            config_model: config.model.clone(),
            config_small_model: config.small_model.clone(),
            config_provider_ids: config
                .provider
                .as_ref()
                .map(|provider| provider.keys().cloned().collect())
                .unwrap_or_default(),
            state_dir: paths.state.clone(),
            output_token_max: output_token_max(),
        })))
    }

    /// The provider state for the current credentials, rebuilt first when
    /// the auth generation moved since the last build.
    fn state(&self) -> Arc<ProviderState> {
        let generation = self.0.auth.generation();
        {
            let current = self.0.state.read().unwrap_or_else(PoisonError::into_inner);
            if current.0 >= generation {
                return current.1.clone();
            }
        }
        let mut current = self.0.state.write().unwrap_or_else(PoisonError::into_inner);
        // Another lookup may have rebuilt while this one waited.
        if current.0 >= generation {
            return current.1.clone();
        }
        match build_state(&self.0.catalog, &self.0.config, self.0.auth.as_ref()) {
            Ok(state) => *current = (generation, Arc::new(state)),
            Err(err) => {
                // Keep serving the previous state; retry on the next change.
                tracing::warn!(?err, "provider state rebuild failed");
                current.0 = generation;
            }
        }
        current.1.clone()
    }

    /// `getModel` (provider.ts:1872-1895) — the model JSON value.
    fn model_value(&self, provider_id: &str, model_id: &str) -> Result<Value, String> {
        self.model_value_in(&self.state(), provider_id, model_id)
    }

    fn model_value_in(
        &self,
        state: &ProviderState,
        provider_id: &str,
        model_id: &str,
    ) -> Result<Value, String> {
        let Some(entry) = state.providers.get(provider_id) else {
            let suggestions = provider_suggestions(state, provider_id);
            return Err(self.model_not_found(provider_id, model_id, suggestions));
        };
        let Some(model) = entry["models"].get(model_id) else {
            let mut suggestions = model_suggestions(Some(entry), model_id);
            if suggestions.is_empty() {
                suggestions = model_suggestions(state.database.get(provider_id), model_id);
            }
            return Err(self.model_not_found(provider_id, model_id, suggestions));
        };
        Ok(model.clone())
    }

    fn model_not_found(
        &self,
        provider_id: &str,
        model_id: &str,
        suggestions: Vec<String>,
    ) -> String {
        let hint = if suggestions.is_empty() {
            String::new()
        } else {
            format!(" Did you mean: {}?", suggestions.join(", "))
        };
        format!("Model not found: {provider_id}/{model_id}.{hint}")
    }

    /// `getLanguage` (provider.ts:1898-1929) — the merged
    /// `provider.options` ← `model.options` record with the `apiKey` and
    /// `baseURL` legs the SDK factory consumes.
    fn llm_options(provider: &Value, model: &Value) -> BTreeMap<String, Value> {
        let mut options = BTreeMap::new();
        for source in [provider, model] {
            if let Some(map) = source["options"].as_object() {
                for (key, value) in map {
                    options.insert(key.clone(), value.clone());
                }
            }
        }
        // Deviation from provider.ts:1781 (`options["apiKey"] === undefined
        // && provider.key`): an empty apiKey — e.g. an `{env:VAR}` config
        // substitution with the variable unset — also falls back to the
        // auth.json credential. TS would hand the empty string to the SDK
        // and fail the request with an opaque 401; an empty key can never
        // authenticate, so the stored credential wins.
        let api_key_empty = options
            .get("apiKey")
            .map(|value| value.as_str().unwrap_or_default().is_empty())
            .unwrap_or(true);
        if api_key_empty {
            if let Some(key) = provider["key"].as_str() {
                if !key.is_empty() {
                    options.insert("apiKey".to_string(), json!(key));
                }
            }
        }
        let url = options
            .get("baseURL")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(String::from)
            .or_else(|| {
                model["api"]["url"]
                    .as_str()
                    .filter(|url| !url.is_empty())
                    .map(String::from)
            });
        match url {
            Some(base) => {
                let base = interpolate_env(&base);
                options.insert("baseURL".to_string(), json!(base));
            }
            None => {
                options.remove("baseURL");
            }
        }
        options
    }

    fn resolve(&self, provider_id: &str, model_id: &str) -> Result<ResolvedModel, String> {
        let state = self.state();
        let model = self.model_value_in(&state, provider_id, model_id)?;
        let provider = state
            .providers
            .get(provider_id)
            .ok_or_else(|| self.model_not_found(provider_id, model_id, Vec::new()))?;
        Ok(ResolvedModel {
            llm: LlmModel {
                id: get_str(&model, "id"),
                provider_id: provider_id.to_string(),
                api_id: model["api"]["id"].as_str().unwrap_or_default().to_string(),
                api_npm: model["api"]["npm"].as_str().unwrap_or_default().to_string(),
                temperature_capable: model["capabilities"]["temperature"]
                    .as_bool()
                    .unwrap_or(false),
                headers: model["headers"]
                    .as_object()
                    .map(|headers| {
                        headers
                            .iter()
                            .filter_map(|(key, value)| {
                                value.as_str().map(|value| (key.clone(), value.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                options: RuntimeModels::llm_options(provider, &model),
                context_limit: model["limit"]["context"].as_f64().unwrap_or(0.0),
                output_limit: model["limit"]["output"].as_f64().unwrap_or(0.0),
                output_token_max: self.0.output_token_max,
            },
            cost: v1_cost(&model["cost"]),
            limits: ModelLimits {
                context: model["limit"]["context"].as_f64().unwrap_or(0.0),
                input: model["limit"]["input"].as_f64(),
                output: model["limit"]["output"].as_f64().unwrap_or(0.0),
            },
            output_token_max: self.0.output_token_max,
        })
    }

    /// `defaultModel` (provider.ts:2026-2041) — the `(providerID, modelID)`
    /// pair; `Err` carries the TS error message.
    fn default_model_ids(&self) -> Result<(String, String), String> {
        let inner = &self.0;
        let state = self.state();
        if let Some(model) = &inner.config_model {
            let (provider_id, model_id) = parse_model(model);
            return Ok((provider_id, model_id));
        }
        for entry in recent_models(&inner.state_dir) {
            let Some(provider) = state.providers.get(&entry.0) else {
                continue;
            };
            if provider["models"].get(&entry.1).is_none() {
                continue;
            }
            return Ok(entry);
        }
        let provider = state
            .providers
            .values()
            .find(|provider| {
                inner.config_provider_ids.is_empty()
                    || provider["id"].as_str().is_some_and(|id| {
                        inner
                            .config_provider_ids
                            .iter()
                            .any(|configured| configured == id)
                    })
            })
            .ok_or_else(|| "No providers are available".to_string())?;
        let provider_id = get_str(provider, "id");
        let models = provider["models"]
            .as_object()
            .map(|models| models.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let sorted = sort_model_ids(models);
        sorted
            .first()
            .map(|model_id| (provider_id.clone(), model_id.clone()))
            .ok_or_else(|| format!("No models are available for provider: {provider_id}"))
    }
}

/// The `<state>/model.json` `recent` list (provider.ts:2028-2038).
fn recent_models(state_dir: &std::path::Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(state_dir.join("model.json")) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    value["recent"]
        .as_array()
        .map(|recent| {
            recent
                .iter()
                .filter_map(|entry| {
                    let provider_id = entry["providerID"].as_str()?;
                    let model_id = entry["modelID"].as_str()?;
                    Some((provider_id.to_string(), model_id.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

impl ModelSource for RuntimeModels {
    fn get_model<'a>(
        &'a self,
        provider_id: &'a str,
        model_id: &'a str,
        _session_id: &'a str,
    ) -> BoxFuture<'a, Result<ResolvedModel, LoopError>> {
        Box::pin(async move {
            self.resolve(provider_id, model_id)
                .map_err(LoopError::Unknown)
        })
    }

    fn get_small_model<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Option<ResolvedModel>> {
        Box::pin(async move {
            if let Some(small) = &self.0.config_small_model {
                let (provider_id, model_id) = parse_model(small);
                return self.resolve(&provider_id, &model_id).ok();
            }
            // TODO: Remove these provider-specific assumptions once model
            // syncing reliably reports available deployments (provider.ts:1963-1966).
            if provider_id == "azure" || provider_id == "azure-cognitive-services" {
                return None;
            }
            let state = self.state();
            let provider = state.providers.get(provider_id)?;
            let priority: Vec<&str> = if provider_id.starts_with("opencode") {
                vec!["gpt-nano"]
            } else if provider_id == "github-copilot" {
                vec!["gpt-mini", "gemini-flash", "gpt-nano", "claude-haiku"]
            } else {
                vec!["gemini-flash", "gpt-nano", "claude-haiku", "glm-flash"]
            };
            let mut models: Vec<&Value> = provider["models"]
                .as_object()
                .map(|models| models.values().collect())
                .unwrap_or_default();
            models.sort_by(|a, b| {
                get_str(b, "release_date")
                    .cmp(&get_str(a, "release_date"))
                    .then_with(|| get_str(b, "id").cmp(&get_str(a, "id")))
            });
            for family in priority {
                let candidates = models
                    .iter()
                    .filter(|model| get_str(model, "family") == family)
                    .collect::<Vec<_>>();
                if provider_id == "amazon-bedrock" {
                    let prefixes = ["global.", "us.", "eu."];
                    if let Some(global) = candidates
                        .iter()
                        .find(|model| get_str(model, "id").starts_with("global."))
                    {
                        return self.resolve(provider_id, &get_str(global, "id")).ok();
                    }
                    if let Some(region) = provider["options"]["region"].as_str() {
                        let Some(prefix) = region.split('-').next() else {
                            continue;
                        };
                        if prefix == "us" || prefix == "eu" {
                            let regional = candidates.iter().find(|model| {
                                get_str(model, "id").starts_with(&format!("{prefix}."))
                            });
                            if let Some(regional) = regional {
                                return self.resolve(provider_id, &get_str(regional, "id")).ok();
                            }
                        }
                    }
                    let unprefixed = candidates.iter().find(|model| {
                        !prefixes
                            .iter()
                            .any(|prefix| get_str(model, "id").starts_with(prefix))
                    });
                    if let Some(unprefixed) = unprefixed {
                        return self.resolve(provider_id, &get_str(unprefixed, "id")).ok();
                    }
                    continue;
                }
                if let Some(model) = candidates.first() {
                    return self.resolve(provider_id, &get_str(model, "id")).ok();
                }
            }
            None
        })
    }
}

// ---------------------------------------------------------------------------
// InputModels — V2 ModelInfo (prompt_input::Models)
// ---------------------------------------------------------------------------

impl RuntimeModels {
    fn model_info(&self, provider_id: &str, model_id: &str) -> Result<ModelInfo, String> {
        let model = self.model_value(provider_id, model_id)?;
        Ok(v1_to_model_info(provider_id, &model))
    }
}

impl Models for RuntimeModels {
    fn get_model<'a>(
        &'a self,
        provider_id: &'a str,
        model_id: &'a str,
    ) -> BoxFuture<'a, Result<ModelInfo, alforria_core::CoreError>> {
        Box::pin(async move {
            self.model_info(provider_id, model_id)
                .map_err(alforria_core::CoreError::Storage)
        })
    }

    fn default_model(&self) -> BoxFuture<'static, Result<ModelInfo, alforria_core::CoreError>> {
        let this = self.clone();
        Box::pin(async move {
            let (provider_id, model_id) = this
                .default_model_ids()
                .map_err(alforria_core::CoreError::Storage)?;
            this.model_info(&provider_id, &model_id)
                .map_err(alforria_core::CoreError::Storage)
        })
    }
}

// ---------------------------------------------------------------------------
// ShareModels — `Provider.Service.getModel` for share sync items
// ---------------------------------------------------------------------------

impl alforria_core::share::ShareModels for RuntimeModels {
    fn get_model(&self, provider_id: &str, model_id: &str) -> Result<Value, String> {
        self.model_value(provider_id, model_id)
    }
}

// ---------------------------------------------------------------------------
// V1 state model → V2 ModelInfo
// ---------------------------------------------------------------------------

/// The V1 provider-state model → V2 `ModelInfo` conversion the
/// prompt-input seam consumes: `variants` keeps the M7.7 variant ids
/// (the agent-variant availability check), `cost` becomes the
/// `[base, ...tiers, over_200k]` list (models-dev.ts:14-49).
pub fn v1_to_model_info(provider_id: &str, model: &Value) -> ModelInfo {
    let variants = model["variants"]
        .as_object()
        .map(|variants| {
            variants
                .iter()
                .map(|(id, variant)| ModelVariant {
                    id: id.clone(),
                    headers: variant["headers"]
                        .as_object()
                        .map(|headers| {
                            headers
                                .iter()
                                .filter_map(|(key, value)| {
                                    value.as_str().map(|value| (key.clone(), value.to_string()))
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    body: variant
                        .as_object()
                        .map(|variant| {
                            variant
                                .iter()
                                .filter(|(key, _)| key.as_str() != "headers")
                                .map(|(key, value)| (key.clone(), value.clone()))
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    let npm = model["api"]["npm"].as_str().unwrap_or_default();
    let api = if npm.is_empty() {
        ModelApi::Native {
            id: get_str(model, "id"),
            url: model["api"]["url"].as_str().map(String::from),
            settings: Default::default(),
        }
    } else {
        ModelApi::Aisdk {
            id: get_str(model, "id"),
            package: npm.to_string(),
            url: model["api"]["url"].as_str().map(String::from),
            settings: None,
        }
    };
    ModelInfo {
        id: get_str(model, "id"),
        provider_id: provider_id.to_string(),
        family: model["family"].as_str().map(String::from),
        name: get_str(model, "name"),
        api,
        capabilities: ModelCapabilities {
            tools: model["capabilities"]["toolcall"].as_bool().unwrap_or(true),
            input: model["capabilities"]["input"]
                .as_object()
                .map(|input| input.keys().cloned().collect())
                .unwrap_or_default(),
            output: model["capabilities"]["output"]
                .as_object()
                .map(|output| output.keys().cloned().collect())
                .unwrap_or_default(),
        },
        request: ModelRequest {
            headers: model["headers"]
                .as_object()
                .map(|headers| {
                    headers
                        .iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            body: model["options"].as_object().cloned().unwrap_or_default(),
            variant: None,
        },
        variants,
        time: ModelTime {
            released: model["release_date"]
                .as_str()
                .and_then(parse_release_date)
                .unwrap_or(0.0),
        },
        cost: cost_list(&model["cost"]),
        status: match model["status"].as_str() {
            Some("alpha") => ModelStatus::Alpha,
            Some("beta") => ModelStatus::Beta,
            Some("deprecated") => ModelStatus::Deprecated,
            _ => ModelStatus::Active,
        },
        enabled: true,
        limit: ModelLimit {
            context: model["limit"]["context"].as_i64().unwrap_or(0),
            input: model["limit"]["input"].as_i64(),
            output: model["limit"]["output"].as_i64().unwrap_or(0),
        },
    }
}

/// `cost()` (provider.ts:1230-1252) — the V1 cost record built by
/// [`crate::provider::cost_value`], into the runtime's `ModelCost`.
fn v1_cost(cost: &Value) -> ModelCost {
    let cache = |value: &Value| CacheCost {
        read: value["read"].as_f64().unwrap_or(0.0),
        write: value["write"].as_f64().unwrap_or(0.0),
    };
    ModelCost {
        input: cost["input"].as_f64().unwrap_or(0.0),
        output: cost["output"].as_f64().unwrap_or(0.0),
        cache: cache(&cost["cache"]),
        tiers: cost["tiers"]
            .as_array()
            .map(|tiers| {
                tiers
                    .iter()
                    .map(|tier| CostTier {
                        input: tier["input"].as_f64().unwrap_or(0.0),
                        output: tier["output"].as_f64().unwrap_or(0.0),
                        cache: cache(&tier["cache"]),
                        kind: alforria_core::catalog::ContextTierType::Context,
                        size: tier["tier"].as_f64().unwrap_or(0.0),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        experimental_over_200k: cost["experimentalOver200K"]
            .as_object()
            .map(|over| Over200k {
                input: over["input"].as_f64().unwrap_or(0.0),
                output: over["output"].as_f64().unwrap_or(0.0),
                cache: cache(&over["cache"]),
            }),
    }
}

/// `cost()` (models-dev.ts:14-49) — the `[base, ...tiers, over_200k]`
/// list shape of `ModelV2Info.cost`.
fn cost_list(cost: &Value) -> Vec<SchemaModelCost> {
    let cache = |value: &Value| ModelCostCache {
        read: value["read"].as_f64().unwrap_or(0.0),
        write: value["write"].as_f64().unwrap_or(0.0),
    };
    let mut out = vec![SchemaModelCost {
        tier: None,
        input: cost["input"].as_f64().unwrap_or(0.0),
        output: cost["output"].as_f64().unwrap_or(0.0),
        cache: cache(&cost["cache"]),
    }];
    if let Some(tiers) = cost["tiers"].as_array() {
        for tier in tiers {
            out.push(SchemaModelCost {
                tier: Some(ModelCostTier {
                    type_: ModelCostTierType::Context,
                    size: tier["tier"].as_f64().unwrap_or(0.0) as i64,
                }),
                input: tier["input"].as_f64().unwrap_or(0.0),
                output: tier["output"].as_f64().unwrap_or(0.0),
                cache: cache(&tier["cache"]),
            });
        }
    }
    if let Some(over) = cost["experimentalOver200K"].as_object() {
        out.push(SchemaModelCost {
            tier: Some(ModelCostTier {
                type_: ModelCostTierType::Context,
                size: 200_000,
            }),
            input: over["input"].as_f64().unwrap_or(0.0),
            output: over["output"].as_f64().unwrap_or(0.0),
            cache: cache(&over["cache"]),
        });
    }
    out
}

/// `released` (models-dev.ts:5-8) — `Date.parse` milliseconds, 0 on
/// invalid input.
fn parse_release_date(date: &str) -> Option<f64> {
    let date = date.split('T').next()?;
    let mut parts = date.split('-');
    let year = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<i64>().ok()?;
    let day = parts.next()?.parse::<i64>().ok()?;
    // Days since the Unix epoch (UTC) — the `Date.parse` result for a
    // date-only string is midnight UTC.
    let days = days_from_civil(year, month, day)?;
    Some((days * 86_400) as f64 * 1000.0)
}

/// Howard Hinnant's `days_from_civil` — days between 1970-01-01 and the
/// given civil date.
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

// ---------------------------------------------------------------------------
// LlmStream — the alforria-llm route client
// ---------------------------------------------------------------------------

/// The `LlmRequestSender` seam: one route per `api_npm`:
///
/// * `@ai-sdk/anthropic` → anthropic-messages (`x-api-key`)
/// * `@ai-sdk/openai` → openai-responses (bearer)
/// * `@ai-sdk/google` → gemini (`x-goog-api-key`)
/// * everything else → openai-compatible-chat (bearer, `baseURL`)
#[derive(Default)]
pub struct RouteLlmSender;

impl RouteLlmSender {
    fn option(model: &LlmModel, key: &str) -> Option<String> {
        model
            .options
            .get(key)
            .and_then(Value::as_str)
            .map(String::from)
    }

    fn handle(model: &LlmModel) -> RouteHandle {
        use alforria_llm::protocols::anthropic_messages;
        use alforria_llm::route::auth::Auth;

        let authed = |mut handle: RouteHandle, auth: Option<Auth>| -> RouteHandle {
            if let Some(auth) = auth {
                handle.auth = auth;
            }
            handle
        };
        let mut handle = match model.api_npm.as_str() {
            "@ai-sdk/anthropic" => authed(
                anthropic_messages::route_handle(),
                Self::option(model, "apiKey")
                    .map(|key| Auth::headers(vec![("x-api-key".to_string(), key)])),
            ),
            "@ai-sdk/google" => authed(
                alforria_llm::protocols::gemini::route_handle(),
                Self::option(model, "apiKey")
                    .map(|key| Auth::headers(vec![("x-goog-api-key".to_string(), key)])),
            ),
            npm => {
                let handle = match npm {
                    "@ai-sdk/openai" => alforria_llm::protocols::openai_responses::route_handle(),
                    _ => alforria_llm::protocols::openai_compatible_chat::route_handle(),
                };
                authed(
                    handle,
                    Self::option(model, "apiKey")
                        .map(|key| Auth::bearer(alforria_llm::route::auth::value(key))),
                )
            }
        };
        if let Some(base_url) = Self::option(model, "baseURL") {
            handle.endpoint.base_url = Some(base_url);
        }
        if !model.headers.is_empty() {
            handle.defaults.headers = model.headers.clone().into_iter().collect();
        }
        handle
    }
}

impl RouteLlmSender {
    fn route_send(
        request: alforria_llm::schema::messages::LlmRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<
        'static,
        Result<alforria_core::session::llm::LlmEventStream, alforria_llm::LlmError>,
    > {
        Box::pin(async move {
            let handle = request.model.route.clone();
            match handle.protocol_id.as_str() {
                "anthropic-messages" => {
                    let route = Route {
                        handle,
                        protocol: Arc::new(
                            alforria_llm::protocols::anthropic_messages::AnthropicMessages,
                        ),
                        executor: Default::default(),
                    };
                    Ok(route.stream_with_halt(&request, cancel).await?)
                }
                "openai-chat" | "openai-compatible-chat" => {
                    let route = Route {
                        handle,
                        protocol: Arc::new(alforria_llm::protocols::openai_chat::OpenAiChat),
                        executor: Default::default(),
                    };
                    Ok(route.stream_with_halt(&request, cancel).await?)
                }
                "openai-responses" => {
                    let route = Route {
                        handle,
                        protocol: Arc::new(
                            alforria_llm::protocols::openai_responses::OpenAiResponses,
                        ),
                        executor: Default::default(),
                    };
                    Ok(route.stream_with_halt(&request, cancel).await?)
                }
                "gemini" => {
                    let route = Route {
                        handle,
                        protocol: Arc::new(alforria_llm::protocols::gemini::Gemini),
                        executor: Default::default(),
                    };
                    Ok(route.stream_with_halt(&request, cancel).await?)
                }
                "bedrock-converse" => {
                    let route = Route {
                        handle,
                        protocol: Arc::new(
                            alforria_llm::protocols::bedrock_converse::BedrockConverse,
                        ),
                        executor: Default::default(),
                    };
                    Ok(route.stream_with_halt(&request, cancel).await?)
                }
                other => Err(alforria_llm::LlmError::invalid(format!(
                    "Unknown route: {other}"
                ))),
            }
        })
    }
}

impl LlmRequestSender for RouteLlmSender {
    fn model_ref(&self, model: &LlmModel) -> ModelRef {
        ModelRef::new(
            model.api_id.clone(),
            model.provider_id.clone(),
            Arc::new(RouteLlmSender::handle(model)),
        )
    }

    fn send(
        &self,
        request: alforria_llm::schema::messages::LlmRequest,
    ) -> BoxFuture<
        'static,
        Result<alforria_core::session::llm::LlmEventStream, alforria_llm::LlmError>,
    > {
        Self::route_send(request, tokio_util::sync::CancellationToken::new())
    }

    fn send_with_cancel(
        &self,
        request: alforria_llm::schema::messages::LlmRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<
        'static,
        Result<alforria_core::session::llm::LlmEventStream, alforria_llm::LlmError>,
    > {
        Self::route_send(request, cancel)
    }
}

/// `${VAR}` interpolation over the env (provider.ts:1734-1741).
fn interpolate_env(url: &str) -> String {
    let mut out = String::new();
    let mut rest = url;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        if let Some(end) = rest.find('}') {
            let key = &rest[2..end];
            match std::env::var(key) {
                Ok(value) => out.push_str(&value),
                Err(_) => out.push_str(&rest[..end + 1]),
            }
            rest = &rest[end + 1..];
        } else {
            out.push_str(rest);
            return out;
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_model_splits_at_first_slash() {
        assert_eq!(parse_model("a/b/c"), ("a".to_string(), "b/c".to_string()));
        assert_eq!(parse_model("a"), ("a".to_string(), String::new()));
    }

    #[test]
    fn model_suggestions_split_query_fallback() {
        let provider = json!({"models": {
            "claude-sonnet-4": {},
            "gpt-5": {},
        }});
        assert_eq!(
            model_suggestions(Some(&provider), "claude sonnet 5"),
            vec!["claude-sonnet-4".to_string()],
        );
        assert!(model_suggestions(Some(&provider), "zzz").is_empty());
    }

    #[test]
    fn release_date_parse_is_epoch_millis() {
        assert_eq!(
            parse_release_date("2025-01-01").unwrap(),
            1_735_689_600_000.0
        );
        assert_eq!(parse_release_date("bogus"), None);
    }

    #[test]
    fn interpolate_env_replaces_vars() {
        std::env::set_var("OPencode_RUNTIME_TEST", "value");
        assert_eq!(
            interpolate_env("https://api.example.com/${OPencode_RUNTIME_TEST}/v1"),
            "https://api.example.com/value/v1"
        );
        assert_eq!(
            interpolate_env("https://api.example.com/${MISSING_OPencode_VAR}/v1"),
            "https://api.example.com/${MISSING_OPencode_VAR}/v1"
        );
        std::env::remove_var("OPencode_RUNTIME_TEST");
    }

    #[test]
    fn llm_options_api_key_fallback() {
        // Empty-string apiKey (an `{env:UNSET}` substitution) falls back to
        // the stored provider key; a present key always wins.
        let provider = json!({
            "key": "stored-key",
            "options": {"apiKey": "", "baseURL": "https://api.example"},
            "models": {},
        });
        let options = RuntimeModels::llm_options(&provider, &json!({}));
        assert_eq!(options["apiKey"], json!("stored-key"));

        let provider = json!({"key": "stored-key", "options": {"apiKey": "config-key"}});
        let options = RuntimeModels::llm_options(&provider, &json!({}));
        assert_eq!(options["apiKey"], json!("config-key"));

        // No options at all — the stored credential applies (provider.ts:1781).
        let provider = json!({"key": "stored-key"});
        let options = RuntimeModels::llm_options(&provider, &json!({}));
        assert_eq!(options["apiKey"], json!("stored-key"));
    }

    /// A built engine resolves models with credentials set after it was
    /// built — in-process (`PUT /auth`, a web sign-in) and out-of-process
    /// (a CLI login rewriting auth.json) — while a model resolved earlier
    /// keeps the key it was resolved with.
    #[tokio::test]
    async fn engine_models_pick_up_credentials_set_after_the_build() {
        use crate::engine::{build_engine, EngineInput, EngineRuntime, EngineSeams};
        use crate::state::FileAuthStore;

        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().join("repo");
        std::fs::create_dir_all(&worktree).unwrap();
        let paths = alforria_core::GlobalPaths {
            home: dir.path().join("home"),
            config: dir.path().join("config"),
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            state: dir.path().join("state"),
        };
        let catalog: alforria_core::catalog::Providers = serde_json::from_value(json!({
            "acme": {
                "id": "acme",
                "name": "Acme",
                "env": ["ALFORRIA_TEST_ACME_API_KEY_UNSET"],
                "npm": "@ai-sdk/openai-compatible",
                "api": "https://acme.invalid/v1",
                "models": {
                    "acme-1": {
                        "id": "acme-1",
                        "name": "Acme 1",
                        "release_date": "2026-01-01",
                        "limit": {"context": 1000, "output": 100},
                    },
                },
            },
        }))
        .unwrap();
        let auth_file = paths.data.join("auth.json");
        let auth = Arc::new(FileAuthStore::new(auth_file.clone()));
        let config: alforria_core::config::schema::Config =
            serde_json::from_value(json!({})).unwrap();
        let storage = Arc::new(alforria_core::Storage::open(dir.path().join("db.sqlite")).unwrap());
        let clock = Arc::new(alforria_core::catalog::SystemClock);
        let background = alforria_core::BackgroundJobService::new(clock.clone());
        let services = Arc::new(alforria_core::SessionServices::new(
            storage,
            background.clone(),
            clock,
            &alforria_core::AgentRegistryInput {
                config: config.clone(),
                skill_dirs: Vec::new(),
                reference_dirs: Vec::new(),
                worktree: worktree.clone(),
                data_dir: paths.data.clone(),
                tmp_dir: dir.path().to_path_buf(),
                home: paths.home.clone(),
            },
        ));
        let engine = build_engine(&EngineInput {
            services,
            background,
            config: Arc::new(config),
            config_dirs: Vec::new(),
            directory: worktree.clone(),
            worktree,
            paths,
            runtime: EngineRuntime {
                catalog: Arc::new(move || Ok(catalog.clone())),
                auth: auth.clone(),
            },
            seams: EngineSeams::default(),
        })
        .unwrap();
        let models = engine.models();
        let api_key = |resolved: &ResolvedModel| resolved.llm.options.get("apiKey").cloned();

        // No credential yet: the provider isn't connected.
        assert!(models.get_model("acme", "acme-1", "ses").await.is_err());

        // In-process set (`PUT /auth`, the web sign-in sink).
        auth.set("acme", json!({"type": "api", "key": "sk-first"}))
            .unwrap();
        let first = models.get_model("acme", "acme-1", "ses").await.unwrap();
        assert_eq!(api_key(&first), Some(json!("sk-first")));

        // Out-of-process write (a CLI login), noticed through auth.json.
        std::fs::write(
            &auth_file,
            json!({"acme": {"type": "api", "key": "sk-second-key"}}).to_string(),
        )
        .unwrap();
        let second = models.get_model("acme", "acme-1", "ses").await.unwrap();
        assert_eq!(api_key(&second), Some(json!("sk-second-key")));
        // The model resolved before the change is untouched.
        assert_eq!(api_key(&first), Some(json!("sk-first")));

        // Removal disconnects the provider again.
        auth.remove("acme").unwrap();
        assert!(models.get_model("acme", "acme-1", "ses").await.is_err());
    }
}
