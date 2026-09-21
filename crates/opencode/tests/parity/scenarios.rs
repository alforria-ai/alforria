//! Parity scenario drivers (spec PARITY §5). One scenario function runs
//! against one binary + one fresh `MockBackend`; the parity test runs it
//! twice (Rust binary, TS reference) and diffs the normalized captures.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::backend::{LibertaiBackend, LiveModel, LlmBackend, MockBackend};
use crate::harness::{Env, Serve};
use crate::parity_harness::{TsServe, TsSource};
use crate::transcript::Transcript;
use crate::wire::{pump_until, Api, EventLog};

/// The e2e fixture both sides consume (mock transcript + driving script).
pub const A1_FILE_MUTATION: &str = "a1_file_mutation";

/// Shared `e2e_agent` seams this harness does not drive: the CLI `run`
/// path (parity drives the HTTP wire), the live backend, and wire helpers
/// whose assertions belong to the e2e suites (spec PARITY §1:
/// deterministic-mock only). Referenced so the reused module stays fully
/// linked under `-D warnings`.
fn e2e_agent_seams() {
    let _ = (
        Env::run,
        Env::run_with,
        Env::run_with_timeout,
        crate::harness::wait,
        Transcript::turn_text,
        LibertaiBackend::new,
        LiveModel::Cheap,
        LiveModel::QualityGlm,
        LiveModel::QualityDeepseek,
        crate::wire::Api::create_session,
        crate::wire::Api::prompt,
        crate::wire::Api::permission_respond,
        crate::wire::Api::abort,
        crate::wire::Api::revert,
        crate::wire::Api::unrevert,
        crate::wire::Api::session_list,
    );
}

/// One side's raw capture of the A1 file-mutation scenario.
pub struct Capture {
    /// `POST /session` response body.
    pub session: Value,
    /// `GET /session/{id}/message` message store.
    pub store: Value,
    /// The legacy `/event` SSE stream (envelope per event).
    pub events: Vec<Value>,
    /// The request bodies recorded by the mock backend, in arrival order.
    pub requests: Vec<Value>,
    /// The `notes.md` bytes after the run.
    pub file: Option<String>,
    /// The project directory (the N4 normalization root).
    pub project: String,
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

fn prompt_body() -> Value {
    json!({
        "parts": [{"type": "text", "text": "create notes.md for me"}],
        "model": {"providerID": "mock", "modelID": "mock-model"},
    })
}

/// The A1 driver against whichever server listens on `port`.
async fn drive_a1(backend: &MockBackend, env: &Env, port: u16, log: &EventLog) -> Capture {
    assert!(!backend.live(), "parity drives mock backends only");
    let directory = env.project_dir().display().to_string();
    let api = Api::new(port, &directory);

    let response = api
        .request(reqwest::Method::POST, "/session", Some(json!({})))
        .await;
    assert!(
        response.status().is_success(),
        "session create failed: {}",
        response.status()
    );
    let session: Value = response.json().await.expect("session body");
    let session_id = session["id"].as_str().expect("session id").to_string();

    api.prompt_async(&session_id, prompt_body()).await;
    pump_until(log, backend.turn_timeout() * 4, |events| {
        events
            .iter()
            .any(|event| event["properties"]["sessionID"] == json!(session_id))
    })
    .await;
    pump_until_idle(backend, log, &session_id).await;
    // Drain the quiescent stream so late control-plane events (session
    // title, status) settle before the capture.
    let events = drain(log).await;

    let store = api.messages(&session_id).await;
    Capture {
        session,
        store: json!(store),
        events,
        requests: Vec::new(),
        file: std::fs::read_to_string(env.project_dir().join("notes.md")).ok(),
        project: directory,
    }
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

/// A1 against the Rust binary (`CARGO_BIN_EXE_opencode serve`).
pub async fn a1_rust(backend: &MockBackend) -> Capture {
    e2e_agent_seams();
    let env = parity_env(backend, "p1-rust");
    let serve = Serve::spawn(&env);
    let directory = env.project_dir().display().to_string();
    let log = EventLog::subscribe(&serve, &directory);
    let mut capture = drive_a1(backend, &env, serve.port, &log).await;
    capture.requests = backend.requests();
    capture
}

/// A1 against the TS reference (`bun … serve` at the pinned commit).
pub async fn a1_ts(backend: &MockBackend, ts: &TsSource) -> Capture {
    let env = parity_env(backend, "p1-ts");
    let serve = TsServe::spawn(ts, &env);
    let directory = env.project_dir().display().to_string();
    let log = EventLog::subscribe_at(serve.port, &directory);
    let mut capture = drive_a1(backend, &env, serve.port, &log).await;
    capture.requests = backend.requests();
    capture
}
