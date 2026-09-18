//! M6.7 acceptance: the v2 `/api/*` families against the real M5 services
//! plus a stub [`SessionEngine`], through the full middleware stack.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::future::BoxFuture;
use opencode_core::{
    session, CoreError, EventBus, RunnerError, SessionError, SessionServices, Storage, WithParts,
};
use opencode_schema::file_diff::{FileDiffStatus, SnapshotFileDiff};
use opencode_schema::session_v1::{UserTime, V1Message, V1SessionInfo};
use opencode_server::routes;
use opencode_server::state::{
    AuthConfig, EmptyUiBackend, InstanceStore, ServerContext, SessionEngine,
};
use tower::ServiceExt;

// -----------------------------------------------------------------------
// fixture (mirrors tests/session_routes.rs)
// -----------------------------------------------------------------------

struct NoJobs;
impl opencode_core::BackgroundJobs for NoJobs {
    fn list(&self) -> Result<Vec<opencode_core::BackgroundJobInfo>, CoreError> {
        Ok(Vec::new())
    }
    fn cancel(&self, _id: &str) -> Result<(), CoreError> {
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

fn make_services(dir: &std::path::Path) -> (Arc<SessionServices>, std::path::PathBuf) {
    let worktree = dir.join("repo");
    std::fs::create_dir_all(&worktree).unwrap();
    let storage = Arc::new(Storage::open(dir.join("db.sqlite")).unwrap());
    let agent_input = opencode_core::AgentRegistryInput {
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
        AuthConfig::new("opencode", None),
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

/// `POST /api/session` with a `location.directory` payload — the v2 create.
async fn create_session(router: &axum::Router, directory: &str) -> serde_json::Value {
    let response = send(
        router,
        "POST",
        "/api/session",
        &serde_json::json!({ "location": { "directory": directory } }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_str(&body_string(response).await).unwrap()
}

struct StubEngine;

impl SessionEngine for StubEngine {
    fn prompt(
        &self,
        input: session::prompt_input::PromptInput,
    ) -> BoxFuture<'static, Result<WithParts, session::prompt_input::PromptError>> {
        Box::pin(async move { Ok(stub_message(&input.session_id)) })
    }

    fn loop_(
        &self,
        session_id: String,
    ) -> BoxFuture<'static, Result<WithParts, RunnerError<SessionError>>> {
        Box::pin(async move { Ok(stub_message(&session_id)) })
    }

    fn command(
        &self,
        input: session::prompt::CommandInput,
    ) -> BoxFuture<'static, Result<WithParts, session::prompt_input::PromptError>> {
        Box::pin(async move { Ok(stub_message(&input.session_id)) })
    }

    fn shell(
        &self,
        input: session::prompt::ShellInput,
    ) -> BoxFuture<'static, Result<WithParts, SessionError>> {
        Box::pin(async move { Ok(stub_message(&input.session_id)) })
    }

    fn revert(
        &self,
        _input: session::revert::RevertInput,
    ) -> BoxFuture<'static, Result<V1SessionInfo, SessionError>> {
        Box::pin(async move {
            Err(SessionError::NotFound(opencode_core::NotFoundError {
                message: "no session".to_string(),
            }))
        })
    }

    fn unrevert(
        &self,
        _session_id: String,
    ) -> BoxFuture<'static, Result<V1SessionInfo, SessionError>> {
        Box::pin(async move {
            Err(SessionError::NotFound(opencode_core::NotFoundError {
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
    ) -> Result<Vec<SnapshotFileDiff>, SessionError> {
        Ok(vec![SnapshotFileDiff {
            file: None,
            patch: None,
            additions: 0.0,
            deletions: 0.0,
            status: Some(FileDiffStatus::Added),
        }])
    }

    fn share(&self, _session: &V1SessionInfo) -> Result<(), String> {
        Ok(())
    }

    fn unshare(&self, _session_id: &str) -> Result<(), String> {
        Ok(())
    }
}

fn stub_message(session_id: &str) -> WithParts {
    WithParts {
        info: V1Message::User {
            id: session::ids::MessageId::ascending(None).unwrap(),
            session_id: session_id.to_string(),
            time: UserTime { created: 1.0 },
            format: None,
            summary: None,
            agent: "build".to_string(),
            model: opencode_schema::session_v1::V1UserModel {
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

// -----------------------------------------------------------------------
// session family
// -----------------------------------------------------------------------

#[tokio::test]
async fn v2_session_create_get_and_projection() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let directory = f.worktree.to_string_lossy().to_string();

    let session = create_session(&router, &directory).await;
    let session_id = session["data"]["id"].as_str().unwrap();
    assert!(session_id.starts_with("ses_"));
    // V2 projection (`Session.Info` from `fromRow`) — unset optional ids are
    // absent from the wire.
    assert!(session["data"]["time"]["created"].is_number());
    assert!(session["data"]["agent"].is_null());
    assert!(session["data"]["model"].is_null());
    assert!(session["data"]["projectID"].is_string());
    assert!(session["data"]["location"]["directory"]
        .as_str()
        .unwrap()
        .starts_with(&directory));

    // session.get resolves the location through the session row itself.
    let response = send(&router, "GET", &format!("/api/session/{session_id}"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let got: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(got["data"]["id"], session["data"]["id"]);

    // session.get on a missing session is the tagged 404.
    let response = send(&router, "GET", "/api/session/ses_missing", "").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_string(response).await,
        "{\"_tag\":\"SessionNotFoundError\",\"sessionID\":\"ses_missing\",\"message\":\"Session not found: ses_missing\"}"
    );
}

#[tokio::test]
async fn v2_session_list_cursors() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let directory = f.worktree.to_string_lossy().to_string();

    let mut ids = Vec::new();
    for _ in 0..3 {
        let session = create_session(&router, &directory).await;
        ids.push(session["data"]["id"].as_str().unwrap().to_string());
    }

    // Full page: all three, cursors on both ends.
    let response = send(&router, "GET", "/api/session", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let page: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(page["data"].as_array().unwrap().len(), 3);
    assert!(page["cursor"]["previous"].is_string());
    assert!(page["cursor"]["next"].is_string());

    // First page of two.
    let response = send(&router, "GET", "/api/session?limit=2", "").await;
    let first: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(first["data"].as_array().unwrap().len(), 2);
    let next = first["cursor"]["next"].as_str().unwrap();

    // Follow the cursor — the remaining session comes back.
    let response = send(
        &router,
        "GET",
        &format!("/api/session?cursor={}", urlencode(next)),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let second: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(second["data"].as_array().unwrap().len(), 1);
    // All timestamps are equal under the fixed clock — paging is by id —
    // but the union of both pages must cover the three sessions.
    let first_ids: Vec<String> = first["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_string())
        .collect();
    let second_id = second["data"][0]["id"].as_str().unwrap().to_string();
    assert!(ids.contains(&second_id));
    assert!(!first_ids.contains(&second_id));

    // An invalid cursor is the tagged 400.
    let response = send(&router, "GET", "/api/session?cursor=garbage", "").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_string(response).await,
        "{\"_tag\":\"InvalidCursorError\",\"message\":\"Invalid cursor\"}"
    );

    // An invalid order value is the v2 query error envelope.
    let response = send(&router, "GET", "/api/session?order=sideways", "").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(error["_tag"], "InvalidRequestError");
    assert_eq!(error["kind"], "Query");
}

// -----------------------------------------------------------------------
// prompt / compact / wait / revert stops
// -----------------------------------------------------------------------

#[tokio::test]
async fn v2_prompt_and_revert_semantics() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let directory = f.worktree.to_string_lossy().to_string();
    let session = create_session(&router, &directory).await;
    let session_id = session["data"]["id"].as_str().unwrap().to_string();

    // prompt on a missing session is the tagged 404.
    let response = send(
        &router,
        "POST",
        "/api/session/ses_missing/prompt",
        &serde_json::json!({ "prompt": { "text": "hi" } }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // prompt with an invalid delivery is a payload error.
    let response = send(
        &router,
        "POST",
        &format!("/api/session/{session_id}/prompt"),
        &serde_json::json!({ "prompt": { "text": "hi" }, "delivery": "bogus" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(error["_tag"], "InvalidRequestError");
    assert_eq!(error["kind"], "Payload");

    // prompt with valid input stops at the M6.7 S1 defect-500.
    let response = send(
        &router,
        "POST",
        &format!("/api/session/{session_id}/prompt"),
        &serde_json::json!({ "prompt": { "text": "hi" } }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // compact / wait are 503 after the 404 check.
    let response = send(
        &router,
        "POST",
        &format!("/api/session/{session_id}/compact"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body_string(response).await,
        "{\"_tag\":\"ServiceUnavailableError\",\"message\":\"Session compact is not available yet\",\"service\":\"session.compact\"}"
    );
    let response = send(
        &router,
        "POST",
        &format!("/api/session/{session_id}/wait"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    // revert.stage always resolves the boundary in the (empty under V1)
    // session_message projection — the tagged 404.
    let message_id = session::ids::MessageId::ascending(None).unwrap();
    let response = send(
        &router,
        "POST",
        &format!("/api/session/{session_id}/revert/stage"),
        &serde_json::json!({ "messageID": message_id }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_string(response).await,
        format!(
            "{{\"_tag\":\"MessageNotFoundError\",\"sessionID\":\"{session_id}\",\"messageID\":\"{message_id}\",\"message\":\"Message not found: {message_id}\"}}"
        )
    );

    // revert.clear / revert.commit are 204 when no revert is staged.
    for operation in ["clear", "commit"] {
        let response = send(
            &router,
            "POST",
            &format!("/api/session/{session_id}/revert/{operation}"),
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(body_string(response).await, "");
    }
}

// -----------------------------------------------------------------------
// context / history / interrupt / messages
// -----------------------------------------------------------------------

#[tokio::test]
async fn v2_context_history_interrupt_and_messages() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let directory = f.worktree.to_string_lossy().to_string();
    let session = create_session(&router, &directory).await;
    let session_id = session["data"]["id"].as_str().unwrap().to_string();

    // context is `{data: SessionMessage[]}` (empty under the V1 engine).
    let response = send(
        &router,
        "GET",
        &format!("/api/session/{session_id}/context"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let context: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(context["data"], serde_json::json!([]));

    // history — empty page, `hasMore: false`.
    let response = send(
        &router,
        "GET",
        &format!("/api/session/{session_id}/history"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let history: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(history["data"], serde_json::json!([]));
    assert_eq!(history["hasMore"], false);

    // history validates `after`.
    let response = send(
        &router,
        "GET",
        &format!("/api/session/{session_id}/history?after=nan"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // interrupt is a 204 no-op on the V1 run-state.
    let response = send(
        &router,
        "POST",
        &format!("/api/session/{session_id}/interrupt"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // messages — empty under the V1 engine, order/cursor combination is
    // rejected.
    let response = send(
        &router,
        "GET",
        &format!("/api/session/{session_id}/message"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let messages: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(messages["data"], serde_json::json!([]));

    let response = send(
        &router,
        "GET",
        &format!("/api/session/{session_id}/message?cursor=Ym9ndXM&order=asc"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_string(response).await,
        "{\"_tag\":\"InvalidCursorError\",\"message\":\"Cursor cannot be combined with order\"}"
    );
}

// -----------------------------------------------------------------------
// permission family
// -----------------------------------------------------------------------

/// `POST /api/session/{id}/permission` with a default ask payload.
async fn permission_ask(
    router: &axum::Router,
    session_id: &str,
    payload: &serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = send(
        router,
        "POST",
        &format!("/api/session/{session_id}/permission"),
        &payload.to_string(),
    )
    .await;
    let status = response.status();
    let body = serde_json::from_str(&body_string(response).await).unwrap_or_default();
    (status, body)
}

#[tokio::test]
async fn v2_permission_ask_reply_flow() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let directory = f.worktree.to_string_lossy().to_string();
    let session = create_session(&router, &directory).await;
    let session_id = session["data"]["id"].as_str().unwrap().to_string();

    // The default build agent allows everything by default — such asks are
    // not registered.
    let (status, body) = permission_ask(
        &router,
        &session_id,
        &serde_json::json!({
            "action": "bash",
            "resources": ["git push"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["effect"], "allow");

    // `doom_loop` is `ask` by default — the pending request registers.
    let (status, body) = permission_ask(
        &router,
        &session_id,
        &serde_json::json!({
            "action": "doom_loop",
            "resources": ["*"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["effect"], "ask");
    let request_id = body["data"]["id"].as_str().unwrap().to_string();
    assert!(request_id.starts_with("per_"));

    // The request is listed for the session.
    let response = send(
        &router,
        "GET",
        &format!("/api/session/{session_id}/permission"),
        "",
    )
    .await;
    let listed: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"][0]["id"], request_id.as_str());

    // request.list uses the location envelope.
    let response = send(
        &router,
        "GET",
        &format!(
            "/api/permission/request?location%5Bdirectory%5D={}",
            urlencode(&directory)
        ),
        "",
    )
    .await;
    let requests: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(requests["location"]["directory"].is_string());
    assert_eq!(requests["data"].as_array().unwrap().len(), 1);

    // Reply once — 204, the registry is drained.
    let response = send(
        &router,
        "POST",
        &format!("/api/session/{session_id}/permission/{request_id}/reply"),
        &serde_json::json!({ "reply": "once" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = send(
        &router,
        "GET",
        &format!("/api/session/{session_id}/permission"),
        "",
    )
    .await;
    let listed: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(listed["data"], serde_json::json!([]));

    // Replying twice is the tagged 404.
    let response = send(
        &router,
        "POST",
        &format!("/api/session/{session_id}/permission/{request_id}/reply"),
        &serde_json::json!({ "reply": "once" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(body_string(response)
        .await
        .contains("PermissionNotFoundError"));
}

#[tokio::test]
async fn v2_permission_deny_all_and_reject_cascade() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let directory = f.worktree.to_string_lossy().to_string();
    let session = create_session(&router, &directory).await;
    let session_id = session["data"]["id"].as_str().unwrap().to_string();

    // An unknown agent is the deny-all ruleset — never registered.
    let (status, body) = permission_ask(
        &router,
        &session_id,
        &serde_json::json!({
            "action": "bash",
            "resources": ["git push"],
            "agent": "nonexistent",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["effect"], "deny");
    let response = send(
        &router,
        "GET",
        &format!("/api/session/{session_id}/permission"),
        "",
    )
    .await;
    let listed: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(listed["data"], serde_json::json!([]));

    // Two asks; rejecting one drains both.
    let mut ids = Vec::new();
    for _ in 0..2 {
        let (_, body) = permission_ask(
            &router,
            &session_id,
            &serde_json::json!({ "action": "doom_loop", "resources": ["*"] }),
        )
        .await;
        ids.push(body["data"]["id"].as_str().unwrap().to_string());
    }
    // Rejecting one drains every pending request of the session.
    let response = send(
        &router,
        "POST",
        &format!("/api/session/{session_id}/permission/{}/reply", ids[0]),
        &serde_json::json!({ "reply": "reject" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = send(
        &router,
        "GET",
        &format!("/api/session/{session_id}/permission"),
        "",
    )
    .await;
    let listed: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(listed["data"], serde_json::json!([]));
}

// -----------------------------------------------------------------------
// misc family
// -----------------------------------------------------------------------

#[tokio::test]
async fn v2_misc_envelopes() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let directory = f.worktree.to_string_lossy().to_string();

    // health — plain JSON body.
    let response = send(&router, "GET", "/api/health", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "{\"healthy\":true}");

    // location — the `Location.Info` shape.
    let response = send(
        &router,
        "GET",
        &format!(
            "/api/location?location%5Bdirectory%5D={}",
            urlencode(&directory)
        ),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let location: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(location["directory"], serde_json::json!(directory));
    assert!(location["project"]["id"].is_string());

    // fs.list — sorted, directories first; paths relative to the location.
    std::fs::write(f.worktree.join("zfile.txt"), b"x").unwrap();
    std::fs::create_dir_all(f.worktree.join("adir")).unwrap();
    let response = send(
        &router,
        "GET",
        &format!(
            "/api/fs/list?location%5Bdirectory%5D={}",
            urlencode(&directory)
        ),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let entries: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(entries["data"][0]["path"], "adir/");
    assert_eq!(entries["data"][0]["type"], "directory");
    assert_eq!(entries["data"][1]["path"], "zfile.txt");
    assert_eq!(entries["data"][1]["type"], "file");

    // fs.read — raw bytes with a mime type.
    let response = send(
        &router,
        "GET",
        &format!(
            "/api/fs/read/zfile.txt?location%5Bdirectory%5D={}",
            urlencode(&directory)
        ),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "x");

    // fs.find — matches by subsequence, entries are relative to the
    // location.
    let response = send(
        &router,
        "GET",
        &format!(
            "/api/fs/find?location%5Bdirectory%5D={}&query=zfi",
            urlencode(&directory)
        ),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let found: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(found["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["path"] == "zfile.txt"));

    // provider.get on a missing provider is the tagged 404.
    let response = send(&router, "GET", "/api/provider/nope", "").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(body_string(response)
        .await
        .contains("ProviderNotFoundError"));
}
