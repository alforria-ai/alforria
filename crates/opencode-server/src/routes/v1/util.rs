//! Shared v1-handler wire helpers — extracted from the M6.5 session family.

use std::fmt::Display;

use axum::body::{Body, Bytes};
use axum::http::{header, StatusCode, Uri};
use axum::response::Response;
use serde_json::Value;

use opencode_core::{CoreError, SessionError};

use crate::error::{ApiError, ServerError};
use crate::middleware::auth::query_param;

/// 200 with a JSON body.
pub fn json_ok(value: impl serde::Serialize) -> Response {
    let body = serde_json::to_string(&value).expect("serialization cannot fail");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("static response parts are valid")
}

/// `HttpApiError.BadRequest` — an empty 400 body (`HttpApiError.ts:41-51`).
pub fn bad_request_empty() -> Response {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Body::empty())
        .expect("static response parts are valid")
}

/// `HttpApiError.InternalServerError` — an empty 500 body.
pub fn internal_error_empty() -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::empty())
        .expect("static response parts are valid")
}

/// A defect — unexpected core failure routed through the defect-500 envelope.
pub fn defect(err: impl Display) -> ServerError {
    ServerError::Core(CoreError::Storage(err.to_string()))
}

/// `mapStorageNotFound` (`handlers/session-errors.ts:6-8`).
pub fn session_error(err: SessionError) -> ServerError {
    match err {
        SessionError::NotFound(e) => ApiError::not_found(e.message).into(),
        other => defect(other),
    }
}

pub fn query_error(message: impl Into<String>) -> ServerError {
    ApiError::bad_request_schema(message, "Query").into()
}

pub fn payload_error(message: impl Into<String>) -> ServerError {
    ApiError::bad_request_schema(message, "Payload").into()
}

pub fn query_i64(uri: &Uri, name: &str) -> Result<Option<i64>, ServerError> {
    match query_param(uri.query(), name) {
        Some(value) => value
            .parse::<i64>()
            .map(Some)
            .map_err(|_| query_error(format!("Expected a number, got {value:?}"))),
        None => Ok(None),
    }
}

pub fn query_bool(uri: &Uri, name: &str) -> Result<bool, ServerError> {
    match query_param(uri.query(), name) {
        Some(value) if value == "true" => Ok(true),
        Some(value) if value == "false" => Ok(false),
        Some(value) => Err(query_error(format!(
            "Expected \"true\" or \"false\", got {value:?}"
        ))),
        None => Ok(false),
    }
}

/// Branded-ID validation (`MessageID.make` etc., session/schema.ts) — a
/// path param without the prefix is a `Params` schema rejection.
pub fn require_param(prefix: &str, value: &str) -> Result<(), ServerError> {
    if value.starts_with(prefix) {
        return Ok(());
    }
    Err(ApiError::bad_request_schema(
        format!("Expected a string starting with {prefix:?}, got {value:?}"),
        "Params",
    )
    .into())
}

pub fn require_payload_id(prefix: &str, value: &str) -> Result<(), ServerError> {
    if value.starts_with(prefix) {
        return Ok(());
    }
    Err(ApiError::bad_request_schema(
        format!("Expected a string starting with {prefix:?}, got {value:?}"),
        "Payload",
    )
    .into())
}

/// Parse a required payload body. Invalid JSON or schema mismatches go
/// through the schema-error middleware shape (`middleware/schema-error.ts:28-41`).
pub fn parse_payload<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, ServerError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|err| payload_error(format!("Invalid JSON payload: {err}")))?;
    serde_json::from_value(value).map_err(|err| payload_error(err.to_string()))
}

/// The instance's merged config (`Config.Service.get`) — M3 precedence.
pub fn load_config(
    directory: &std::path::Path,
) -> Result<opencode_core::config::schema::Config, ServerError> {
    let params = opencode_core::LoadParams::new(directory.to_path_buf())
        .paths(opencode_core::GlobalPaths::from_env());
    let (config, _) = opencode_core::ConfigLoader::new().load(&params)?;
    Ok(config)
}
