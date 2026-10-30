//! Dynamic workflows (alforria extension).
//!
//! Project-scoped, like the v1 API: every call takes `?directory=`. The run
//! endpoints return plain JSON (no v2 envelope) and are registered outside
//! the frozen v1/v2 route tables, so the TS OpenAPI surface is unchanged.
//!
//! - `GET    /workflow`                       saved definitions
//! - `POST   /workflow`                       save a project/global script
//! - `DELETE /workflow/{name}`                delete a saved script
//! - `GET    /workflow/run`                   runs, newest first
//! - `POST   /workflow/run`                   start a run
//! - `GET    /workflow/run/{runID}`           one run
//! - `POST   /workflow/run/{runID}/wait`      wait for completion
//! - `POST   /workflow/run/{runID}/abort`     stop a run

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::Uri;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use alforria_schema::workflow::{
    WorkflowDefinition, WorkflowRun, WorkflowSaveRequest, WorkflowStartRequest,
};

use crate::error::ServerError;
use crate::middleware::location::LocationContext;
use crate::state::{resolve_directory, ServerContext, SessionEngine};

pub fn register(router: Router<Arc<ServerContext>>) -> Router<Arc<ServerContext>> {
    router
        .route("/workflow", get(list).post(save))
        .route("/workflow/run", get(runs).post(start).delete(remove_run))
        .route("/workflow/run/{runID}", get(run))
        .route("/workflow/run/{runID}/wait", post(wait))
        .route("/workflow/run/{runID}/abort", post(abort))
        .route("/workflow/{name}", delete(remove))
}

fn engine(ctx: &ServerContext, uri: &Uri) -> Result<Arc<dyn SessionEngine>, ServerError> {
    let directory = crate::middleware::auth::query_param(uri.query(), "directory")
        .map(PathBuf::from)
        .unwrap_or_else(crate::state::cwd);
    let directory = resolve_directory(&directory);
    let services = ctx.instances.load(&directory)?;
    let location = LocationContext {
        directory,
        workspace_id: None,
        services,
    };
    (ctx.engine_factory)(&location)
}

async fn list(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
) -> Result<Json<Vec<WorkflowDefinition>>, ServerError> {
    let engine = engine(&ctx, &uri)?;
    Ok(Json(engine.workflow_list().await?))
}

async fn save(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Json(request): Json<WorkflowSaveRequest>,
) -> Result<Json<WorkflowDefinition>, ServerError> {
    let engine = engine(&ctx, &uri)?;
    Ok(Json(engine.workflow_save(request)?))
}

async fn remove(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Path(name): Path<String>,
) -> Result<Json<Value>, ServerError> {
    let engine = engine(&ctx, &uri)?;
    Ok(Json(json!({ "deleted": engine.workflow_delete(name)? })))
}

/// `DELETE /workflow/{name}` for a workflow named `run`: the literal
/// `/workflow/run` route wins over the parameter one.
async fn remove_run(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
) -> Result<Json<Value>, ServerError> {
    let engine = engine(&ctx, &uri)?;
    Ok(Json(
        json!({ "deleted": engine.workflow_delete("run".to_string())? }),
    ))
}

async fn runs(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
) -> Result<Json<Vec<WorkflowRun>>, ServerError> {
    let engine = engine(&ctx, &uri)?;
    Ok(Json(engine.workflow_runs().await?))
}

async fn start(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Json(request): Json<WorkflowStartRequest>,
) -> Result<Json<WorkflowRun>, ServerError> {
    let engine = engine(&ctx, &uri)?;
    let run = engine.workflow_start(request).await?;
    Ok(Json(run))
}

async fn run(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Path(run_id): Path<String>,
) -> Result<Json<Option<WorkflowRun>>, ServerError> {
    let engine = engine(&ctx, &uri)?;
    Ok(Json(engine.workflow_get(run_id).await?))
}

async fn wait(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Path(run_id): Path<String>,
) -> Result<Json<WorkflowRun>, ServerError> {
    let engine = engine(&ctx, &uri)?;
    Ok(Json(engine.workflow_wait(run_id).await?))
}

async fn abort(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Path(run_id): Path<String>,
) -> Result<Json<WorkflowRun>, ServerError> {
    let engine = engine(&ctx, &uri)?;
    Ok(Json(engine.workflow_abort(run_id).await?))
}
