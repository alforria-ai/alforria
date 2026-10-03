//! v1 provider route family (M6.6) — port of `httpapi/handlers/provider.ts`
//! over the M7.7 provider service (`crate::provider`): the models-dev
//! catalog with variants, the config enabled/disabled filters, connected
//! `source`/`key` merging and the `ProviderAuth` seam.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{json, Value};

use alforria_core::config::schema::Config;

use crate::error::{ApiError, ServerError};
use crate::middleware::location::LocationContext;
use crate::provider::{build_state, from_models_dev_provider, load_catalog};
use crate::routes::v1::util::*;
use crate::state::ServerContext;

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
// provider sets (`handlers/provider.ts:44-89`)
// ---------------------------------------------------------------------------

/// The catalog providers filtered by the config enabled/disabled lists
/// (`handlers/provider.ts:50-58`), mapped through `fromModelsDevProvider`.
fn filtered_catalog(
    ctx: &ServerContext,
    config: &Config,
) -> Result<BTreeMap<String, Value>, ServerError> {
    let catalog = load_catalog(&ctx.catalog)?;
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

/// The connected providers — the `Provider.Service` state
/// (`provider.list()`, provider.ts:1732) with the env/api `source`/`key`
/// merging (M7.7).
fn connected_providers(
    ctx: &ServerContext,
    config: &Config,
) -> Result<BTreeMap<String, Value>, ServerError> {
    let catalog = load_catalog(&ctx.catalog)?;
    let state = build_state(&catalog, config, ctx.auth_store.as_ref())?;
    Ok(state.providers)
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

/// `config.providers` result (`handlers/config.ts:27-33`) — the connected
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
        .authorize(
            ctx.auth_store.clone(),
            &provider_id,
            parsed.method,
            parsed.inputs,
        )
        .map_err(provider_auth_error)?;
    Ok(json_ok(result))
}

/// `callback` (`handlers/provider.ts:130-139`). The callback blocks until
/// the flow settles (a browser sign-in waits up to its loopback timeout),
/// so it runs on the blocking pool; an aborted request leaves the flow to
/// that timeout or to the next authorize.
pub async fn callback(
    State(ctx): State<Arc<ServerContext>>,
    Path(provider_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: CallbackPayload = parse_payload(&body)?;
    let service = ctx.provider_auth.clone();
    tokio::task::spawn_blocking(move || {
        service.callback(&provider_id, payload.method, payload.code)
    })
    .await
    .map_err(|_| crate::state::ProviderAuthError::Defect)
    .and_then(|result| result)
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
        crate::state::ProviderAuthError::Defect => ServerError::Core(
            alforria_core::CoreError::Storage("ProviderAuth.Service defect".to_string()),
        ),
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
