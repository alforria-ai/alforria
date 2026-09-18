//! M6.2 acceptance: the Basic-auth middleware matrix, exercised end-to-end
//! through the full middleware stack with oneshot requests.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use base64::Engine;
use opencode_server::routes;
use opencode_server::state::{AuthConfig, ServerContext};
use tower::ServiceExt;

const PASSWORD: &str = "hunter2";

fn router(password: Option<&str>) -> axum::Router {
    let ctx = ServerContext::for_tests_with_auth(AuthConfig::new(
        "opencode",
        password.map(str::to_string),
    ));
    routes::build_router(Arc::new(ctx))
}

fn basic(password: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}"));
    format!("Basic {encoded}")
}

async fn body(response: axum::http::Response<Body>) -> String {
    String::from_utf8(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[tokio::test]
async fn passthrough_when_no_password_is_set() {
    // Empty password → required() is false → pure pass-through (v1
    // authorization.ts:104).
    for password in [None, Some("")] {
        let router = router(password);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}

#[tokio::test]
async fn v1_routes_reject_with_an_empty_401() {
    let router = router(Some(PASSWORD));
    for path in [
        "/session",
        "/global/health",
        "/session/ses_1/message",
        "/pty/pty_1/connect-token",
    ] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(
            response.headers()["www-authenticate"],
            "Basic realm=\"Secure Area\"",
            "{path}"
        );
        assert!(
            response.headers().get(header::CONTENT_TYPE).is_none(),
            "{path}: {:#?}",
            response.headers()
        );
        assert_eq!(body(response).await.len(), 0, "{path}");
    }
}

#[tokio::test]
async fn raw_routes_reject_with_an_empty_401() {
    let router = router(Some(PASSWORD));
    for path in ["/doc", "/definitely-not-a-route"] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(
            response.headers()["www-authenticate"],
            "Basic realm=\"Secure Area\"",
            "{path}"
        );
        assert_eq!(body(response).await.len(), 0, "{path}");
    }
}

#[tokio::test]
async fn v2_routes_reject_with_unauthorized_error_json() {
    let router = router(Some(PASSWORD));
    for (method, path) in [
        ("GET", "/api/session"),
        ("GET", "/api/health"),
        ("GET", "/api/session/ses_1/message/msg_1"),
        ("POST", "/experimental/project/prj_1/copy"),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(
            response.headers()["www-authenticate"],
            "Basic realm=\"Secure Area\"",
            "{path}"
        );
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(
            body(response).await,
            "{\"_tag\":\"UnauthorizedError\",\"message\":\"Authentication required\"}",
            "{path}"
        );
    }
}

#[tokio::test]
async fn valid_credentials_pass_on_every_surface() {
    let router = router(Some(PASSWORD));
    for path in ["/session", "/doc", "/api/session"] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::AUTHORIZATION, basic(PASSWORD))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

#[tokio::test]
async fn wrong_username_or_password_is_rejected() {
    let router = router(Some(PASSWORD));
    for password in ["wrong", "Hunter2", ""] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/session")
                    .header(header::AUTHORIZATION, basic(password))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{password}");
    }

    let encoded = base64::engine::general_purpose::STANDARD.encode("nobody:hunter2");
    let response = router
        .oneshot(
            Request::builder()
                .uri("/session")
                .header(header::AUTHORIZATION, format!("Basic {encoded}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn password_containing_colons_survives_the_split() {
    let router = router(Some("a:b"));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/session")
                .header(header::AUTHORIZATION, basic("a:b"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn lowercase_basic_scheme_is_accepted() {
    let router = router(Some(PASSWORD));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/session")
                .header(
                    header::AUTHORIZATION,
                    basic(PASSWORD).replacen("Basic", "basic", 1),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn invalid_base64_or_missing_separator_is_empty_credentials() {
    // Neither is a 400 — the credential decodes empty and auth fails.
    let router = router(Some(PASSWORD));
    for token in ["!!!not-base64!!!", "bm9jb2xvbmhlcmU="] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/session")
                    .header(header::AUTHORIZATION, format!("Basic {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{token}");
    }
}

#[tokio::test]
async fn auth_token_query_beats_the_authorization_header() {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("opencode:{PASSWORD}"));
    let router = router(Some(PASSWORD));

    // Valid token, no header.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/session?auth_token={encoded}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);

    // Valid token overrides a garbage header (token wins).
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/session?auth_token={encoded}"))
                .header(header::AUTHORIZATION, basic("wrong"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);

    // Garbage token overrides a valid header (token wins).
    let response = router
        .oneshot(
            Request::builder()
                .uri("/session?auth_token=!!!")
                .header(header::AUTHORIZATION, basic(PASSWORD))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn empty_auth_token_falls_back_to_the_header() {
    let router = router(Some(PASSWORD));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/session?auth_token=")
                .header(header::AUTHORIZATION, basic(PASSWORD))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_token_supports_percent_encoding() {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("opencode:{PASSWORD}"));
    let percent_encoded = format!("{}%3D%3D", encoded.trim_end_matches('='));
    let router = router(Some(PASSWORD));
    let response = router
        .oneshot(
            Request::builder()
                .uri(format!("/session?auth_token={percent_encoded}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn public_ui_paths_bypass_auth() {
    let router = router(Some(PASSWORD));
    for path in [
        "/site.webmanifest",
        "/web-app-manifest-192x192.png",
        "/web-app-manifest-512x512.png",
    ] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(body(response).await, "{\"error\":\"Not Found\"}", "{path}");
    }
}

#[tokio::test]
async fn pty_connect_ticket_bypasses_credentials() {
    let router = router(Some(PASSWORD));

    // Browsers cannot set headers on WebSocket upgrades — a non-empty
    // `ticket` query param skips the credential check entirely.
    for (path, expected) in [
        (
            "/pty/pty_1/connect?ticket=tkt_1",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            "/api/pty/pty_1/connect?ticket=tkt_1",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        ("/pty/pty_1/connect?ticket=", StatusCode::UNAUTHORIZED),
        ("/pty/pty_1/connect", StatusCode::UNAUTHORIZED),
        ("/api/pty/pty_1/connect", StatusCode::UNAUTHORIZED),
        ("/pty/pty_1?ticket=tkt_1", StatusCode::UNAUTHORIZED),
        (
            "/api/pty/pty_1/connect-token?ticket=tkt_1",
            StatusCode::UNAUTHORIZED,
        ),
    ] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path}");
    }
}

#[tokio::test]
async fn openapi_json_bypasses_auth() {
    // `HttpApiBuilder.layer(Api, {openapiPath})` registers the OpenAPI route
    // bare on the router (HttpApiBuilder.js:52-54) — outside any auth
    // middleware.
    let router = router(Some(PASSWORD));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn method_mismatch_falls_back_to_the_raw_middleware() {
    // PUT /session misses the registered methods, serving the UI catch-all —
    // which in TS carries the raw (empty-401) router middleware.
    let router = router(Some(PASSWORD));
    let response = router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers()["www-authenticate"],
        "Basic realm=\"Secure Area\""
    );
    assert_eq!(body(response).await.len(), 0);
}

#[tokio::test]
async fn options_preflight_skips_auth() {
    // The cors layer short-circuits OPTIONS before auth runs (TS cors runs
    // outside the route handlers).
    let router = router(Some(PASSWORD));
    let response = router
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/session")
                .header("origin", "http://localhost:3000")
                .header("access-control-request-method", "GET")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn auth_401s_still_receive_cors_headers() {
    let router = router(Some(PASSWORD));
    let response = router
        .oneshot(
            Request::builder()
                .uri("/session")
                .header("origin", "http://localhost:3000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "http://localhost:3000"
    );
}
