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
