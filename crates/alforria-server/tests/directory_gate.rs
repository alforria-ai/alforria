//! The directory allowlist and the directory a handler acts in are one
//! value (`middleware::directory`). A credential confined to
//! `server.directories` used to name an allowed directory under a spelling
//! the handler ignored — `?location[directory]=` or the
//! `x-opencode-directory` header on a route that read only `?directory=` —
//! pass the gate, and act in the server's working directory.
//!
//! The server's working directory here is the crate directory (cargo runs a
//! test binary there): it has a `Cargo.toml`, and it is outside the
//! allowlist.
mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use support::{bearer, body_string, fixture_with, urlencode, Dirs, Fixture, Signer};

const AUDIENCE: &str = "agent-42";

fn host_config(signer: &Signer) -> impl FnOnce(&Dirs) -> Value + '_ {
    move |dirs| {
        json!({
            "server": {
                "auth": { "bearer": { "keys": [signer.public_key()], "audience": AUDIENCE } },
                "directories": [dirs.repo.display().to_string()]
            },
            "memory": { "enabled": true }
        })
    }
}

/// The three ways a request spells its directory.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Spelling {
    Query,
    LocationQuery,
    Header,
}

const SPELLINGS: [Spelling; 3] = [Spelling::Query, Spelling::LocationQuery, Spelling::Header];

/// `method path` with `directory` under `spelling` (none: not named).
async fn send(
    f: &Fixture,
    auth: &str,
    method: &str,
    path: &str,
    named: &[(Spelling, &str)],
    body: Option<Value>,
) -> (StatusCode, String) {
    let mut uri = path.to_string();
    let mut request = Request::builder().method(method);
    for (spelling, directory) in named {
        let sep = if uri.contains('?') { '&' } else { '?' };
        match spelling {
            Spelling::Query => uri.push_str(&format!("{sep}directory={}", urlencode(directory))),
            Spelling::LocationQuery => uri.push_str(&format!(
                "{sep}location%5Bdirectory%5D={}",
                urlencode(directory)
            )),
            Spelling::Header => {
                request = request.header("x-opencode-directory", urlencode(directory));
            }
        }
    }
    request = request.uri(uri).header("authorization", auth);
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    let response = tower::ServiceExt::oneshot(f.router.clone(), request)
        .await
        .unwrap();
    let status = response.status();
    (status, body_string(response).await)
}

/// One read per own-route family. Each answers 2xx (or its own 404) in an
/// allowed directory.
const FAMILIES: &[(&str, &str)] = &[
    ("GET", "/file/raw?path=only-in-repo.txt"),
    ("GET", "/trigger"),
    ("GET", "/memory"),
    ("GET", "/memory/search?q=anything"),
    ("GET", "/workflow"),
    ("GET", "/workflow/run"),
    ("GET", "/session/ses_none/goal"),
];

fn fixture(signer: &Signer) -> Fixture {
    let f = fixture_with(host_config(signer), Some("pw"), Vec::new());
    std::fs::write(f.dirs.repo.join("only-in-repo.txt"), "repo file").unwrap();
    let cwd = std::env::current_dir().unwrap();
    assert!(
        cwd.join("Cargo.toml").exists(),
        "the test runs in the crate"
    );
    assert!(!f.ctx.policy.directories.allows(&cwd));
    f
}

#[tokio::test]
async fn every_own_route_family_acts_in_the_directory_the_gate_checked() {
    let signer = Signer::new();
    let f = fixture(&signer);
    // `connector`: the narrowest scope that reaches every family below,
    // confined to `server.directories` like `chat`.
    let chat = bearer(&signer.token(AUDIENCE, "connector"));
    let refused = |status: StatusCode, body: &str| {
        status == StatusCode::FORBIDDEN
            && (body.contains("server.directories") || body.contains("working directory"))
    };
    let repo = f.dirs.repo.display().to_string();
    let other = f.dirs.other.display().to_string();

    for (method, path) in FAMILIES {
        // Nothing named: the working directory, which is not allowed.
        let (status, body) = send(&f, &chat, method, path, &[], None).await;
        assert!(refused(status, &body), "{method} {path}: {status} {body}");

        for spelling in SPELLINGS {
            // An allowed directory, however it is spelled: admitted, and
            // the handler acts there.
            let (status, body) = send(&f, &chat, method, path, &[(spelling, &repo)], None).await;
            assert!(
                status.is_success(),
                "{method} {path} via {spelling:?}: {status} {body}"
            );
            // A directory outside the allowlist, however it is spelled.
            let (status, body) = send(&f, &chat, method, path, &[(spelling, &other)], None).await;
            assert!(
                refused(status, &body),
                "{method} {path} via {spelling:?}: {status} {body}"
            );
            // An allowed one under this spelling does not launder a
            // refused one under another.
            for second in SPELLINGS.into_iter().filter(|second| *second != spelling) {
                let named = [(spelling, repo.as_str()), (second, other.as_str())];
                let (status, body) = send(&f, &chat, method, path, &named, None).await;
                assert!(
                    refused(status, &body),
                    "{method} {path} via {spelling:?}+{second:?}: {status} {body}"
                );
            }
        }
    }
}

/// The reproduction: the file read answered with the working directory's
/// `Cargo.toml` when the allowed directory was named under a spelling the
/// handler ignored.
#[tokio::test]
async fn a_file_read_never_falls_back_to_the_working_directory() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let repo = f.dirs.repo.display().to_string();

    for spelling in SPELLINGS {
        // `Cargo.toml` exists in the working directory, not in the repo.
        let named = [(spelling, repo.as_str())];
        let (status, body) =
            send(&f, &chat, "GET", "/file/raw?path=Cargo.toml", &named, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{spelling:?}: {body}");
        assert!(!body.contains("[package]"), "{spelling:?}: {body}");
        let (status, body) = send(
            &f,
            &chat,
            "GET",
            "/file/raw?path=only-in-repo.txt",
            &named,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{spelling:?}: {body}");
        assert_eq!(body, "repo file");

        // A ticket is minted for the repo's file, not the cwd's.
        let (status, body) = send(
            &f,
            &chat,
            "POST",
            "/file/ticket",
            &named,
            Some(json!({ "path": "Cargo.toml" })),
        )
        .await;
        assert_ne!(status, StatusCode::OK, "{spelling:?}: {body}");
    }
}

#[tokio::test]
async fn writes_land_in_the_named_directory_under_every_spelling() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let admin = support::basic("alforria", "pw");
    let repo = f.dirs.repo.display().to_string();
    let cwd = std::env::current_dir().unwrap();

    for (index, spelling) in SPELLINGS.into_iter().enumerate() {
        let named = [(spelling, repo.as_str())];
        // An upload.
        let name = format!("uploads/gate-{index}.txt");
        let request = {
            let (mut uri, mut request) = (
                format!("/file/content?path={name}"),
                Request::builder().method("PUT"),
            );
            match spelling {
                Spelling::Query => uri.push_str(&format!("&directory={}", urlencode(&repo))),
                Spelling::LocationQuery => {
                    uri.push_str(&format!("&location%5Bdirectory%5D={}", urlencode(&repo)))
                }
                Spelling::Header => {
                    request = request.header("x-opencode-directory", urlencode(&repo))
                }
            }
            request
                .uri(uri)
                .header("authorization", &chat)
                .body(Body::from("probe"))
                .unwrap()
        };
        let response = tower::ServiceExt::oneshot(f.router.clone(), request)
            .await
            .unwrap();
        assert!(response.status().is_success(), "{spelling:?}");
        assert!(f.dirs.repo.join(&name).exists(), "{spelling:?}");
        assert!(
            !cwd.join(&name).exists(),
            "{spelling:?}: written to the cwd"
        );

        // A trigger: created in the repo's instance, so the operator finds
        // it there under the plain spelling.
        let (status, body) = send(
            &f,
            &chat,
            "POST",
            "/trigger",
            &named,
            Some(json!({ "on": { "interval": "1h" }, "prompt": format!("gate {index}") })),
        )
        .await;
        assert!(status.is_success(), "{spelling:?}: {status} {body}");
        let (status, body) = send(
            &f,
            &admin,
            "GET",
            "/trigger",
            &[(Spelling::Query, &repo)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(&format!("gate {index}")),
            "{spelling:?}: {body}"
        );
    }
}

/// The gate and the handler decode the value the same number of times:
/// `%252e%252e` is `..` to the v1 location layer and must be to the gate.
#[tokio::test]
async fn a_twice_encoded_traversal_is_judged_as_the_handler_reads_it() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let escape = format!("{}/%252e%252e/other", f.dirs.repo.display());
    for path in [
        "/file/raw?path=x",
        "/trigger",
        "/session",
        "/config/providers",
    ] {
        let sep = if path.contains('?') { '&' } else { '?' };
        let uri = format!("{path}{sep}directory={escape}");
        let request = Request::builder()
            .uri(&uri)
            .header("authorization", &chat)
            .body(Body::empty())
            .unwrap();
        let response = tower::ServiceExt::oneshot(f.router.clone(), request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
    }
}

/// The operator is not confined, and reaches the working directory the way
/// it always did.
#[tokio::test]
async fn the_operator_still_defaults_to_the_working_directory() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let admin = support::basic("alforria", "pw");
    let (status, body) = send(&f, &admin, "GET", "/file/raw?path=Cargo.toml", &[], None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("[package]"));
}

/// A bearer server whose working directory is outside `server.directories`
/// says so at startup.
#[tokio::test]
async fn a_bearer_server_outside_its_allowlist_warns_at_startup() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let warnings = alforria_server::startup_warnings(&f.ctx);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("server.directories"), "{warnings:?}");

    // No bearer auth, or the working directory allowed (the default
    // allowlist): nothing to say.
    let plain = fixture_with(|_| json!({}), Some("pw"), Vec::new());
    assert!(alforria_server::startup_warnings(&plain.ctx).is_empty());
}

/// A bearer server does not listen when a directory its tokens may open
/// contains the host config or the data directory: the agent could write
/// the policy it runs under (the project at `$HOME`).
#[tokio::test]
async fn a_bearer_server_does_not_serve_a_directory_holding_its_host_config() {
    let signer = Signer::new();
    let key = signer.public_key();
    let bearer_over = |directory: fn(&Dirs) -> std::path::PathBuf| {
        let key = key.clone();
        move |dirs: &Dirs| {
            json!({ "server": {
                "auth": { "bearer": { "keys": [key], "audience": AUDIENCE } },
                "directories": [directory(dirs).display().to_string()],
            } })
        }
    };
    // The home directory holds `.config/alforria` and `.local/share/alforria`.
    let f = fixture_with(
        bearer_over(|dirs| dirs.home.clone()),
        Some("pw"),
        Vec::new(),
    );
    let reason = alforria_server::startup_refusal(&f.ctx).expect("must refuse");
    assert!(reason.contains("server.directories"), "{reason}");
    let error = alforria_server::listen_with(
        &alforria_server::ListenOptions {
            port: 0,
            ..Default::default()
        },
        f.ctx.clone(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("refusing to listen"), "{error}");

    // The config directory itself, or a parent of it.
    let f = fixture_with(
        bearer_over(|dirs| dirs.home.join(".config")),
        Some("pw"),
        Vec::new(),
    );
    assert!(alforria_server::startup_refusal(&f.ctx).is_some());

    // Inside a protected location, or one itself — a directory under the
    // config directory, the data directory, a place under the state
    // directory that does not exist yet — and reached through a symlink:
    // each is refused, with an error naming both paths.
    fn inside_config(dirs: &Dirs) -> std::path::PathBuf {
        dirs.home.join(".config/alforria/work")
    }
    fn the_data_dir(dirs: &Dirs) -> std::path::PathBuf {
        dirs.home.join(".local/share/alforria")
    }
    fn under_state(dirs: &Dirs) -> std::path::PathBuf {
        dirs.home.join(".local/state/alforria/not/yet/there")
    }
    fn through_a_link(dirs: &Dirs) -> std::path::PathBuf {
        let link = dirs.other.join("link-to-config");
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(dirs.home.join(".config"), &link);
        link.join("alforria/work")
    }
    for directory in [inside_config, the_data_dir, under_state, through_a_link] {
        let f = fixture_with(bearer_over(directory), Some("pw"), Vec::new());
        let served = directory(&f.dirs);
        let reason = alforria_server::startup_refusal(&f.ctx)
            .unwrap_or_else(|| panic!("{} must be refused", served.display()));
        assert!(reason.contains("overlaps"), "{reason}");
        assert!(
            reason.contains(&f.dirs.home.display().to_string()),
            "both paths are named: {reason}"
        );
        let error = alforria_server::listen_with(
            &alforria_server::ListenOptions {
                port: 0,
                ..Default::default()
            },
            f.ctx.clone(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("refusing to listen"), "{error}");
    }

    // A project beside the home, and a server without bearer auth over
    // anything: served.
    let f = fixture_with(
        bearer_over(|dirs| dirs.repo.clone()),
        Some("pw"),
        Vec::new(),
    );
    assert_eq!(alforria_server::startup_refusal(&f.ctx), None);
    let f = fixture_with(
        |dirs| json!({ "server": { "directories": [dirs.home.display().to_string()] } }),
        Some("pw"),
        Vec::new(),
    );
    assert_eq!(alforria_server::startup_refusal(&f.ctx), None);
}
