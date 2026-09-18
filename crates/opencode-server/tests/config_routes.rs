//! M6.6 acceptance: the v1 config, permission, question and provider route
//! families against the full middleware stack.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use opencode_core::{EventBus, SessionServices, Storage};
use opencode_server::routes;
use opencode_server::state::{AuthConfig, EmptyUiBackend, InstanceStore, ServerContext};
use tower::ServiceExt;

// -----------------------------------------------------------------------
// fixture (mirrors session_routes.rs)
// -----------------------------------------------------------------------

struct NoJobs;
impl opencode_core::BackgroundJobs for NoJobs {
    fn list(&self) -> Result<Vec<opencode_core::BackgroundJobInfo>, opencode_core::CoreError> {
        Ok(Vec::new())
    }
    fn cancel(&self, _id: &str) -> Result<(), opencode_core::CoreError> {
        Ok(())
    }
}

struct FixedClock;
impl opencode_core::Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        0
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    ctx: Arc<ServerContext>,
    worktree: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let worktree = dir.path().join("repo");
    std::fs::create_dir_all(&worktree).unwrap();
    let storage = Arc::new(Storage::open(dir.path().join("db.sqlite")).unwrap());
    let agent_input = opencode_core::AgentRegistryInput {
        config: serde_json::from_value(serde_json::json!({})).unwrap(),
        skill_dirs: Vec::new(),
        reference_dirs: Vec::new(),
        worktree: worktree.clone(),
        data_dir: dir.path().to_path_buf(),
        tmp_dir: dir.path().to_path_buf(),
        home: dir.path().to_path_buf(),
    };
    let services = Arc::new(SessionServices::new(
        storage.clone(),
        Arc::new(NoJobs),
        Arc::new(FixedClock),
        &agent_input,
    ));
    let services_for_factory = services.clone();
    let instances =
        InstanceStore::new(Arc::new(move |_directory| Ok(services_for_factory.clone())));
    let ctx = ServerContext::new(
        AuthConfig::new("opencode", None),
        instances,
        storage.clone(),
        Arc::new(EventBus::new_shared(storage, None)),
        Vec::new(),
        Arc::new(EmptyUiBackend),
    );
    Fixture {
        _dir: dir,
        ctx: Arc::new(ctx),
        worktree,
    }
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: &str,
) -> axum::http::Response<Body> {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn body_string(response: axum::http::Response<Body>) -> String {
    String::from_utf8(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

fn urlencode(input: &str) -> String {
    let mut out = String::new();
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn instance_uri(worktree: &std::path::Path, rest: &str) -> String {
    format!(
        "/{rest}?directory={}",
        urlencode(&worktree.to_string_lossy())
    )
}

// -----------------------------------------------------------------------
// config (`handlers/config.ts`)
// -----------------------------------------------------------------------

#[tokio::test]
async fn config_get_returns_merged_config() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let response = send(&router, "GET", &instance_uri(&f.worktree, "config"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let config: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(config.is_object());
}

#[tokio::test]
async fn config_update_merges_into_config_json() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let response = send(
        &router,
        "PATCH",
        &instance_uri(&f.worktree, "config"),
        r#"{"shell":"/bin/sh","theme":"dark"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let echoed: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(echoed["shell"], "/bin/sh");
    assert_eq!(echoed["theme"], "dark");

    let written = std::fs::read_to_string(f.worktree.join("config.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(value["shell"], "/bin/sh");
    assert_eq!(value["theme"], "dark");

    // Invalid payloads are rejected before the file is touched.
    std::fs::remove_file(f.worktree.join("config.json")).unwrap();
    let response = send(
        &router,
        "PATCH",
        &instance_uri(&f.worktree, "config"),
        r#"{"autoupdate":"yes-please"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!f.worktree.join("config.json").exists());
}

#[tokio::test]
async fn config_providers_shape() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let response = send(
        &router,
        "GET",
        &instance_uri(&f.worktree, "config/providers"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(value["providers"].is_array());
    assert!(value["default"].is_object());
}

// -----------------------------------------------------------------------
// permission (`handlers/permission.ts`)
// -----------------------------------------------------------------------

#[tokio::test]
async fn permission_list_and_reply() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let response = send(&router, "GET", &instance_uri(&f.worktree, "permission"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "[]");

    // A requestID without the `per` prefix is a Params schema rejection.
    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "permission/bad/reply"),
        r#"{"reply":"once"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // An unknown-but-well-formed request is the 404 PermissionNotFoundError
    // (`{"_tag", fields}` — Effect TaggedErrorClass serialization).
    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "permission/per_000/reply"),
        r#"{"reply":"once"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_string(response).await,
        r#"{"_tag":"PermissionNotFoundError","requestID":"per_000","message":"Permission request not found: per_000"}"#
    );
}

// -----------------------------------------------------------------------
// question (`handlers/question.ts`)
// -----------------------------------------------------------------------

#[tokio::test]
async fn question_list_reply_and_reject() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let response = send(&router, "GET", &instance_uri(&f.worktree, "question"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "[]");

    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "question/bad/reply"),
        r#"{"answers":[]}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "question/que_000/reply"),
        r#"{"answers":[["yes"]]}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_string(response).await,
        r#"{"_tag":"QuestionNotFoundError","requestID":"que_000","message":"Question request not found: que_000"}"#
    );

    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "question/que_000/reject"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// -----------------------------------------------------------------------
// provider (`handlers/provider.ts`)
// -----------------------------------------------------------------------

#[tokio::test]
async fn provider_list_and_auth_methods() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let response = send(&router, "GET", &instance_uri(&f.worktree, "provider"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(value["all"].is_array());
    assert!(value["default"].is_object());
    assert!(value["connected"].is_array());

    let response = send(
        &router,
        "GET",
        &instance_uri(&f.worktree, "provider/auth"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "{}");
}

#[tokio::test]
async fn provider_oauth_authorize_and_callback_map_errors() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

    // Invalid JSON body → the raw-authorize BadRequest wire shape.
    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "provider/anthropic/oauth/authorize"),
        "{not json}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_string(response).await,
        r#"{"name":"BadRequest","data":{}}"#
    );

    // The unwired ProviderAuth seam answers the callback with BadRequest.
    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "provider/anthropic/oauth/callback"),
        r#"{"method":0}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_string(response).await,
        r#"{"name":"BadRequest","data":{}}"#
    );
}
