//! JSON-RPC connection over stdio — the `vscode-jsonrpc` surface
//! `lsp/client.ts` builds on (`StreamMessageReader`/`StreamMessageWriter`,
//! LSP base-protocol framing: `Content-Length` headers followed by a
//! blank line and the JSON body).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, MutexGuard};

use crate::tool::def::BoxFuture as Future;

/// `sendRequest` result — the `result` or `error` member of the response.
pub type RequestResult = Result<Value, Value>;

/// Server-initiated traffic dispatch (`connection.onNotification` /
/// `connection.onRequest` handlers, client.ts:160-206).
pub trait Dispatch: Send + Sync {
    fn notification(&self, method: &str, params: Value);
    fn request<'a>(&'a self, method: &'a str, params: Value) -> Future<'a, RequestResult>;
}

struct Inner {
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Mutex<HashMap<i64, tokio::sync::oneshot::Sender<RequestResult>>>,
}

/// A live JSON-RPC connection to one language server process.
pub struct Connection {
    inner: Arc<Inner>,
    dispatch: Arc<dyn Dispatch>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Connection")
    }
}

fn encode(message: &Value) -> String {
    serde_json::to_string(message).unwrap_or_else(|_| "null".to_string())
}

async fn write_message(mut stdin: MutexGuard<'_, ChildStdin>, message: &Value) {
    let body = encode(message);
    let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
    if stdin.write_all(framed.as_bytes()).await.is_err() {
        // The server went away — the reader task drops the pending
        // oneshots, surfacing the failure to in-flight requests.
    }
    let _ = stdin.flush().await;
}

impl Connection {
    /// Start the reader task over the child's stdio.
    pub fn spawn(
        child: &mut Child,
        dispatch: Arc<dyn Dispatch>,
    ) -> std::io::Result<Arc<Connection>> {
        let stdout = child.stdout.take().expect("stdout piped");
        let stdin = child.stdin.take().expect("stdin piped");
        let connection = Arc::new(Connection {
            inner: Arc::new(Inner {
                stdin: Mutex::new(stdin),
                next_id: AtomicI64::new(0),
                pending: Mutex::new(HashMap::new()),
            }),
            dispatch,
        });
        tokio::spawn(read_loop(stdout, Arc::clone(&connection)));
        Ok(connection)
    }

    async fn stdin(&self) -> MutexGuard<'_, ChildStdin> {
        self.inner.stdin.lock().await
    }

    async fn send_message(&self, message: &Value) {
        let guard = self.stdin().await;
        write_message(guard, message).await;
    }

    /// `connection.sendNotification` — a JSON-RPC notification.
    pub async fn send_notification(&self, method: &str, params: Value) {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.send_message(&message).await;
    }

    /// `connection.sendRequest` — await the response `result`.
    pub async fn send_request(&self, method: &str, params: Value) -> RequestResult {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = tokio::sync::oneshot::channel::<RequestResult>();
        let mut pending = self.inner.pending.lock().await;
        pending.insert(id, sender);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let guard = self.stdin().await;
        write_message(guard, &message).await;
        drop(pending);
        match receiver.await {
            Ok(result) => result,
            Err(_) => Err(json!({ "code": -32603, "message": "connection closed" })),
        }
    }

    /// `connection.end()` — stop servicing incoming traffic and fail every
    /// in-flight request (`shutdown`, client.ts:640-645).
    pub async fn end(&self) {
        self.inner.pending.lock().await.clear();
    }

    async fn complete(&self, id: Value, result: RequestResult) {
        let id = match id.as_i64() {
            Some(id) => id,
            None => return,
        };
        let sender = self.inner.pending.lock().await.remove(&id);
        if let Some(sender) = sender {
            let _ = sender.send(result);
        }
    }

    async fn handle_incoming(&self, message: Value) {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id");
        match (method, id) {
            // A response to one of our requests.
            (None, _) => {
                if let Some(id) = id {
                    let result = match message.get("error") {
                        Some(error) => Err(error.clone()),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    self.complete(id.clone(), result).await;
                }
            }
            // A server-initiated request.
            (Some(method), Some(id)) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let result = self.dispatch.request(method, params).await;
                let payload = match &result {
                    Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                    Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
                };
                let guard = self.stdin().await;
                write_message(guard, &payload).await;
            }
            // A server-initiated notification.
            (Some(method), None) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                self.dispatch.notification(method, params);
            }
        }
    }
}

/// Parse `Content-Length` framed bodies off the stdout stream.
async fn read_loop(stdout: tokio::process::ChildStdout, connection: Arc<Connection>) {
    let mut reader = BufReader::new(stdout);
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        match read_frame(&mut reader, &mut buffer).await {
            Ok(Some(frame)) => {
                let message = match serde_json::from_slice::<Value>(&frame) {
                    Ok(message) => message,
                    Err(_) => continue,
                };
                connection.handle_incoming(message).await;
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    // The server exited — fail every pending request.
    connection.inner.pending.lock().await.clear();
}

/// Read one framed body. `buffer` carries bytes left over between calls.
async fn read_frame(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    buffer: &mut Vec<u8>,
) -> std::io::Result<Option<Vec<u8>>> {
    loop {
        // Scan for the end of the header block.
        let header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n");
        let Some(header_end) = header_end else {
            let mut chunk = [0u8; 4096];
            let read = reader.read(&mut chunk).await?;
            if read == 0 {
                return Ok(None);
            }
            buffer.extend_from_slice(&chunk[..read]);
            continue;
        };
        let split = header_end + 4;
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let mut length = None;
        for line in headers.split("\r\n") {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().ok();
            }
        }
        let Some(length) = length else {
            buffer.drain(..split);
            continue;
        };
        if buffer.len() < split + length {
            let mut chunk = [0u8; 4096];
            let read = reader.read(&mut chunk).await?;
            if read == 0 {
                return Ok(None);
            }
            buffer.extend_from_slice(&chunk[..read]);
            continue;
        }
        let body = buffer[split..split + length].to_vec();
        buffer.drain(..split + length);
        return Ok(Some(body));
    }
}

/// A dispatch table for tests + the nothing-registered default.
pub struct NoopDispatch;

impl Dispatch for NoopDispatch {
    fn notification(&self, _method: &str, _params: Value) {}

    fn request<'a>(&'a self, _method: &'a str, _params: Value) -> BoxFuture<'a, RequestResult> {
        Box::pin(async { Ok(Value::Null) })
    }
}
