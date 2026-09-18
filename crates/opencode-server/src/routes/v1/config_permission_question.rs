//! v1 config + permission + question route families (M6.6) — port of
//! `httpapi/handlers/{config,permission,question}.ts` over the M3 config
//! loader and the M5 permission/question services.

use std::path::Path;

use axum::body::Bytes;
use axum::extract::{Path as PathParam, State};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use opencode_core::config::schema::decode_config;
use opencode_schema::permission_v1::{PermissionV1Reply, PermissionV1ReplyInput};
use opencode_schema::question_v1::QuestionV1Answer;

use crate::error::{ApiError, ServerError};
use crate::middleware::location::LocationContext;
use crate::routes::v1::util::*;
use crate::state::ServerContext;

// ---------------------------------------------------------------------------
// config (`handlers/config.ts`)
// ---------------------------------------------------------------------------

/// `Config.Service.update` (config.ts:637-668): merge the payload into
/// `<directory>/config.json`, pretty-printed JSON.
fn update_config_file(directory: &Path, payload: &Value) -> Result<(), ServerError> {
    let file = directory.join("config.json");
    let original = match std::fs::read_to_string(&file) {
        Ok(text) if !text.trim().is_empty() => opencode_core::parse_jsonc(&text, &file)
            .map_err(|err| payload_error(err.to_string()))?,
        _ => json!({}),
    };
    let mut merged = original;
    opencode_core::merge_deep(&mut merged, payload);
    std::fs::write(
        &file,
        serde_json::to_string_pretty(&merged).expect("serialization"),
    )
    .map_err(|err| defect(format!("failed to write config: {err}")))?;
    Ok(())
}

/// `get` (`handlers/config.ts:15-17`).
pub async fn config_get(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let config = load_config(&location.directory)?;
    Ok(json_ok(config))
}

/// `update` (`handlers/config.ts:19-23`) — the response is the payload; the
/// instance is disposed after the response (lifecycle.ts).
pub async fn config_update(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|err| payload_error(format!("Invalid JSON payload: {err}")))?;
    if !payload.is_object() {
        return Err(payload_error("Expected an object"));
    }
    decode_config(&payload, &location.directory.join("opencode.json"))
        .map_err(|err| payload_error(err.to_string()))?;
    update_config_file(&location.directory, &payload)?;
    let response = json_ok(&payload);
    ctx.instances.dispose_directory(&location.directory);
    Ok(response)
}

/// `providers` (`handlers/config.ts:25-31`).
pub async fn config_providers(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let config = load_config(&location.directory)?;
    Ok(json_ok(
        crate::routes::v1::provider::config_providers_result(&ctx, &config)?,
    ))
}

// ---------------------------------------------------------------------------
// permission (`handlers/permission.ts`)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PermissionReplyPayload {
    reply: PermissionV1Reply,
    #[serde(default)]
    message: Option<String>,
}

/// `list` (`handlers/permission.ts:13-18`).
pub async fn permission_list(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    Ok(json_ok(location.services.permission.list()))
}

/// `reply` (`handlers/permission.ts:20-43`).
pub async fn permission_reply(
    axum::Extension(location): axum::Extension<LocationContext>,
    PathParam(request_id): PathParam<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    require_param("per", &request_id)?;
    let payload: PermissionReplyPayload = parse_payload(&body)?;
    match location.services.permission.reply(PermissionV1ReplyInput {
        request_id: request_id.clone(),
        reply: payload.reply,
        message: payload.message,
    }) {
        Ok(()) => Ok(json_ok(true)),
        Err(opencode_core::PermissionError::NotFound { request_id }) => {
            Err(ApiError::PermissionNotFound {
                request_id: request_id.clone(),
                message: format!("Permission request not found: {request_id}"),
            }
            .into())
        }
        Err(err) => Err(defect(err)),
    }
}

// ---------------------------------------------------------------------------
// question (`handlers/question.ts`)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct QuestionReplyPayload {
    answers: Vec<QuestionV1Answer>,
}

/// `list` (`handlers/question.ts:13-17`).
pub async fn question_list(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    Ok(json_ok(location.services.question.list()))
}

/// `reply` (`handlers/question.ts:19-44`).
pub async fn question_reply(
    axum::Extension(location): axum::Extension<LocationContext>,
    PathParam(request_id): PathParam<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    require_param("que", &request_id)?;
    let payload: QuestionReplyPayload = parse_payload(&body)?;
    match location
        .services
        .question
        .reply(&request_id, payload.answers)
    {
        Ok(()) => Ok(json_ok(true)),
        Err(opencode_core::QuestionError::NotFound { request_id }) => {
            Err(ApiError::QuestionNotFound {
                request_id: request_id.clone(),
                message: format!("Question request not found: {request_id}"),
            }
            .into())
        }
        Err(err) => Err(defect(err)),
    }
}

/// `reject` (`handlers/question.ts:46-62`).
pub async fn question_reject(
    axum::Extension(location): axum::Extension<LocationContext>,
    PathParam(request_id): PathParam<String>,
) -> Result<Response, ServerError> {
    require_param("que", &request_id)?;
    match location.services.question.reject(&request_id) {
        Ok(()) => Ok(json_ok(true)),
        Err(opencode_core::QuestionError::NotFound { request_id }) => {
            Err(ApiError::QuestionNotFound {
                request_id: request_id.clone(),
                message: format!("Question request not found: {request_id}"),
            }
            .into())
        }
        Err(err) => Err(defect(err)),
    }
}

// ---------------------------------------------------------------------------
// route registration
// ---------------------------------------------------------------------------

pub fn register(
    router: axum::Router<Arc<ServerContext>>,
    method: &str,
    path: &'static str,
) -> (axum::Router<Arc<ServerContext>>, bool) {
    use axum::routing::patch;
    let router = match (method, path) {
        ("GET", "/config") => router.route(path, get(config_get)),
        ("PATCH", "/config") => router.route(path, patch(config_update)),
        ("GET", "/config/providers") => router.route(path, get(config_providers)),
        ("GET", "/permission") => router.route(path, get(permission_list)),
        ("POST", "/permission/{requestID}/reply") => router.route(path, post(permission_reply)),
        ("GET", "/question") => router.route(path, get(question_list)),
        ("POST", "/question/{requestID}/reply") => router.route(path, post(question_reply)),
        ("POST", "/question/{requestID}/reject") => router.route(path, post(question_reject)),
        _ => return (router, false),
    };
    (router, true)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_config_file_merges_and_pretty_prints() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), r#"{"theme":"dark"}"#).unwrap();
        update_config_file(dir.path(), &json!({"shell": "/bin/sh"})).unwrap();
        let written = std::fs::read_to_string(dir.path().join("config.json")).unwrap();
        let value: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(value["shell"], "/bin/sh");
    }

    #[test]
    fn update_config_file_creates_missing() {
        let dir = tempfile::tempdir().unwrap();
        update_config_file(dir.path(), &json!({"username": "tester"})).unwrap();
        let written = std::fs::read_to_string(dir.path().join("config.json")).unwrap();
        assert!(written.contains('\n'), "pretty printed: {written}");
        let value: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(value["username"], "tester");
    }
}
