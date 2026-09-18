//! v1 session family (M6.5) — port of `httpapi/handlers/session.ts` and
//! its group definitions (`groups/session.ts`) over the M5 services and
//! the per-directory [`SessionEngine`] seam.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use opencode_core::session::event_definitions::SESSION_ERROR;
use opencode_core::session::ids::{MessageId, PartId};
use opencode_core::session::message::cursor;
use opencode_core::session::prompt::{CommandFilePart, CommandInput, ShellInput};
use opencode_core::session::prompt_input::{PromptInput, PromptPartInput};
use opencode_core::session::revert::RevertInput;
use opencode_core::{
    session, Clock, CoreError, ListInput, PublishOptions, SessionContext, SessionError,
};
use opencode_schema::permission_v1::{
    PermissionV1Reply, PermissionV1ReplyInput, PermissionV1Ruleset,
};
use opencode_schema::session_todo::TodoInfo;
use opencode_schema::session_v1::{
    OutputFormat, TextPartTime, V1Message, V1Part, V1SessionInfo, V1SessionModel, V1UserModel,
};

use crate::error::{ApiError, ServerError};
use crate::middleware::auth::query_param;
use crate::middleware::location::LocationContext;
use crate::state::{ServerContext, SessionEngine};

// ---------------------------------------------------------------------------
// wire payloads (groups/session.ts)
// ---------------------------------------------------------------------------

type JsonMap = serde_json::Map<String, Value>;

#[derive(Deserialize)]
struct CreatePayload {
    #[serde(rename = "parentID", default)]
    parent_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<V1SessionModel>,
    #[serde(default)]
    metadata: Option<JsonMap>,
    #[serde(default)]
    permission: Option<PermissionV1Ruleset>,
    #[serde(rename = "workspaceID", default)]
    workspace_id: Option<String>,
}

#[derive(Deserialize)]
struct UpdatePayload {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    metadata: Option<JsonMap>,
    #[serde(default)]
    permission: Option<PermissionV1Ruleset>,
    #[serde(default)]
    time: Option<UpdateTime>,
}

#[derive(Deserialize)]
struct UpdateTime {
    #[serde(default)]
    archived: Option<u64>,
}

#[derive(Deserialize, Default)]
struct ForkPayload {
    #[serde(rename = "messageID", default)]
    message_id: Option<String>,
}

#[derive(Deserialize)]
struct InitPayload {
    #[serde(rename = "modelID")]
    model_id: String,
    #[serde(rename = "providerID")]
    provider_id: String,
    #[serde(rename = "messageID")]
    message_id: String,
}

#[derive(Deserialize)]
struct SummarizePayload {
    #[serde(rename = "providerID")]
    provider_id: String,
    #[serde(rename = "modelID")]
    model_id: String,
    #[serde(default)]
    auto: Option<bool>,
}

#[derive(Deserialize)]
struct RevertPayload {
    #[serde(rename = "messageID")]
    message_id: String,
    #[serde(rename = "partID", default)]
    part_id: Option<String>,
}

#[derive(Deserialize)]
struct PermissionResponsePayload {
    response: PermissionV1Reply,
}

#[derive(Deserialize)]
struct WireModelRef {
    #[serde(rename = "providerID")]
    provider_id: String,
    #[serde(rename = "modelID")]
    model_id: String,
}

/// `PromptInput["parts"][number]` — `TextPartInput` | `FilePartInput` |
/// `AgentPartInput` | `SubtaskPartInput`
/// (`packages/schema/src/v1/session.ts:397-451`).
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WirePartInput {
    Text {
        #[serde(default)]
        id: Option<String>,
        text: String,
        #[serde(default)]
        synthetic: Option<bool>,
        #[serde(default)]
        ignored: Option<bool>,
        #[serde(default)]
        time: Option<TextPartTime>,
        #[serde(default)]
        metadata: Option<JsonMap>,
    },
    File {
        #[serde(default)]
        id: Option<String>,
        mime: String,
        #[serde(default)]
        filename: Option<String>,
        url: String,
        #[serde(default)]
        source: Option<opencode_schema::session_v1::V1FilePartSource>,
    },
    Agent {
        #[serde(default)]
        id: Option<String>,
        name: String,
        #[serde(default)]
        source: Option<opencode_schema::session_v1::AgentPartSource>,
    },
    Subtask {
        #[serde(default)]
        id: Option<String>,
        prompt: String,
        description: String,
        agent: String,
        #[serde(default)]
        model: Option<opencode_schema::session_v1::V1SubtaskModel>,
        #[serde(default)]
        command: Option<String>,
    },
}

impl WirePartInput {
    fn into_part(self) -> PromptPartInput {
        match self {
            WirePartInput::Text {
                id,
                text,
                synthetic,
                ignored,
                time,
                metadata,
            } => PromptPartInput::Text {
                id,
                text,
                synthetic,
                ignored,
                time,
                metadata,
            },
            WirePartInput::File {
                id,
                mime,
                filename,
                url,
                source,
            } => PromptPartInput::File {
                id,
                mime,
                filename,
                url,
                source,
            },
            WirePartInput::Agent { id, name, source } => {
                PromptPartInput::Agent { id, name, source }
            }
            WirePartInput::Subtask {
                id,
                prompt,
                description,
                agent,
                model,
                command,
            } => PromptPartInput::Subtask {
                id,
                prompt,
                description,
                agent,
                model,
                command,
            },
        }
    }
}

#[derive(Deserialize)]
struct PromptPayload {
    #[serde(rename = "messageID", default)]
    message_id: Option<String>,
    #[serde(default)]
    model: Option<WireModelRef>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(rename = "noReply", default)]
    no_reply: Option<bool>,
    #[serde(default)]
    tools: Option<BTreeMap<String, bool>>,
    #[serde(default)]
    format: Option<OutputFormat>,
    #[serde(default)]
    system: Option<String>,
    #[serde(default)]
    variant: Option<String>,
    parts: Vec<WirePartInput>,
}

#[derive(Deserialize)]
struct WireCommandFilePart {
    mime: String,
    #[serde(default)]
    filename: Option<String>,
    url: String,
}

#[derive(Deserialize)]
struct CommandPayload {
    #[serde(rename = "messageID", default)]
    message_id: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    variant: Option<String>,
    arguments: String,
    command: String,
    #[serde(default)]
    parts: Vec<WireCommandFilePart>,
}

#[derive(Deserialize)]
struct ShellPayload {
    #[serde(rename = "messageID", default)]
    message_id: Option<String>,
    agent: String,
    #[serde(default)]
    model: Option<WireModelRef>,
    command: String,
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn json_ok(value: impl serde::Serialize) -> Response {
    let body = serde_json::to_string(&value).expect("serialization cannot fail");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("static response parts are valid")
}

/// `HttpApiError.BadRequest` — an empty 400 body
/// (`HttpApiError.ts:41-51`).
fn bad_request_empty() -> Response {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Body::empty())
        .expect("static response parts are valid")
}

/// `HttpApiError.InternalServerError` — an empty 500 body.
fn internal_error_empty() -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::empty())
        .expect("static response parts are valid")
}

fn defect(err: impl std::fmt::Display) -> ServerError {
    ServerError::Core(CoreError::Storage(err.to_string()))
}

/// `mapStorageNotFound` (`handlers/session-errors.ts:6-8`).
fn session_error(err: SessionError) -> ServerError {
    match err {
        SessionError::NotFound(e) => ApiError::not_found(e.message).into(),
        other => defect(other),
    }
}

fn require_session(
    location: &LocationContext,
    session_id: &str,
) -> Result<V1SessionInfo, ServerError> {
    location
        .services
        .sessions
        .get(session_id)
        .map_err(session_error)
}

/// `mapBusy` (`handlers/session-errors.ts:10-20`).
fn busy_error(session_id: &str) -> ApiError {
    ApiError::SessionBusy {
        session_id: session_id.to_string(),
        message: format!("Session is busy: {session_id}"),
    }
}

fn engine(
    ctx: &ServerContext,
    location: &LocationContext,
) -> Result<Arc<dyn SessionEngine>, ServerError> {
    (ctx.engine_factory)(location)
}

fn query_error(message: impl Into<String>) -> ServerError {
    ApiError::bad_request_schema(message, "Query").into()
}

fn payload_error(message: impl Into<String>) -> ServerError {
    ApiError::bad_request_schema(message, "Payload").into()
}

fn query_i64(uri: &Uri, name: &str) -> Result<Option<i64>, ServerError> {
    match query_param(uri.query(), name) {
        Some(value) => value
            .parse::<i64>()
            .map(Some)
            .map_err(|_| query_error(format!("Expected a number, got {value:?}"))),
        None => Ok(None),
    }
}

fn query_bool(uri: &Uri, name: &str) -> Result<bool, ServerError> {
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
fn require_param(prefix: &str, value: &str) -> Result<(), ServerError> {
    if value.starts_with(prefix) {
        return Ok(());
    }
    Err(ApiError::bad_request_schema(
        format!("Expected a string starting with {prefix:?}, got {value:?}"),
        "Params",
    )
    .into())
}

fn require_payload_id(prefix: &str, value: &str) -> Result<(), ServerError> {
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
/// through the schema-error middleware shape
/// (`middleware/schema-error.ts:28-41`).
fn parse_payload<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, ServerError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|err| payload_error(format!("Invalid JSON payload: {err}")))?;
    serde_json::from_value(value).map_err(|err| payload_error(err.to_string()))
}

/// The empty-body JSON parse for the raw handlers — invalid JSON is a bare
/// 400 (`tryParseJson`, handlers/session.ts:42-46).
fn try_parse_json(body: &Bytes) -> Result<Value, ()> {
    serde_json::from_slice(body).map_err(|_| ())
}

/// `InstanceState.context` — project/directory/worktree/workspace resolved
/// for the instance (M6: worktree == directory; TODO(M7) git worktrees).
fn session_ctx(location: &LocationContext) -> Result<SessionContext, ServerError> {
    let project_id = location
        .services
        .sessions
        .ensure_project(&location.directory)
        .map_err(session_error)?;
    Ok(SessionContext {
        project_id,
        directory: location.directory.clone(),
        worktree: location.directory.clone(),
        workspace_id: location.workspace_id.clone(),
    })
}

fn with_parts_json(message: &session::WithParts) -> Value {
    json!({
        "info": serde_json::to_value(&message.info).expect("message serializes"),
        "parts": serde_json::to_value(&message.parts).expect("parts serialize"),
    })
}

// ---------------------------------------------------------------------------
// route registration
// ---------------------------------------------------------------------------

pub fn register(
    router: Router<Arc<ServerContext>>,
    method: &str,
    path: &'static str,
) -> (Router<Arc<ServerContext>>, bool) {
    let router = match (method, path) {
        ("GET", "/session") => router.route(path, get(list)),
        ("POST", "/session") => router.route(path, post(create)),
        ("GET", "/session/status") => router.route(path, get(status)),
        ("GET", "/session/{sessionID}") => router.route(path, get(get_session)),
        ("DELETE", "/session/{sessionID}") => router.route(path, delete(remove)),
        ("PATCH", "/session/{sessionID}") => router.route(path, patch(update)),
        ("GET", "/session/{sessionID}/children") => router.route(path, get(children)),
        ("GET", "/session/{sessionID}/todo") => router.route(path, get(todo)),
        ("GET", "/session/{sessionID}/diff") => router.route(path, get(diff)),
        ("GET", "/session/{sessionID}/message") => router.route(path, get(messages)),
        ("POST", "/session/{sessionID}/message") => router.route(path, post(prompt)),
        ("GET", "/session/{sessionID}/message/{messageID}") => router.route(path, get(message)),
        ("DELETE", "/session/{sessionID}/message/{messageID}") => {
            router.route(path, delete(delete_message))
        }
        ("POST", "/session/{sessionID}/fork") => router.route(path, post(fork)),
        ("POST", "/session/{sessionID}/abort") => router.route(path, post(abort)),
        ("POST", "/session/{sessionID}/init") => router.route(path, post(init)),
        ("POST", "/session/{sessionID}/share") => router.route(path, post(share)),
        ("DELETE", "/session/{sessionID}/share") => router.route(path, delete(unshare)),
        ("POST", "/session/{sessionID}/summarize") => router.route(path, post(summarize)),
        ("POST", "/session/{sessionID}/prompt_async") => router.route(path, post(prompt_async)),
        ("POST", "/session/{sessionID}/command") => router.route(path, post(command)),
        ("POST", "/session/{sessionID}/shell") => router.route(path, post(shell)),
        ("POST", "/session/{sessionID}/revert") => router.route(path, post(revert)),
        ("POST", "/session/{sessionID}/unrevert") => router.route(path, post(unrevert)),
        ("POST", "/session/{sessionID}/permissions/{permissionID}") => {
            router.route(path, post(permission_respond))
        }
        ("DELETE", "/session/{sessionID}/message/{messageID}/part/{partID}") => {
            router.route(path, delete(delete_part))
        }
        ("PATCH", "/session/{sessionID}/message/{messageID}/part/{partID}") => {
            router.route(path, patch(update_part))
        }
        _ => return (router, false),
    };
    (router, true)
}

// ---------------------------------------------------------------------------
// handlers (handlers/session.ts)
// ---------------------------------------------------------------------------

/// `list` (`handlers/session.ts:64-75`).
pub async fn list(
    axum::Extension(location): axum::Extension<LocationContext>,
    uri: Uri,
) -> Result<Response, ServerError> {
    let scope = query_param(uri.query(), "scope");
    if let Some(scope) = &scope {
        if scope != "project" {
            return Err(query_error(format!("Expected \"project\", got {scope:?}")));
        }
    }
    let path = query_param(uri.query(), "path");
    let roots = query_bool(&uri, "roots")?;
    let start = query_i64(&uri, "start")?;
    let search = query_param(uri.query(), "search");
    let limit = query_i64(&uri, "limit")?;
    let directory_param =
        matches!(query_param(uri.query(), "directory").as_deref(), Some(d) if !d.is_empty());

    let ctx = session_ctx(&location)?;
    let directory = if scope.as_deref() == Some("project") {
        None
    } else if directory_param {
        Some(location.directory.to_string_lossy().into_owned())
    } else {
        None
    };
    let input = ListInput {
        directory,
        scope_project: scope.as_deref() == Some("project"),
        path,
        workspace_id: None,
        roots,
        start,
        search,
        limit,
    };
    let items = location
        .services
        .sessions
        .list(&ctx, &input)
        .map_err(session_error)?;
    Ok(json_ok(items))
}

/// `status` (`handlers/session.ts:77-79`) — `Object.fromEntries` of the
/// status list.
pub async fn status(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let list = location.services.status.list();
    let mut map = serde_json::Map::new();
    for (session_id, info) in list {
        map.insert(
            session_id,
            serde_json::to_value(info).expect("status serializes"),
        );
    }
    Ok(json_ok(Value::Object(map)))
}

/// `get` (`handlers/session.ts:85-87`).
pub async fn get_session(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    require_session(&location, &session_id).map(json_ok)
}

/// `children` (`handlers/session.ts:89-92`).
pub async fn children(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    require_session(&location, &session_id)?;
    let children = location
        .services
        .sessions
        .children(&session_id)
        .map_err(session_error)?;
    Ok(json_ok(children))
}

/// `todo` (`handlers/session.ts:94-97`) — `Todo.Service.get`
/// (`session/todo.ts:25-42`).
pub async fn todo(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    require_session(&location, &session_id)?;
    let rows = location
        .services
        .storage
        .list_todos(&session_id)
        .map_err(defect)?;
    let todos: Vec<TodoInfo> = rows
        .into_iter()
        .map(|row| TodoInfo {
            content: row.content,
            status: row.status,
            priority: row.priority,
        })
        .collect();
    Ok(json_ok(todos))
}

/// `diff` (`handlers/session.ts:99-104`).
pub async fn diff(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    uri: Uri,
) -> Result<Response, ServerError> {
    let engine = engine(&ctx, &location)?;
    let message_id = query_param(uri.query(), "messageID");
    let diffs = engine
        .diff(&session_id, message_id.as_deref())
        .map_err(session_error)?;
    Ok(json_ok(diffs))
}

/// `messages` (`handlers/session.ts:106-145`).
pub async fn messages(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, ServerError> {
    let before = query_param(uri.query(), "before");
    let limit = match query_param(uri.query(), "limit") {
        Some(raw) => Some(
            raw.parse::<usize>()
                .map_err(|_| query_error(format!("Expected a number, got {raw:?}")))?,
        ),
        None => None,
    };
    if before.is_some() && limit.is_none() {
        return Ok(bad_request_empty());
    }
    if let Some(before) = &before {
        if cursor::decode(before).is_err() {
            return Ok(bad_request_empty());
        }
    }
    require_session(&location, &session_id)?;
    match limit {
        None | Some(0) => {
            let items = location
                .services
                .sessions
                .messages(&session_id, None)
                .map_err(session_error)?;
            Ok(json_ok(
                items.iter().map(with_parts_json).collect::<Vec<_>>(),
            ))
        }
        Some(limit) => {
            let page = location
                .services
                .messages
                .page(&session_id, limit, before.as_deref())
                .map_err(session_error)?;
            match page.cursor {
                Some(cursor) => Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Access-Control-Expose-Headers", "Link, X-Next-Cursor")
                    .header(
                        header::LINK,
                        format!(
                            "<{}>; rel=\"next\"",
                            next_link_url(&headers, &uri, limit, &cursor)
                        ),
                    )
                    .header("X-Next-Cursor", cursor)
                    .body(Body::from(
                        serde_json::to_string(
                            &page.items.iter().map(with_parts_json).collect::<Vec<_>>(),
                        )
                        .expect("serialization cannot fail"),
                    ))
                    .expect("static response parts are valid")),
                None => Ok(json_ok(
                    page.items.iter().map(with_parts_json).collect::<Vec<_>>(),
                )),
            }
        }
    }
}

/// `message` (`handlers/session.ts:147-153`).
pub async fn message(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path((session_id, message_id)): Path<(String, String)>,
) -> Result<Response, ServerError> {
    require_param("msg", &message_id)?;
    let message = location
        .services
        .messages
        .get(&session_id, &message_id)
        .map_err(session_error)?;
    Ok(json_ok(with_parts_json(&message)))
}

/// `createRaw` (`handlers/session.ts:155-176`) — empty body = defaults,
/// invalid JSON or schema = 400.
pub async fn create(
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: Option<CreatePayload> = if String::from_utf8_lossy(&body).trim().is_empty() {
        None
    } else {
        let payload = match try_parse_json(&body) {
            Ok(json) => json,
            Err(()) => return Ok(bad_request_empty()),
        };
        if payload.is_null() {
            None
        } else {
            match serde_json::from_value::<CreatePayload>(payload) {
                Ok(payload) => Some(payload),
                Err(_) => return Ok(bad_request_empty()),
            }
        }
    };
    let input = match payload.map(|p| decode_create_payload(&p)) {
        Some(Ok(input)) => Some(input),
        Some(Err(())) => return Ok(bad_request_empty()),
        None => None,
    }
    .unwrap_or_default();
    let ctx = session_ctx(&location)?;
    let info = location
        .services
        .sessions
        .create(&ctx, &input)
        .map_err(session_error)?;
    // TODO(M7): `SessionShare.create`'s auto-share fork
    // (share/session.ts:43-51) — the share service is engine-scoped.
    Ok(json_ok(info))
}

fn decode_create_payload(payload: &CreatePayload) -> Result<session::CreateInput, ()> {
    if let Some(id) = &payload.parent_id {
        if !id.starts_with("ses") {
            return Err(());
        }
    }
    if let Some(id) = &payload.workspace_id {
        if !id.starts_with("wrk") {
            return Err(());
        }
    }
    Ok(session::CreateInput {
        parent_id: payload.parent_id.clone(),
        title: payload.title.clone(),
        agent: payload.agent.clone(),
        model: payload.model.clone(),
        metadata: payload.metadata.clone(),
        permission: payload.permission.clone(),
        workspace_id: payload.workspace_id.clone(),
        ..Default::default()
    })
}

/// `remove` (`handlers/session.ts:178-181`).
pub async fn remove(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    location
        .services
        .sessions
        .remove(&session_id)
        .map_err(session_error)?;
    Ok(json_ok(true))
}

/// `update` (`handlers/session.ts:183-204`).
pub async fn update(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: UpdatePayload = parse_payload(&body)?;
    let current = require_session(&location, &session_id)?;
    if let Some(title) = &payload.title {
        location
            .services
            .sessions
            .set_title(&session_id, title)
            .map_err(session_error)?;
    }
    if let Some(metadata) = payload.metadata {
        location
            .services
            .sessions
            .set_metadata(&session_id, metadata)
            .map_err(session_error)?;
    }
    if let Some(permission) = &payload.permission {
        let merged: PermissionV1Ruleset = current
            .permission
            .clone()
            .unwrap_or_default()
            .into_iter()
            .chain(permission.iter().cloned())
            .collect();
        location
            .services
            .sessions
            .set_permission(&session_id, merged)
            .map_err(session_error)?;
    }
    if let Some(archived) = payload.time.and_then(|time| time.archived) {
        location
            .services
            .sessions
            .set_archived(&session_id, Some(archived))
            .map_err(session_error)?;
    }
    let updated = require_session(&location, &session_id)?;
    Ok(json_ok(updated))
}

/// `forkRaw` (`handlers/session.ts:206-230`).
pub async fn fork(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload = if String::from_utf8_lossy(&body).trim().is_empty() {
        ForkPayload::default()
    } else {
        let parsed = match try_parse_json(&body) {
            Ok(json) => json,
            Err(()) => return Ok(bad_request_empty()),
        };
        match serde_json::from_value::<ForkPayload>(parsed) {
            Ok(payload) => payload,
            Err(_) => return Ok(bad_request_empty()),
        }
    };
    if let Some(message_id) = &payload.message_id {
        if !message_id.starts_with("msg") {
            return Ok(bad_request_empty());
        }
    }
    let ctx = session_ctx(&location)?;
    let info = location
        .services
        .sessions
        .fork(&ctx, &session_id, payload.message_id.as_deref())
        .map_err(session_error)?;
    Ok(json_ok(info))
}

/// `abort` (`handlers/session.ts:232-235`).
pub async fn abort(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    location
        .services
        .run_state
        .cancel(&session_id)
        .await
        .map_err(defect)?;
    Ok(json_ok(true))
}

/// `init` (`handlers/session.ts:237-252`) — the `init` command
/// (`Command.Default.INIT`, command/index.ts:46).
pub async fn init(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: InitPayload = parse_payload(&body)?;
    require_payload_id("msg", &payload.message_id)?;
    require_session(&location, &session_id)?;
    let engine = engine(&ctx, &location)?;
    match engine
        .command(CommandInput {
            session_id: session_id.clone(),
            message_id: Some(payload.message_id),
            agent: None,
            model: Some(format!("{}/{}", payload.provider_id, payload.model_id)),
            variant: None,
            arguments: String::new(),
            command: "init".to_string(),
            parts: Vec::new(),
        })
        .await
    {
        Ok(_) => Ok(json_ok(true)),
        Err(_) => Ok(bad_request_empty()),
    }
}

/// `share` (`handlers/session.ts:259-263`).
pub async fn share(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    require_session(&location, &session_id)?;
    let engine = engine(&ctx, &location)?;
    if engine
        .share(&require_session(&location, &session_id)?)
        .is_err()
    {
        return Ok(internal_error_empty());
    }
    Ok(json_ok(require_session(&location, &session_id)?))
}

/// `unshare` (`handlers/session.ts:265-271`).
pub async fn unshare(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    require_session(&location, &session_id)?;
    let engine = engine(&ctx, &location)?;
    if engine.unshare(&session_id).is_err() {
        return Ok(internal_error_empty());
    }
    Ok(json_ok(require_session(&location, &session_id)?))
}

/// `summarize` (`handlers/session.ts:273-293`) — cleanup, compaction-create,
/// then the prompt loop.
pub async fn summarize(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    require_session(&location, &session_id)?;
    let payload: SummarizePayload = parse_payload(&body)?;
    let engine = engine(&ctx, &location)?;

    let session = require_session(&location, &session_id)?;
    engine.cleanup(&session).map_err(session_error)?;
    let messages = location
        .services
        .sessions
        .messages(&session_id, None)
        .map_err(session_error)?;
    let default_agent = location.services.agents.default_info().map_err(defect)?;
    let current_agent = messages
        .iter()
        .rev()
        .find(|message| matches!(message.info, V1Message::User { .. }))
        .and_then(|message| match &message.info {
            V1Message::User { agent, .. } => Some(agent.clone()),
            _ => None,
        })
        .unwrap_or(default_agent.name);

    let message_id = MessageId::ascending(None).map_err(defect)?;
    let now = opencode_core::catalog::SystemClock.now_ms();
    location
        .services
        .sessions
        .update_message(&V1Message::User {
            id: message_id.clone(),
            session_id: session_id.clone(),
            time: opencode_schema::session_v1::UserTime {
                created: now as f64,
            },
            format: None,
            summary: None,
            agent: current_agent,
            model: V1UserModel {
                provider_id: payload.provider_id,
                model_id: payload.model_id,
                variant: None,
            },
            system: None,
            tools: None,
        })
        .map_err(session_error)?;
    location
        .services
        .sessions
        .update_part(&V1Part::Compaction {
            id: PartId::ascending(None).map_err(defect)?,
            session_id: session_id.clone(),
            message_id,
            auto: payload.auto.unwrap_or(false),
            overflow: None,
            tail_start_id: None,
        })
        .map_err(session_error)?;

    engine
        .loop_(session_id.clone())
        .await
        .map_err(|_| defect("prompt loop failed"))?;
    Ok(json_ok(true))
}

/// `prompt` (`handlers/session.ts:295-309`) — the created message as a
/// single-body JSON stream.
pub async fn prompt(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: PromptPayload = parse_payload(&body)?;
    if let Some(message_id) = &payload.message_id {
        require_payload_id("msg", message_id)?;
    }
    require_session(&location, &session_id)?;
    let engine = engine(&ctx, &location)?;
    let message = match engine
        .prompt(PromptInput {
            session_id: session_id.clone(),
            message_id: payload.message_id,
            model: payload.model.map(|m| session::prompt_input::ModelRef {
                provider_id: m.provider_id,
                model_id: m.model_id,
            }),
            agent: payload.agent,
            no_reply: payload.no_reply,
            tools: payload.tools,
            format: payload.format,
            system: payload.system,
            variant: payload.variant,
            parts: payload
                .parts
                .into_iter()
                .map(WirePartInput::into_part)
                .collect(),
        })
        .await
    {
        Ok(message) => message,
        Err(_) => return Ok(bad_request_empty()),
    };
    Ok(json_ok(with_parts_json(&message)))
}

/// `promptAsync` (`handlers/session.ts:311-329`) — fork, log failures as
/// `session.error`, 204 No Content.
pub async fn prompt_async(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: PromptPayload = parse_payload(&body)?;
    if let Some(message_id) = &payload.message_id {
        require_payload_id("msg", message_id)?;
    }
    require_session(&location, &session_id)?;
    let engine = engine(&ctx, &location)?;
    let services = location.services.clone();
    let input = PromptInput {
        session_id: session_id.clone(),
        message_id: payload.message_id,
        model: payload.model.map(|m| session::prompt_input::ModelRef {
            provider_id: m.provider_id,
            model_id: m.model_id,
        }),
        agent: payload.agent,
        no_reply: payload.no_reply,
        tools: payload.tools,
        format: payload.format,
        system: payload.system,
        variant: payload.variant,
        parts: payload
            .parts
            .into_iter()
            .map(WirePartInput::into_part)
            .collect(),
    };
    tokio::spawn(async move {
        if let Err(cause) = engine.prompt(input).await {
            tracing::error!("prompt_async failed: {session_id}");
            let _ = services.events.publish(
                &SESSION_ERROR,
                json!({
                    "sessionID": session_id,
                    "error": { "name": "Unknown", "data": { "message": cause.to_string() } },
                }),
                PublishOptions::default(),
            );
        }
    });
    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("static response parts are valid")
        .into_response())
}

/// `command` (`handlers/session.ts:331-339`).
pub async fn command(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: CommandPayload = parse_payload(&body)?;
    if let Some(message_id) = &payload.message_id {
        require_payload_id("msg", message_id)?;
    }
    require_session(&location, &session_id)?;
    let engine = engine(&ctx, &location)?;
    let message = match engine
        .command(CommandInput {
            session_id: session_id.clone(),
            message_id: payload.message_id,
            agent: payload.agent,
            model: payload.model,
            variant: payload.variant,
            arguments: payload.arguments,
            command: payload.command,
            parts: payload
                .parts
                .into_iter()
                .map(|part| CommandFilePart {
                    mime: part.mime,
                    filename: part.filename,
                    url: part.url,
                })
                .collect(),
        })
        .await
    {
        Ok(message) => message,
        Err(_) => return Ok(bad_request_empty()),
    };
    Ok(json_ok(with_parts_json(&message)))
}

/// `shell` (`handlers/session.ts:341-347`) — Busy → 409.
pub async fn shell(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: ShellPayload = parse_payload(&body)?;
    if let Some(message_id) = &payload.message_id {
        require_payload_id("msg", message_id)?;
    }
    require_session(&location, &session_id)?;
    let engine = engine(&ctx, &location)?;
    match engine
        .shell(ShellInput {
            session_id: session_id.clone(),
            message_id: payload.message_id,
            agent: payload.agent,
            model: payload.model.map(|m| session::prompt_input::ModelRef {
                provider_id: m.provider_id,
                model_id: m.model_id,
            }),
            command: payload.command,
        })
        .await
    {
        Ok(message) => Ok(json_ok(with_parts_json(&message))),
        Err(SessionError::Busy(err)) => Err(busy_error(&err.session_id).into()),
        Err(err) => Err(session_error(err)),
    }
}

/// `revert` (`handlers/session.ts:349-355`).
pub async fn revert(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: RevertPayload = parse_payload(&body)?;
    require_payload_id("msg", &payload.message_id)?;
    require_session(&location, &session_id)?;
    let engine = engine(&ctx, &location)?;
    match engine
        .revert(RevertInput {
            session_id: session_id.clone(),
            message_id: payload.message_id,
            part_id: payload.part_id,
        })
        .await
    {
        Ok(info) => Ok(json_ok(info)),
        Err(SessionError::Busy(err)) => Err(busy_error(&err.session_id).into()),
        Err(err) => Err(session_error(err)),
    }
}

/// `unrevert` (`handlers/session.ts:357-360`).
pub async fn unrevert(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    require_session(&location, &session_id)?;
    let engine = engine(&ctx, &location)?;
    match engine.unrevert(session_id.clone()).await {
        Ok(info) => Ok(json_ok(info)),
        Err(SessionError::Busy(err)) => Err(busy_error(&err.session_id).into()),
        Err(err) => Err(session_error(err)),
    }
}

/// `permissionRespond` (`handlers/session.ts:362-378`) — deprecated.
pub async fn permission_respond(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path((session_id, permission_id)): Path<(String, String)>,
    body: Bytes,
) -> Result<Response, ServerError> {
    require_param("per", &permission_id)?;
    require_session(&location, &session_id)?;
    let payload: PermissionResponsePayload = parse_payload(&body)?;
    match location.services.permission.reply(PermissionV1ReplyInput {
        request_id: permission_id,
        reply: payload.response,
        message: None,
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

/// `deleteMessage` (`handlers/session.ts:380-387`).
pub async fn delete_message(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path((session_id, message_id)): Path<(String, String)>,
) -> Result<Response, ServerError> {
    require_param("msg", &message_id)?;
    require_session(&location, &session_id)?;
    if let Err(err) = location.services.run_state.assert_not_busy(&session_id) {
        return Err(busy_error(&err.session_id).into());
    }
    location
        .services
        .sessions
        .remove_message(&session_id, &message_id)
        .map_err(session_error)?;
    Ok(json_ok(true))
}

/// `deletePart` (`handlers/session.ts:389-395`).
pub async fn delete_part(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path((session_id, message_id, part_id)): Path<(String, String, String)>,
) -> Result<Response, ServerError> {
    require_param("msg", &message_id)?;
    require_param("prt", &part_id)?;
    require_session(&location, &session_id)?;
    location
        .services
        .sessions
        .remove_part(&session_id, &message_id, &part_id)
        .map_err(session_error)?;
    Ok(json_ok(true))
}

/// `updatePart` (`handlers/session.ts:397-411`) — body ids must match the
/// path ids.
pub async fn update_part(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path((session_id, message_id, part_id)): Path<(String, String, String)>,
    body: Bytes,
) -> Result<Response, ServerError> {
    require_param("msg", &message_id)?;
    require_param("prt", &part_id)?;
    require_session(&location, &session_id)?;
    let part: V1Part = parse_payload(&body)?;
    let ids_match = match &part {
        V1Part::Text {
            id,
            session_id: s,
            message_id: m,
            ..
        }
        | V1Part::Subtask {
            id,
            session_id: s,
            message_id: m,
            ..
        }
        | V1Part::Reasoning {
            id,
            session_id: s,
            message_id: m,
            ..
        }
        | V1Part::File {
            id,
            session_id: s,
            message_id: m,
            ..
        }
        | V1Part::Tool {
            id,
            session_id: s,
            message_id: m,
            ..
        }
        | V1Part::StepStart {
            id,
            session_id: s,
            message_id: m,
            ..
        }
        | V1Part::StepFinish {
            id,
            session_id: s,
            message_id: m,
            ..
        }
        | V1Part::Snapshot {
            id,
            session_id: s,
            message_id: m,
            ..
        }
        | V1Part::Patch {
            id,
            session_id: s,
            message_id: m,
            ..
        }
        | V1Part::Agent {
            id,
            session_id: s,
            message_id: m,
            ..
        }
        | V1Part::Retry {
            id,
            session_id: s,
            message_id: m,
            ..
        }
        | V1Part::Compaction {
            id,
            session_id: s,
            message_id: m,
            ..
        } => *id == part_id && *s == session_id && *m == message_id,
    };
    if !ids_match {
        return Ok(bad_request_empty());
    }
    location
        .services
        .sessions
        .update_part(&part)
        .map_err(session_error)?;
    Ok(json_ok(part))
}

// ---------------------------------------------------------------------------
// Link header URL (`handlers/session.ts:132-144`)
// ---------------------------------------------------------------------------

/// `URLSearchParams` application/x-www-form-urlencoded serialization:
/// space → `+`; everything but `A-Za-z0-9*-._` percent-encoded.
fn form_urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b' ' => out.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

/// Percent-decode one query component (`+` → space).
fn form_urldecode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                };
                if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push(hi * 16 + lo);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `URLSearchParams.set` — keep the first occurrence's position, drop the
/// rest, append when absent.
fn url_search_params_set(pairs: &mut Vec<(String, String)>, name: &str, value: &str) {
    let mut seen = false;
    let mut out: Vec<(String, String)> = Vec::with_capacity(pairs.len());
    for (key, val) in pairs.iter() {
        if key == name {
            if !seen {
                out.push((name.to_string(), value.to_string()));
                seen = true;
            }
        } else {
            out.push((key.clone(), val.clone()));
        }
    }
    if !seen {
        out.push((name.to_string(), value.to_string()));
    }
    *pairs = out;
}

/// The `Link` header URL — `HttpServerRequest.toURL` honors the Host header
/// and `x-forwarded-proto` (fallback base `http://localhost`), then
/// `searchParams.set("limit")` / `set("before")`.
fn next_link_url(headers: &HeaderMap, uri: &Uri, limit: usize, cursor: &str) -> String {
    let scheme = match headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
    {
        Some("https") => "https",
        _ => "http",
    };
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some(query) = uri.query() {
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            pairs.push((form_urldecode(key), form_urldecode(value)));
        }
    }
    url_search_params_set(&mut pairs, "limit", &limit.to_string());
    url_search_params_set(&mut pairs, "before", cursor);
    let query = pairs
        .iter()
        .map(|(key, value)| format!("{}={}", form_urlencode(key), form_urlencode(value)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{scheme}://{host}{}?{query}", uri.path())
}
