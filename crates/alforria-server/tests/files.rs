//! File tickets, raw downloads and uploads (alforria extension,
//! `routes/file_ext.rs`), through the full middleware stack.

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use support::{
    bearer, body_json, body_string, call, fixture_with, urlencode, Dirs, Fixture, Signer,
};

const AUDIENCE: &str = "agent-42";
const PASSWORD: &str = "hunter2";

fn host_config(
    signer: &Signer,
    max_bytes: u64,
    upload_directories: Option<Value>,
) -> impl FnOnce(&Dirs) -> Value + '_ {
    move |dirs| {
        let mut upload = json!({ "max_bytes": max_bytes });
        if let Some(directories) = upload_directories {
            upload["directories"] = directories;
        }
        json!({
            "server": {
                "auth": { "bearer": { "keys": [signer.public_key()], "audience": AUDIENCE } },
                "directories": [dirs.repo.display().to_string()],
                "upload": upload
            }
        })
    }
}

fn fixture(signer: &Signer) -> Fixture {
    fixture_uploading_into(signer, None)
}

fn fixture_uploading_into(signer: &Signer, upload_directories: Option<Value>) -> Fixture {
    let f = fixture_with(
        host_config(signer, 1024, upload_directories),
        Some(PASSWORD),
        Vec::new(),
    );
    std::fs::create_dir_all(f.dirs.repo.join("uploads")).unwrap();
    std::fs::write(f.dirs.repo.join("uploads/old.txt"), "0123456789").unwrap();
    std::fs::create_dir_all(f.dirs.repo.join("img")).unwrap();
    std::fs::write(f.dirs.repo.join("img/cat.png"), b"\x89PNG\r\n\x1a\n-cat-").unwrap();
    std::fs::write(f.dirs.repo.join("notes.txt"), "0123456789").unwrap();
    std::fs::write(f.dirs.repo.join("page.html"), "<script>alert(1)</script>").unwrap();
    std::fs::write(f.dirs.other.join("secret.txt"), "s3cret").unwrap();
    f
}

fn dir(f: &Fixture) -> String {
    format!(
        "directory={}",
        urlencode(&f.dirs.repo.display().to_string())
    )
}

async fn ticket_for(f: &Fixture, auth: &str, path: &str) -> Value {
    let response = call(
        &f.router,
        "POST",
        &format!("/file/ticket?{}", dir(f)),
        Some(auth),
        Some(json!({ "path": path })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "ticket for {path}");
    body_json(response).await
}

async fn put(
    f: &Fixture,
    auth: Option<&str>,
    path: &str,
    extra: &str,
    body: &[u8],
) -> axum::http::Response<Body> {
    let mut request = Request::builder().method("PUT").uri(format!(
        "/file/content?path={}&{}{extra}",
        urlencode(path),
        dir(f)
    ));
    if let Some(auth) = auth {
        request = request.header("authorization", auth);
    }
    f.router
        .clone()
        .oneshot(request.body(Body::from(body.to_vec())).unwrap())
        .await
        .unwrap()
}

/// A ticket opens exactly one file, with no other credential, until it
/// expires — enough for an `<img>` or a download link.
#[tokio::test]
async fn a_file_ticket_reads_its_file_without_a_header() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let ticket = ticket_for(&f, &chat, "img/cat.png").await;
    assert_eq!(ticket["expires_in"], 60);
    assert_eq!(ticket["path"], "img/cat.png");
    let url = ticket["url"].as_str().unwrap();
    assert!(url.starts_with("/file/raw?"), "{url}");

    // The ready-made URL, with no Authorization header.
    let response = call(&f.router, "GET", url, None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert!(response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .starts_with("sandbox"));
    assert_eq!(
        response.headers()["content-disposition"],
        "inline; filename=\"cat.png\""
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"\x89PNG\r\n\x1a\n-cat-");

    // Usable again (a browser re-fetches), and as a download.
    let again = call(&f.router, "GET", &format!("{url}&download=1"), None, None).await;
    assert_eq!(again.status(), StatusCode::OK);
    assert_eq!(
        again.headers()["content-disposition"],
        "attachment; filename=\"cat.png\""
    );

    // The frozen JSON route takes the ticket too.
    let id = ticket["ticket"].as_str().unwrap();
    let json_read = call(
        &f.router,
        "GET",
        &format!("/file/content?path=img%2Fcat.png&{}&ticket={id}", dir(&f)),
        None,
        None,
    )
    .await;
    assert_eq!(json_read.status(), StatusCode::OK);

    // Without a ticket the routes are as closed as any other.
    let bare = call(
        &f.router,
        "GET",
        &format!("/file/raw?path=img%2Fcat.png&{}", dir(&f)),
        None,
        None,
    )
    .await;
    assert_eq!(bare.status(), StatusCode::UNAUTHORIZED);
}

/// A ticket is for one path of one project, read-only, and nothing else.
#[tokio::test]
async fn a_file_ticket_opens_nothing_but_its_file() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let ticket = ticket_for(&f, &chat, "img/cat.png").await;
    let id = ticket["ticket"].as_str().unwrap();
    let other = urlencode(&f.dirs.other.display().to_string());

    for (method, uri) in [
        // Another file of the project.
        (
            "GET",
            format!("/file/raw?path=notes.txt&{}&ticket={id}", dir(&f)),
        ),
        (
            "GET",
            format!("/file/content?path=notes.txt&{}&ticket={id}", dir(&f)),
        ),
        // The same relative path in another project.
        (
            "GET",
            format!("/file/raw?path=img%2Fcat.png&directory={other}&ticket={id}"),
        ),
        // Traversal from the ticket's path.
        (
            "GET",
            format!(
                "/file/raw?path=img%2F..%2Fnotes.txt&{}&ticket={id}",
                dir(&f)
            ),
        ),
        // Other routes entirely.
        ("GET", format!("/file?path=.&{}&ticket={id}", dir(&f))),
        ("GET", format!("/session?{}&ticket={id}", dir(&f))),
        ("GET", format!("/config?{}&ticket={id}", dir(&f))),
        // Writing.
        (
            "PUT",
            format!("/file/content?path=img%2Fcat.png&{}&ticket={id}", dir(&f)),
        ),
        ("POST", format!("/file/ticket?{}&ticket={id}", dir(&f))),
        // A ticket that was never issued.
        (
            "GET",
            format!("/file/raw?path=img%2Fcat.png&{}&ticket=nope", dir(&f)),
        ),
    ] {
        let response = call(&f.router, method, &uri, None, None).await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri}"
        );
    }

    // An equivalent spelling of the ticket's own path is still its file.
    let same = call(
        &f.router,
        "GET",
        &format!("/file/raw?path=img%2F.%2Fcat.png&{}&ticket={id}", dir(&f)),
        None,
        None,
    )
    .await;
    assert_eq!(same.status(), StatusCode::OK);
}

/// Tickets are only minted for existing files inside the project, by a
/// caller who could read the file anyway.
#[tokio::test]
async fn tickets_are_only_minted_for_project_files() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let secret = f.dirs.other.join("secret.txt").display().to_string();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        f.dirs.other.join("secret.txt"),
        f.dirs.repo.join("link.txt"),
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&f.dirs.other, f.dirs.repo.join("linkdir")).unwrap();

    let mint = |path: String| {
        let (router, chat, query) = (f.router.clone(), chat.clone(), dir(&f));
        async move {
            call(
                &router,
                "POST",
                &format!("/file/ticket?{query}"),
                Some(&chat),
                Some(json!({ "path": path })),
            )
            .await
            .status()
        }
    };
    assert_eq!(
        mint("../other/secret.txt".to_string()).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        mint("img/../../other/secret.txt".to_string()).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(mint(secret.clone()).await, StatusCode::FORBIDDEN);
    assert_eq!(mint("/etc/passwd".to_string()).await, StatusCode::FORBIDDEN);
    #[cfg(unix)]
    {
        assert_eq!(mint("link.txt".to_string()).await, StatusCode::FORBIDDEN);
        assert_eq!(
            mint("linkdir/secret.txt".to_string()).await,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(mint("missing.txt".to_string()).await, StatusCode::NOT_FOUND);
    assert_eq!(mint("img".to_string()).await, StatusCode::BAD_REQUEST);
    assert_eq!(mint(String::new()).await, StatusCode::BAD_REQUEST);

    // No credential, no ticket.
    let anonymous = call(
        &f.router,
        "POST",
        &format!("/file/ticket?{}", dir(&f)),
        None,
        Some(json!({ "path": "notes.txt" })),
    )
    .await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    // A project outside the allowlist.
    let elsewhere = call(
        &f.router,
        "POST",
        &format!(
            "/file/ticket?directory={}",
            urlencode(&f.dirs.other.display().to_string())
        ),
        Some(&chat),
        Some(json!({ "path": "secret.txt" })),
    )
    .await;
    assert_eq!(elsewhere.status(), StatusCode::FORBIDDEN);
}

/// A path is judged where the kernel lands that spelling, not where the
/// spelling reads once `..` is folded out of it: with `out -> other/deep`,
/// `out/../notes.txt` is `other/notes.txt` for the kernel and
/// `repo/notes.txt` for a lexical normaliser. A ticket for the project's
/// `notes.txt` and a `chat` token used to read the other file through the
/// frozen `GET /file/content`, which hands the spelling to the kernel.
#[cfg(unix)]
#[tokio::test]
async fn file_routes_judge_a_path_where_the_kernel_lands_it() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    std::fs::create_dir_all(f.dirs.other.join("deep")).unwrap();
    std::fs::write(f.dirs.other.join("notes.txt"), "theirs").unwrap();
    std::os::unix::fs::symlink(f.dirs.other.join("deep"), f.dirs.repo.join("out")).unwrap();
    std::os::unix::fs::symlink(f.dirs.other.join("deep"), f.dirs.repo.join("uploads/out")).unwrap();
    let spelled = urlencode("out/../notes.txt");

    // A file ticket for the project's notes.txt does not open the other.
    let ticket = ticket_for(&f, &chat, "notes.txt").await;
    let id = ticket["ticket"].as_str().unwrap();
    for route in ["/file/content", "/file/raw"] {
        let uri = format!("{route}?path={spelled}&{}&ticket={id}", dir(&f));
        let response = call(&f.router, "GET", &uri, None, None).await;
        let status = response.status();
        let body = body_string(response).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}: {body}");
        assert!(!body.contains("theirs"), "{uri}: {body}");
    }
    // The ticket still opens its own file under an honest `..`.
    let honest = format!(
        "/file/content?path={}&{}&ticket={id}",
        urlencode("img/../notes.txt"),
        dir(&f)
    );
    let response = call(&f.router, "GET", &honest, None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_string(response).await.contains("0123456789"));

    // Nor does a `chat` token read, list or mint a ticket through it.
    for (method, uri, body) in [
        (
            "GET",
            format!("/file/content?path={spelled}&{}", dir(&f)),
            None,
        ),
        ("GET", format!("/file/raw?path={spelled}&{}", dir(&f)), None),
        (
            "GET",
            format!("/file?path={}&{}", urlencode("out/.."), dir(&f)),
            None,
        ),
        (
            "POST",
            format!("/file/ticket?{}", dir(&f)),
            Some(json!({ "path": "out/../notes.txt" })),
        ),
    ] {
        let response = call(&f.router, method, &uri, Some(&chat), body).await;
        let status = response.status();
        let body = body_string(response).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {body}");
        assert!(!body.contains("theirs"), "{method} {uri}: {body}");
    }

    // The upload area: `uploads/out/../x.txt` is beside where `out` leads,
    // outside the project — not `uploads/x.txt`. And below a link, a
    // directory that does not exist yet is where the link leads.
    for path in ["uploads/out/../x.txt", "uploads/out/new/dir/x.txt"] {
        let response = put(&f, Some(&chat), path, "", b"x").await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    assert!(!f.dirs.repo.join("uploads/x.txt").exists());
    assert!(!f.dirs.other.join("x.txt").exists());
    assert!(!f.dirs.other.join("deep/new").exists());
    // What cannot be resolved — a `..` after a directory that does not
    // exist — is refused, not guessed.
    let response = put(&f, Some(&chat), "uploads/nowhere/../x.txt", "", b"x").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // An honest spelling still uploads.
    let response = put(&f, Some(&chat), "uploads/../uploads/./x.txt", "", b"x").await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

/// An upload area whose parent is a symlink and which does not exist yet is
/// where the link leads: inside the project it is an area, outside it is
/// none.
#[cfg(unix)]
#[tokio::test]
async fn an_upload_area_behind_a_link_is_where_the_link_leads() {
    let signer = Signer::new();
    let f = fixture_uploading_into(&signer, Some(json!(["inbox/new", "away/new"])));
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    std::fs::create_dir_all(f.dirs.repo.join("real-inbox")).unwrap();
    std::os::unix::fs::symlink(f.dirs.repo.join("real-inbox"), f.dirs.repo.join("inbox")).unwrap();
    std::os::unix::fs::symlink(&f.dirs.other, f.dirs.repo.join("away")).unwrap();

    let inside = put(&f, Some(&chat), "inbox/new/a.txt", "", b"x").await;
    assert_eq!(inside.status(), StatusCode::CREATED);
    assert!(f.dirs.repo.join("real-inbox/new/a.txt").is_file());
    // The same place under its real name is the same area.
    let real = put(&f, Some(&chat), "real-inbox/new/b.txt", "", b"x").await;
    assert_eq!(real.status(), StatusCode::CREATED);
    let outside = put(&f, Some(&chat), "away/new/a.txt", "", b"x").await;
    assert_eq!(outside.status(), StatusCode::FORBIDDEN);
    assert!(!f.dirs.other.join("new").exists());
}

/// An upload is written through the handle of the directory that was
/// judged: a link put in the way between the check and the write — here by
/// a hook that runs exactly there — is not followed into `.opencode`.
#[cfg(unix)]
#[tokio::test]
async fn an_upload_is_not_redirected_by_a_swap_after_the_check() {
    use alforria_core::protect::fail_point::between_check_and_write;
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let agents = f.dirs.repo.join(".opencode/agent");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::create_dir_all(f.dirs.repo.join("uploads/sub")).unwrap();
    let repo = std::fs::canonicalize(&f.dirs.repo).unwrap();

    let (at, moved, link_to) = (
        repo.join("uploads/sub"),
        repo.join("uploads/sub-was"),
        repo.join(".opencode/agent"),
    );
    let armed = between_check_and_write(&repo.join("uploads/sub"), move |_| {
        std::fs::rename(&at, &moved).unwrap();
        std::os::unix::fs::symlink(&link_to, &at).unwrap();
    });
    let response = put(&f, Some(&chat), "uploads/sub/x.md", "", b"be sneaky").await;
    let status = response.status();
    drop(armed);
    assert!(repo.join("uploads/sub").is_symlink(), "the swap happened");
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "{}",
        body_string(response).await
    );
    assert_eq!(
        std::fs::read_dir(&agents).unwrap().count(),
        0,
        "nothing was written into .opencode/agent"
    );

    // Without a swap the same upload goes through.
    let response = put(&f, Some(&chat), "uploads/other/x.md", "", b"fine").await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

/// The raw route with a bearer header: ranges, media types, and the same
/// project boundary.
#[tokio::test]
async fn raw_reads_serve_ranges_and_stay_in_the_project() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let raw = |path: &str| format!("/file/raw?path={}&{}", urlencode(path), dir(&f));

    let whole = call(&f.router, "GET", &raw("notes.txt"), Some(&chat), None).await;
    assert_eq!(whole.status(), StatusCode::OK);
    assert_eq!(whole.headers()["content-type"], "text/plain; charset=utf-8");
    assert_eq!(whole.headers()["content-length"], "10");
    assert_eq!(body_string(whole).await, "0123456789");

    let request = Request::builder()
        .uri(raw("notes.txt"))
        .header("authorization", &chat)
        .header("range", "bytes=2-5")
        .body(Body::empty())
        .unwrap();
    let partial = f.router.clone().oneshot(request).await.unwrap();
    assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(partial.headers()["content-range"], "bytes 2-5/10");
    assert_eq!(body_string(partial).await, "2345");

    let request = Request::builder()
        .uri(raw("notes.txt"))
        .header("authorization", &chat)
        .header("range", "bytes=50-")
        .body(Body::empty())
        .unwrap();
    let unsatisfiable = f.router.clone().oneshot(request).await.unwrap();
    assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);

    // An HTML file is served as an inert, sandboxed document.
    let html = call(&f.router, "GET", &raw("page.html"), Some(&chat), None).await;
    assert_eq!(html.status(), StatusCode::OK);
    assert!(html.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .starts_with("sandbox"));

    for path in ["../other/secret.txt", "/etc/passwd"] {
        let response = call(&f.router, "GET", &raw(path), Some(&chat), None).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    assert_eq!(
        call(&f.router, "GET", &raw("missing.bin"), Some(&chat), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&f.router, "GET", &raw("img"), Some(&chat), None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
}

/// Uploads write the body as is, create directories, replace atomically,
/// and can be told not to overwrite.
#[tokio::test]
async fn uploads_write_files_into_the_upload_area() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));

    let created = put(
        &f,
        Some(&chat),
        "uploads/inbox/2026/report.bin",
        "",
        &[0, 1, 2, 255],
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let body = body_json(created).await;
    assert_eq!(body["path"], "uploads/inbox/2026/report.bin");
    assert_eq!(body["size"], 4);
    assert_eq!(body["created"], true);
    assert_eq!(
        std::fs::read(f.dirs.repo.join("uploads/inbox/2026/report.bin")).unwrap(),
        [0, 1, 2, 255]
    );

    let replaced = put(&f, Some(&chat), "uploads/old.txt", "", b"new").await;
    assert_eq!(replaced.status(), StatusCode::OK);
    assert_eq!(body_json(replaced).await["created"], false);
    let old = f.dirs.repo.join("uploads/old.txt");
    assert_eq!(std::fs::read_to_string(&old).unwrap(), "new");

    let kept = put(
        &f,
        Some(&chat),
        "uploads/old.txt",
        "&overwrite=false",
        b"again",
    )
    .await;
    assert_eq!(kept.status(), StatusCode::CONFLICT);
    assert_eq!(std::fs::read_to_string(&old).unwrap(), "new");

    // `..` that stays inside the area is just a path — once what it
    // climbs out of exists: a `..` after a directory that is not there has
    // no kernel reading, and is refused instead of folded away.
    let unresolved = put(&f, Some(&chat), "uploads/a/b/../c.txt", "", b"ok").await;
    assert_eq!(unresolved.status(), StatusCode::FORBIDDEN);
    std::fs::create_dir_all(f.dirs.repo.join("uploads/a/b")).unwrap();
    let inside = put(&f, Some(&chat), "uploads/a/b/../c.txt", "", b"ok").await;
    assert_eq!(inside.status(), StatusCode::CREATED);
    assert!(f.dirs.repo.join("uploads/a/c.txt").exists());

    // Nothing is left behind beside the targets.
    let leftovers: Vec<String> = std::fs::read_dir(f.dirs.repo.join("uploads"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".alforria-upload-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    assert_eq!(
        put(&f, Some(&chat), "uploads/a", "", b"x").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        put(&f, None, "uploads/x.txt", "", b"x").await.status(),
        StatusCode::UNAUTHORIZED
    );
}

/// The size limit holds whether or not the client declares a length, and
/// an oversized upload leaves the existing file untouched.
#[tokio::test]
async fn uploads_are_size_limited() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let exact = vec![b'a'; 1024];
    assert_eq!(
        put(&f, Some(&chat), "uploads/exact.bin", "", &exact)
            .await
            .status(),
        StatusCode::CREATED
    );

    let over = vec![b'b'; 1025];
    let declared = put(&f, Some(&chat), "uploads/old.txt", "", &over).await;
    assert_eq!(declared.status(), StatusCode::PAYLOAD_TOO_LARGE);

    // A streamed body with no Content-Length.
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
        (0..5).map(|_| Ok(vec![b'c'; 300])).collect();
    let request = Request::builder()
        .method("PUT")
        .uri(format!("/file/content?path=uploads%2Fold.txt&{}", dir(&f)))
        .header("authorization", &chat)
        .body(Body::from_stream(futures::stream::iter(chunks)))
        .unwrap();
    let streamed = f.router.clone().oneshot(request).await.unwrap();
    assert_eq!(streamed.status(), StatusCode::PAYLOAD_TOO_LARGE);

    assert_eq!(
        std::fs::read_to_string(f.dirs.repo.join("uploads/old.txt")).unwrap(),
        "0123456789",
        "the old file survives a refused upload"
    );
    let leftovers = std::fs::read_dir(f.dirs.repo.join("uploads"))
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".alforria-upload-")
        })
        .count();
    assert_eq!(leftovers, 0, "the partial upload was removed");
}

fn basic() -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("alforria:{PASSWORD}"))
    )
}

/// Path traversal and symlink escapes out of the project are refused for
/// every caller: an HTTP request cannot ask to cross the project boundary
/// the way a tool can.
#[tokio::test]
async fn uploads_cannot_leave_the_project() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let basic = basic();
    #[cfg(unix)]
    {
        for base in ["", "uploads/"] {
            let at = |name: &str| f.dirs.repo.join(format!("{base}{name}"));
            std::os::unix::fs::symlink(&f.dirs.other, at("linkdir")).unwrap();
            std::os::unix::fs::symlink(f.dirs.other.join("secret.txt"), at("link.txt")).unwrap();
            // A dangling link to a file that does not exist yet.
            std::os::unix::fs::symlink(f.dirs.other.join("planted.txt"), at("dangling.txt"))
                .unwrap();
        }
    }
    let outside = f.dirs.other.join("planted.txt").display().to_string();
    let mut escapes = vec![
        "../other/planted.txt".to_string(),
        "uploads/../../other/planted.txt".to_string(),
        "uploads/new/dir/../../../../other/planted.txt".to_string(),
        outside.clone(),
        "/tmp/alforria-upload-escape-test".to_string(),
    ];
    if cfg!(unix) {
        escapes.extend(
            [
                "linkdir/planted.txt",
                "linkdir/new/planted.txt",
                "link.txt",
                "uploads/linkdir/planted.txt",
                "uploads/linkdir/new/planted.txt",
                "uploads/link.txt",
            ]
            .map(str::to_string),
        );
    }
    for auth in [chat.as_str(), basic.as_str()] {
        for path in &escapes {
            let response = put(&f, Some(auth), path, "", b"planted").await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        }
    }
    assert!(!f.dirs.other.join("planted.txt").exists());
    assert!(!f.dirs.other.join("new").exists());
    assert!(!std::path::Path::new("/tmp/alforria-upload-escape-test").exists());
    assert_eq!(
        std::fs::read_to_string(f.dirs.other.join("secret.txt")).unwrap(),
        "s3cret",
        "a link to an outside file was not written through"
    );

    // A dangling link is replaced, not written through.
    #[cfg(unix)]
    {
        let response = put(&f, Some(&chat), "uploads/dangling.txt", "", b"mine").await;
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(!f.dirs.other.join("planted.txt").exists());
        assert_eq!(
            std::fs::read_to_string(f.dirs.repo.join("uploads/dangling.txt")).unwrap(),
            "mine"
        );
    }
}

/// Below `admin` an upload lands in the upload area or nowhere: the rest of
/// the project is the agent's (through the model) and the operator's.
#[tokio::test]
async fn narrow_tokens_upload_into_the_upload_area_only() {
    let signer = Signer::new();
    let f = fixture(&signer);
    #[cfg(unix)]
    {
        // Links out of the area, to other parts of the project.
        std::os::unix::fs::symlink(f.dirs.repo.join("img"), f.dirs.repo.join("uploads/to-img"))
            .unwrap();
        std::os::unix::fs::symlink(&f.dirs.repo, f.dirs.repo.join("uploads/to-root")).unwrap();
    }
    for scope in ["chat", "connector"] {
        let token = bearer(&signer.token(AUDIENCE, scope));
        let mut outside_area = vec![
            "notes.txt",
            "new.txt",
            "img/cat.png",
            "src/main.rs",
            "uploads/../notes.txt",
            "uploadsx/a.txt",
            "Uploads2/a.txt",
        ];
        if cfg!(unix) {
            outside_area.extend([
                "uploads/to-img/cat.png",
                "uploads/to-root/notes.txt",
                "uploads/to-root/opencode.json",
            ]);
        }
        for path in outside_area {
            let response = put(&f, Some(&token), path, "", b"x").await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{scope} {path}");
            assert!(body_string(response)
                .await
                .contains("server.upload.directories"));
        }
        assert_eq!(
            put(&f, Some(&token), &format!("uploads/{scope}.txt"), "", b"x")
                .await
                .status(),
            StatusCode::CREATED
        );
    }
    assert_eq!(
        std::fs::read_to_string(f.dirs.repo.join("notes.txt")).unwrap(),
        "0123456789"
    );
    assert!(!f.dirs.repo.join("opencode.json").exists());
    assert!(
        !f.dirs.repo.join("src").exists(),
        "no directory was created on the way"
    );

    // The operator and an admin token write anywhere in the project.
    let admin = bearer(&signer.token(AUDIENCE, "admin"));
    assert_eq!(
        put(&f, Some(&admin), "src/main.rs", "", b"x")
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        put(&f, Some(&basic()), "notes.txt", "", b"y")
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        put(&f, Some(&admin), ".opencode/command/ok.md", "", b"hi")
            .await
            .status(),
        StatusCode::CREATED
    );
}

/// Every name the loaders act on — taken from the constants the loaders
/// themselves use — is closed to an upload below `admin`, at any depth, in
/// any case, as a file or as a directory to be created, inside the upload
/// area and with the whole project opened up as the upload area. A new
/// discovery location added to `alforria_core::project_files` is covered
/// here without touching this test.
#[tokio::test]
async fn narrow_tokens_cannot_upload_anything_the_loaders_read() {
    use alforria_core::project_files::{loader_files, LOADER_DIRS};

    let mut names: Vec<String> = Vec::new();
    for file in loader_files() {
        names.push(file.to_string());
        names.push(file.to_uppercase());
        names.push(file.to_lowercase());
    }
    let mut paths: Vec<String> = Vec::new();
    for name in &names {
        // The name itself, nested, and as a directory to create.
        paths.push(name.clone());
        paths.push(format!("deep/er/{name}"));
        paths.push(format!("{name}/inner.txt"));
    }
    for directory in LOADER_DIRS {
        for spelling in [directory.to_string(), directory.to_uppercase()] {
            paths.push(format!("{spelling}/x.md"));
            paths.push(format!("{spelling}/agent/x.md"));
            paths.push(format!("{spelling}/command/x.md"));
            paths.push(format!("{spelling}/skills/s/SKILL.md"));
            paths.push(format!("{spelling}/workflows/w.js"));
            paths.push(format!("{spelling}/hooks/pre-commit"));
            paths.push(format!("pkg/{spelling}/x.md"));
        }
    }
    // The places the loaders are known to look today, spelled out, so the
    // constants cannot quietly lose one.
    for known in [
        "opencode.json",
        "opencode.jsonc",
        "config.json",
        "AGENTS.md",
        "CLAUDE.md",
        "CONTEXT.md",
        ".opencode/opencode.json",
        ".opencode/agent/a.md",
        ".opencode/command/c.md",
        ".opencode/mode/m.md",
        ".opencode/skill/s/SKILL.md",
        ".opencode/workflows/w.js",
        ".opencode/plans/p.md",
        ".claude/skills/s/SKILL.md",
        ".claude/workflows/w.js",
        ".claude/CLAUDE.md",
        ".agents/skills/s/SKILL.md",
        ".git/hooks/pre-commit",
        ".git/config",
    ] {
        paths.push(known.to_string());
    }

    let signer = Signer::new();
    for (mode, directories, prefixes) in [
        ("default area", None, vec!["uploads/"]),
        (
            "whole project",
            Some(json!(["."])),
            vec!["", "uploads/", "sub/"],
        ),
    ] {
        let f = fixture_uploading_into(&signer, directories);
        let chat = bearer(&signer.token(AUDIENCE, "chat"));
        // Links from the area into loader locations.
        std::fs::create_dir_all(f.dirs.repo.join(".opencode/command")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            f.dirs.repo.join(".opencode"),
            f.dirs.repo.join("uploads/cfg"),
        )
        .unwrap();
        for prefix in &prefixes {
            for path in &paths {
                let path = format!("{prefix}{path}");
                let response = put(&f, Some(&chat), &path, "", b"{}").await;
                assert_eq!(response.status(), StatusCode::FORBIDDEN, "{mode}: {path}");
            }
        }
        #[cfg(unix)]
        assert_eq!(
            put(&f, Some(&chat), "uploads/cfg/command/pwn.md", "", b"x")
                .await
                .status(),
            StatusCode::FORBIDDEN,
            "{mode}: through a link into .opencode"
        );
        assert!(!f.dirs.repo.join(".opencode/command/pwn.md").exists());
        for created in [
            "uploads/deep",
            "uploads/.git",
            "uploads/.opencode",
            "sub",
            ".claude",
        ] {
            assert!(
                !f.dirs.repo.join(created).exists(),
                "{mode}: {created} was created on the way to a refusal"
            );
        }
        // An ordinary file still uploads.
        assert_eq!(
            put(
                &f,
                Some(&chat),
                &format!("{}plain.txt", prefixes[0]),
                "",
                b"x"
            )
            .await
            .status(),
            StatusCode::CREATED,
            "{mode}"
        );
    }
}
