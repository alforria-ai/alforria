//! v2 session + message families (M6.7) — port of
//! `packages/server/src/handlers/{session,message}.ts` as a thin adapter
//! over the M5 V1 engine (spec §9 M6.7 S1).
//!
//! Divergences from the TS V2 `SessionV2` service (all recorded in the
//! M6.7 report):
//!
//! * `prompt` cannot produce `Admitted` delivery semantics — the V2 runner
//!   (`SessionInput.admit` + `SessionExecution`) is M7. The handler performs
//!   every pre-check TS does (session 404, prompt decode, durable
//!   message-ID conflict 409) and then stops for review (defect-500).
//! * `revert.stage` reads the `session_message` projection; under the V1
//!   engine the table is always empty, so the 404 `MessageNotFoundError`
//!   path is the only reachable branch.
//! * `revert.clear`/`commit` no-op when no revert is staged (faithful), but
//!   stop for review when one is — snapshot restore and the
//!   `session.next.revert.*` durable events need M7 machinery.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::Value;

use alforria_core::session::store::SessionPatch;
use alforria_core::{Clock, SessionError, SessionServices};
use alforria_schema::question::QuestionAnswer;
use alforria_schema::session_v1::V1SessionInfo;

use crate::error::{ApiError, ServerError};
use crate::middleware::auth::query_param;
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

use super::util::{
    decode_sessions_cursor, encode_sessions_cursor, envelope, no_content, params_error,
    parse_payload, payload_error, query_error, session_from_v1, SessionsQuery,
};

// ---------------------------------------------------------------------------
// route registration
// ---------------------------------------------------------------------------

pub fn register(
    router: Router<Arc<ServerContext>>,
    method: &str,
    path: &'static str,
) -> (Router<Arc<ServerContext>>, bool) {
    let router = match (method, path) {
        ("GET", "/api/session") => router.route(path, get(list)),
        ("POST", "/api/session") => router.route(path, post(create)),
        ("GET", "/api/session/active") => router.route(path, get(active)),
        ("GET", "/api/session/{sessionID}") => router.route(path, get(get_session)),
        ("POST", "/api/session/{sessionID}/agent") => router.route(path, post(switch_agent)),
        ("POST", "/api/session/{sessionID}/model") => router.route(path, post(switch_model)),
        ("POST", "/api/session/{sessionID}/prompt") => router.route(path, post(prompt)),
        ("POST", "/api/session/{sessionID}/compact") => router.route(path, post(compact)),
        ("POST", "/api/session/{sessionID}/wait") => router.route(path, post(wait)),
        ("POST", "/api/session/{sessionID}/revert/stage") => router.route(path, post(revert_stage)),
        ("POST", "/api/session/{sessionID}/revert/clear") => router.route(path, post(revert_clear)),
        ("POST", "/api/session/{sessionID}/revert/commit") => {
            router.route(path, post(revert_commit))
        }
        ("GET", "/api/session/{sessionID}/context") => router.route(path, get(context)),
        ("GET", "/api/session/{sessionID}/history") => router.route(path, get(history)),
        ("POST", "/api/session/{sessionID}/interrupt") => router.route(path, post(interrupt)),
        ("GET", "/api/session/{sessionID}/message/{messageID}") => router.route(path, get(message)),
        ("GET", "/api/session/{sessionID}/message") => router.route(path, get(messages)),
        // ---- question (`handlers/question.ts`) ----
        ("GET", "/api/question/request") => router.route(path, get(question_request_list)),
        ("GET", "/api/session/{sessionID}/question") => router.route(path, get(question_list)),
        ("POST", "/api/session/{sessionID}/question/{requestID}/reply") => {
            router.route(path, post(question_reply))
        }
        ("POST", "/api/session/{sessionID}/question/{requestID}/reject") => {
            router.route(path, post(question_reject))
        }
        _ => return (router, false),
    };
    (router, true)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn defect(err: impl std::fmt::Display) -> ServerError {
    ServerError::Core(alforria_core::CoreError::Storage(err.to_string()))
}

fn session_error(err: SessionError) -> ServerError {
    match err {
        SessionError::NotFound(e) => ApiError::SessionNotFound {
            session_id: e
                .message
                .strip_prefix("Session not found: ")
                .unwrap_or(&e.message)
                .to_string(),
            message: e.message,
        }
        .into(),
        other => defect(other),
    }
}

fn json_ok(value: impl serde::Serialize) -> Response {
    let body = serde_json::to_string(&value).expect("serialization cannot fail");
    Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .expect("static response parts are valid")
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

fn now_ms() -> u64 {
    alforria_core::catalog::SystemClock.now_ms()
}

// ---------------------------------------------------------------------------
// session.list (`handlers/session.ts:14-50`)
// ---------------------------------------------------------------------------

fn parse_sessions_query(uri: &axum::http::Uri) -> Result<SessionsQuery, ServerError> {
    let q = |name: &str| query_param(uri.query(), name);
    let limit = match q("limit") {
        Some(raw) => {
            let value = raw
                .parse::<u64>()
                .map_err(|_| query_error(format!("Expected a number, got {raw:?}")))?;
            if value == 0 {
                return Err(query_error(format!(
                    "Expected a positive number, got {raw:?}"
                )));
            }
            Some(value)
        }
        None => None,
    };
    let order = match q("order") {
        Some(order) if order == "asc" || order == "desc" => Some(order),
        Some(other) => {
            return Err(query_error(format!(
                "Expected \"asc\" or \"desc\", got {other:?}"
            )))
        }
        None => None,
    };
    let directory = q("directory");
    Ok(SessionsQuery {
        workspace: q("workspace").filter(|value| !value.is_empty()),
        limit,
        order,
        search: q("search"),
        directory,
        project: q("project"),
        subpath: q("subpath"),
        cursor: q("cursor"),
    })
}

/// The V2 list (`core/src/session.ts` `SessionV2.list`): directory/project
/// filters, workspace + title search, `time_created`-ordered with an
/// exclusive anchor, limit always set (default 50).
fn list_rows(
    storage: &Arc<alforria_core::Storage>,
    query: &SessionsQuery,
    anchor: Option<&alforria_schema::session::SessionListAnchor>,
    direction: &alforria_schema::session::SessionListDirection,
    limit: i64,
) -> Result<Vec<SessionRow>, ServerError> {
    use alforria_schema::session::SessionListDirection;
    let requested = query.order.as_deref().unwrap_or("desc");
    let order = match (direction, requested) {
        (SessionListDirection::Previous, "asc") => "desc",
        (SessionListDirection::Previous, _) => "asc",
        (_, "asc") => "asc",
        _ => "desc",
    };
    let mut sql = String::from("SELECT * FROM session WHERE 1=1");
    let mut params: Vec<String> = Vec::new();
    if let Some(directory) = &query.directory {
        sql.push_str(&format!(" AND directory = ?{}", params.len() + 1));
        params.push(directory.clone());
    }
    if let Some(workspace) = &query.workspace {
        sql.push_str(&format!(" AND workspace_id = ?{}", params.len() + 1));
        params.push(workspace.clone());
    }
    if let Some(project) = &query.project {
        sql.push_str(&format!(" AND project_id = ?{}", params.len() + 1));
        params.push(project.clone());
    }
    if let Some(search) = &query.search {
        sql.push_str(&format!(" AND title LIKE ?{}", params.len() + 1));
        params.push(format!("%{search}%"));
    }
    if let Some(anchor) = anchor {
        let (comparison, tie) = if order == "asc" {
            (">", ">")
        } else {
            ("<", "<")
        };
        let n = params.len() + 1;
        sql.push_str(&format!(
            " AND (time_created {comparison} ?{n} OR (time_created = ?{n} AND id {tie} ?{id}))",
            id = n + 1
        ));
        params.push(format!("{}", anchor.time as i64));
        params.push(anchor.id.clone());
    }
    let id_order = if order == "asc" { "ASC" } else { "DESC" };
    sql.push_str(&format!(
        " ORDER BY time_created {order}, id {id_order} LIMIT ?{}",
        params.len() + 1
    ));
    params.push(limit.to_string());

    storage
        .with_connection(move |conn| {
            let mut stmt = conn.prepare(&sql).map_err(|err| err.to_string())?;
            let params_ref: Vec<&dyn rusqlite::ToSql> = params
                .iter()
                .map(|value| value as &dyn rusqlite::ToSql)
                .collect();
            let mut rows = stmt
                .query(params_ref.as_slice())
                .map_err(|err| err.to_string())?;
            let mut result = Vec::new();
            while let Some(row) = rows.next().map_err(|err| err.to_string())? {
                result.push(SessionRow {
                    id: row.get("id").map_err(|err| err.to_string())?,
                    project_id: row.get("project_id").map_err(|err| err.to_string())?,
                    parent_id: row.get("parent_id").map_err(|err| err.to_string())?,
                    agent: row.get("agent").map_err(|err| err.to_string())?,
                    model: row.get("model").map_err(|err| err.to_string())?,
                    cost: row.get("cost").map_err(|err| err.to_string())?,
                    tokens_input: row.get("tokens_input").map_err(|err| err.to_string())?,
                    tokens_output: row.get("tokens_output").map_err(|err| err.to_string())?,
                    tokens_reasoning: row.get("tokens_reasoning").map_err(|err| err.to_string())?,
                    tokens_cache_read: row
                        .get("tokens_cache_read")
                        .map_err(|err| err.to_string())?,
                    tokens_cache_write: row
                        .get("tokens_cache_write")
                        .map_err(|err| err.to_string())?,
                    directory: row.get("directory").map_err(|err| err.to_string())?,
                    workspace_id: row.get("workspace_id").map_err(|err| err.to_string())?,
                    path: row.get("path").map_err(|err| err.to_string())?,
                    title: row.get("title").map_err(|err| err.to_string())?,
                    revert: row.get("revert").map_err(|err| err.to_string())?,
                    time_created: row.get("time_created").map_err(|err| err.to_string())?,
                    time_updated: row.get("time_updated").map_err(|err| err.to_string())?,
                    time_archived: row.get("time_archived").map_err(|err| err.to_string())?,
                });
            }
            Ok::<_, String>(result)
        })
        .map_err(defect)
}

/// The columns `session_from_row` needs (subset of the `session` table).
struct SessionRow {
    id: String,
    project_id: String,
    parent_id: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    cost: f64,
    tokens_input: i64,
    tokens_output: i64,
    tokens_reasoning: i64,
    tokens_cache_read: i64,
    tokens_cache_write: i64,
    directory: String,
    workspace_id: Option<String>,
    path: Option<String>,
    title: String,
    revert: Option<String>,
    time_created: i64,
    time_updated: i64,
    time_archived: Option<i64>,
}

impl SessionRow {
    /// `fromRow` (`core/src/session/info.ts`) over the selected columns.
    fn to_info(&self) -> Result<alforria_schema::session::SessionInfo, ServerError> {
        let model = self
            .model
            .as_deref()
            .map(serde_json::from_str::<alforria_schema::session_v1::V1SessionModel>)
            .transpose()
            .map_err(|err| defect(format!("invalid session model: {err}")))?;
        let model = model.map(|model| alforria_schema::model::ModelRef {
            id: model.id,
            provider_id: model.provider_id,
            variant: Some(model.variant.unwrap_or_else(|| "default".to_string())),
        });
        let revert = self
            .revert
            .as_deref()
            .map(serde_json::from_str::<alforria_schema::revert::RevertState>)
            .transpose()
            .map_err(|err| defect(format!("invalid session revert: {err}")))?;
        Ok(alforria_schema::session::SessionInfo {
            id: self.id.clone(),
            parent_id: self.parent_id.clone(),
            project_id: self.project_id.clone(),
            agent: self.agent.clone(),
            model,
            cost: self.cost,
            tokens: alforria_schema::session::SessionTokens {
                input: self.tokens_input as f64,
                output: self.tokens_output as f64,
                reasoning: self.tokens_reasoning as f64,
                cache: alforria_schema::session::SessionTokensCache {
                    read: self.tokens_cache_read as f64,
                    write: self.tokens_cache_write as f64,
                },
            },
            time: alforria_schema::session::SessionTime {
                created: self.time_created,
                updated: self.time_updated,
                archived: self.time_archived,
            },
            title: self.title.clone(),
            location: alforria_schema::location::LocationRef {
                directory: self.directory.clone(),
                workspace_id: self.workspace_id.clone(),
                project: None,
            },
            subpath: self.path.clone().filter(|path| !path.is_empty()),
            revert,
        })
    }
}

/// `session.list` (`handlers/session.ts:14-50`).
async fn list(
    State(ctx): State<Arc<ServerContext>>,
    uri: axum::http::Uri,
) -> Result<Response, ServerError> {
    let query = parse_sessions_query(&uri)?;
    let (query, anchor) = match &query.cursor {
        Some(cursor) => {
            let decoded = decode_sessions_cursor(cursor).map_err(|_| ApiError::InvalidCursor {
                message: "Invalid cursor".to_string(),
            })?;
            (decoded.query, Some(decoded.anchor))
        }
        None => (query, None),
    };
    let limit = query.limit.unwrap_or(50) as i64;
    let direction = anchor
        .as_ref()
        .map(|anchor| anchor.direction)
        .unwrap_or(alforria_schema::session::SessionListDirection::Next);

    // The V2 list is process-global (no location); sessions live in the
    // shared storage.
    let rows = list_rows(&ctx.storage, &query, anchor.as_ref(), &direction, limit)?;
    let mut rows = rows;
    use alforria_schema::session::SessionListDirection;
    if direction == SessionListDirection::Previous {
        rows.reverse();
    }
    let infos: Vec<alforria_schema::session::SessionInfo> = rows
        .iter()
        .map(|row| row.to_info())
        .collect::<Result<_, _>>()?;

    // Cursors — `{...query, anchor}` with the first/last row anchored.
    let cursor_for = |id: &str,
                      time: i64,
                      direction: SessionListDirection|
     -> Result<Option<String>, ServerError> {
        let mut q = query.clone();
        q.cursor = None;
        Ok(Some(encode_sessions_cursor(
            &q,
            &alforria_schema::session::SessionListAnchor {
                id: id.to_string(),
                time: time as f64,
                direction,
            },
        )))
    };
    let mut previous = None;
    let mut next = None;
    if let (Some(first), Some(last)) = (infos.first(), infos.last()) {
        previous = cursor_for(
            &first.id,
            first.time.created,
            SessionListDirection::Previous,
        )?;
        next = cursor_for(&last.id, last.time.created, SessionListDirection::Next)?;
    }
    Ok(json_ok(serde_json::json!({
        "data": infos,
        "cursor": { "previous": previous, "next": next },
    })))
}

// ---------------------------------------------------------------------------
// session.create (`handlers/session.ts:52-66`)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreatePayload {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<alforria_schema::session_v1::V1SessionModel>,
    #[serde(default)]
    location: Option<LocationPayload>,
}

#[derive(Deserialize)]
struct LocationPayload {
    directory: String,
    #[serde(rename = "workspaceID", default)]
    workspace_id: Option<String>,
}

async fn create(
    State(ctx): State<Arc<ServerContext>>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: CreatePayload = parse_payload(&body)?;
    if let Some(id) = &payload.id {
        if !id.starts_with("ses") {
            return Err(payload_error(format!(
                "Expected a string starting with \"ses\", got {id:?}"
            )));
        }
    }
    if let Some(workspace) = payload
        .location
        .as_ref()
        .and_then(|location| location.workspace_id.as_ref())
    {
        if !workspace.starts_with("wrk_") {
            return Err(payload_error(format!(
                "Expected a string starting with \"wrk_\", got {workspace:?}"
            )));
        }
    }
    let directory = payload
        .location
        .as_ref()
        .map(|location| location.directory.clone())
        .unwrap_or_else(|| crate::state::cwd().to_string_lossy().into_owned());
    let services = ctx.instances.load(std::path::Path::new(&directory))?;

    // `store.get(sessionID)` — an existing id returns the recorded session.
    if let Some(id) = &payload.id {
        if let Ok(info) = services.sessions.get(id) {
            return Ok(json_ok(
                serde_json::json!({ "data": session_from_v1(&info) }),
            ));
        }
    }

    let workspace_id = payload
        .location
        .as_ref()
        .and_then(|location| location.workspace_id.clone());
    let session_ctx = services
        .instance_context(std::path::Path::new(&directory), workspace_id)
        .map_err(session_error)?;
    let input = alforria_core::session::CreateInput {
        id: payload.id.clone(),
        agent: payload.agent.clone(),
        model: payload.model.clone(),
        workspace_id: None,
        ..Default::default()
    };
    let info = services
        .sessions
        .create(&session_ctx, &input)
        .map_err(session_error)?;
    Ok(json_ok(
        serde_json::json!({ "data": session_from_v1(&info) }),
    ))
}

// ---------------------------------------------------------------------------
// session.active (`handlers/session.ts:68-77`)
// ---------------------------------------------------------------------------

async fn active(State(ctx): State<Arc<ServerContext>>) -> Result<Response, ServerError> {
    let mut data = serde_json::Map::new();
    for services in ctx.instances.cached() {
        for (session_id, status) in services.status.list() {
            if matches!(
                status,
                alforria_schema::session_status::SessionStatusInfo::Busy
            ) {
                data.insert(session_id, serde_json::json!({ "type": "running" }));
            }
        }
    }
    Ok(json_ok(serde_json::json!({ "data": Value::Object(data) })))
}

// ---------------------------------------------------------------------------
// session.get (`handlers/session.ts:79-94`)
// ---------------------------------------------------------------------------

async fn get_session(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    let info = require_session(&location, &session_id)?;
    Ok(json_ok(
        serde_json::json!({ "data": session_from_v1(&info) }),
    ))
}

// ---------------------------------------------------------------------------
// session.switchAgent / switchModel (204)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SwitchAgentPayload {
    agent: String,
}

async fn switch_agent(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: SwitchAgentPayload = parse_payload(&body)?;
    let session = require_session(&location, &session_id)?;
    location
        .services
        .sessions
        .patch(
            &session_id,
            SessionPatch {
                agent: Some(payload.agent),
                time: Some(alforria_core::session::store::PartialTime {
                    updated: Some(now_ms()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .map_err(session_error)?;
    // The agent patch covers the row mutation; the TS projector also
    // inserts a `session_message` row and publishes
    // `session.next.agent.switched` (M7 V2-runner machinery).
    let _ = session;
    Ok(no_content())
}

#[derive(Deserialize)]
struct SwitchModelPayload {
    model: alforria_schema::session_v1::V1SessionModel,
}

async fn switch_model(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: SwitchModelPayload = parse_payload(&body)?;
    let info = require_session(&location, &session_id)?;
    let same = info.model.as_ref().is_some_and(|model| {
        model.id == payload.model.id
            && model.provider_id == payload.model.provider_id
            && model
                .variant
                .clone()
                .unwrap_or_else(|| "default".to_string())
                == payload
                    .model
                    .variant
                    .clone()
                    .unwrap_or_else(|| "default".to_string())
    });
    if !same {
        location
            .services
            .sessions
            .patch(
                &session_id,
                SessionPatch {
                    model: Some(payload.model),
                    time: Some(alforria_core::session::store::PartialTime {
                        updated: Some(now_ms()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .map_err(session_error)?;
    }
    Ok(no_content())
}

// ---------------------------------------------------------------------------
// session.prompt — M6.7 S1 STOP
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PromptPayload {
    #[serde(default)]
    id: Option<String>,
    prompt: alforria_schema::prompt_input::PromptInput,
    #[serde(default)]
    delivery: Option<String>,
    #[serde(default)]
    resume: Option<bool>,
}

async fn prompt(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: PromptPayload = parse_payload(&body)?;
    if let Some(id) = &payload.id {
        if !id.starts_with("msg") {
            return Err(payload_error(format!(
                "Expected a string starting with \"msg\", got {id:?}"
            )));
        }
    }
    // `Schema.Literals(["steer", "queue"])` — decode validation only; the
    // admission path that consumes it is the M6.7 S1 stop below.
    if let Some(delivery) = &payload.delivery {
        if delivery != "steer" && delivery != "queue" {
            return Err(payload_error(format!(
                "Expected \"steer\" or \"queue\", got {delivery:?}"
            )));
        }
    }
    // `prompt` and `resume` are consumed by the admission/wake path that
    // stops below; decoding them above validated the wire shape.
    let _prompt_attachments = payload.prompt.files.as_ref().map_or(0, Vec::len);
    let _resume = payload.resume.unwrap_or(true);
    require_session(&location, &session_id)?;
    let message_id = payload.id.clone().unwrap_or_else(|| {
        alforria_core::session::ids::MessageId::ascending(None).expect("generates msg_ id")
    });
    // `SessionInput.LifecycleConflict` → 409 — the closest V1-backed check
    // is the durable message row.
    let exists = location
        .services
        .messages
        .get(&session_id, &message_id)
        .is_ok();
    if exists {
        return Err(ApiError::Conflict {
            message: format!(
                "Prompt message ID conflicts with an existing durable record: {message_id}"
            ),
            resource: Some(message_id),
        }
        .into());
    }
    // M6.7 S1 STOP: `Admitted` delivery semantics need the V2 runner
    // (`SessionInput.admit` + `SessionExecution.wake`), which M5 did not
    // port. The pre-checks above match TS; the admission itself stops for
    // review rather than inventing a `admittedSeq`.
    Err(defect(
        "session prompt is not available yet (M6.7 S1: V2 runner not ported)",
    ))
}

// ---------------------------------------------------------------------------
// session.compact / session.wait — always 503 after the 404 session check
// ---------------------------------------------------------------------------

fn operation_unavailable(operation: &str) -> ServerError {
    ApiError::ServiceUnavailable {
        message: format!("Session {operation} is not available yet"),
        service: Some(format!("session.{operation}")),
    }
    .into()
}

async fn compact(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    require_session(&location, &session_id)?;
    Err(operation_unavailable("compact"))
}

async fn wait(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    require_session(&location, &session_id)?;
    Err(operation_unavailable("wait"))
}

// ---------------------------------------------------------------------------
// session.revert.* (`core/src/session/revert.ts`)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RevertStagePayload {
    #[serde(rename = "messageID")]
    message_id: String,
    #[serde(default)]
    files: Option<bool>,
}

/// `session_message` row lookup by id (the V2 store's `message`).
fn session_message_row(
    services: &SessionServices,
    message_id: &str,
) -> Result<Option<(String, String, String)>, ServerError> {
    services
        .storage
        .with_connection(|conn| {
            let mut stmt = conn
                .prepare("SELECT session_id, type, data FROM session_message WHERE id = ?1")
                .map_err(|err| err.to_string())?;
            let mut rows = stmt.query([message_id]).map_err(|err| err.to_string())?;
            match rows.next().map_err(|err| err.to_string())? {
                Some(row) => Ok::<_, String>(Some((
                    row.get::<_, String>(0).map_err(|err| err.to_string())?,
                    row.get::<_, String>(1).map_err(|err| err.to_string())?,
                    row.get::<_, String>(2).map_err(|err| err.to_string())?,
                ))),
                None => Ok(None),
            }
        })
        .map_err(defect)
}

fn message_not_found(session_id: &str, message_id: &str) -> ApiError {
    ApiError::MessageNotFound {
        session_id: session_id.to_string(),
        message_id: message_id.to_string(),
        message: format!("Message not found: {message_id}"),
    }
}

async fn revert_stage(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: RevertStagePayload = parse_payload(&body)?;
    if !payload.message_id.starts_with("msg") {
        return Err(payload_error(format!(
            "Expected a string starting with \"msg_\", got {:?}",
            payload.message_id
        )));
    }
    let session = require_session(&location, &session_id)?;
    // `SessionRevert.plan` resolves the boundary in the `session_message`
    // projection — empty under the V1 engine.
    let _ = session;
    // `files !== false` gates the snapshot staging in `SessionRevert.stage`;
    // decoding it above validated the wire shape and the staging is
    // unreachable under the V1 backing store.
    let _files = payload.files;
    match session_message_row(&location.services, &payload.message_id)? {
        Some(_) => Err(defect(
            "revert staging is not available yet (M6.7 S1: snapshot service not ported)",
        )),
        None => Err(message_not_found(&session_id, &payload.message_id).into()),
    }
}

async fn revert_clear(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    let info = require_session(&location, &session_id)?;
    if info.revert.is_none() {
        return Ok(no_content());
    }
    // M6.7 S1 STOP: clearing a staged revert restores snapshot files and
    // publishes `session.next.revert.cleared` (durable) — both need M7
    // machinery.
    Err(defect(
        "revert clear is not available yet (M6.7 S1: snapshot service not ported)",
    ))
}

async fn revert_commit(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    let info = require_session(&location, &session_id)?;
    if info.revert.is_none() {
        return Ok(no_content());
    }
    // M6.7 S1 STOP: committing publishes `session.next.revert.committed`
    // (durable) through the V2 bus manifest.
    Err(defect(
        "revert commit is not available yet (M6.7 S1: V2 event manifest not ported)",
    ))
}

// ---------------------------------------------------------------------------
// session.context (`core/src/session/history.ts` `load`)
// ---------------------------------------------------------------------------

fn decode_session_message(id: &str, type_: &str, data: &str) -> Result<Value, ServerError> {
    let mut merged: Value = serde_json::from_str(data)
        .map_err(|_| defect(format!("failed to decode session message {id}")))?;
    let map = merged
        .as_object_mut()
        .ok_or_else(|| defect(format!("failed to decode session message {id}")))?;
    map.insert("id".to_string(), Value::String(id.to_string()));
    map.insert("type".to_string(), Value::String(type_.to_string()));
    alforria_schema::session_message::SessionMessage::deserialize(&merged)
        .map(|_| merged)
        .map_err(|_| defect(format!("failed to decode session message {id}")))
}

async fn context(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    require_session(&location, &session_id)?;
    // `SessionHistory.load` (`core/src/session/history.ts:38-48`): the
    // context window starts at the latest compaction; a context-epoch
    // baseline additionally hides older `system` rows. Empty under the V1
    // engine (no `session_message` rows).
    let rows: Vec<(String, String, String)> = location
        .services
        .storage
        .with_connection(|conn| {
            let baseline: Option<i64> = conn
                .query_row(
                    "SELECT baseline_seq FROM session_context_epoch WHERE session_id = ?1",
                    [&session_id],
                    |row| row.get(0),
                )
                .ok();
            let compaction: Option<i64> = conn
                .query_row(
                    "SELECT seq FROM session_message WHERE session_id = ?1 AND type = \
                     'compaction' ORDER BY seq DESC LIMIT 1",
                    [&session_id],
                    |row| row.get(0),
                )
                .ok();
            let mut sql =
                String::from("SELECT id, type, data FROM session_message WHERE session_id = ?1");
            if compaction.is_some() {
                sql.push_str(
                    " AND (seq >= (SELECT seq FROM session_message WHERE session_id = ?1 AND \
                     type = 'compaction' ORDER BY seq DESC LIMIT 1)",
                );
                match baseline {
                    Some(baseline) => {
                        sql.push_str(&format!(" OR (type = 'system' AND seq > {baseline})"));
                    }
                    None => sql.push(')'),
                }
            }
            if let Some(baseline) = baseline {
                sql.push_str(&format!(" AND (type != 'system' OR seq > {baseline})"));
            }
            sql.push_str(" ORDER BY seq ASC");
            let mut stmt = conn.prepare(&sql).map_err(|err| err.to_string())?;
            let mut rows = stmt.query([&session_id]).map_err(|err| err.to_string())?;
            let mut result = Vec::new();
            while let Some(row) = rows.next().map_err(|err| err.to_string())? {
                let id: String = row.get(0).map_err(|err| err.to_string())?;
                let type_: String = row.get(1).map_err(|err| err.to_string())?;
                let data: String = row.get(2).map_err(|err| err.to_string())?;
                result.push((id, type_, data));
            }
            Ok::<_, String>(result)
        })
        .map_err(defect)?;
    let mut messages = Vec::new();
    for (id, type_, data) in rows {
        messages.push(decode_session_message(&id, &type_, &data)?);
    }
    Ok(json_ok(serde_json::json!({ "data": messages })))
}

// ---------------------------------------------------------------------------
// session.history (`EventV2.readAggregate` + `SessionDurable`)
// ---------------------------------------------------------------------------

/// The `SessionDurable` manifest (`schema-src/session-event.ts`) — durable
/// `session.next.*` types with their manifest versions.
const DURABLE_EVENT_VERSIONS: &[(&str, i64)] = &[
    ("session.next.agent.switched", 1),
    ("session.next.model.switched", 1),
    ("session.next.moved", 1),
    ("session.next.prompted", 1),
    ("session.next.prompt.admitted", 1),
    ("session.next.context.updated", 1),
    ("session.next.synthetic", 1),
    ("session.next.shell.started", 1),
    ("session.next.shell.ended", 1),
    ("session.next.step.started", 1),
    ("session.next.step.ended", 2),
    ("session.next.step.failed", 2),
    ("session.next.text.started", 1),
    ("session.next.text.ended", 1),
    ("session.next.tool.input.started", 1),
    ("session.next.tool.input.ended", 1),
    ("session.next.tool.called", 1),
    ("session.next.tool.progress", 1),
    ("session.next.tool.success", 1),
    ("session.next.tool.failed", 1),
    ("session.next.reasoning.started", 1),
    ("session.next.reasoning.ended", 1),
    ("session.next.retried", 1),
    ("session.next.compaction.started", 1),
    ("session.next.compaction.ended", 1),
    ("session.next.revert.staged", 1),
    ("session.next.revert.cleared", 1),
    ("session.next.revert.committed", 1),
];

async fn history(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    uri: axum::http::Uri,
) -> Result<Response, ServerError> {
    require_session(&location, &session_id)?;
    let after = match query_param(uri.query(), "after") {
        Some(raw) => match raw.parse::<i64>() {
            Ok(value) if value >= 0 => Some(value),
            _ => {
                return Err(query_error(format!("Expected a number, got {raw:?}")));
            }
        },
        None => None,
    };
    let limit = match query_param(uri.query(), "limit") {
        Some(raw) => {
            let value = raw
                .parse::<i64>()
                .map_err(|_| query_error(format!("Expected a number, got {raw:?}")))?;
            if !(1..=100).contains(&value) {
                return Err(query_error(format!(
                    "Expected a number between 1 and 100, got {raw:?}"
                )));
            }
            value
        }
        None => 50,
    };
    let after = after.unwrap_or(-1);
    let rows: Vec<(String, i64, String, String)> = location
        .services
        .storage
        .with_connection(|conn| {
            let types = DURABLE_EVENT_VERSIONS
                .iter()
                .map(|(type_, _)| format!("'{}'", type_.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT id, seq, type, data FROM event WHERE aggregate_id = ?1 AND seq > ?2 \
                 AND type IN ({types}) ORDER BY seq ASC LIMIT ?3"
            );
            let mut stmt = conn.prepare(&sql).map_err(|err| err.to_string())?;
            let mut rows = stmt
                .query(rusqlite::params![session_id, after, limit + 1])
                .map_err(|err| err.to_string())?;
            let mut result = Vec::new();
            while let Some(row) = rows.next().map_err(|err| err.to_string())? {
                result.push((
                    row.get::<_, String>(0).map_err(|err| err.to_string())?,
                    row.get::<_, i64>(1).map_err(|err| err.to_string())?,
                    row.get::<_, String>(2).map_err(|err| err.to_string())?,
                    row.get::<_, String>(3).map_err(|err| err.to_string())?,
                ));
            }
            Ok::<_, String>(result)
        })
        .map_err(defect)?;
    let has_more = rows.len() as i64 > limit;
    let mut page: Vec<Value> = Vec::new();
    for (id, seq, type_, data) in rows.into_iter().take(limit as usize) {
        let version = DURABLE_EVENT_VERSIONS
            .iter()
            .find(|(event_type, _)| *event_type == type_)
            .map(|(_, version)| *version)
            .unwrap_or(1);
        let data: Value = serde_json::from_str(&data)
            .map_err(|err| defect(format!("failed to decode durable event {id}: {err}")))?;
        page.push(serde_json::json!({
            "id": id,
            "type": type_,
            "durable": { "aggregateID": session_id, "seq": seq, "version": version },
            "data": data,
        }));
    }
    Ok(json_ok(serde_json::json!({
        "data": page,
        "hasMore": has_more,
    })))
}

// ---------------------------------------------------------------------------
// session.interrupt — `SessionExecution.interrupt` → `SessionRunState::cancel`
// ---------------------------------------------------------------------------

async fn interrupt(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    location
        .services
        .run_state
        .cancel(&session_id)
        .await
        .map_err(defect)?;
    Ok(no_content())
}

// ---------------------------------------------------------------------------
// session.message (`handlers/session.ts:373-383`)
// ---------------------------------------------------------------------------

async fn message(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path((session_id, message_id)): Path<(String, String)>,
) -> Result<Response, ServerError> {
    if !message_id.starts_with("msg") {
        return Err(params_error(format!(
            "Expected a string starting with \"msg_\", got {message_id:?}"
        )));
    }
    match session_message_row(&location.services, &message_id)? {
        Some((row_session, type_, data)) if row_session == session_id => {
            Ok(json_ok(serde_json::json!({
                "data": decode_session_message(&message_id, &type_, &data)?,
            })))
        }
        _ => Err(message_not_found(&session_id, &message_id).into()),
    }
}

// ---------------------------------------------------------------------------
// session.messages (`handlers/message.ts`)
// ---------------------------------------------------------------------------

fn encode_message_cursor(id: &str, order: &str, direction: &str) -> String {
    use base64::Engine;
    let json = format!("{{\"id\":{id},\"order\":{order},\"direction\":{direction}}}");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

/// Decode the message cursor (`handlers/message.ts:17-28`). `Err(())` maps
/// to `InvalidCursorError`.
fn decode_message_cursor(input: &str) -> Result<(String, String, String), ()> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(input)
        .map_err(|_| ())?;
    let json: Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
    let Value::Object(map) = json else {
        return Err(());
    };
    let id = match map.get("id") {
        Some(Value::String(id)) if id.starts_with("msg") => id.clone(),
        _ => return Err(()),
    };
    let order = match map.get("order") {
        Some(Value::String(order)) if order == "asc" || order == "desc" => order.clone(),
        _ => return Err(()),
    };
    let direction = match map.get("direction") {
        Some(Value::String(direction)) if direction == "previous" || direction == "next" => {
            direction.clone()
        }
        _ => return Err(()),
    };
    Ok((id, order, direction))
}

async fn messages(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    uri: axum::http::Uri,
) -> Result<Response, ServerError> {
    let cursor = query_param(uri.query(), "cursor");
    let order_param = query_param(uri.query(), "order");
    if cursor.is_some() && order_param.is_some() {
        return Err(ApiError::InvalidCursor {
            message: "Cursor cannot be combined with order".to_string(),
        }
        .into());
    }
    let (cursor_id, order, direction) = match &cursor {
        Some(cursor) => decode_message_cursor(cursor).map_err(|_| ApiError::InvalidCursor {
            message: "Invalid cursor".to_string(),
        })?,
        None => (String::new(), "desc".to_string(), "next".to_string()),
    };
    let requested_order = match order_param.as_deref() {
        None | Some("desc") => "desc",
        Some("asc") => "asc",
        Some(other) => {
            return Err(query_error(format!(
                "Expected \"asc\" or \"desc\", got {other:?}"
            )))
        }
    };
    // `session.messages` core: order from the cursor wins; previous pages
    // invert it.
    let base_order = if cursor.is_some() {
        order
    } else {
        requested_order.to_string()
    };
    let order = match (direction.as_str(), base_order.as_str()) {
        ("previous", "asc") => "desc",
        ("previous", _) => "asc",
        (_, o) => o,
    };
    let limit = match query_param(uri.query(), "limit") {
        Some(raw) => {
            let value = raw
                .parse::<i64>()
                .map_err(|_| query_error(format!("Expected a number, got {raw:?}")))?;
            if !(1..=200).contains(&value) {
                return Err(query_error(format!(
                    "Expected a number between 1 and 200, got {raw:?}"
                )));
            }
            value
        }
        None => 50,
    };
    require_session(&location, &session_id)?;

    let rows: Vec<(String, i64, String, String)> = location
        .services
        .storage
        .with_connection(|conn| {
            let anchor: Option<i64> = if cursor.is_some() {
                conn.query_row(
                    "SELECT seq FROM session_message WHERE session_id = ?1 AND id = ?2",
                    rusqlite::params![session_id, cursor_id],
                    |row| row.get(0),
                )
                .ok()
            } else {
                None
            };
            if cursor.is_some() && anchor.is_none() {
                return Ok(Vec::new());
            }
            let sql = format!(
                "SELECT id, seq, type, data FROM session_message WHERE session_id = ?1 {} \
                 ORDER BY seq {}",
                match anchor {
                    Some(_) =>
                        if order == "asc" {
                            "AND seq > ?2"
                        } else {
                            "AND seq < ?2"
                        },
                    None => "",
                },
                if order == "asc" { "ASC" } else { "DESC" },
            );
            let mut stmt = conn.prepare(&sql).map_err(|err| err.to_string())?;
            let mut rows = match anchor {
                Some(seq) => stmt
                    .query(rusqlite::params![session_id, seq])
                    .map_err(|err| err.to_string())?,
                None => stmt
                    .query(rusqlite::params![session_id])
                    .map_err(|err| err.to_string())?,
            };
            let mut result = Vec::new();
            while let Some(row) = rows.next().map_err(|err| err.to_string())? {
                result.push((
                    row.get::<_, String>(0).map_err(|err| err.to_string())?,
                    row.get::<_, i64>(1).map_err(|err| err.to_string())?,
                    row.get::<_, String>(2).map_err(|err| err.to_string())?,
                    row.get::<_, String>(3).map_err(|err| err.to_string())?,
                ));
            }
            result.truncate(limit as usize);
            Ok::<_, String>(result)
        })
        .map_err(defect)?;
    let mut rows = rows;
    if direction == "previous" {
        rows.reverse();
    }
    let messages: Vec<Value> = rows
        .iter()
        .map(|(id, _, type_, data)| decode_session_message(id, type_, data))
        .collect::<Result<_, _>>()?;
    let first = rows.first();
    let last = rows.last();
    let previous = first.map(|(id, _, _, _)| encode_message_cursor(id, order, "previous"));
    let next = last.map(|(id, _, _, _)| encode_message_cursor(id, order, "next"));
    Ok(json_ok(serde_json::json!({
        "data": messages,
        "cursor": { "previous": previous, "next": next },
    })))
}

// ---------------------------------------------------------------------------
// question (`handlers/question.ts`)
// ---------------------------------------------------------------------------

async fn question_request_list(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let requests = location.services.question.list();
    envelope(&location, requests)
}

async fn question_list(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    let requests = location
        .services
        .question
        .list()
        .into_iter()
        .filter(|request| request.session_id == session_id)
        .collect::<Vec<_>>();
    Ok(json_ok(serde_json::json!({ "data": requests })))
}

#[derive(Deserialize)]
struct QuestionReplyPayload {
    answers: Vec<QuestionAnswer>,
}

fn question_not_found(request_id: &str) -> ApiError {
    ApiError::QuestionNotFound {
        request_id: request_id.to_string(),
        message: format!("Question request not found: {request_id}"),
    }
}

fn owned_question(
    location: &LocationContext,
    session_id: &str,
    request_id: &str,
) -> Result<(), ServerError> {
    let owned = location
        .services
        .question
        .list()
        .into_iter()
        .any(|request| request.id == request_id && request.session_id == session_id);
    if owned {
        return Ok(());
    }
    Err(question_not_found(request_id).into())
}

async fn question_reply(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path((session_id, request_id)): Path<(String, String)>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: QuestionReplyPayload = parse_payload(&body)?;
    owned_question(&location, &session_id, &request_id)?;
    use alforria_core::QuestionError;
    location
        .services
        .question
        .reply(&request_id, payload.answers)
        .map_err(|err| match err {
            QuestionError::NotFound { request_id } => question_not_found(&request_id).into(),
            other => defect(other),
        })?;
    Ok(no_content())
}

async fn question_reject(
    axum::Extension(location): axum::Extension<LocationContext>,
    Path((session_id, request_id)): Path<(String, String)>,
) -> Result<Response, ServerError> {
    owned_question(&location, &session_id, &request_id)?;
    use alforria_core::QuestionError;
    location
        .services
        .question
        .reject(&request_id)
        .map_err(|err| match err {
            QuestionError::NotFound { request_id } => question_not_found(&request_id).into(),
            other => defect(other),
        })?;
    Ok(no_content())
}
