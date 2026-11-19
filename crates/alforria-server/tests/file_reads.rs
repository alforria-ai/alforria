//! The file routes read a regular file of bounded size and nothing else,
//! and write what they judged (alforria extension): a FIFO does not hang a
//! worker, a huge file is not read into memory, a dangling link does not
//! smuggle a loader name, and `overwrite=false` creates a file once.

#![cfg(unix)]

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use support::{basic, bearer, body_json, call, fixture_with, urlencode, Fixture, Signer};

const AUDIENCE: &str = "agent-42";
const PASSWORD: &str = "hunter2";

fn fixture(signer: &Signer) -> Fixture {
    let f = fixture_with(
        |dirs| {
            json!({
                "server": {
                    "auth": { "bearer": { "keys": [signer.public_key()], "audience": AUDIENCE } },
                    "directories": [dirs.repo.display().to_string()],
                    "upload": { "max_bytes": 1024 }
                }
            })
        },
        Some(PASSWORD),
        Vec::new(),
    );
    std::fs::create_dir_all(f.dirs.repo.join("uploads")).unwrap();
    f
}

fn dir(f: &Fixture) -> String {
    format!(
        "directory={}",
        urlencode(&f.dirs.repo.display().to_string())
    )
}

async fn get(f: &Fixture, auth: &str, route: &str, path: &str) -> axum::http::Response<Body> {
    let uri = format!("{route}?path={}&{}", urlencode(path), dir(f));
    tokio::time::timeout(
        Duration::from_secs(2),
        call(&f.router, "GET", &uri, Some(auth), None),
    )
    .await
    .unwrap_or_else(|_| panic!("GET {route} of {path} did not answer within 2 s"))
}

async fn put(f: &Fixture, auth: &str, path: &str, extra: &str, body: &[u8]) -> StatusCode {
    let request = Request::builder()
        .method("PUT")
        .uri(format!(
            "/file/content?path={}&{}{extra}",
            urlencode(path),
            dir(f)
        ))
        .header("authorization", auth)
        .body(Body::from(body.to_vec()))
        .unwrap();
    f.router.clone().oneshot(request).await.unwrap().status()
}

/// A FIFO in the project (the agent's shell can make one) has no writer:
/// opening it for reading blocks for ever. The read routes answer `400`
/// instead of parking a worker on it — for the operator and below.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fifo_is_refused_at_once_by_the_read_routes() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let made = std::process::Command::new("mkfifo")
        .arg(f.dirs.repo.join("pipe"))
        .status()
        .unwrap();
    assert!(made.success());
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    for auth in [basic("alforria", PASSWORD), chat] {
        for route in ["/file/content", "/file/raw"] {
            let response = get(&f, &auth, route, "pipe").await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{route}");
        }
    }
    // A directory is not a file either.
    let response = get(&f, &basic("alforria", PASSWORD), "/file/content", "uploads").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// `GET /file/content` puts the whole file in one JSON document: it reads
/// at most `server.upload.max_bytes`, the size of the largest file the
/// server takes, and says `413` for more — without reading it.
#[tokio::test]
async fn the_json_read_is_capped_at_the_upload_size() {
    let signer = Signer::new();
    let f = fixture(&signer);
    std::fs::write(f.dirs.repo.join("fits.txt"), vec![b'a'; 1024]).unwrap();
    std::fs::write(f.dirs.repo.join("over.txt"), vec![b'a'; 1025]).unwrap();
    // Sparse: 50 GiB that are not there.
    std::fs::File::create(f.dirs.repo.join("sparse.bin"))
        .unwrap()
        .set_len(50 << 30)
        .unwrap();
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    for auth in [basic("alforria", PASSWORD), chat] {
        let response = get(&f, &auth, "/file/content", "fits.txt").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await["content"].as_str().unwrap().len(),
            1024
        );
        for path in ["over.txt", "sparse.bin"] {
            let response = get(&f, &auth, "/file/content", path).await;
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE, "{path}");
        }
    }
    // A file that is not there reads as empty text, as before.
    let response = get(
        &f,
        &basic("alforria", PASSWORD),
        "/file/content",
        "nope.txt",
    )
    .await;
    assert_eq!(
        body_json(response).await,
        json!({ "type": "text", "content": "" })
    );
}

/// An upload renames over its target and does not follow a last component
/// that is a link to nothing — so it lands where the link stands, under
/// the link's name. That name is judged like any other: `AGENTS.md` is not
/// uploaded by way of a dangling link called `AGENTS.md`.
#[tokio::test]
async fn an_upload_onto_a_dangling_link_with_a_loader_name_is_refused() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let uploads = f.dirs.repo.join("uploads");
    for name in ["AGENTS.md", "opencode.json"] {
        let link = uploads.join(name);
        std::os::unix::fs::symlink("/nonexistent/target", &link).unwrap();
        let status = put(&f, &chat, &format!("uploads/{name}"), "", b"be evil").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{name}");
        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "{name} was replaced by a real file"
        );
    }
    // An ordinary name behind a dangling link is still replaced in place.
    let link = uploads.join("notes.txt");
    std::os::unix::fs::symlink("/nonexistent/target", &link).unwrap();
    assert_eq!(
        put(&f, &chat, "uploads/notes.txt", "", b"notes").await,
        StatusCode::CREATED
    );
    assert_eq!(std::fs::read(&link).unwrap(), b"notes");
}

/// `overwrite=false` is decided by the rename, not by a look beforehand:
/// of many uploads of one new file, one creates it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_uploads_that_may_not_overwrite_create_exactly_one_file() {
    let signer = Signer::new();
    let f = std::sync::Arc::new(fixture(&signer));
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    for (round, auth, count) in [(0, basic("alforria", PASSWORD), 24), (1, chat, 4)] {
        for attempt in 0..10 {
            let path = format!("uploads/once-{round}-{attempt}.txt");
            let tasks: Vec<_> = (0..count)
                .map(|n| {
                    let (f, auth, path) = (f.clone(), auth.clone(), path.clone());
                    tokio::spawn(async move {
                        let body = format!("writer {n}");
                        (
                            put(&f, &auth, &path, "&overwrite=false", body.as_bytes()).await,
                            body,
                        )
                    })
                })
                .collect();
            let mut created = Vec::new();
            for task in tasks {
                let (status, body) = task.await.unwrap();
                match status {
                    StatusCode::CREATED => created.push(body),
                    StatusCode::CONFLICT => {}
                    other => panic!("{other}"),
                }
            }
            assert_eq!(created.len(), 1, "{path}: {created:?}");
            // And the file is that one's.
            assert_eq!(
                std::fs::read_to_string(f.dirs.repo.join(&path)).unwrap(),
                created[0]
            );
        }
    }
}
