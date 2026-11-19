//! Bearer authentication end to end (alforria extension): scopes against
//! representative routes, the directory allowlist, and every way a token is
//! refused.

mod support;

use axum::http::StatusCode;
use base64::Engine;
use serde_json::{json, Value};

use support::{
    bearer, body_json, body_string, call, fixture_with, urlencode, Dirs, Fixture, Signer,
};

const AUDIENCE: &str = "agent-42";
const PASSWORD: &str = "hunter2";

fn host_config(signer: &Signer) -> impl FnOnce(&Dirs) -> Value + '_ {
    move |dirs| {
        json!({
            "server": {
                "auth": { "bearer": {
                    "keys": [signer.public_key()],
                    "audience": AUDIENCE,
                    "tokens_sha256": [
                        alforria_server::bearer::token_digest("static-admin"),
                        {
                            "sha256": alforria_server::bearer::token_digest("static-sidecar"),
                            "scope": "connector",
                            "name": "sidecar"
                        }
                    ]
                } },
                "directories": [dirs.repo.display().to_string()]
            }
        })
    }
}

fn fixture(signer: &Signer) -> Fixture {
    fixture_with(host_config(signer), Some(PASSWORD), Vec::new())
}

fn basic() -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("alforria:{PASSWORD}"));
    format!("Basic {encoded}")
}

fn dir_query(path: &std::path::Path) -> String {
    format!("directory={}", urlencode(&path.display().to_string()))
}

async fn status(f: &Fixture, method: &str, uri: &str, auth: &str) -> StatusCode {
    call(&f.router, method, uri, Some(auth), None)
        .await
        .status()
}

async fn create_session(f: &Fixture, directory: &std::path::Path, auth: &str) -> String {
    let response = call(
        &f.router,
        "POST",
        &format!("/session?{}", dir_query(directory)),
        Some(auth),
        Some(json!({})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_string()
}

/// A `chat` token talks to the agent in the configured project and nothing
/// else: no config, credentials, MCP management, terminals or instance
/// control, no v2 API, no web UI.
#[tokio::test]
async fn a_chat_token_reaches_chat_routes_only() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let dir = dir_query(&f.dirs.repo);
    let session = create_session(&f, &f.dirs.repo, &chat).await;

    for (method, uri) in [
        ("GET", format!("/session?{dir}")),
        ("GET", format!("/session/status?{dir}")),
        ("GET", format!("/session/{session}")),
        ("GET", format!("/session/{session}/message")),
        ("GET", format!("/session/{session}/children")),
        ("GET", format!("/session/{session}/todo")),
        ("POST", format!("/session/{session}/abort")),
        ("GET", format!("/permission?{dir}")),
        ("GET", format!("/question?{dir}")),
        ("GET", format!("/agent?{dir}")),
        ("GET", format!("/file?path=.&{dir}")),
        ("GET", "/global/health".to_string()),
    ] {
        assert_eq!(
            status(&f, method, &uri, &chat).await,
            StatusCode::OK,
            "{method} {uri}"
        );
    }

    for (method, uri) in [
        ("GET", format!("/config?{dir}")),
        ("PATCH", format!("/config?{dir}")),
        ("GET", "/global/config".to_string()),
        ("GET", format!("/config/providers?{dir}")),
        ("GET", format!("/provider?{dir}")),
        ("GET", format!("/provider/auth?{dir}")),
        ("PUT", "/auth/openai".to_string()),
        ("DELETE", "/auth/openai".to_string()),
        ("POST", format!("/provider/openai/oauth/authorize?{dir}")),
        ("GET", format!("/mcp?{dir}")),
        ("POST", format!("/mcp?{dir}")),
        ("POST", format!("/mcp/x/connect?{dir}")),
        ("GET", format!("/pty?{dir}")),
        ("POST", format!("/pty?{dir}")),
        ("GET", format!("/pty/pty_1/connect?{dir}")),
        ("POST", format!("/pty/pty_1/connect-token?{dir}")),
        ("POST", format!("/instance/dispose?{dir}")),
        ("POST", "/global/dispose".to_string()),
        ("GET", "/global/event".to_string()),
        ("POST", format!("/session/{session}/shell")),
        ("POST", format!("/session/{session}/share")),
        ("GET", format!("/experimental/session?{dir}")),
        ("POST", format!("/vcs/apply?{dir}")),
        ("GET", format!("/workflow?{dir}")),
        ("GET", "/api/session".to_string()),
        ("GET", "/api/health".to_string()),
        ("GET", "/".to_string()),
        ("GET", "/doc".to_string()),
        ("GET", "/no/such/route".to_string()),
    ] {
        let response = call(&f.router, method, &uri, Some(&chat), None).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        assert!(
            response.headers()["www-authenticate"]
                .to_str()
                .unwrap()
                .contains("insufficient_scope"),
            "{method} {uri}"
        );
        let body = body_json(response).await;
        assert_eq!(body["_tag"], "ForbiddenError", "{method} {uri}");
    }
}

/// Event streams take the bearer header like any other route: no
/// `?auth_token=` needed.
#[tokio::test]
async fn event_streams_accept_the_bearer_header() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let response = call(
        &f.router,
        "GET",
        &format!("/event?{}", dir_query(&f.dirs.repo)),
        Some(&chat),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));

    // Without the header the stream is closed to everyone.
    let anonymous = call(
        &f.router,
        "GET",
        &format!("/event?{}", dir_query(&f.dirs.repo)),
        None,
        None,
    )
    .await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
}

/// `connector` is `chat` plus what a sidecar watches: the global event
/// stream, workflow runs and status reads — still no config, credentials,
/// terminals, shell or workflow scripts.
#[tokio::test]
async fn a_connector_token_adds_automation_routes() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let connector = bearer(&signer.token(AUDIENCE, "connector"));
    let dir = dir_query(&f.dirs.repo);
    let session = create_session(&f, &f.dirs.repo, &connector).await;

    for (method, uri) in [
        ("GET", format!("/session/{session}/message")),
        ("GET", "/global/event".to_string()),
        ("GET", format!("/mcp?{dir}")),
        ("GET", format!("/workflow?{dir}")),
        ("GET", format!("/workflow/run?{dir}")),
    ] {
        assert_eq!(
            status(&f, method, &uri, &connector).await,
            StatusCode::OK,
            "{method} {uri}"
        );
    }
    for (method, uri) in [
        // Nothing below admin executes directly, a sidecar included.
        ("POST", format!("/session/{session}/shell")),
        ("POST", format!("/workflow?{dir}")),
        ("POST", format!("/workflow/run?{dir}")),
        ("GET", format!("/config?{dir}")),
        ("PUT", "/auth/openai".to_string()),
        ("POST", format!("/mcp?{dir}")),
        ("POST", format!("/pty?{dir}")),
        ("POST", format!("/instance/dispose?{dir}")),
        ("GET", "/api/session".to_string()),
    ] {
        assert_eq!(
            status(&f, method, &uri, &connector).await,
            StatusCode::FORBIDDEN,
            "{method} {uri}"
        );
    }
}

/// `admin` is everything, and Basic auth still is too.
#[tokio::test]
async fn admin_tokens_and_basic_auth_reach_everything() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let admin = bearer(&signer.token(AUDIENCE, "admin"));
    let dir = dir_query(&f.dirs.repo);
    for auth in [admin.as_str(), basic().as_str(), "Bearer static-admin"] {
        for uri in [
            format!("/config?{dir}"),
            format!("/pty?{dir}"),
            format!("/mcp?{dir}"),
            format!("/session?{dir}"),
            "/global/config".to_string(),
            "/api/health".to_string(),
            // Outside the allowlist: admin is not confined to it.
            format!("/session?{}", dir_query(&f.dirs.other)),
        ] {
            assert_eq!(status(&f, "GET", &uri, auth).await, StatusCode::OK, "{uri}");
        }
    }
}

/// Static tokens authenticate by digest and carry the scope configured
/// beside them.
#[tokio::test]
async fn static_tokens_carry_their_configured_scope() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let dir = dir_query(&f.dirs.repo);
    let sidecar = "Bearer static-sidecar";
    assert_eq!(
        status(&f, "GET", "/global/event", sidecar).await,
        StatusCode::OK
    );
    assert_eq!(
        status(&f, "GET", &format!("/config?{dir}"), sidecar).await,
        StatusCode::FORBIDDEN
    );
    // Case-insensitive scheme, as RFC 6750 clients send it.
    assert_eq!(
        status(&f, "GET", "/global/event", "bearer static-sidecar").await,
        StatusCode::OK
    );
    let unknown = call(
        &f.router,
        "GET",
        "/global/health",
        Some("Bearer nope"),
        None,
    )
    .await;
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);
    // The digest itself is not a credential.
    let digest = alforria_server::bearer::token_digest("static-admin");
    assert_eq!(
        status(&f, "GET", "/global/health", &bearer(&digest)).await,
        StatusCode::UNAUTHORIZED
    );
}

/// A token narrower than `admin` opens the configured directories and
/// nothing else, whichever way the directory is named.
#[tokio::test]
async fn narrow_tokens_are_confined_to_the_directory_allowlist() {
    let signer = Signer::new();
    let f = fixture(&signer);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let connector = bearer(&signer.token(AUDIENCE, "connector"));
    let other = f.dirs.other.display().to_string();
    let repo = f.dirs.repo.display().to_string();

    // ?directory= to somewhere else.
    let response = call(
        &f.router,
        "GET",
        &format!("/session?{}", dir_query(&f.dirs.other)),
        Some(&chat),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let message = body_json(response).await["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(message.contains("server.directories"), "{message}");

    // The header form.
    let request = axum::http::Request::builder()
        .uri("/session")
        .header("authorization", &chat)
        .header("x-opencode-directory", urlencode(&other))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = tower::ServiceExt::oneshot(f.router.clone(), request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // No directory at all: the server's working directory is not listed.
    assert_eq!(
        status(&f, "GET", "/session", &chat).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(&f, "GET", "/workflow", &connector).await,
        StatusCode::FORBIDDEN,
        "a route that falls back to the working directory on its own"
    );

    // Traversal out of the allowed directory.
    let traversal = format!("{repo}/../other");
    assert_eq!(
        status(
            &f,
            "GET",
            &format!("/session?directory={}", urlencode(&traversal)),
            &chat
        )
        .await,
        StatusCode::FORBIDDEN
    );

    // A symlink inside the allowed directory that leads out of it.
    #[cfg(unix)]
    {
        let link = f.dirs.repo.join("escape");
        std::os::unix::fs::symlink(&f.dirs.other, &link).unwrap();
        assert_eq!(
            status(&f, "GET", &format!("/session?{}", dir_query(&link)), &chat).await,
            StatusCode::FORBIDDEN
        );
    }

    // A workspace selection.
    assert_eq!(
        status(
            &f,
            "GET",
            &format!("/session?{}&workspace=wrk_1", dir_query(&f.dirs.repo)),
            &chat
        )
        .await,
        StatusCode::FORBIDDEN
    );

    // A session that lives elsewhere: its own directory routes the request.
    let foreign = create_session(&f, &f.dirs.other, &basic()).await;
    for uri in [
        format!("/session/{foreign}/message"),
        format!("/session/{foreign}/message?{}", dir_query(&f.dirs.repo)),
    ] {
        assert_eq!(
            status(&f, "GET", &uri, &chat).await,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
    let prompt = call(
        &f.router,
        "POST",
        &format!("/session/{foreign}/prompt_async"),
        Some(&chat),
        Some(json!({ "parts": [{ "type": "text", "text": "hi" }] })),
    )
    .await;
    assert_eq!(prompt.status(), StatusCode::FORBIDDEN);
    assert_eq!(f.llm.request_count(), 0, "the foreign session never ran");

    // The listed directory, under any spelling of it — and not a
    // directory below it: an instance opened there would take its config
    // from files inside the project.
    let nested = f.dirs.repo.join("packages/app");
    std::fs::create_dir_all(&nested).unwrap();
    for directory in [f.dirs.repo.clone(), f.dirs.repo.join("packages/..")] {
        assert_eq!(
            status(
                &f,
                "GET",
                &format!("/session?{}", dir_query(&directory)),
                &chat
            )
            .await,
            StatusCode::OK,
            "{}",
            directory.display()
        );
    }
    assert_eq!(
        status(
            &f,
            "GET",
            &format!("/session?{}", dir_query(&nested)),
            &chat
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(
            &f,
            "GET",
            &format!("/workflow?{}", dir_query(&f.dirs.repo)),
            &connector
        )
        .await,
        StatusCode::OK
    );
}

/// Every way a JWT is refused answers 401 with a Bearer challenge, and
/// never reaches a handler.
#[tokio::test]
async fn bad_tokens_are_refused_with_401() {
    let signer = Signer::new();
    let other_signer = Signer::new();
    let f = fixture(&signer);
    let now = support::now_secs();
    let claims = |patch: Value| {
        let mut claims = json!({ "aud": AUDIENCE, "scope": "admin", "exp": now + 300 });
        for (key, value) in patch.as_object().unwrap() {
            if value.is_null() {
                claims.as_object_mut().unwrap().remove(key);
            } else {
                claims[key] = value.clone();
            }
        }
        claims
    };
    let payload = support::b64url(claims(json!({})).to_string().as_bytes());
    let hs256 = {
        let signed = format!(
            "{}.{payload}",
            support::b64url(br#"{"alg":"HS256","typ":"JWT"}"#)
        );
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &signer.public_key_bytes());
        let mac = ring::hmac::sign(&key, signed.as_bytes());
        format!("{signed}.{}", support::b64url(mac.as_ref()))
    };
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "expired",
            signer.sign(claims(json!({ "exp": now - 3600 }))),
            "expired",
        ),
        (
            "not yet valid",
            signer.sign(claims(json!({ "nbf": now + 3600 }))),
            "not valid yet",
        ),
        (
            "no exp",
            signer.sign(claims(json!({ "exp": null }))),
            "no exp",
        ),
        (
            "wrong audience",
            signer.sign(claims(json!({ "aud": "agent-43" }))),
            "different audience",
        ),
        (
            "no audience",
            signer.sign(claims(json!({ "aud": null }))),
            "different audience",
        ),
        (
            "wrong key",
            other_signer.sign(claims(json!({}))),
            "matches no configured key",
        ),
        (
            "no known scope",
            signer.sign(claims(json!({ "scope": "read:all" }))),
            "no scope",
        ),
        (
            "alg none",
            format!("{}.{payload}.", support::b64url(br#"{"alg":"none"}"#)),
            "only EdDSA",
        ),
        ("alg HS256 keyed with the public key", hs256, "only EdDSA"),
        (
            "an EdDSA signature under an RS256 header",
            signer.sign_with_header(json!({ "alg": "RS256" }), claims(json!({}))),
            "only EdDSA",
        ),
        ("garbage", "not.a.token".to_string(), "malformed"),
        (
            "unknown opaque token",
            "opaque".to_string(),
            "not recognised",
        ),
    ];
    for (name, token, reason) in cases {
        let response = call(
            &f.router,
            "GET",
            "/global/health",
            Some(&bearer(&token)),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{name}");
        let challenge = response.headers()["www-authenticate"]
            .to_str()
            .unwrap()
            .to_string();
        assert!(challenge.starts_with("Bearer "), "{name}: {challenge}");
        assert!(challenge.contains("invalid_token"), "{name}: {challenge}");
        let body = body_string(response).await;
        assert!(body.contains(reason), "{name}: {body}");
    }
    // A good token still works on the same server.
    assert_eq!(
        status(
            &f,
            "GET",
            "/global/health",
            &bearer(&signer.token(AUDIENCE, "chat"))
        )
        .await,
        StatusCode::OK
    );
}

/// A bearer credential never falls back to Basic, and a server without
/// bearer authentication refuses the scheme outright.
#[tokio::test]
async fn bearer_is_refused_where_it_is_not_configured() {
    let f = fixture_with(|_| json!({}), Some(PASSWORD), Vec::new());
    let response = call(
        &f.router,
        "GET",
        "/global/health",
        Some("Bearer x.y.z"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(body_string(response).await.contains("not configured"));
    // Basic is untouched.
    assert_eq!(
        status(&f, "GET", "/global/health", &basic()).await,
        StatusCode::OK
    );
}

/// With bearer authentication and no password the server is closed, not
/// open: nothing gets in without a token, and Basic has nothing to match.
#[tokio::test]
async fn a_bearer_only_server_is_closed_without_a_token() {
    let signer = Signer::new();
    for password in [None, Some("")] {
        let f = fixture_with(host_config(&signer), password, Vec::new());
        let anonymous = call(&f.router, "GET", "/global/health", None, None).await;
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            anonymous.headers()["www-authenticate"],
            "Bearer realm=\"alforria\""
        );
        let empty_basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("alforria:")
        );
        assert_eq!(
            status(&f, "GET", "/global/health", &empty_basic).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(
                &f,
                "GET",
                "/global/health",
                &bearer(&signer.token(AUDIENCE, "chat"))
            )
            .await,
            StatusCode::OK
        );
        // The public UI shell stays public, as it is under Basic.
        assert_ne!(
            call(&f.router, "GET", "/openapi.json", None, None)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
}

// ---------------------------------------------------------------------
// A token below `admin` never executes anything except through the model
// under the operator's permission policy, and never changes that policy.
// ---------------------------------------------------------------------

fn policy_config(signer: &Signer, reply_always: bool) -> impl FnOnce(&Dirs) -> Value + '_ {
    move |dirs| {
        let repo = dirs.repo.display().to_string();
        json!({
            "server": {
                "auth": { "bearer": {
                    "keys": [signer.public_key()],
                    "audience": AUDIENCE,
                    "reply_always": reply_always,
                } },
                "directories": [repo]
            },
            // The operator's policy: shell is asked about, except one command.
            "permission": { "bash": { "*": "ask", "touch *allowed-marker": "allow" } },
            "agent": {
                "ghost": { "mode": "primary", "hidden": true, "description": "hidden", "permission": { "bash": "allow" } },
                "helper": { "mode": "subagent", "description": "a subagent", "permission": { "bash": "allow" } }
            },
            "command": {
                "as-helper": { "template": "Do: $ARGUMENTS", "agent": "helper" },
                "as-ghost": { "template": "Do: $ARGUMENTS", "agent": "ghost" },
                "with-notes": { "template": format!("Read @{repo}/notes.txt then: $ARGUMENTS") },
                "echo": { "template": "Summarise: $ARGUMENTS" },
                "status": { "template": format!("State: !`touch {repo}/ask-marker`") },
                "mark": { "template": format!("State: !`touch {repo}/allowed-marker`") },
                "greet": { "template": format!("Hello !`echo $1 > {repo}/greeted; echo hi`") }
            }
        })
    }
}

async fn run_command(
    f: &Fixture,
    session: &str,
    auth: &str,
    command: &str,
    arguments: &str,
) -> StatusCode {
    call(
        &f.router,
        "POST",
        &format!("/session/{session}/command"),
        Some(auth),
        Some(json!({ "command": command, "arguments": arguments })),
    )
    .await
    .status()
}

/// Arguments to a custom command are data, for every caller: inline shell
/// in them never runs, and they cannot break out of the template's own
/// inline shell.
#[tokio::test]
async fn command_arguments_are_never_executed() {
    let signer = Signer::new();
    let f = fixture_with(policy_config(&signer, false), Some(PASSWORD), Vec::new());
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let pwned = f.dirs.repo.join("pwned");
    let injection = format!("!`touch {}`", pwned.display());
    for auth in [chat.as_str(), basic().as_str()] {
        let session = create_session(&f, &f.dirs.repo, auth).await;
        assert_eq!(
            run_command(&f, &session, auth, "echo", &injection).await,
            StatusCode::OK
        );
        assert!(!pwned.exists(), "an argument ran as shell");
    }
    // Through the operator's own inline shell: the argument is a parameter.
    let session = create_session(&f, &f.dirs.repo, &basic()).await;
    let breakout = format!("x; touch {}", pwned.display());
    assert_eq!(
        run_command(&f, &session, &basic(), "greet", &breakout).await,
        StatusCode::OK
    );
    assert!(
        !pwned.exists(),
        "an argument broke out of the template's shell"
    );
    assert!(
        f.dirs.repo.join("greeted").exists(),
        "the template's shell ran"
    );
    // The model was handed the injection as text.
    let seen = f.llm.requests.lock().unwrap();
    let text = format!("{:?}", seen.iter().map(|r| &r.messages).collect::<Vec<_>>());
    assert!(
        text.contains("touch"),
        "the argument reached the model as text"
    );
}

fn user_texts(messages: &Value) -> String {
    messages.to_string()
}

async fn messages(f: &Fixture, session: &str, auth: &str) -> Value {
    body_json(
        call(
            &f.router,
            "GET",
            &format!("/session/{session}/message"),
            Some(auth),
            None,
        )
        .await,
    )
    .await
}

/// Below `admin`, a command whose template has inline shell is refused
/// whole, whatever the bash permission says: nothing is checked against a
/// string the shell would not see. The operator runs it as before.
#[tokio::test]
async fn commands_with_inline_shell_need_admin() {
    let signer = Signer::new();
    let f = fixture_with(policy_config(&signer, false), Some(PASSWORD), Vec::new());
    for scope in ["chat", "connector"] {
        let token = bearer(&signer.token(AUDIENCE, scope));
        let session = create_session(&f, &f.dirs.repo, &token).await;
        // `status` is asked about by the policy, `mark` is allowed by it,
        // `greet` takes an argument: all three are refused.
        for (command, arguments) in [("status", ""), ("mark", ""), ("greet", "x")] {
            let response = call(
                &f.router,
                "POST",
                &format!("/session/{session}/command"),
                Some(&token),
                Some(json!({ "command": command, "arguments": arguments })),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{scope} /{command}"
            );
            let body = body_string(response).await;
            assert!(body.contains(&format!("/{command}")), "{body}");
        }
    }
    for marker in ["ask-marker", "allowed-marker", "greeted"] {
        assert!(
            !f.dirs.repo.join(marker).exists(),
            "{marker}: inline shell ran"
        );
    }
    assert_eq!(f.llm.request_count(), 0);

    // The operator, and an admin token, as before.
    let admin = bearer(&signer.token(AUDIENCE, "admin"));
    let session = create_session(&f, &f.dirs.repo, &admin).await;
    assert_eq!(
        run_command(&f, &session, &admin, "status", "").await,
        StatusCode::OK
    );
    assert!(f.dirs.repo.join("ask-marker").exists());
    assert_eq!(
        run_command(&f, &session, &basic(), "mark", "").await,
        StatusCode::OK
    );
    assert!(f.dirs.repo.join("allowed-marker").exists());
}

/// Below `admin`, a command's arguments are text and nothing else: no
/// inline shell, no `@file` or `@agent` mention, however they are quoted.
/// The operator's own template text still resolves its mentions.
#[tokio::test]
async fn command_arguments_from_narrow_tokens_are_inert() {
    let signer = Signer::new();
    let f = fixture_with(policy_config(&signer, false), Some(PASSWORD), Vec::new());
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    std::fs::write(f.dirs.other.join("secret.txt"), "s3cret-content").unwrap();
    std::fs::write(f.dirs.repo.join("notes.txt"), "project-notes-content").unwrap();
    std::fs::write(f.dirs.repo.join("inside.txt"), "inside-content").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        f.dirs.other.join("secret.txt"),
        f.dirs.repo.join("link.txt"),
    )
    .unwrap();
    let secret = f.dirs.other.join("secret.txt").display().to_string();
    let pwned = f.dirs.repo.join("pwned");
    let touch = format!("touch {}", pwned.display());
    let session = create_session(&f, &f.dirs.repo, &chat).await;

    let arguments = [
        format!("!`{touch}`"),
        format!("$({touch})"),
        format!("`{touch}`"),
        format!("x; {touch}"),
        format!("@{secret}"),
        "@/etc/passwd".to_string(),
        "@../other/secret.txt".to_string(),
        "@../../outside".to_string(),
        "\"@/etc/passwd\"".to_string(),
        format!("\"@{secret}\""),
        format!("'@{secret}'"),
        "'@~/.ssh/id_ed25519'".to_string(),
        "@~/.bashrc".to_string(),
        "@link.txt".to_string(),
        "@inside.txt".to_string(),
        format!("@{}", f.dirs.repo.join("inside.txt").display()),
        "@helper".to_string(),
        "@ghost do it".to_string(),
    ];
    for argument in &arguments {
        assert_eq!(
            run_command(&f, &session, &chat, "echo", argument).await,
            StatusCode::OK,
            "{argument}"
        );
    }
    assert!(!pwned.exists(), "an argument ran as shell");
    let transcript = messages(&f, &session, &chat).await;
    let text = user_texts(&transcript);
    assert!(!text.contains("s3cret-content"), "an outside file was read");
    assert!(!text.contains("root:"), "/etc/passwd was read");
    assert!(
        !text.contains("inside-content"),
        "even an inside file is not attached from an argument"
    );
    let parts: Vec<&Value> = transcript
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap())
        .collect();
    assert!(
        parts
            .iter()
            .all(|part| part["type"] != "file" && part["type"] != "agent"),
        "an argument became a file or agent part"
    );
    // The argument text itself reached the model, verbatim.
    assert!(text.contains("@/etc/passwd"));

    // The template's own mention is the operator's and still resolves.
    assert_eq!(
        run_command(&f, &session, &chat, "with-notes", &format!("@{secret}")).await,
        StatusCode::OK
    );
    let text = user_texts(&messages(&f, &session, &chat).await);
    assert!(
        text.contains("project-notes-content"),
        "the template's own mention was not resolved"
    );
    assert!(!text.contains("s3cret-content"));

    // The operator's arguments keep upstream behaviour: mentions resolve.
    let session = create_session(&f, &f.dirs.repo, &basic()).await;
    let inside = format!("@{}", f.dirs.repo.join("inside.txt").display());
    assert_eq!(
        run_command(&f, &session, &basic(), "echo", &inside).await,
        StatusCode::OK
    );
    let text = user_texts(&messages(&f, &session, &basic()).await);
    assert!(
        text.contains("inside-content"),
        "the operator's mention was not resolved"
    );
}

/// File parts from a narrow token are confined where the file is read:
/// inline data and project files only — no absolute path or `file://` URL
/// out of the project, no symlink out of it, no MCP resource, and no URL
/// for the server or the provider to fetch.
#[tokio::test]
async fn file_parts_from_narrow_tokens_are_confined_at_the_read() {
    let signer = Signer::new();
    let f = fixture_with(policy_config(&signer, false), Some(PASSWORD), Vec::new());
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let dir = dir_query(&f.dirs.repo);
    std::fs::write(f.dirs.other.join("secret.txt"), "s3cret-content").unwrap();
    std::fs::write(f.dirs.repo.join("notes.txt"), "project-notes-content").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            f.dirs.other.join("secret.txt"),
            f.dirs.repo.join("link.txt"),
        )
        .unwrap();
        std::os::unix::fs::symlink(&f.dirs.other, f.dirs.repo.join("linkdir")).unwrap();
    }
    let secret = f.dirs.other.join("secret.txt").display().to_string();
    let repo = f.dirs.repo.display().to_string();
    let session = create_session(&f, &f.dirs.repo, &chat).await;
    let with_file = |url: String, extra: Value| {
        let mut part = json!({ "type": "file", "mime": "text/plain", "url": url });
        for (key, value) in extra.as_object().unwrap() {
            part[key] = value.clone();
        }
        json!({ "noReply": true, "parts": [{ "type": "text", "text": "see attached" }, part] })
    };

    let mut refused = vec![
        (format!("file://{secret}"), json!({})),
        ("file:///etc/passwd".to_string(), json!({})),
        (format!("file://{repo}/../other/secret.txt"), json!({})),
        (format!("FILE://{secret}"), json!({})),
        (
            "http://169.254.169.254/latest/meta-data/".to_string(),
            json!({}),
        ),
        ("https://example.com/x.txt".to_string(), json!({})),
        ("ftp://example.com/x.txt".to_string(), json!({})),
        (secret.clone(), json!({})),
        (
            "file:///ignored".to_string(),
            json!({ "source": { "type": "resource", "clientName": "srv", "uri": "res://x",
                               "text": { "value": "", "start": 0, "end": 0 } } }),
        ),
    ];
    if cfg!(unix) {
        refused.push((format!("file://{repo}/link.txt"), json!({})));
        refused.push((format!("file://{repo}/linkdir/secret.txt"), json!({})));
    }
    for (url, extra) in &refused {
        let response = call(
            &f.router,
            "POST",
            &format!("/session/{session}/message"),
            Some(&chat),
            Some(with_file(url.clone(), extra.clone())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{url}");
        // The asynchronous route answers before the engine looks; the
        // refusal arrives as a session error and nothing is stored.
        let response = call(
            &f.router,
            "POST",
            &format!("/session/{session}/prompt_async"),
            Some(&chat),
            Some(with_file(url.clone(), extra.clone())),
        )
        .await;
        assert!(response.status().is_success());
    }
    // A command's file parts take the same path.
    let response = call(
        &f.router,
        "POST",
        &format!("/session/{session}/command"),
        Some(&chat),
        Some(json!({
            "command": "echo", "arguments": "x",
            "parts": [{ "mime": "text/plain", "url": format!("file://{secret}") }]
        })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let transcript = messages(&f, &session, &chat).await;
    assert_eq!(
        transcript,
        json!([]),
        "a refused prompt left something behind"
    );

    // Inline data and a project file are fine.
    for url in [
        "data:text/plain;base64,aGVsbG8=".to_string(),
        format!("file://{repo}/notes.txt"),
    ] {
        let response = call(
            &f.router,
            "POST",
            &format!("/session/{session}/message"),
            Some(&chat),
            Some(with_file(url.clone(), json!({}))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{url}");
    }
    let text = user_texts(&messages(&f, &session, &chat).await);
    assert!(text.contains("project-notes-content"));
    assert!(!text.contains("s3cret-content"));

    // Listing a directory outside the project.
    assert_eq!(
        status(&f, "GET", &format!("/file?path=..%2Fother&{dir}"), &chat).await,
        StatusCode::FORBIDDEN
    );
    // The operator is not confined.
    let session = create_session(&f, &f.dirs.repo, &basic()).await;
    let response = call(
        &f.router,
        "POST",
        &format!("/session/{session}/message"),
        Some(&basic()),
        Some(with_file(format!("file://{secret}"), json!({}))),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

/// Every prompt-like entry point, against every way an agent can come to
/// run a turn. The engine judges the agent it resolved, so a vector the
/// routes never look at is still refused — and a new entry point or vector
/// belongs in these two lists.
#[tokio::test]
async fn no_entry_point_runs_an_agent_a_narrow_token_could_not_pick() {
    let signer = Signer::new();
    let f = fixture_with(policy_config(&signer, false), Some(PASSWORD), Vec::new());
    let dir = dir_query(&f.dirs.repo);
    let text = json!({ "type": "text", "text": "hi" });

    // (name, body builder per entry point). `None`: the vector does not
    // exist on that entry point.
    type Build = fn(&str, &Value) -> Option<Value>;
    let entry_points: [(&str, Build); 3] = [
        ("message", |vector, text| match vector {
            "field:hidden" => Some(json!({ "agent": "ghost", "parts": [text] })),
            "field:subagent" => Some(json!({ "agent": "helper", "parts": [text] })),
            "field:builtin-hidden" => Some(json!({ "agent": "title", "parts": [text] })),
            "field:builtin-subagent" => Some(json!({ "agent": "general", "parts": [text] })),
            "part:agent" => Some(json!({ "parts": [text, { "type": "agent", "name": "helper" }] })),
            "part:agent-visible" => {
                Some(json!({ "parts": [text, { "type": "agent", "name": "build" }] }))
            }
            "part:subtask" => Some(json!({ "parts": [{
                "type": "subtask", "prompt": "p", "description": "d", "agent": "helper" }] })),
            "omitted" => Some(json!({ "parts": [text] })),
            _ => None,
        }),
        ("prompt_async", |vector, text| match vector {
            "field:hidden" => Some(json!({ "agent": "ghost", "parts": [text] })),
            "field:subagent" => Some(json!({ "agent": "helper", "parts": [text] })),
            "field:builtin-hidden" => Some(json!({ "agent": "compaction", "parts": [text] })),
            "field:builtin-subagent" => Some(json!({ "agent": "explore", "parts": [text] })),
            "part:agent" => Some(json!({ "parts": [text, { "type": "agent", "name": "helper" }] })),
            "part:agent-visible" => {
                Some(json!({ "parts": [text, { "type": "agent", "name": "build" }] }))
            }
            "part:subtask" => Some(json!({ "parts": [{
                "type": "subtask", "prompt": "p", "description": "d", "agent": "helper" }] })),
            "omitted" => Some(json!({ "parts": [text] })),
            _ => None,
        }),
        ("command", |vector, _| match vector {
            "field:hidden" => {
                Some(json!({ "command": "echo", "arguments": "x", "agent": "ghost" }))
            }
            "field:subagent" => {
                Some(json!({ "command": "echo", "arguments": "x", "agent": "helper" }))
            }
            "field:builtin-hidden" => {
                Some(json!({ "command": "echo", "arguments": "x", "agent": "summary" }))
            }
            "field:builtin-subagent" => {
                Some(json!({ "command": "echo", "arguments": "x", "agent": "general" }))
            }
            "command:configured-subagent" => {
                Some(json!({ "command": "as-helper", "arguments": "x" }))
            }
            "command:configured-hidden" => Some(json!({ "command": "as-ghost", "arguments": "x" })),
            "argument:agent-mention" => {
                Some(json!({ "command": "echo", "arguments": "@helper go" }))
            }
            "omitted" => Some(json!({ "command": "echo", "arguments": "x" })),
            _ => None,
        }),
    ];
    // (vector, session kind, must the turn be refused?)
    let vectors = [
        ("field:hidden", "plain", true),
        ("field:subagent", "plain", true),
        ("field:builtin-hidden", "plain", true),
        ("field:builtin-subagent", "plain", true),
        ("part:agent", "plain", true),
        ("part:agent-visible", "plain", true),
        ("part:subtask", "plain", true),
        ("command:configured-subagent", "plain", true),
        ("command:configured-hidden", "plain", true),
        // An `@agent` in a command's arguments is text: the turn runs, as
        // the default agent, with no agent part.
        ("argument:agent-mention", "plain", false),
        // No agent named: the session's own stored agent, or a child's.
        ("omitted", "child-of-subagent", true),
        ("omitted", "stored-hidden-agent", false),
        ("omitted", "plain", false),
        // The "session created by" axis: the same requests against a
        // session the operator made are refused, every one — whatever a
        // narrow token started there would go on with the operator's
        // rights.
        ("field:hidden", "operators", true),
        ("field:subagent", "operators", true),
        ("part:agent", "operators", true),
        ("part:subtask", "operators", true),
        ("command:configured-subagent", "operators", true),
        ("argument:agent-mention", "operators", true),
        ("omitted", "operators", true),
    ];

    for scope in ["chat", "connector"] {
        let token = bearer(&signer.token(AUDIENCE, scope));
        for (route, build) in &entry_points {
            for (vector, kind, refused) in &vectors {
                let Some(body) = build(vector, &text) else {
                    continue;
                };
                // A fresh session per case, set up by the operator where
                // the token could not have made it itself.
                let session = match *kind {
                    "plain" => create_session(&f, &f.dirs.repo, &token).await,
                    "stored-hidden-agent" => {
                        let response = call(
                            &f.router,
                            "POST",
                            &format!("/session?{dir}"),
                            Some(&basic()),
                            Some(json!({ "agent": "ghost" })),
                        )
                        .await;
                        let id = body_json(response).await["id"]
                            .as_str()
                            .unwrap()
                            .to_string();
                        // A restricted session holding an agent its token
                        // could not have stored.
                        f.ctx.sessions.restrict(&id).unwrap();
                        id
                    }
                    "operators" => create_session(&f, &f.dirs.repo, &basic()).await,
                    "child-of-subagent" => {
                        let parent = create_session(&f, &f.dirs.repo, &token).await;
                        let response = call(
                            &f.router,
                            "POST",
                            &format!("/session?{dir}"),
                            Some(&basic()),
                            Some(json!({ "parentID": parent, "agent": "helper" })),
                        )
                        .await;
                        body_json(response).await["id"]
                            .as_str()
                            .unwrap()
                            .to_string()
                    }
                    other => unreachable!("{other}"),
                };
                let before = f.llm.request_count();
                let response = call(
                    &f.router,
                    "POST",
                    &format!("/session/{session}/{route}"),
                    Some(&token),
                    Some(body),
                )
                .await;
                let case = format!("{scope} {route} {vector} {kind}");
                if *route == "prompt_async" && *kind != "operators" {
                    // Answers 204 before the engine decides; wait for the
                    // turn (or its refusal) to settle.
                    assert!(response.status().is_success(), "{case}");
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                } else if *refused {
                    assert_eq!(response.status(), StatusCode::FORBIDDEN, "{case}");
                } else {
                    assert_eq!(response.status(), StatusCode::OK, "{case}");
                }
                let ran: Vec<String> = f.llm.requests.lock().unwrap()[before..]
                    .iter()
                    .map(|request| request.agent.name.clone())
                    .collect();
                if *refused {
                    assert!(ran.is_empty(), "{case}: a refused turn ran as {ran:?}");
                    let stored = messages(&f, &session, &basic()).await;
                    assert_eq!(stored, json!([]), "{case}: a refused turn was stored");
                } else {
                    // Whatever ran, ran as an agent the token could pick:
                    // an omitted agent means the default, never the
                    // session's stored one.
                    assert!(!ran.is_empty(), "{case}: nothing ran");
                    for agent in &ran {
                        assert_eq!(agent, "build", "{case}");
                    }
                    let stored = messages(&f, &session, &basic()).await.to_string();
                    assert!(
                        !stored.contains("\"type\":\"agent\""),
                        "{case}: an agent part was stored"
                    );
                }
            }
        }
    }
}

/// A narrow token cannot hand a session permission rules or switch tools
/// on.
#[tokio::test]
async fn narrow_tokens_cannot_change_the_permission_policy() {
    let signer = Signer::new();
    let f = fixture_with(policy_config(&signer, false), Some(PASSWORD), Vec::new());
    let dir = dir_query(&f.dirs.repo);
    let allow_all = json!([{ "permission": "*", "pattern": "*", "action": "allow" }]);
    let prompt = |extra: Value| {
        let mut body = json!({ "noReply": true, "parts": [{ "type": "text", "text": "hi" }] });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        body
    };

    for scope in ["chat", "connector"] {
        let token = bearer(&signer.token(AUDIENCE, scope));
        let session = create_session(&f, &f.dirs.repo, &token).await;
        let post = |uri: String, body: Value| {
            let (router, token) = (f.router.clone(), token.clone());
            async move { call(&router, "POST", &uri, Some(&token), Some(body)).await }
        };

        // Rulesets, on create and on update.
        let created = post(
            format!("/session?{dir}"),
            json!({ "permission": allow_all }),
        )
        .await;
        assert_eq!(created.status(), StatusCode::FORBIDDEN, "{scope}: create");
        let updated = call(
            &f.router,
            "PATCH",
            &format!("/session/{session}"),
            Some(&token),
            Some(json!({ "permission": allow_all })),
        )
        .await;
        assert_eq!(updated.status(), StatusCode::FORBIDDEN, "{scope}: update");
        // Renaming is still theirs to do.
        let renamed = call(
            &f.router,
            "PATCH",
            &format!("/session/{session}"),
            Some(&token),
            Some(json!({ "title": "renamed" })),
        )
        .await;
        assert_eq!(renamed.status(), StatusCode::OK, "{scope}: rename");

        // The tools map: off is fine, on is not — on either prompt route.
        let message = format!("/session/{session}/message");
        let on = post(
            message.clone(),
            prompt(json!({ "tools": { "bash": true } })),
        )
        .await;
        assert_eq!(on.status(), StatusCode::FORBIDDEN, "{scope}: tools on");
        let on_async = post(
            format!("/session/{session}/prompt_async"),
            prompt(json!({ "tools": { "bash": true, "edit": false } })),
        )
        .await;
        assert!(on_async.status().is_success());
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let info = body_json(
            call(
                &f.router,
                "GET",
                &format!("/session/{session}"),
                Some(&token),
                None,
            )
            .await,
        )
        .await;
        assert!(
            info["permission"].is_null() || info["permission"] == json!([]),
            "{scope}: the session gained rules: {info}"
        );
        let off = post(
            message.clone(),
            prompt(json!({ "tools": { "webfetch": false } })),
        )
        .await;
        assert_eq!(off.status(), StatusCode::OK, "{scope}: tools off");
        let info = body_json(
            call(
                &f.router,
                "GET",
                &format!("/session/{session}"),
                Some(&token),
                None,
            )
            .await,
        )
        .await;
        assert_eq!(
            info["permission"],
            json!([{ "permission": "webfetch", "pattern": "*", "action": "deny" }]),
            "{scope}: only the deny rule was written"
        );
    }
    assert_eq!(
        f.llm.request_count(),
        0,
        "nothing refused reached the model"
    );

    // The operator may do all of it.
    let session = create_session(&f, &f.dirs.repo, &basic()).await;
    let updated = call(
        &f.router,
        "PATCH",
        &format!("/session/{session}"),
        Some(&basic()),
        Some(json!({ "permission": allow_all })),
    )
    .await;
    assert_eq!(updated.status(), StatusCode::OK);
    let tools = call(
        &f.router,
        "POST",
        &format!("/session/{session}/message"),
        Some(&basic()),
        Some(prompt(
            json!({ "tools": { "bash": true }, "agent": "general" }),
        )),
    )
    .await;
    assert_eq!(tools.status(), StatusCode::OK);
}

/// Message and part ids are upserts. A narrow token does not get to choose
/// them, so it cannot name an existing message or part and replace it.
#[tokio::test]
async fn narrow_tokens_cannot_overwrite_history_with_chosen_ids() {
    let signer = Signer::new();
    let f = fixture_with(policy_config(&signer, false), Some(PASSWORD), Vec::new());
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let session = create_session(&f, &f.dirs.repo, &chat).await;
    let say = |body: Value| {
        let (router, chat, session) = (f.router.clone(), chat.clone(), session.clone());
        async move {
            let response = call(
                &router,
                "POST",
                &format!("/session/{session}/message"),
                Some(&chat),
                Some(body),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }
    };
    say(json!({ "parts": [{ "type": "text", "text": "the original question" }] })).await;
    let before = messages(&f, &session, &chat).await;
    let user_id = before[0]["info"]["id"].as_str().unwrap().to_string();
    let user_part = before[0]["parts"][0]["id"].as_str().unwrap().to_string();
    let answer_part = before[1]["parts"][0]["id"].as_str().unwrap().to_string();

    // Aim a new prompt at the existing message, and its parts at existing
    // parts — the user's own and the assistant's.
    say(json!({
        "messageID": user_id,
        "noReply": true,
        "parts": [
            { "type": "text", "id": user_part, "text": "rewritten question" },
            { "type": "text", "id": answer_part, "text": "rewritten answer" }
        ]
    }))
    .await;
    let after = messages(&f, &session, &chat).await;
    assert_eq!(after[0], before[0], "the first message was replaced");
    assert_eq!(after[1], before[1], "the answer was replaced");
    assert_eq!(
        after.as_array().unwrap().len(),
        3,
        "a new message was appended"
    );
    assert_ne!(after[2]["info"]["id"], json!(user_id));
}

async fn restricted(f: &Fixture, session: &str) -> bool {
    let response = call(
        &f.router,
        "GET",
        &format!("/session/{session}/restriction"),
        Some(&basic()),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response).await["restricted"].as_bool().unwrap()
}

async fn wait_idle(f: &Fixture, session: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut quiet = 0;
    loop {
        let status = body_json(
            call(
                &f.router,
                "GET",
                &format!("/session/status?{}", dir_query(&f.dirs.repo)),
                Some(&basic()),
                None,
            )
            .await,
        )
        .await;
        if status.get(session).is_none() || status[session]["type"] == "idle" {
            quiet += 1;
            if quiet >= 10 {
                return;
            }
        } else {
            quiet = 0;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the session never settled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
}

fn agents_run(f: &Fixture, from: usize) -> Vec<String> {
    f.llm.requests.lock().unwrap()[from..]
        .iter()
        .map(|request| request.agent.name.clone())
        .collect()
}

/// A session a narrow token creates is restricted for life, and so is
/// everything descended from it — whoever prompts it later.
#[tokio::test]
async fn a_restricted_session_stays_restricted_for_every_caller() {
    let signer = Signer::new();
    let f = fixture_with(policy_config(&signer, false), Some(PASSWORD), Vec::new());
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let dir = dir_query(&f.dirs.repo);
    let post = |uri: String, auth: String, body: Value| {
        let router = f.router.clone();
        async move { call(&router, "POST", &uri, Some(&auth), Some(body)).await }
    };

    let session = create_session(&f, &f.dirs.repo, &chat).await;
    assert!(restricted(&f, &session).await);
    let operators = create_session(&f, &f.dirs.repo, &basic()).await;
    assert!(!restricted(&f, &operators).await);

    // Edge validation: an agent that could never run is refused at create.
    for agent in ["ghost", "helper", "general", "title"] {
        let response = post(
            format!("/session?{dir}"),
            chat.clone(),
            json!({ "agent": agent }),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "create as {agent}"
        );
    }
    // An update has no way to switch the agent: the field is not one.
    let patched = call(
        &f.router,
        "PATCH",
        &format!("/session/{session}"),
        Some(&chat),
        Some(json!({ "agent": "ghost", "title": "t" })),
    )
    .await;
    assert_eq!(patched.status(), StatusCode::OK);
    assert_ne!(body_json(patched).await["agent"], "ghost");

    // Children and forks inherit it, including ones the operator makes.
    let child = post(
        format!("/session?{dir}"),
        basic(),
        json!({ "parentID": session }),
    )
    .await;
    let child = body_json(child).await["id"].as_str().unwrap().to_string();
    assert!(
        restricted(&f, &child).await,
        "a child of a restricted session"
    );
    let fork = post(format!("/session/{session}/fork"), basic(), json!({})).await;
    assert_eq!(fork.status(), StatusCode::OK);
    let fork = body_json(fork).await["id"].as_str().unwrap().to_string();
    assert!(
        restricted(&f, &fork).await,
        "a fork of a restricted session"
    );

    // The operator's own requests to it are bound as well...
    let message = format!("/session/{session}/message");
    let text = json!({ "type": "text", "text": "hi" });
    for (what, body) in [
        (
            "a hidden agent",
            json!({ "agent": "ghost", "parts": [text] }),
        ),
        (
            "tools switched on",
            json!({ "noReply": true, "tools": { "bash": true }, "parts": [text] }),
        ),
        (
            "a file outside the project",
            json!({ "noReply": true, "parts": [text, {
                "type": "file", "mime": "text/plain", "url": "file:///etc/hostname" }] }),
        ),
    ] {
        let response = post(message.clone(), basic(), body).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "operator: {what}");
    }
    assert_eq!(
        run_command(&f, &session, &basic(), "status", "").await,
        StatusCode::FORBIDDEN,
        "operator: a command with inline shell"
    );
    assert!(!f.dirs.repo.join("ask-marker").exists());
    assert_eq!(f.llm.request_count(), 0);

    // ...until an admin lifts the restriction, which a narrow token cannot.
    let uri = format!("/session/{session}/restriction");
    assert_eq!(status(&f, "GET", &uri, &chat).await, StatusCode::FORBIDDEN);
    assert_eq!(
        status(&f, "DELETE", &uri, &chat).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(status(&f, "DELETE", &uri, &basic()).await, StatusCode::OK);
    assert!(!restricted(&f, &session).await);
    let response = post(
        message.clone(),
        basic(),
        json!({ "agent": "ghost", "parts": [text] }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(agents_run(&f, 0), ["ghost"]);
    // The token that made the session is still what it is.
    let response = post(
        message,
        chat.clone(),
        json!({ "agent": "ghost", "parts": [text] }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// The turns the engine starts by itself are bound by the session: a goal a
/// narrow token starts runs every round, reviewer included, as an agent the
/// token could pick, and a stored agent it could not pick never runs.
#[tokio::test]
async fn engine_started_turns_are_bound_by_the_session() {
    let signer = Signer::new();
    let f = fixture_with(policy_config(&signer, false), Some(PASSWORD), Vec::new());
    let chat = bearer(&signer.token(AUDIENCE, "chat"));

    // A goal, started by the token, left to run its rounds: the reviewer
    // keeps saying "continue" until the goal stalls out.
    f.llm
        .set_toolless(r#"{"verdict":"continue","reason":"not done yet","next_step":"keep going"}"#);
    let session = create_session(&f, &f.dirs.repo, &chat).await;
    assert_eq!(
        run_command(&f, &session, &chat, "goal", "tidy the notes").await,
        StatusCode::OK
    );
    wait_idle(&f, &session).await;
    let ran = agents_run(&f, 0);
    assert!(ran.len() >= 3, "the goal ran its rounds: {ran:?}");
    assert!(ran.iter().all(|agent| agent == "build"), "{ran:?}");
    let goal = body_json(
        call(
            &f.router,
            "GET",
            &format!("/session/{session}/goal?{}", dir_query(&f.dirs.repo)),
            Some(&chat),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(goal["startedBy"], "restricted", "{goal}");
    assert!(goal["rounds"].as_u64().unwrap() >= 2, "{goal}");
    assert!(restricted(&f, &session).await);

    // A restricted session that somehow holds an agent the token could not
    // pick (here: made by the operator, then restricted). A goal on it
    // would run its rounds as the stored agent; no round does.
    let response = call(
        &f.router,
        "POST",
        &format!("/session?{}", dir_query(&f.dirs.repo)),
        Some(&basic()),
        Some(json!({ "agent": "ghost" })),
    )
    .await;
    let stored = body_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    f.ctx.sessions.restrict(&stored).unwrap();
    let before = f.llm.request_count();
    for auth in [chat.clone(), basic()] {
        let response = call(
            &f.router,
            "POST",
            &format!("/session/{stored}/command"),
            Some(&auth),
            Some(json!({ "command": "goal", "arguments": "anything", "agent": "ghost" })),
        )
        .await;
        assert_ne!(response.status(), StatusCode::OK);
    }
    wait_idle(&f, &stored).await;
    assert!(
        !agents_run(&f, before).contains(&"ghost".to_string()),
        "the hidden agent ran in a restricted session"
    );

    // The engine's own prompt into a restricted session, with no caller at
    // all (what a resume or a trigger is): bound by the session.
    let directory = f.dirs.repo.clone();
    let location = alforria_server::middleware::location::LocationContext {
        services: f.ctx.instances.load(&directory).unwrap(),
        directory,
        workspace_id: None,
    };
    let engine = (f.ctx.engine_factory)(&location).unwrap();
    let synthetic = |agent: Option<&str>, tools: Option<bool>| {
        use alforria_core::session::prompt_input::{PromptInput, PromptPartInput};
        PromptInput {
            session_id: stored.clone(),
            message_id: None,
            model: None,
            agent: agent.map(str::to_string),
            no_reply: Some(true),
            tools: tools.map(|on| [("bash".to_string(), on)].into_iter().collect()),
            format: None,
            system: None,
            variant: None,
            parts: vec![PromptPartInput::Text {
                id: None,
                text: "resume".to_string(),
                synthetic: Some(true),
                ignored: None,
                time: None,
                metadata: None,
            }],
            caller: alforria_core::session::caller::Caller::Session,
        }
    };
    use alforria_core::session::prompt_input::PromptError;
    assert!(matches!(
        engine.prompt(synthetic(Some("ghost"), None)).await,
        Err(PromptError::Forbidden { .. })
    ));
    assert!(matches!(
        engine.prompt(synthetic(None, Some(true))).await,
        Err(PromptError::Forbidden { .. })
    ));
    assert!(engine.prompt(synthetic(Some("build"), None)).await.is_ok());
    // The same prompt in an unrestricted session is the operator's engine.
    let free = create_session(&f, &f.dirs.repo, &basic()).await;
    let mut input = synthetic(Some("ghost"), Some(true));
    input.session_id = free;
    assert!(engine.prompt(input).await.is_ok());
}

/// A subagent the model starts with the task tool runs in a restricted
/// session: the engine's run of it is not a request, the child session
/// inherits the restriction, and nobody below `admin` can prompt the child.
#[tokio::test]
async fn subagents_started_by_the_model_run_and_inherit_the_restriction() {
    let signer = Signer::new();
    let task = support::tool_turn(
        "call_task",
        "task",
        json!({ "description": "help", "prompt": "do the thing", "subagent_type": "helper" }),
    );
    let f = fixture_with(policy_config(&signer, false), Some(PASSWORD), vec![task]);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let session = create_session(&f, &f.dirs.repo, &chat).await;
    let response = call(
        &f.router,
        "POST",
        &format!("/session/{session}/message"),
        Some(&chat),
        Some(json!({ "parts": [{ "type": "text", "text": "use the helper" }] })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    wait_idle(&f, &session).await;
    let ran = agents_run(&f, 0);
    assert!(
        ran.contains(&"helper".to_string()),
        "the subagent ran: {ran:?}"
    );

    let children = body_json(
        call(
            &f.router,
            "GET",
            &format!("/session/{session}/children"),
            Some(&chat),
            None,
        )
        .await,
    )
    .await;
    let child = children[0]["id"]
        .as_str()
        .expect("a child session")
        .to_string();
    assert!(
        restricted(&f, &child).await,
        "the subagent's session inherits"
    );
    let direct = call(
        &f.router,
        "POST",
        &format!("/session/{child}/message"),
        Some(&chat),
        Some(json!({ "parts": [{ "type": "text", "text": "hello helper" }] })),
    )
    .await;
    assert_eq!(direct.status(), StatusCode::FORBIDDEN);
}

async fn pending_permission(f: &Fixture, directory: &std::path::Path, auth: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let list = body_json(
            call(
                &f.router,
                "GET",
                &format!("/permission?{}", dir_query(directory)),
                Some(auth),
                None,
            )
            .await,
        )
        .await;
        if let Some(id) = list[0]["id"].as_str() {
            return id.to_string();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no permission ask appeared"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

async fn start_bash_ask(f: &Fixture, directory: &std::path::Path, auth: &str) -> String {
    let session = create_session(f, directory, auth).await;
    let response = call(
        &f.router,
        "POST",
        &format!("/session/{session}/prompt_async"),
        Some(auth),
        Some(json!({ "parts": [{ "type": "text", "text": "go" }] })),
    )
    .await;
    assert!(response.status().is_success(), "{}", response.status());
    pending_permission(f, directory, auth).await
}

async fn reply(f: &Fixture, id: &str, auth: &str, answer: &str) -> StatusCode {
    call(
        &f.router,
        "POST",
        &format!("/permission/{id}/reply?{}", dir_query(&f.dirs.repo)),
        Some(auth),
        Some(json!({ "reply": answer })),
    )
    .await
    .status()
}

fn bash_ask() -> support::Script {
    support::tool_turn(
        "call_1",
        "bash",
        json!({ "command": "echo hi", "description": "say hi" }),
    )
}

/// A narrow token answers an ask once or rejects it. `always` writes a
/// standing allow rule for the whole project, so it needs `admin` unless
/// the operator opts in.
#[tokio::test]
async fn narrow_tokens_answer_asks_but_cannot_make_standing_rules() {
    let signer = Signer::new();
    let f = fixture_with(
        policy_config(&signer, false),
        Some(PASSWORD),
        vec![bash_ask()],
    );
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let id = start_bash_ask(&f, &f.dirs.repo, &chat).await;

    assert_eq!(reply(&f, &id, &chat, "always").await, StatusCode::FORBIDDEN);
    assert_eq!(
        pending_permission(&f, &f.dirs.repo, &chat).await,
        id,
        "the refused reply left the ask pending"
    );
    assert_eq!(reply(&f, &id, &chat, "once").await, StatusCode::OK);

    // Opted in by the operator.
    let f = fixture_with(
        policy_config(&signer, true),
        Some(PASSWORD),
        vec![bash_ask()],
    );
    let id = start_bash_ask(&f, &f.dirs.repo, &chat).await;
    assert_eq!(reply(&f, &id, &chat, "always").await, StatusCode::OK);
}

/// Replies find their ask by id in any loaded project — but below `admin`
/// only in projects the token may open.
#[tokio::test]
async fn narrow_tokens_cannot_answer_another_projects_asks() {
    let signer = Signer::new();
    let f = fixture_with(
        policy_config(&signer, false),
        Some(PASSWORD),
        vec![bash_ask()],
    );
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    // An ask pending in a project outside the allowlist, started by the operator.
    let id = start_bash_ask(&f, &f.dirs.other, &basic()).await;
    // Load the allowed project too, so the reply has somewhere to start from.
    create_session(&f, &f.dirs.repo, &chat).await;

    assert_eq!(reply(&f, &id, &chat, "once").await, StatusCode::FORBIDDEN);
    assert_eq!(
        pending_permission(&f, &f.dirs.other, &basic()).await,
        id,
        "the ask is still waiting for its own project"
    );
    // The operator's reply does cross projects, as before.
    assert_eq!(reply(&f, &id, &basic(), "once").await, StatusCode::OK);
}

/// A narrow token acts only in restricted sessions. In an operator's
/// session anything it started — a goal above all — would go on running
/// with the operator's rights, so it starts nothing there: it may read the
/// session, and fork it into a restricted one of its own.
#[tokio::test]
async fn narrow_tokens_cannot_act_in_an_operators_session() {
    let signer = Signer::new();
    let f = fixture_with(
        policy_config(&signer, false),
        Some(PASSWORD),
        vec![bash_ask()],
    );
    f.llm
        .set_toolless(r#"{"verdict":"continue","reason":"not done","next_step":"go on"}"#);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let operators = create_session(&f, &f.dirs.repo, &basic()).await;
    let text = json!({ "parts": [{ "type": "text", "text": "hi" }] });

    // An ask waiting in the operator's session is not the token's to answer.
    let response = call(
        &f.router,
        "POST",
        &format!("/session/{operators}/prompt_async"),
        Some(&basic()),
        Some(text.clone()),
    )
    .await;
    assert!(response.status().is_success());
    let ask = pending_permission(&f, &f.dirs.repo, &basic()).await;
    assert_eq!(reply(&f, &ask, &chat, "once").await, StatusCode::FORBIDDEN);
    assert_eq!(
        call(
            &f.router,
            "POST",
            &format!("/session/{operators}/permissions/{ask}"),
            Some(&chat),
            Some(json!({ "response": "once" })),
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(pending_permission(&f, &f.dirs.repo, &basic()).await, ask);
    assert_eq!(reply(&f, &ask, &basic(), "reject").await, StatusCode::OK);
    wait_idle(&f, &operators).await;
    let baseline = f.llm.request_count();

    // Every way of acting in it is refused...
    for (method, path, body) in [
        ("POST", "message", text.clone()),
        ("POST", "prompt_async", text.clone()),
        (
            "POST",
            "command",
            json!({ "command": "goal", "arguments": "take over" }),
        ),
        (
            "POST",
            "command",
            json!({ "command": "echo", "arguments": "x" }),
        ),
        (
            "POST",
            "init",
            json!({ "providerID": "anthropic", "modelID": "claude", "messageID": "msg_1" }),
        ),
        (
            "POST",
            "summarize",
            json!({ "providerID": "anthropic", "modelID": "claude" }),
        ),
        ("POST", "revert", json!({ "messageID": "msg_1" })),
        ("POST", "unrevert", json!({})),
        ("POST", "abort", json!({})),
        ("PATCH", "", json!({ "title": "mine now" })),
        ("DELETE", "", json!({})),
    ] {
        let uri = if path.is_empty() {
            format!("/session/{operators}")
        } else {
            format!("/session/{operators}/{path}")
        };
        let response = call(&f.router, method, &uri, Some(&chat), Some(body)).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {uri}");
    }
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert_eq!(
        f.llm.request_count(),
        baseline,
        "nothing ran in the operator's session"
    );
    let goal_of = |session: String, auth: String| {
        let (router, query) = (f.router.clone(), dir_query(&f.dirs.repo));
        async move {
            body_json(
                call(
                    &router,
                    "GET",
                    &format!("/session/{session}/goal?{query}"),
                    Some(&auth),
                    None,
                )
                .await,
            )
            .await
        }
    };
    let goal = goal_of(operators.clone(), basic()).await;
    assert!(goal.is_null(), "no goal was armed: {goal}");
    assert!(!restricted(&f, &operators).await);

    // ...including through a goal the operator started: it cannot be
    // resumed, paused or cleared by the token.
    assert_eq!(
        run_command(&f, &operators, &basic(), "goal", "the operator's goal").await,
        StatusCode::OK
    );
    wait_idle(&f, &operators).await;
    let operator_rounds = f.llm.request_count();
    assert!(operator_rounds > baseline);
    for verb in ["resume", "pause", "clear"] {
        assert_eq!(
            run_command(&f, &operators, &chat, "goal", verb).await,
            StatusCode::FORBIDDEN,
            "goal {verb}"
        );
    }
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    assert_eq!(
        f.llm.request_count(),
        operator_rounds,
        "no round was triggered"
    );
    assert_eq!(
        goal_of(operators.clone(), basic()).await["startedBy"],
        "operator"
    );

    // Reading is allowed (one user per machine), and so is forking: the
    // fork is restricted, and what the engine runs in it is too.
    assert_eq!(
        status(&f, "GET", &format!("/session/{operators}/message"), &chat).await,
        StatusCode::OK
    );
    let fork = call(
        &f.router,
        "POST",
        &format!("/session/{operators}/fork"),
        Some(&chat),
        Some(json!({})),
    )
    .await;
    assert_eq!(fork.status(), StatusCode::OK);
    let fork = body_json(fork).await["id"].as_str().unwrap().to_string();
    assert!(restricted(&f, &fork).await, "a fork made by a narrow token");
    assert!(!restricted(&f, &operators).await, "the source is untouched");
    wait_idle(&f, &fork).await;
    let before = f.llm.request_count();
    assert_eq!(
        run_command(&f, &fork, &chat, "goal", "work in the fork").await,
        StatusCode::OK
    );
    wait_idle(&f, &fork).await;
    let ran = agents_run(&f, before);
    assert!(ran.len() >= 3, "the fork's goal ran its rounds: {ran:?}");
    assert!(ran.iter().all(|agent| agent == "build"), "{ran:?}");
    assert_eq!(
        goal_of(fork.clone(), chat.clone()).await["startedBy"],
        "restricted"
    );

    // An operator's prompt to the restricted fork is bound like anyone's,
    // and does not lift the restriction for what follows.
    let response = call(
        &f.router,
        "POST",
        &format!("/session/{fork}/message"),
        Some(&basic()),
        Some(json!({ "agent": "ghost", "parts": [{ "type": "text", "text": "hi" }] })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = call(
        &f.router,
        "POST",
        &format!("/session/{fork}/message"),
        Some(&basic()),
        Some(text),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(restricted(&f, &fork).await);
    assert_eq!(
        run_command(&f, &fork, &basic(), "status", "").await,
        StatusCode::FORBIDDEN,
        "still no inline shell in the restricted fork, for anyone"
    );
    // A goal the operator resumes there keeps running restricted.
    let before = f.llm.request_count();
    assert_eq!(
        run_command(&f, &fork, &basic(), "goal", "resume").await,
        StatusCode::OK
    );
    wait_idle(&f, &fork).await;
    let ran = agents_run(&f, before);
    assert!(ran.iter().all(|agent| agent == "build"), "{ran:?}");
    assert!(restricted(&f, &fork).await);
}

/// Routes that take two identifiers act on the inner one only when it
/// belongs to the outer one. For each: a valid session in the path, and a
/// message, part or pending ask that belongs to a *different* session —
/// the request is refused and the other session is untouched.
#[tokio::test]
async fn an_inner_id_from_another_session_is_never_acted_on() {
    let signer = Signer::new();
    let f = fixture_with(
        policy_config(&signer, false),
        Some(PASSWORD),
        vec![bash_ask()],
    );
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let say = |session: String, auth: String, text: &'static str| {
        let router = f.router.clone();
        async move {
            let response = call(
                &router,
                "POST",
                &format!("/session/{session}/message"),
                Some(&auth),
                Some(json!({ "noReply": true, "parts": [{ "type": "text", "text": text }] })),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }
    };

    // `mine`: a session the token may act in. `theirs`: the operator's,
    // with a message, and a turn waiting on a permission ask.
    let mine = create_session(&f, &f.dirs.repo, &chat).await;
    say(mine.clone(), chat.clone(), "mine-text").await;
    let theirs = create_session(&f, &f.dirs.repo, &basic()).await;
    say(theirs.clone(), basic(), "theirs-text").await;
    let their_messages = messages(&f, &theirs, &basic()).await;
    let their_message = their_messages[0]["info"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let their_part = their_messages[0]["parts"][0].clone();
    let their_part_id = their_part["id"].as_str().unwrap().to_string();
    let response = call(
        &f.router,
        "POST",
        &format!("/session/{theirs}/prompt_async"),
        Some(&basic()),
        Some(json!({ "parts": [{ "type": "text", "text": "go" }] })),
    )
    .await;
    assert!(response.status().is_success());
    let their_ask = pending_permission(&f, &f.dirs.repo, &basic()).await;

    let mut rewritten = their_part.clone();
    rewritten["text"] = json!("rewritten");
    // (who, method, route under /session/{mine}, body)
    let cases: Vec<(&str, &str, String, Value)> = vec![
        (
            "chat",
            "POST",
            format!("permissions/{their_ask}"),
            json!({ "response": "once" }),
        ),
        (
            "basic",
            "POST",
            format!("permissions/{their_ask}"),
            json!({ "response": "once" }),
        ),
        (
            "basic",
            "POST",
            format!("permissions/{their_ask}"),
            json!({ "response": "reject" }),
        ),
        (
            "chat",
            "GET",
            format!("message/{their_message}"),
            Value::Null,
        ),
        (
            "basic",
            "GET",
            format!("message/{their_message}"),
            Value::Null,
        ),
        (
            "basic",
            "DELETE",
            format!("message/{their_message}"),
            Value::Null,
        ),
        (
            "basic",
            "PATCH",
            format!("message/{their_message}/part/{their_part_id}"),
            rewritten,
        ),
        (
            "basic",
            "DELETE",
            format!("message/{their_message}/part/{their_part_id}"),
            Value::Null,
        ),
        (
            "chat",
            "POST",
            "revert".to_string(),
            json!({ "messageID": their_message }),
        ),
        (
            "basic",
            "POST",
            "revert".to_string(),
            json!({ "messageID": their_message }),
        ),
    ];
    for (who, method, route, body) in cases {
        let auth = if who == "chat" { chat.clone() } else { basic() };
        let body = (!body.is_null()).then_some(body);
        let response = call(
            &f.router,
            method,
            &format!("/session/{mine}/{route}"),
            Some(&auth),
            body,
        )
        .await;
        let status = response.status();
        let text = body_string(response).await;
        assert!(
            status == StatusCode::NOT_FOUND
                || status == StatusCode::FORBIDDEN
                || status == StatusCode::BAD_REQUEST,
            "{who} {method} {route}: {status} {text}"
        );
        assert!(
            !text.contains("theirs-text"),
            "{who} {method} {route} returned the other session's content"
        );
        // The other session is exactly as it was.
        assert_eq!(
            pending_permission(&f, &f.dirs.repo, &basic()).await,
            their_ask,
            "{who} {method} {route}: the other session's ask was answered"
        );
        let now = messages(&f, &theirs, &basic()).await;
        assert_eq!(
            now[0], their_messages[0],
            "{who} {method} {route}: the other session's message changed"
        );
    }

    // A fork "from" a message of another session copies nothing of it.
    let fork = call(
        &f.router,
        "POST",
        &format!("/session/{mine}/fork"),
        Some(&chat),
        Some(json!({ "messageID": their_message })),
    )
    .await;
    assert_eq!(fork.status(), StatusCode::OK);
    let fork = body_json(fork).await["id"].as_str().unwrap().to_string();
    let forked = messages(&f, &fork, &chat).await.to_string();
    assert!(!forked.contains("theirs-text"), "{forked}");

    // The ask is answerable where it belongs.
    assert_eq!(
        call(
            &f.router,
            "POST",
            &format!("/session/{theirs}/permissions/{their_ask}"),
            Some(&basic()),
            Some(json!({ "response": "reject" })),
        )
        .await
        .status(),
        StatusCode::OK
    );
}

/// The `write` tool, asked to create one file.
fn write_turn(path: &str) -> support::Script {
    support::tool_turn(
        "call_w",
        "write",
        json!({ "filePath": path, "content": "{ \"permission\": { \"bash\": \"allow\" } }\n" }),
    )
}

/// A host config under which nothing but the write guard stands between a
/// session's edit tools and a file: `edit` and `external_directory` are
/// allowed outright.
fn permissive(signer: &Signer, extra: Value) -> impl FnOnce(&Dirs) -> Value + '_ {
    move |dirs| {
        let mut config = host_config(signer)(dirs);
        config["permission"] = json!({ "edit": "allow", "external_directory": "allow" });
        config["triggers"] = json!({ "webhook_min_interval": "0s" });
        config["trigger"] = json!({
            // A webhook run carries outside input; its own block grants edit.
            "deliveries": { "on": { "webhook": true }, "prompt": "Handle the delivery",
                            "permission": { "edit": "allow", "external_directory": "allow" } },
        });
        alforria_core::merge_patch(&mut config, &extra);
        config
    }
}

/// Every path the loaders consult for this instance, as files to try to
/// write: the instance's own protected set (built from the config loader's
/// search directories, the global paths and the loaded config), and every
/// loader name in the project, in an ancestor of it, and in the home.
fn loader_targets(f: &Fixture) -> Vec<std::path::PathBuf> {
    use alforria_core::project_files::{loader_files, LOADER_DIRS};
    let services = f.ctx.instances.load(&f.dirs.repo).expect("instance");
    let protected = services.agents.protected();
    assert!(protected.locations().len() >= 6, "{protected:?}");
    let mut targets: Vec<std::path::PathBuf> = protected
        .locations()
        .iter()
        .map(|(_, location)| location.join("zz-guard-probe.txt"))
        .collect();
    let ancestor = f.dirs.repo.parent().unwrap().to_path_buf();
    for base in [
        f.dirs.repo.clone(),
        f.dirs.repo.join("sub"),
        ancestor,
        f.dirs.home.clone(),
    ] {
        for name in loader_files() {
            targets.push(base.join(name));
        }
        for dir in LOADER_DIRS {
            targets.push(base.join(dir).join("zz-guard-probe.md"));
        }
    }
    targets
}

async fn prompt_and_settle(f: &Fixture, auth: &str, text: &str) {
    let session = create_session(f, &f.dirs.repo, auth).await;
    let response = call(
        &f.router,
        "POST",
        &format!("/session/{session}/prompt_async"),
        Some(auth),
        Some(json!({ "parts": [{ "type": "text", "text": text }] })),
    )
    .await;
    assert!(response.status().is_success(), "{}", response.status());
    wait_idle(f, &session).await;
}

/// The agent's own edit tools do not write what its configuration is loaded
/// from when the session is restricted — whichever loader would read the
/// file and wherever it looks: in the project, in an ancestor directory,
/// in the home or a global location. The guard is at the write itself, so
/// it holds with `edit` and `external_directory` allowed outright. The
/// operator's own session writes them as before, and `edit_loader` in the
/// host config hands the write back.
#[tokio::test]
async fn a_restricted_sessions_edit_tools_refuse_every_loader_location() {
    let signer = Signer::new();
    // One fixture to learn the targets (their paths differ per fixture, so
    // the scripts are built per fixture below).
    let scripts_for = |targets: &[std::path::PathBuf]| -> Vec<support::Script> {
        targets
            .iter()
            .flat_map(|target| {
                [
                    write_turn(&target.display().to_string()),
                    support::text_turn("done"),
                ]
            })
            .collect()
    };

    let f = fixture_with(permissive(&signer, json!({})), Some(PASSWORD), Vec::new());
    let targets = loader_targets(&f);
    assert!(targets.len() > 40, "{}", targets.len());
    f.llm.extend(scripts_for(&targets));
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    for target in &targets {
        prompt_and_settle(&f, &chat, &format!("write {}", target.display())).await;
        assert!(
            !target.exists(),
            "a restricted session wrote {}",
            target.display()
        );
    }
    // The guard is not a blanket refusal: an ordinary file is written.
    f.llm.extend(scripts_for(&[f.dirs.repo.join("notes.txt")]));
    prompt_and_settle(&f, &chat, "write notes").await;
    assert!(f.dirs.repo.join("notes.txt").exists());

    // The operator's session is not guarded.
    let own = [
        f.dirs.repo.join("opencode.json"),
        f.dirs.repo.join("AGENTS.md"),
    ];
    f.llm.extend(scripts_for(&own));
    for target in &own {
        prompt_and_settle(&f, &basic(), "write").await;
        assert!(target.exists(), "{}", target.display());
    }
    drop(f);

    // The operator's policy can grant it to restricted sessions.
    let f = fixture_with(
        permissive(&signer, json!({ "permission": { "edit_loader": "allow" } })),
        Some(PASSWORD),
        Vec::new(),
    );
    let granted = [
        f.dirs.repo.join("AGENTS.md"),
        f.dirs.repo.join(".opencode/agent/a.md"),
    ];
    f.llm.extend(scripts_for(&granted));
    for target in &granted {
        prompt_and_settle(&f, &chat, "write").await;
        assert!(target.exists(), "{}", target.display());
    }
}

/// A session that carries outside input — a webhook run — is guarded the
/// same way, though it was the operator's trigger that started it and its
/// own block grants `edit`.
#[tokio::test]
async fn an_outside_input_sessions_edit_tools_refuse_loader_locations() {
    let signer = Signer::new();
    let f = fixture_with(permissive(&signer, json!({})), Some(PASSWORD), Vec::new());
    let mut targets = loader_targets(&f);
    // Every protected location, and a spread of the names.
    let locations = f
        .ctx
        .instances
        .load(&f.dirs.repo)
        .unwrap()
        .agents
        .protected()
        .locations()
        .len();
    let names: Vec<_> = targets
        .split_off(locations)
        .into_iter()
        .step_by(5)
        .collect();
    targets.extend(names);
    targets.push(f.dirs.repo.join("notes.txt"));
    f.llm.extend(
        targets
            .iter()
            .flat_map(|target| {
                [
                    write_turn(&target.display().to_string()),
                    support::text_turn("done"),
                ]
            })
            .collect(),
    );
    let dir = dir_query(&f.dirs.repo);
    for (index, target) in targets.iter().enumerate() {
        let response = call(
            &f.router,
            "POST",
            &format!("/trigger/deliveries/hook?{dir}"),
            Some(&basic()),
            Some(json!({ "delivery": index })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let run = body_json(response).await;
        // Wait for this run to settle before the next delivery.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let runs = body_json(
                call(
                    &f.router,
                    "GET",
                    &format!("/trigger/deliveries/runs?{dir}"),
                    Some(&basic()),
                    None,
                )
                .await,
            )
            .await;
            let settled = runs.as_array().unwrap().iter().any(|entry| {
                entry["id"] == run["id"]
                    && entry["status"] != "running"
                    && entry["status"] != "queued"
            });
            if settled {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the run never settled: {runs}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let expected = *target == f.dirs.repo.join("notes.txt");
        assert_eq!(target.exists(), expected, "{}", target.display());
    }
}

/// Every route that names a session and is not a read, in both generations
/// and alforria's own, taken from the route tables — so a route added to
/// one is tested here without anyone remembering to.
fn mutating_session_routes() -> Vec<(&'static str, &'static str)> {
    let mut routes: Vec<(&str, &str)> = alforria_server::routes::v1::ROUTES
        .iter()
        .chain(alforria_server::routes::v2::ROUTES.iter())
        .copied()
        .chain(
            alforria_server::middleware::scope::ROUTES
                .iter()
                .map(|(method, route, _)| (*method, *route)),
        )
        // alforria's own, outside every table.
        .chain([("DELETE", "/session/{sessionID}/restriction")])
        .collect();
    routes.sort();
    routes.dedup();
    // The guard keys on the parameter's name: every table spells it so.
    for (_, route) in &routes {
        assert_eq!(
            route.contains("/session/{"),
            route.contains("/session/{sessionID}"),
            "{route}: a session in a path is `{{sessionID}}`"
        );
    }
    routes
        .into_iter()
        .filter(|(method, route)| *method != "GET" && route.contains("{sessionID}"))
        .collect()
}

/// A route template with the session filled in and every other parameter
/// given a placeholder.
fn filled(route: &str, session: &str) -> String {
    route
        .split('/')
        .map(|segment| match segment {
            "{sessionID}" => session.to_string(),
            "{messageID}" => "msg_x".to_string(),
            "{partID}" => "prt_x".to_string(),
            "{permissionID}" | "{requestID}" => "per_x".to_string(),
            other if other.starts_with('{') => "x".to_string(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// `POST /session` names a session too, in its body: `parentID`. The guard
/// over the session routes reads the path, so a `chat` token could create
/// a session as the child of an operator's — listed among that session's
/// subagents, resumable by its model as a task, and inheriting what a
/// child inherits. A token below `admin` gives a session only a parent it
/// may act on.
#[tokio::test]
async fn a_narrow_token_cannot_create_a_child_of_an_operators_session() {
    let signer = Signer::new();
    let f = fixture_with(policy_config(&signer, false), Some(PASSWORD), Vec::new());
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let operators = create_session(&f, &f.dirs.repo, &basic()).await;
    let own = create_session(&f, &f.dirs.repo, &chat).await;
    let create = |auth: String, parent: String| {
        let router = f.router.clone();
        let uri = format!("/session?{}", dir_query(&f.dirs.repo));
        async move {
            let body = json!({ "parentID": parent });
            let response = call(&router, "POST", &uri, Some(&auth), Some(body)).await;
            (response.status(), body_string(response).await)
        }
    };
    let children = |parent: String| {
        let router = f.router.clone();
        async move {
            let uri = format!("/session/{parent}/children");
            let response = call(&router, "GET", &uri, Some(&basic()), None).await;
            body_json(response).await.as_array().unwrap().len()
        }
    };

    let (status, body) = create(chat.clone(), operators.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("belongs to the operator"), "{body}");
    assert_eq!(children(operators.clone()).await, 0, "nothing was created");

    // Its own session takes a child, and the operator's takes the operator's.
    let (status, body) = create(chat.clone(), own.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(children(own).await, 1);
    let (status, body) = create(basic(), operators.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(children(operators).await, 1);
}

/// A token below `admin` writes nothing to an operator's session, on any
/// route that names one — title, metadata, archive, delete, share, revert,
/// summarize, abort, init, command, shell, message and part edits, the v2
/// routes — however the session id is spelled. The check used to look the
/// session up under the raw path segment: `/session/ses%5F…` named none,
/// nothing was refused, and the handler then decoded it to the operator's
/// session.
#[tokio::test]
async fn every_mutating_session_route_refuses_a_narrow_token_on_an_operators_session() {
    use alforria_server::bearer::Scope;
    let signer = Signer::new();
    let f = fixture_with(policy_config(&signer, false), Some(PASSWORD), Vec::new());
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let routes = mutating_session_routes();
    assert!(routes.len() > 25, "{routes:?}");
    let exempt = alforria_server::confine::NOT_AN_ACT_IN_THE_SESSION;
    assert_eq!(exempt, [("POST", "/session/{sessionID}/fork")]);
    let send = |method: &'static str, uri: String, auth: String| {
        let router = f.router.clone();
        async move {
            // A request that is let through may start a turn nobody ends.
            let sent = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                call(&router, method, &uri, Some(&auth), Some(json!({}))),
            )
            .await;
            match sent {
                Ok(response) => {
                    let status = response.status();
                    (Some(status), body_string(response).await)
                }
                Err(_) => (None, String::new()),
            }
        }
    };
    for (method, route) in routes {
        if exempt.contains(&(method, route)) {
            continue;
        }
        let operators = create_session(&f, &f.dirs.repo, &basic()).await;
        let open_to_chat =
            alforria_server::middleware::scope::required(method, &filled(route, &operators))
                <= Scope::Chat;
        // Plain, percent-encoded, and encoded with a directory the token
        // may open named beside it (which is where the request was placed
        // when the encoded id named no session).
        let encoded = operators.replace('_', "%5F");
        for uri in [
            filled(route, &operators),
            filled(route, &encoded),
            format!("{}?{}", filled(route, &encoded), dir_query(&f.dirs.repo)),
        ] {
            let (status, body) = send(method, uri.clone(), chat.clone()).await;
            assert_eq!(
                status,
                Some(StatusCode::FORBIDDEN),
                "{method} {uri}: {body}"
            );
            // Where the scope table lets `chat` in, it is the session
            // guard that refuses.
            if open_to_chat {
                assert!(
                    body.contains("belongs to the operator"),
                    "{method} {uri}: {body}"
                );
            }
        }
        // Nothing happened to it.
        let after = call(
            &f.router,
            "GET",
            &format!("/session/{operators}"),
            Some(&basic()),
            None,
        )
        .await;
        assert_eq!(after.status(), StatusCode::OK, "{method} {route}");
        let after = body_json(after).await;
        assert!(
            after["share"].is_null() && after["revert"].is_null(),
            "{after}"
        );
        assert!(after["time"]["archived"].is_null(), "{after}");
        assert!(!restricted(&f, &operators).await);

        // The operator is not refused…
        let (status, body) = send(method, filled(route, &operators), basic()).await;
        assert!(
            !matches!(
                status,
                Some(StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED)
            ),
            "admin {method} {route}: {status:?} {body}"
        );
        // …and neither is the token in a session of its own.
        if open_to_chat {
            for encoded in [false, true] {
                // One each: the route may be the one that deletes it.
                let mine = create_session(&f, &f.dirs.repo, &chat).await;
                let spelled = if encoded {
                    mine.replace('_', "%5F")
                } else {
                    mine
                };
                let uri = filled(route, &spelled);
                let (status, body) = send(method, uri.clone(), chat.clone()).await;
                assert_ne!(
                    status,
                    Some(StatusCode::FORBIDDEN),
                    "{method} {uri}: {body}"
                );
            }
        }
    }

    // A session read by an encoded id is placed by the session's own
    // directory, as by its plain id: one in a directory the token may not
    // open is not readable through a `?directory=` it may.
    let elsewhere = create_session(&f, &f.dirs.other, &basic()).await;
    for spelled in [elsewhere.clone(), elsewhere.replace('_', "%5F")] {
        let uri = format!("/session/{spelled}/message?{}", dir_query(&f.dirs.repo));
        assert_eq!(
            status(&f, "GET", &uri, &chat).await,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
}

/// The frozen `GET /permission` lists a token below `admin` the asks it
/// may answer — not an operator's session's, which it could only read
/// about — in upstream's shape.
#[tokio::test]
async fn pending_asks_are_listed_to_whoever_may_answer_them() {
    let signer = Signer::new();
    let f = fixture_with(
        policy_config(&signer, false),
        Some(PASSWORD),
        vec![bash_ask(), bash_ask()],
    );
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let list = |auth: String| {
        let (router, uri) = (
            f.router.clone(),
            format!("/permission?{}", dir_query(&f.dirs.repo)),
        );
        async move { body_json(call(&router, "GET", &uri, Some(&auth), None).await).await }
    };
    let operators = start_bash_ask(&f, &f.dirs.repo, &basic()).await;
    assert_eq!(list(chat.clone()).await, json!([]));
    let questions = call(
        &f.router,
        "GET",
        &format!("/question?{}", dir_query(&f.dirs.repo)),
        Some(&chat),
        None,
    )
    .await;
    assert_eq!(body_json(questions).await, json!([]));

    let mine = start_bash_ask(&f, &f.dirs.repo, &chat).await;
    assert_ne!(mine, operators);
    let shown = list(chat.clone()).await;
    assert_eq!(shown.as_array().unwrap().len(), 1, "{shown}");
    assert_eq!(shown[0]["id"], mine.as_str());
    // The entry is upstream's, whole.
    for key in [
        "id",
        "sessionID",
        "permission",
        "patterns",
        "metadata",
        "always",
    ] {
        assert!(!shown[0][key].is_null(), "{key}: {shown}");
    }
    let all = list(basic()).await;
    assert_eq!(all.as_array().unwrap().len(), 2, "{all}");
    for id in [&operators, &mine] {
        assert_eq!(reply(&f, id, &basic(), "reject").await, StatusCode::OK);
    }
}
