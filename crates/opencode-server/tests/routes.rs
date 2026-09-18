//! M6.1 acceptance: router-table exhaustiveness + wire-quirk matrix, exercised
//! against the full middleware stack with oneshot requests.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use opencode_server::routes;
use opencode_server::state::ServerContext;
use tower::ServiceExt;

fn router() -> axum::Router {
    routes::build_router(Arc::new(ServerContext::for_tests()))
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
async fn every_v1_route_is_registered_and_reachable() {
    let router = router();
    for (method, path) in routes::v1::ROUTES {
        let request = Request::builder()
            .method(*method)
            .uri(path.replace(['{', '}', '*'], ""))
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "{method} {path} must resolve to the stub, not the UI fallback"
        );
    }
}

#[tokio::test]
async fn every_v2_route_is_registered_and_reachable() {
    let router = router();
    for (method, path) in routes::v2::ROUTES {
        let request = Request::builder()
            .method(*method)
            .uri(path.replace(['{', '}', '*'], ""))
            .body(Body::empty())
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        // Location middleware (M6.3) runs before the stub handlers: the
        // session-scoped routes reject with SessionNotFoundError, everything
        // else reaches the defect-500 stub. Both differ from the UI
        // fallback's `{"error":"Not Found"}` body.
        let body = body(response).await;
        assert!(
            body != "{\"error\":\"Not Found\"}",
            "{method} {path} must resolve past the UI fallback"
        );
    }
}

#[tokio::test]
async fn stub_responses_carry_the_defect_envelope() {
    let router = router();
    let response = router
        .oneshot(
            Request::builder()
                .uri("/session")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body(response).await;
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["name"], "UnknownError");
    assert_eq!(
        parsed["data"]["message"],
        "Unexpected server error. Check server logs for details."
    );
    let reference = parsed["data"]["ref"].as_str().unwrap();
    assert!(reference.starts_with("err_"));
    assert_eq!(reference.len(), 12);
}

#[tokio::test]
async fn unregistered_path_returns_ui_404_envelope() {
    let router = router();
    let response = router
        .oneshot(
            Request::builder()
                .uri("/definitely-not-a-route")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(body(response).await, "{\"error\":\"Not Found\"}");
}

#[tokio::test]
async fn method_mismatch_falls_through_to_the_ui() {
    // TS registers the UI as a `*` catch-all for every method, so a
    // wrong-method request on a registered path serves the UI (404), not a
    // 405 (find-my-way has no 405 on the catch-all path).
    let router = router();
    let response = router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/global/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body(response).await, "{\"error\":\"Not Found\"}");
}

#[tokio::test]
async fn trailing_slashes_are_exact_match_not_found() {
    let router = router();
    let response = router
        .oneshot(
            Request::builder()
                .uri("/session/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body(response).await, "{\"error\":\"Not Found\"}");
}

#[tokio::test]
async fn options_requests_short_circuit_into_cors_preflight() {
    let router = router();
    let response = router
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/session")
                .header("origin", "http://localhost:3000")
                .header("access-control-request-method", "GET")
                .header("access-control-request-headers", "authorization")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "http://localhost:3000"
    );
    assert_eq!(
        response.headers()["access-control-allow-headers"],
        "authorization"
    );
    assert_eq!(
        response.headers()["access-control-allow-methods"],
        "GET, HEAD, PUT, PATCH, POST, DELETE"
    );
    assert_eq!(response.headers()["access-control-max-age"], "86400");
    assert_eq!(
        response.headers()["vary"],
        "Origin, Access-Control-Request-Headers"
    );
}

#[tokio::test]
async fn simple_get_receives_cors_headers() {
    let router = router();
    let response = router
        .oneshot(
            Request::builder()
                .uri("/session")
                .header("origin", "http://localhost:1234")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "http://localhost:1234"
    );
    assert_eq!(response.headers()["vary"], "Origin");
}

#[tokio::test]
async fn doc_and_openapi_json_are_registered() {
    let router = router();
    for path in ["/doc", "/openapi.json"] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{path} must be registered"
        );
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}

#[tokio::test]
async fn route_table_totals() {
    assert_eq!(routes::v1::ROUTES.len(), 127);
    assert_eq!(routes::v2::ROUTES.len(), 61);
}

#[tokio::test]
async fn layers_apply_to_the_ui_fallback() {
    let router = router();
    let response = router
        .oneshot(
            Request::builder()
                .uri("/definitely-not-a-route")
                .header("origin", "http://localhost:3000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "http://localhost:3000",
        "the cors layer must wrap the UI fallback too (TS wraps serveUI)"
    );
    assert_eq!(response.headers()["vary"], "Origin");
}
