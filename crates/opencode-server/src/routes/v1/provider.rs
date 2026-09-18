//! v1 provider route family (M6.6) — port of `httpapi/handlers/provider.ts`
//! over the M3 models-dev catalog, the config enabled/disabled filters and
//! the `ProviderAuth` seam.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{json, Value};

use opencode_core::catalog::{self, Cost, Interleaved, Model, Provider as CatalogProvider};
use opencode_core::config::schema::Config;

use crate::error::{ApiError, ServerError};
use crate::middleware::location::LocationContext;
use crate::routes::v1::util::*;
use crate::state::ServerContext;

// ---------------------------------------------------------------------------
// models-dev → provider.Info mapping (provider/provider.ts:1265-1333)
// ---------------------------------------------------------------------------

/// `cloudflareGatewayNpm` (provider.ts:1249-1256).
fn cloudflare_gateway_npm(provider_id: &str, model_id: &str) -> Option<&'static str> {
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

/// `cost` (provider.ts:1221-1248).
fn cost_value(c: Option<&Cost>) -> Value {
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

/// `fromModelsDevModel` (provider.ts:1265-1320) — variants land empty
/// (TODO(M7): `ProviderTransform.variants`).
fn from_models_dev_model(provider: &CatalogProvider, model: &Model) -> Value {
    let capabilities = |model: &Model| {
        json!({
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
        })
    };
    let base = |model: &Model| {
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
            "capabilities": capabilities(model),
            "release_date": model.release_date,
            "variants": {},
        });
        if let Some(family) = &model.family {
            value["family"] = json!(family);
        }
        if let Some(input) = model.limit.input {
            value["limit"]["input"] = json!(input);
        }
        value
    };
    base(model)
}

/// `modeOptions` (provider.ts:1355-1367) — snake_case body keys become
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

/// `fromModelsDevProvider` (provider.ts:1322-1352) including the
/// experimental mode expansion. Variants land empty
/// (TODO(M7): `ProviderTransform.variants`).
fn from_models_dev_provider(provider: &CatalogProvider) -> Value {
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
// default model ids (`Provider.defaultModelIDs`, provider.ts:1137-1139)
// ---------------------------------------------------------------------------

/// `sort` (provider.ts:2047-2056) — priority index desc, `latest` first,
/// id desc; stable.
fn sort_model_ids(ids: &mut [String]) {
    const PRIORITY: [&str; 4] = ["gpt-5", "claude-sonnet-4", "big-pickle", "gemini-3-pro"];
    let priority = |id: &str| -> i64 {
        PRIORITY
            .iter()
            .position(|f| id.contains(f))
            .map(|i| i as i64)
            .unwrap_or(-1)
    };
    ids.sort_by(|a, b| {
        priority(b)
            .cmp(&priority(a))
            .then_with(|| (!a.contains("latest")).cmp(&!b.contains("latest")))
            .then_with(|| b.cmp(a))
    });
}

/// `defaultModelIDs` (provider.ts:1137-1139): first model of the sorted list.
pub fn default_model_ids(providers: &BTreeMap<String, Value>) -> Value {
    let mut out = serde_json::Map::new();
    for (id, provider) in providers {
        let Some(models) = provider["models"].as_object() else {
            continue;
        };
        let mut ids: Vec<String> = models.keys().cloned().collect();
        sort_model_ids(&mut ids);
        if let Some(first) = ids.first() {
            out.insert(id.clone(), json!(first));
        }
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// provider sets (`handlers/provider.ts:59-89`)
// ---------------------------------------------------------------------------

/// Credential check against the env-var names and the auth store — the
/// "env"/"api keys" legs of `Provider.Service` state init
/// (provider.ts:1581-1599).
fn has_credential(ctx: &ServerContext, provider: &CatalogProvider) -> bool {
    if ctx.auth_store.has(&provider.id) {
        return true;
    }
    provider
        .env
        .iter()
        .any(|var| std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false))
}

/// The catalog providers filtered by the config enabled/disabled lists
/// (`handlers/provider.ts:63-70`), mapped through `fromModelsDevProvider`.
fn filtered_catalog(
    ctx: &ServerContext,
    config: &Config,
) -> Result<BTreeMap<String, Value>, ServerError> {
    let catalog = (ctx.catalog)().map_err(defect)?;
    let disabled: Vec<&str> = config
        .disabled_providers
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let enabled: Option<Vec<&str>> = config
        .enabled_providers
        .as_ref()
        .map(|ids| ids.iter().map(String::as_str).collect());
    let mut out = BTreeMap::new();
    for (id, provider) in &catalog {
        if let Some(enabled) = &enabled {
            if !enabled.contains(&id.as_str()) {
                continue;
            }
        }
        if disabled.contains(&id.as_str()) {
            continue;
        }
        out.insert(id.clone(), from_models_dev_provider(provider));
    }
    Ok(out)
}

/// `Provider.Service.list()` — the connected (authenticated) providers
/// (provider.ts:1732). TODO(M7): `source`/`key` merging per auth type.
fn connected_providers(
    ctx: &ServerContext,
    config: &Config,
) -> Result<BTreeMap<String, Value>, ServerError> {
    let catalog = (ctx.catalog)().map_err(defect)?;
    let disabled: Vec<&str> = config
        .disabled_providers
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let mut out = BTreeMap::new();
    for (id, provider) in &catalog {
        if disabled.contains(&id.as_str()) {
            continue;
        }
        if has_credential(ctx, provider) {
            out.insert(id.clone(), from_models_dev_provider(provider));
        }
    }
    Ok(out)
}

/// `list` result (`handlers/provider.ts:59-89`) — `{all, default,
/// connected}`.
pub fn list_result(ctx: &ServerContext, config: &Config) -> Result<serde_json::Value, ServerError> {
    let filtered = filtered_catalog(ctx, config)?;
    let connected = connected_providers(ctx, config)?;
    let mut providers = filtered;
    for (id, value) in &connected {
        providers.insert(id.clone(), value.clone());
    }
    let connected_ids: Vec<String> = providers
        .keys()
        .filter(|id| connected.contains_key(*id) || ctx.auth_store.has(id.as_str()))
        .cloned()
        .collect();
    let all: Vec<&Value> = providers.values().collect();
    Ok(json!({
        "all": all,
        "default": default_model_ids(&providers),
        "connected": connected_ids,
    }))
}

/// `config.providers` result (`handlers/config.ts:33-39`) — the connected
/// providers only.
pub fn config_providers_result(
    ctx: &ServerContext,
    config: &Config,
) -> Result<serde_json::Value, ServerError> {
    let providers = connected_providers(ctx, config)?;
    let all: Vec<&Value> = providers.values().collect();
    Ok(json!({
        "providers": all,
        "default": default_model_ids(&providers),
    }))
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// `list` (`handlers/provider.ts:59-89`).
pub async fn list(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let config = load_config(&location.directory)?;
    Ok(json_ok(list_result(&ctx, &config)?))
}

/// `auth` (`handlers/provider.ts:90-92`) — `ProviderAuth.Service.methods()`.
pub async fn auth(State(ctx): State<Arc<ServerContext>>) -> Result<Response, ServerError> {
    let methods = ctx.provider_auth.methods()?;
    Ok(json_ok(methods))
}

#[derive(Deserialize)]
struct AuthorizePayload {
    method: i64,
    #[serde(default)]
    inputs: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct CallbackPayload {
    method: i64,
    #[serde(default)]
    code: Option<String>,
}

/// `mapProviderAuthError` fallback — the empty `BadRequest` shape
/// (`handlers/provider.ts:34-36`).
fn provider_auth_bad_request() -> ServerError {
    ServerError::from(ApiError::ProviderAuth {
        name: "BadRequest",
        provider_id: None,
        field: None,
        message: None,
    })
}

/// `authorize` (`handlers/provider.ts:112-128`) — raw body; invalid JSON is
/// the `BadRequest` provider-auth error, the result serializes as JSON
/// `null` when absent.
pub async fn authorize(
    State(ctx): State<Arc<ServerContext>>,
    Path(provider_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: Value = serde_json::from_slice(&body).map_err(|_| provider_auth_bad_request())?;
    let parsed: AuthorizePayload =
        serde_json::from_value(payload).map_err(|_| provider_auth_bad_request())?;
    let result = ctx
        .provider_auth
        .authorize(&provider_id, parsed.method, parsed.inputs)
        .map_err(provider_auth_error)?;
    Ok(json_ok(serde_json::to_value(result).unwrap_or(Value::Null)))
}

/// `callback` (`handlers/provider.ts:130-139`).
pub async fn callback(
    State(ctx): State<Arc<ServerContext>>,
    Path(provider_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: CallbackPayload = parse_payload(&body)?;
    ctx.provider_auth
        .callback(&provider_id, payload.method, payload.code)
        .map_err(provider_auth_error)?;
    Ok(json_ok(true))
}

/// `mapProviderAuthError` (`handlers/provider.ts:20-36`).
fn provider_auth_error(err: crate::state::ProviderAuthError) -> ServerError {
    match err {
        crate::state::ProviderAuthError::OauthMissing { provider_id } => ApiError::ProviderAuth {
            name: "ProviderAuthOauthMissing",
            provider_id: Some(provider_id),
            field: None,
            message: None,
        }
        .into(),
        crate::state::ProviderAuthError::OauthCodeMissing { provider_id } => {
            ApiError::ProviderAuth {
                name: "ProviderAuthOauthCodeMissing",
                provider_id: Some(provider_id),
                field: None,
                message: None,
            }
            .into()
        }
        crate::state::ProviderAuthError::OauthCallbackFailed => ApiError::ProviderAuth {
            name: "ProviderAuthOauthCallbackFailed",
            provider_id: None,
            field: None,
            message: None,
        }
        .into(),
        crate::state::ProviderAuthError::ValidationFailed { field, message } => {
            ApiError::ProviderAuth {
                name: "ProviderAuthValidationFailed",
                provider_id: None,
                field: Some(field),
                message: Some(message),
            }
            .into()
        }
        crate::state::ProviderAuthError::BadRequest => provider_auth_bad_request(),
    }
}

pub fn register(
    router: axum::Router<Arc<ServerContext>>,
    method: &str,
    path: &'static str,
) -> (axum::Router<Arc<ServerContext>>, bool) {
    let router = match (method, path) {
        ("GET", "/provider") => router.route(path, get(list)),
        ("GET", "/provider/auth") => router.route(path, get(auth)),
        ("POST", "/provider/{providerID}/oauth/authorize") => router.route(path, post(authorize)),
        ("POST", "/provider/{providerID}/oauth/callback") => router.route(path, post(callback)),
        _ => return (router, false),
    };
    (router, true)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> CatalogProvider {
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

    #[test]
    fn from_models_dev_provider_shape() {
        let info = from_models_dev_provider(&provider());
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
    }

    #[test]
    fn sort_model_ids_matches_ts_priority() {
        let mut ids = vec![
            "gemini-flash".to_string(),
            "gpt-5-turbo".to_string(),
            "claude-sonnet-4.5".to_string(),
        ];
        sort_model_ids(&mut ids);
        assert_eq!(
            ids,
            vec![
                "claude-sonnet-4.5".to_string(),
                "gpt-5-turbo".to_string(),
                "gemini-flash".to_string(),
            ]
        );
    }

    #[test]
    fn default_model_ids_picks_first_sorted() {
        let provider = json!({"models": {
            "claude-haiku": {"id": "claude-haiku"},
            "z-latest": {"id": "z-latest"},
            "gemini-flash": {"id": "gemini-flash"},
        }});
        let mut providers = BTreeMap::new();
        providers.insert("anthropic".to_string(), provider);
        let ids = default_model_ids(&providers);
        assert_eq!(ids["anthropic"], "z-latest");
    }
}
