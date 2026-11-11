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
//!
//! # Who may do what
//!
//! Basic auth and an `admin` token are the operator: what they write is the
//! user's own act (`Source::Api`), and their rewrite is the one thing that
//! vouches for a record.
//!
//! A credential below `admin` (`chat`, `connector`) reads and writes through
//! the rules a turn of its own would meet with the `memory` and
//! `session_search` tools:
//!
//! * a write needs the default agent's `memory` permission for the action
//!   (`remember`, `update`, `forget`) to be `allow`, and `GET
//!   /memory/session` needs `session_search`. An `ask` is a refusal: nobody
//!   is there to answer one for an HTTP call;
//! * what it writes is recorded as a client's (`Source::Client`), from a
//!   restricted writer: shown to restricted sessions, never injected into
//!   the operator's, and its rewrite of an unverified record leaves it
//!   unverified. Nothing in a body sets a record's origin or trust.
//!
//! A record id is looked up within the instance's memory (its project, or
//! the global one): an id from elsewhere is a 404.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::Uri;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use alforria_core::memory::{
    ListQuery, MemoryError, MemoryHit, MemoryPatch, MemoryRecord, MemoryService, Provenance,
};
use alforria_core::CoreError;

use crate::bearer::Principal;
use crate::error::{ApiError, ServerError};
use crate::middleware::auth::query_param;
use crate::middleware::directory::RequestDirectory;
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

pub fn register(router: Router<Arc<ServerContext>>) -> Router<Arc<ServerContext>> {
    router
        .route("/memory", get(list).post(create))
        .route("/memory/search", get(search))
        .route("/memory/session", get(search_sessions))
        .route("/memory/{id}", get(read).patch(update).delete(remove))
}

fn memory(
    ctx: &ServerContext,
    directory: &RequestDirectory,
) -> Result<(Arc<MemoryService>, String), ServerError> {
    located(ctx, directory).map(|(memory, project_id, _)| (memory, project_id))
}

fn located(
    ctx: &ServerContext,
    directory: &RequestDirectory,
) -> Result<
    (
        Arc<MemoryService>,
        String,
        Arc<alforria_core::SessionServices>,
    ),
    ServerError,
> {
    // The directory the auth layer resolved and the allowlist admitted
    // (`middleware::directory`): a handler never reads it off the request.
    let directory = directory.0.clone();
    let services = ctx.instances.load(&directory)?;
    let location = LocationContext {
        directory,
        workspace_id: None,
        services: services.clone(),
    };
    let (memory, project_id) = (ctx.engine_factory)(&location)?.memory().ok_or_else(|| {
        ServerError::Core(CoreError::Storage(
            "memory is not available on this engine".to_string(),
        ))
    })?;
    Ok((memory, project_id, services))
}

/// Who is writing, and whether it may: the operator freely, a credential
/// below `admin` when the default agent's rules allow `permission` for
/// `pattern` outright (see the module docs).
fn writer(
    ctx: &ServerContext,
    directory: &RequestDirectory,
    principal: &Option<axum::Extension<Principal>>,
    permission: &str,
    pattern: &str,
) -> Result<(Arc<MemoryService>, String, Provenance), ServerError> {
    let (memory, project_id, services) = located(ctx, directory)?;
    if !crate::confine::restricted(principal) {
        return Ok((memory, project_id, Provenance::api()));
    }
    let allowed = services.agents.default_info().is_ok_and(|agent| {
        alforria_core::tool::permission::evaluate(permission, pattern, &[&agent.permission]).action
            == alforria_schema::permission_v1::PermissionV1Action::Allow
    });
    if !allowed {
        return Err(ServerError::Api(ApiError::Forbidden {
            message: format!(
                "the `{permission}` permission does not allow `{pattern}` outright here, and \
                 nobody can be asked for an HTTP call: this needs the admin scope"
            ),
        }));
    }
    Ok((memory, project_id, Provenance::client()))
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
    axum::Extension(directory): axum::Extension<RequestDirectory>,
) -> Result<Json<Vec<MemoryRecord>>, ServerError> {
    let (memory, _) = memory(&ctx, &directory)?;
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
    principal: Option<axum::Extension<Principal>>,
    axum::Extension(directory): axum::Extension<RequestDirectory>,
    Json(body): Json<CreateBody>,
) -> Result<Json<MemoryRecord>, ServerError> {
    let (memory, _, by) = writer(&ctx, &directory, &principal, "memory", "remember")?;
    memory
        .remember_from(body.kind.as_deref(), &body.content, body.metadata, by)
        .map(Json)
        .map_err(memory_error)
}

async fn search(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    axum::Extension(directory): axum::Extension<RequestDirectory>,
) -> Result<Json<Vec<MemoryHit>>, ServerError> {
    let (memory, _) = memory(&ctx, &directory)?;
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
    principal: Option<axum::Extension<Principal>>,
    uri: Uri,
    axum::Extension(directory): axum::Extension<RequestDirectory>,
) -> Result<Json<Value>, ServerError> {
    // The tool asks for `session_search` before it reads other sessions.
    let (memory, project_id, _) = writer(&ctx, &directory, &principal, "session_search", "*")?;
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
    axum::Extension(directory): axum::Extension<RequestDirectory>,
    Path(id): Path<String>,
) -> Result<Json<MemoryRecord>, ServerError> {
    let (memory, _) = memory(&ctx, &directory)?;
    memory
        .get(&id)
        .map_err(memory_error)?
        .map(Json)
        .ok_or_else(|| memory_error(MemoryError::NotFound(format!("memory not found: {id}"))))
}

async fn update(
    State(ctx): State<Arc<ServerContext>>,
    principal: Option<axum::Extension<Principal>>,
    axum::Extension(directory): axum::Extension<RequestDirectory>,
    Path(id): Path<String>,
    Json(patch): Json<MemoryPatch>,
) -> Result<Json<MemoryRecord>, ServerError> {
    let (memory, _, by) = writer(&ctx, &directory, &principal, "memory", "update")?;
    memory
        .update_by(&id, &patch, &by)
        .map(Json)
        .map_err(memory_error)
}

async fn remove(
    State(ctx): State<Arc<ServerContext>>,
    principal: Option<axum::Extension<Principal>>,
    axum::Extension(directory): axum::Extension<RequestDirectory>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ServerError> {
    let (memory, _, by) = writer(&ctx, &directory, &principal, "memory", "forget")?;
    let deleted = memory.forget_by(&id, &by).map_err(memory_error)?;
    Ok(Json(json!({ "deleted": deleted })))
}
