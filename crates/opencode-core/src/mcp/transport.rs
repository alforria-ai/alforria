//! MCP transports — hand-rolled JSON-RPC transports for stdio
//! (`StdioClientTransport`) and streamable HTTP
//! (`StreamableHTTPClientTransport`), pinned to the TS lockfile's SDK
//! (`@modelcontextprotocol/sdk@1.29.0`, STOP S1: no new dependency).
//!
//! * stdio (`stdio.js`): newline-delimited JSON-RPC over the child's
//!   stdin/stdout; stderr is piped to /dev/null.
//! * HTTP (`streamableHttp.js:283-431`): every message is a POST with
//!   `mcp-session-id` / `mcp-protocol-version` headers (`:60-74`); the
//!   response is either direct JSON, an SSE stream whose `data:` frames
//!   carry the JSON-RPC response, or 202 Accepted with no body.
//!
//! Divergence: the SDK's standalone GET SSE stream (server-initiated
//! messages + reconnection backoff, `streamableHttp.js:84-118`) is not
//! ported — the Rust client has no consumer of server-initiated messages
//! yet (`ToolListChanged` live refresh arrives with a later chunk).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex};

use crate::session::store::INSTALLATION_VERSION;

/// `LATEST_PROTOCOL_VERSION` (SDK types.js:2).
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
/// `SUPPORTED_PROTOCOL_VERSIONS` (SDK types.js:4).
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] = [
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];

/// One transport failure — the message is the wire-compat surface the
/// service classifies (`index.ts:294-295`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpError {
    pub message: String,
    /// 401-class failures (`UnauthorizedError`) drive the needs_auth /
    /// needs_client_registration statuses.
    pub unauthorized: bool,
}

impl McpError {
    pub fn failed(message: impl Into<String>) -> McpError {
        McpError {
            message: message.into(),
            unauthorized: false,
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> McpError {
        McpError {
            message: message.into(),
            unauthorized: true,
        }
    }
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for McpError {}

/// Serialize one JSON-RPC message onto a transport body.
fn encode(message: &Value) -> String {
    serde_json::to_string(message).unwrap_or_else(|_| "null".to_string())
}

// ---------------------------------------------------------------------------
// stdio
// ---------------------------------------------------------------------------

/// Newline-delimited JSON-RPC over a spawned child process.
pub struct StdioTransport {
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    responses: Arc<Mutex<mpsc::UnboundedReceiver<Value>>>,
    child: tokio::sync::Mutex<tokio::process::Child>,
    pub pid: Option<u32>,
}

impl StdioTransport {
    /// `connectLocal` env (index.ts:352-356): the process env, the
    /// `opencode`-command `BUN_BE_BUN` marker, then the configured
    /// `environment`. `directory` is the `roots` capability root
    /// (index.ts:77-79).
    pub async fn spawn(
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
        environment: Option<&BTreeMap<String, String>>,
        directory: &Path,
    ) -> Result<StdioTransport, McpError> {
        let mut command_builder = tokio::process::Command::new(command);
        command_builder
            .args(args)
            .current_dir(cwd.unwrap_or_else(|| Path::new(".")))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if command == "opencode" {
            command_builder.env("BUN_BE_BUN", "1");
        }
        if let Some(environment) = environment {
            for (key, value) in environment {
                command_builder.env(key, value);
            }
        }
        let mut child = command_builder
            .spawn()
            .map_err(|err| McpError::failed(err.to_string()))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stdin = child.stdin.take().expect("piped stdin");
        let pid = child.id();
        let stdin = Arc::new(Mutex::new(stdin));
        let (tx, rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(read_lines(
            stdout,
            tx,
            Arc::clone(&stdin),
            path_to_file_url(directory),
        ));
        Ok(StdioTransport {
            stdin,
            responses: Arc::new(Mutex::new(rx)),
            child: tokio::sync::Mutex::new(child),
            pid,
        })
    }

    /// Write one message.
    pub async fn send(&self, message: &Value) -> Result<(), McpError> {
        let mut stdin = self.stdin.lock().await;
        let line = encode(message);
        async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
            Ok::<(), std::io::Error>(())
        }
        .await
        .map_err(|_| McpError::failed("transport closed"))
    }

    /// Read messages until the callback accepts one (request
    /// correlation); notifications are skipped.
    pub async fn recv_until(&self, accept: impl Fn(&Value) -> bool) -> Result<Value, McpError> {
        let mut responses = self.responses.lock().await;
        loop {
            match responses.recv().await {
                Some(value) if accept(&value) => return Ok(value),
                Some(_) => {}
                None => return Err(McpError::failed("transport closed")),
            }
        }
    }

    /// Close the transport (`transport.close()`): stdin EOF signals the
    /// child to exit; the process is reaped and killed after a grace
    /// period.
    pub async fn close(&self) {
        self.stdin.lock().await.shutdown().await.ok();
        let mut child = self.child.lock().await;
        match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
            Ok(_) => {}
            Err(_) => {
                child.start_kill().ok();
            }
        }
    }
}

/// The reader half: parse lines, auto-answer `roots/list` requests
/// (`ListRootsRequestSchema`, index.ts:77-79), forward everything else.
async fn read_lines(
    stdout: tokio::process::ChildStdout,
    tx: mpsc::UnboundedSender<Value>,
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    root_url: String,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = value.get("method").and_then(Value::as_str);
        if method == Some("roots/list") {
            let reply = json!({
                "jsonrpc": "2.0",
                "id": value["id"],
                "result": {"roots": [{"uri": root_url}]},
            });
            let mut stdin = stdin.lock().await;
            if stdin
                .write_all(format!("{}\n", encode(&reply)).as_bytes())
                .await
                .is_err()
            {
                break;
            }
            continue;
        }
        let _ = tx.send(value);
    }
}

// ---------------------------------------------------------------------------
// streamable HTTP
// ---------------------------------------------------------------------------

pub struct HttpTransport {
    url: String,
    headers: Vec<(String, String)>,
    pub session_id: Option<String>,
    pub protocol_version: Option<String>,
    bearer: Option<String>,
}

fn reqwest_error(err: reqwest::Error) -> McpError {
    McpError::failed(format!("{err}"))
}

impl HttpTransport {
    pub fn new(url: &str, headers: Vec<(String, String)>) -> HttpTransport {
        HttpTransport {
            url: url.to_string(),
            headers,
            session_id: None,
            protocol_version: None,
            bearer: None,
        }
    }

    /// Use stored access tokens for the `Authorization: Bearer` header
    /// (`_commonHeaders`, streamableHttp.js:60-74).
    pub fn set_bearer(&mut self, bearer: Option<String>) {
        self.bearer = bearer;
    }

    fn common_headers(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderName, HeaderValue};
        let mut headers = reqwest::header::HeaderMap::new();
        let set = |headers: &mut reqwest::header::HeaderMap, key: &str, value: &str| {
            if let (Ok(key), Ok(value)) = (HeaderName::try_from(key), HeaderValue::from_str(value))
            {
                headers.insert(key, value);
            };
        };
        if let Some(bearer) = &self.bearer {
            set(&mut headers, "Authorization", &format!("Bearer {bearer}"));
        }
        if let Some(session_id) = &self.session_id {
            set(&mut headers, "mcp-session-id", session_id);
        }
        if let Some(version) = &self.protocol_version {
            set(&mut headers, "mcp-protocol-version", version);
        }
        for (key, value) in &self.headers {
            set(&mut headers, key, value);
        }
        headers
    }

    /// POST one message. Returns `None` for 202 Accepted (no body) and
    /// `Some(response)` otherwise (`_send`, streamableHttp.js:288-431).
    pub async fn send(&mut self, message: &Value) -> Result<Option<Value>, McpError> {
        let response = self.post(message).await?;
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(String::from);
        if let Some(session_id) = session_id {
            self.session_id = Some(session_id);
        }
        let status = response.status();
        if !status.is_success() {
            if status.as_u16() == 401 {
                return Err(McpError::unauthorized("Unauthorized"));
            }
            let text = response.text().await.unwrap_or_default();
            return Err(McpError::failed(format!("Streamable HTTP error: {text}")));
        }
        if status.as_u16() == 202 {
            return Ok(None);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = response
            .text()
            .await
            .map_err(|err| McpError::failed(format!("{err}")))?;
        if content_type.contains("text/event-stream") {
            return Ok(Some(sse_response(&body, message["id"].clone())));
        }
        let value: Value = serde_json::from_str(&body)
            .map_err(|err| McpError::failed(format!("Invalid response body: {err}")))?;
        Ok(Some(value))
    }

    /// Fire a notification (`202 Accepted` path — responses are never
    /// consumed).
    pub async fn notify(&self, message: &Value) -> Result<(), McpError> {
        self.post(message).await.map(|_| ())
    }

    async fn post(&self, message: &Value) -> Result<reqwest::Response, McpError> {
        let client = reqwest::Client::new();
        client
            .post(&self.url)
            .headers(self.common_headers())
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(encode(message))
            .send()
            .await
            .map_err(reqwest_error)
    }
}

/// Parse `text/event-stream` bodies: the JSON-RPC response for a request
/// is the `data:` frame carrying the same `id` (notifications between
/// frames are skipped).
fn sse_response(body: &str, id: Value) -> Value {
    for frame in body.split("\n\n") {
        let mut data = String::new();
        for line in frame.split('\n') {
            if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest.trim());
            }
        }
        if data.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let is_response = value.get("result").is_some() || value.get("error").is_some();
        if is_response && value.get("id") == Some(&id) {
            return value;
        }
    }
    Value::Null
}

/// `pathToFileURL(directory).href` (index.ts:78) — `file:` URL with the
/// path percent-encoded outside the unreserved set.
pub fn path_to_file_url(directory: &Path) -> String {
    let mut url = String::from("file://");
    for byte in directory.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                url.push(byte as char)
            }
            _ => url.push_str(&format!("%{byte:02X}")),
        }
    }
    url
}

/// The `initialize` params (`createClient`, index.ts:75-81 + SDK
/// client/index.js:306-311).
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": LATEST_PROTOCOL_VERSION,
        "capabilities": {
            // https://github.com/anomalyco/opencode/issues/2308
            "roots": {},
        },
        "clientInfo": {
            "name": "opencode",
            "version": INSTALLATION_VERSION,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_response_matches_the_request_id() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"n\"}\n\n\
             data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n";
        let value = sse_response(body, Value::from(7));
        assert_eq!(value["result"]["ok"], true);
        assert_eq!(sse_response(body, Value::from(9)), Value::Null);
    }

    #[test]
    fn path_url_encodes() {
        assert_eq!(path_to_file_url(Path::new("/a b")), "file:///a%20b");
        assert_eq!(path_to_file_url(Path::new("/x")), "file:///x");
    }

    #[test]
    fn initialize_params_shape() {
        let value = initialize_params();
        assert_eq!(value["protocolVersion"], LATEST_PROTOCOL_VERSION);
        assert_eq!(value["capabilities"]["roots"], json!({}));
        assert_eq!(value["clientInfo"]["name"], "opencode");
    }
}
