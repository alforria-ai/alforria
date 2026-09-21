//! ACP end-to-end suite (spec ACP §14.2): drives the built `opencode`
//! binary's `acp` command through the real JSON-RPC/ndjson wire
//! protocol, backed by the scripted mock LLM.

#[path = "e2e_agent/backend.rs"]
#[allow(dead_code)]
mod backend;
#[path = "e2e_agent/harness.rs"]
#[allow(dead_code)]
mod harness;
#[path = "e2e_agent/scenarios.rs"]
#[allow(dead_code)]
mod scenarios;
#[path = "e2e_agent/transcript.rs"]
#[allow(dead_code)]
mod transcript;
#[path = "e2e_agent/wire.rs"]
#[allow(dead_code)]
mod wire;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use backend::LlmBackend;
use harness::Env;
use scenarios::A1_FILE_MUTATION;

/// The `opencode acp` child speaking JSON-RPC over piped stdio.
struct AcpChild {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    notifications: Arc<Mutex<Vec<Value>>>,
    next_id: u64,
}

impl AcpChild {
    fn spawn(env: &Env) -> AcpChild {
        let mut command = Command::new(env!("CARGO_BIN_EXE_opencode"));
        command
            .args(["acp", "--port", "0"])
            .env("OPENCODE_TEST_HOME", &env.home)
            .env("PWD", env.project_dir())
            .env("XDG_CONFIG_HOME", env.home.join(".config"))
            .env("XDG_DATA_HOME", env.home.join(".local/share"))
            .env("XDG_CACHE_HOME", env.home.join(".cache"))
            .env("XDG_STATE_HOME", env.home.join(".local/state"))
            .env("OPENCODE_MODELS_PATH", &env.models_path)
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
            .env_remove("OPENCODE_SERVER_PASSWORD")
            .current_dir(env.project_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().expect("spawn opencode acp");
        let stdin = child.stdin.take().expect("acp stdin");
        let stdout = child.stdout.take().expect("acp stdout");
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // A closed channel means the test is done — the
                // reader must not block process exit.
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        AcpChild {
            child,
            stdin,
            lines: rx,
            notifications: Arc::new(Mutex::new(Vec::new())),
            next_id: 1,
        }
    }

    /// Send a request; collect notifications until its response lands.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            })
            .to_string(),
        );
        self.wait_for_response(id, method)
    }

    fn must(&mut self, method: &str, params: Value) -> Value {
        self.request(method, params)
            .unwrap_or_else(|error| panic!("{method} failed: {error}"))
    }

    fn send(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write line");
    }

    fn wait_for_response(&mut self, id: u64, method: &str) -> Result<Value, String> {
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let line = match self.lines.recv_timeout(Duration::from_millis(200)) {
                Ok(line) => line,
                Err(_) if Instant::now() < deadline => continue,
                Err(_) => panic!("{method} timed out"),
            };
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|err| panic!("bad acp line {line}: {err}"));
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(error.to_string());
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            if let Some(method) = message.get("method").and_then(Value::as_str) {
                self.notifications.lock().unwrap().push(json!({
                    "method": method,
                    "params": message.get("params").cloned().unwrap_or(Value::Null),
                }));
            }
        }
    }

    /// Collect notifications until the stream goes quiet for
    /// `seconds`.
    fn drain_notifications(&mut self, seconds: u64) {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            let line = self.lines.recv_timeout(Duration::from_millis(200)).ok();
            let line = match line {
                Some(line) => line,
                None if Instant::now() < deadline => continue,
                None => break,
            };
            let message: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            if let Some(method) = message.get("method").and_then(Value::as_str) {
                self.notifications.lock().unwrap().push(json!({
                    "method": method,
                    "params": message.get("params").cloned().unwrap_or(Value::Null),
                }));
            }
            // keep pumping past the deadline while lines keep coming
            if Instant::now() > deadline + Duration::from_secs(1) {
                break;
            }
        }
    }

    fn session_updates(&self) -> Vec<Value> {
        self.notifications
            .lock()
            .unwrap()
            .iter()
            .filter(|notification| notification["method"] == json!("session/update"))
            .map(|notification| notification["params"]["update"].clone())
            .filter(|update| !update.is_null())
            .collect()
    }
}

impl Drop for AcpChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn acp_end_to_end_flow() {
    let backend = backend::MockBackend::new(A1_FILE_MUTATION);
    let env = Env::new("acp_e2e");
    env.write_provider_config(
        &backend.base_url(),
        "acp_e2e",
        &backend.api_key(),
        backend.provider_id(),
        backend.model_id(),
        backend.context_limit(),
    );
    let project = env.project_dir();
    std::fs::create_dir_all(&project).expect("create project");

    let mut acp = AcpChild::spawn(&env);

    // initialize — the static handshake.
    let initialized = acp.must(
        "initialize",
        json!({ "protocolVersion": 1, "clientCapabilities": {} }),
    );
    assert_eq!(initialized["protocolVersion"], json!(1));
    assert_eq!(initialized["agentInfo"]["name"], json!("OpenCode"));
    assert!(
        initialized["agentCapabilities"]["sessionCapabilities"]["fork"] == json!({}),
        "session capabilities: {initialized}"
    );

    // authenticate — the opencode-login method id.
    let _ = acp.must("authenticate", json!({ "methodId": "opencode-login" }));

    // new_session — creates a backing session.
    let session = acp.must("session/new", json!({ "cwd": project, "mcpServers": [] }));
    let session_id = session["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();
    assert!(!session_id.is_empty());
    let model_option = session["configOptions"]
        .as_array()
        .and_then(|options| options.iter().find(|option| option["id"] == "model"))
        .cloned()
        .expect("model config option");
    assert!(
        model_option["currentValue"]
            .as_str()
            .unwrap_or_default()
            .starts_with("mock/"),
        "model select option: {model_option}"
    );

    // prompt — the full tool round-trip over the wire.
    let response = acp.must(
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [ { "type": "text", "text": "create notes.md for me" } ],
        }),
    );
    assert_eq!(response["stopReason"], json!("end_turn"), "{response}");
    assert!(
        response["usage"]["totalTokens"].as_f64().is_some(),
        "usage in the response: {response}"
    );
    acp.drain_notifications(3);

    let updates = acp.session_updates();
    let kinds = |kind: &str| {
        updates
            .iter()
            .filter(|update| update["sessionUpdate"] == json!(kind))
            .count()
    };
    assert!(kinds("tool_call") >= 1, "no tool_call update: {updates:?}");
    assert!(
        kinds("tool_call_update") >= 1,
        "no tool_call_update update: {updates:?}"
    );
    assert!(
        kinds("agent_message_chunk") >= 1,
        "no agent_message_chunk update: {updates:?}"
    );
    assert!(kinds("usage_update") >= 1, "no usage_update: {updates:?}");
    assert!(
        kinds("available_commands_update") >= 1,
        "no available_commands_update: {updates:?}"
    );
    let tool_call = updates
        .iter()
        .find(|update| update["sessionUpdate"] == json!("tool_call"))
        .cloned()
        .expect("tool_call update");
    assert_eq!(tool_call["kind"], json!("edit"), "{tool_call}");
    // The pending state starts with an empty input; the running update
    // resolves the write location (tool.ts toLocations).
    let located = updates
        .iter()
        .find(|update| {
            update["sessionUpdate"] == json!("tool_call_update")
                && update["status"] == json!("in_progress")
        })
        .cloned()
        .expect("running tool_call_update");
    assert!(
        located["locations"]
            .as_array()
            .map(|locations| !locations.is_empty())
            .unwrap_or_default(),
        "write tool carries a file location: {located}"
    );

    // list_sessions — the created session is present.
    let listed = acp.must("session/list", json!({ "cwd": project }));
    let session_ids: Vec<&str> = listed["sessions"]
        .as_array()
        .map(|sessions| {
            sessions
                .iter()
                .filter_map(|session| session["sessionId"].as_str())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        session_ids.contains(&session_id.as_str()),
        "session list must contain {session_id}: {listed}"
    );

    // set_mode — the build mode from the snapshot.
    let modes: Vec<&str> = session["configOptions"]
        .as_array()
        .and_then(|options| {
            options
                .iter()
                .find(|option| option["id"] == "mode")
                .and_then(|mode| {
                    mode["options"]
                        .as_array()
                        .map(|opts| opts.iter().filter_map(|o| o["value"].as_str()).collect())
                })
        })
        .unwrap_or_default();
    if let Some(mode) = modes.first() {
        let _ = acp.must(
            "session/set_mode",
            json!({ "sessionId": session_id, "modeId": mode }),
        );
    }

    // set_model — re-selecting the current model.
    let _ = acp.must(
        "session/set_model",
        json!({
            "sessionId": session_id,
            "modelId": format!("{}/{}", backend.provider_id(), backend.model_id())
        }),
    );
    acp.drain_notifications(1);

    // close_session — removes it.
    let _ = acp.must("session/close", json!({ "sessionId": session_id }));
    acp.drain_notifications(1);
}
