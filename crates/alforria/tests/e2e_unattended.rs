//! Unattended-server e2e: the built `alforria serve` under a supervisor's
//! signals, driven over HTTP against the scripted LLM backend.
#![allow(dead_code)]
#![cfg(unix)]

#[path = "e2e_agent/backend.rs"]
mod backend;
#[path = "e2e_agent/harness.rs"]
mod harness;
#[path = "e2e_agent/transcript.rs"]
mod transcript;
#[path = "e2e_agent/wire.rs"]
mod wire;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use backend::{LlmBackend, MockBackend};
use harness::{Env, Serve};
use wire::{pump_until, Api, EventLog};

const BUDGET: Duration = Duration::from_secs(60);

fn env_for(backend: &MockBackend, scenario: &str) -> Env {
    let env = Env::new(scenario);
    env.write_provider_config(
        &backend.base_url(),
        scenario,
        &backend.api_key(),
        backend.provider_id(),
        backend.model_id(),
        backend.context_limit(),
    );
    env
}

/// The host config: `<XDG_CONFIG_HOME>/alforria/opencode.json`.
fn write_host_config(env: &Env, config: Value) {
    let dir = env.home.join(".config/alforria");
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(
        dir.join("opencode.json"),
        serde_json::to_string_pretty(&config).expect("serialize"),
    )
    .expect("write host config");
}

fn prompt_body(backend: &MockBackend, text: &str) -> Value {
    json!({
        "parts": [{"type": "text", "text": text}],
        "model": {"providerID": backend.provider_id(), "modelID": backend.model_id()},
    })
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + BUDGET;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn tool_pid(project: &Path) -> i32 {
    wait_for("the tool process to start", || {
        std::fs::read_to_string(project.join("tool.pid"))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
    })
}

fn wal_path(env: &Env) -> PathBuf {
    env.home.join(".local/share/alforria/opencode.db-wal")
}

struct Running {
    env: Env,
    serve: Serve,
    api: Api,
    log: EventLog,
    session_id: String,
    project: PathBuf,
}

async fn start(backend: &MockBackend, env: Env) -> Running {
    start_with(backend, env, |_| {}).await
}

async fn start_with(
    backend: &MockBackend,
    env: Env,
    configure: impl FnOnce(&mut std::process::Command),
) -> Running {
    let project = env.project_dir();
    let serve = Serve::spawn_with(&env, configure);
    let directory = project.display().to_string();
    let api = Api::new(serve.port, &directory);
    let log = EventLog::subscribe(&serve, &directory);
    let session_id = api.create_session().await;
    api.prompt_async(&session_id, prompt_body(backend, "go"))
        .await;
    Running {
        env,
        serve,
        api,
        log,
        session_id,
        project,
    }
}

/// SIGTERM while a bash tool call is running: the server exits 0 well inside
/// its deadline, the tool's process group is gone, the write-ahead log is
/// empty, and a restarted server reads the interrupted turn back.
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_mid_turn_stops_cleanly_and_kills_the_tool() {
    let backend = MockBackend::new("u1_shutdown_mid_tool");
    let env = env_for(&backend, "u1");
    let mut run = start(&backend, env).await;
    let pid = tool_pid(&run.project);
    assert!(alive(pid), "the tool process is running");

    let stopped = Instant::now();
    run.serve.signal(libc::SIGTERM);
    let status = run
        .serve
        .wait_exit(Duration::from_secs(20))
        .expect("serve exits after SIGTERM");
    assert_eq!(status.code(), Some(0), "a clean stop exits 0");
    assert!(
        stopped.elapsed() < Duration::from_secs(10),
        "stopped in {:?}, inside the default deadline",
        stopped.elapsed()
    );
    wait_for("the tool process to die", || (!alive(pid)).then_some(()));

    let wal = wal_path(&run.env);
    let wal_len = std::fs::metadata(&wal).map(|meta| meta.len()).unwrap_or(0);
    assert_eq!(wal_len, 0, "the WAL was checkpointed into the database");

    // The same data directory serves again, with the interrupted turn in it.
    let serve = Serve::spawn(&run.env);
    let api = Api::new(serve.port, run.project.display().to_string());
    let messages = api.messages(&run.session_id).await;
    let assistant = messages
        .iter()
        .rfind(|message| message["info"]["role"] == json!("assistant"))
        .unwrap_or_else(|| panic!("no assistant message: {messages:?}"));
    assert_eq!(
        assistant["info"]["error"]["name"],
        json!("MessageAbortedError"),
        "the turn was recorded as interrupted: {assistant}"
    );
    assert!(assistant["info"]["time"]["completed"].is_number());
}

/// SIGINT is handled the same way as SIGTERM.
#[tokio::test(flavor = "multi_thread")]
async fn sigint_stops_cleanly_too() {
    let backend = MockBackend::new("u1_shutdown_mid_tool");
    let env = env_for(&backend, "u1-int");
    let mut run = start(&backend, env).await;
    let pid = tool_pid(&run.project);

    run.serve.signal(libc::SIGINT);
    let status = run
        .serve
        .wait_exit(Duration::from_secs(20))
        .expect("serve exits after SIGINT");
    assert_eq!(status.code(), Some(0));
    wait_for("the tool process to die", || (!alive(pid)).then_some(()));
}

/// `server.shutdown.sessions: "drain"` lets a running turn finish before
/// the server exits.
#[tokio::test(flavor = "multi_thread")]
async fn draining_shutdown_lets_the_turn_finish() {
    let backend = MockBackend::new("u2_shutdown_drain");
    let env = env_for(&backend, "u2");
    write_host_config(
        &env,
        json!({ "server": { "shutdown": { "timeout": "30s", "sessions": "drain" } } }),
    );
    let mut run = start(&backend, env).await;
    let session_id = run.session_id.clone();
    let _ = pump_until(&run.log, BUDGET, |events| {
        events.iter().any(|event| {
            event["type"] == json!("message.part.updated")
                && event["properties"]["sessionID"] == json!(session_id)
                && event["properties"]["part"]["type"] == json!("text")
        })
    })
    .await;

    run.serve.signal(libc::SIGTERM);
    let status = run
        .serve
        .wait_exit(Duration::from_secs(40))
        .expect("serve exits once the turn is done");
    assert_eq!(status.code(), Some(0));

    let serve = Serve::spawn(&run.env);
    let api = Api::new(serve.port, run.project.display().to_string());
    let messages = api.messages(&run.session_id).await;
    let assistant = messages
        .iter()
        .rfind(|message| message["info"]["role"] == json!("assistant"))
        .unwrap_or_else(|| panic!("no assistant message: {messages:?}"));
    assert!(
        assistant["info"]["error"].is_null(),
        "the turn was not interrupted: {assistant}"
    );
    let text: String = assistant["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == json!("text"))
        .filter_map(|part| part["text"].as_str())
        .collect();
    assert_eq!(text, "almost done");
}

/// A hard kill mid-turn loses nothing that was committed: the next server
/// opens the same database and lists the session.
#[tokio::test(flavor = "multi_thread")]
async fn a_hard_kill_leaves_a_usable_database() {
    let backend = MockBackend::new("u1_shutdown_mid_tool");
    let env = env_for(&backend, "u1-kill");
    let mut run = start(&backend, env).await;
    let pid = tool_pid(&run.project);

    run.serve.signal(libc::SIGKILL);
    let status = run
        .serve
        .wait_exit(Duration::from_secs(20))
        .expect("serve dies");
    assert_ne!(status.code(), Some(0));
    // Nothing ran to clean up: the orphaned tool is ours to remove.
    unsafe { libc::kill(-pid, libc::SIGKILL) };

    let serve = Serve::spawn(&run.env);
    let api = Api::new(serve.port, run.project.display().to_string());
    let sessions = api.session_list().await;
    assert!(
        sessions
            .iter()
            .any(|session| session["id"] == json!(run.session_id)),
        "{sessions:?}"
    );
    let messages = api.messages(&run.session_id).await;
    assert!(messages
        .iter()
        .any(|message| message["info"]["role"] == json!("user")));
}

/// A host config that does not parse stops `serve` from listening: the
/// access policy in force would not be the one written down.
#[test]
fn serve_refuses_a_broken_host_config() {
    let env = Env::new("broken-host-config");
    let dir = env.home.join(".config/alforria");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("opencode.json"), "{ not json").unwrap();
    let mut command = env.command(&["serve", "--port", "0"]);
    command.env("OPENCODE_SERVER_PASSWORD", "");
    let output = command.output().expect("run serve");
    assert_ne!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("host config could not be applied"),
        "stderr={stderr}"
    );
}

fn read_env_dump(path: &Path) -> String {
    wait_for(&format!("{}", path.display()), || {
        std::fs::read_to_string(path)
            .ok()
            .filter(|text| text.contains("PATH="))
    })
}

/// The agent's shells do not see the server's secrets: the bash tool, the
/// session `shell` command and a terminal all start from the `shell_env`
/// policy, which withholds the built-in deny list (provider keys, the
/// server's own secrets) and whatever the host config denies.
#[tokio::test(flavor = "multi_thread")]
async fn the_agents_shells_cannot_read_denied_variables() {
    let backend = MockBackend::new("u3_shell_env");
    let env = env_for(&backend, "u3");
    write_host_config(
        &env,
        json!({ "shell_env": { "deny": ["U3_DENIED_*"], "set": { "U3_FROM_CONFIG": "yes" } } }),
    );
    let run = start_with(&backend, env, |command| {
        command
            .env("LIBERTAI_API_KEY", "sk-libertai-leak")
            .env("OPENCODE_AUTH_CONTENT", "{\"leak\":\"auth-content-leak\"}")
            .env("U3_DENIED_TOKEN", "denied-leak")
            .env("U3_VISIBLE", "visible");
    })
    .await;

    let check = |dump: &str, what: &str| {
        for leak in ["sk-libertai-leak", "auth-content-leak", "denied-leak"] {
            assert!(!dump.contains(leak), "{what} leaked {leak}:\n{dump}");
        }
        assert!(dump.contains("U3_VISIBLE=visible"), "{what}:\n{dump}");
        assert!(dump.contains("U3_FROM_CONFIG=yes"), "{what}:\n{dump}");
    };

    // The bash tool, driven by the model.
    check(
        &read_env_dump(&run.project.join("bash-env.txt")),
        "bash tool",
    );

    // The session shell command.
    let session_id = run.session_id.clone();
    let _ = pump_until(&run.log, BUDGET, |events| {
        events.iter().any(|event| {
            event["type"] == json!("session.idle")
                && event["properties"]["sessionID"] == json!(session_id)
        })
    })
    .await;
    let response = run
        .api
        .request(
            reqwest::Method::POST,
            &format!("/session/{}/shell", run.session_id),
            Some(json!({ "agent": "build", "command": "env > shell-env.txt" })),
        )
        .await;
    assert!(response.status().is_success(), "{}", response.status());
    check(
        &read_env_dump(&run.project.join("shell-env.txt")),
        "session shell",
    );

    // A terminal.
    let response = run
        .api
        .request(
            reqwest::Method::POST,
            "/pty",
            Some(json!({ "command": "sh", "args": ["-c", "env > pty-env.txt"] })),
        )
        .await;
    assert!(response.status().is_success(), "{}", response.status());
    check(&read_env_dump(&run.project.join("pty-env.txt")), "terminal");
}
