//! M6.3 acceptance: the location & workspace-routing middleware matrix,
//! exercised through probe handlers behind the full middleware stack
//! (mirrors `httpapi-workspace-routing.test.ts`'s probe API).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use alforria_core::{
    CreateInput, EventBus, SessionContext, SessionError, SessionServices, Storage,
};
use alforria_server::middleware::apply_stack;
use alforria_server::middleware::location::LocationLayer;
use alforria_server::state::{
    AuthConfig, EmptyUiBackend, InstanceFactory, InstanceStore, ServerContext,
};
use axum::body::Body;
use axum::http::{header, Request, Response, StatusCode};
use axum::routing::get;
use tower::ServiceExt;

/// A context whose instance factory records every load and returns a real
/// service graph.
struct Fixture {
    ctx: Arc<ServerContext>,
    loads: Arc<Mutex<Vec<String>>>,
}

fn test_services() -> SessionServices {
    let dir = tempfile::tempdir().unwrap();
    let worktree = dir.path().join("repo");
    std::fs::create_dir_all(&worktree).unwrap();
    let storage = Arc::new(Storage::open(dir.path().join("db.sqlite")).unwrap());
    let agent_input = alforria_core::AgentRegistryInput {
        config: serde_json::from_value(serde_json::json!({})).unwrap(),
        skill_dirs: Vec::new(),
        reference_dirs: Vec::new(),
        worktree: worktree.clone(),
        data_dir: dir.path().to_path_buf(),
        tmp_dir: dir.path().to_path_buf(),
        home: dir.path().to_path_buf(),
    };
    SessionServices::new(
        storage,
        Arc::new(NoJobs),
        Arc::new(FixedClock),
        &agent_input,
    )
}

struct NoJobs;
impl alforria_core::BackgroundJobs for NoJobs {
    fn list(&self) -> Result<Vec<alforria_core::BackgroundJobInfo>, alforria_core::CoreError> {
        Ok(Vec::new())
    }
    fn cancel(&self, _id: &str) -> Result<(), alforria_core::CoreError> {
        Ok(())
    }
}

struct FixedClock;
impl alforria_core::Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        0
    }
}

fn fixture() -> Fixture {
    let storage = Arc::new(Storage::open_in_memory().unwrap());
    let bus = Arc::new(EventBus::new_shared(storage.clone(), None));
    let loads = Arc::new(Mutex::new(Vec::new()));
    let loads_for_factory = loads.clone();
    let factory: InstanceFactory = Arc::new(move |directory: &std::path::Path| {
        loads_for_factory
            .lock()
            .unwrap()
            .push(directory.display().to_string());
        Ok(Arc::new(test_services()))
    });
    let ctx = Arc::new(ServerContext::new(
        AuthConfig::new("alforria", None),
        InstanceStore::new(factory),
        storage,
        bus,
        Vec::new(),
        Arc::new(EmptyUiBackend),
    ));
    Fixture { ctx, loads }
}

/// The probe endpoint: echoes the resolved LocationContext as JSON.
async fn probe(req: Request<Body>) -> axum::response::Response {
    let payload = match req
        .extensions()
        .get::<alforria_server::middleware::location::LocationContext>()
    {
        Some(location) => serde_json::json!({
            "present": true,
            "directory": location.directory,
            "workspaceID": location.workspace_id,
        }),
        None => serde_json::json!({ "present": false }),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap()
}

fn probe_router(ctx: &Arc<ServerContext>) -> axum::Router {
    let router = axum::Router::new()
        .route("/agent", get(probe))
        .route("/session", get(probe))
        .route("/session/{sessionID}", get(probe))
        .route("/session/{sessionID}/message", get(probe))
        .route("/global/health", get(probe))
        .route("/api/agent", get(probe))
        .route("/api/health", get(probe))
        .route("/api/session/{sessionID}", get(probe))
        .with_state(ctx.clone());
    apply_stack(router, ctx)
}

async fn body_string(response: Response<Body>) -> String {
    String::from_utf8(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

async fn probe_json(router: &axum::Router, uri: &str) -> serde_json::Value {
    let response = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    let body = body_string(response).await;
    serde_json::from_str(&body).unwrap()
}

async fn probe_json_headers(
    router: &axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> serde_json::Value {
    let mut builder = Request::builder().uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    let body = body_string(response).await;
    serde_json::from_str(&body).unwrap()
}

fn create_session(
    ctx: &ServerContext,
    id: &str,
    directory: &str,
    workspace_id: Option<String>,
) -> Result<(), SessionError> {
    ctx.storage
        .with_connection(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO project (id, worktree, sandboxes, time_created, time_updated)
                 VALUES ('prj_test', '/repo', '[]', 1, 1)",
                [],
            )
        })
        .unwrap();
    ctx.sessions
        .create(
            &SessionContext {
                project_id: "prj_test".to_string(),
                directory: PathBuf::from(directory),
                worktree: PathBuf::from(directory),
                workspace_id: None,
            },
            &CreateInput {
                id: Some(id.to_string()),
                directory: Some(directory.to_string()),
                workspace_id,
                ..Default::default()
            },
        )
        .map(|_| ())
}

// ---------------------------------------------------------------- v1

#[tokio::test]
async fn v1_directory_query_header_and_cwd_fallback() {
    let fixture = fixture();
    let router = probe_router(&fixture.ctx);

    // ?directory= wins over the header.
    let probe = probe_json(
        &router,
        "/agent?directory=%2Frepo&x-opencode-directory=ignored",
    )
    .await;
    assert_eq!(probe["directory"], "/repo");

    // The header applies when the query param is absent.
    let probe = probe_json_headers(&router, "/agent", &[("x-opencode-directory", "/hdr")]).await;
    assert_eq!(probe["directory"], "/hdr");

    // An empty ?directory= is falsy — the header applies.
    let probe = probe_json_headers(
        &router,
        "/agent?directory=",
        &[("x-opencode-directory", "/hdr")],
    )
    .await;
    assert_eq!(probe["directory"], "/hdr");

    // cwd fallback.
    let probe = probe_json(&router, "/agent").await;
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(probe["directory"], cwd.display().to_string());
}

#[tokio::test]
async fn v1_percent_decodes_the_resolved_directory() {
    let fixture = fixture();
    let router = probe_router(&fixture.ctx);

    // The instance-context decode is applied after searchParams decoding:
    // %2520 → "%20" → " " (instance-context.ts:15-21).
    let probe = probe_json(&router, "/agent?directory=/a%2520b").await;
    assert_eq!(probe["directory"], "/a b");

    // The v1 header is decoded the same way.
    let probe = probe_json_headers(&router, "/agent", &[("x-opencode-directory", "/a%20b")]).await;
    assert_eq!(probe["directory"], "/a b");
}

#[tokio::test]
async fn v1_unknown_workspace_is_a_text_500() {
    let fixture = fixture();
    let router = probe_router(&fixture.ctx);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/agent?workspace=wrk_missing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
    assert_eq!(
        body_string(response).await,
        "Workspace not found: wrk_missing"
    );
}

#[tokio::test]
async fn v1_invalid_workspace_id_is_a_defect_500() {
    // WorkspaceV2.ID.make throws on ids without the `wrk` prefix
    // (workspace-routing.ts:71).
    let fixture = fixture();
    let router = probe_router(&fixture.ctx);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/agent?workspace=nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_string(response).await;
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["name"], "UnknownError");
}

#[tokio::test]
async fn v1_env_workspace_id_bypasses_the_lookup() {
    let fixture = fixture();
    let router = axum::Router::new()
        .route("/agent", get(probe))
        .with_state(fixture.ctx.clone())
        .layer(LocationLayer::with_env_workspace_id(
            fixture.ctx.clone(),
            Some("wrk_env".to_string()),
        ));
    let probe = probe_json(&router, "/agent?workspace=wrk_any").await;
    assert_eq!(probe["workspaceID"], "wrk_env");
}

#[tokio::test]
async fn v1_session_derived_directory() {
    let fixture = fixture();
    create_session(&fixture.ctx, "ses_route", "/repo/work", None).unwrap();
    let router = probe_router(&fixture.ctx);

    // The session's directory wins over query hints.
    let probe = probe_json(&router, "/session/ses_route?directory=/elsewhere").await;
    assert_eq!(probe["directory"], "/repo/work");
    assert_eq!(probe["workspaceID"], serde_json::Value::Null);

    // A missing session routes like no session at all.
    let probe = probe_json(&router, "/session/ses_missing?directory=/elsewhere").await;
    assert_eq!(probe["directory"], "/elsewhere");
    assert_eq!(probe["workspaceID"], serde_json::Value::Null);

    // /session/status never derives from a session.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/session/status?directory=/status-dir")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let probe: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(probe["directory"], "/status-dir");
}

#[tokio::test]
async fn v1_session_workspace_routes_into_workspace_selection() {
    // A session carrying a workspaceID selects that workspace — with no
    // workspace registry (M7) the plan is MissingWorkspace
    // (workspace-routing.ts:173-175).
    let fixture = fixture();
    create_session(
        &fixture.ctx,
        "ses_ws",
        "/repo/work",
        Some("wrk_ses".to_string()),
    )
    .unwrap();
    let router = probe_router(&fixture.ctx);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/session/ses_ws")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body_string(response).await, "Workspace not found: wrk_ses");
}

#[tokio::test]
async fn v1_invalid_session_id_in_path_is_a_defect_500() {
    // SessionID.make throws for non-`ses` ids
    // (shared/workspace-routing.ts:20-29).
    let fixture = fixture();
    let router = probe_router(&fixture.ctx);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/session/not-a-session-id")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_string(response).await;
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["name"], "UnknownError");
}

// ---------------------------------------------------------------- v2

#[tokio::test]
async fn v2_location_query_and_headers() {
    let fixture = fixture();
    let router = probe_router(&fixture.ctx);

    let probe = probe_json(&router, "/api/agent?location[directory]=%2Frepo").await;
    assert_eq!(probe["directory"], "/repo");

    let probe = probe_json_headers(
        &router,
        "/api/agent?location[directory]=%2Fquery",
        &[("x-opencode-directory", "/header")],
    )
    .await;
    assert_eq!(probe["directory"], "/query");

    // The header is percent-decoded best-effort (location.ts:41-47).
    let probe =
        probe_json_headers(&router, "/api/agent", &[("x-opencode-directory", "/a%20b")]).await;
    assert_eq!(probe["directory"], "/a b");

    // The workspace ref carries through without any registry lookup.
    let probe = probe_json(
        &router,
        "/api/agent?location[directory]=%2Frepo&location[workspace]=wrk_1",
    )
    .await;
    assert_eq!(probe["directory"], "/repo");
    assert_eq!(probe["workspaceID"], "wrk_1");

    let probe = probe_json_headers(
        &router,
        "/api/agent",
        &[("x-opencode-workspace", "wrk_hdr")],
    )
    .await;
    assert_eq!(probe["workspaceID"], "wrk_hdr");

    let probe = probe_json(&router, "/api/agent").await;
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(probe["directory"], cwd.display().to_string());
}

#[tokio::test]
async fn v2_invalid_workspace_id_is_a_defect_500() {
    let fixture = fixture();
    let router = probe_router(&fixture.ctx);
    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/agent?location[workspace]=nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_string(response).await;
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["name"], "UnknownError");
}

#[tokio::test]
async fn v2_session_location_matrix() {
    let fixture = fixture();
    create_session(
        &fixture.ctx,
        "ses_v2",
        "/repo/v2",
        Some("wrk_v2".to_string()),
    )
    .unwrap();
    let router = probe_router(&fixture.ctx);

    // Invalid session id → 400 InvalidRequestError.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/session/bogus")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_string(response).await,
        "{\"_tag\":\"InvalidRequestError\",\"message\":\"Invalid session ID\",\"field\":\"sessionID\"}"
    );

    // Missing session → 404 SessionNotFoundError.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/session/ses_missing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_string(response).await,
        "{\"_tag\":\"SessionNotFoundError\",\"sessionID\":\"ses_missing\",\"message\":\"Session not found: ses_missing\"}"
    );

    // Existing session → its directory and workspace.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/session/ses_v2?location[directory]=%2Fignored")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let probe: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(probe["directory"], "/repo/v2");
    assert_eq!(probe["workspaceID"], "wrk_v2");
}

// ---------------------------------------------------------------- surfaces

#[tokio::test]
async fn routes_without_location_middleware_get_no_context() {
    let fixture = fixture();
    let router = probe_router(&fixture.ctx);
    let probe = probe_json(&router, "/global/health").await;
    assert_eq!(probe["present"], false);
    let probe = probe_json(&router, "/api/health").await;
    assert_eq!(probe["present"], false);
    let loads = fixture.loads.lock().unwrap().len();
    assert_eq!(loads, 0, "no instance may load for non-location routes");
}

#[tokio::test]
async fn instances_are_cached_per_directory_until_disposed() {
    let fixture = fixture();
    let router = probe_router(&fixture.ctx);

    probe_json(&router, "/agent?directory=/repo").await;
    probe_json(&router, "/agent?directory=/repo/x/../").await;
    probe_json(&router, "/agent?directory=/repo").await;
    assert_eq!(fixture.loads.lock().unwrap().len(), 1);

    fixture
        .ctx
        .instances
        .dispose_directory(std::path::Path::new("/repo"));
    probe_json(&router, "/agent?directory=/repo").await;
    assert_eq!(fixture.loads.lock().unwrap().len(), 2);

    fixture.ctx.instances.dispose_all();
    probe_json(&router, "/agent?directory=/repo").await;
    assert_eq!(fixture.loads.lock().unwrap().len(), 3);
}
