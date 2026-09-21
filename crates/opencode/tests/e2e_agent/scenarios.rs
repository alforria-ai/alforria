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
pub const A4_SUBAGENT: &str = "a4_subagent";
pub const A5_DOOM_LOOP: &str = "a5_doom_loop";
pub const A6_COMPACTION: &str = "a6_compaction";
pub const A7_REVERT: &str = "a7_revert";
pub const A8_CANCEL_MID_STREAM: &str = "a8_cancel_mid_stream";
pub const A9_STRUCTURED_OUTPUT: &str = "a9_structured_output";
pub const A9_STRUCTURED_ERROR: &str = "a9_structured_error";
pub const CLI_AUTO_REPLY: &str = "cli_auto_reply";

pub const B3_PERMISSION_ASK: &str = "b3_permission_ask";
pub const B4_SUBAGENT: &str = "b4_subagent";
pub const B5_CANCEL_REPROMPT: &str = "b5_cancel_reprompt";
pub const B6_STRUCTURED_OUTPUT: &str = "b6_structured_output";
pub const B7_EXPORT_ROUND_TRIP: &str = "b7_export_round_trip";

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
    pub fn tool_parts(&self, tool: &str) -> Vec<&Value> {
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
    run_scenario_with(backend, scenario, prompt, &[], setup)
}

/// The extra-flags variant (`--auto`, …) of the CLI `run` driver.
fn run_scenario_with(
    backend: &impl LlmBackend,
    scenario: &str,
    prompt: &str,
    extra: &[&str],
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
    let mut args: Vec<&str> = vec!["run", "--format", "json"];
    args.extend_from_slice(extra);
    args.extend(["--model", &model, prompt]);
    let output = env.run_with_timeout(&args, None, backend.turn_timeout() * 4);
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
    // Live models need a directive prompt to guarantee a `write` call.
    let prompt = if backend.live() {
        "Use the write tool to create a file named notes.md with a note about opencode, then confirm."
    } else {
        "create notes.md for me"
    };
    let run = run_scenario(backend, A1_FILE_MUTATION, prompt, |_| {});
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

    if backend.live() {
        // Structural relaxation (spec E2E §3.3): the model's own tool
        // call created the file — any completed tool part proves it.
        let completed = run.events.iter().any(|event| {
            event["type"] == json!("tool_use")
                && event["part"]["state"]["status"] == json!("completed")
        });
        assert!(completed, "no completed tool part\n{}", run.output.stdout);
        return run;
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
    // Live models need a directive prompt to guarantee a `read` call.
    let prompt = if backend.live() {
        "Use the read tool to read a.txt and b.txt, then tell me their contents."
    } else {
        "read the two files and answer"
    };
    let run = run_scenario(backend, A2_MULTI_STEP, prompt, |project| {
        std::fs::write(project.join("a.txt"), "alpha\n").expect("seed a.txt");
        std::fs::write(project.join("b.txt"), "beta\n").expect("seed b.txt");
    });
    run.assert_exit_zero();

    let reads = run.tool_parts("read");
    if backend.live() {
        // Structural relaxation (spec E2E §3.3): any completed read
        // counts live — the model picks its own call count.
        assert!(!reads.is_empty(), "no read parts\n{}", run.output.stdout);
        for part in &reads {
            assert_eq!(
                part["part"]["state"]["status"],
                json!("completed"),
                "{part}"
            );
        }
        return run;
    }
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
    session_id: String,
    budget: Duration,
}

impl WireSession {
    pub fn project_dir(&self) -> PathBuf {
        self._env.project_dir()
    }

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
        budget: backend.turn_timeout() * 4,
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
    policy: impl FnMut(usize, &Value) -> Option<&'static str>,
) -> Vec<Value> {
    sess.api.prompt_async(&sess.session_id, prompt).await;
    pump(sess, policy).await
}

/// Pump the SSE stream after the prompt is in flight: reply to every
/// `permission.asked` through `policy` (ask index is 1-based) until the
/// session goes idle. Returns the asks seen, in order.
async fn pump(
    sess: &WireSession,
    mut policy: impl FnMut(usize, &Value) -> Option<&'static str>,
) -> Vec<Value> {
    let mut asks: Vec<Value> = Vec::new();
    let mut cursor = 0;
    let mut busy = false;
    let deadline = Instant::now() + sess.budget;
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
    let _ = pump_until(&sess.log, sess.budget, |events| {
        events.iter().any(|event| {
            event["type"] == json!("message.part.updated")
                && event["properties"]["sessionID"] == json!(session_id)
                && event["properties"]["part"]["type"] == json!("tool")
                && event["properties"]["part"]["state"]["status"] == json!("pending")
        })
    })
    .await;
    sess.api.abort(&sess.session_id).await;

    // The interrupted assistant finalizes: abort error + completed time.
    // The tool part completed before the abort landed (the AI SDK runtime
    // forks a tool call as soon as its arguments parse — p7 parity): the
    // part carries its accumulated input, not the interrupted marker.
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
    let completed = aborted["parts"]
        .as_array()
        .map(|parts| {
            parts.iter().any(|part| {
                part["type"] == json!("tool")
                    && part["state"]["status"] == json!("completed")
                    && part["state"]["input"] == json!({ "filePath": "a.txt" })
            })
        })
        .unwrap_or_default();
    assert!(completed, "no completed tool part\n{aborted}");

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

/// A4 — the subagent over the wire: a `task` tool call spawns a child
/// session whose answer flows back into the parent. Returns
/// `(parent_id, child_id)` for request-capture assertions.
pub async fn a4_subagent(backend: &impl LlmBackend) -> (String, String) {
    let sess = start_wire(backend, A4_SUBAGENT, |_| {}).await;
    drive(
        &sess,
        wire_prompt_body(backend, "spawn a subagent"),
        |_, _| None,
    )
    .await;

    // The task tool part carries parentSessionId metadata and the
    // wrapped result XML (task tool, tools.ts).
    let messages = sess.api.messages(&sess.session_id).await;
    let task = messages
        .iter()
        .flat_map(|message| {
            message["parts"]
                .as_array()
                .map(|parts| parts.to_vec())
                .unwrap_or_default()
        })
        .find(|part| part["type"] == "tool" && part["tool"] == "task")
        .expect("task tool part");
    assert_eq!(task["state"]["status"], json!("completed"), "{task}");
    assert_eq!(
        task["state"]["metadata"]["parentSessionId"],
        json!(sess.session_id),
        "{task}"
    );
    let child_id = task["state"]["metadata"]["sessionId"]
        .as_str()
        .expect("child session id")
        .to_string();
    let output = task["state"]["output"].as_str().expect("task output");
    assert!(
        output.contains(&format!("<task id=\"{child_id}\" state=\"completed\">")),
        "got {output}"
    );
    assert!(output.contains("subagent answer"), "got {output}");

    // The child session row is listed with the parent link.
    let list = sess.api.session_list().await;
    let child = list
        .iter()
        .find(|item| item["id"] == json!(child_id))
        .expect("child session in the session list");
    assert_eq!(child["parentID"], json!(sess.session_id), "{child}");
    assert_eq!(child["agent"], json!("general"), "{child}");
    (sess.session_id.clone(), child_id)
}

/// A6 — compaction over the wire: usage overflow triggers the compaction
/// fork (served from the transcript slot after the overflowing turn), the
/// loop continues on the compacted history and publishes
/// `session.compacted`.
pub async fn a6_compaction(backend: &impl LlmBackend) {
    let sess = start_wire(backend, A6_COMPACTION, |project| {
        std::fs::write(project.join("a.txt"), "x\n").expect("seed a.txt");
    })
    .await;
    drive(
        &sess,
        wire_prompt_body(backend, "trigger overflow"),
        |_, _| None,
    )
    .await;

    let events = sess.log.events();
    assert!(
        events
            .iter()
            .any(|event| event["type"] == json!("session.compacted")
                && event["properties"]["sessionID"] == json!(sess.session_id)),
        "no session.compacted event\n{}",
        serde_json::to_string_pretty(&events).expect("events")
    );
    let messages = sess.api.messages(&sess.session_id).await;
    let continued = messages
        .iter()
        .filter(|message| message["info"]["role"] == json!("assistant"))
        .any(|message| WireSession::text_of(message).contains("after compaction"));
    assert!(
        continued,
        "no continuation on the compacted history\n{messages:?}"
    );
}

/// A7 — revert/unrevert over a real git worktree (not `InMemorySnapshot`):
/// a `write` tool edit rolls back through the git snapshot restore and the
/// diff summary numbers land on the session.
fn git(project: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(project)
        .status()
        .expect("git spawns");
    assert!(status.success(), "git {args:?} failed");
}

pub async fn a7_revert(backend: &impl LlmBackend) {
    let sess = start_wire(backend, A7_REVERT, |project| {
        git(project, &["init", "--quiet"]);
        git(project, &["config", "user.email", "e2e@opencode.test"]);
        git(project, &["config", "user.name", "E2E"]);
        std::fs::write(project.join("a.txt"), "v1\n").expect("seed a.txt");
        git(project, &["add", "-A"]);
        git(project, &["commit", "--quiet", "-m", "init"]);
    })
    .await;
    // The forked write races the step-finish patch (native-runtime
    // settlements concatenate after the provider stream, so the edit can
    // land after the patch walk). The fixture stalls the stream so the
    // edit is placed deterministically inside the step — after the
    // step-start snapshot, before the stream ends — while the real tool
    // dispatch still executes.
    sess.api
        .prompt_async(&sess.session_id, wire_prompt_body(backend, "edit the file"))
        .await;
    let session_id = sess.session_id.clone();
    pump_until(&sess.log, sess.budget, |events| {
        events.iter().any(|event| {
            event["type"] == json!("message.part.updated")
                && event["properties"]["sessionID"] == json!(session_id)
                && event["properties"]["part"]["type"] == json!("step-start")
        })
    })
    .await;
    std::fs::write(sess.project_dir().join("a.txt"), "v2\n").expect("seed edit");
    pump(&sess, |_, _| None).await;

    let a_txt = sess.project_dir().join("a.txt");
    assert_eq!(
        std::fs::read_to_string(&a_txt).expect("a.txt"),
        "v2\n",
        "the write tool must land the edit"
    );

    // Revert at the user message: the edit rolls back through the git
    // snapshot (revert.ts:38-89) and the session records the diff
    // summary numbers.
    let messages = sess.api.messages(&sess.session_id).await;
    let user_id = messages
        .iter()
        .find(|message| message["info"]["role"] == json!("user"))
        .map(|message| {
            message["info"]["id"]
                .as_str()
                .expect("user message id")
                .to_string()
        })
        .expect("user message");
    let reverted = sess.api.revert(&sess.session_id, &user_id).await;
    assert_eq!(
        reverted["revert"]["messageID"],
        json!(user_id),
        "{reverted}"
    );
    let summary = reverted.get("summary").expect("diff summary recorded");
    assert_eq!(summary["files"], json!(1), "{reverted}");
    assert_eq!(summary["additions"], json!(1), "{reverted}");
    assert_eq!(summary["deletions"], json!(1), "{reverted}");
    assert_eq!(
        std::fs::read_to_string(&a_txt).expect("a.txt"),
        "v1\n",
        "revert must roll the edit back on disk"
    );

    let _ = sess.api.unrevert(&sess.session_id).await;
    assert_eq!(
        std::fs::read_to_string(&a_txt).expect("a.txt"),
        "v2\n",
        "unrevert must restore the edit"
    );
}

/// A9 — structured output over the wire: the prompt body carries a
/// `format` json_schema, the scripted model answers through the
/// `StructuredOutput` tool call, and the captured payload lands on the
/// final assistant message in the store (prompt.ts:1282-1289). Returns
/// the message store for request-capture assertions.
pub async fn a9_structured_output(backend: &impl LlmBackend) -> Vec<Value> {
    let sess = start_wire(backend, A9_STRUCTURED_OUTPUT, |_| {}).await;
    drive(&sess, structured_prompt_body(backend), |_, _| None).await;
    sess.api.messages(&sess.session_id).await
}

/// A9 error variant: the model streams plain text without calling the
/// `StructuredOutput` tool — the assistant message fails with the
/// StructuredOutput error (prompt.ts:1291-1319).
pub async fn a9_structured_error(backend: &impl LlmBackend) -> Vec<Value> {
    let sess = start_wire(backend, A9_STRUCTURED_ERROR, |_| {}).await;
    drive(&sess, structured_prompt_body(backend), |_, _| None).await;
    sess.api.messages(&sess.session_id).await
}

/// The `format` json_schema prompt body (spec E2E §2.4 A9): a plain
/// prompt body plus the output format the loop turns into the
/// `StructuredOutput` tool.
fn structured_prompt_body(backend: &impl LlmBackend) -> Value {
    let mut body = wire_prompt_body(backend, "answer in json");
    body["format"] = json!({
        "type": "json_schema",
        "schema": {
            "type": "object",
            "properties": { "answer": { "type": "number" } }
        }
    });
    body
}

/// A10 — CLI stream shape golden: the filtered event-type sequence of a
/// one-tool session (the A1 transcript), asserted against the committed
/// golden file `golden/a10_cli_stream.json`.
pub fn a10_cli_stream(backend: &impl LlmBackend) {
    let run = run_scenario(backend, A1_FILE_MUTATION, "create notes.md for me", |_| {});
    run.assert_exit_zero();
    if backend.live() {
        return;
    }
    let golden: Vec<String> =
        serde_json::from_str(include_str!("golden/a10_cli_stream.json")).expect("golden file");
    assert_eq!(
        event_sequence(&run.events),
        golden,
        "CLI stream diverged from the committed golden\n{}",
        run.output.stdout
    );
}

/// CLI gap #4 (spec E2E §1.6) — `run` with a permission-gated tool:
/// with `--auto` the run loop answers every `permission.asked` with
/// `once` (run.rs:724-747) and the loop continues; without it the CLI
/// auto-rejects and the loop breaks.
pub fn cli_permission_auto(backend: &impl LlmBackend, auto: bool) -> ScenarioRun {
    let run = run_scenario_with(
        backend,
        CLI_AUTO_REPLY,
        "read twice",
        if auto { &["--auto"] } else { &[] },
        |project| {
            std::fs::write(project.join("secret.env"), "TOKEN=1\n").expect("seed secret.env");
        },
    );
    run.assert_exit_zero();
    if !backend.live() {
        let reads = run.tool_parts("read");
        if auto {
            assert_eq!(reads.len(), 2, "both reads ran\n{}", run.output.stdout);
            for part in &reads {
                assert_eq!(
                    part["part"]["state"]["status"],
                    json!("completed"),
                    "{part}"
                );
            }
        } else {
            assert_eq!(
                reads.len(),
                1,
                "the rejected read must stop the run\n{}",
                run.output.stdout
            );
            assert_eq!(reads[0]["part"]["state"]["status"], json!("error"));
        }
    }
    run
}

// ---------------------------------------------------------------------------
// Live scenarios (spec E2E §3.3): outcome assertions only — the model's
// transcript is whatever the live backend produces. Flaky mismatch is
// absorbed by the whole-scenario retry in `live.rs`, not by loosening
// these asserts.
// ---------------------------------------------------------------------------

/// B3 — permission ask observed over the wire: reading `secret.env`
/// publishes a `permission.asked`; answering `once` through the API lets
/// the tool run to completion.
pub async fn b3_permission_ask(backend: &impl LlmBackend) {
    let sess = start_wire(backend, B3_PERMISSION_ASK, |project| {
        std::fs::write(project.join("secret.env"), "TOKEN=1\n").expect("seed secret.env");
    })
    .await;
    let asks = drive(
        &sess,
        wire_prompt_body(
            backend,
            "read the file secret.env and tell me what is inside it",
        ),
        |_, _| Some("once"),
    )
    .await;

    assert!(!asks.is_empty(), "no permission ask was published");
    let messages = sess.api.messages(&sess.session_id).await;
    let tools = sess.tool_parts(&messages);
    assert!(
        tools
            .iter()
            .any(|part| part["state"]["status"] == json!("completed")),
        "no completed tool part after the reply\n{messages:?}"
    );
}

/// B4 — subagent over the wire: a `task` tool call spawns a child
/// session; the child appears in the session list with the parent link.
pub async fn b4_subagent(backend: &impl LlmBackend) {
    let sess = start_wire(backend, B4_SUBAGENT, |_| {}).await;
    drive(
        &sess,
        wire_prompt_body(
            backend,
            "use the task tool to spawn a subagent and tell me what it said",
        ),
        |_, _| None,
    )
    .await;

    let messages = sess.api.messages(&sess.session_id).await;
    let task = messages
        .iter()
        .flat_map(|message| {
            message["parts"]
                .as_array()
                .map(|parts| parts.to_vec())
                .unwrap_or_default()
        })
        .find(|part| part["type"] == "tool" && part["tool"] == "task")
        .expect("task tool part");
    assert_eq!(task["state"]["status"], json!("completed"), "{task}");
    assert_eq!(
        task["state"]["metadata"]["parentSessionId"],
        json!(sess.session_id),
        "{task}"
    );
    let child_id = task["state"]["metadata"]["sessionId"]
        .as_str()
        .expect("child session id")
        .to_string();
    let list = sess.api.session_list().await;
    let child = list
        .iter()
        .find(|item| item["id"] == json!(child_id))
        .expect("child session in the session list");
    assert_eq!(child["parentID"], json!(sess.session_id), "{child}");
}

/// B5 — cancel + re-prompt over the wire: abort once a part is in
/// flight (or the turn already finished), then a re-prompt must run a
/// fresh turn to completion.
pub async fn b5_cancel_reprompt(backend: &impl LlmBackend) {
    let sess = start_wire(backend, B5_CANCEL_REPROMPT, |project| {
        std::fs::write(project.join("a.txt"), "x\n").expect("seed a.txt");
    })
    .await;
    sess.api
        .prompt_async(
            &sess.session_id,
            wire_prompt_body(backend, "read a.txt and summarize it"),
        )
        .await;

    let session_id = sess.session_id.clone();
    pump_until(&sess.log, sess.budget, |events| {
        events.iter().any(|event| {
            event["properties"]["sessionID"] == json!(session_id)
                && (event["type"] == json!("message.part.updated")
                    || (event["type"] == json!("session.status")
                        && event["properties"]["status"]["type"] == json!("idle")))
        })
    })
    .await;
    let _ = sess
        .api
        .request(
            reqwest::Method::POST,
            &format!("/session/{}/abort", sess.session_id),
            Some(json!({})),
        )
        .await;
    pump(&sess, |_, _| None).await;

    let resumed = sess
        .api
        .prompt(
            &sess.session_id,
            wire_prompt_body(backend, "thanks, reply with the word done"),
        )
        .await;
    assert!(
        !WireSession::text_of(&resumed).trim().is_empty(),
        "no resumed text\n{resumed}"
    );
}

/// B6 — structured output over the wire: the `StructuredOutput` tool
/// capture lands the schema payload on the final assistant message.
pub async fn b6_structured_output(backend: &impl LlmBackend) {
    let sess = start_wire(backend, B6_STRUCTURED_OUTPUT, |_| {}).await;
    drive(&sess, structured_prompt_body(backend), |_, _| None).await;

    let messages = sess.api.messages(&sess.session_id).await;
    let assistant = messages
        .iter()
        .rev()
        .find(|message| message["info"]["role"] == json!("assistant"))
        .expect("assistant message");
    let structured = &assistant["info"]["structured"];
    assert!(structured.is_object(), "no structured payload\n{assistant}");
    assert!(structured["answer"].is_number(), "{assistant}");
}

/// B7 — session export round-trip over a live session: `export latest`,
/// `import` and re-export must be byte-equal (a storage property, so it
/// holds for any transcript the model produced).
pub fn b7_session_export_round_trip(backend: &impl LlmBackend) {
    let run = run_scenario(backend, B7_EXPORT_ROUND_TRIP, "say hi", |_| {});
    run.assert_exit_zero();

    let env = &run._env;
    let export = env.run(&["export"]);
    assert_eq!(export.code, Some(0), "stderr={}", export.stderr);
    assert!(
        export.stderr.contains("Exporting session: latest"),
        "stderr={}",
        export.stderr
    );
    assert!(
        export.stdout.contains("\"info\""),
        "stdout={}",
        export.stdout
    );
    let file = env.home.join("session.json");
    std::fs::write(&file, export.stdout.trim_end()).expect("write export");
    let import = env.run(&["import", file.to_str().expect("export path")]);
    assert_eq!(import.code, Some(0), "stderr={}", import.stderr);
    let session_id = import
        .stdout
        .trim()
        .strip_prefix("Imported session: ")
        .expect("imported marker")
        .to_string();
    let reexport = env.run(&["export", &session_id]);
    assert_eq!(reexport.code, Some(0), "stderr={}", reexport.stderr);
    assert_eq!(
        reexport.stdout.trim_end(),
        export.stdout.trim_end(),
        "re-export differs"
    );
}
