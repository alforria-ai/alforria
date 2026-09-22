//! M6.5 acceptance: the v1 session family (27 endpoints) against the real
//! M5 services plus a stub [`SessionEngine`], through the full middleware
//! stack.

use std::sync::Arc;

use alforria_core::{
    session, CoreError, EventBus, RunnerError, SessionError, SessionServices, Storage, WithParts,
};
use alforria_schema::file_diff::{FileDiffStatus, SnapshotFileDiff};
use alforria_schema::session_v1::{UserTime, V1Message, V1Part, V1SessionInfo, V1UserModel};
use alforria_server::routes;
use alforria_server::state::{
    AuthConfig, EmptyUiBackend, InstanceStore, ServerContext, SessionEngine,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::future::BoxFuture;
use tower::ServiceExt;

// -----------------------------------------------------------------------
// fixture
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

/// Test directory owning the shared storage.
struct Fixture {
    _dir: tempfile::TempDir,
    ctx: Arc<ServerContext>,
    services: Arc<SessionServices>,
    worktree: std::path::PathBuf,
    engine: Arc<StubEngine>,
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

fn fixture_with(engine: Arc<StubEngine>) -> Fixture {
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
    let engine_for_factory = engine.clone();
    ctx.engine_factory =
        Arc::new(move |_location| Ok(engine_for_factory.clone() as Arc<dyn SessionEngine>));
    Fixture {
        _dir: dir,
        ctx: Arc::new(ctx),
        services,
        worktree,
        engine,
    }
}

fn fixture() -> Fixture {
    fixture_with(Arc::new(StubEngine::default()))
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

/// Create a session through the real HTTP surface.
async fn create_session(router: &axum::Router) -> serde_json::Value {
    let response = send(router, "POST", "/session", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_str(&body_string(response).await).unwrap()
}

/// Create a session scoped to `?directory=` through the real HTTP surface.
async fn create_session_in(router: &axum::Router, directory: &str) -> serde_json::Value {
    let response = send(
        router,
        "POST",
        &format!("/session?directory={}", urlencode(directory)),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_str(&body_string(response).await).unwrap()
}

/// Write a user message directly through the store.
fn seed_message(services: &SessionServices, session_id: &str, text: &str) -> String {
    let id = session::ids::MessageId::ascending(None).unwrap();
    services
        .sessions
        .update_message(&V1Message::User {
            id: id.clone(),
            session_id: session_id.to_string(),
            time: UserTime { created: 0.0 },
            format: None,
            summary: None,
            agent: "build".to_string(),
            model: V1UserModel {
                provider_id: "test".to_string(),
                model_id: "model".to_string(),
                variant: None,
            },
            system: None,
            tools: None,
        })
        .unwrap();
    services
        .sessions
        .update_part(&V1Part::Text {
            id: session::ids::PartId::ascending(None).unwrap(),
            session_id: session_id.to_string(),
            message_id: id.clone(),
            text: text.to_string(),
            synthetic: None,
            ignored: None,
            time: None,
            metadata: None,
        })
        .unwrap();
    id
}

// -----------------------------------------------------------------------
// stub engine
// -----------------------------------------------------------------------

#[derive(Default)]
struct StubState {
    prompts: Vec<String>,
    loops: Vec<String>,
    commands: Vec<String>,
    shells: Vec<String>,
    cleanups: Vec<String>,
    diffs: usize,
    shares: Vec<String>,
    unshares: Vec<String>,
    reverts: Vec<String>,
    unreverts: Vec<String>,
    fail_share: bool,
    busy: bool,
}

#[derive(Default)]
struct StubEngine {
    state: std::sync::Mutex<StubState>,
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
            model: V1UserModel {
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

impl StubEngine {
    fn fail_share(self: &Arc<Self>) {
        self.state.lock().unwrap().fail_share = true;
    }

    fn set_busy(self: &Arc<Self>) {
        self.state.lock().unwrap().busy = true;
    }
}

impl SessionEngine for StubEngine {
    fn prompt(
        &self,
        input: session::prompt_input::PromptInput,
    ) -> BoxFuture<'static, Result<WithParts, session::prompt_input::PromptError>> {
        self.state
            .lock()
            .unwrap()
            .prompts
            .push(input.session_id.clone());
        Box::pin(async move { Ok(stub_message(&input.session_id)) })
    }

    fn loop_(
        &self,
        session_id: String,
    ) -> BoxFuture<'static, Result<WithParts, RunnerError<SessionError>>> {
        self.state.lock().unwrap().loops.push(session_id.clone());
        Box::pin(async move { Ok(stub_message(&session_id)) })
    }

    fn command(
        &self,
        input: session::prompt::CommandInput,
    ) -> BoxFuture<'static, Result<WithParts, session::prompt_input::PromptError>> {
        self.state
            .lock()
            .unwrap()
            .commands
            .push(input.command.clone());
        Box::pin(async move { Ok(stub_message(&input.session_id)) })
    }

    fn shell(
        &self,
        input: session::prompt::ShellInput,
    ) -> BoxFuture<'static, Result<WithParts, SessionError>> {
        self.state
            .lock()
            .unwrap()
            .shells
            .push(input.session_id.clone());
        let busy = self.state.lock().unwrap().busy;
        Box::pin(async move {
            if busy {
                return Err(SessionError::Busy(alforria_core::BusyError {
                    session_id: input.session_id,
                }));
            }
            Ok(stub_message(&input.session_id))
        })
    }

    fn revert(
        &self,
        input: session::revert::RevertInput,
    ) -> BoxFuture<'static, Result<V1SessionInfo, SessionError>> {
        self.state
            .lock()
            .unwrap()
            .reverts
            .push(input.session_id.clone());
        let busy = self.state.lock().unwrap().busy;
        let session_info = self.session_info(&input.session_id);
        Box::pin(async move {
            if busy {
                return Err(SessionError::Busy(alforria_core::BusyError {
                    session_id: input.session_id,
                }));
            }
            Ok(session_info)
        })
    }

    fn unrevert(
        &self,
        session_id: String,
    ) -> BoxFuture<'static, Result<V1SessionInfo, SessionError>> {
        self.state
            .lock()
            .unwrap()
            .unreverts
            .push(session_id.clone());
        let busy = self.state.lock().unwrap().busy;
        let session_info = self.session_info(&session_id);
        Box::pin(async move {
            if busy {
                return Err(SessionError::Busy(alforria_core::BusyError {
                    session_id: session_id.clone(),
                }));
            }
            Ok(session_info)
        })
    }

    fn cleanup(&self, session: &V1SessionInfo) -> Result<(), SessionError> {
        self.state.lock().unwrap().cleanups.push(session.id.clone());
        Ok(())
    }

    fn diff(
        &self,
        session_id: &str,
        _message_id: Option<&str>,
    ) -> Result<Vec<SnapshotFileDiff>, SessionError> {
        self.state.lock().unwrap().diffs += 1;
        Ok(vec![SnapshotFileDiff {
            file: Some(format!("{session_id}/file.txt")),
            patch: None,
            additions: 1.0,
            deletions: 2.0,
            status: Some(FileDiffStatus::Added),
        }])
    }

    fn share(&self, session: &V1SessionInfo) -> Result<(), String> {
        let fail = self.state.lock().unwrap().fail_share;
        if fail {
            return Err("sharing is disabled".to_string());
        }
        self.state.lock().unwrap().shares.push(session.id.clone());
        Ok(())
    }

    fn unshare(&self, session_id: &str) -> Result<(), String> {
        let fail = self.state.lock().unwrap().fail_share;
        if fail {
            return Err("sharing is disabled".to_string());
        }
        self.state
            .lock()
            .unwrap()
            .unshares
            .push(session_id.to_string());
        Ok(())
    }
}

impl StubEngine {
    fn session_info(&self, session_id: &str) -> V1SessionInfo {
        use alforria_schema::session_v1::V1SessionTime;
        V1SessionInfo {
            id: session_id.to_string(),
            slug: "slug".to_string(),
            project_id: "prj_test".to_string(),
            directory: "/repo".to_string(),
            time: V1SessionTime {
                created: 0,
                updated: 0,
                compacting: None,
                archived: None,
            },
            version: "0.0.1".to_string(),
            title: "t".to_string(),
            cost: Some(0.0),
            tokens: None,
            parent_id: None,
            path: None,
            workspace_id: None,
            agent: None,
            model: None,
            metadata: None,
            permission: None,
            revert: None,
            share: None,
            summary: None,
        }
    }
}

// -----------------------------------------------------------------------
// tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn session_crud_lifecycle() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();
    assert!(session_id.starts_with("ses_"));
    assert!(session["slug"].is_string());
    assert_eq!(session["cost"], 0.0);

    let response = send(&router, "GET", &format!("/session/{session_id}"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let got: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(got["id"], session["id"]);

    let response = send(
        &router,
        "GET",
        &format!("/session/{session_id}/children"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "[]");

    let response = send(&router, "GET", &format!("/session/{session_id}/todo"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "[]");

    let response = send(&router, "DELETE", &format!("/session/{session_id}"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");

    let response = send(&router, "GET", &format!("/session/{session_id}"), "").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_string(response).await,
        format!("{{\"name\":\"NotFoundError\",\"data\":{{\"message\":\"Session not found: {session_id}\"}}}}")
    );
}

#[tokio::test]
async fn create_body_semantics() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

    // Invalid JSON → empty 400 (HttpApiError.BadRequest).
    let response = send(&router, "POST", "/session", "{not json").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_string(response).await, "");

    // Body `null` decodes to the default create input.
    let response = send(&router, "POST", "/session", "null").await;
    assert_eq!(response.status(), StatusCode::OK);

    // Schema failure (bad parentID prefix) → empty 400.
    let response = send(&router, "POST", "/session", "{\"parentID\":\"nope\"}").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_string(response).await, "");

    // Full body round-trips the declared fields.
    let response = send(
        &router,
        "POST",
        "/session",
        "{\"title\":\"Custom\",\"agent\":\"build\",\"parentID\":\"ses_01JDY\"}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let created: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(created["title"], "Custom");
    assert_eq!(created["agent"], "build");
    assert_eq!(created["parentID"], "ses_01JDY");
}

#[tokio::test]
async fn list_scopes_to_the_directory_param() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let worktree = f.worktree.to_string_lossy().into_owned();
    let session = create_session_in(&router, &worktree).await;
    let session_id = session["id"].as_str().unwrap().to_string();

    // With the `?directory=` param the session shows up.
    let response = send(
        &router,
        "GET",
        &format!("/session?directory={}", urlencode(&worktree)),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let listed: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
    assert_eq!(listed[0]["id"], serde_json::Value::from(session_id.clone()));

    // search filters by title.
    let response = send(
        &router,
        "GET",
        &format!("/session?directory={}&search=nothing", urlencode(&worktree)),
        "",
    )
    .await;
    let listed: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(listed.as_array().map(Vec::len), Some(0));

    // limit caps the list.
    let response = send(
        &router,
        "GET",
        &format!("/session?directory={}&limit=1", urlencode(&worktree)),
        "",
    )
    .await;
    let listed: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(listed.as_array().map(Vec::len), Some(1));

    // Without the param the request routes to the process cwd's instance —
    // a different project, so the worktree session is not listed.
    let response = send(&router, "GET", "/session", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let listed: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(listed.as_array().map(Vec::len), Some(0));

    // Invalid roots literal → schema-error 400.
    let response = send(&router, "GET", "/session?roots=yes", "").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_string(response).await,
        "{\"name\":\"BadRequest\",\"data\":{\"message\":\"Expected \\\"true\\\" or \\\"false\\\", got \\\"yes\\\"\",\"kind\":\"Query\"}}"
    );
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

#[tokio::test]
async fn messages_paging_link_headers() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();
    let base = format!("/session/{session_id}/message");

    for i in 0..5 {
        seed_message(&f.services, &session_id, &format!("m{i}"));
    }

    // Full list when limit is absent.
    let response = send(&router, "GET", &base, "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let response_headers = response.headers().clone();
    let body_text = body_string(response).await;
    let full: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    assert_eq!(full.as_array().map(Vec::len), Some(5));
    assert!(response_headers.get("link").is_none());

    // `before` without `limit` → empty 400.
    let response = send(&router, "GET", &format!("{base}?before=abc"), "").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_string(response).await, "");

    // Invalid cursor → empty 400.
    let response = send(&router, "GET", &format!("{base}?limit=2&before=%2%2"), "").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_string(response).await, "");

    // Page 1: Link + X-Next-Cursor + Access-Control-Expose-Headers.
    let response = send(&router, "GET", &format!("{base}?limit=2"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("access-control-expose-headers")
            .unwrap(),
        "Link, X-Next-Cursor"
    );
    let cursor = response
        .headers()
        .get("x-next-cursor")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let link = response.headers().get("link").unwrap().to_str().unwrap();
    assert_eq!(
        link,
        format!(
            "<http://localhost{base}?limit=2&before={}>; rel=\"next\"",
            urlencode(&cursor)
        )
    );
    let page: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(page.as_array().map(Vec::len), Some(2));

    // Page 2 via the cursor.
    let response = send(
        &router,
        "GET",
        &format!("{base}?limit=4&before={}", urlencode(&cursor)),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("x-next-cursor").is_none());
    let last: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(last.as_array().map(Vec::len), Some(3));

    // limit=0 → the full list.
    let response = send(&router, "GET", &format!("{base}?limit=0"), "").await;
    let all: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(all.as_array().map(Vec::len), Some(5));
}

#[tokio::test]
async fn message_get_and_delete() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();
    let message_id = seed_message(&f.services, &session_id, "hello");

    let response = send(
        &router,
        "GET",
        &format!("/session/{session_id}/message/{message_id}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let got: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(
        got["info"]["id"],
        serde_json::Value::from(message_id.clone())
    );

    // A well-prefixed but unknown message → 404.
    let response = send(
        &router,
        "GET",
        &format!("/session/{session_id}/message/msg_missing"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // A bad MessageID prefix fails param decoding (kind "Params").
    let response = send(
        &router,
        "GET",
        &format!("/session/{session_id}/message/nope"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_string(response).await;
    assert!(body.contains("\"kind\":\"Params\""), "{body}");

    let response = send(
        &router,
        "DELETE",
        &format!("/session/{session_id}/message/{message_id}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");
}

#[tokio::test]
async fn update_merges_permission_and_archives() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();

    let response = send(
        &router,
        "PATCH",
        &format!("/session/{session_id}"),
        "{\"title\":\"New Title\",\"permission\":[{\"permission\":\"bash\",\"pattern\":\"*\",\"action\":\"allow\"}]}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let updated: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(updated["title"], "New Title");
    assert_eq!(
        updated["permission"][0]["permission"],
        serde_json::Value::from("bash")
    );

    // A second update merges (concatenates) the permission ruleset.
    let response = send(
        &router,
        "PATCH",
        &format!("/session/{session_id}"),
        "{\"permission\":[{\"permission\":\"read\",\"pattern\":\"*\",\"action\":\"deny\"}]}",
    )
    .await;
    let updated: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(updated["permission"].as_array().map(Vec::len), Some(2));

    // Archived timestamp.
    let response = send(
        &router,
        "PATCH",
        &format!("/session/{session_id}"),
        "{\"time\":{\"archived\":12345}}",
    )
    .await;
    let updated: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    // JS-number wire parity: whole f64s serialize as integers
    // (`js_number.rs`), like the TS reference's JSON.stringify.
    assert_eq!(updated["time"]["archived"], serde_json::Value::from(12345));

    // Invalid payload → schema-error 400.
    let response = send(
        &router,
        "PATCH",
        &format!("/session/{session_id}"),
        "{\"title\":123}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_string(response).await;
    assert!(body.contains("\"name\":\"BadRequest\""));
    assert!(body.contains("\"kind\":\"Payload\""));
}

#[tokio::test]
async fn status_maps_sessions() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();

    f.services
        .status
        .set(
            &session_id,
            alforria_schema::session_status::SessionStatusInfo::Busy,
        )
        .unwrap();

    let response = send(&router, "GET", "/session/status", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let status: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(status[session_id]["type"], serde_json::Value::from("busy"));
}

#[tokio::test]
async fn fork_copies_messages_and_accepts_empty_body() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();
    seed_message(&f.services, &session_id, "one");

    let response = send(&router, "POST", &format!("/session/{session_id}/fork"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let forked: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_ne!(forked["id"], serde_json::Value::from(session_id.clone()));
    // TS fork never sets parentID (session.ts:695-701).
    assert_eq!(forked["parentID"], serde_json::Value::Null);

    // Invalid JSON → empty 400; bad messageID → empty 400.
    let response = send(
        &router,
        "POST",
        &format!("/session/{session_id}/fork"),
        "{oops",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_string(response).await, "");
    let response = send(
        &router,
        "POST",
        &format!("/session/{session_id}/fork"),
        "{\"messageID\":\"nope\"}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_string(response).await, "");
}

#[tokio::test]
async fn abort_returns_true() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();

    let response = send(&router, "POST", &format!("/session/{session_id}/abort"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");
}

#[tokio::test]
async fn todo_lists_seeded_rows() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();

    f.services
        .storage
        .put_todo(&alforria_core::storage::Todo {
            session_id: session_id.clone(),
            content: "write tests".to_string(),
            status: "in_progress".to_string(),
            priority: "high".to_string(),
            position: 0,
            time_created: 0,
            time_updated: 0,
        })
        .unwrap();

    let response = send(&router, "GET", &format!("/session/{session_id}/todo"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_string(response).await,
        "[{\"content\":\"write tests\",\"status\":\"in_progress\",\"priority\":\"high\"}]"
    );
}

#[tokio::test]
async fn engine_routes_prompt_command_shell_async() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();
    let response = send(
        &router,
        "POST",
        &format!("/session/{session_id}/message"),
        "{\"parts\":[{\"type\":\"text\",\"text\":\"hi\"}]}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let created: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(created["info"]["id"].is_string());
    assert_eq!(created["info"]["agent"], serde_json::Value::from("build"));
    assert!(f.engine.state.lock().unwrap().prompts.contains(&session_id));

    let response = send(
        &router,
        "POST",
        &format!("/session/{session_id}/command"),
        "{\"command\":\"custom\",\"arguments\":\"arg\"}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(f
        .engine
        .state
        .lock()
        .unwrap()
        .commands
        .contains(&"custom".to_string()));

    let response = send(
        &router,
        "POST",
        &format!("/session/{session_id}/shell"),
        "{\"agent\":\"build\",\"command\":\"ls\"}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(f.engine.state.lock().unwrap().shells.contains(&session_id));

    // prompt_async returns 204 with no body.
    let response = send(
        &router,
        "POST",
        &format!("/session/{session_id}/prompt_async"),
        "{\"parts\":[{\"type\":\"text\",\"text\":\"hi\"}]}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(body_string(response).await, "");

    // Payload decode failures are 400s.
    let response = send(
        &router,
        "POST",
        &format!("/session/{session_id}/message"),
        "{\"parts\":\"not-an-array\"}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn busy_maps_to_409() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();

    f.engine.set_busy();
    let response = send(
        &router,
        "POST",
        &format!("/session/{session_id}/shell"),
        "{\"agent\":\"build\",\"command\":\"ls\"}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_string(response).await,
        format!(
            "{{\"_tag\":\"SessionBusyError\",\"sessionID\":\"{session_id}\",\"message\":\"Session is busy: {session_id}\"}}"
        )
    );

    let response = send(
        &router,
        "POST",
        &format!("/session/{session_id}/revert"),
        "{\"messageID\":\"msg_01JDY\"}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn revert_unrevert_and_diff() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();

    let response = send(
        &router,
        "POST",
        &format!("/session/{session_id}/revert"),
        "{\"messageID\":\"msg_01JDY\"}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let info: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(info["id"], serde_json::Value::from(session_id.clone()));

    let response = send(
        &router,
        "POST",
        &format!("/session/{session_id}/unrevert"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let response = send(&router, "GET", &format!("/session/{session_id}/diff"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let diffs: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(
        diffs[0]["file"],
        serde_json::Value::from(format!("{session_id}/file.txt"))
    );
}

#[tokio::test]
async fn summarize_runs_cleanup_compaction_and_loop() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();
    seed_message(&f.services, &session_id, "hello");

    let response = send(
        &router,
        "POST",
        &format!("/session/{session_id}/summarize"),
        "{\"providerID\":\"test\",\"modelID\":\"model\"}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");

    {
        let state = f.engine.state.lock().unwrap();
        assert!(state.cleanups.contains(&session_id));
        assert!(state.loops.contains(&session_id));
    }
    // The compaction user message + part were written.
    let page = f.services.messages.page(&session_id, 50, None).unwrap();
    let compactions: Vec<&V1Part> = page
        .items
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter(|p| matches!(p, V1Part::Compaction { .. }))
        .collect();
    assert_eq!(compactions.len(), 1);
}

#[tokio::test]
async fn init_runs_the_init_command() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();

    let response = send(
        &router,
        "POST",
        &format!("/session/{session_id}/init"),
        "{\"providerID\":\"test\",\"modelID\":\"model\",\"messageID\":\"msg_01JDY\"}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");
    assert!(f
        .engine
        .state
        .lock()
        .unwrap()
        .commands
        .contains(&"init".to_string()));
}

#[tokio::test]
async fn share_and_unshare() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();

    let response = send(&router, "POST", &format!("/session/{session_id}/share"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(f.engine.state.lock().unwrap().shares.contains(&session_id));

    let response = send(
        &router,
        "DELETE",
        &format!("/session/{session_id}/share"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(f
        .engine
        .state
        .lock()
        .unwrap()
        .unshares
        .contains(&session_id));

    // A failing share service maps to the empty 500.
    f.engine.fail_share();
    let response = send(&router, "POST", &format!("/session/{session_id}/share"), "").await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body_string(response).await, "");
}

#[tokio::test]
async fn permission_respond_maps_404() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();

    let response = send(
        &router,
        "POST",
        &format!("/session/{session_id}/permissions/per_01JDY"),
        "{\"response\":\"once\"}",
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_string(response).await,
        "{\"_tag\":\"PermissionNotFoundError\",\"requestID\":\"per_01JDY\",\"message\":\"Permission request not found: per_01JDY\"}"
    );
}

#[tokio::test]
async fn update_part_validates_path_ids() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let session = create_session(&router).await;
    let session_id = session["id"].as_str().unwrap().to_string();
    let message_id = seed_message(&f.services, &session_id, "hello");
    let part_id = session::ids::PartId::ascending(None).unwrap();
    f.services
        .sessions
        .update_part(&V1Part::Text {
            id: part_id.clone(),
            session_id: session_id.clone(),
            message_id: message_id.clone(),
            text: "hello".to_string(),
            synthetic: None,
            ignored: None,
            time: None,
            metadata: None,
        })
        .unwrap();

    // Mismatched path ids → empty 400.
    let response = send(
        &router,
        "PATCH",
        &format!(
            "/session/{session_id}/message/{message_id}/part/{part_id}"
        ),
        &format!(
            "{{\"type\":\"text\",\"id\":\"{part_id}\",\"sessionID\":\"ses_other\",\"messageID\":\"{message_id}\",\"text\":\"x\"}}"
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_string(response).await, "");

    // Matching ids update and echo the part.
    let response = send(
        &router,
        "PATCH",
        &format!(
            "/session/{session_id}/message/{message_id}/part/{part_id}"
        ),
        &format!(
            "{{\"type\":\"text\",\"id\":\"{part_id}\",\"sessionID\":\"{session_id}\",\"messageID\":\"{message_id}\",\"text\":\"x\"}}"
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let part: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(part["id"], serde_json::Value::from(part_id.clone()));

    let response = send(
        &router,
        "DELETE",
        &format!("/session/{session_id}/message/{message_id}/part/{part_id}"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");
}

#[tokio::test]
async fn missing_session_404s_and_unwired_engine_500s() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());
    let response = send(&router, "GET", "/session/ses_missing/message", "").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // The unwired engine factory surfaces as the defect-500 envelope.
    let dir = tempfile::tempdir().unwrap();
    let (services, _worktree) = make_services(dir.path());
    let storage = services.storage.clone();
    let services_for_factory = services.clone();
    let instances = InstanceStore::new(Arc::new(move |_| Ok(services_for_factory.clone())));
    let ctx = Arc::new(ServerContext::new(
        AuthConfig::new("alforria", None),
        instances,
        storage.clone(),
        Arc::new(EventBus::new_shared(storage, None)),
        Vec::new(),
        Arc::new(EmptyUiBackend),
    ));
    let router = routes::build_router(ctx);
    let response = send(
        &router,
        "POST",
        "/session/ses_missing/message",
        "{\"parts\":[]}",
    )
    .await;
    // requireSession runs before the engine lookup → 404 for a missing
    // session even with no engine wired.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
