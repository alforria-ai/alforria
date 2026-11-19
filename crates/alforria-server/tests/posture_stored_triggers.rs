//! What only the running server knows of its posture: the address it
//! binds, and the triggers it has stored.
//!
//! Alone in its file on purpose: creating a `webhook` trigger tightens the
//! process's posture and records the reason, which `GET /server/posture`
//! reports for every server in the process (`posture.rs`,
//! `posture_standalone.rs`).

mod support;

use axum::http::StatusCode;
use serde_json::{json, Value};

use support::{call, fixture_with};

fn posture_of(paths: &alforria_core::GlobalPaths, hostname: &str) -> Vec<&'static str> {
    let options = alforria_server::ListenOptions {
        hostname: hostname.to_string(),
        ..alforria_server::ListenOptions::default()
    };
    let seams = alforria_server::engine::EngineSeams {
        env: alforria_server::engine::no_ambient_env(),
        ..alforria_server::engine::EngineSeams::default()
    };
    let ctx = alforria_server::production_context(&options, paths.clone(), seams).unwrap();
    assert_eq!(
        ctx.policy.shell_env.is_restricted(),
        ctx.policy.unattended_or_remote()
    );
    ctx.policy
        .posture
        .iter()
        .map(|reason| reason.as_str())
        .collect()
}

/// What only the running server knows: the address it binds, and the
/// triggers it has stored. A server bound beyond loopback is restricted;
/// so is the next start of one that was given a `webhook` trigger.
#[tokio::test]
async fn the_listener_and_the_stored_triggers_count() {
    let f = fixture_with(|_| json!({}), None, Vec::new());
    assert_eq!(posture_of(&f.paths, "127.0.0.1"), [""; 0]);
    assert_eq!(posture_of(&f.paths, "0.0.0.0"), ["non-loopback-bind"]);

    // A clock trigger is the server's own doing and changes nothing.
    let dir = format!(
        "directory={}",
        support::urlencode(&f.dirs.repo.display().to_string())
    );
    let create = |body: Value| {
        let (router, uri) = (f.router.clone(), format!("/trigger?{dir}"));
        async move { call(&router, "POST", &uri, None, Some(body)).await }
    };
    let response = create(json!({
        "id": "nightly", "on": { "cron": "0 3 * * *" }, "prompt": "Tidy up"
    }))
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(posture_of(&f.paths, "127.0.0.1"), [""; 0]);

    let response = create(json!({
        "id": "inbox", "on": { "webhook": true }, "prompt": "Triage the delivery"
    }))
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        posture_of(&f.paths, "127.0.0.1"),
        ["webhook-or-watch-trigger"]
    );
    assert_eq!(
        posture_of(&f.paths, "0.0.0.0"),
        ["non-loopback-bind", "webhook-or-watch-trigger"]
    );
}
