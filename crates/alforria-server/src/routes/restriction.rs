//! A session's restriction (alforria extension, `session::caller`).
//!
//! A session created by a credential below `admin` is restricted for life,
//! and so are its children and forks: every turn in it — a request's or
//! the engine's own — is bound as that credential was. An operator who
//! wants to take such a session over lifts the restriction first.
//!
//! - `GET    /session/{sessionID}/restriction`   `{ "restricted": bool, "outsideContent": bool, "outsideStart": bool }`
//!   (the last two are the session's marks, `session::store::SessionMark`)
//! - `DELETE /session/{sessionID}/restriction`   lift it; `{ "restricted": false, "lifted": bool }`
//!
//! Both are outside the frozen tables and absent from the scope table, so
//! they need `admin`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::error::ServerError;
use crate::state::ServerContext;

pub fn register(router: Router<Arc<ServerContext>>) -> Router<Arc<ServerContext>> {
    router.route("/session/{sessionID}/restriction", get(show).delete(lift))
}

fn session_error(error: alforria_core::SessionError) -> ServerError {
    ServerError::Core(alforria_core::CoreError::Storage(error.to_string()))
}

async fn show(
    State(ctx): State<Arc<ServerContext>>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, ServerError> {
    ctx.sessions.get(&session_id).map_err(session_error)?;
    let restricted = ctx
        .sessions
        .is_restricted(&session_id)
        .map_err(session_error)?;
    let marks = ctx.sessions.marks(&session_id).map_err(session_error)?;
    Ok(Json(json!({
        "restricted": restricted,
        "outsideContent": marks.outside_content,
        "outsideStart": marks.outside_start,
    })))
}

async fn lift(
    State(ctx): State<Arc<ServerContext>>,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, ServerError> {
    ctx.sessions.get(&session_id).map_err(session_error)?;
    let lifted = ctx
        .sessions
        .lift_restriction(&session_id)
        .map_err(session_error)?;
    Ok(Json(json!({ "restricted": false, "lifted": lifted })))
}
