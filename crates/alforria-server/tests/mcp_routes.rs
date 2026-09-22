//! M7.6 acceptance: the v1 `/mcp` family — status map wire shape, `add`,
//! the OAuth-unsupported 400 (flat `{"error": ...}`), the
//! `McpServerNotFoundError` 404 matrix and `GET /experimental/resource`.

use std::path::PathBuf;
use std::sync::Arc;

use alforria_core::mcp::{McpService, McpServiceInput};
use alforria_core::{EventBus, SessionServices, Storage};
use alforria_server::routes;
use alforria_server::state::{
    AuthConfig, EmptyUiBackend, InstanceFactory, InstanceStore, ServerContext,
};
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

/// A minimal MCP stdio server (M7.6 acceptance fixture — a spawned
/// subprocess, never a real endpoint).
const FIXTURE: &str = r#"
import json, sys
def out(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method", "")
    rid = msg.get("id")
    if method == "initialize":
        out({"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": msg["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture", "version": "1.0"},
        }})
    elif method == "tools/list":
        out({"jsonrpc": "2.0", "id": rid, "result": {"tools": [
            {"name": "echo", "description": "Echo",
             "inputSchema": {"type": "object"}},
        ]}})
    elif method == "roots/list":
        out({"jsonrpc": "2.0", "id": rid, "result": {"roots": []}})
"#;

fn fixture_context() -> Arc<ServerContext> {
    let storage = Arc::new(Storage::open_in_memory().unwrap());
    let bus = Arc::new(EventBus::new_shared(storage.clone(), None));
    let factory_storage = storage.clone();
    let factory: InstanceFactory = Arc::new(move |directory: &std::path::Path| {
        let agent_input = alforria_core::AgentRegistryInput {
            config: serde_json::from_value(serde_json::json!({})).unwrap(),
            skill_dirs: Vec::new(),
            reference_dirs: Vec::new(),
            worktree: directory.to_path_buf(),
            data_dir: std::env::temp_dir(),
            tmp_dir: std::env::temp_dir(),
            home: std::env::temp_dir(),
        };
        Ok(Arc::new(SessionServices::new(
            factory_storage.clone(),
            Arc::new(NoJobs),
            Arc::new(FixedClock),
            &agent_input,
        )))
    });
    let mut ctx = ServerContext::new(
        AuthConfig::new("alforria", None),
        InstanceStore::new(factory),
        storage,
        bus,
        Vec::new(),
        Arc::new(EmptyUiBackend),
    );
    let service = McpService::new(McpServiceInput {
        directory: PathBuf::from("/tmp"),
        data_dir: std::env::temp_dir(),
        mcp: serde_json::from_value(serde_json::json!({
            "srv": {
                "type": "local",
                "command": ["python3", "-c"],
            }
        }))
        .map(
            |mut entries: std::collections::BTreeMap<
                String,
                alforria_core::config::schema::McpEntry,
            >| {
                if let Some(alforria_core::config::schema::McpEntry::Server(
                    alforria_core::config::schema::McpInfo::Local(ref mut local),
                )) = entries.get_mut("srv")
                {
                    local.command.push(FIXTURE.to_string());
                }
                entries
            },
        )
        .unwrap(),
        mcp_timeout: None,
        events: None,
    });
    ctx.mcp = Arc::new(FixedMcp(Arc::new(service)));
    Arc::new(ctx)
}

struct FixedMcp(Arc<McpService>);

impl alforria_server::state::McpSource for FixedMcp {
    fn service(
        &self,
        _location: &alforria_server::middleware::location::LocationContext,
    ) -> Result<Arc<McpService>, alforria_server::ServerError> {
        Ok(self.0.clone())
    }
}

async fn request_json(
    ctx: &Arc<ServerContext>,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let router = routes::build_router(ctx.clone());
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
    }
    let request = builder
        .body(Body::from(
            body.map(|body| body.to_string()).unwrap_or_default(),
        ))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

fn q(directory: &std::path::Path) -> String {
    format!("?directory={}", directory.display())
}

#[tokio::test]
async fn status_map_has_the_wire_shape() {
    let ctx = fixture_context();
    let dir = tempfile::tempdir().unwrap();
    let (status, json) = request_json(&ctx, "GET", &format!("/mcp{}", q(dir.path())), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json,
        serde_json::json!({"srv": {"status": "connected"}}),
        "wire shape: Record<string, MCPStatus>"
    );
}

#[tokio::test]
async fn add_returns_the_status_map() {
    let ctx = fixture_context();
    let dir = tempfile::tempdir().unwrap();
    let (status, json) = request_json(
        &ctx,
        "POST",
        &format!("/mcp{}", q(dir.path())),
        Some(serde_json::json!({
            "name": "dyn",
            "config": {
                "type": "local",
                "command": ["python3", "-c", "pass"],
                "enabled": false,
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json,
        serde_json::json!({
            "srv": {"status": "connected"},
            "dyn": {"status": "disabled"},
        })
    );
}

#[tokio::test]
async fn auth_start_on_local_server_is_the_flat_400() {
    let ctx = fixture_context();
    let dir = tempfile::tempdir().unwrap();
    let (status, json) = request_json(
        &ctx,
        "POST",
        &format!("/mcp/srv/auth{}", q(dir.path())),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json,
        serde_json::json!({"error": "MCP server srv does not support OAuth"}),
        "McpUnsupportedOAuthError serializes flat (no _tag, no wrapper)"
    );
}

#[tokio::test]
async fn auth_remove_and_the_404_matrix() {
    let ctx = fixture_context();
    let dir = tempfile::tempdir().unwrap();

    let (status, json) = request_json(
        &ctx,
        "DELETE",
        &format!("/mcp/srv/auth{}", q(dir.path())),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json, serde_json::json!({"success": true}));

    let (status, json) = request_json(
        &ctx,
        "DELETE",
        &format!("/mcp/missing/auth{}", q(dir.path())),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        json,
        serde_json::json!({
            "_tag": "McpServerNotFoundError",
            "name": "missing",
            "message": "MCP server not found: missing",
        })
    );

    for (route, body) in [
        ("connect", None),
        ("disconnect", None),
        ("auth/callback", Some(serde_json::json!({"code": "x"}))),
        ("auth/authenticate", None),
    ] {
        let (status, _) = request_json(
            &ctx,
            "POST",
            &format!("/mcp/missing/{route}{}", q(dir.path())),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{route} 404s");
    }
}

#[tokio::test]
async fn disconnect_flips_the_status_to_disabled() {
    let ctx = fixture_context();
    let dir = tempfile::tempdir().unwrap();
    let (status, json) = request_json(
        &ctx,
        "POST",
        &format!("/mcp/srv/disconnect{}", q(dir.path())),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json, serde_json::json!(true));

    let (status, json) = request_json(&ctx, "GET", &format!("/mcp{}", q(dir.path())), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json, serde_json::json!({"srv": {"status": "disabled"}}));
}

#[tokio::test]
async fn experimental_resource_lists_mcp_resources() {
    let ctx = fixture_context();
    let dir = tempfile::tempdir().unwrap();
    let (status, json) = request_json(
        &ctx,
        "GET",
        &format!("/experimental/resource{}", q(dir.path())),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json,
        serde_json::json!({}),
        "the tools-only fixture advertises no resources"
    );
}
