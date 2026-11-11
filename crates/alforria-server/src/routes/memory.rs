//! Long-term memory (alforria extension).
//!
//! Project-scoped like the v1 API (`?directory=`), plain JSON, outside the
//! frozen v1/v2 route tables.
//!
//! - `GET    /memory`              records, newest first
//!   (`?kind=`, `?archived=true`, `?limit=`, `?offset=`)
//! - `POST   /memory`              save `{ content, kind?, metadata? }`
//! - `GET    /memory/search`       `?q=` (`?kind=`, `?archived=true`, `?limit=`);
//!   records with a `snippet`
//! - `GET    /memory/session`      `?q=` (`?limit=`): excerpts from past
//!   sessions, `{ hits, indexing }`
//! - `GET    /memory/{id}`         one record
//! - `PATCH  /memory/{id}`         `{ content?, kind?, metadata?, archived? }`
//! - `DELETE /memory/{id}`         `{ deleted }`
//!
//! Changes are announced by `memory.updated` (`{ id, action }`). With
//! `memory.enabled` off every route answers 403.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::Uri;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use alforria_core::memory::{
    ListQuery, MemoryError, MemoryHit, MemoryPatch, MemoryRecord, MemoryService,
};
use alforria_core::CoreError;

use crate::error::{ApiError, ServerError};
use crate::middleware::auth::query_param;
use crate::middleware::location::LocationContext;
use crate::state::{resolve_directory, ServerContext};

pub fn register(router: Router<Arc<ServerContext>>) -> Router<Arc<ServerContext>> {
    router
        .route("/memory", get(list).post(create))
        .route("/memory/search", get(search))
        .route("/memory/session", get(search_sessions))
        .route("/memory/{id}", get(read).patch(update).delete(remove))
}

fn memory(ctx: &ServerContext, uri: &Uri) -> Result<(Arc<MemoryService>, String), ServerError> {
    let directory = query_param(uri.query(), "directory")
        .map(PathBuf::from)
        .unwrap_or_else(crate::state::cwd);
    let directory = resolve_directory(&directory);
    let services = ctx.instances.load(&directory)?;
    let location = LocationContext {
        directory,
        workspace_id: None,
        services,
    };
    (ctx.engine_factory)(&location)?.memory().ok_or_else(|| {
        ServerError::Core(CoreError::Storage(
            "memory is not available on this engine".to_string(),
        ))
    })
}

fn memory_error(error: MemoryError) -> ServerError {
    let message = error.to_string();
    match error {
        MemoryError::Disabled => ServerError::Api(ApiError::Forbidden {
            message: "memory is disabled: set `memory.enabled` to true in the config".to_string(),
        }),
        MemoryError::Invalid(_) => ServerError::Api(ApiError::InvalidRequest {
            message,
            kind: None,
            field: None,
        }),
        MemoryError::Forbidden(_) => ServerError::Api(ApiError::Forbidden { message }),
        MemoryError::NotFound(_) => ServerError::Api(ApiError::not_found(message)),
        MemoryError::Failed(_) => ServerError::Core(CoreError::Storage(message)),
    }
}

fn flag(uri: &Uri, name: &str) -> bool {
    matches!(
        query_param(uri.query(), name).as_deref(),
        Some("true" | "1")
    )
}

fn number(uri: &Uri, name: &str) -> Option<usize> {
    query_param(uri.query(), name).and_then(|value| value.parse().ok())
}

fn query_text(uri: &Uri) -> Result<String, ServerError> {
    query_param(uri.query(), "q")
        .or_else(|| query_param(uri.query(), "query"))
        .ok_or_else(|| memory_error(MemoryError::Invalid("search needs `?q=`".to_string())))
}

async fn list(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
) -> Result<Json<Vec<MemoryRecord>>, ServerError> {
    let (memory, _) = memory(&ctx, &uri)?;
    memory
        .list(&ListQuery {
            kind: query_param(uri.query(), "kind"),
            archived: flag(&uri, "archived"),
            limit: number(&uri, "limit"),
            offset: number(&uri, "offset").unwrap_or(0),
            verified_only: false,
        })
        .map(Json)
        .map_err(memory_error)
}

#[derive(Deserialize)]
struct CreateBody {
    content: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    metadata: Option<Value>,
}

async fn create(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Json(body): Json<CreateBody>,
) -> Result<Json<MemoryRecord>, ServerError> {
    let (memory, _) = memory(&ctx, &uri)?;
    memory
        .remember(body.kind.as_deref(), &body.content, body.metadata)
        .map(Json)
        .map_err(memory_error)
}

async fn search(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
) -> Result<Json<Vec<MemoryHit>>, ServerError> {
    let (memory, _) = memory(&ctx, &uri)?;
    let query = query_text(&uri)?;
    memory
        .search(
            &query,
            query_param(uri.query(), "kind").as_deref(),
            flag(&uri, "archived"),
            number(&uri, "limit").unwrap_or(20),
        )
        .map(Json)
        .map_err(memory_error)
}

async fn search_sessions(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
) -> Result<Json<Value>, ServerError> {
    let (memory, project_id) = memory(&ctx, &uri)?;
    let query = query_text(&uri)?;
    let limit = number(&uri, "limit").unwrap_or(20);
    // The search may index a backlog first: keep it off the runtime.
    let (hits, indexing) = tokio::task::spawn_blocking(move || {
        memory.search_sessions(&query, limit, None, &project_id)
    })
    .await
    .map_err(|error| ServerError::Core(CoreError::Storage(error.to_string())))?
    .map_err(memory_error)?;
    Ok(Json(json!({ "hits": hits, "indexing": indexing })))
}

async fn read(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Path(id): Path<String>,
) -> Result<Json<MemoryRecord>, ServerError> {
    let (memory, _) = memory(&ctx, &uri)?;
    memory
        .get(&id)
        .map_err(memory_error)?
        .map(Json)
        .ok_or_else(|| memory_error(MemoryError::NotFound(format!("memory not found: {id}"))))
}

async fn update(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Path(id): Path<String>,
    Json(patch): Json<MemoryPatch>,
) -> Result<Json<MemoryRecord>, ServerError> {
    let (memory, _) = memory(&ctx, &uri)?;
    memory.update(&id, &patch).map(Json).map_err(memory_error)
}

async fn remove(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Path(id): Path<String>,
) -> Result<Json<Value>, ServerError> {
    let (memory, _) = memory(&ctx, &uri)?;
    let deleted = memory.forget(&id).map_err(memory_error)?;
    Ok(Json(json!({ "deleted": deleted })))
}
