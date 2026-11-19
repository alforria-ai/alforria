//! `GET /server/posture` (alforria extension): the admin's view of which
//! posture the agent's processes run in, and why.

mod support;

use axum::http::StatusCode;
use base64::Engine;
use serde_json::{json, Value};

use support::{bearer, body_json, call, fixture_with, Signer};

const AUDIENCE: &str = "agent-42";
const PASSWORD: &str = "hunter2";

fn basic() -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("alforria:{PASSWORD}"));
    format!("Basic {encoded}")
}

// The posture and the reasons it was tightened for are one per process, so
// this file holds one test: the standalone case is in
// `posture_standalone.rs`, and the stored triggers (which tighten the
// process when one is created) are in `posture_stored_triggers.rs`.

/// A server with bearer auth and a webhook trigger reports the restricted
/// posture and each reason for it — to the operator and to an `admin`
/// token, and to no narrower one.
#[tokio::test]
async fn an_unattended_server_reports_why_to_admin_only() {
    let signer = Signer::new();
    let f = fixture_with(
        |dirs| {
            json!({
                "server": {
                    "auth": { "bearer": {
                        "keys": [signer.public_key()],
                        "audience": AUDIENCE,
                    } },
                    "directories": [dirs.repo.display().to_string()]
                },
                "shell_env": { "inherit": ["PATH"] },
                "trigger": {
                    "inbox": { "on": { "webhook": true }, "prompt": "Triage the delivery" }
                }
            })
        },
        Some(PASSWORD),
        Vec::new(),
    );
    assert!(f.ctx.policy.unattended_or_remote());
    let expected: Value = json!({
        "unattendedOrRemote": true,
        "reasons": ["bearer-auth", "webhook-or-watch-trigger"],
        "tightened": [],
        "shellEnv": { "posture": "restricted", "inherit": "configured" }
    });
    for auth in [basic(), bearer(&signer.token(AUDIENCE, "admin"))] {
        let response = call(&f.router, "GET", "/server/posture", Some(&auth), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await, expected);
    }
    for scope in ["chat", "connector"] {
        let auth = bearer(&signer.token(AUDIENCE, scope));
        let response = call(&f.router, "GET", "/server/posture", Some(&auth), None).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{scope}");
    }
    let response = call(&f.router, "GET", "/server/posture", None, None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
