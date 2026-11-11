//! `GET /server/posture` (alforria extension): which posture the server
//! runs the agent's processes in, and why.
//!
//! ```jsonc
//! {
//!   "unattendedOrRemote": true,
//!   "reasons": ["bearer-auth", "non-loopback-bind"],   // empty when false
//!   "tightened": [],                  // reasons that appeared after startup
//!   "shellEnv": {
//!     "posture": "restricted",        // or "standalone": what the next spawn gets
//!     "inherit": "default"            // or "configured": the host set shell_env.inherit
//!   }
//! }
//! ```
//!
//! `unattendedOrRemote` (`ServerPolicy::unattended_or_remote`) decides the
//! shell environment's posture and nothing else. The reasons are
//! `bearer-auth`, `trust-project-restricted`, `non-loopback-bind`,
//! `permission-unattended`, `question-unattended`, `hook-webhook` and
//! `webhook-or-watch-trigger`: `reasons` as they stood when the server
//! started, `tightened` the ones that appeared since (a trigger created
//! at run time, a listener bound beyond loopback). The posture only
//! tightens while the process lives. A session that outside input started
//! gets the restricted environment whatever is reported here.
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
    let tightened = alforria_core::shell_env::tightened();
    // What the next spawn gets: the process-wide policy, not the one
    // loaded at startup.
    let restricted = alforria_core::shell_env::current().is_restricted();
    let inherit = if policy.shell_env.inherit_configured() {
        "configured"
    } else {
        "default"
    };
    Json(json!({
        "unattendedOrRemote": policy.unattended_or_remote() || !tightened.is_empty(),
        "reasons": reasons,
        "tightened": tightened,
        "shellEnv": {
            "posture": if restricted { "restricted" } else { "standalone" },
            "inherit": inherit,
        },
    }))
}
