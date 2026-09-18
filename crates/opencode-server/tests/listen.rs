//! M6.1 acceptance: listener port fallback (`server/server.ts:117-122`) and
//! the live server round-trip.

use std::net::TcpListener;
use std::sync::Arc;

use opencode_server::{ListenOptions, ServerContext};

fn opts() -> ListenOptions {
    ListenOptions::default()
}

#[tokio::test]
async fn port_zero_prefers_4096_then_falls_back_when_taken() {
    // Free 4096 first: bind-and-drop guarantees availability right now.
    let holder = TcpListener::bind(("127.0.0.1", 4096)).unwrap();
    drop(holder);
    let listener = opencode_server::listen_with(&opts(), test_ctx())
        .await
        .expect("listen must succeed on 4096");
    assert_eq!(listener.port, 4096);
    assert_eq!(listener.url, "http://127.0.0.1:4096");
    listener.stop().await.unwrap();

    // With 4096 held, explicit 0 falls back to any free port.
    let holder = TcpListener::bind(("127.0.0.1", 4096)).unwrap();
    let listener = opencode_server::listen_with(&opts(), test_ctx())
        .await
        .expect("listen must succeed with any free port");
    assert_ne!(
        listener.port, 4096,
        "must fall back to a random port when 4096 is taken"
    );
    listener.stop().await.unwrap();
    drop(holder);
}

#[tokio::test]
async fn explicit_port_is_respected() {
    let holder = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = holder.local_addr().unwrap().port();
    drop(holder);
    let listener = opencode_server::listen_with(
        &ListenOptions {
            port,
            ..ListenOptions::default()
        },
        test_ctx(),
    )
    .await
    .unwrap();
    assert_eq!(listener.port, port);
    assert_eq!(listener.url, format!("http://127.0.0.1:{port}"));
    listener.stop().await.unwrap();
}

#[tokio::test]
async fn live_request_hits_the_router() {
    // Avoid the default port-0 fallback: it grabs 4096 and would race with
    // `port_zero_prefers_4096_then_falls_back_when_taken`.
    let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let listener = opencode_server::listen_with(
        &ListenOptions {
            port,
            ..ListenOptions::default()
        },
        test_ctx(),
    )
    .await
    .unwrap();
    let url = format!("http://127.0.0.1:{}/session", listener.port);
    let registered = reqwest::get(&url).await.unwrap();
    assert_eq!(registered.status(), 500);
    let body: serde_json::Value = registered.json().await.unwrap();
    assert_eq!(body["name"], "UnknownError");

    let missing = reqwest::get(format!("http://127.0.0.1:{}/nope", listener.port))
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    let text = missing.text().await.unwrap();
    assert_eq!(text, "{\"error\":\"Not Found\"}");
    listener.stop().await.unwrap();
}

fn test_ctx() -> Arc<ServerContext> {
    Arc::new(ServerContext::for_tests())
}
