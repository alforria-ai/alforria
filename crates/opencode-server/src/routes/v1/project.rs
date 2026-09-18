//! The v1 `/project` family — port of `handlers/project.ts:13-63`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, patch, post};
use opencode_core::project::registry::{Field, UpdateInput};
use opencode_core::{CoreError, SessionError};
use opencode_schema::project::{ProjectCommands, ProjectIcon};

use crate::error::{ApiError, ServerError};
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

use super::util::{json_ok, parse_payload};

fn defect(err: impl std::fmt::Display) -> ServerError {
    ServerError::Core(CoreError::Storage(err.to_string()))
}

fn session_defect(err: SessionError) -> ServerError {
    defect(err)
}

/// `Project.NotFoundError` → 404 `ProjectNotFoundError`
/// (`handlers/project.ts:36-50`).
fn project_not_found(project_id: &str) -> ApiError {
    ApiError::ProjectNotFound {
        project_id: project_id.to_string(),
        message: format!("Project not found: {project_id}"),
    }
}

/// `list` (`handlers/project.ts:15-17`).
async fn list(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(_location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let projects = ctx.projects.list().map_err(|err| defect(err.to_string()))?;
    Ok(json_ok(projects))
}

/// `current` (`handlers/project.ts:19-21`) — `InstanceState.context.project`.
async fn current(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let instance = location
        .services
        .instance(&location.directory)
        .map_err(session_defect)?;
    Ok(json_ok(instance.project))
}

/// `initGit` (`handlers/project.ts:23-34`).
async fn init_git(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let instance = location
        .services
        .instance(&location.directory)
        .map_err(session_defect)?;
    let next = ctx
        .projects
        .init_git(&location.directory, &instance.project)
        .map_err(|err| defect(err.to_string()))?;
    // `markInstanceForReload` — the response is sent before the instance
    // reloads; disposing the cached services re-boots it on the next
    // request (`lifecycle.ts:35-40`).
    if next.id != instance.project.id
        || next.vcs != instance.project.vcs
        || next.worktree != instance.project.worktree
    {
        ctx.instances.dispose_directory(&location.directory);
    }
    Ok(json_ok(next))
}

#[derive(serde::Deserialize, Default)]
struct UpdatePayload {
    #[serde(default)]
    name: Field<String>,
    #[serde(default)]
    icon: Field<ProjectIcon>,
    #[serde(default)]
    commands: Field<ProjectCommands>,
}

/// `update` (`handlers/project.ts:36-50`).
async fn update(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(_location): axum::Extension<LocationContext>,
    Path(project_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: UpdatePayload = parse_payload(&body)?;
    let updated = ctx
        .projects
        .update(&UpdateInput {
            project_id: project_id.clone(),
            name: payload.name,
            icon: payload.icon,
            commands: payload.commands,
        })
        .map_err(|err| match err {
            opencode_core::project::registry::RegistryError::NotFound(err) => {
                project_not_found(&err.project_id).into()
            }
            other => defect(other.to_string()),
        })?;
    Ok(json_ok(updated))
}

/// `directories` (`handlers/project.ts:52-54`).
async fn directories(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(_location): axum::Extension<LocationContext>,
    Path(project_id): Path<String>,
) -> Result<Response, ServerError> {
    let directories = ctx
        .projects
        .directories(&project_id)
        .map_err(|err| defect(err.to_string()))?;
    Ok(json_ok(directories))
}

/// A body-parse rejection in a 404-less route keeps the schema-error 400
/// shape (`middleware/schema-error.ts:28-41`).
pub fn register(
    router: axum::Router<Arc<ServerContext>>,
    method: &str,
    path: &'static str,
) -> (axum::Router<Arc<ServerContext>>, bool) {
    let router = match (method, path) {
        ("GET", "/project") => router.route(path, get(list)),
        ("GET", "/project/current") => router.route(path, get(current)),
        ("POST", "/project/git/init") => router.route(path, post(init_git)),
        ("PATCH", "/project/{projectID}") => router.route(path, patch(update)),
        ("GET", "/project/{projectID}/directories") => router.route(path, get(directories)),
        _ => return (router, false),
    };
    (router, true)
}
