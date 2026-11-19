//! `GET /server/posture` on a server with none of the unattended features.
//!
//! Alone in its file on purpose: the shell posture is one per process and
//! only ever tightens, so this needs a process no other test has restricted
//! (the restricted cases are in `posture.rs`).

mod support;

use axum::http::StatusCode;
use serde_json::json;

use support::{body_json, call, fixture_with};

/// A developer's own loopback server with none of the unattended
/// features: standalone, and it says so.
#[tokio::test]
async fn a_local_server_reports_the_standalone_posture() {
    let f = fixture_with(|_| json!({}), None, Vec::new());
    assert!(!f.ctx.policy.unattended_or_remote());
    let response = call(&f.router, "GET", "/server/posture", None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await,
        json!({
            "unattendedOrRemote": false,
            "reasons": [],
            "tightened": [],
            "shellEnv": { "posture": "standalone", "inherit": "default" }
        })
    );
}
