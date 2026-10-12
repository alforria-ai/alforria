//! `PATCH /global/config` clears a key on `null` (deviation: TS writes the
//! null, so "Server default" could never remove `model`). Its own test
//! binary: it points `XDG_CONFIG_HOME` at a temp dir for the whole process.

use std::sync::Arc;

use alforria_server::routes;
use alforria_server::state::ServerContext;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

async fn patch(router: &axum::Router, body: &str) -> (StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/global/config")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

#[tokio::test]
async fn null_deletes_a_global_config_key() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", dir.path());
    let file = dir.path().join("opencode/opencode.jsonc");
    let router = routes::build_router(Arc::new(ServerContext::for_tests()));

    let (status, _) = patch(
        &router,
        r#"{"model":"a/b","small_model":"c/d","agent":{"build":{"model":"e/f","temperature":0.5}}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, merged) = patch(
        &router,
        r#"{"model":null,"agent":{"build":{"model":null}},"never_set":null}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let expected = serde_json::json!({"small_model":"c/d","agent":{"build":{"temperature":0.5}}});
    assert_eq!(merged, expected);
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(written, expected);
}
