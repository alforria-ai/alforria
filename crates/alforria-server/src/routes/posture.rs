//! `GET /server/posture` (alforria extension): which posture the server
//! runs the agent's processes in, and why.
//!
//! ```jsonc
//! {
//!   "unattendedOrRemote": true,
//!   "reasons": ["bearer-auth", "non-loopback-bind"],   // empty when false
//!   "shellEnv": {
//!     "posture": "restricted",        // or "standalone"
//!     "inherit": "default"            // or "configured": the host set shell_env.inherit
//!   }
//! }
//! ```
//!
//! `unattendedOrRemote` (`ServerPolicy::unattended_or_remote`) decides the
//! shell environment's posture and nothing else. The reasons are
//! `bearer-auth`, `trust-project-restricted`, `non-loopback-bind`,
//! `permission-unattended`, `question-unattended`, `hook-webhook` and
//! `webhook-or-watch-trigger`, as they stood when the server started.
//!
//! Outside the frozen tables and absent from the scope table, so a bearer
//! credential needs `admin`.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::state::ServerContext;

pub fn register(router: Router<Arc<ServerContext>>) -> Router<Arc<ServerContext>> {
    router.route("/server/posture", get(show))
}

async fn show(State(ctx): State<Arc<ServerContext>>) -> Json<Value> {
    let policy = &ctx.policy;
    let reasons: Vec<&str> = policy
        .posture
        .iter()
        .map(|reason| reason.as_str())
        .collect();
    Json(json!({
        "unattendedOrRemote": policy.unattended_or_remote(),
        "reasons": reasons,
        "shellEnv": {
            "posture": if policy.shell_env.is_restricted() { "restricted" } else { "standalone" },
            "inherit": if policy.shell_env.inherit_configured() { "configured" } else { "default" },
        },
    }))
}
