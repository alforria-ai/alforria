//! v2 integration routes — port of
//! `packages/protocol/src/groups/integration.ts` +
//! `packages/server/src/handlers/integration.ts`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::Path;
use axum::response::Response;
use axum::routing::{delete, get, post};

use crate::error::{ApiError, ServerError};
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

use super::util::{defect, envelope, no_content, parse_payload};

type Router = axum::Router<Arc<ServerContext>>;

pub fn register(router: Router, method: &str, path: &'static str) -> (Router, bool) {
    let router = match (method, path) {
        ("GET", "/api/integration") => router.route(path, get(list)),
        ("GET", "/api/integration/{integrationID}") => router.route(path, get(one)),
        ("POST", "/api/integration/{integrationID}/connect/key") => {
            router.route(path, post(connect_key))
        }
        ("POST", "/api/integration/{integrationID}/connect/oauth") => {
            router.route(path, post(connect_oauth))
        }
        ("GET", "/api/integration/attempt/{attemptID}") => router.route(path, get(attempt_status)),
        ("DELETE", "/api/integration/attempt/{attemptID}") => {
            router.route(path, delete(attempt_cancel))
        }
        ("POST", "/api/integration/attempt/{attemptID}/complete") => {
            router.route(path, post(attempt_complete))
        }
        _ => return (router, false),
    };
    (router, true)
}

/// `authorize` (`handlers/integration.ts:8-17`) — every
/// `AuthorizationError` maps to the same `InvalidRequestError` shape.
fn authorization_failed() -> ServerError {
    ApiError::InvalidRequest {
        message: "Authentication failed".to_string(),
        kind: Some("integration_authorization".to_string()),
        field: None,
    }
    .into()
}

/// `integration.list` (`handlers/integration.ts:21-28`).
async fn list(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let integrations = location.services.integrations.list();
    envelope(&location, integrations)
}

/// `integration.get` (`handlers/integration.ts:30-37`).
async fn one(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(integration_id): Path<String>,
) -> Result<Response, ServerError> {
    let integration = location.services.integrations.get(&integration_id);
    envelope(&location, integration)
}

/// `integration.connect.key` (`handlers/integration.ts:39-52`).
async fn connect_key(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(integration_id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, ServerError> {
    let payload: ConnectKeyPayload = parse_payload(&body)?;
    location
        .services
        .integrations
        .connection_key(&integration_id, &payload.key, payload.label.as_deref())
        .map_err(|_| authorization_failed())?;
    Ok(no_content())
}

#[derive(serde::Deserialize)]
struct ConnectKeyPayload {
    key: String,
    #[serde(default)]
    label: Option<String>,
}

/// `integration.connect.oauth` (`handlers/integration.ts:54-69`).
async fn connect_oauth(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(integration_id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, ServerError> {
    let payload: ConnectOauthPayload = parse_payload(&body)?;
    let inputs: HashMap<String, String> = payload.inputs.clone();
    let attempt = location
        .services
        .integrations
        .connection_oauth(
            &integration_id,
            &payload.method_id,
            &inputs,
            payload.label.as_deref(),
        )
        .map_err(|_| authorization_failed())?;
    envelope(&location, attempt)
}

#[derive(serde::Deserialize)]
struct ConnectOauthPayload {
    method_id: String,
    #[serde(default)]
    inputs: HashMap<String, String>,
    #[serde(default)]
    label: Option<String>,
}

/// `integration.attempt.status` (`handlers/integration.ts:71-77`).
async fn attempt_status(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(attempt_id): Path<String>,
) -> Result<Response, ServerError> {
    let status = location
        .services
        .integrations
        .attempt_status(&attempt_id)
        .map_err(defect)?;
    envelope(&location, status)
}

/// `integration.attempt.complete` (`handlers/integration.ts:79-101`).
async fn attempt_complete(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(attempt_id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, ServerError> {
    let payload: CompletePayload = parse_payload(&body)?;
    let result = location
        .services
        .integrations
        .attempt_complete(&attempt_id, payload.code.as_deref())
        .await
        .map_err(defect)?;
    if result.is_err() {
        // `Integration.CodeRequired` → 400 with the code-required kind.
        return Err(ApiError::InvalidRequest {
            message: "Authorization code is required".to_string(),
            kind: Some("integration_code_required".to_string()),
            field: None,
        }
        .into());
    }
    Ok(no_content())
}

#[derive(serde::Deserialize)]
struct CompletePayload {
    #[serde(default)]
    code: Option<String>,
}

/// `integration.attempt.cancel` (`handlers/integration.ts:103-108`).
async fn attempt_cancel(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(attempt_id): Path<String>,
) -> Result<Response, ServerError> {
    location.services.integrations.attempt_cancel(&attempt_id);
    Ok(no_content())
}
