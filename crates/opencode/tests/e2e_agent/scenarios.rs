//! Backend-agnostic scenario drivers (spec E2E §2.4): prompt → pump →
//! assert. Deterministic assertions relax into outcome assertions for
//! live backends (`backend.live()`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::backend::LlmBackend;
use crate::harness::{Env, ProcOutput, Serve};
use crate::wire::{pump_until, Api, EventLog};

pub const A1_FILE_MUTATION: &str = "a1_file_mutation";
pub const A2_MULTI_STEP: &str = "a2_multi_step";
pub const A3_PERMISSION_GATE: &str = "a3_permission_gate";
pub const A5_DOOM_LOOP: &str = "a5_doom_loop";
pub const A8_CANCEL_MID_STREAM: &str = "a8_cancel_mid_stream";

/// One driven scenario: the process output plus the parsed `--format
/// json` event stream. Keeps the `Env` (and its tempdir) alive so the
/// project directory survives for assertions.
pub struct ScenarioRun {
    pub _env: Env,
    pub output: ProcOutput,
    pub project_dir: PathBuf,
    pub events: Vec<Value>,
}

impl ScenarioRun {
    /// The `tool_use` events for the given tool, in stream order.
    fn tool_parts(&self, tool: &str) -> Vec<&Value> {
        self.events
            .iter()
            .filter(|event| {
                event["type"] == json!("tool_use") && event["part"]["tool"] == json!(tool)
            })
            .collect()
    }

    fn assert_exit_zero(&self) {
        assert_eq!(
            self.output.code,
            Some(0),
            "run failed\nstdout={}\nstderr={}",
            self.output.stdout,
            self.output.stderr
        );
    }
}

fn run_scenario(
    backend: &impl LlmBackend,
    scenario: &str,
    prompt: &str,
    setup: impl FnOnce(&Path),
) -> ScenarioRun {
    let env = Env::new(scenario);
    env.write_provider_config(
        &backend.base_url(),
        scenario,
        &backend.api_key(),
        backend.provider_id(),
        backend.model_id(),
        backend.context_limit(),
    );
    let project = env.project_dir();
    std::fs::create_dir_all(&project).expect("create project");
    setup(&project);
    let model = format!("{}/{}", backend.provider_id(), backend.model_id());
    let output = env.run(&["run", "--format", "json", "--model", &model, prompt]);
    let events = output
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("valid JSON event"))
        .collect();
    ScenarioRun {
        _env: env,
        output,
        project_dir: project,
        events,
    }
}

/// The filtered event-type sequence (types + part tags, ids normalized):
/// the golden shape of a `--format json` run. Consecutive parallel tool
/// calls settle in nondeterministic order, so each `tool_use` run is
/// sorted.
fn event_sequence(events: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    let mut run: Vec<String> = Vec::new();
    for event in events {
        let tag = match event["type"].as_str().unwrap_or_default() {
            "tool_use" => format!(
                "tool_use:{}",
                event["part"]["tool"].as_str().unwrap_or_default()
            ),
            kind => kind.to_string(),
        };
        if tag.starts_with("tool_use:") {
            run.push(tag);
        } else {
            run.sort();
            out.append(&mut run);
            out.push(tag);
        }
    }
    run.sort();
    out.append(&mut run);
    out
}

/// A1 — file mutation round-trip: a `write` tool call over the wire, the
/// file lands on disk, the tool result feeds the follow-up request, and
/// the final text answer closes the loop.
pub fn a1_file_mutation(backend: &impl LlmBackend) -> ScenarioRun {
    let run = run_scenario(backend, A1_FILE_MUTATION, "create notes.md for me", |_| {});
    run.assert_exit_zero();

    let content = std::fs::read_to_string(run.project_dir.join("notes.md"))
        .unwrap_or_else(|err| panic!("notes.md must exist: {err}"));
    if let Some(transcript) = backend.transcript(A1_FILE_MUTATION) {
        assert_eq!(
            content,
            transcript
                .tool_argument("cal_1", "content")
                .expect("scripted content"),
            "file content must match the scripted tool call"
        );
    } else {
        assert!(!content.trim().is_empty(), "model-created file is empty");
    }

    let tools = run.tool_parts("write");
    assert_eq!(tools.len(), 1, "one write tool part\n{}", run.output.stdout);
    assert_eq!(tools[0]["part"]["state"]["status"], json!("completed"));
    assert!(tools[0]["part"]["state"]["time"].get("start").is_some());
    assert!(tools[0]["part"]["state"]["time"].get("end").is_some());

    if !backend.live() {
        assert_eq!(
            event_sequence(&run.events),
            [
                // The step finishes (tool-calls) before the forked tool
                // settlement lands on the part.
                "step_start",
                "step_finish",
                "tool_use:write",
                "step_start",
                "text",
                "step_finish",
            ]
        );
    }
    assert!(
        run.events
            .iter()
            .any(|event| event["type"] == json!("text")),
        "no text answer in the stream\n{}",
        run.output.stdout
    );
    run
}

/// A2 — multi-step loop: three model steps, the middle one issuing two
/// parallel `read` calls in one assistant turn.
pub fn a2_multi_step(backend: &impl LlmBackend) -> ScenarioRun {
    let run = run_scenario(
        backend,
        A2_MULTI_STEP,
        "read the two files and answer",
        |project| {
            std::fs::write(project.join("a.txt"), "alpha\n").expect("seed a.txt");
            std::fs::write(project.join("b.txt"), "beta\n").expect("seed b.txt");
        },
    );
    run.assert_exit_zero();

    let reads = run.tool_parts("read");
    assert_eq!(reads.len(), 3, "three read parts across the steps");
    for part in &reads {
        assert_eq!(part["part"]["state"]["status"], json!("completed"));
    }

    if !backend.live() {
        assert_eq!(
            event_sequence(&run.events),
            [
                "step_start",
                "step_finish",
                "tool_use:read",
                "step_start",
                "step_finish",
                "tool_use:read",
                "tool_use:read",
                "step_start",
                "text",
                "step_finish",
            ]
        );
    }
    run
}

// ---------------------------------------------------------------------------
// Wire scenarios (spec E2E §2.4 A3/A5/A8): the test is the HTTP client —
// exactly like a real TUI — driving a spawned `opencode serve` with
// mid-session API access (permission respond, abort).
// ---------------------------------------------------------------------------

/// The reply policy for a permission ask (A3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reply {
    Once,
    Always,
    Reject,
}

/// One driven session over the real server: keeps the environment (and
/// its tempdir) plus the serve child alive.
pub struct WireSession {
    _env: Env,
    _serve: Serve,
    api: Api,
    log: EventLog,
    pub session_id: String,
}

impl WireSession {
    /// The tool parts of every assistant message, in message order.
    fn tool_parts(&self, messages: &[Value]) -> Vec<Value> {
        messages
            .iter()
            .filter(|message| message["info"]["role"] == json!("assistant"))
            .flat_map(|message| {
                message["parts"]
                    .as_array()
                    .map(|parts| parts.to_vec())
                    .unwrap_or_default()
            })
            .filter(|part| part["type"] == json!("tool"))
            .collect()
    }

    /// The text parts of one message, joined.
    fn text_of(message: &Value) -> String {
        message["parts"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter(|part| part["type"] == json!("text"))
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default()
    }
}

async fn start_wire(
    backend: &impl LlmBackend,
    scenario: &str,
    setup: impl FnOnce(&Path),
) -> WireSession {
    let env = Env::new(scenario);
    env.write_provider_config(
        &backend.base_url(),
        scenario,
        &backend.api_key(),
        backend.provider_id(),
        backend.model_id(),
        backend.context_limit(),
    );
    let project = env.project_dir();
    std::fs::create_dir_all(&project).expect("create project");
    setup(&project);
    let serve = Serve::spawn(&env);
    let directory = project.display().to_string();
    let api = Api::new(serve.port, &directory);
    let log = EventLog::subscribe(&serve, &directory);
    let session_id = api.create_session().await;
    WireSession {
        _env: env,
        _serve: serve,
        api,
        log,
        session_id,
    }
}

fn wire_prompt_body(backend: &impl LlmBackend, text: &str) -> Value {
    json!({
        "parts": [{"type": "text", "text": text}],
        "model": {"providerID": backend.provider_id(), "modelID": backend.model_id()},
    })
}

/// Prompt the session and pump the SSE stream, replying to every
/// `permission.asked` through `policy` (ask index is 1-based), until the
/// session goes idle. Returns the asks seen, in order.
async fn drive(
    sess: &WireSession,
    prompt: Value,
    mut policy: impl FnMut(usize, &Value) -> Option<&'static str>,
) -> Vec<Value> {
    sess.api.prompt_async(&sess.session_id, prompt).await;
    let mut asks: Vec<Value> = Vec::new();
    let mut cursor = 0;
    let mut busy = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let events = sess.log.events();
        let mut done = false;
        for event in &events[cursor..] {
            let properties = &event["properties"];
            if properties["sessionID"] != json!(sess.session_id) {
                continue;
            }
            match event["type"].as_str().unwrap_or_default() {
                "permission.asked" => {
                    asks.push(properties.clone());
                    if let Some(reply) = policy(asks.len(), properties) {
                        sess.api
                            .permission_respond(
                                &sess.session_id,
                                properties["id"].as_str().expect("permission id"),
                                reply,
                            )
                            .await;
                    }
                }
                "session.status" => match properties["status"]["type"].as_str() {
                    Some("busy") => busy = true,
                    Some("idle") => done = busy,
                    _ => {}
                },
                _ => {}
            }
        }
        cursor = events.len();
        if done {
            return asks;
        }
        assert!(
            Instant::now() < deadline,
            "session never went idle\n{}",
            serde_json::to_string_pretty(&events).expect("events")
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A3 — the permission gate over the wire: two sequential steps each
/// read `secret.env`, which the default agent ruleset asks for. Replies
/// arrive through `POST /session/{id}/permissions`.
pub async fn a3_permission_gate(backend: &impl LlmBackend, reply: Reply) -> Vec<Value> {
    let sess = start_wire(backend, A3_PERMISSION_GATE, |project| {
        std::fs::write(project.join("secret.env"), "TOKEN=1\n").expect("seed secret.env");
    })
    .await;
    let asks = drive(
        &sess,
        wire_prompt_body(backend, "read twice"),
        |index, _| match (reply, index) {
            (Reply::Once, _) => Some("once"),
            (Reply::Always, 1) => Some("always"),
            (Reply::Reject, 1) => Some("reject"),
            _ => None,
        },
    )
    .await;

    let messages = sess.api.messages(&sess.session_id).await;
    let tools = sess.tool_parts(&messages);
    match reply {
        Reply::Once | Reply::Always => {
            assert_eq!(tools.len(), 2, "both reads ran\n{messages:?}");
            for part in &tools {
                assert_eq!(part["state"]["status"], json!("completed"), "{part}");
            }
            let answered = messages
                .iter()
                .filter(|message| message["info"]["role"] == json!("assistant"))
                .any(|message| WireSession::text_of(message).contains("done reading"));
            assert!(answered, "no final text\n{messages:?}");
        }
        Reply::Reject => {
            assert!(
                tools
                    .iter()
                    .any(|part| part["state"]["status"] == json!("error")),
                "no tool-error part\n{messages:?}"
            );
        }
    }
    match reply {
        Reply::Once => assert_eq!(asks.len(), 2, "once must ask for every call"),
        Reply::Always => assert_eq!(asks.len(), 1, "always must skip the second ask"),
        Reply::Reject => assert!(!asks.is_empty(), "no ask was published"),
    }
    asks
}

/// A5 — doom-loop over the wire: three identical `read` calls trigger one
/// `doom_loop` ask with `always = [tool]`; answering unblocks and all
/// three calls ran.
pub async fn a5_doom_loop(backend: &impl LlmBackend) -> Vec<Value> {
    let sess = start_wire(backend, A5_DOOM_LOOP, |project| {
        std::fs::write(project.join("a.txt"), "x\n").expect("seed a.txt");
    })
    .await;
    let asks = drive(&sess, wire_prompt_body(backend, "loop the tool"), |_, _| {
        Some("once")
    })
    .await;

    assert_eq!(asks.len(), 1, "exactly one doom_loop ask\n{asks:?}");
    let ask = &asks[0];
    assert_eq!(ask["permission"], json!("doom_loop"));
    assert_eq!(ask["patterns"], json!(["read"]));
    assert_eq!(ask["always"], json!(["read"]));

    let messages = sess.api.messages(&sess.session_id).await;
    let tools = sess.tool_parts(&messages);
    assert_eq!(tools.len(), 3, "all three calls ran\n{messages:?}");
    for part in &tools {
        assert_eq!(part["state"]["status"], json!("completed"), "{part}");
    }
    let recovered = messages
        .iter()
        .filter(|message| message["info"]["role"] == json!("assistant"))
        .any(|message| WireSession::text_of(message).contains("recovered"));
    assert!(recovered, "no recovery text\n{messages:?}");
    asks
}

/// A8 — cancel mid-stream: the transcript streams a tool call, then
/// stalls; `POST /session/{id}/abort` interrupts it. The re-prompt
/// continues on the next turn. Returns the final message store.
pub async fn a8_cancel_mid_stream(backend: &impl LlmBackend) -> Vec<Value> {
    let sess = start_wire(backend, A8_CANCEL_MID_STREAM, |project| {
        std::fs::write(project.join("a.txt"), "x\n").expect("seed a.txt");
    })
    .await;
    sess.api
        .prompt_async(&sess.session_id, wire_prompt_body(backend, "go"))
        .await;

    // The tool part is in flight (the transcript stalls behind it).
    let session_id = sess.session_id.clone();
    let _ = pump_until(&sess.log, |events| {
        events.iter().any(|event| {
            event["type"] == json!("message.part.updated")
                && event["properties"]["sessionID"] == json!(session_id)
                && event["properties"]["part"]["type"] == json!("tool")
                && event["properties"]["part"]["state"]["status"] == json!("pending")
        })
    })
    .await;
    sess.api.abort(&sess.session_id).await;

    // The interrupted assistant finalizes: abort error, completed time,
    // the in-flight tool part marked interrupted (prompt.ts:1206-1212).
    let deadline = Instant::now() + Duration::from_secs(60);
    let aborted = loop {
        let messages = sess.api.messages(&sess.session_id).await;
        let done = messages
            .iter()
            .rfind(|message| {
                message["info"]["role"] == json!("assistant")
                    && message["info"]["error"].is_object()
                    && message["info"]["time"]["completed"].is_number()
            })
            .cloned();
        if let Some(message) = done {
            break message;
        }
        assert!(
            Instant::now() < deadline,
            "aborted assistant never finalized\n{messages:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(
        aborted["info"]["error"]["name"],
        json!("MessageAbortedError"),
        "{aborted}"
    );
    let interrupted = aborted["parts"]
        .as_array()
        .map(|parts| {
            parts.iter().any(|part| {
                part["type"] == json!("tool")
                    && part["state"]["status"] == json!("error")
                    && part["state"]["metadata"]["interrupted"] == json!(true)
            })
        })
        .unwrap_or_default();
    assert!(interrupted, "no interrupted tool part\n{aborted}");

    // A re-prompt continues the session normally.
    let resumed = sess
        .api
        .prompt(&sess.session_id, wire_prompt_body(backend, "go again"))
        .await;
    assert!(
        WireSession::text_of(&resumed).contains("resumed"),
        "no resumed text\n{resumed}"
    );
    sess.api.messages(&sess.session_id).await
}
