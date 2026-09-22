//! C9 CLI e2e suite (milestone acceptance): drives the built `opencode`
//! binary against a mock-LLM HTTP server.

#[path = "e2e_agent/harness.rs"]
mod harness;

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::{Duration, Instant};

use axum::routing::{any, post};
use axum::Router;

use harness::{Env, MockServer};

/// The mock-LLM SSE script. One text delta, then a stop frame with usage.
fn llm_sse(text: &str) -> String {
    let mut body = format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n");
    body.push_str(
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":6}}\n\n",
    );
    body.push_str("data: [DONE]\n\n");
    body
}

fn llm_server(text: &str) -> (MockServer, String) {
    let script = llm_sse(text);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || async move {
            axum::response::Response::builder()
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(script.clone()))
                .expect("response")
        }),
    );
    let server = MockServer::new(app);
    let url = format!("http://127.0.0.1:{}/v1", server.port);
    (server, url)
}

fn env_with_llm(tag: &str, text: &str) -> Env {
    let (_server, url) = llm_server(text);
    let env = Env::new(tag);
    env.write_project_config(&url, tag);
    env
}

// -----------------------------------------------------------------------
// run: default + JSON event streams
// -----------------------------------------------------------------------

#[test]
fn run_default_format_prints_text_to_stdout() {
    let env = env_with_llm("run-default", "Hello from the mock");
    let output = env.run(&["run", "--model", "mock/mock-model", "Say hi"]);
    assert_eq!(
        output.code,
        Some(0),
        "stdout={}\nstderr={}",
        output.stdout,
        output.stderr
    );
    assert!(
        output.stdout.contains("Hello from the mock"),
        "stdout={}",
        output.stdout
    );
}

#[test]
fn run_json_stream_golden() {
    let env = env_with_llm("run-json", "Hello from the mock");
    let output = env.run(&[
        "run",
        "--format",
        "json",
        "--model",
        "mock/mock-model",
        "Say hi",
    ]);
    assert_eq!(output.code, Some(0), "stderr={}", output.stderr);
    let lines: Vec<&str> = output
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert!(!lines.is_empty(), "stdout={}", output.stdout);
    let mut saw_text = false;
    for line in &lines {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("invalid JSON line {line}: {err}"));
        assert!(value.get("timestamp").is_some(), "{line}");
        assert!(value.get("sessionID").is_some(), "{line}");
        if value["type"] == "text" {
            saw_text = true;
            assert!(
                value["part"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("Hello from the mock"),
                "{line}"
            );
        }
    }
    assert!(saw_text, "no text event in stream");
}

// -----------------------------------------------------------------------
// run: exit codes
// -----------------------------------------------------------------------

#[test]
fn run_missing_message_exits_one() {
    let env = env_with_llm("run-no-message", "x");
    let output = env.run(&["run"]);
    assert_eq!(output.code, Some(1));
    assert!(
        output
            .stderr
            .contains("You must provide a message or a command"),
        "stderr={}",
        output.stderr
    );
}

#[test]
fn run_unknown_session_exits_one() {
    let env = env_with_llm("run-unknown-session", "x");
    let output = env.run(&["run", "--session", "ses_does_not_exist", "hi"]);
    assert_eq!(output.code, Some(1));
    assert!(
        output.stderr.contains("Session not found"),
        "stderr={}",
        output.stderr
    );
}

#[test]
fn run_missing_file_exits_one() {
    let env = env_with_llm("run-bad-file", "x");
    let output = env.run(&["run", "-f", "does-not-exist.txt", "hi"]);
    assert_eq!(output.code, Some(1));
    assert!(
        output.stderr.contains("File not found: does-not-exist.txt"),
        "stderr={}",
        output.stderr
    );
}

// -----------------------------------------------------------------------
// serve: handshake + attach
// -----------------------------------------------------------------------

#[test]
fn serve_prints_warning_and_handshake() {
    let env = env_with_llm("serve", "x");
    let mut child = env
        .command(&["serve", "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve");
    let stdout = child.stdout.take().expect("serve stdout");
    let stderr = child.stderr.take().expect("serve stderr");
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let collected = lines.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            collected.lock().unwrap().push(line);
        }
    });
    let err_lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let collected = err_lines.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            collected.lock().unwrap().push(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let ready = lines
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("alforria server listening on "));
        if ready {
            let lines = lines.lock().unwrap();
            assert!(
                lines[0].starts_with("Warning: OPENCODE_SERVER_PASSWORD is not set"),
                "first line: {lines:?}"
            );
            assert!(
                lines[1].starts_with("alforria server listening on http://"),
                "second line: {lines:?}"
            );
            assert!(err_lines.lock().unwrap().is_empty());
            break;
        }
        assert!(Instant::now() < deadline, "serve handshake timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn run_attach_against_running_serve() {
    let env = env_with_llm("attach", "Hello from the mock");
    let serve = harness::Serve::spawn(&env);
    let output = env.run(&[
        "run",
        "--attach",
        &format!("http://127.0.0.1:{}", serve.port),
        "--model",
        "mock/mock-model",
        "Say hi",
    ]);
    drop(serve);
    assert_eq!(
        output.code,
        Some(0),
        "stdout={}\nstderr={}",
        output.stdout,
        output.stderr
    );
    assert!(
        output.stdout.contains("Hello from the mock"),
        "stdout={}",
        output.stdout
    );
}

// -----------------------------------------------------------------------
// models / agent / session list
// -----------------------------------------------------------------------

#[test]
fn models_lists_fixture_provider() {
    let env = env_with_llm("models", "x");
    let output = env.run(&["models"]);
    assert_eq!(output.code, Some(0), "stderr={}", output.stderr);
    assert!(
        output.stdout.contains("anthropic/claude-sonnet-4-5"),
        "stdout={}",
        output.stdout
    );
}

#[test]
fn agent_list_prints_agents() {
    let env = env_with_llm("agent-list", "x");
    let output = env.run(&["agent", "list"]);
    assert_eq!(output.code, Some(0), "stderr={}", output.stderr);
    assert!(
        output.stdout.contains("build (primary)"),
        "stdout={}",
        output.stdout
    );
    assert!(
        output.stdout.contains("\"permission\""),
        "stdout={}",
        output.stdout
    );
}

#[test]
fn session_list_json_after_run() {
    let env = env_with_llm("session-list", "Hello from the mock");
    let output = env.run(&[
        "run",
        "--title",
        "e2e test session",
        "--model",
        "mock/mock-model",
        "Say hi",
    ]);
    assert_eq!(output.code, Some(0), "stderr={}", output.stderr);
    let list = env.run(&["session", "list", "--format", "json"]);
    assert_eq!(list.code, Some(0), "stderr={}", list.stderr);
    assert!(
        list.stdout.contains("e2e test session"),
        "stdout={}",
        list.stdout
    );
}

// -----------------------------------------------------------------------
// providers login/logout round-trip
// -----------------------------------------------------------------------

#[test]
fn providers_login_logout_roundtrip() {
    let env = env_with_llm("providers", "x");
    let logout_empty = env.run(&["providers", "logout", "anthropic"]);
    assert_eq!(logout_empty.code, Some(0));
    assert!(
        logout_empty.stderr.contains("No credentials found"),
        "stderr={}",
        logout_empty.stderr
    );

    let login = env.run_with(
        &["providers", "login", "-p", "anthropic"],
        Some("sk-test-key\n"),
    );
    assert_eq!(
        login.code,
        Some(0),
        "stdout={}\nstderr={}",
        login.stdout,
        login.stderr
    );

    let list = env.run(&["providers", "list"]);
    assert!(list.stderr.contains("Anthropic"), "stderr={}", list.stderr);
    let auth = std::fs::read_to_string(env.home.join(".local/share/opencode/auth.json"))
        .expect("auth.json");
    assert!(auth.contains("sk-test-key"), "{auth}");

    let logout = env.run(&["providers", "logout", "anthropic"]);
    assert_eq!(logout.code, Some(0));
    assert!(
        logout.stderr.contains("Logout successful"),
        "stderr={}",
        logout.stderr
    );

    let gone = env.run(&["providers", "logout", "anthropic"]);
    assert!(
        gone.stderr.contains("No credentials found"),
        "stderr={}",
        gone.stderr
    );
}

// -----------------------------------------------------------------------
// export / import round-trips
// -----------------------------------------------------------------------

#[test]
fn export_import_roundtrip_is_byte_equal() {
    let env = env_with_llm("export-import", "Hello from the mock");
    let output = env.run(&[
        "run",
        "--title",
        "export me",
        "--model",
        "mock/mock-model",
        "Say hi",
    ]);
    assert_eq!(output.code, Some(0), "stderr={}", output.stderr);

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
    let import = env.run(&["import", file.to_str().unwrap()]);
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

#[test]
fn export_sanitize_redacts_transcript() {
    let env = env_with_llm("sanitize", "Hello from the mock");
    env.run(&["run", "--model", "mock/mock-model", "Say hi"]);
    let export = env.run(&["export", "--sanitize"]);
    assert_eq!(export.code, Some(0), "stderr={}", export.stderr);
    assert!(
        !export.stdout.contains("Hello from the mock"),
        "transcript not sanitized: {}",
        export.stdout
    );
    assert!(
        export.stdout.contains("[redacted:session-title:"),
        "stdout={}",
        export.stdout
    );
}

#[test]
fn import_missing_file_is_an_error() {
    let env = env_with_llm("import-missing", "x");
    let output = env.run(&["import", "/nonexistent/session.json"]);
    assert_eq!(output.code, Some(1));
    assert!(
        output
            .stderr
            .contains("File not found: /nonexistent/session.json"),
        "stderr={}",
        output.stderr
    );
}

#[test]
fn import_share_url_roundtrip() {
    let share = serde_json::json!([
        {"type": "session", "data": {
            "id": "ses_shared",
            "slug": "ses_shared",
            "projectID": "prj_remote",
            "directory": "/remote",
            "title": "shared session",
            "version": "1",
            "time": {"created": 1, "updated": 2},
            "cost": 0,
            "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}}
        }},
        {"type": "message", "data": {
            "role": "user",
            "id": "msg_shared",
            "sessionID": "ses_shared",
            "time": {"created": 1},
            "agent": "build",
            "model": {"providerID": "mock", "modelID": "mock-model"}
        }},
        {"type": "part", "data": {
            "type": "text",
            "id": "prt_shared",
            "sessionID": "ses_shared",
            "messageID": "msg_shared",
            "text": "hello share"
        }},
    ]);
    let app = Router::new().route(
        "/api/share/{slug}/data",
        any(move || async move {
            axum::response::Response::builder()
                .header("content-type", "application/json")
                .body(axum::body::Body::from(share.to_string()))
                .expect("response")
        }),
    );
    let server = MockServer::new(app);
    let env = Env::new("import-share");
    let output = env.run(&[
        "import",
        &format!("http://127.0.0.1:{}/share/ses_shared", server.port),
    ]);
    assert_eq!(
        output.code,
        Some(0),
        "stdout={}\nstderr={}",
        output.stdout,
        output.stderr
    );
    assert_eq!(output.stdout.trim(), "Imported session: ses_shared");
}

// -----------------------------------------------------------------------
// stats
// -----------------------------------------------------------------------

#[test]
fn stats_tables_after_run() {
    let env = env_with_llm("stats", "Hello from the mock");
    let output = env.run(&["run", "--model", "mock/mock-model", "Say hi"]);
    assert_eq!(output.code, Some(0), "stderr={}", output.stderr);
    let stats = env.run(&["stats", "--models"]);
    assert_eq!(stats.code, Some(0), "stderr={}", stats.stderr);
    assert!(stats.stdout.contains("OVERVIEW"), "{}", stats.stdout);
    assert!(stats.stdout.contains("COST & TOKENS"), "{}", stats.stdout);
    assert!(stats.stdout.contains("MODEL USAGE"), "{}", stats.stdout);
    assert!(stats.stdout.contains("mock/mock-model"), "{}", stats.stdout);

    let empty = Env::new("stats-empty");
    let output = empty.run(&["stats"]);
    assert_eq!(output.code, Some(0));
    assert!(
        output
            .stdout
            .lines()
            .any(|line| line.contains("Sessions") && line.contains("0")),
        "{}",
        output.stdout
    );
}

// -----------------------------------------------------------------------
// generate
// -----------------------------------------------------------------------

#[test]
fn generate_outputs_openapi_with_code_samples() {
    let env = Env::new("generate");
    let output = env.run(&["generate"]);
    assert_eq!(output.code, Some(0), "stderr={}", output.stderr);
    let spec: serde_json::Value =
        serde_json::from_str(output.stdout.trim()).expect("valid openapi json");
    assert_eq!(spec["openapi"], "3.1.0");
    let mut sampled = 0;
    let paths = spec["paths"].as_object().expect("paths");
    for item in paths.values() {
        for method in ["get", "post", "put", "delete", "patch"] {
            if let Some(operation) = item.get(method) {
                if operation.get("operationId").is_some() {
                    assert!(
                        operation.get("x-codeSamples").is_some(),
                        "missing samples on {method}"
                    );
                    sampled += 1;
                }
            }
        }
    }
    assert!(sampled > 10, "sampled {sampled} operations");
}
