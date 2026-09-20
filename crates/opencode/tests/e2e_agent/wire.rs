//! Wire plumbing for scenarios with mid-session API access (spec E2E
//! §2.4): an HTTP client speaking the v1 API like the CLI does, plus the
//! SSE event log the test client listens on — the test plays the TUI.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::{json, Value};

use crate::harness::Serve;

const TIMEOUT: Duration = Duration::from_secs(60);

fn encode_uri_component(input: &str) -> String {
    const SAFE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        if SAFE.contains(byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A v1 HTTP API client for one location directory (mirrors the CLI's
/// `OpencodeClient`: GET carries `?directory=`, other methods the
/// `x-opencode-directory` header).
pub struct Api {
    http: reqwest::Client,
    port: u16,
    directory: String,
}

impl Api {
    pub fn new(port: u16, directory: impl Into<String>) -> Api {
        Api {
            http: reqwest::Client::new(),
            port,
            directory: directory.into(),
        }
    }

    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> reqwest::Response {
        let url = format!("http://127.0.0.1:{}{path}", self.port);
        let mut request = self
            .http
            .request(method.clone(), &url)
            .header("content-type", "application/json")
            .header(
                "x-opencode-directory",
                encode_uri_component(&self.directory),
            );
        if method == reqwest::Method::GET {
            request = request.query(&[("directory", self.directory.clone())]);
        }
        let request = match body {
            Some(value) => request.json(&value),
            None => request,
        };
        request
            .send()
            .await
            .unwrap_or_else(|err| panic!("request {url} failed: {err}"))
    }

    async fn json(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Value {
        let response = self.request(method, path, body).await;
        let status = response.status();
        let text = response.text().await.expect("response body");
        assert!(
            status.is_success(),
            "{path} failed: {status} {text}",
            status = status.as_u16()
        );
        serde_json::from_str(&text).unwrap_or_else(|err| panic!("{path} body: {err}"))
    }

    /// `POST /session` — create a session, return its id.
    pub async fn create_session(&self) -> String {
        let value = self
            .json(reqwest::Method::POST, "/session", Some(json!({})))
            .await;
        value["id"].as_str().expect("session id").to_string()
    }

    /// `POST /session/{id}/prompt_async` — 204, the loop runs on.
    pub async fn prompt_async(&self, session_id: &str, body: Value) {
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/session/{session_id}/prompt_async"),
                Some(body),
            )
            .await;
        assert!(
            response.status().is_success(),
            "prompt_async failed: {}",
            response.status()
        );
    }

    /// `POST /session/{id}/message` — the blocking prompt; returns the
    /// final assistant message.
    pub async fn prompt(&self, session_id: &str, body: Value) -> Value {
        self.json(
            reqwest::Method::POST,
            &format!("/session/{session_id}/message"),
            Some(body),
        )
        .await
    }

    /// `POST /session/{id}/permissions/{permissionID}` — the deprecated
    /// `permissionRespond` route (handlers/session.ts:362-378).
    pub async fn permission_respond(
        &self,
        session_id: &str,
        permission_id: &str,
        response: &str,
    ) -> reqwest::Response {
        self.request(
            reqwest::Method::POST,
            &format!("/session/{session_id}/permissions/{permission_id}"),
            Some(json!({ "response": response })),
        )
        .await
    }

    /// `POST /session/{id}/abort`.
    pub async fn abort(&self, session_id: &str) {
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/session/{session_id}/abort"),
                Some(json!({})),
            )
            .await;
        assert!(
            response.status().is_success(),
            "abort failed: {}",
            response.status()
        );
    }

    /// `GET /session/{id}/message` — the message store with parts.
    pub async fn messages(&self, session_id: &str) -> Vec<Value> {
        self.json(
            reqwest::Method::GET,
            &format!("/session/{session_id}/message"),
            None,
        )
        .await
        .as_array()
        .cloned()
        .expect("message list")
    }
}

/// The parsed `/event` SSE stream, appended by a background pump task.
/// Subscribe before prompting: events are not replayed to late joiners.
#[derive(Clone)]
pub struct EventLog(Arc<Mutex<Vec<Value>>>);

impl EventLog {
    pub fn subscribe(serve: &Serve, directory: &str) -> EventLog {
        let log = EventLog(Arc::new(Mutex::new(Vec::new())));
        let url = format!(
            "http://127.0.0.1:{}/event?directory={}",
            serve.port,
            encode_uri_component(directory)
        );
        let sink = log.0.clone();
        tokio::spawn(async move {
            let http = reqwest::Client::new();
            let response = http.get(&url).send().await.expect("event subscribe");
            assert!(
                response.status().is_success(),
                "event subscribe failed: {}",
                response.status()
            );
            let mut stream = response.bytes_stream();
            let mut buffer = String::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.expect("event chunk");
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(index) = buffer.find("\n\n") {
                    let block = buffer[..index].to_string();
                    buffer.drain(..index + 2);
                    if let Some(data) = sse_block_data(&block) {
                        if let Ok(value) = serde_json::from_str::<Value>(&data) {
                            sink.lock().unwrap().push(value);
                        }
                    }
                }
            }
        });
        log
    }

    /// A snapshot of the events seen so far.
    pub fn events(&self) -> Vec<Value> {
        self.0.lock().unwrap().clone()
    }
}

/// Parse one SSE block into its data payload: `data:` lines joined with
/// newlines (comment frames skipped).
fn sse_block_data(block: &str) -> Option<String> {
    let mut data: Vec<&str> = Vec::new();
    for line in block.lines() {
        if line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            None => (line, ""),
            Some((field, rest)) => (field, rest.strip_prefix(' ').unwrap_or(rest)),
        };
        if field == "data" {
            data.push(value);
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    }
}

/// Poll until `check` passes over a fresh event snapshot, bounded by the
/// scenario timeout. Returns the passing snapshot.
pub async fn pump_until(log: &EventLog, mut check: impl FnMut(&[Value]) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let events = log.events();
        if check(&events) {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for an event\n{}",
            serde_json::to_string_pretty(&events).expect("events")
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
