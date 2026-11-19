//! `GET /attention` (alforria extension): everything that is waiting for a
//! person, across every loaded project.
//!
//! `permission.asked` and `question.asked` are events: a client that was
//! not connected when one fired never hears of it. This is the state behind
//! them, for a control plane to poll as a safety net:
//!
//! ```jsonc
//! {
//!   "count": 1,
//!   "items": [{
//!     "kind": "permission",            // or "question"
//!     "id": "per_…",                   // what a reply names
//!     "sessionID": "ses_…",
//!     "title": "Fix the login page",   // the session's title
//!     "directory": "/home/agent/workspace",
//!     "summary": "bash: git push",     // what is being asked, briefly
//!     "asked": 1794408349584,          // ms since the epoch
//!     "age": 5321,                     // ms
//!     "expires": 1794408649584         // when the unattended policy answers, or null
//!   }]
//! }
//! ```
//!
//! Oldest first. A credential below `admin` sees what it could answer and
//! nothing else: asks in the projects it may open (`server.directories`)
//! and, there, in the sessions it may act in (restricted ones — the same
//! line `confine::check_ask_session` draws for a reply). An operator's
//! session waiting in the same project is the operator's. Outside the
//! frozen v1/v2 tables.
//!
//! For the operator (no authentication, Basic, or an `admin` token) on a
//! server with `hook.webhook` configured there is one more member,
//! `"outbox": { "pending": 0, "failed": 0, "dropped": 0 }`: deliveries still
//! owed, deliveries given up that `hook.outbox.retention` still keeps, and
//! undelivered events this process dropped beyond `hook.outbox.max_pending`
//! since it started. The numbers are the whole server's, across every
//! directory, so a credential below `admin` never gets them. Without hooks
//! the member is absent.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde_json::{json, Value};

use crate::bearer::Principal;
use crate::state::ServerContext;

pub fn register(router: Router<Arc<ServerContext>>) -> Router<Arc<ServerContext>> {
    router.route("/attention", get(attention))
}

/// One line saying what a permission ask is for.
fn permission_summary(request: &alforria_schema::permission_v1::PermissionV1Request) -> String {
    let what = request
        .metadata
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| request.patterns.first().cloned())
        .unwrap_or_default();
    let what: String = what.chars().take(200).collect();
    if what.is_empty() {
        request.permission.clone()
    } else {
        format!("{}: {what}", request.permission)
    }
}

async fn attention(
    State(ctx): State<Arc<ServerContext>>,
    principal: Option<Extension<Principal>>,
) -> Json<Value> {
    let restricted = crate::confine::restricted(&principal);
    let now = alforria_core::session::permission::now_ms();
    let mut items: Vec<(u64, Value)> = Vec::new();
    for services in ctx.instances.cached() {
        if restricted && !crate::confine::instance_allowed(&ctx, &services) {
            continue;
        }
        let directory = services
            .instance_location()
            .map(|instance| instance.directory.display().to_string());
        let title = |session_id: &str| {
            services
                .sessions
                .get(session_id)
                .ok()
                .map(|session| session.title)
        };
        // A credential below `admin` is shown the asks it may answer.
        let visible =
            |session_id: &str| !restricted || crate::confine::may_act_on(&ctx, session_id).is_ok();
        for pending in services.permission.pending() {
            if !visible(&pending.request.session_id) {
                continue;
            }
            items.push((
                pending.asked_at,
                json!({
                    "kind": "permission",
                    "id": pending.request.id,
                    "sessionID": pending.request.session_id,
                    "title": title(&pending.request.session_id),
                    "directory": directory,
                    "summary": permission_summary(&pending.request),
                    "asked": pending.asked_at,
                    "age": now.saturating_sub(pending.asked_at),
                    "expires": pending.expires_at,
                }),
            ));
        }
        for pending in services.question.pending_asks() {
            if !visible(&pending.request.session_id) {
                continue;
            }
            let summary: String = pending
                .request
                .questions
                .first()
                .map(|question| question.question.chars().take(200).collect())
                .unwrap_or_default();
            items.push((
                pending.asked_at,
                json!({
                    "kind": "question",
                    "id": pending.request.id,
                    "sessionID": pending.request.session_id,
                    "title": title(&pending.request.session_id),
                    "directory": directory,
                    "summary": summary,
                    "asked": pending.asked_at,
                    "age": now.saturating_sub(pending.asked_at),
                    "expires": pending.expires_at,
                }),
            ));
        }
    }
    items.sort_by_key(|(asked, _)| *asked);
    let items: Vec<Value> = items.into_iter().map(|(_, item)| item).collect();
    let mut body = json!({ "count": items.len(), "items": items });
    // The webhook outbox in numbers, on a server that has hooks: what is
    // still owed, what was given up, and what was dropped for want of room.
    // They count every directory's events: the operator's to read.
    if !restricted && ctx.outbox.enabled() {
        let stats = ctx.outbox.stats();
        body["outbox"] = json!({
            "pending": stats.pending,
            "failed": stats.failed,
            "dropped": stats.dropped,
        });
    }
    Json(body)
}
