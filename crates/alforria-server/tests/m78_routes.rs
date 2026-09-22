//! M7.8 acceptance: the sync, workspace, project-copy (v1) and
//! credential/integration/reference/project-copy (v2) route families
//! through the full middleware stack.

use std::sync::Arc;

use alforria_core::{
    session, CoreError, EventBus, RunnerError, SessionError, SessionServices, Storage, WithParts,
};
use alforria_schema::session_v1::V1SessionInfo;
use alforria_server::routes;
use alforria_server::state::{
    AuthConfig, EmptyUiBackend, InstanceStore, ServerContext, SessionEngine,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::future::BoxFuture;
use tower::ServiceExt;

// -----------------------------------------------------------------------
// fixture (mirrors tests/v2_routes.rs)
// -----------------------------------------------------------------------

struct NoJobs;
impl alforria_core::BackgroundJobs for NoJobs {
    fn list(&self) -> Result<Vec<alforria_core::BackgroundJobInfo>, CoreError> {
        Ok(Vec::new())
    }
    fn cancel(&self, _id: &str) -> Result<(), CoreError> {
        Ok(())
    }
}

struct FixedClock;
impl alforria_core::Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        0
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    ctx: Arc<ServerContext>,
    worktree: std::path::PathBuf,
}

fn make_services(dir: &std::path::Path) -> (Arc<SessionServices>, std::path::PathBuf) {
    let worktree = dir.join("repo");
    std::fs::create_dir_all(&worktree).unwrap();
    let storage = Arc::new(Storage::open(dir.join("db.sqlite")).unwrap());
    let agent_input = alforria_core::AgentRegistryInput {
        config: serde_json::from_value(serde_json::json!({})).unwrap(),
        skill_dirs: Vec::new(),
        reference_dirs: Vec::new(),
        worktree: worktree.clone(),
        data_dir: dir.to_path_buf(),
        tmp_dir: dir.to_path_buf(),
        home: dir.to_path_buf(),
    };
    (
        Arc::new(SessionServices::new(
            storage,
            Arc::new(NoJobs),
            Arc::new(FixedClock),
            &agent_input,
        )),
        worktree,
    )
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let (services, worktree) = make_services(dir.path());
    let storage = services.storage.clone();
    let services_for_factory = services.clone();
    let instances =
        InstanceStore::new(Arc::new(move |_directory| Ok(services_for_factory.clone())));
    let mut ctx = ServerContext::new(
        AuthConfig::new("alforria", None),
        instances,
        storage.clone(),
        Arc::new(EventBus::new_shared(storage, None)),
        Vec::new(),
        Arc::new(EmptyUiBackend),
    );
    let engine: Arc<StubEngine> = Arc::new(StubEngine);
    let engine_for_factory = engine.clone();
    ctx.engine_factory =
        Arc::new(move |_location| Ok(engine_for_factory.clone() as Arc<dyn SessionEngine>));
    Fixture {
        _dir: dir,
        ctx: Arc::new(ctx),
        worktree,
    }
}

struct StubEngine;

impl SessionEngine for StubEngine {
    fn prompt(
        &self,
        _input: session::prompt_input::PromptInput,
    ) -> BoxFuture<'static, Result<WithParts, session::prompt_input::PromptError>> {
        Box::pin(async { Ok(stub_message()) })
    }

    fn loop_(
        &self,
        _session_id: String,
    ) -> BoxFuture<'static, Result<WithParts, RunnerError<SessionError>>> {
        Box::pin(async { Ok(stub_message()) })
    }

    fn command(
        &self,
        _input: session::prompt::CommandInput,
    ) -> BoxFuture<'static, Result<WithParts, session::prompt_input::PromptError>> {
        Box::pin(async { Ok(stub_message()) })
    }

    fn shell(
        &self,
        _input: session::prompt::ShellInput,
    ) -> BoxFuture<'static, Result<WithParts, SessionError>> {
        Box::pin(async { Ok(stub_message()) })
    }

    fn revert(
        &self,
        _input: session::revert::RevertInput,
    ) -> BoxFuture<'static, Result<V1SessionInfo, SessionError>> {
        Box::pin(async {
            Err(SessionError::NotFound(alforria_core::NotFoundError {
                message: "no session".to_string(),
            }))
        })
    }

    fn unrevert(
        &self,
        _session_id: String,
    ) -> BoxFuture<'static, Result<V1SessionInfo, SessionError>> {
        Box::pin(async {
            Err(SessionError::NotFound(alforria_core::NotFoundError {
                message: "no session".to_string(),
            }))
        })
    }

    fn cleanup(&self, _session: &V1SessionInfo) -> Result<(), SessionError> {
        Ok(())
    }

    fn diff(
        &self,
        _session_id: &str,
        _message_id: Option<&str>,
    ) -> Result<Vec<alforria_schema::file_diff::SnapshotFileDiff>, SessionError> {
        Ok(Vec::new())
    }

    fn share(&self, _session: &V1SessionInfo) -> Result<(), String> {
        Ok(())
    }

    fn unshare(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
}

fn stub_message() -> WithParts {
    WithParts {
        info: alforria_schema::session_v1::V1Message::User {
            id: session::ids::MessageId::ascending(None).unwrap(),
            session_id: "ses_stub".to_string(),
            time: alforria_schema::session_v1::UserTime { created: 1.0 },
            format: None,
            summary: None,
            agent: "build".to_string(),
            model: alforria_schema::session_v1::V1UserModel {
                provider_id: "test".to_string(),
                model_id: "model".to_string(),
                variant: None,
            },
            system: None,
            tools: None,
        },
        parts: vec![],
    }
}

fn router(fixture: &Fixture) -> axum::Router {
    routes::build_router(fixture.ctx.clone())
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

/// `POST /api/session` — a session with a durable `session.created` event.
async fn create_session(router: &axum::Router, directory: &str) -> String {
    let response = send(
        router,
        "POST",
        "/api/session",
        &serde_json::json!({ "location": { "directory": directory } }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    body["data"]["id"].as_str().expect("session id").to_string()
}

// -----------------------------------------------------------------------
// sync (v1)
// -------------------------------------------------------------------

#[tokio::test]
async fn sync_start_returns_true() {
    let fixture = fixture();
    let router = router(&fixture);
    let response = send(&router, "POST", "/sync/start", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");
}

#[tokio::test]
async fn sync_replay_responds_first_aggregate() {
    let fixture = fixture();
    let router = router(&fixture);
    let directory = fixture.worktree.to_string_lossy().into_owned();
    let session_id = create_session(&router, &directory).await;

    // The stored V1SessionInfo round-trips as the `session.updated` payload.
    let info: V1SessionInfo = fixture
        .ctx
        .instances
        .load(std::path::Path::new(&directory))
        .unwrap()
        .sessions
        .get(&session_id)
        .unwrap();
    let info = serde_json::to_value(&info).unwrap();
    let payload = serde_json::json!({
        "directory": directory,
        "events": [{
            "id": "evt_test_replay",
            "aggregateID": session_id,
            "seq": 1,
            "type": "session.updated.1",
            "data": { "sessionID": session_id, "info": info },
        }],
    })
    .to_string();
    let response = send(&router, "POST", "/sync/replay", &payload).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["sessionID"], serde_json::json!(session_id));
}

#[tokio::test]
async fn sync_replay_rejects_empty_events() {
    let fixture = fixture();
    let router = router(&fixture);
    let response = send(
        &router,
        "POST",
        "/sync/replay",
        &serde_json::json!({ "directory": "/tmp", "events": [] }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn sync_steal_requires_workspace() {
    let fixture = fixture();
    let router = router(&fixture);
    let directory = fixture.worktree.to_string_lossy().into_owned();
    let session_id = create_session(&router, &directory).await;

    // No workspace in the instance context → bare tagged BadRequest.
    let response = send(
        &router,
        "POST",
        "/sync/steal",
        &serde_json::json!({ "sessionID": session_id }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_string(response).await, r#"{"_tag":"BadRequest"}"#);
}

#[tokio::test]
async fn sync_history_filters_by_last_known_seq() {
    let fixture = fixture();
    let router = router(&fixture);
    let directory = fixture.worktree.to_string_lossy().into_owned();
    let session_id = create_session(&router, &directory).await;

    // Full history: the session.created row comes back snake_case.
    let response = send(&router, "POST", "/sync/history", "{}").await;
    assert_eq!(response.status(), StatusCode::OK);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&body_string(response).await).unwrap();
    let row = rows
        .iter()
        .find(|row| row["aggregate_id"] == serde_json::json!(session_id))
        .expect("session row");
    assert_eq!(row["type"], "session.created.1");
    assert_eq!(row["seq"], serde_json::json!(0));

    // Filtering at the current seq hides the row.
    let response = send(
        &router,
        "POST",
        "/sync/history",
        &serde_json::json!({ session_id.clone(): 0 }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(
        !rows
            .iter()
            .any(|row| row["aggregate_id"] == serde_json::json!(session_id)),
        "row filtered at last known seq"
    );
}

// -----------------------------------------------------------------------
// workspace (v1)
// -------------------------------------------------------------------

#[tokio::test]
async fn workspace_adapters_and_list() {
    let fixture = fixture();
    let router = router(&fixture);
    let response = send(&router, "GET", "/experimental/workspace/adapter", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let adapters: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(
        adapters,
        serde_json::json!([{
            "type": "worktree",
            "name": "Worktree",
            "description": "Create a git worktree",
        }])
    );

    // `flags.experimentalWorkspaces` is off by default → empty list.
    let response = send(&router, "GET", "/experimental/workspace", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let list: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(list, serde_json::json!([]));

    let response = send(&router, "GET", "/experimental/workspace/status", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "[]");
}

#[tokio::test]
async fn workspace_create_unknown_adapter_maps_create_error() {
    let fixture = fixture();
    let router = router(&fixture);
    let response = send(
        &router,
        "POST",
        "/experimental/workspace",
        &serde_json::json!({ "type": "nope" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["name"], "WorkspaceCreateError");
    assert_eq!(body["data"]["message"], "Unknown workspace adapter: nope");
}

#[tokio::test]
async fn workspace_warp_missing_workspace_is_404() {
    let fixture = fixture();
    let router = router(&fixture);
    let directory = fixture.worktree.to_string_lossy().into_owned();
    let session_id = create_session(&router, &directory).await;

    // No workspace in the instance context → detach path (204).
    let response = send(
        &router,
        "POST",
        "/experimental/workspace/warp",
        &serde_json::json!({
            "id": null,
            "sessionID": session_id,
        })
        .to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // A missing workspace id → ApiNotFoundError.
    let response = send(
        &router,
        "POST",
        "/experimental/workspace/warp",
        &serde_json::json!({
            "id": "wrk_missing",
            "sessionID": session_id,
        })
        .to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["name"], "NotFoundError");
    assert_eq!(body["data"]["message"], "Workspace not found: wrk_missing");
}

#[tokio::test]
async fn workspace_remove_missing_is_null() {
    let fixture = fixture();
    let router = router(&fixture);
    let response = send(&router, "DELETE", "/experimental/workspace/wrk_missing", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "null");
}

// -----------------------------------------------------------------------
// project-copy generate-name (v1)
// -------------------------------------------------------------------

#[tokio::test]
async fn generate_name_empty_context_is_a_slug() {
    let fixture = fixture();
    let router = router(&fixture);
    let response = send(
        &router,
        "POST",
        "/experimental/project/proj/copy/generate-name",
        &serde_json::json!({ "context": "   " }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let name = body["name"].as_str().expect("name");
    assert!(!name.is_empty(), "slug fallback");
    assert!(
        name.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "slug chars: {name}"
    );
}

#[tokio::test]
async fn generate_name_accepts_missing_context() {
    let fixture = fixture();
    let router = router(&fixture);
    let response = send(
        &router,
        "POST",
        "/experimental/project/proj/copy/generate-name",
        "{}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(!body["name"].as_str().unwrap().is_empty());
}

// -----------------------------------------------------------------------
// v2 integration / credential / reference
// -------------------------------------------------------------------

#[tokio::test]
async fn v2_integration_list_get_and_errors() {
    let fixture = fixture();
    let router = router(&fixture);
    let directory = fixture.worktree.to_string_lossy().into_owned();
    let location = urlencode(&directory);

    // Empty registry — lists `[]` and get `null` through the envelope.
    let response = send(
        &router,
        "GET",
        &format!("/api/integration?location={location}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(
        body["data"],
        serde_json::json!([]),
        "empty registry lists []"
    );
    // The envelope carries the resolved location info.
    assert!(body["location"]["directory"].is_string());

    let response = send(
        &router,
        "GET",
        &format!("/api/integration/missing?location={location}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["data"], serde_json::Value::Null);

    // Key connect on a missing integration → 400 integration_authorization.
    let response = send(
        &router,
        "POST",
        &format!("/api/integration/missing/connect/key?location={location}"),
        &serde_json::json!({ "key": "secret" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["_tag"], "InvalidRequestError");
    assert_eq!(body["message"], "Authentication failed");
    assert_eq!(body["kind"], "integration_authorization");

    // Attempt status for an unknown attempt → defect-style 400/500 —
    // the attempt lifecycle is core-tested; here only the empty
    // registry shape is asserted.

    // OAuth connect on a missing integration → 400 too.
    let response = send(
        &router,
        "POST",
        &format!("/api/integration/missing/connect/oauth?location={location}"),
        &serde_json::json!({ "methodID": "oauth", "inputs": {} }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn v2_credential_update_remove_missing_are_204() {
    let fixture = fixture();
    let router = router(&fixture);
    let directory = fixture.worktree.to_string_lossy().into_owned();
    let location = urlencode(&directory);

    let response = send(
        &router,
        "PATCH",
        &format!("/api/credential/cred_missing?location={location}"),
        &serde_json::json!({ "label": "renamed" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = send(
        &router,
        "DELETE",
        &format!("/api/credential/cred_missing?location={location}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn v2_reference_lists_configured_references() {
    let fixture = fixture();
    let router = router(&fixture);
    let directory = fixture.worktree.to_string_lossy().into_owned();
    let location = urlencode(&directory);

    // No references configured — empty list through the envelope.
    let response = send(
        &router,
        "GET",
        &format!("/api/reference?location={location}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["data"], serde_json::json!([]));
}

// -----------------------------------------------------------------------
// v2 project-copy
// -------------------------------------------------------------------

#[tokio::test]
async fn v2_project_copy_source_not_found_maps_project_copy_error() {
    let fixture = fixture();
    let router = router(&fixture);
    let directory = fixture.worktree.to_string_lossy().into_owned();
    let location = urlencode(&directory);
    let response = send(
        &router,
        "POST",
        &format!("/experimental/project/proj/copy?location={location}"),
        &serde_json::json!({
            "strategy": "git_worktree",
            "directory": "/tmp/opencode-copy-dest",
            "name": "copy",
        })
        .to_string(),
    )
    .await;
    // The project directory itself is not registered in project_directory
    // (the route resolves the source through the project registry), so
    // `create` fails with SourceDirectoryNotFound → 400 ProjectCopyError.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["name"], "ProjectCopyError");
    assert!(body["data"]["message"]
        .as_str()
        .unwrap()
        .starts_with("Project copy source not found: "));
}

#[tokio::test]
async fn v2_project_copy_refresh_is_204() {
    let fixture = fixture();
    let router = router(&fixture);
    let directory = fixture.worktree.to_string_lossy().into_owned();
    let location = urlencode(&directory);
    let response = send(
        &router,
        "POST",
        &format!("/experimental/project/proj/copy/refresh?location={location}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

// -----------------------------------------------------------------------
// helpers
// -------------------------------------------------------------------

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
