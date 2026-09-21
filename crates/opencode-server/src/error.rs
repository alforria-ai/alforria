//! Error wire envelopes — port of `httpapi/errors.ts`, `protocol/src/errors.ts`
//! and `httpapi/middleware/error.ts`.
//!
//! TS defines two wire families:
//!
//! * `Schema.TaggedErrorClass` errors (`httpapi/errors.ts:3-176`) serialize as
//!   a flat object with `_tag` first and the schema fields after it — the exact
//!   shape of the `InvalidRequestError`/`SessionBusyError` OpenAPI components.
//! * `Schema.ErrorClass` / `NamedError` errors (`ApiNotFoundError`,
//!   `NamedError.Unknown`, the `ConfigErrorV1` classes, and the v1 schema-error
//!   `BadRequest` body) serialize as `{ name, data }`.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::future::FutureExt;
use opencode_core::{CoreError, SchemaIssue};
use serde_json::Value;
use uuid::Uuid;

/// Serialize one JSON scalar/array/object value.
fn ser(value: &Value) -> String {
    serde_json::to_string(value).expect("Value serialization cannot fail")
}

/// Serialize an object with the fields in the given (insertion) order.
///
/// `serde_json::Value` maps sort keys, which would break byte-parity with TS
/// (`JSON.stringify` emits schema-declaration order), so envelopes are built
/// from pre-serialized fragments instead.
fn object(pairs: &[(&str, String)]) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::to_string(key).expect("key serialization cannot fail"));
        out.push(':');
        out.push_str(value);
    }
    out.push('}');
    out
}

/// `{"_tag": name, ...fields}` — TS `Schema.TaggedErrorClass` serialization
/// (`_tag` literal first, schema fields in declaration order).
fn tagged_body(tag: &str, fields: Vec<(&str, String)>) -> String {
    let mut pairs = vec![("_tag", ser(&Value::from(tag)))];
    pairs.extend(fields);
    object(&pairs)
}

/// `{"name": name, "data": {...}}` — TS `NamedError.toObject()` /
/// `Schema.ErrorClass` serialization.
fn named_body(name: &str, data: Vec<(&str, String)>) -> String {
    object(&[("name", ser(&Value::from(name))), ("data", object(&data))])
}

fn str_field(value: &str) -> String {
    ser(&Value::from(value))
}

fn opt_str_field(value: &Option<String>) -> Option<String> {
    value.as_ref().map(|v| str_field(v))
}

/// Required fields followed by `Schema.optional` fields (omitted when
/// `None`, matching `JSON.stringify` of an absent property).
fn opt_fields<'a>(
    required: &[(&'a str, String)],
    optional: &[(&'a str, Option<String>)],
) -> Vec<(&'a str, String)> {
    let mut fields: Vec<(&'a str, String)> = required.to_vec();
    for (key, value) in optional {
        if let Some(value) = value {
            fields.push((key, value.clone()));
        }
    }
    fields
}

/// One variant per TS error class (`httpapi/errors.ts` + `protocol/src/errors.ts`).
#[derive(Debug, Clone)]
pub enum ApiError {
    /// `InvalidRequestError` (400, tagged).
    InvalidRequest {
        message: String,
        kind: Option<String>,
        field: Option<String>,
    },
    /// `UnauthorizedError` (401, tagged).
    Unauthorized { message: String },
    /// `ForbiddenError` (403, tagged).
    Forbidden { message: String },
    /// `ConflictError` (409, tagged).
    Conflict {
        message: String,
        resource: Option<String>,
    },
    /// `UpstreamError` (502, tagged).
    Upstream {
        message: String,
        service: Option<String>,
        status: Option<i64>,
    },
    /// `ServiceUnavailableError` (503, tagged).
    ServiceUnavailable {
        message: String,
        service: Option<String>,
    },
    /// `TimeoutError` (504, tagged).
    Timeout {
        message: String,
        operation: Option<String>,
    },
    /// `UnknownError` (500, tagged).
    Unknown {
        message: String,
        reference: Option<String>,
    },
    /// `ProviderNotFoundError` (404, tagged).
    ProviderNotFound {
        provider_id: String,
        message: String,
    },
    /// `ModelNotFoundError` (404, tagged).
    ModelNotFound {
        provider_id: String,
        model_id: String,
        suggestions: Vec<String>,
        message: String,
    },
    /// `SessionNotFoundError` (404, tagged).
    SessionNotFound { session_id: String, message: String },
    /// `MessageNotFoundError` (404, tagged).
    MessageNotFound {
        session_id: String,
        message_id: String,
        message: String,
    },
    /// `InvalidCursorError` (400, tagged).
    InvalidCursor { message: String },
    /// `SessionBusyError` (409, tagged).
    SessionBusy { session_id: String, message: String },
    /// `QuestionNotFoundError` (404, tagged).
    QuestionNotFound { request_id: String, message: String },
    /// `PermissionNotFoundError` (404, tagged).
    PermissionNotFound { request_id: String, message: String },
    /// `McpServerNotFoundError` (404, tagged).
    McpServerNotFound { name: String, message: String },
    /// `PtyNotFoundError` (404, tagged).
    PtyNotFound { pty_id: String, message: String },
    /// `PtyForbiddenError` (403, tagged).
    PtyForbidden { message: String },
    /// `ProjectNotFoundError` (404, tagged).
    ProjectNotFound { project_id: String, message: String },
    /// `ProviderAuthApiError` (400, `groups/provider.ts:14-32`) —
    /// `ErrorClass` with `{name, data}` fields.
    ProviderAuth {
        name: &'static str,
        provider_id: Option<String>,
        field: Option<String>,
        message: Option<String>,
    },
    /// `ApiNotFoundError` (`errors.ts:178-186`) — `ErrorClass("NotFoundError")`,
    /// wire `{"name":"NotFoundError","data":{"message":...}}`.
    NotFound { message: String },
    /// v1 schema-rejection 400 (`middleware/schema-error.ts:35-38`), wire
    /// `{"name":"BadRequest","data":{"message":...,"kind":...}}`.
    BadRequest {
        message: String,
        kind: Option<String>,
    },
    /// `ApiVcsApplyError` (400, `groups/instance.ts:32-41`) —
    /// `{"name":"VcsApplyError","data":{"message":...,"reason":...}}`.
    VcsApply {
        message: String,
        reason: &'static str,
    },
    /// `WorktreeApiError` (400, `groups/experimental.ts:70-77`) —
    /// `{"name":<tag>,"data":{"message":...}}`.
    Worktree { tag: &'static str, message: String },
    /// `ApiMoveSessionError` (400, `groups/control-plane.ts:9-17`) —
    /// `{"name":"MoveSessionError","data":{"message":...}}`.
    MoveSession { message: String },
    /// `ProjectCopyError` (400, `groups/project-copy.ts:16-25`) —
    /// `{"name":"ProjectCopyError","data":{"message":...,"forceRequired"?}}`.
    ProjectCopy {
        message: String,
        force_required: Option<bool>,
    },
    /// `McpUnsupportedOAuthError` (400, `groups/mcp.ts:35-37`) — a plain
    /// `Schema.ErrorClass`, serializing FLAT: `{"error": "..."}`.
    McpUnsupportedOAuth { error: String },
    /// `HttpApiError.BadRequest` (400) — Effect's bare tagged error, wire
    /// `{"_tag": "BadRequest"}`.
    TaggedBadRequest,
    /// `ApiWorkspaceCreateError` (400, `groups/workspace.ts:18-26`) —
    /// `{"name":"WorkspaceCreateError","data":{"message":...}}`.
    WorkspaceCreate { message: String },
    /// `ApiWorkspaceWarpError` (400, `groups/workspace.ts:9-16`) —
    /// `{"name":"WorkspaceWarpError","data":{"message":...}}`.
    WorkspaceWarp { message: String },
}

impl ApiError {
    /// `notFound(message)` constructor (`errors.ts:188-193`).
    pub fn not_found(message: impl Into<String>) -> ApiError {
        ApiError::NotFound {
            message: message.into(),
        }
    }

    /// v1 schema-rejection 400 with truncated reason
    /// (`middleware/schema-error.ts:10-13`).
    pub fn bad_request_schema(message: impl Into<String>, kind: &str) -> ApiError {
        ApiError::BadRequest {
            message: truncate_reason(&message.into()),
            kind: Some(kind.to_string()),
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::InvalidRequest { .. } => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized { .. } => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden { .. } => StatusCode::FORBIDDEN,
            ApiError::Conflict { .. } => StatusCode::CONFLICT,
            ApiError::Upstream { .. } => StatusCode::BAD_GATEWAY,
            ApiError::ServiceUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            ApiError::Unknown { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::ProviderNotFound { .. }
            | ApiError::ModelNotFound { .. }
            | ApiError::SessionNotFound { .. }
            | ApiError::MessageNotFound { .. }
            | ApiError::QuestionNotFound { .. }
            | ApiError::PermissionNotFound { .. }
            | ApiError::McpServerNotFound { .. }
            | ApiError::PtyNotFound { .. }
            | ApiError::ProjectNotFound { .. }
            | ApiError::NotFound { .. } => StatusCode::NOT_FOUND,
            ApiError::ProviderAuth { .. } => StatusCode::BAD_REQUEST,
            ApiError::InvalidCursor { .. } => StatusCode::BAD_REQUEST,
            ApiError::SessionBusy { .. } => StatusCode::CONFLICT,
            ApiError::PtyForbidden { .. } => StatusCode::FORBIDDEN,
            ApiError::BadRequest { .. } => StatusCode::BAD_REQUEST,
            ApiError::VcsApply { .. }
            | ApiError::Worktree { .. }
            | ApiError::ProjectCopy { .. }
            | ApiError::MoveSession { .. }
            | ApiError::McpUnsupportedOAuth { .. }
            | ApiError::TaggedBadRequest
            | ApiError::WorkspaceCreate { .. }
            | ApiError::WorkspaceWarp { .. } => StatusCode::BAD_REQUEST,
        }
    }

    fn body(&self) -> String {
        match self {
            ApiError::InvalidRequest {
                message,
                kind,
                field,
            } => tagged_body(
                "InvalidRequestError",
                opt_fields(
                    &[("message", str_field(message))],
                    &[
                        ("kind", opt_str_field(kind)),
                        ("field", opt_str_field(field)),
                    ],
                ),
            ),
            ApiError::Unauthorized { message } => {
                tagged_body("UnauthorizedError", vec![("message", str_field(message))])
            }
            ApiError::Forbidden { message } => {
                tagged_body("ForbiddenError", vec![("message", str_field(message))])
            }
            ApiError::Conflict { message, resource } => tagged_body(
                "ConflictError",
                opt_fields(
                    &[("message", str_field(message))],
                    &[("resource", opt_str_field(resource))],
                ),
            ),
            ApiError::Upstream {
                message,
                service,
                status,
            } => tagged_body(
                "UpstreamError",
                opt_fields(
                    &[("message", str_field(message))],
                    &[
                        ("service", opt_str_field(service)),
                        ("status", status.map(|s| Value::from(s).to_string())),
                    ],
                ),
            ),
            ApiError::ServiceUnavailable { message, service } => tagged_body(
                "ServiceUnavailableError",
                opt_fields(
                    &[("message", str_field(message))],
                    &[("service", opt_str_field(service))],
                ),
            ),
            ApiError::Timeout { message, operation } => tagged_body(
                "TimeoutError",
                opt_fields(
                    &[("message", str_field(message))],
                    &[("operation", opt_str_field(operation))],
                ),
            ),
            ApiError::Unknown { message, reference } => tagged_body(
                "UnknownError",
                opt_fields(
                    &[("message", str_field(message))],
                    &[("ref", opt_str_field(reference))],
                ),
            ),
            ApiError::ProviderNotFound {
                provider_id,
                message,
            } => tagged_body(
                "ProviderNotFoundError",
                vec![
                    ("providerID", str_field(provider_id)),
                    ("message", str_field(message)),
                ],
            ),
            ApiError::ModelNotFound {
                provider_id,
                model_id,
                suggestions,
                message,
            } => tagged_body(
                "ModelNotFoundError",
                vec![
                    ("providerID", str_field(provider_id)),
                    ("modelID", str_field(model_id)),
                    ("suggestions", serde_json::to_string(suggestions).unwrap()),
                    ("message", str_field(message)),
                ],
            ),
            ApiError::SessionNotFound {
                session_id,
                message,
            } => tagged_body(
                "SessionNotFoundError",
                vec![
                    ("sessionID", str_field(session_id)),
                    ("message", str_field(message)),
                ],
            ),
            ApiError::MessageNotFound {
                session_id,
                message_id,
                message,
            } => tagged_body(
                "MessageNotFoundError",
                vec![
                    ("sessionID", str_field(session_id)),
                    ("messageID", str_field(message_id)),
                    ("message", str_field(message)),
                ],
            ),
            ApiError::InvalidCursor { message } => {
                tagged_body("InvalidCursorError", vec![("message", str_field(message))])
            }
            ApiError::SessionBusy {
                session_id,
                message,
            } => tagged_body(
                "SessionBusyError",
                vec![
                    ("sessionID", str_field(session_id)),
                    ("message", str_field(message)),
                ],
            ),
            ApiError::QuestionNotFound {
                request_id,
                message,
            } => tagged_body(
                "QuestionNotFoundError",
                vec![
                    ("requestID", str_field(request_id)),
                    ("message", str_field(message)),
                ],
            ),
            ApiError::PermissionNotFound {
                request_id,
                message,
            } => tagged_body(
                "PermissionNotFoundError",
                vec![
                    ("requestID", str_field(request_id)),
                    ("message", str_field(message)),
                ],
            ),
            ApiError::McpServerNotFound { name, message } => tagged_body(
                "McpServerNotFoundError",
                vec![("name", str_field(name)), ("message", str_field(message))],
            ),
            ApiError::PtyNotFound { pty_id, message } => tagged_body(
                "PtyNotFoundError",
                vec![
                    ("ptyID", str_field(pty_id)),
                    ("message", str_field(message)),
                ],
            ),
            ApiError::PtyForbidden { message } => {
                tagged_body("PtyForbiddenError", vec![("message", str_field(message))])
            }
            ApiError::ProjectNotFound {
                project_id,
                message,
            } => tagged_body(
                "ProjectNotFoundError",
                vec![
                    ("projectID", str_field(project_id)),
                    ("message", str_field(message)),
                ],
            ),
            ApiError::ProviderAuth {
                name,
                provider_id,
                field,
                message,
            } => {
                let data = opt_fields(
                    &[],
                    &[
                        ("providerID", opt_str_field(provider_id)),
                        ("field", opt_str_field(field)),
                        ("message", opt_str_field(message)),
                    ],
                );
                object(&[("name", str_field(name)), ("data", object(&data))])
            }
            // `ErrorClass` shapes — `{name, data}` on the wire.
            ApiError::NotFound { message } => {
                named_body("NotFoundError", vec![("message", str_field(message))])
            }
            ApiError::BadRequest { message, kind } => named_body(
                "BadRequest",
                opt_fields(
                    &[("message", str_field(message))],
                    &[("kind", opt_str_field(kind))],
                ),
            ),
            ApiError::VcsApply { message, reason } => named_body(
                "VcsApplyError",
                vec![
                    ("message", str_field(message)),
                    ("reason", str_field(reason)),
                ],
            ),
            ApiError::Worktree { tag, message } => {
                named_body(tag, vec![("message", str_field(message))])
            }
            ApiError::MoveSession { message } => {
                named_body("MoveSessionError", vec![("message", str_field(message))])
            }
            ApiError::ProjectCopy {
                message,
                force_required,
            } => {
                let data = match force_required {
                    Some(force_required) => {
                        serde_json::json!({"message": message, "forceRequired": force_required})
                    }
                    None => serde_json::json!({"message": message}),
                };
                object(&[
                    ("name", str_field("ProjectCopyError")),
                    ("data", ser(&data)),
                ])
            }
            // `Schema.ErrorClass` shapes — flat fields, no wrapper.
            ApiError::McpUnsupportedOAuth { error } => object(&[("error", str_field(error))]),
            ApiError::TaggedBadRequest => tagged_body("BadRequest", vec![]),
            ApiError::WorkspaceCreate { message } => named_body(
                "WorkspaceCreateError",
                vec![("message", str_field(message))],
            ),
            ApiError::WorkspaceWarp { message } => {
                named_body("WorkspaceWarpError", vec![("message", str_field(message))])
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = self.body();
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body))
            .expect("static response parts are valid")
    }
}

/// Cap schema-rejection reasons at 1024 chars
/// (`middleware/schema-error.ts:10-13`).
const REASON_LIMIT: usize = 1024;

fn truncate_reason(reason: &str) -> String {
    // TS `reason.length` counts UTF-16 code units (schema-error.ts:10-13).
    let units: Vec<u16> = reason.encode_utf16().collect();
    if units.len() <= REASON_LIMIT {
        return reason.to_string();
    }
    let prefix = String::from_utf16_lossy(&units[..REASON_LIMIT]).to_string();
    let extra = units.len() - REASON_LIMIT;
    format!("{prefix}\u{2026} ({extra} more chars)")
}

/// `NamedError.Unknown` defect envelope (`middleware/error.ts:29-41`).
pub fn defect_response() -> Response {
    let reference = format!("err_{}", &Uuid::new_v4().simple().to_string()[..8]);
    eprintln!("[{reference}] unexpected server error");
    tracing::error!(reference = %reference, "failed: unexpected server error");
    let body = named_body(
        "UnknownError",
        vec![
            (
                "message",
                str_field("Unexpected server error. Check server logs for details."),
            ),
            ("ref", str_field(&reference)),
        ],
    );
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .expect("static response parts are valid")
}

/// Config-error serialization: TS `SchemaIssue` wire order is
/// `{message, path}` (`core/v1/config/error.ts:8-12`).
fn issue_body(issue: &SchemaIssue) -> String {
    object(&[
        ("message", str_field(&issue.message)),
        ("path", serde_json::to_string(&issue.path).unwrap()),
    ])
}

/// A handler failure: either a typed API error (renders its own envelope) or a
/// core defect routed through the error middleware.
#[derive(Debug)]
pub enum ServerError {
    Api(ApiError),
    Core(CoreError),
}

impl From<ApiError> for ServerError {
    fn from(err: ApiError) -> Self {
        ServerError::Api(err)
    }
}

impl From<CoreError> for ServerError {
    fn from(err: CoreError) -> Self {
        ServerError::Core(err)
    }
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        match self {
            ServerError::Api(err) => err.into_response(),
            ServerError::Core(err) => core_error_response(&err),
        }
    }
}

/// The `ConfigErrorV1` branch of the error middleware
/// (`middleware/error.ts:19-27`): config JSON/schema failures map to a 400
/// with their `toObject()` shape; every other core error is a defect-500.
fn core_error_response(err: &CoreError) -> Response {
    let status = StatusCode::BAD_REQUEST;
    let body = match err {
        CoreError::Jsonc { path, message, .. } => named_body(
            "ConfigJsonError",
            vec![
                ("path", str_field(&path.display().to_string())),
                ("message", str_field(message)),
            ],
        ),
        CoreError::ConfigInvalid {
            path,
            message,
            issues,
        } => {
            let mut data = vec![("path", str_field(&path.display().to_string()))];
            if !issues.is_empty() {
                data.push((
                    "issues",
                    format!(
                        "[{}]",
                        issues.iter().map(issue_body).collect::<Vec<_>>().join(",")
                    ),
                ));
            }
            if let Some(message) = message {
                data.push(("message", str_field(message)));
            }
            named_body("ConfigInvalidError", data)
        }
        _ => return defect_response(),
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .expect("static response parts are valid")
}

/// The `errorLayer` panic boundary (`middleware/error.ts:7-43`): unexpected
/// panics surface as the defect-500 envelope with a fresh `err_` ref.
#[derive(Clone, Default)]
pub struct PanicLayer;

impl<S> tower::Layer<S> for PanicLayer {
    type Service = PanicService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        PanicService { inner }
    }
}

#[derive(Clone)]
pub struct PanicService<S> {
    inner: S,
}

impl<S> tower::Service<axum::http::Request<axum::body::Body>> for PanicService<S>
where
    S: tower::Service<axum::http::Request<axum::body::Body>, Response = Response<axum::body::Body>>
        + Clone
        + Send
        + 'static,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<axum::body::Body>) -> Self::Future {
        let inner = self.inner.clone();
        Box::pin(async move {
            let mut inner = inner;
            let future = std::panic::AssertUnwindSafe(inner.call(req)).catch_unwind();
            match future.await {
                Ok(result) => result,
                Err(payload) => {
                    // The panic message feeds stderr (and carries the defect
                    // ref), mirroring the TS errorLayer's cause log
                    // (`middleware/error.ts:7-43`).
                    let message = payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "non-string panic payload".to_string());
                    eprintln!("panic caught in request handler: {message}");
                    Ok(defect_response())
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_string(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn tagged_envelope_goldens() {
        let err = ApiError::SessionBusy {
            session_id: "ses_123".into(),
            message: "Session is busy: ses_123".into(),
        };
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            body_string(response).await, "{\"_tag\":\"SessionBusyError\",\"sessionID\":\"ses_123\",\"message\":\"Session is busy: ses_123\"}");

        let err = ApiError::InvalidRequest {
            message: "bad".into(),
            kind: Some("Query".into()),
            field: None,
        };
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_string(response).await,
            "{\"_tag\":\"InvalidRequestError\",\"message\":\"bad\",\"kind\":\"Query\"}"
        );

        let err = ApiError::InvalidCursor {
            message: "Invalid cursor".into(),
        };
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_string(response).await,
            "{\"_tag\":\"InvalidCursorError\",\"message\":\"Invalid cursor\"}"
        );
    }

    #[tokio::test]
    async fn optional_fields_omit_to_json_null() {
        let err = ApiError::InvalidRequest {
            message: "bad".into(),
            kind: None,
            field: None,
        };
        let response = err.into_response();
        assert_eq!(
            body_string(response).await,
            "{\"_tag\":\"InvalidRequestError\",\"message\":\"bad\"}"
        );
    }

    #[tokio::test]
    async fn named_envelope_goldens() {
        let response = ApiError::not_found("Session not found: ses_x").into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_string(response).await,
            "{\"name\":\"NotFoundError\",\"data\":{\"message\":\"Session not found: ses_x\"}}"
        );

        let response =
            ApiError::bad_request_schema("Expected string, got number", "Query").into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_string(response).await,
            "{\"name\":\"BadRequest\",\"data\":{\"message\":\"Expected string, got number\",\"kind\":\"Query\"}}"
        );
    }

    #[tokio::test]
    async fn model_not_found_includes_suggestions_array() {
        let response = ApiError::ModelNotFound {
            provider_id: "test".into(),
            model_id: "m1".into(),
            suggestions: vec!["a".into(), "b".into()],
            message: "nope".into(),
        }
        .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_string(response).await,
            "{\"_tag\":\"ModelNotFoundError\",\"providerID\":\"test\",\"modelID\":\"m1\",\"suggestions\":[\"a\",\"b\"],\"message\":\"nope\"}"
        );
    }

    #[tokio::test]
    async fn defect_envelope_shape() {
        let response = defect_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_string(response).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["name"], "UnknownError");
        assert_eq!(
            parsed["data"]["message"],
            "Unexpected server error. Check server logs for details."
        );
        let reference = parsed["data"]["ref"].as_str().unwrap();
        assert!(reference.starts_with("err_"));
        assert_eq!(reference.len(), 12);
        assert!(
            reference[4..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "ref must be err_ + 8 lowercase hex chars, got {reference}"
        );
    }

    #[test]
    fn core_error_config_mapping() {
        let err = CoreError::Jsonc {
            path: "/tmp/config.json".into(),
            line: Some(2),
            column: Some(3),
            message: "bad json".into(),
        };
        let response = core_error_response(&err);
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn core_error_config_mapping_body() {
        let err = CoreError::ConfigInvalid {
            path: "/tmp/config.json".into(),
            message: Some("invalid".into()),
            issues: vec![SchemaIssue {
                path: vec!["model".into()],
                message: "Expected string".into(),
            }],
        };
        let response = ServerError::Core(err).into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_string(response).await,
            "{\"name\":\"ConfigInvalidError\",\"data\":{\"path\":\"/tmp/config.json\",\"issues\":[{\"message\":\"Expected string\",\"path\":[\"model\"]}],\"message\":\"invalid\"}}"
        );
    }

    #[tokio::test]
    async fn non_config_core_error_is_defect_500() {
        let err = CoreError::Catalog("boom".into());
        let response = ServerError::Core(err).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn truncate_reason_cap() {
        let reason = "x".repeat(1024 + 10);
        let truncated = truncate_reason(&reason);
        assert!(truncated.contains('…'));
        assert!(truncated.contains("(10 more chars)"));
        assert_eq!(truncate_reason("short"), "short");
    }

    #[tokio::test]
    async fn every_variant_carries_its_wire_name_and_status() {
        let errors: Vec<(ApiError, &str, StatusCode)> = vec![
            (
                ApiError::InvalidRequest {
                    message: "m".into(),
                    kind: None,
                    field: None,
                },
                "InvalidRequestError",
                StatusCode::BAD_REQUEST,
            ),
            (
                ApiError::Unauthorized {
                    message: "m".into(),
                },
                "UnauthorizedError",
                StatusCode::UNAUTHORIZED,
            ),
            (
                ApiError::Forbidden {
                    message: "m".into(),
                },
                "ForbiddenError",
                StatusCode::FORBIDDEN,
            ),
            (
                ApiError::Conflict {
                    message: "m".into(),
                    resource: None,
                },
                "ConflictError",
                StatusCode::CONFLICT,
            ),
            (
                ApiError::Upstream {
                    message: "m".into(),
                    service: None,
                    status: None,
                },
                "UpstreamError",
                StatusCode::BAD_GATEWAY,
            ),
            (
                ApiError::ServiceUnavailable {
                    message: "m".into(),
                    service: None,
                },
                "ServiceUnavailableError",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ApiError::Timeout {
                    message: "m".into(),
                    operation: None,
                },
                "TimeoutError",
                StatusCode::GATEWAY_TIMEOUT,
            ),
            (
                ApiError::Unknown {
                    message: "m".into(),
                    reference: None,
                },
                "UnknownError",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                ApiError::ProviderNotFound {
                    provider_id: "p".into(),
                    message: "m".into(),
                },
                "ProviderNotFoundError",
                StatusCode::NOT_FOUND,
            ),
            (
                ApiError::ModelNotFound {
                    provider_id: "p".into(),
                    model_id: "m2".into(),
                    suggestions: Vec::new(),
                    message: "m".into(),
                },
                "ModelNotFoundError",
                StatusCode::NOT_FOUND,
            ),
            (
                ApiError::SessionNotFound {
                    session_id: "s".into(),
                    message: "m".into(),
                },
                "SessionNotFoundError",
                StatusCode::NOT_FOUND,
            ),
            (
                ApiError::MessageNotFound {
                    session_id: "s".into(),
                    message_id: "msg".into(),
                    message: "m".into(),
                },
                "MessageNotFoundError",
                StatusCode::NOT_FOUND,
            ),
            (
                ApiError::InvalidCursor {
                    message: "m".into(),
                },
                "InvalidCursorError",
                StatusCode::BAD_REQUEST,
            ),
            (
                ApiError::SessionBusy {
                    session_id: "s".into(),
                    message: "m".into(),
                },
                "SessionBusyError",
                StatusCode::CONFLICT,
            ),
            (
                ApiError::QuestionNotFound {
                    request_id: "r".into(),
                    message: "m".into(),
                },
                "QuestionNotFoundError",
                StatusCode::NOT_FOUND,
            ),
            (
                ApiError::PermissionNotFound {
                    request_id: "r".into(),
                    message: "m".into(),
                },
                "PermissionNotFoundError",
                StatusCode::NOT_FOUND,
            ),
            (
                ApiError::McpServerNotFound {
                    name: "n".into(),
                    message: "m".into(),
                },
                "McpServerNotFoundError",
                StatusCode::NOT_FOUND,
            ),
            (
                ApiError::PtyNotFound {
                    pty_id: "p".into(),
                    message: "m".into(),
                },
                "PtyNotFoundError",
                StatusCode::NOT_FOUND,
            ),
            (
                ApiError::PtyForbidden {
                    message: "m".into(),
                },
                "PtyForbiddenError",
                StatusCode::FORBIDDEN,
            ),
            (
                ApiError::ProjectNotFound {
                    project_id: "p".into(),
                    message: "m".into(),
                },
                "ProjectNotFoundError",
                StatusCode::NOT_FOUND,
            ),
            (
                ApiError::not_found("m"),
                "NotFoundError",
                StatusCode::NOT_FOUND,
            ),
            (
                ApiError::BadRequest {
                    message: "m".into(),
                    kind: None,
                },
                "BadRequest",
                StatusCode::BAD_REQUEST,
            ),
        ];
        for (error, name, status) in errors {
            let response = error.into_response();
            assert_eq!(response.status(), status, "{name} status");
            assert_eq!(response.headers()["content-type"], "application/json");
            let body = body_string(response).await;
            let key = if name == "NotFoundError" || name == "BadRequest" {
                "name"
            } else {
                "_tag"
            };
            let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(parsed[key], *name, "{name} body: {body}");
        }
    }

    #[tokio::test]
    async fn panic_layer_catches_handler_panics() {
        use tower::{Layer, ServiceExt};
        async fn panicking(
            _: axum::http::Request<axum::body::Body>,
        ) -> Result<axum::response::Response, std::convert::Infallible> {
            panic!("boom");
        }
        let service = PanicLayer.layer(tower::service_fn(panicking));
        let response = service
            .oneshot(
                axum::http::Request::builder()
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_string(response).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["name"], "UnknownError");
    }
}
