//! PTY HTTP routes — port of `httpapi/handlers/pty.ts` (v1, the
//! experimental HttpApi surface) and `packages/server/src/handlers/pty.ts`
//! (v2, `/api`), including the shared connect-token check. The connect
//! WebSocket handlers live in [`crate::pty::ws`].

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::Router;
use serde_json::Value;

use alforria_schema::pty::{PtyCreateInput, PtyInfo, PtyStatus, PtyUpdateInput};

use crate::error::{ApiError, ServerError};
use crate::middleware::cors::request_origin_allowed;
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

use crate::pty::ticket::Scope;
use crate::pty::ws;

/// `PTY_CONNECT_TOKEN_HEADER` (`groups/pty.ts:10`).
pub const PTY_CONNECT_TOKEN_HEADER: &str = "x-opencode-ticket";
/// `PTY_CONNECT_TOKEN_HEADER_VALUE` (`groups/pty.ts:11`).
pub const PTY_CONNECT_TOKEN_HEADER_VALUE: &str = "1";

/// Route registration for the v1 + v2 PTY families.
pub fn register(
    router: Router<Arc<ServerContext>>,
    method: &str,
    path: &'static str,
) -> (Router<Arc<ServerContext>>, bool) {
    let router = match (method, path) {
        // ---- v1 (`httpapi/handlers/pty.ts`) ----
        ("GET", "/pty/shells") => router.route(path, get(shells)),
        ("GET", "/pty") => router.route(path, get(list)),
        ("POST", "/pty") => router.route(path, post(create)),
        ("GET", "/pty/{ptyID}") => router.route(path, get(get_session)),
        ("PUT", "/pty/{ptyID}") => router.route(path, put(update)),
        ("DELETE", "/pty/{ptyID}") => router.route(path, delete(remove)),
        ("POST", "/pty/{ptyID}/connect-token") => router.route(path, post(connect_token)),
        ("GET", "/pty/{ptyID}/connect") => router.route(path, get(ws::connect_v1)),
        // ---- v2 (`packages/server/src/handlers/pty.ts`) ----
        ("GET", "/api/pty") => router.route(path, get(list_v2)),
        ("POST", "/api/pty") => router.route(path, post(create_v2)),
        ("GET", "/api/pty/{ptyID}") => router.route(path, get(get_session_v2)),
        ("PUT", "/api/pty/{ptyID}") => router.route(path, put(update_v2)),
        ("DELETE", "/api/pty/{ptyID}") => router.route(path, delete(remove_v2)),
        ("POST", "/api/pty/{ptyID}/connect-token") => router.route(path, post(connect_token_v2)),
        ("GET", "/api/pty/{ptyID}/connect") => router.route(path, get(ws::connect_v2)),
        _ => return (router, false),
    };
    (router, true)
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

fn json_ok(value: impl serde::Serialize) -> Response {
    let body = serde_json::to_string(&value).expect("serialization cannot fail");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("static response parts are valid")
}

/// 200 with a `Location.response` envelope
/// (`packages/server/src/location.ts:15-27`).
fn envelope(
    location: &LocationContext,
    data: &impl serde::Serialize,
) -> Result<Response, ServerError> {
    let defect = |err: alforria_core::SessionError| {
        ServerError::Core(alforria_core::CoreError::Storage(err.to_string()))
    };
    let context = location
        .services
        .instance(&location.directory)
        .map_err(defect)?;
    let body = serde_json::json!({
        "location": {
            "directory": location.directory.display().to_string(),
            "workspaceID": location.workspace_id,
            "project": {
                "id": context.project.id,
                "directory": context.worktree.display().to_string(),
            },
        },
        "data": serde_json::to_value(data).map_err(|err| {
            ServerError::Core(alforria_core::CoreError::Storage(err.to_string()))
        })?,
    });
    let body = serde_json::to_string(&body)
        .map_err(|err| ServerError::Core(alforria_core::CoreError::Storage(err.to_string())))?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("static response parts are valid"))
}

pub(crate) fn require_pty_id_v1(pty_id: &str) -> Result<(), ServerError> {
    if pty_id.starts_with("pty_") {
        return Ok(());
    }
    Err(ApiError::bad_request_schema(
        format!("Expected a string starting with \\\"pty\\\", got {pty_id:?}"),
        "Params",
    )
    .into())
}

pub(crate) fn require_pty_id_v2(pty_id: &str) -> Result<(), ServerError> {
    if pty_id.starts_with("pty_") {
        return Ok(());
    }
    Err(ApiError::InvalidRequest {
        message: format!("Expected a string starting with \\\"pty\\\", got {pty_id:?}"),
        kind: None,
        field: None,
    }
    .into())
}

fn pty_not_found(pty_id: &str) -> ServerError {
    ApiError::PtyNotFound {
        pty_id: pty_id.to_string(),
        message: format!("PTY session not found: {pty_id}"),
    }
    .into()
}

fn payload_error(message: impl Into<String>) -> ServerError {
    ServerError::from(ApiError::bad_request_schema(message, "Payload"))
}

fn parse_payload(body: &Bytes) -> Result<Value, ServerError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|err| payload_error(format!("Invalid JSON payload: {err}")))?;
    Ok(value)
}

fn parse_payload_v2<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, ServerError> {
    let value: Value = serde_json::from_slice(body).map_err(|err| {
        ServerError::from(ApiError::InvalidRequest {
            message: format!("Invalid JSON payload: {err}"),
            kind: Some("Payload".to_string()),
            field: None,
        })
    })?;
    serde_json::from_value(value).map_err(|err| {
        ServerError::from(ApiError::InvalidRequest {
            message: err.to_string(),
            kind: Some("Payload".to_string()),
            field: None,
        })
    })
}

/// The `PtyForbiddenError` (v1) / `ForbiddenError` (v2) check shared by both
/// connect-token handlers (`httpapi/handlers/pty.ts:146-147`,
/// `packages/server/src/handlers/pty.ts:120-125`). The custom header forces
/// a CORS preflight, so cross-origin browser pages cannot mint tickets
/// without passing the server's origin policy.
fn ticket_request_allowed(headers: &HeaderMap, cors: &[String]) -> bool {
    let has_header = headers
        .get(PTY_CONNECT_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == PTY_CONNECT_TOKEN_HEADER_VALUE);
    has_header && request_origin_allowed(headers, cors)
}

pub(crate) fn ticket_scope(pty_id: &str, location: &LocationContext) -> Scope {
    Scope {
        pty_id: pty_id.to_string(),
        directory: Some(location.directory.display().to_string()),
        workspace_id: location.workspace_id.clone(),
    }
}

/// v1 `get` — the running check is folded into the not-found mapping
/// (`httpapi/handlers/pty.ts:84-102`).
fn get_running(
    ctx: &Arc<ServerContext>,
    location: &LocationContext,
    pty_id: &str,
) -> Result<PtyInfo, ServerError> {
    require_pty_id_v1(pty_id)?;
    ctx.ptys
        .resolve(location)
        .get(pty_id)
        .ok()
        .filter(|info| matches!(info.status, PtyStatus::Running))
        .ok_or_else(|| pty_not_found(pty_id))
}

// ---------------------------------------------------------------------------
// v1 handlers (`httpapi/handlers/pty.ts`)
// ---------------------------------------------------------------------------

/// `shells` (`:60-62`).
async fn shells() -> Response {
    json_ok(crate::pty::shells())
}

/// `list` — v1 hides exited sessions (`:64-67`).
async fn list(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Response {
    let sessions: Vec<PtyInfo> = ctx
        .ptys
        .resolve(&location)
        .list()
        .into_iter()
        .filter(|info| matches!(info.status, PtyStatus::Running))
        .collect();
    json_ok(sessions)
}

/// `create` (`:69-82`).
async fn create(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: PtyCreateInput = serde_json::from_value(parse_payload(&body)?)
        .map_err(|err| payload_error(err.to_string()))?;
    let cwd = payload
        .cwd
        .clone()
        .filter(|cwd| !cwd.is_empty())
        .unwrap_or_else(|| location.directory.display().to_string());
    let info = ctx.ptys.resolve(&location).create(&PtyCreateInput {
        cwd: Some(cwd),
        ..payload
    })?;
    Ok(json_ok(info))
}

async fn get_session(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    axum::extract::Path(pty_id): axum::extract::Path<String>,
) -> Result<Response, ServerError> {
    Ok(json_ok(get_running(&ctx, &location, &pty_id)?))
}

/// `update` (`:105-127`).
async fn update(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    axum::extract::Path(pty_id): axum::extract::Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    get_running(&ctx, &location, &pty_id)?;
    let payload: PtyUpdateInput = serde_json::from_value(parse_payload(&body)?)
        .map_err(|err| payload_error(err.to_string()))?;
    let info = ctx
        .ptys
        .resolve(&location)
        .update(&pty_id, &payload)
        .map_err(|_| pty_not_found(&pty_id))?;
    Ok(json_ok(info))
}

/// `remove` (`:129-142`).
async fn remove(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    axum::extract::Path(pty_id): axum::extract::Path<String>,
) -> Result<Response, ServerError> {
    get_running(&ctx, &location, &pty_id)?;
    ctx.ptys
        .resolve(&location)
        .remove(&pty_id)
        .map_err(|_| pty_not_found(&pty_id))?;
    Ok(json_ok(true))
}

/// `connectToken` (`:144-150`).
async fn connect_token(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    headers: HeaderMap,
    axum::extract::Path(pty_id): axum::extract::Path<String>,
) -> Result<Response, ServerError> {
    require_pty_id_v1(&pty_id)?;
    if !ticket_request_allowed(&headers, &ctx.cors) {
        return Err(ApiError::PtyForbidden {
            message: "Invalid PTY connect token request".to_string(),
        }
        .into());
    }
    get_running(&ctx, &location, &pty_id)?;
    Ok(json_ok(
        ctx.pty_tickets.issue(ticket_scope(&pty_id, &location)),
    ))
}

// ---------------------------------------------------------------------------
// v2 handlers (`packages/server/src/handlers/pty.ts`)
// ---------------------------------------------------------------------------

/// `pty.list` (`:33-35`) — the canonical surface keeps exited sessions.
async fn list_v2(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let sessions = ctx.ptys.resolve(&location).list();
    envelope(&location, &sessions)
}

/// `pty.create` (`:38-55`).
async fn create_v2(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: PtyCreateInput = parse_payload_v2(&body)?;
    let cwd = payload
        .cwd
        .clone()
        .filter(|cwd| !cwd.is_empty())
        .unwrap_or_else(|| location.directory.display().to_string());
    let info = ctx.ptys.resolve(&location).create(&PtyCreateInput {
        cwd: Some(cwd),
        ..payload
    })?;
    envelope(&location, &info)
}

async fn get_session_v2(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    axum::extract::Path(pty_id): axum::extract::Path<String>,
) -> Result<Response, ServerError> {
    require_pty_id_v2(&pty_id)?;
    let info = ctx
        .ptys
        .resolve(&location)
        .get(&pty_id)
        .map_err(|_| pty_not_found(&pty_id))?;
    envelope(&location, &info)
}

async fn update_v2(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    axum::extract::Path(pty_id): axum::extract::Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    require_pty_id_v2(&pty_id)?;
    let payload: PtyUpdateInput = parse_payload_v2(&body)?;
    let info = ctx
        .ptys
        .resolve(&location)
        .update(&pty_id, &payload)
        .map_err(|_| pty_not_found(&pty_id))?;
    envelope(&location, &info)
}

async fn remove_v2(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    axum::extract::Path(pty_id): axum::extract::Path<String>,
) -> Result<Response, ServerError> {
    require_pty_id_v2(&pty_id)?;
    ctx.ptys
        .resolve(&location)
        .remove(&pty_id)
        .map_err(|_| pty_not_found(&pty_id))?;
    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("static response parts are valid"))
}

async fn connect_token_v2(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    headers: HeaderMap,
    axum::extract::Path(pty_id): axum::extract::Path<String>,
) -> Result<Response, ServerError> {
    require_pty_id_v2(&pty_id)?;
    if !ticket_request_allowed(&headers, &ctx.cors) {
        return Err(ApiError::Forbidden {
            message: "Invalid PTY connect token request".to_string(),
        }
        .into());
    }
    ctx.ptys
        .resolve(&location)
        .get(&pty_id)
        .map_err(|_| pty_not_found(&pty_id))?;
    let token = ctx.pty_tickets.issue(ticket_scope(&pty_id, &location));
    envelope(&location, &token)
}
