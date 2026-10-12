//! S1: who can talk to the server — the Host allowlist, frame protection on
//! the UI, the narrowed CORS allowlist and PTY tickets checked at the auth
//! boundary, all through the full middleware stack.

use std::sync::Arc;

use alforria_server::middleware::host::HostAllowlist;
use alforria_server::routes;
use alforria_server::state::{AuthConfig, EmbeddedUiBackend, ServerContext};
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use base64::Engine;
use tower::ServiceExt;

fn ctx(password: Option<&str>) -> ServerContext {
    ServerContext::for_tests_with_ui(
        AuthConfig::new("alforria", password.map(str::to_string)),
        Arc::new(EmbeddedUiBackend),
    )
}

async fn send(ctx: ServerContext, request: Request<Body>) -> Response<Body> {
    routes::build_router(Arc::new(ctx))
        .oneshot(request)
        .await
        .unwrap()
}

fn get(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut request = Request::builder().uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn rebound_host_is_refused_without_a_password() {
    for host in ["evil.example:4096", "evil.example", "10.0.0.5:4096"] {
        let response = send(ctx(None), get("/path", &[("host", host)])).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{host}");
    }
    // Same-origin DNS rebinding: Origin and Host agree, the write still fails.
    let request = Request::builder()
        .method("POST")
        .uri("/pty")
        .header("host", "evil.example:4096")
        .header("origin", "http://evil.example:4096")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(send(ctx(None), request).await.status(), 403);
    // Preflights too.
    let request = Request::builder()
        .method("OPTIONS")
        .uri("/session")
        .header("host", "evil.example:4096")
        .header("origin", "http://evil.example:4096")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(ctx(None), request).await.status(), 403);

    for host in ["127.0.0.1:4096", "localhost:4096", "[::1]:4096"] {
        let response = send(ctx(None), get("/path", &[("host", host)])).await;
        assert_ne!(response.status(), StatusCode::FORBIDDEN, "{host}");
    }
}

#[tokio::test]
async fn bound_hostname_and_cors_hosts_are_allowed() {
    let mut context = ctx(None);
    context.hosts = HostAllowlist::new("box.lan", &[], &["https://partner.example".to_string()]);
    for host in ["box.lan:4096", "partner.example"] {
        let response = send(ctx_clone(&context), get("/path", &[("host", host)])).await;
        assert_ne!(response.status(), StatusCode::FORBIDDEN, "{host}");
    }
    let response = send(context, get("/path", &[("host", "evil.example")])).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// `ServerContext` is not `Clone`; rebuild one with the same allowlist.
fn ctx_clone(context: &ServerContext) -> ServerContext {
    let mut fresh = ctx(None);
    fresh.hosts = context.hosts.clone();
    fresh
}

#[tokio::test]
async fn host_check_is_off_with_a_password() {
    // Auth decides: a rebound page has no credentials.
    let response = send(
        ctx(Some("hunter2")),
        get("/path", &[("host", "evil.example:4096")]),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn ui_cannot_be_framed() {
    let response = send(ctx(None), get("/", &[("host", "127.0.0.1:4096")])).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-frame-options"], "DENY");
    let csp = response.headers()["content-security-policy"]
        .to_str()
        .unwrap();
    assert!(csp.contains("frame-ancestors 'none'"), "csp: {csp}");
}

#[tokio::test]
async fn foreign_origins_get_no_allow_origin() {
    for origin in [
        "http://localhost:3000",
        "http://127.0.0.1:5173",
        "https://app.opencode.ai",
        "tauri://localhost",
        "oc://renderer",
    ] {
        let response = send(
            ctx(None),
            get("/path", &[("host", "127.0.0.1:4096"), ("origin", origin)]),
        )
        .await;
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none(),
            "{origin}"
        );
    }
    let response = send(
        ctx(None),
        get(
            "/path",
            &[
                ("host", "127.0.0.1:4096"),
                ("origin", "http://127.0.0.1:4096"),
            ],
        ),
    )
    .await;
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "http://127.0.0.1:4096"
    );
}

#[tokio::test]
async fn pty_tickets_are_checked_at_the_auth_boundary() {
    // A made-up ticket no longer gets past auth.
    for uri in [
        "/pty/pty_1/connect?ticket=garbage",
        "/api/pty/pty_1/connect?ticket=garbage",
    ] {
        let response = send(ctx(Some("hunter2")), get(uri, &[])).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }

    // A live ticket for another PTY doesn't either; one for this PTY does.
    let context = ctx(Some("hunter2"));
    let ticket = context
        .pty_tickets
        .issue(alforria_server::pty::ticket::Scope {
            pty_id: "pty_1".to_string(),
            directory: None,
            workspace_id: None,
        })
        .ticket;
    let other = context
        .pty_tickets
        .issue(alforria_server::pty::ticket::Scope {
            pty_id: "pty_2".to_string(),
            directory: None,
            workspace_id: None,
        })
        .ticket;
    let router = routes::build_router(Arc::new(context));
    let response = router
        .clone()
        .oneshot(get(&format!("/pty/pty_1/connect?ticket={other}"), &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = router
        .oneshot(get(&format!("/pty/pty_1/connect?ticket={ticket}"), &[]))
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);

    // Basic auth still works without a ticket.
    let basic = base64::engine::general_purpose::STANDARD.encode("alforria:hunter2");
    let response = send(
        ctx(Some("hunter2")),
        get(
            "/pty/pty_1/connect",
            &[("authorization", &format!("Basic {basic}"))],
        ),
    )
    .await;
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
}
