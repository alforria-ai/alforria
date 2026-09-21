//! Parity scenario drivers (spec PARITY §5). One scenario function runs
//! against one binary + one fresh `MockBackend`; the parity test runs it
//! twice (Rust binary, TS reference) and diffs the normalized captures.
//!
//! Chunk 2 generalizes the driver to a per-scenario operation list: each
//! driver returns named HTTP response captures in driving order, and the
//! runner records the mock-LLM request bodies and the legacy `/event`
//! SSE stream alongside them (spec PARITY §2.3).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::backend::{LibertaiBackend, LiveModel, LlmBackend, MockBackend};
use crate::harness::{Env, ProcOutput, Serve};
use crate::parity_harness::{golden_mode, record_mode, TsServe, TsSource};
use crate::transcript::Transcript;
use crate::wire::{pump_until, Api, EventLog};

/// The e2e fixtures both sides consume (mock transcripts + prompts).
pub const A1_FILE_MUTATION: &str = "a1_file_mutation";
pub const A2_MULTI_STEP: &str = "a2_multi_step";
pub const A3_PERMISSION_GATE: &str = "a3_permission_gate";
pub const A4_SUBAGENT: &str = "a4_subagent";
pub const A6_COMPACTION: &str = "a6_compaction";
pub const A7_REVERT: &str = "a7_revert";
pub const A8_CANCEL_MID_STREAM: &str = "a8_cancel_mid_stream";
pub const A9_STRUCTURED_OUTPUT: &str = "a9_structured_output";

/// Named HTTP captures produced by one scenario driver, in driving order.
/// The order is the normalization order — it must be identical on both
/// sides for the id/timestamp counters to align.
pub type Named = Vec<(&'static str, Value)>;

type DriveFuture<'a> = Pin<Box<dyn Future<Output = Named> + 'a>>;

/// The side-specific CLI runner (`opencode export …` / `bun … export …`).
pub struct Cli<'a> {
    pub run: CliRunner<'a>,
}

/// A boxed CLI runner: invokes the side's binary with `opencode`-style
/// args and returns its combined output.
pub type CliRunner<'a> = Box<dyn Fn(&[&str]) -> ProcOutput + 'a>;

/// Shared `e2e_agent` seams this harness does not drive: the CLI `run`
/// path extras, the live backend, and wire/backend helpers whose
/// assertions belong to the e2e suites (spec PARITY §1:
/// deterministic-mock only). Referenced so the reused module stays fully
/// linked under `-D warnings`.
fn e2e_agent_seams() {
    let _ = (
        Env::run_with,
        Env::run_with_timeout,
        crate::harness::wait,
        Transcript::turn_text,
        LibertaiBackend::new,
        LiveModel::Cheap,
        LiveModel::QualityGlm,
        LiveModel::QualityDeepseek,
        crate::wire::Api::abort,
        crate::wire::Api::create_session,
        <MockBackend as LlmBackend>::transcript as fn(&MockBackend, &str) -> Option<Transcript>,
        <MockBackend as LlmBackend>::live as fn(&MockBackend) -> bool,
        MockBackend::request as fn(&MockBackend, usize) -> Value,
    );
}

/// One side's raw capture of a parity scenario.
pub struct Capture {
    /// The named HTTP response bodies, in driving order (N4 root).
    pub named: Named,
    /// The request bodies recorded by the mock backend, in arrival order.
    pub requests: Vec<Value>,
    /// The legacy `/event` SSE stream (envelope per event).
    pub events: Vec<Value>,
    /// The V2 `/api/event` SSE stream. TS `opencode serve` does not mount
    /// the V2 event group (it lives in the embedded `packages/server`
    /// web server, `protocol/src/groups/event.ts`), so this stays empty
    /// on the TS side and the V2 envelope is checked on the Rust capture.
    pub v2_events: Vec<Value>,
    /// The project directory (the N4 normalization root).
    pub project: String,
}

/// The serialized form of [`Capture`] stored in the golden files (spec
/// PARITY §7). Raw captures, not normalized ones: normalization is a
/// pure function of the capture, so replay-time normalization is
/// byte-identical to record-time, and the raw artifacts keep the
/// scenario-specific assertions working in golden mode.
#[derive(Serialize, Deserialize)]
struct GoldenCapture {
    named: Vec<(String, Value)>,
    requests: Vec<Value>,
    events: Vec<Value>,
    v2_events: Vec<Value>,
    project: String,
}

impl Capture {
    fn to_golden(&self) -> GoldenCapture {
        GoldenCapture {
            named: self
                .named
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect(),
            requests: self.requests.clone(),
            events: self.events.clone(),
            v2_events: self.v2_events.clone(),
            project: self.project.clone(),
        }
    }

    fn from_golden(golden: GoldenCapture) -> Capture {
        Capture {
            named: golden
                .named
                .into_iter()
                .map(|(key, value)| {
                    let key: &'static str = Box::leak(key.into_boxed_str());
                    (key, value)
                })
                .collect(),
            requests: golden.requests,
            events: golden.events,
            v2_events: golden.v2_events,
            project: golden.project,
        }
    }
}

/// The committed golden file of one scenario's TS capture.
fn golden_path(check: &str) -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/parity/golden/ts"))
        .join(format!("{check}.json"))
}

fn store_golden(check: &str, capture: &Capture) {
    let path = golden_path(check);
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&capture.to_golden()).expect("serialize golden"),
    )
    .expect("write golden");
}

fn load_golden(check: &str) -> Capture {
    let path = golden_path(check);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "golden capture {path:?} missing ({err}) — record it with \
             OPENCODE_PARITY_RECORD=1 on a machine with the pinned TS clone"
        )
    });
    Capture::from_golden(serde_json::from_str(&text).expect("golden capture body"))
}

/// Fresh isolated env wired at the given mock-LLM base URL.
fn parity_env(backend: &MockBackend, tag: &str) -> Env {
    let env = Env::new(tag);
    env.write_provider_config(
        &backend.base_url(),
        tag,
        &backend.api_key(),
        backend.provider_id(),
        backend.model_id(),
        backend.context_limit(),
    );
    env
}

fn prompt_body(text: &str) -> Value {
    json!({
        "parts": [{"type": "text", "text": text}],
        "model": {"providerID": "mock", "modelID": "mock-model"},
    })
}

/// `POST /session` — the response body.
async fn create_session(api: &Api) -> Value {
    let response = api
        .request(reqwest::Method::POST, "/session", Some(json!({})))
        .await;
    assert!(
        response.status().is_success(),
        "session create failed: {}",
        response.status()
    );
    response.json().await.expect("session body")
}

fn session_id(session: &Value) -> String {
    session["id"].as_str().expect("session id").to_string()
}

/// Pump the SSE stream until the session goes busy → idle.
async fn pump_until_idle(backend: &MockBackend, log: &EventLog, session_id: &str) {
    let deadline = Instant::now() + backend.turn_timeout() * 4;
    let mut busy = false;
    let mut cursor = 0;
    loop {
        let events = log.events();
        for event in &events[cursor..] {
            if event["properties"]["sessionID"] != json!(session_id) {
                continue;
            }
            if event["type"] == json!("session.status") {
                match event["properties"]["status"]["type"].as_str() {
                    Some("busy") => busy = true,
                    Some("idle") if busy => return,
                    _ => {}
                }
            }
        }
        cursor = events.len();
        assert!(
            Instant::now() < deadline,
            "session never went idle\n{}",
            serde_json::to_string_pretty(&events).expect("events")
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Pump the SSE stream while replying to every `permission.asked` through
/// `policy` (ask index is 1-based) until the session goes idle. Returns
/// the ask payloads, in order.
async fn pump_asks(
    backend: &MockBackend,
    api: &Api,
    log: &EventLog,
    session_id: &str,
    mut policy: impl FnMut(usize) -> Option<&'static str>,
) -> Vec<Value> {
    let mut asks: Vec<Value> = Vec::new();
    let mut cursor = 0;
    let mut busy = false;
    let deadline = Instant::now() + backend.turn_timeout() * 4;
    loop {
        let events = log.events();
        let mut done = false;
        for event in &events[cursor..] {
            let properties = &event["properties"];
            if properties["sessionID"] != json!(session_id) {
                continue;
            }
            match event["type"].as_str().unwrap_or_default() {
                "permission.asked" => {
                    asks.push(properties.clone());
                    if let Some(reply) = policy(asks.len()) {
                        api.permission_respond(
                            session_id,
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

/// Bounded quiescence drain: pump until no new events arrive for a beat.
async fn drain(log: &EventLog) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut quiet = Instant::now();
    loop {
        let before = log.events().len();
        assert!(Instant::now() < deadline, "quiescence drain timed out");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let events = log.events();
        if events.len() == before {
            if quiet.elapsed() >= Duration::from_millis(500) {
                return events;
            }
        } else {
            quiet = Instant::now();
        }
    }
}

/// Assemble the capture once the driver is done: drain the streams so
/// late control-plane events settle before the capture.
async fn finish(
    env: &Env,
    log: &EventLog,
    v2_log: &EventLog,
    named: Named,
    requests: Vec<Value>,
) -> Capture {
    Capture {
        named,
        requests,
        events: drain(log).await,
        v2_events: drain(v2_log).await,
        project: env.project_dir().display().to_string(),
    }
}

/// Wait until a fresh subscription is actually connected — both binaries
/// emit `server.connected` as the first frame on connect, so awaiting it
/// closes the subscribe-before-drive race (no published event is lost).
async fn await_connected(log: &EventLog) {
    let _ = pump_until(log, Duration::from_secs(30), |events| {
        events
            .first()
            .is_some_and(|event| event["type"] == json!("server.connected"))
    })
    .await;
}

/// The Rust half of one dual run.
async fn run_rust<S, F>(tag: &str, fixture: &str, setup: &S, drive: &F) -> Capture
where
    S: Fn(&Path),
    F: for<'a> Fn(&'a MockBackend, &'a Env, u16, &'a EventLog, &'a Cli<'a>) -> DriveFuture<'a>,
{
    e2e_agent_seams();
    let backend = MockBackend::new(fixture);
    let env = parity_env(&backend, &format!("{tag}-rust"));
    setup(&env.project_dir());
    let serve = Serve::spawn(&env);
    let directory = env.project_dir().display().to_string();
    let v2_log = EventLog::subscribe_url(&format!("http://127.0.0.1:{}/api/event", serve.port));
    await_connected(&v2_log).await;
    let log = EventLog::subscribe(&serve, &directory);
    await_connected(&log).await;
    let cli = Cli {
        run: Box::new(|args| env.run(args)),
    };
    let named = drive(&backend, &env, serve.port, &log, &cli).await;
    finish(&env, &log, &v2_log, named, backend.requests()).await
}

/// The TS half of one dual run.
async fn run_ts<S, F>(tag: &str, fixture: &str, ts: &TsSource, setup: &S, drive: &F) -> Capture
where
    S: Fn(&Path),
    F: for<'a> Fn(&'a MockBackend, &'a Env, u16, &'a EventLog, &'a Cli<'a>) -> DriveFuture<'a>,
{
    let backend = MockBackend::new(fixture);
    let env = parity_env(&backend, &format!("{tag}-ts"));
    setup(&env.project_dir());
    let serve = TsServe::spawn(ts, &env);
    let directory = env.project_dir().display().to_string();
    let log = EventLog::subscribe_at(serve.port, &directory);
    await_connected(&log).await;
    // `opencode serve` has no `/api/event` route — see `Capture::v2_events`.
    let v2_log = EventLog::empty();
    let cli = Cli {
        run: Box::new(|args| crate::parity_harness::ts_cli(ts, &env, args)),
    };
    let named = drive(&backend, &env, serve.port, &log, &cli).await;
    finish(&env, &log, &v2_log, named, backend.requests()).await
}

/// Run one scenario against both binaries against a fresh project env +
/// mock backend per side. `check` is the scenario's report/golden key.
/// Golden mode (spec PARITY §7) replaces the live TS run with the
/// committed golden capture; record mode rewrites the goldens from a
/// live TS run.
pub async fn scenario<S, F>(check: &str, fixture: &str, setup: S, drive: F) -> (Capture, Capture)
where
    S: Fn(&Path),
    F: for<'a> Fn(&'a MockBackend, &'a Env, u16, &'a EventLog, &'a Cli<'a>) -> DriveFuture<'a>,
{
    assert!(
        !(golden_mode() && record_mode()),
        "OPENCODE_PARITY_GOLDEN and OPENCODE_PARITY_RECORD are mutually exclusive"
    );
    let rs = run_rust(check, fixture, &setup, &drive).await;
    if golden_mode() {
        let ts = load_golden(check);
        return (ts, rs);
    }
    let ts_source = TsSource::resolve();
    let ts = run_ts(check, fixture, &ts_source, &setup, &drive).await;
    if record_mode() {
        store_golden(check, &ts);
    }
    (ts, rs)
}

// ---------------------------------------------------------------------------
// Scenario drivers (spec PARITY §5). Each returns the named HTTP captures
// its parity assertion is diffed against.
// ---------------------------------------------------------------------------

/// The ordered part-type sequence of the assistant messages, with
/// consecutive parallel tool runs sorted (the e2e golden relaxation).
fn part_sequence(store: &[Value]) -> Value {
    let mut out: Vec<String> = Vec::new();
    let mut run: Vec<String> = Vec::new();
    for message in store {
        if message["info"]["role"] != json!("assistant") {
            continue;
        }
        for part in message["parts"]
            .as_array()
            .map(|p| p.to_vec())
            .unwrap_or_default()
        {
            let tag = if part["type"] == json!("tool") {
                format!("tool:{}", part["tool"].as_str().unwrap_or_default())
            } else {
                part["type"].as_str().unwrap_or_default().to_string()
            };
            if tag.starts_with("tool:") {
                run.push(tag);
            } else {
                run.sort();
                out.append(&mut run);
                out.push(tag);
            }
        }
    }
    run.sort();
    out.append(&mut run);
    json!(out)
}

/// P1 — file mutation round-trip (A1): the write tool lands the scripted
/// bytes and the final text answer closes the loop.
pub async fn drive_p1(backend: &MockBackend, env: &Env, port: u16, log: &EventLog) -> Named {
    let api = Api::new(port, env.project_dir().display().to_string());
    let session = create_session(&api).await;
    let id = session_id(&session);
    api.prompt_async(&id, prompt_body("create notes.md for me"))
        .await;
    pump_until_idle(backend, log, &id).await;
    let store = api.messages(&id).await;
    let file = std::fs::read_to_string(env.project_dir().join("notes.md")).ok();
    vec![
        ("session", session),
        ("store", json!(store)),
        ("part_sequence", part_sequence(&store)),
        ("file", json!(file)),
    ]
}

/// P2 — multi-step loop with two parallel `read` calls in the middle
/// step (A2): plus the request bodies the mock backend recorded (tool
/// results fed back).
pub async fn drive_p2(backend: &MockBackend, env: &Env, port: u16, log: &EventLog) -> Named {
    let api = Api::new(port, env.project_dir().display().to_string());
    let session = create_session(&api).await;
    let id = session_id(&session);
    api.prompt_async(&id, prompt_body("read the two files and answer"))
        .await;
    pump_until_idle(backend, log, &id).await;
    let store = api.messages(&id).await;
    vec![
        ("session", session),
        ("store", json!(store)),
        ("part_sequence", part_sequence(&store)),
    ]
}

/// P3 — the permission gate (A3): the reply policy mirrors the e2e
/// scenario: once → ask per call, always → one ask, reject → the tool
/// errors and the loop breaks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Once,
    Always,
    Reject,
}

pub async fn drive_p3(
    backend: &MockBackend,
    env: &Env,
    port: u16,
    log: &EventLog,
    reply: Reply,
) -> Named {
    let api = Api::new(port, env.project_dir().display().to_string());
    let session = create_session(&api).await;
    let id = session_id(&session);
    api.prompt_async(&id, prompt_body("read twice")).await;
    let asks = pump_asks(backend, &api, log, &id, |index| match (reply, index) {
        (Reply::Once, _) => Some("once"),
        (Reply::Always, 1) => Some("always"),
        (Reply::Reject, 1) => Some("reject"),
        _ => None,
    })
    .await;
    let store = api.messages(&id).await;
    vec![
        ("session", session),
        ("asks", json!(asks)),
        ("store", json!(store)),
        ("part_sequence", part_sequence(&store)),
    ]
}

/// P4 — the subagent over the wire (A4): a `task` tool call spawns a
/// child session whose answer flows back into the parent.
pub async fn drive_p4(backend: &MockBackend, env: &Env, port: u16, log: &EventLog) -> Named {
    let api = Api::new(port, env.project_dir().display().to_string());
    let session = create_session(&api).await;
    let id = session_id(&session);
    api.prompt_async(&id, prompt_body("spawn a subagent")).await;
    pump_until_idle(backend, log, &id).await;
    let store = api.messages(&id).await;
    let list = api.session_list().await;
    vec![
        ("session", session),
        ("store", json!(store)),
        ("session_list", json!(list)),
    ]
}

/// P5 — compaction over the wire (A6): usage overflow triggers the
/// compaction fork; the loop continues on the compacted history.
pub async fn drive_p5(backend: &MockBackend, env: &Env, port: u16, log: &EventLog) -> Named {
    let api = Api::new(port, env.project_dir().display().to_string());
    let session = create_session(&api).await;
    let id = session_id(&session);
    api.prompt_async(&id, prompt_body("trigger overflow")).await;
    pump_until_idle(backend, log, &id).await;
    let store = api.messages(&id).await;
    vec![
        ("session", session),
        ("store", json!(store)),
        ("part_sequence", part_sequence(&store)),
    ]
}

/// P6 — revert/unrevert over the wire (A7): a `write` tool edit rolls
/// back through the git snapshot restore, and the diff summary numbers
/// land on the session.
pub async fn drive_p6(backend: &MockBackend, env: &Env, port: u16, log: &EventLog) -> Named {
    let api = Api::new(port, env.project_dir().display().to_string());
    let session = create_session(&api).await;
    let id = session_id(&session);
    api.prompt_async(&id, prompt_body("edit the file")).await;
    let deadline = backend.turn_timeout() * 4;
    pump_until(log, deadline, |events| {
        events.iter().any(|event| {
            event["type"] == json!("message.part.updated")
                && event["properties"]["sessionID"] == json!(id)
                && event["properties"]["part"]["type"] == json!("step-start")
        })
    })
    .await;
    std::fs::write(env.project_dir().join("a.txt"), "v2\n").expect("seed edit");
    pump_until_idle(backend, log, &id).await;

    let store = api.messages(&id).await;
    let user_id = store
        .iter()
        .find(|message| message["info"]["role"] == json!("user"))
        .and_then(|message| message["info"]["id"].as_str())
        .expect("user message")
        .to_string();
    let revert = api.revert(&id, &user_id).await;
    let a_txt_reverted = std::fs::read_to_string(env.project_dir().join("a.txt")).expect("a.txt");
    let unrevert = api.unrevert(&id).await;
    let a_txt_unreverted = std::fs::read_to_string(env.project_dir().join("a.txt")).expect("a.txt");
    vec![
        ("session", session),
        ("store", json!(store)),
        ("revert", revert),
        ("a_txt_reverted", json!(a_txt_reverted)),
        ("unrevert", unrevert),
        ("a_txt_unreverted", json!(a_txt_unreverted)),
    ]
}

/// P7 — cancel mid-stream (A8): the transcript streams a tool call, then
/// stalls behind it; `POST /session/{id}/abort` interrupts the in-flight
/// turn (the pinned route inventory has no `/cancel` route — abort is
/// the wired twin of the spec's `POST /cancel`), and a re-prompt
/// continues on turn 2. The interrupted-part event sequence is the
/// strict SSE stream diff's subject.
pub async fn drive_p7(backend: &MockBackend, env: &Env, port: u16, log: &EventLog) -> Named {
    let api = Api::new(port, env.project_dir().display().to_string());
    std::fs::write(env.project_dir().join("a.txt"), "x\n").expect("seed a.txt");
    let session = create_session(&api).await;
    let id = session_id(&session);
    api.prompt_async(&id, prompt_body("go")).await;

    // The tool part is in flight (the transcript stalls behind it).
    pump_until(log, backend.turn_timeout(), |events| {
        events.iter().any(|event| {
            event["type"] == json!("message.part.updated")
                && event["properties"]["sessionID"] == json!(id)
                && event["properties"]["part"]["type"] == json!("tool")
                && event["properties"]["part"]["state"]["status"] == json!("pending")
        })
    })
    .await;
    api.abort(&id).await;

    // The interrupted assistant finalizes: abort error, completed time,
    // the in-flight tool part marked interrupted.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let messages = api.messages(&id).await;
        let finalized = messages.iter().rfind(|message| {
            message["info"]["role"] == json!("assistant")
                && message["info"]["error"].is_object()
                && message["info"]["time"]["completed"].is_number()
        });
        if finalized.is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "aborted assistant never finalized\n{messages:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let store = api.messages(&id).await;

    // The re-prompt continues the session on turn 2.
    let resumed = api.prompt(&id, prompt_body("go again")).await;
    vec![
        ("session", session),
        ("store", json!(store)),
        ("part_sequence", part_sequence(&store)),
        ("resumed", resumed),
    ]
}

/// P8 — structured output (A9): the prompt body carries a `format`
/// json_schema; the blocking prompt returns the final assistant message
/// with the captured `structured` payload. The prompt response carries
/// the parts (TS `GET /message` 400s for format sessions — Effect
/// response-encodes against the `Schema.Class` union and rejects plain
/// objects — so the driver uses the prompt response, not the store).
pub async fn drive_p8(_backend: &MockBackend, env: &Env, port: u16, _log: &EventLog) -> Named {
    let api = Api::new(port, env.project_dir().display().to_string());
    let session = create_session(&api).await;
    let id = session_id(&session);
    let mut body = prompt_body("answer in json");
    body["format"] = json!({
        "type": "json_schema",
        "schema": {
            "type": "object",
            "properties": { "answer": { "type": "number" } }
        }
    });
    let message = api.prompt(&id, body).await;
    vec![
        ("session", session),
        ("message", message.clone()),
        ("store", json!([message])),
    ]
}

/// P9 — session lifecycle listing (B7-shaped): session create → list →
/// CLI export/import round-trip response bodies.
pub async fn drive_p9(
    backend: &MockBackend,
    env: &Env,
    port: u16,
    log: &EventLog,
    cli: &Cli<'_>,
) -> Named {
    let api = Api::new(port, env.project_dir().display().to_string());
    let session = create_session(&api).await;
    let id = session_id(&session);
    api.prompt_async(&id, prompt_body("create notes.md for me"))
        .await;
    pump_until_idle(backend, log, &id).await;
    let store = api.messages(&id).await;
    let list = api.session_list().await;

    let export = (cli.run)(&["export", &id]);
    assert_eq!(
        export.code,
        Some(0),
        "export failed\nstderr={}",
        export.stderr
    );
    let exported = serde_json::from_str::<Value>(&export.stdout).expect("export body");

    let path = env.project_dir().join("session.json");
    std::fs::write(&path, export.stdout.trim_end()).expect("write export");
    let import = (cli.run)(&["import", &path.display().to_string()]);
    assert_eq!(
        import.code,
        Some(0),
        "import failed\nstderr={}",
        import.stderr
    );
    let imported = import
        .stdout
        .trim()
        .strip_prefix("Imported session: ")
        .expect("imported marker")
        .to_string();
    let reexport = (cli.run)(&["export", &imported]);
    assert_eq!(
        reexport.code,
        Some(0),
        "re-export failed\nstderr={}",
        reexport.stderr
    );
    let reexported = serde_json::from_str::<Value>(&reexport.stdout).expect("re-export body");
    vec![
        ("session", session),
        ("store", json!(store)),
        ("session_list", json!(list)),
        ("export", exported),
        ("import", json!(imported)),
        ("reexport", reexported),
    ]
}
