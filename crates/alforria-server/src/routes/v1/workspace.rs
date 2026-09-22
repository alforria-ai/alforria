//! v1 workspace routes — port of
//! `packages/opencode/src/server/routes/instance/httpapi/{groups,handlers}/workspace.ts`
//! over `crate::workspace::WorkspaceService`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as PathParam, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};

use crate::error::{ApiError, ServerError};
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;
use crate::workspace::{CreateInput, WarpError, WarpInput, WorkspaceService};

use super::util::{json_ok, parse_payload};

/// `HttpApiSchema.NoContent` — an empty 204 body.
fn no_content() -> Response {
    axum::http::StatusCode::NO_CONTENT.into_response()
}

type Router = axum::Router<Arc<ServerContext>>;

pub fn register(router: Router, method: &str, path: &'static str) -> (Router, bool) {
    let router = match (method, path) {
        ("GET", "/experimental/workspace/adapter") => router.route(path, get(adapters)),
        ("GET", "/experimental/workspace") => router.route(path, get(list)),
        ("POST", "/experimental/workspace") => router.route(path, post(create)),
        ("POST", "/experimental/workspace/sync-list") => router.route(path, post(sync_list)),
        ("GET", "/experimental/workspace/status") => router.route(path, get(status)),
        ("DELETE", "/experimental/workspace/{id}") => router.route(path, delete(remove)),
        ("POST", "/experimental/workspace/warp") => router.route(path, post(warp)),
        _ => return (router, false),
    };
    (router, true)
}

fn service(ctx: &Arc<ServerContext>, location: &LocationContext) -> WorkspaceService {
    WorkspaceService {
        directory: location.directory.clone(),
        services: location.services.clone(),
        worktree: ctx.worktree.clone(),
        deps: ctx.worktree_deps.clone(),
    }
}

/// `adapters` (`handlers/workspace.ts:11-15`).
async fn adapters(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    Ok(json_ok(service(&ctx, &location).adapters()))
}

/// `list` (`handlers/workspace.ts:17-19`).
async fn list(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    Ok(json_ok(service(&ctx, &location).list()?))
}

#[derive(serde::Deserialize)]
struct CreatePayload {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type")]
    type_: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    extra: Option<serde_json::Value>,
}

/// `create` (`handlers/workspace.ts:21-42`) — failures map onto the 400
/// `WorkspaceCreateError` wire shape with the die-reason extraction.
async fn create(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: CreatePayload = parse_payload(&body)?;
    let workspace = service(&ctx, &location)
        .create(&CreateInput {
            id: payload.id,
            r#type: payload.type_,
            branch: payload.branch,
            extra: payload.extra,
        })
        .map_err(|err| ApiError::WorkspaceCreate {
            message: err.to_string(),
        })?;
    Ok(json_ok(workspace))
}

/// `syncList` (`handlers/workspace.ts:44-46`) — 204 either way.
async fn sync_list(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    service(&ctx, &location).sync_list()?;
    Ok(no_content())
}

/// `status` (`handlers/workspace.ts:48-51`) — connection statuses filtered
/// to the listed workspace ids.
async fn status(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let service = service(&ctx, &location);
    let ids = service
        .list()?
        .iter()
        .filter_map(|workspace| workspace["id"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    let statuses = service
        .status()
        .into_iter()
        .filter(|status| {
            status["workspaceID"]
                .as_str()
                .is_some_and(|id| ids.iter().any(|listed| listed == id))
        })
        .collect::<Vec<_>>();
    Ok(json_ok(statuses))
}

/// `remove` (`handlers/workspace.ts:53-56`) — `undefined | Workspace.Info`.
async fn remove(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    PathParam(id): PathParam<String>,
) -> Result<Response, ServerError> {
    Ok(json_ok(service(&ctx, &location).remove(&id)?))
}

#[derive(serde::Deserialize)]
struct WarpPayload {
    id: Option<String>,
    #[serde(rename = "sessionID")]
    session_id: String,
    #[serde(default)]
    #[serde(rename = "copyChanges")]
    copy_changes: Option<bool>,
}

/// `warp` (`handlers/workspace.ts:58-82`) — `WorkspaceNotFoundError` → 404
/// `ApiNotFoundError`, `Vcs.PatchApplyError` → `ApiVcsApplyError`, else 400
/// `WorkspaceWarpError`.
async fn warp(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: WarpPayload = parse_payload(&body)?;
    match service(&ctx, &location).session_warp(&WarpInput {
        workspace_id: payload.id,
        session_id: payload.session_id,
        copy_changes: payload.copy_changes,
    }) {
        Ok(()) => Ok(no_content()),
        Err(WarpError::NotFound(err)) => Err(ApiError::NotFound {
            message: err.message,
        }
        .into()),
        Err(WarpError::Other(message)) => Err(ApiError::WorkspaceWarp { message }.into()),
    }
}
