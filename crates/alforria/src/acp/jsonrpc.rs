//! ndjson JSON-RPC 2.0 framing — the `@agentclientprotocol/sdk`
//! `Connection` (dist/acp.js:890-1230): one JSON message per line over
//! the transport, sequential request dispatch, request/response
//! correlation for agent -> client requests.

use std::collections::HashMap;
use std::io::{BufRead as _, Write as _};
use std::sync::Arc;

use serde_json::{json, Value};

/// `RequestError` codes (dist/acp.js:1256-1281).
#[derive(Debug, Clone, PartialEq)]
pub struct AcpError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl AcpError {
    pub fn invalid_params(data: Value, additional_message: Option<&str>) -> AcpError {
        AcpError {
            code: -32602,
            message: match additional_message {
                Some(suffix) => format!("Invalid params: {suffix}"),
                None => "Invalid params".to_string(),
            },
            data: Some(data),
        }
    }

    pub fn method_not_found(method: &str) -> AcpError {
        AcpError {
            code: -32601,
            message: format!("\"Method not found\": {method}"),
            data: Some(json!({ "method": method })),
        }
    }

    pub fn internal_error(data: Value, additional_message: Option<&str>) -> AcpError {
        AcpError {
            code: -32603,
            message: match additional_message {
                Some(suffix) => format!("Internal error: {suffix}"),
                None => "Internal error".to_string(),
            },
            data: Some(data),
        }
    }

    pub fn auth_required(data: Value, additional_message: Option<&str>) -> AcpError {
        AcpError {
            code: -32000,
            message: match additional_message {
                Some(suffix) => format!("Authentication required: {suffix}"),
                None => "Authentication required".to_string(),
            },
            data: Some(data),
        }
    }

    pub fn parse_error(data: Option<Value>) -> AcpError {
        AcpError {
            code: -32700,
            message: "Parse error".to_string(),
            data,
        }
    }
}

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "code {} {}", self.code, self.message)?;
        if let Some(data) = &self.data {
            write!(f, " {data}")?;
        }
        Ok(())
    }
}

impl std::error::Error for AcpError {}

/// One inbound line parsed (dist/acp.js `processMessage`, 1059-1092).
#[derive(Debug, Clone)]
pub enum Message {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        result: Option<Value>,
        error: Option<AcpError>,
    },
}

impl Message {
    pub fn parse(line: &str) -> Option<Message> {
        let value: Value = serde_json::from_str(line).ok()?;
        if !value.is_object() {
            return None;
        }
        let id = value.get("id");
        let method = value.get("method").and_then(Value::as_str);
        match (method, id) {
            (Some(method), Some(id)) => Some(Message::Request {
                id: id.clone(),
                method: method.to_string(),
                params: value.get("params").cloned().unwrap_or(Value::Null),
            }),
            (Some(method), None) => Some(Message::Notification {
                method: method.to_string(),
                params: value.get("params").cloned().unwrap_or(Value::Null),
            }),
            (None, Some(id)) => {
                let error = value
                    .get("error")
                    .and_then(|error| error.as_object())
                    .map(|error| AcpError {
                        code: error
                            .get("code")
                            .and_then(Value::as_i64)
                            .unwrap_or_default(),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        data: error.get("data").cloned(),
                    });
                Some(Message::Response {
                    id: id.clone(),
                    result: value.get("result").cloned(),
                    error,
                })
            }
            (None, None) => None,
        }
    }
}

/// The bidirectional channel — stdio in production, in-memory pipes in
/// tests (`ndJsonStream`).
pub trait Transport: Send + Sync {
    /// Next line, `None` when the stream ends.
    fn read(&self) -> Option<String>;
    fn write(&self, line: &str);
    /// Whether the underlying stream still has a peer — used to stop
    /// outbound request pumps on disconnect.
    fn is_closed(&self) -> bool {
        false
    }
}

/// Serialized message sender + outbound request correlation
/// (`Connection.sendRequest`/`handleResponse`, dist/acp.js:1140-1176).
pub struct Connection {
    transport: Arc<dyn Transport>,
    pending: std::sync::Mutex<HashMap<i64, Option<std::sync::mpsc::Sender<Message>>>>,
    next_id: std::sync::atomic::AtomicI64,
}

impl Connection {
    pub fn new(transport: Arc<dyn Transport>) -> Connection {
        Connection {
            transport,
            pending: std::sync::Mutex::new(HashMap::new()),
            next_id: std::sync::atomic::AtomicI64::new(1),
        }
    }

    pub fn transport(&self) -> Arc<dyn Transport> {
        self.transport.clone()
    }

    /// Route an inbound response line to its pending request. Returns
    /// `false` when no request is pending for the id (unknown-response
    /// log parity, dist/acp.js:1176).
    pub fn handle_response(&self, message: Message) -> bool {
        let Message::Response { id, .. } = &message else {
            return false;
        };
        let mut pending = self.pending.lock().unwrap();
        let Some(sender) = pending.remove(&id_as_key(id)) else {
            return false;
        };
        if let Some(sender) = sender {
            let _ = sender.send(message);
        }
        true
    }

    pub async fn send_request(&self, method: &str, params: Value) -> Result<Value, AcpError> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel::<Message>();
        self.pending.lock().unwrap().insert(id, Some(tx.clone()));
        self.transport.write(
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            })
            .to_string(),
        );
        let message = tokio::task::spawn_blocking(move || rx.recv().ok())
            .await
            .ok()
            .flatten()
            .ok_or_else(|| AcpError {
                code: -32603,
                message: "ACP connection closed".to_string(),
                data: None,
            })?;
        match message {
            Message::Response { result, error, .. } => match error {
                Some(error) => Err(error),
                None => Ok(result.unwrap_or(Value::Null)),
            },
            _ => Err(AcpError::internal_error(Value::Null, None)),
        }
    }

    pub async fn send_notification(&self, method: &str, params: Value) {
        self.transport.write(
            &json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            })
            .to_string(),
        );
    }
}

/// Stable key for a JSON-RPC id (number or string).
fn id_as_key(id: &Value) -> i64 {
    match id {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|f| f as i64))
            .unwrap_or_default(),
        Value::String(text) => text.bytes().fold(0i64, |acc, byte| acc * 31 + byte as i64),
        _ => 0,
    }
}

/// The transport used in production: stdin/stdout. Every write goes
/// through a lock so concurrent tasks never interleave lines; logs
/// belong on stderr only.
pub struct StdioTransport {
    stdin: std::sync::Mutex<std::io::BufReader<std::io::Stdin>>,
    stdout: std::sync::Mutex<std::io::Stdout>,
}

impl StdioTransport {
    pub fn new() -> StdioTransport {
        StdioTransport {
            stdin: std::sync::Mutex::new(std::io::BufReader::new(std::io::stdin())),
            stdout: std::sync::Mutex::new(std::io::stdout()),
        }
    }
}

impl Default for StdioTransport {
    fn default() -> Self {
        StdioTransport::new()
    }
}

impl Transport for StdioTransport {
    fn read(&self) -> Option<String> {
        let mut line = String::new();
        let mut stdin = self.stdin.lock().unwrap();
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) => return None,
                Ok(_) => {
                    let trimmed = line.trim_end_matches(['\n', '\r']);
                    if trimmed.is_empty() {
                        continue;
                    }
                    return Some(trimmed.to_string());
                }
                Err(_) => return None,
            }
        }
    }

    fn write(&self, line: &str) {
        let mut stdout = self.stdout.lock().unwrap();
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}
