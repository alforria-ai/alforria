//! A guarded session's `read`, `grep` and `glob` do not open the host's
//! own files: `auth.json` and the rest of the data directory, the global
//! config (webhook secrets, MCP headers). The `external_directory` ask was
//! the only thing in the way, and the token that asked could answer it.

mod support;

use axum::http::StatusCode;
use serde_json::{json, Value};

use support::{basic, bearer, body_json, call, fixture_with, urlencode, Dirs, Fixture, Signer};

const AUDIENCE: &str = "agent-42";
const PASSWORD: &str = "hunter2";
const SECRET: &str = "sk-live-0123456789-host-secret";

fn host_config(signer: &Signer) -> impl FnOnce(&Dirs) -> Value + '_ {
    move |dirs| {
        json!({
            "server": {
                "auth": { "bearer": { "keys": [signer.public_key()], "audience": AUDIENCE } },
                "directories": [dirs.repo.display().to_string()]
            },
            "hook": { "webhook": [{
                "url": "https://hooks.example.com/alforria",
                "events": ["session.idle"],
                "secret": SECRET,
            }] },
        })
    }
}

fn dir_query(f: &Fixture) -> String {
    format!(
        "directory={}",
        urlencode(&f.dirs.repo.display().to_string())
    )
}

/// Create a session as `auth`, send one prompt, answer every permission
/// ask with `once` as `auth`, and return how many asks there were and the
/// session's messages once it is idle.
async fn run_turn(f: &Fixture, auth: &str) -> (Vec<Value>, Value) {
    let response = call(
        &f.router,
        "POST",
        &format!("/session?{}", dir_query(f)),
        Some(auth),
        Some(json!({})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let session = body_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let response = call(
        &f.router,
        "POST",
        &format!("/session/{session}/prompt_async"),
        Some(auth),
        Some(json!({ "parts": [{ "type": "text", "text": "go" }] })),
    )
    .await;
    assert!(response.status().is_success(), "{}", response.status());

    let admin = basic("alforria", PASSWORD);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut asks = Vec::new();
    let mut quiet = 0;
    while quiet < 10 {
        assert!(
            std::time::Instant::now() < deadline,
            "the session never settled"
        );
        let pending = body_json(
            call(
                &f.router,
                "GET",
                &format!("/permission?{}", dir_query(f)),
                Some(auth),
                None,
            )
            .await,
        )
        .await;
        if let Some(id) = pending[0]["id"].as_str() {
            asks.push(pending[0].clone());
            let reply = call(
                &f.router,
                "POST",
                &format!("/permission/{id}/reply?{}", dir_query(f)),
                Some(auth),
                Some(json!({ "reply": "once" })),
            )
            .await;
            assert_eq!(reply.status(), StatusCode::OK);
            quiet = 0;
            continue;
        }
        let status = body_json(
            call(
                &f.router,
                "GET",
                &format!("/session/status?{}", dir_query(f)),
                Some(&admin),
                None,
            )
            .await,
        )
        .await;
        if status.get(&session).is_none() || status[&session]["type"] == "idle" {
            quiet += 1;
        } else {
            quiet = 0;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    let messages = body_json(
        call(
            &f.router,
            "GET",
            &format!("/session/{session}/message?{}", dir_query(f)),
            Some(&admin),
            None,
        )
        .await,
    )
    .await;
    (asks, messages)
}

/// The tool parts of a transcript: `(status, output or error)`.
fn tool_results(messages: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for message in messages.as_array().into_iter().flatten() {
        for part in message["parts"].as_array().into_iter().flatten() {
            if part["type"] == "tool" {
                let state = &part["state"];
                let text = state["output"]
                    .as_str()
                    .or(state["error"].as_str())
                    .unwrap_or_default();
                out.push((
                    state["status"].as_str().unwrap_or_default().to_string(),
                    text.to_string(),
                ));
            }
        }
    }
    out
}

fn fixture(signer: &Signer, tool: &str, input: impl FnOnce(&Fixture) -> Value) -> Fixture {
    let f = fixture_with(host_config(signer), Some(PASSWORD), Vec::new());
    std::fs::create_dir_all(&f.paths.data).unwrap();
    std::fs::write(
        f.paths.data.join("auth.json"),
        json!({ "acme": { "type": "api", "key": SECRET } }).to_string(),
    )
    .unwrap();
    let input = input(&f);
    f.llm.extend(vec![
        support::tool_turn("call_1", tool, input),
        support::text_turn("done"),
    ]);
    f
}

/// A tool, and the input it is called with.
type Attempt = (&'static str, fn(&Fixture) -> Value);

/// What each tool is pointed at: the credential store, by `read`; the data
/// directory, by `grep` for the key; the global config directory, by
/// `glob`; and the global config file, by `read`.
fn attempts() -> Vec<Attempt> {
    vec![
        (
            "read",
            |f| json!({ "filePath": f.paths.data.join("auth.json") }),
        ),
        (
            "read",
            |f| json!({ "filePath": f.paths.config.join("opencode.json") }),
        ),
        (
            "grep",
            |f| json!({ "pattern": "sk-live", "path": f.paths.data }),
        ),
        (
            "glob",
            |f| json!({ "pattern": "*.json", "path": f.paths.config }),
        ),
    ]
}

#[tokio::test]
async fn a_chat_session_cannot_read_grep_or_glob_a_host_location_and_is_not_asked() {
    for (tool, input) in attempts() {
        let signer = Signer::new();
        let f = fixture(&signer, tool, input);
        let chat = bearer(&signer.token(AUDIENCE, "chat"));
        let (asks, messages) = run_turn(&f, &chat).await;
        let results = tool_results(&messages);
        assert_eq!(results.len(), 1, "{tool}: {messages}");
        let transcript = messages.to_string();
        assert!(
            !transcript.contains(SECRET) && !transcript.contains("opencode.json\\n"),
            "{tool}: a host file reached the transcript: {results:?}"
        );
        assert_eq!(asks, Vec::<Value>::new(), "{tool}: an ask was raised");
        assert_eq!(results[0].0, "error", "{tool}: {results:?}");
        assert!(results[0].1.contains("may not read"), "{tool}: {results:?}");
    }
}

/// The operator's own session is not guarded: it is asked about the
/// directory outside the project, as before, and reads once it says yes.
#[tokio::test]
async fn an_operator_session_reads_a_host_location_after_the_usual_ask() {
    let signer = Signer::new();
    let f = fixture(
        &signer,
        "read",
        |f| json!({ "filePath": f.paths.data.join("auth.json") }),
    );
    let (asks, messages) = run_turn(&f, &basic("alforria", PASSWORD)).await;
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0]["permission"], "external_directory");
    let results = tool_results(&messages);
    assert_eq!(results[0].0, "completed", "{results:?}");
    assert!(results[0].1.contains(SECRET), "{results:?}");
}

/// What a guarded session is pointed at by the server itself stays
/// readable: the file a truncated tool output was saved to.
#[tokio::test]
async fn a_chat_session_still_reads_a_saved_tool_output() {
    let signer = Signer::new();
    let f = fixture(&signer, "read", |f| {
        let saved = f.paths.data.join("tool-output").join("tool_1");
        std::fs::create_dir_all(saved.parent().unwrap()).unwrap();
        std::fs::write(&saved, "the rest of the output").unwrap();
        json!({ "filePath": saved })
    });
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let (asks, messages) = run_turn(&f, &chat).await;
    assert_eq!(asks, Vec::<Value>::new());
    let results = tool_results(&messages);
    assert_eq!(results[0].0, "completed", "{results:?}");
    assert!(
        results[0].1.contains("the rest of the output"),
        "{results:?}"
    );
}

/// A search that starts above a host location walks into it: the operator
/// gets the match, a guarded session gets nothing from inside.
#[tokio::test]
async fn a_search_from_above_a_host_location_returns_nothing_from_inside_it() {
    let above = |f: &Fixture| json!({ "pattern": "sk-live", "path": f.dirs.home });
    let signer = Signer::new();
    let f = fixture(&signer, "grep", above);
    let (_, messages) = run_turn(&f, &basic("alforria", PASSWORD)).await;
    assert!(
        messages.to_string().contains(SECRET),
        "the walk reaches the data directory: {:?}",
        tool_results(&messages)
    );

    let f = fixture(&signer, "grep", above);
    let chat = bearer(&signer.token(AUDIENCE, "chat"));
    let (asks, messages) = run_turn(&f, &chat).await;
    // Outside the project, so asked about, and answered by the token.
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert!(
        !messages.to_string().contains(SECRET),
        "{:?}",
        tool_results(&messages)
    );
}
