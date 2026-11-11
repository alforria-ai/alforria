//! Session goals (alforria extension).
//!
//! `/goal <objective>` keeps a session working until a reviewer agrees the
//! objective is met (`alforria_core::session::goal`). The command itself
//! goes through the ordinary session command route; this adds the read side
//! clients need to show the goal:
//!
//! - `GET /session/{sessionID}/goal` — the goal, or `null`
//!
//! Changes are announced by the `goal.changed` event
//! (`{ sessionID, goal }`). Like the workflow routes this sits outside the
//! frozen v1/v2 tables, takes `?directory=`, and returns plain JSON.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::Uri;
use axum::routing::get;
use axum::{Json, Router};

use alforria_core::session::goal::GoalState;

use crate::error::ServerError;
use crate::middleware::location::LocationContext;
use crate::state::{resolve_directory, ServerContext};

pub fn register(router: Router<Arc<ServerContext>>) -> Router<Arc<ServerContext>> {
    router.route("/session/{sessionID}/goal", get(goal))
}

async fn goal(
    State(ctx): State<Arc<ServerContext>>,
    uri: Uri,
    Path(session_id): Path<String>,
) -> Result<Json<Option<GoalState>>, ServerError> {
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
    let engine = (ctx.engine_factory)(&location)?;
    Ok(Json(engine.goal(&session_id)))
}
