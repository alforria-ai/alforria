//! Triggers — scheduled and event-started turns (alforria extension).
//!
//! Project-scoped like the v1 API (`?directory=`), plain JSON, and registered
//! outside the frozen v1/v2 route tables, like `/workflow`.
//!
//! - `GET    /trigger`                 every trigger, config and stored
//! - `POST   /trigger`                 create a stored trigger (`id` optional)
//! - `GET    /trigger/{id}`            one trigger
//! - `PATCH  /trigger/{id}`            JSON merge patch over the spec
//! - `DELETE /trigger/{id}`            delete a stored trigger
//! - `POST   /trigger/{id}/run`        fire now
//! - `POST   /trigger/{id}/hook`       the webhook entry; the body reaches the
//!   turn as fenced, untrusted data
//! - `GET    /trigger/{id}/runs`       runs, newest first (`?limit=`)
//!
//! The two firing routes answer as soon as the run is recorded; its progress
//! is announced by `trigger.fired`, `trigger.completed`, `trigger.failed` and
//! `trigger.skipped`. They are authenticated like every other route: the
//! webhook has no secret of its own.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::Uri;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use alforria_core::trigger::{
    TriggerError, TriggerRecord, TriggerRun, TriggerService, TriggerSpec,
};
use alforria_core::CoreError;

use crate::error::{ApiError, ServerError};
use crate::middleware::location::LocationContext;
use crate::state::{resolve_directory, ServerContext};

pub fn register(router: Router<Arc<ServerContext>>) -> Router<Arc<ServerContext>> {
    router
        .route("/trigger", get(list).post(create))
        .route("/trigger/{id}", get(read).patch(update).delete(remove))
        .route("/trigger/{id}/run", post(run))
        .route("/trigger/{id}/hook", post(hook))
        .route("/trigger/{id}/runs", get(runs))
}

fn triggers(ctx: &ServerContext, uri: &Uri) -> Result<Arc<TriggerService>, ServerError> {
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
    (ctx.engine_factory)(&location)?.triggers().ok_or_else(|| {
        ServerError::Core(CoreError::Storage(
            "triggers are not available on this engine".to_string(),
        ))
    })
}

fn trigger_error(error: TriggerError) -> ServerError {
    let message = error.to_string();
    match error {
        TriggerError::Disabled => ServerError::Api(ApiError::Forbidden { message }),
        TriggerError::Invalid(_) => ServerError::Api(ApiError::InvalidRequest {
            message,
            kind: None,
            field: None,
        }),
        TriggerError::NotFound(_) => ServerError::Api(ApiError::not_found(message)),
        TriggerError::Conflict(_) => ServerError::Api(ApiError::Conflict {
            message,
            resource: Some("trigger".to_string()),
        }),
        TriggerError::Failed(_) => ServerError::Core(CoreError::Storage(message)),
    }
}

fn invalid(message: String) -> ServerError {
    trigger_error(TriggerError::Invalid(message))
}

async fn list(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
) -> Result<Json<Vec<TriggerRecord>>, ServerError> {
    Ok(Json(triggers(&ctx, &uri)?.list()))
}

async fn create(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Json(mut body): Json<Value>,
) -> Result<Json<TriggerRecord>, ServerError> {
    let service = triggers(&ctx, &uri)?;
    let id = match body.as_object_mut().and_then(|object| object.remove("id")) {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) => Some(id),
        Some(_) => return Err(invalid("`id` must be a string".to_string())),
    };
    let spec: TriggerSpec =
        serde_json::from_value(body).map_err(|error| invalid(error.to_string()))?;
    service
        .create(id, spec, None, None)
        .map(Json)
        .map_err(trigger_error)
}

async fn read(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Path(id): Path<String>,
) -> Result<Json<TriggerRecord>, ServerError> {
    triggers(&ctx, &uri)?
        .get(&id)
        .map(Json)
        .ok_or_else(|| trigger_error(TriggerError::NotFound(format!("trigger not found: {id}"))))
}

async fn update(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Path(id): Path<String>,
    Json(patch): Json<Value>,
) -> Result<Json<TriggerRecord>, ServerError> {
    triggers(&ctx, &uri)?
        .update(&id, &patch)
        .map(Json)
        .map_err(trigger_error)
}

async fn remove(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Path(id): Path<String>,
) -> Result<Json<Value>, ServerError> {
    let deleted = triggers(&ctx, &uri)?.delete(&id).map_err(trigger_error)?;
    Ok(Json(json!({ "deleted": deleted })))
}

async fn run(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Path(id): Path<String>,
) -> Result<Json<TriggerRun>, ServerError> {
    triggers(&ctx, &uri)?
        .run_now(&id)
        .map(Json)
        .map_err(trigger_error)
}

async fn hook(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<TriggerRun>, ServerError> {
    triggers(&ctx, &uri)?
        .hook(&id, &body)
        .map(Json)
        .map_err(trigger_error)
}

async fn runs(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Path(id): Path<String>,
) -> Result<Json<Vec<TriggerRun>>, ServerError> {
    let limit = crate::middleware::auth::query_param(uri.query(), "limit")
        .and_then(|limit| limit.parse::<usize>().ok())
        .unwrap_or(50);
    triggers(&ctx, &uri)?
        .runs(&id, limit)
        .map(Json)
        .map_err(trigger_error)
}
