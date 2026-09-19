//! v2 credential routes — port of
//! `packages/protocol/src/groups/credential.ts` +
//! `packages/server/src/handlers/credential.ts`.

use std::sync::Arc;

use axum::extract::Path;
use axum::response::Response;
use axum::routing::{delete, patch};

use crate::error::ServerError;
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

use super::util::{defect, no_content, parse_payload};

type Router = axum::Router<Arc<ServerContext>>;

pub fn register(router: Router, method: &str, path: &'static str) -> (Router, bool) {
    let router = match (method, path) {
        ("PATCH", "/api/credential/{credentialID}") => router.route(path, patch(update)),
        ("DELETE", "/api/credential/{credentialID}") => router.route(path, delete(remove)),
        _ => return (router, false),
    };
    (router, true)
}

/// `credential.update` (`handlers/credential.ts:10-14`).
async fn update(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(credential_id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, ServerError> {
    let payload: UpdatePayload = parse_payload(&body)?;
    location
        .services
        .integrations
        .connection_update(&credential_id, Some(&payload.label))
        .map_err(defect)?;
    Ok(no_content())
}

#[derive(serde::Deserialize)]
struct UpdatePayload {
    label: String,
}

/// `credential.remove` (`handlers/credential.ts:16-21`).
async fn remove(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(credential_id): Path<String>,
) -> Result<Response, ServerError> {
    location
        .services
        .integrations
        .connection_remove(&credential_id)
        .map_err(defect)?;
    Ok(no_content())
}
