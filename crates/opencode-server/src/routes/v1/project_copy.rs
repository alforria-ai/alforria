//! v1 project-copy route — port of
//! `packages/opencode/src/server/routes/instance/httpapi/{groups,handlers}/project-copy.ts`
//! (only `generate-name` lives under v1; the v2 family lives in
//! `routes/v2/project_copy.rs`).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use axum::routing::post;
use serde::Deserialize;

use crate::error::ServerError;
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

use super::util::{json_ok, parse_payload};

type Router = axum::Router<Arc<ServerContext>>;

pub fn register(router: Router, method: &str, path: &'static str) -> (Router, bool) {
    let router = match (method, path) {
        ("POST", "/experimental/project/{projectID}/copy/generate-name") => {
            router.route(path, post(generate_name))
        }
        _ => return (router, false),
    };
    (router, true)
}

#[derive(Deserialize)]
struct GenerateNamePayload {
    #[serde(default)]
    context: Option<String>,
}

/// `generateName` (`handlers/project-copy.ts:22-69`) — empty/missing
/// context or no default model → `Slug.create()`; LLM failures →
/// `Slug.create()`; else the first 3 whitespace words, slugified.
async fn generate_name(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: GenerateNamePayload = parse_payload(&body)?;
    let engine = (ctx.engine_factory)(&location)?;
    let name = engine
        .generate_copy_name(payload.context.as_deref().unwrap_or_default())
        .await;
    Ok(json_ok(serde_json::json!({ "name": name })))
}
