//! v1 sync routes — port of
//! `packages/opencode/src/server/routes/instance/httpapi/{groups,handlers}/sync.ts`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::response::Response;
use axum::routing::post;
use serde::Deserialize;

use alforria_core::event::bus::ReplayOpts;
use alforria_core::event::definition::SerializedEvent;

use crate::error::{ApiError, ServerError};
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

use super::util::{defect, json_ok, parse_payload, session_error};

type Router = axum::Router<Arc<ServerContext>>;

pub fn register(router: Router, method: &str, path: &'static str) -> (Router, bool) {
    let router = match (method, path) {
        ("POST", "/sync/start") => router.route(path, post(start)),
        ("POST", "/sync/replay") => router.route(path, post(replay)),
        ("POST", "/sync/steal") => router.route(path, post(steal)),
        ("POST", "/sync/history") => router.route(path, post(history)),
        _ => return (router, false),
    };
    (router, true)
}

/// `start` (`handlers/sync.ts:20-26`) — fork
/// `workspace.startWorkspaceSyncing(project.id)`; the sync runtime is a
/// forked continuation (§7.4), so the fork is a no-op seam.
async fn start() -> Result<Response, ServerError> {
    Ok(json_ok(true))
}

#[derive(Deserialize)]
struct ReplayEvent {
    id: String,
    #[serde(rename = "aggregateID")]
    aggregate_id: String,
    seq: i64,
    r#type: String,
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct ReplayPayload {
    /// Logged in TS (`handlers/sync.ts:37-43`); not otherwise consumed.
    #[serde(rename = "directory")]
    _directory: String,
    events: Vec<ReplayEvent>,
}

/// `replay` (`handlers/sync.ts:28-49`) — replay a complete sync event
/// history into the durable log, respond `{sessionID: <first aggregateID>}`.
async fn replay(
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: ReplayPayload = parse_payload(&body)?;
    if payload.events.is_empty() {
        return Err(ApiError::BadRequest {
            message: "Expected non-empty events".to_string(),
            kind: None,
        }
        .into());
    }
    // `payload[0].aggregateID` — echoed as the response `sessionID`
    // (`handlers/sync.ts:31-48`).
    let source = payload.events[0].aggregate_id.clone();
    let events: Vec<SerializedEvent> = payload
        .events
        .into_iter()
        .map(|event| SerializedEvent {
            id: event.id,
            r#type: event.r#type,
            seq: event.seq,
            aggregate_id: event.aggregate_id,
            data: event.data,
        })
        .collect();
    let opts = ReplayOpts {
        publish: false,
        owner_id: location.workspace_id.clone(),
        strict_owner: true,
    };
    location
        .services
        .events
        .replay_all(events, opts)
        .map_err(|err| defect(err.to_string()))?;
    Ok(json_ok(serde_json::json!({
        "sessionID": source,
    })))
}

#[derive(Deserialize)]
struct StealPayload {
    #[serde(rename = "sessionID")]
    session_id: String,
}

/// `steal` (`handlers/sync.ts:51-60`) — 400 when the instance has no
/// workspace id; else move the session into this workspace.
async fn steal(
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: StealPayload = parse_payload(&body)?;
    let Some(workspace_id) = location.workspace_id.as_deref() else {
        return Err(ApiError::TaggedBadRequest.into());
    };
    location
        .services
        .sessions
        .set_workspace(&payload.session_id, Some(workspace_id.to_string()))
        .map_err(session_error)?;
    Ok(json_ok(serde_json::json!({
        "sessionID": payload.session_id,
    })))
}

/// `history` (`handlers/sync.ts:62-73`) — event rows ordered `seq asc`,
/// excluding `aggregate = id AND seq <= value` per entry of the payload
/// map.
async fn history(
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: std::collections::HashMap<String, i64> = parse_payload(&body)?;
    let rows = location
        .services
        .storage
        .with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, aggregate_id, seq, type, data FROM event
                 ORDER BY seq ASC",
            )?;
            let mut rows = stmt.query(rusqlite::params![])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let aggregate: String = row.get(1)?;
                let seq: i64 = row.get(2)?;
                let r#type: String = row.get(3)?;
                let data: String = row.get(4)?;
                if let Some(last) = payload.get(&aggregate) {
                    if seq <= *last {
                        continue;
                    }
                }
                out.push(serde_json::json!({
                    "id": id,
                    "aggregate_id": aggregate,
                    "seq": seq,
                    "type": r#type,
                    "data": serde_json::from_str::<serde_json::Value>(&data)
                        .unwrap_or(serde_json::Value::Null),
                }));
            }
            Ok::<_, alforria_core::CoreError>(out)
        })
        .map_err(|err| defect(err.to_string()))?;
    Ok(json_ok(rows))
}
