//! M6.6 acceptance: the v1 global, control, instance, file, experimental and
//! tui route families against the full middleware stack.

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
// global (`handlers/global.ts`)
// -----------------------------------------------------------------------

#[tokio::test]
async fn global_health_and_config() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

    let response = send(&router, "GET", "/global/health", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(value["healthy"], true);
    assert!(value["version"].is_string());

    let response = send(&router, "GET", "/global/config", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(value.is_object());

    let response = send(&router, "POST", "/global/dispose", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");
}

#[tokio::test]
async fn global_upgrade_rejects_non_semver() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

    let response = send(&router, "POST", "/global/upgrade", r#"{"target":"nope"}"#).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Valid semver reaches the Installation seam, which is the unknown
    // installation method → `{"success":false,...}` 400.
    let response = send(&router, "POST", "/global/upgrade", r#"{"target":"v1.2.3"}"#).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(value["success"], false);
}

// -----------------------------------------------------------------------
// control (`handlers/control.ts`)
// -----------------------------------------------------------------------

#[tokio::test]
async fn control_auth_roundtrip() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

    // Invalid Auth.Info payloads are rejected.
    let response = send(&router, "PUT", "/auth/anthropic", r#"{"type":"bogus"}"#).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = send(
        &router,
        "PUT",
        "/auth/anthropic",
        r#"{"type":"api","key":"sk-test"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");

    let response = send(&router, "DELETE", "/auth/anthropic", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");
}

#[tokio::test]
async fn control_log_validates_level() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

    let response = send(
        &router,
        "POST",
        "/log",
        r#"{"service":"test","level":"info","message":"hello"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");

    let response = send(
        &router,
        "POST",
        "/log",
        r#"{"service":"test","level":"loud","message":"hello"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// -----------------------------------------------------------------------
// instance (`handlers/instance.ts`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn instance_dispose_and_path() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "instance/dispose"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");

    let response = send(&router, "GET", &instance_uri(&f.worktree, "path"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(value["worktree"], f.worktree.to_string_lossy().as_ref());
    assert_eq!(value["directory"], f.worktree.to_string_lossy().as_ref());
    assert!(value["home"].is_string());
    assert!(value["state"].is_string());
    assert!(value["config"].is_string());
}

#[tokio::test]
async fn instance_vcs_status_and_command() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

    let response = send(&router, "GET", &instance_uri(&f.worktree, "vcs/status"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "[]");

    let response = send(&router, "GET", &instance_uri(&f.worktree, "command"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let commands: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let names: Vec<&str> = commands
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"init"));
    assert!(names.contains(&"review"));
}

#[tokio::test]
async fn instance_agent_skill_lsp_formatter() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

    let response = send(&router, "GET", &instance_uri(&f.worktree, "agent"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let agents: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(agents
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["name"] == "build"));

    for family in ["skill", "lsp", "formatter"] {
        let response = send(&router, "GET", &instance_uri(&f.worktree, family), "").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_string(response).await, "[]");
    }
}

// -----------------------------------------------------------------------
// file (`handlers/file.ts`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn file_find_text_and_files() {
    let f = fixture();
    std::fs::write(f.worktree.join("hello.txt"), "needle in a haystack\n").unwrap();
    let router = routes::build_router(f.ctx.clone());

    let response = send(
        &router,
        "GET",
        &format!("{}&pattern=needle", instance_uri(&f.worktree, "find")),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let matches: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let entries = matches.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["path"]["text"], "hello.txt");
    assert_eq!(entries[0]["lines"]["text"], "needle in a haystack\n");
    assert_eq!(entries[0]["line_number"], 1);
    let submatches = entries[0]["submatches"].as_array().unwrap();
    assert_eq!(submatches.len(), 1);
    assert_eq!(submatches[0]["match"]["text"], "needle");

    // Missing pattern → query schema error.
    let response = send(&router, "GET", &instance_uri(&f.worktree, "find"), "").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn file_find_file_and_listing() {
    let f = fixture();
    std::fs::write(f.worktree.join("readme.md"), "hi").unwrap();
    std::fs::create_dir_all(f.worktree.join("src")).unwrap();
    std::fs::write(f.worktree.join("src").join("main.rs"), "fn main() {}").unwrap();
    let router = routes::build_router(f.ctx.clone());

    let response = send(
        &router,
        "GET",
        &format!("{}&query=read", instance_uri(&f.worktree, "find/file")),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let paths: Vec<String> = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(paths, vec!["readme.md".to_string()]);

    let response = send(
        &router,
        "GET",
        &format!(
            "{}&type=directory&query=sr",
            instance_uri(&f.worktree, "find/file")
        ),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let dirs: Vec<String> = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(dirs, vec!["src/".to_string()]);

    let response = send(
        &router,
        "GET",
        &instance_uri(&f.worktree, "find/symbol"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "[]");
}

#[tokio::test]
async fn file_list_and_content() {
    let f = fixture();
    std::fs::write(f.worktree.join("a.txt"), "plain text\n").unwrap();
    std::fs::write(f.worktree.join("b.bin"), "a\0b\0c").unwrap();
    let router = routes::build_router(f.ctx.clone());

    let response = send(
        &router,
        "GET",
        &format!("{}&path=", instance_uri(&f.worktree, "file")),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let entries: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(entries.is_array());
    assert!(entries
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["name"] == "a.txt" && e["type"] == "file"));

    let response = send(
        &router,
        "GET",
        &format!("{}&path=a.txt", instance_uri(&f.worktree, "file/content")),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let content: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(content["type"], "text");
    assert_eq!(content["content"], "plain text");

    let response = send(
        &router,
        "GET",
        &format!("{}&path=b.bin", instance_uri(&f.worktree, "file/content")),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let content: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(content["type"], "binary");
    assert_eq!(content["encoding"], "base64");

    let response = send(
        &router,
        "GET",
        &instance_uri(&f.worktree, "file/status"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "[]");
}

// -----------------------------------------------------------------------
// experimental (`handlers/experimental.ts`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn experimental_read_only_families() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

    let response = send(
        &router,
        "GET",
        &instance_uri(&f.worktree, "experimental/capabilities"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_string(response).await,
        r#"{"backgroundSubagents":false}"#
    );

    // Missing provider/model → query schema error.
    let response = send(
        &router,
        "GET",
        &instance_uri(&f.worktree, "experimental/tool"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // The tool registry is an M7 seam: it fails as a defect, not a 404.
    let response = send(
        &router,
        "GET",
        &format!(
            "{}&provider=anthropic&model=claude",
            instance_uri(&f.worktree, "experimental/tool")
        ),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let response = send(
        &router,
        "GET",
        &instance_uri(&f.worktree, "experimental/resource"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "{}");

    let response = send(
        &router,
        "GET",
        &instance_uri(&f.worktree, "experimental/session"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let (parts, body) = response.into_parts();
    let body = String::from_utf8(
        axum::body::to_bytes(body, usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert_eq!(body, "[]");
    assert!(parts.headers.get("x-next-cursor").is_none());

    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "experimental/session/ses_000/background"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "false");
}

// -----------------------------------------------------------------------
// tui (`handlers/tui.ts`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tui_command_routes() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "tui/append-prompt"),
        r#"{"text":"hello"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");

    for path in ["open-help", "open-sessions", "open-themes", "open-models"] {
        let response = send(
            &router,
            "POST",
            &instance_uri(&f.worktree, &format!("tui/{path}")),
            "",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_string(response).await, "true");
    }

    // Known aliases map to a command; unknown commands publish `{}`.
    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "tui/execute-command"),
        r#"{"command":"session_new"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");

    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "tui/show-toast"),
        r#"{"message":"done","variant":"success"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");

    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "tui/show-toast"),
        r#"{"message":"bad","variant":"loud"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn tui_publish_and_select_session() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "tui/publish"),
        r#"{"type":"tui.prompt.append","properties":{"text":"hi"}}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");

    // A bad sessionID prefix is a Payload schema rejection in the union
    // handler but an empty 400 from selectSession.
    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "tui/publish"),
        r#"{"type":"tui.session.select","properties":{"sessionID":"bogus"}}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "tui/select-session"),
        r#"{"sessionID":"bogus"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_string(response).await, "");

    // A well-formed unknown session maps to the storage NotFoundError.
    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "tui/select-session"),
        r#"{"sessionID":"ses_000"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn tui_control_queue_roundtrip() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

    f.ctx.tui.push_request(serde_json::json!({"kind": "pick"}));
    let response = send(
        &router,
        "GET",
        &instance_uri(&f.worktree, "tui/control/next"),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(value["kind"], "pick");

    let response = send(
        &router,
        "POST",
        &instance_uri(&f.worktree, "tui/control/response"),
        r#"{"index":0}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");
}
