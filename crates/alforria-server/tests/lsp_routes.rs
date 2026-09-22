//! M7.9 acceptance: `GET /lsp` (`getLsp`, handlers/instance.ts:79-81) and
//! `GET /find/symbol` (handlers/file.ts:62-64 — the pinned TS handler
//! returns `[]`) through the full middleware stack.

use std::sync::Arc;

use alforria_core::{EventBus, SessionServices, Storage};
use alforria_server::routes;
use alforria_server::state::{AuthConfig, EmptyUiBackend, InstanceStore, ServerContext};
use alforria_server::ServerError;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

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

/// `LSP.Service.status` — one connected client (lsp.ts:313-326).
struct OneClientLsp;

impl alforria_server::state::LspSource for OneClientLsp {
    fn status(
        &self,
        _location: &alforria_server::middleware::location::LocationContext,
    ) -> Result<Vec<serde_json::Value>, ServerError> {
        Ok(vec![serde_json::json!({
            "id": "rust",
            "name": "rust",
            "root": "",
            "status": "connected",
        })])
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
    let agent_input = alforria_core::AgentRegistryInput {
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
    let mut ctx = ServerContext::new(
        AuthConfig::new("alforria", None),
        instances,
        storage.clone(),
        Arc::new(EventBus::new_shared(storage, None)),
        Vec::new(),
        Arc::new(EmptyUiBackend),
    );
    ctx.lsp = Arc::new(OneClientLsp);
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

#[tokio::test]
async fn lsp_status_lists_connected_clients() {
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

    let response = send(&router, "GET", &instance_uri(&f.worktree, "lsp"), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_string(response).await,
        r#"[{"id":"rust","name":"rust","root":"","status":"connected"}]"#
    );
}

#[tokio::test]
async fn find_symbol_is_empty_at_the_pinned_commit() {
    // `findSymbol` (handlers/file.ts:62-64) is a hardcoded `[]` in the
    // pinned TS source — the spec's "returns workspaceSymbol results" is
    // superseded by the file.
    let f = fixture();
    let router = routes::build_router(f.ctx.clone());

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
