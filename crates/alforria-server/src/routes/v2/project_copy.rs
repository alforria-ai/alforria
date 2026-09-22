//! v2 project-copy routes — port of
//! `packages/protocol/src/groups/project-copy.ts` +
//! `packages/server/src/handlers/project-copy.ts`. The group root is the
//! un-prefixed `/experimental/project/:projectID/copy`.

use std::sync::Arc;

use axum::extract::Path;
use axum::response::Response;
use axum::routing::{delete, post};

use crate::error::{ApiError, ServerError};
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

use super::util::{defect, json_ok, no_content, parse_payload};
use alforria_core::project_copy::{CopyError, CreateInput, RemoveInput};

type Router = axum::Router<Arc<ServerContext>>;

pub fn register(router: Router, method: &str, path: &'static str) -> (Router, bool) {
    let router = match (method, path) {
        ("POST", "/experimental/project/{projectID}/copy") => router.route(path, post(create)),
        ("DELETE", "/experimental/project/{projectID}/copy") => router.route(path, delete(remove)),
        ("POST", "/experimental/project/{projectID}/copy/refresh") => {
            router.route(path, post(refresh))
        }
        _ => return (router, false),
    };
    (router, true)
}

/// `badRequest` (`handlers/project-copy.ts:59-77`) — every copy error maps
/// onto the 400 `ProjectCopyError` wire shape.
fn bad_request(err: CopyError) -> ServerError {
    ApiError::ProjectCopy {
        message: err.to_string(),
        force_required: err.force_required(),
    }
    .into()
}

/// `projectCopy.create` (`handlers/project-copy.ts:10-21`) — the source is
/// the location's project directory.
async fn create(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(project_id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, ServerError> {
    let payload: CreatePayload = parse_payload(&body)?;
    let context = location
        .services
        .instance(&location.directory)
        .map_err(defect)?;
    let source_directory = context.worktree.to_string_lossy().into_owned();
    let copy = location
        .services
        .project_copy
        .create(
            &project_id,
            &CreateInput {
                strategy: &payload.strategy,
                source_directory: &source_directory,
                directory: &payload.directory,
                name: payload.name.as_deref(),
            },
        )
        .map_err(bad_request)?;
    Ok(json_ok(copy))
}

#[derive(serde::Deserialize)]
struct CreatePayload {
    strategy: String,
    directory: String,
    #[serde(default)]
    name: Option<String>,
}

/// `projectCopy.remove` (`handlers/project-copy.ts:23-32`).
async fn remove(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(project_id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, ServerError> {
    let payload: RemovePayload = parse_payload(&body)?;
    location
        .services
        .project_copy
        .remove(
            &project_id,
            &RemoveInput {
                directory: &payload.directory,
                force: payload.force,
            },
        )
        .map_err(bad_request)?;
    Ok(no_content())
}

#[derive(serde::Deserialize)]
struct RemovePayload {
    directory: String,
    force: bool,
}

/// `projectCopy.refresh` (`handlers/project-copy.ts:34-43`).
async fn refresh(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(project_id): Path<String>,
) -> Result<Response, ServerError> {
    location
        .services
        .project_copy
        .refresh(&project_id)
        .map_err(bad_request)?;
    Ok(no_content())
}
