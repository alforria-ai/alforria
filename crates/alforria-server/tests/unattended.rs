//! The unattended policy for permission asks and questions, and
//! `GET /attention`, through the real engine over a scripted model.

mod support;

use axum::http::StatusCode;
use serde_json::{json, Value};

use support::{bearer, body_json, call, fixture_with, tool_turn, urlencode, Dirs, Fixture, Signer};

const AUDIENCE: &str = "agent-42";

/// The host config: `bash` asks; bearer authentication only when a signer
/// is given (otherwise the server is open, as the operator's own).
fn config(
    signer: Option<&Signer>,
    permission_unattended: Option<Value>,
    question_unattended: Option<Value>,
) -> impl FnOnce(&Dirs) -> Value + '_ {
    move |dirs| {
        let mut permission = json!({ "bash": "ask" });
        if let Some(unattended) = permission_unattended {
            permission["unattended"] = unattended;
        }
        let mut config = json!({
            "server": { "directories": [dirs.repo.display().to_string()] },
            "permission": permission,
        });
        if let Some(signer) = signer {
            config["server"]["auth"] =
                json!({ "bearer": { "keys": [signer.public_key()], "audience": AUDIENCE } });
        }
        if let Some(unattended) = question_unattended {
            config["question"] = json!({ "unattended": unattended });
        }
        config
    }
}

fn dir(path: &std::path::Path) -> String {
    format!("directory={}", urlencode(&path.display().to_string()))
}

async fn create_session(f: &Fixture, directory: &std::path::Path, auth: Option<&str>) -> String {
    let response = call(
        &f.router,
        "POST",
        &format!("/session?{}", dir(directory)),
        auth,
        Some(json!({ "title": "Deploy the site" })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response).await["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn prompt(f: &Fixture, session: &str, auth: Option<&str>) -> Value {
    let response = call(
        &f.router,
        "POST",
        &format!("/session/{session}/message"),
        auth,
        Some(json!({ "parts": [{ "type": "text", "text": "go" }] })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response).await
}

async fn prompt_async(f: &Fixture, session: &str, auth: Option<&str>) {
    let response = call(
        &f.router,
        "POST",
        &format!("/session/{session}/prompt_async"),
        auth,
        Some(json!({ "parts": [{ "type": "text", "text": "go" }] })),
    )
    .await;
    assert!(response.status().is_success());
}

async fn attention(f: &Fixture, auth: Option<&str>) -> Value {
    let response = call(&f.router, "GET", "/attention", auth, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response).await
}

async fn wait_for_attention(f: &Fixture, auth: Option<&str>, count: usize) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let body = attention(f, auth).await;
        if body["count"] == json!(count) {
            return body;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "attention never reached {count}: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The tool part of the first assistant message of a session.
async fn tool_part(f: &Fixture, session: &str, auth: Option<&str>) -> Value {
    let messages = body_json(
        call(
            &f.router,
            "GET",
            &format!("/session/{session}/message"),
            auth,
            None,
        )
        .await,
    )
    .await;
    messages
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["parts"].as_array().unwrap().clone())
        .find(|part| part["type"] == "tool")
        .unwrap_or_else(|| panic!("no tool part: {messages}"))
}

fn bash(command: &str) -> support::Script {
    tool_turn(
        "call_1",
        "bash",
        json!({ "command": command, "description": "d" }),
    )
}

/// Nobody answers: the ask is denied after the timeout, the model is told
/// why and carries on, and `permission.replied` says nobody answered.
#[tokio::test]
async fn an_unanswered_ask_times_out_to_deny() {
    let f = fixture_with(
        config(
            None,
            Some(json!({ "timeout": "300ms", "on_timeout": "deny" })),
            None,
        ),
        None,
        vec![bash("touch denied-marker")],
    );
    let session = create_session(&f, &f.dirs.repo, None).await;
    let services = f.ctx.instances.load(&f.dirs.repo).unwrap();
    let mut replied = services.events.subscribe("permission.replied");

    let started = std::time::Instant::now();
    let answer = prompt(&f, &session, None).await;
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(300),
        "the ask waited for its timeout"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "the turn did not hang"
    );

    assert!(
        !f.dirs.repo.join("denied-marker").exists(),
        "the command ran"
    );
    let part = tool_part(&f, &session, None).await;
    assert_eq!(part["state"]["status"], "error", "{part}");
    let error = part["state"]["error"].as_str().unwrap();
    assert!(error.contains("denied automatically"), "{error}");
    assert!(error.contains("within 300ms"), "{error}");
    // The model saw the denial and the turn went on to its answer.
    assert_eq!(
        f.llm.request_count(),
        2,
        "the loop continued after the denial"
    );
    assert_eq!(answer["info"]["role"], "assistant");
    assert!(answer["info"]["error"].is_null(), "{answer}");

    let event = replied.recv().await.unwrap();
    assert_eq!(event.data["reply"], "reject");
    assert_eq!(event.data["automatic"], true);
    assert_eq!(event.data["sessionID"], json!(session));
    assert_eq!(attention(&f, None).await["count"], 0);
}

/// `on_timeout: "allow"` lets the call through, once.
#[tokio::test]
async fn an_unanswered_ask_times_out_to_allow() {
    let f = fixture_with(
        config(
            None,
            Some(json!({ "timeout": 200, "on_timeout": "allow" })),
            None,
        ),
        None,
        vec![bash("touch allowed-marker")],
    );
    let session = create_session(&f, &f.dirs.repo, None).await;
    let services = f.ctx.instances.load(&f.dirs.repo).unwrap();
    let mut replied = services.events.subscribe("permission.replied");

    prompt(&f, &session, None).await;
    assert!(
        f.dirs.repo.join("allowed-marker").exists(),
        "the command ran"
    );
    let part = tool_part(&f, &session, None).await;
    assert_eq!(part["state"]["status"], "completed", "{part}");
    let event = replied.recv().await.unwrap();
    assert_eq!(event.data["reply"], "once");
    assert_eq!(event.data["automatic"], true);
}

/// A question nobody answers is dismissed with a message telling the model
/// the user is unavailable, and the turn goes on.
#[tokio::test]
async fn an_unanswered_question_times_out() {
    let question = tool_turn(
        "call_q",
        "question",
        json!({ "questions": [{
            "question": "Which environment should I deploy to?",
            "header": "Environment",
            "options": [
                { "label": "staging", "description": "the staging cluster" },
                { "label": "production", "description": "the real thing" }
            ]
        }] }),
    );
    let f = fixture_with(
        config(
            None,
            None,
            Some(json!({ "timeout": "300ms", "on_timeout": "reject" })),
        ),
        None,
        vec![question],
    );
    let session = create_session(&f, &f.dirs.repo, None).await;
    let services = f.ctx.instances.load(&f.dirs.repo).unwrap();
    let mut rejected = services.events.subscribe("question.rejected");

    let answer = prompt(&f, &session, None).await;
    let part = tool_part(&f, &session, None).await;
    assert_eq!(part["tool"], "question");
    assert_eq!(part["state"]["status"], "error", "{part}");
    let error = part["state"]["error"].as_str().unwrap();
    assert!(error.contains("The user is not available"), "{error}");
    assert!(error.contains("within 300ms"), "{error}");
    assert_eq!(f.llm.request_count(), 2, "the model got to react");
    assert!(answer["info"]["error"].is_null(), "{answer}");

    let event = rejected.recv().await.unwrap();
    assert_eq!(event.data["automatic"], true);
    assert_eq!(event.data["sessionID"], json!(session));
    assert_eq!(attention(&f, None).await["count"], 0);
}

/// With no policy (the default, `wait`) an ask stays pending; `/attention`
/// lists it with what a control plane needs, and clears when it is
/// answered.
#[tokio::test]
async fn attention_lists_pending_asks_and_clears() {
    let question = tool_turn(
        "call_q",
        "question",
        json!({ "questions": [{
            "question": "Ship it?", "header": "Ship",
            "options": [{ "label": "yes", "description": "ship" }, { "label": "no", "description": "wait" }]
        }] }),
    );
    let f = fixture_with(
        config(None, None, None),
        None,
        vec![bash("echo hi"), question],
    );
    assert_eq!(
        attention(&f, None).await,
        json!({ "count": 0, "items": [] })
    );

    let session = create_session(&f, &f.dirs.repo, None).await;
    prompt_async(&f, &session, None).await;
    let body = wait_for_attention(&f, None, 1).await;
    let item = &body["items"][0];
    assert_eq!(item["kind"], "permission");
    assert!(item["id"].as_str().unwrap().starts_with("per"));
    assert_eq!(item["sessionID"], json!(session));
    assert_eq!(item["title"], "Deploy the site");
    assert_eq!(item["directory"], json!(f.dirs.repo.display().to_string()));
    assert_eq!(item["summary"], "bash: echo hi");
    assert!(item["asked"].as_u64().unwrap() > 1_600_000_000_000);
    assert!(item["expires"].is_null(), "no unattended policy: it waits");

    // Still there a moment later, older.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let later = wait_for_attention(&f, None, 1).await;
    assert_eq!(later["items"][0]["id"], item["id"]);
    assert!(later["items"][0]["age"].as_u64().unwrap() >= 400);

    // Answering it clears it; the turn goes on to the question.
    let reply = call(
        &f.router,
        "POST",
        &format!(
            "/permission/{}/reply?{}",
            item["id"].as_str().unwrap(),
            dir(&f.dirs.repo)
        ),
        None,
        Some(json!({ "reply": "once" })),
    )
    .await;
    assert_eq!(reply.status(), StatusCode::OK);
    let body = wait_for_attention(&f, None, 1).await;
    let item = &body["items"][0];
    assert_eq!(item["kind"], "question", "{body}");
    assert!(item["id"].as_str().unwrap().starts_with("que"));
    assert_eq!(item["summary"], "Ship it?");
    let reject = call(
        &f.router,
        "POST",
        &format!(
            "/question/{}/reject?{}",
            item["id"].as_str().unwrap(),
            dir(&f.dirs.repo)
        ),
        None,
        None,
    )
    .await;
    assert_eq!(reject.status(), StatusCode::OK);
    wait_for_attention(&f, None, 0).await;
}

/// With a timeout the item says when it will be answered, and it leaves
/// the list by itself.
#[tokio::test]
async fn attention_reports_the_deadline_and_clears_on_timeout() {
    let f = fixture_with(
        config(
            None,
            Some(json!({ "timeout": "1s", "on_timeout": "deny" })),
            None,
        ),
        None,
        vec![bash("echo hi")],
    );
    let session = create_session(&f, &f.dirs.repo, None).await;
    prompt_async(&f, &session, None).await;
    let body = wait_for_attention(&f, None, 1).await;
    let item = &body["items"][0];
    assert_eq!(
        item["expires"].as_u64().unwrap(),
        item["asked"].as_u64().unwrap() + 1000
    );
    wait_for_attention(&f, None, 0).await;
}

/// A token below `admin` sees what waits in the projects it may open, and
/// only those; the route needs a credential at all.
#[tokio::test]
async fn attention_is_scoped_to_the_callers_projects() {
    let signer = Signer::new();
    let f = fixture_with(
        config(Some(&signer), None, None),
        Some("hunter2"),
        vec![bash("echo one"), bash("echo two")],
    );
    use base64::Engine;
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("alforria:hunter2")
    );
    let chat = bearer(&signer.token(AUDIENCE, "chat"));

    let anonymous = call(&f.router, "GET", "/attention", None, None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let here = create_session(&f, &f.dirs.repo, Some(&basic)).await;
    prompt_async(&f, &here, Some(&basic)).await;
    wait_for_attention(&f, Some(&basic), 1).await;
    let elsewhere = create_session(&f, &f.dirs.other, Some(&basic)).await;
    prompt_async(&f, &elsewhere, Some(&basic)).await;
    let all = wait_for_attention(&f, Some(&basic), 2).await;
    assert_eq!(all["items"][0]["sessionID"], json!(here), "oldest first");
    assert_eq!(all["items"][1]["sessionID"], json!(elsewhere));

    let mine = attention(&f, Some(&chat)).await;
    assert_eq!(mine["count"], 1);
    assert_eq!(mine["items"][0]["sessionID"], json!(here));
}

/// The policy is the host's: a project file cannot turn asks into allows.
#[tokio::test]
async fn a_project_config_cannot_set_the_unattended_policy() {
    let f = fixture_with(
        config(None, None, None),
        None,
        vec![bash("touch project-marker")],
    );
    std::fs::write(
        f.dirs.repo.join("opencode.json"),
        json!({ "permission": { "unattended": { "timeout": "100ms", "on_timeout": "allow" } } })
            .to_string(),
    )
    .unwrap();
    let session = create_session(&f, &f.dirs.repo, None).await;
    prompt_async(&f, &session, None).await;
    wait_for_attention(&f, None, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let body = attention(&f, None).await;
    assert_eq!(body["count"], 1, "still waiting: {body}");
    assert!(body["items"][0]["expires"].is_null());
    assert!(!f.dirs.repo.join("project-marker").exists());
}
