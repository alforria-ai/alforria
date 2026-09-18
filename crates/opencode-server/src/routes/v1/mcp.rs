//! The v1 `/mcp` family — port of `handlers/mcp.ts:11-105`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{delete, get, post};
use opencode_core::config::schema::McpInfo;
use opencode_core::mcp::{FinishAuthError, NotFoundError, StartAuthError};
use opencode_schema::mcp::McpStatus;
use serde_json::json;

use crate::error::ApiError;
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;
use crate::ServerError;

use super::util::{json_ok, parse_payload};

/// `MCP.NotFoundError` → 404 `McpServerNotFoundError`
/// (`handlers/mcp.ts:27-31` and friends).
fn not_found(err: NotFoundError) -> ApiError {
    ApiError::McpServerNotFound {
        name: err.name.clone(),
        message: format!("MCP server not found: {}", err.name),
    }
}

/// A plain `Error` throw in TS (`startAuth`, `finishAuth`) defects into the
/// 500 envelope.
fn defect(message: String) -> ServerError {
    ServerError::Core(opencode_core::CoreError::Storage(message))
}

fn start_auth_error(err: StartAuthError) -> ServerError {
    match err {
        StartAuthError::NotFound(name) => ApiError::McpServerNotFound {
            message: format!("MCP server not found: {name}"),
            name,
        }
        .into(),
        StartAuthError::Failed(message) => defect(message),
    }
}

fn finish_auth_error(err: FinishAuthError) -> ServerError {
    match err {
        FinishAuthError::NotFound(name) => ApiError::McpServerNotFound {
            message: format!("MCP server not found: {name}"),
            name,
        }
        .into(),
        FinishAuthError::Failed(message) => defect(message),
    }
}

/// `status` (`handlers/mcp.ts:15-17`).
async fn status(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let mcp = ctx.mcp.service(&location)?;
    Ok(json_ok(mcp.status().await))
}

#[derive(serde::Deserialize)]
struct AddPayload {
    name: String,
    config: McpInfo,
}

/// `add` (`handlers/mcp.ts:19-26`).
async fn add(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: AddPayload = parse_payload(&body)?;
    let mcp = ctx.mcp.service(&location)?;
    Ok(json_ok(mcp.add(&payload.name, payload.config).await))
}

/// The `supportsOAuth` gate shared by `authStart` and `authAuthenticate`
/// (`handlers/mcp.ts:29-35, 50-56`).
async fn require_oauth(
    mcp: &opencode_core::mcp::McpService,
    name: &str,
) -> Result<(), ServerError> {
    if !mcp.supports_oauth(name).await.map_err(not_found)? {
        return Err(ApiError::McpUnsupportedOAuth {
            error: format!("MCP server {name} does not support OAuth"),
        }
        .into());
    }
    Ok(())
}

/// `authStart` (`handlers/mcp.ts:29-43`).
async fn auth_start(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(name): Path<String>,
) -> Result<Response, ServerError> {
    let mcp = ctx.mcp.service(&location)?;
    require_oauth(&mcp, &name).await?;
    let (authorization_url, oauth_state) = mcp.start_auth(&name).await.map_err(start_auth_error)?;
    Ok(json_ok(json!({
        "authorizationUrl": authorization_url,
        "oauthState": oauth_state,
    })))
}

#[derive(serde::Deserialize)]
struct AuthCallbackPayload {
    code: String,
}

/// `authCallback` (`handlers/mcp.ts:45-59`).
async fn auth_callback(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: AuthCallbackPayload = parse_payload(&body)?;
    let mcp = ctx.mcp.service(&location)?;
    let status = mcp
        .finish_auth(&name, &payload.code)
        .await
        .map_err(finish_auth_error)?;
    Ok(json_ok(status))
}

/// `authAuthenticate` (`handlers/mcp.ts:61-72`).
async fn auth_authenticate(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(name): Path<String>,
) -> Result<Response, ServerError> {
    let mcp = ctx.mcp.service(&location)?;
    require_oauth(&mcp, &name).await?;
    let status = mcp.authenticate(&name).await.map_err(start_auth_error)?;
    Ok(json_ok(status))
}

/// `authRemove` (`handlers/mcp.ts:74-82`).
async fn auth_remove(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(name): Path<String>,
) -> Result<Response, ServerError> {
    let mcp = ctx.mcp.service(&location)?;
    let status: std::collections::BTreeMap<String, McpStatus> = mcp.status().await;
    if !status.contains_key(&name) {
        return Err(ApiError::McpServerNotFound {
            name: name.clone(),
            message: format!("MCP server not found: {name}"),
        }
        .into());
    }
    mcp.remove_auth(&name).await;
    Ok(json_ok(json!({ "success": true })))
}

/// `connect` (`handlers/mcp.ts:84-89`).
async fn connect(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(name): Path<String>,
) -> Result<Response, ServerError> {
    let mcp = ctx.mcp.service(&location)?;
    mcp.connect(&name).await.map_err(not_found)?;
    Ok(json_ok(true))
}

/// `disconnect` (`handlers/mcp.ts:91-105`).
async fn disconnect(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(name): Path<String>,
) -> Result<Response, ServerError> {
    let mcp = ctx.mcp.service(&location)?;
    mcp.disconnect(&name).await.map_err(not_found)?;
    Ok(json_ok(true))
}

pub fn register(
    router: axum::Router<Arc<ServerContext>>,
    method: &str,
    path: &'static str,
) -> (axum::Router<Arc<ServerContext>>, bool) {
    let router = match (method, path) {
        ("GET", "/mcp") => router.route(path, get(status)),
        ("POST", "/mcp") => router.route(path, post(add)),
        ("POST", "/mcp/{name}/auth") => router.route(path, post(auth_start)),
        ("DELETE", "/mcp/{name}/auth") => router.route(path, delete(auth_remove)),
        ("POST", "/mcp/{name}/auth/callback") => router.route(path, post(auth_callback)),
        ("POST", "/mcp/{name}/auth/authenticate") => router.route(path, post(auth_authenticate)),
        ("POST", "/mcp/{name}/connect") => router.route(path, post(connect)),
        ("POST", "/mcp/{name}/disconnect") => router.route(path, post(disconnect)),
        _ => return (router, false),
    };
    (router, true)
}
