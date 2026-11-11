//! Graceful shutdown of a live listener (alforria extension, `shutdown.rs`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use alforria_server::policy::ServerPolicy;
use alforria_server::{ListenOptions, ServerContext, ShutdownOptions};

fn options(timeout: Duration) -> ShutdownOptions {
    ShutdownOptions {
        timeout,
        ..ShutdownOptions::default()
    }
}

/// An open event stream never ends on its own; shutdown must not wait for
/// it beyond the deadline, and must stop accepting.
#[tokio::test]
async fn shutdown_does_not_wait_for_open_event_streams() {
    let ctx = Arc::new(ServerContext::for_tests());
    let listener = alforria_server::listen_with(&ListenOptions::default(), ctx)
        .await
        .unwrap();
    let url = listener.url.clone();
    let mut stream = reqwest::get(format!("{url}/global/event")).await.unwrap();
    assert!(stream.status().is_success());
    let first = stream.chunk().await.unwrap();
    assert!(first.is_some(), "the stream is open");

    let started = Instant::now();
    let report = listener.shutdown(options(Duration::from_secs(4))).await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(report.cancelled, 0);
    assert_eq!(report.abandoned, 0);
    assert!(report.checkpointed);

    // The port no longer accepts. (An open stream goes with the process.)
    assert!(reqwest::get(format!("{url}/global/health")).await.is_err());
}

/// A host config that could not be applied must not be served around.
#[tokio::test]
async fn a_listener_refuses_to_bind_without_its_host_config() {
    let mut ctx = ServerContext::for_tests();
    ctx.policy = Arc::new(ServerPolicy {
        config_error: Some("opencode.json: unexpected token".to_string()),
        ..ServerPolicy::default()
    });
    let err = alforria_server::listen_with(&ListenOptions::default(), Arc::new(ctx))
        .await
        .expect_err("must refuse");
    assert!(err.to_string().contains("host config"), "{err}");
}
