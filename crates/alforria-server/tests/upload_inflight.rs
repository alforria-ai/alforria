//! Uploads still being received (alforria extension): they hold their
//! bytes against the upload directory's quota, there are only so many at
//! a time, and one that is dropped leaves nothing behind. (One that stops
//! sending is ended: `upload_idle_timeout.rs`, which shortens the timeout
//! for its whole process.)

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use serde_json::json;
use tower::ServiceExt;

use support::{bearer, body_json, fixture_with, urlencode, Fixture, Signer};

const AUDIENCE: &str = "agent-42";
const PASSWORD: &str = "hunter2";

fn fixture(signer: &Signer, total: u64) -> Fixture {
    let f = fixture_with(
        |dirs| {
            json!({
                "server": {
                    "auth": { "bearer": { "keys": [signer.public_key()], "audience": AUDIENCE } },
                    "directories": [dirs.repo.display().to_string()],
                    "upload": { "max_bytes": 1024, "max_total_bytes": total, "max_files": 50 }
                }
            })
        },
        Some(PASSWORD),
        Vec::new(),
    );
    std::fs::create_dir_all(f.dirs.repo.join("uploads")).unwrap();
    f
}

fn request(
    f: &Fixture,
    auth: &str,
    path: &str,
    declared: Option<u64>,
) -> axum::http::request::Builder {
    let mut request = Request::builder()
        .method("PUT")
        .uri(format!(
            "/file/content?path={}&directory={}",
            urlencode(path),
            urlencode(&f.dirs.repo.display().to_string())
        ))
        .header("authorization", auth);
    if let Some(declared) = declared {
        request = request.header("content-length", declared);
    }
    request
}

async fn put(f: &Fixture, auth: &str, path: &str, body: &[u8]) -> axum::http::Response<Body> {
    f.router
        .clone()
        .oneshot(
            request(f, auth, path, Some(body.len() as u64))
                .body(Body::from(body.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap()
}

/// An upload that declares `declared` bytes, sends `sent` and then
/// nothing more, for as long as the task lives.
fn stalled(
    f: &Fixture,
    auth: &str,
    path: &str,
    declared: u64,
    sent: usize,
) -> tokio::task::JoinHandle<axum::http::Response<Body>> {
    let first = futures::stream::once(async move {
        Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![b'x'; sent]))
    });
    let body = Body::from_stream(first.chain(futures::stream::pending()));
    let request = request(f, auth, path, Some(declared)).body(body).unwrap();
    let router = f.router.clone();
    tokio::spawn(async move { router.oneshot(request).await.unwrap() })
}

fn temporaries(area: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(area)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".alforria-upload-"))
        })
        .collect()
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "never: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Two uploads that each declare half the quota and then stall hold the
/// whole of it: a third is refused while they are open. Dropped, they
/// leave no temporary file and nothing reserved.
#[tokio::test]
async fn stalled_uploads_hold_their_bytes_and_a_dropped_one_leaves_nothing() {
    let signer = Signer::new();
    let f = fixture(&signer, 100);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let area = f.dirs.repo.join("uploads");
    let open: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|name| stalled(&f, &chat, &format!("uploads/{name}"), 50, 10))
        .collect();
    until("both uploads are being received", || {
        temporaries(&area).len() == 2
            && temporaries(&area)
                .iter()
                .all(|path| std::fs::metadata(path).is_ok_and(|meta| meta.len() == 10))
    })
    .await;

    let refused = put(&f, &chat, "uploads/c", b"y").await;
    assert_eq!(
        refused.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "an upload was admitted beside two that hold the whole quota"
    );
    assert_eq!(body_json(refused).await["name"], "UploadQuotaError");

    for upload in open {
        upload.abort();
        let _ = upload.await;
    }
    until("the dropped uploads' temporary files are gone", || {
        temporaries(&area).is_empty()
    })
    .await;
    // Nothing is held for them any more: the whole quota fits.
    let response = put(&f, &chat, "uploads/c", &[b'y'; 100]).await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

use alforria_server::routes::file_ext::{
    upload_area_in_flight, UPLOAD_MAX_IN_FLIGHT, UPLOAD_MAX_IN_FLIGHT_PER_PRINCIPAL,
};

/// A `chat` token for `subject`.
fn chat_as(signer: &Signer, subject: &str) -> String {
    bearer(&signer.sign(json!({
        "aud": AUDIENCE,
        "scope": "chat",
        "exp": support::now_secs() + 300,
        "sub": subject,
    })))
}

/// One principal holds only so many of an upload directory's places: at
/// its own cap it is refused, and another principal still uploads.
#[tokio::test]
async fn one_principal_cannot_hold_every_place_in_an_upload_directory() {
    let signer = Signer::new();
    let f = fixture(&signer, 10_000);
    let (greedy, other) = (chat_as(&signer, "greedy"), chat_as(&signer, "other"));
    let area = f.dirs.repo.join("uploads");
    let open: Vec<_> = (0..UPLOAD_MAX_IN_FLIGHT_PER_PRINCIPAL)
        .map(|n| stalled(&f, &greedy, &format!("uploads/g{n}"), 10, 1))
        .collect();
    until("the uploads are being received", || {
        upload_area_in_flight(&area).0 == UPLOAD_MAX_IN_FLIGHT_PER_PRINCIPAL
    })
    .await;
    assert!(UPLOAD_MAX_IN_FLIGHT_PER_PRINCIPAL < UPLOAD_MAX_IN_FLIGHT);
    let refused = put(&f, &greedy, "uploads/one-more", b"y").await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body_json(refused).await["name"], "TooManyUploadsError");
    let response = put(&f, &other, "uploads/theirs", b"y").await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "one principal's stalled uploads kept another's out"
    );
    for upload in open {
        upload.abort();
        let _ = upload.await;
    }
    until("nothing is held", || upload_area_in_flight(&area) == (0, 0)).await;
    let response = put(&f, &greedy, "uploads/one-more", b"y").await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

/// An upload directory receives only so many uploads at a time.
#[tokio::test]
async fn an_upload_directory_receives_a_bounded_number_of_uploads_at_once() {
    let signer = Signer::new();
    let f = fixture(&signer, 10_000);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let area = f.dirs.repo.join("uploads");
    // From as many principals as it takes: each has a cap of its own.
    let open: Vec<_> = (0..UPLOAD_MAX_IN_FLIGHT)
        .map(|n| {
            let auth = chat_as(
                &signer,
                &format!("user-{}", n / UPLOAD_MAX_IN_FLIGHT_PER_PRINCIPAL),
            );
            stalled(&f, &auth, &format!("uploads/f{n}"), 10, 1)
        })
        .collect();
    until("the uploads are being received", || {
        upload_area_in_flight(&area) == (UPLOAD_MAX_IN_FLIGHT, 10 * UPLOAD_MAX_IN_FLIGHT as u64)
    })
    .await;
    let refused = put(&f, &chat, "uploads/one-more", b"y").await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body_json(refused).await["name"], "TooManyUploadsError");
    // The operator is not counted among them.
    let admin = support::basic("alforria", PASSWORD);
    let response = put(&f, &admin, "uploads/operator", b"y").await;
    assert_eq!(response.status(), StatusCode::CREATED);

    for upload in open {
        upload.abort();
        let _ = upload.await;
    }
    until("nothing is held", || upload_area_in_flight(&area) == (0, 0)).await;
    let response = put(&f, &chat, "uploads/one-more", b"y").await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

/// A temporary file nobody is writing — what a server killed mid-upload
/// leaves — is removed when the directory is counted, and not counted. A
/// recent one may be another process's upload, and stays.
#[tokio::test]
async fn a_recount_removes_leftover_temporary_files() {
    let signer = Signer::new();
    let f = fixture(&signer, 100);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let area = f.dirs.repo.join("uploads");
    let (old, recent) = (
        area.join(".alforria-upload-old"),
        area.join(".alforria-upload-recent"),
    );
    std::fs::write(&old, [b'x'; 90]).unwrap();
    std::fs::write(&recent, [b'x'; 10]).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    // 90 of 100 would not fit beside the leftover.
    let response = put(&f, &chat, "uploads/a", &[b'y'; 90]).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert!(!old.exists(), "the leftover is still there");
    assert!(recent.exists(), "a recent temporary file was removed");
}
