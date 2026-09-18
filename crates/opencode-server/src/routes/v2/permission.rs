//! v2 permission family (M6.7) — port of `packages/server/src/handlers/permission.ts`
//! over a process-wide pending registry.
//!
//! TS owns one `PermissionV2.Service` per location node (an in-memory pending
//! map per directory, `core/src/permission.ts`). The M6 adapter keeps one
//! registry per server process, tagged with the requesting location's
//! directory; list/get/forSession filter by it. Recorded divergence: pending
//! requests are visible process-wide internally (they are filtered to the
//! location before leaving the registry) and the `assert`/`Deferred`
//! ask-answer channel of the V2 runner is not wired — the registry serves
//! the HTTP surface only.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::Value;

use opencode_core::session::ids::PermissionId;
use opencode_schema::permission::{
    PermissionEffect, PermissionReply, PermissionRequest, PermissionRule, PermissionSource,
};
use opencode_schema::schema::JsonMap;

use crate::error::{ApiError, ServerError};
use crate::middleware::auth::query_param;
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

use super::util::{
    defect, envelope, evaluate_permission, no_content, parse_payload, payload_error,
    relabel_ruleset,
};

// ---------------------------------------------------------------------------
// the pending registry (`core/src/permission.ts:120-150`)
// ---------------------------------------------------------------------------

/// `permission.v2.asked` (`schema-src/permission.ts:43`).
pub const PERMISSION_ASKED: opencode_core::event::definition::Definition =
    opencode_core::event::definition::Definition::ephemeral("permission.v2.asked");

/// `permission.v2.replied` (`schema-src/permission.ts:44`).
pub const PERMISSION_REPLIED: opencode_core::event::definition::Definition =
    opencode_core::event::definition::Definition::ephemeral("permission.v2.replied");

struct PendingEntry {
    request: PermissionRequest,
    agent: Option<String>,
    directory: PathBuf,
}

/// The process-wide pending-permission registry (a per-location node in
/// TS, `core/src/permission.ts:118-150`).
#[derive(Default)]
pub struct PermissionRegistry {
    pending: Mutex<HashMap<String, PendingEntry>>,
}

impl PermissionRegistry {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, PendingEntry>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn insert(&self, entry: PendingEntry) {
        self.lock().insert(entry.request.id.clone(), entry);
    }

    fn get(&self, id: &str) -> Option<PermissionRequest> {
        self.lock().get(id).map(|entry| entry.request.clone())
    }

    fn list(&self, directory: &std::path::Path) -> Vec<PermissionRequest> {
        self.lock()
            .values()
            .filter(|entry| entry.directory == directory)
            .map(|entry| entry.request.clone())
            .collect()
    }

    fn for_session(&self, directory: &std::path::Path, session_id: &str) -> Vec<PermissionRequest> {
        self.lock()
            .values()
            .filter(|entry| entry.directory == directory && entry.request.session_id == session_id)
            .map(|entry| entry.request.clone())
            .collect()
    }

    fn remove(&self, id: &str) -> Option<PendingEntry> {
        self.lock().remove(id)
    }

    fn entries_for_session(
        &self,
        directory: &std::path::Path,
        session_id: &str,
    ) -> Vec<(String, PermissionRequest, Option<String>)> {
        self.lock()
            .iter()
            .filter(|(_, entry)| {
                entry.directory == directory && entry.request.session_id == session_id
            })
            .map(|(id, entry)| (id.clone(), entry.request.clone(), entry.agent.clone()))
            .collect()
    }
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
        ("GET", "/api/permission/request") => router.route(path, get(request_list)),
        ("GET", "/api/permission/saved") => router.route(path, get(saved_list)),
        ("DELETE", "/api/permission/saved/{id}") => router.route(path, delete(saved_remove)),
        ("POST", "/api/session/{sessionID}/permission") => router.route(path, post(permission_ask)),
        ("GET", "/api/session/{sessionID}/permission") => router.route(path, get(permission_list)),
        ("GET", "/api/session/{sessionID}/permission/{requestID}") => {
            router.route(path, get(permission_get))
        }
        ("POST", "/api/session/{sessionID}/permission/{requestID}/reply") => {
            router.route(path, post(permission_reply))
        }
        _ => return (router, false),
    };
    (router, true)
}

// ---------------------------------------------------------------------------
// saved-permission storage (M3 `permission` table — `core/src/permission/saved.ts`)
// ---------------------------------------------------------------------------

/// One saved permission row (`PermissionSaved.Info`,
/// `schema-src/permission-saved.ts`).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SavedPermission {
    id: String,
    project_id: String,
    action: String,
    resource: String,
}

fn saved_list_sql(
    ctx: &ServerContext,
    project_id: &str,
) -> Result<Vec<SavedPermission>, ServerError> {
    ctx.storage
        .with_connection(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, project_id, action, resource FROM permission WHERE project_id = ?1",
                )
                .map_err(|err| err.to_string())?;
            let mut rows = stmt.query([project_id]).map_err(|err| err.to_string())?;
            let mut result = Vec::new();
            while let Some(row) = rows.next().map_err(|err| err.to_string())? {
                result.push(SavedPermission {
                    id: row.get(0).map_err(|err| err.to_string())?,
                    project_id: row.get(1).map_err(|err| err.to_string())?,
                    action: row.get(2).map_err(|err| err.to_string())?,
                    resource: row.get(3).map_err(|err| err.to_string())?,
                });
            }
            Ok::<_, String>(result)
        })
        .map_err(defect)
}

/// The saved rules as `{action, resource, effect: "allow"}` rules
/// (`savedRules`, `core/src/permission.ts:124-128`).
fn saved_rules(ctx: &ServerContext, project_id: &str) -> Result<Vec<PermissionRule>, ServerError> {
    Ok(saved_list_sql(ctx, project_id)?
        .into_iter()
        .map(|row| PermissionRule {
            action: row.action,
            resource: row.resource,
            effect: PermissionEffect::Allow,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// `permission.request.list` — the location envelope over the pending
/// requests for the location.
async fn request_list(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let requests = ctx.v2_permissions.list(&location.directory);
    envelope(&location, requests)
}

/// `permission.saved.list`.
async fn saved_list(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    uri: axum::http::Uri,
) -> Result<Response, ServerError> {
    let project_id = match query_param(uri.query(), "projectID") {
        Some(project) => project,
        None => {
            location
                .services
                .instance(&location.directory)
                .map_err(defect)?
                .project
                .id
        }
    };
    let rows = saved_list_sql(&ctx, &project_id)?;
    Ok(super::util::json_ok(serde_json::json!({ "data": rows })))
}

/// `permission.saved.remove`.
async fn saved_remove(
    State(ctx): State<Arc<ServerContext>>,
    Path(id): Path<String>,
) -> Result<Response, ServerError> {
    ctx.storage
        .with_connection(|conn| {
            conn.execute("DELETE FROM permission WHERE id = ?1", [&id])
                .map_err(|err| err.to_string())?;
            Ok::<_, String>(())
        })
        .map_err(defect)?;
    Ok(no_content())
}

#[derive(Deserialize)]
struct AskPayload {
    #[serde(default)]
    id: Option<String>,
    action: String,
    resources: Vec<String>,
    #[serde(default)]
    save: Option<Vec<String>>,
    #[serde(default)]
    metadata: Option<JsonMap>,
    #[serde(default)]
    source: Option<PermissionSource>,
    #[serde(default)]
    agent: Option<String>,
}

/// `configured` (`core/src/permission.ts:130-136`) — the resolved agent's
/// rules (the default agent when unset), deny-all when the agent has none.
fn configured(
    location: &LocationContext,
    session_id: &str,
    agent_id: Option<&str>,
) -> Result<Vec<PermissionRule>, ServerError> {
    let session = location.services.sessions.get(session_id).map_err(defect)?;
    let missing = || {
        vec![PermissionRule {
            action: "*".to_string(),
            resource: "*".to_string(),
            effect: PermissionEffect::Deny,
        }]
    };
    // `agents.resolve(agentID ?? session.agent)` — a missing id resolves
    // the *default* agent (`agent.ts:90-93`), deny-all only applies when
    // the resolved agent has no rules.
    let agent = agent_id
        .or(session.agent.as_deref())
        .map(|agent| agent.to_string())
        .or_else(|| location.services.agents.default_agent().ok());
    let Some(agent) = agent else {
        return Ok(missing());
    };
    let Some(info) = location.services.agents.get(&agent) else {
        return Ok(missing());
    };
    Ok(relabel_ruleset(&info.permission))
}

fn evaluate_input(input: &AskPayload, rules: &[PermissionRule]) -> PermissionEffect {
    let effects: Vec<PermissionEffect> = input
        .resources
        .iter()
        .map(|resource| evaluate_permission(&input.action, resource, rules).effect)
        .collect();
    if effects.contains(&PermissionEffect::Deny) {
        PermissionEffect::Deny
    } else if effects.contains(&PermissionEffect::Ask) {
        PermissionEffect::Ask
    } else {
        PermissionEffect::Allow
    }
}

fn denied(input: &AskPayload, rules: &[PermissionRule]) -> bool {
    input.resources.iter().any(|resource| {
        evaluate_permission(&input.action, resource, rules).effect == PermissionEffect::Deny
    })
}

/// `session.permission.create` (`handlers/permission.ts:40-67`).
async fn permission_ask(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: AskPayload = parse_payload(&body)?;
    if let Some(id) = &payload.id {
        if !id.starts_with("per_") {
            return Err(payload_error(format!(
                "Expected a string starting with \"per_\", got {id:?}"
            )));
        }
    }
    // `result.get(sessionID)` — the middleware guarantees existence; the
    // store re-check keeps the 404 parity with TS.
    location
        .services
        .sessions
        .get(&session_id)
        .map_err(|_| ApiError::SessionNotFound {
            session_id: session_id.clone(),
            message: format!("Session not found: {session_id}"),
        })?;
    let project_id = location
        .services
        .instance(&location.directory)
        .map_err(defect)?
        .project
        .id;
    let rules = configured(&location, &session_id, payload.agent.as_deref())?;
    let saved = saved_rules(&ctx, &project_id)?;
    let all_rules: Vec<PermissionRule> = rules.iter().chain(saved.iter()).cloned().collect();
    let effect = evaluate_input(&payload, &all_rules);

    let id = payload.id.clone().unwrap_or_else(PermissionId::generate);
    if effect == PermissionEffect::Ask {
        let request = PermissionRequest {
            id: id.clone(),
            session_id: session_id.clone(),
            action: payload.action.clone(),
            resources: payload.resources.clone(),
            save: payload.save.clone(),
            metadata: payload.metadata.clone(),
            source: payload.source.clone(),
        };
        ctx.bus
            .publish(
                &PERMISSION_ASKED,
                serde_json::to_value(&request).map_err(defect)?,
                Default::default(),
            )
            .map_err(defect)?;
        ctx.v2_permissions.insert(PendingEntry {
            request,
            agent: payload.agent.clone(),
            directory: location.directory.clone(),
        });
    }
    Ok(super::util::json_ok(serde_json::json!({
        "data": { "id": id, "effect": effect }
    })))
}

/// `session.permission.list`.
async fn permission_list(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path(session_id): Path<String>,
) -> Result<Response, ServerError> {
    let requests = ctx
        .v2_permissions
        .for_session(&location.directory, &session_id);
    Ok(super::util::json_ok(
        serde_json::json!({ "data": requests }),
    ))
}

/// `session.permission.get` (`handlers/permission.ts:69-76`).
async fn permission_get(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(_location): axum::Extension<LocationContext>,
    Path((session_id, request_id)): Path<(String, String)>,
) -> Result<Response, ServerError> {
    match ctx.v2_permissions.get(&request_id) {
        Some(request) if request.session_id == session_id => {
            Ok(super::util::json_ok(serde_json::json!({ "data": request })))
        }
        _ => Err(permission_not_found(&request_id).into()),
    }
}

fn permission_not_found(request_id: &str) -> ApiError {
    ApiError::PermissionNotFound {
        request_id: request_id.to_string(),
        message: format!("Permission request not found: {request_id}"),
    }
}

fn owned_request(
    ctx: &ServerContext,
    session_id: &str,
    request_id: &str,
) -> Result<(), ServerError> {
    match ctx.v2_permissions.get(request_id) {
        Some(request) if request.session_id == session_id => Ok(()),
        _ => Err(permission_not_found(request_id).into()),
    }
}

/// The TS payload also carries an optional `message` string that only feeds
/// the (unwired) `CorrectedError` deferred — it is not modeled here, unknown
/// fields are ignored on decode.
#[derive(Deserialize)]
struct ReplyPayload {
    reply: PermissionReply,
}

/// `session.permission.reply` (`core/src/permission.ts:152-216`).
async fn permission_reply(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    Path((session_id, request_id)): Path<(String, String)>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: ReplyPayload = parse_payload(&body)?;
    owned_request(&ctx, &session_id, &request_id)?;
    let project_id = location
        .services
        .instance(&location.directory)
        .map_err(defect)?
        .project
        .id;

    let entry = ctx
        .v2_permissions
        .remove(&request_id)
        .ok_or_else(|| permission_not_found(&request_id))?;
    let request = entry.request.clone();
    let reply_value = serde_json::to_value(payload.reply).map_err(defect)?;
    let publish_replied =
        |session_id: &str, request_id: &str, reply: Value| -> Result<(), ServerError> {
            ctx.bus
                .publish(
                    &PERMISSION_REPLIED,
                    serde_json::json!({
                        "sessionID": session_id,
                        "requestID": request_id,
                        "reply": reply,
                    }),
                    Default::default(),
                )
                .map(|_| ())
                .map_err(defect)
        };
    publish_replied(&session_id, &request_id, reply_value.clone())?;

    if payload.reply == PermissionReply::Reject {
        // Reject the whole session: every other pending request is
        // answered with "reject" (`core/src/permission.ts:177-192`).
        for (id, request, _agent) in ctx
            .v2_permissions
            .entries_for_session(&location.directory, &session_id)
        {
            publish_replied(&session_id, &request.id, serde_json::json!("reject"))?;
            ctx.v2_permissions.remove(&id);
        }
        return Ok(no_content());
    }

    if payload.reply == PermissionReply::Always
        && request.save.as_deref().is_some_and(|s| !s.is_empty())
    {
        // `saved.add` — one row per resource.
        ctx.storage
            .with_connection(|conn| {
                let save = request.save.clone().unwrap_or_default();
                for resource in save {
                    conn.execute(
                        "INSERT OR IGNORE INTO permission (id, project_id, action, resource, time_created, time_updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![
                            PermissionId::generate(),
                            project_id,
                            request.action,
                            resource,
                            now_ms(),
                            now_ms()
                        ],
                    )
                    .map_err(|err| err.to_string())?;
                }
                Ok::<_, String>(())
            })
            .map_err(defect)?;
    }

    if payload.reply != PermissionReply::Always
        || request.save.as_deref().is_none_or(|s| s.is_empty())
    {
        return Ok(no_content());
    }

    // Re-check the remaining pending requests of the session against the
    // updated saved rules (`core/src/permission.ts:198-216`).
    let remembered = saved_rules(&ctx, &project_id)?;
    for (id, request, agent) in ctx
        .v2_permissions
        .entries_for_session(&location.directory, &session_id)
    {
        let ask = AskPayload {
            id: Some(request.id.clone()),
            action: request.action.clone(),
            resources: request.resources.clone(),
            save: request.save.clone(),
            metadata: request.metadata.clone(),
            source: request.source.clone(),
            agent,
        };
        let rules = match configured(&location, &request.session_id, ask.agent.as_deref()) {
            Ok(rules) => rules,
            Err(_) => continue,
        };
        if denied(&ask, &rules) {
            continue;
        }
        let all: Vec<PermissionRule> = rules.iter().chain(remembered.iter()).cloned().collect();
        let allowed = ask.resources.iter().all(|resource| {
            evaluate_permission(&ask.action, resource, &all).effect == PermissionEffect::Allow
        });
        if !allowed {
            continue;
        }
        publish_replied(&session_id, &request.id, serde_json::json!("always"))?;
        ctx.v2_permissions.remove(&id);
    }
    Ok(no_content())
}

fn now_ms() -> i64 {
    use opencode_core::Clock;
    opencode_core::catalog::SystemClock.now_ms() as i64
}
