//! An upload that stops sending, or sends too slowly to ever finish, is
//! ended (alforria extension).
//!
//! Alone in its file: the tests shorten the idle timeout and the total
//! deadline for the whole process, which would end the stalled uploads
//! other tests hold open.

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use serde_json::json;
use tower::ServiceExt;

use support::{bearer, body_json, fixture_with, urlencode, Signer};

const AUDIENCE: &str = "agent-42";

/// Both tests, the same values: they share a process.
fn shorten() {
    alforria_server::routes::file_ext::upload_idle_timeout(Duration::from_millis(200));
    alforria_server::routes::file_ext::upload_total_deadline(Duration::from_millis(700));
}

/// An upload that stops sending is ended, and gives back what it held.
#[tokio::test]
async fn an_upload_that_stops_sending_is_ended() {
    shorten();
    let signer = Signer::new();
    let f = fixture_with(
        |dirs| {
            json!({
                "server": {
                    "auth": { "bearer": { "keys": [signer.public_key()], "audience": AUDIENCE } },
                    "directories": [dirs.repo.display().to_string()],
                    "upload": { "max_bytes": 1024, "max_total_bytes": 100, "max_files": 50 }
                }
            })
        },
        Some("hunter2"),
        Vec::new(),
    );
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let area = f.dirs.repo.join("uploads");
    let started = std::time::Instant::now();
    // Declares 50 bytes, sends 10, then nothing.
    let first = futures::stream::once(async {
        Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![b'x'; 10]))
    });
    let request = Request::builder()
        .method("PUT")
        .uri(format!(
            "/file/content?path=uploads/slow&directory={}",
            urlencode(&f.dirs.repo.display().to_string())
        ))
        .header("authorization", &chat)
        .header("content-length", 50)
        .body(Body::from_stream(first.chain(futures::stream::pending())))
        .unwrap();
    let response = f.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(body_json(response).await["name"], "UploadTimeoutError");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        alforria_server::routes::file_ext::upload_area_in_flight(&area),
        (0, 0)
    );
    let left: Vec<_> = std::fs::read_dir(&area).unwrap().flatten().collect();
    assert!(left.is_empty(), "{left:?}");
    assert!(!area.join("slow").exists());
}

/// A body that sends a byte just inside each idle window never trips the
/// idle timeout. The upload as a whole has a deadline: it is cut off, and
/// its place, its hold on the quota and its temporary file are given up.
#[tokio::test]
async fn an_upload_that_trickles_is_cut_off_at_the_total_deadline() {
    shorten();
    let signer = Signer::new();
    let f = fixture_with(
        |dirs| {
            json!({
                "server": {
                    "auth": { "bearer": { "keys": [signer.public_key()], "audience": AUDIENCE } },
                    "directories": [dirs.repo.display().to_string()],
                    "upload": { "max_bytes": 1024, "max_total_bytes": 1000, "max_files": 50 }
                }
            })
        },
        Some("hunter2"),
        Vec::new(),
    );
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let area = f.dirs.repo.join("uploads");
    // One byte every 50 ms, for ever: well inside the 200 ms idle window.
    let trickle = futures::stream::unfold((), |()| async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Some((
            Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"x")),
            (),
        ))
    });
    let request = Request::builder()
        .method("PUT")
        .uri(format!(
            "/file/content?path=uploads/trickle&directory={}",
            urlencode(&f.dirs.repo.display().to_string())
        ))
        .header("authorization", &chat)
        .header("content-length", 500)
        .body(Body::from_stream(trickle))
        .unwrap();
    let started = std::time::Instant::now();
    let response = tokio::time::timeout(Duration::from_secs(10), f.router.clone().oneshot(request))
        .await
        .expect("the trickling upload was never cut off")
        .unwrap();
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(body_json(response).await["name"], "UploadTimeoutError");
    assert!(started.elapsed() >= Duration::from_millis(600));
    assert_eq!(
        alforria_server::routes::file_ext::upload_area_in_flight(&area),
        (0, 0)
    );
    let left: Vec<_> = std::fs::read_dir(&area).unwrap().flatten().collect();
    assert!(left.is_empty(), "{left:?}");
}
