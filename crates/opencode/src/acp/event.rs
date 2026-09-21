//! The `/global/event` subscription (`acp/event.ts`) — SSE pump,
//! session-update translation and the `runUntilIdle` handshake.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use serde_json::{json, Value};

use crate::acp::content::ReplayPart;
use crate::acp::jsonrpc::Connection;
use crate::acp::permission;
use crate::acp::server::ServerClient;
use crate::acp::session::SessionStore;
use crate::acp::tool;

/// One parsed SSE frame payload (`{type, properties}`).
pub type EventPayload = Value;

/// The `Subscription` (event.ts:39-400): state shared by the pump and
/// the request handlers.
pub struct Subscription {
    server: ServerClient,
    connection: Arc<Connection>,
    sessions: SessionStore,
    abort: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    idle_waiters: Mutex<HashMap<String, Vec<tokio::sync::oneshot::Sender<()>>>>,
    connection_waiters: Mutex<Vec<tokio::sync::oneshot::Sender<()>>>,
    shell_snapshots: Mutex<HashMap<String, String>>,
    tool_starts: Mutex<std::collections::HashSet<String>>,
    permission_queues: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

/// `start` (event.ts:33-37).
pub fn start(
    server: ServerClient,
    connection: Arc<Connection>,
    sessions: SessionStore,
) -> Arc<Subscription> {
    let subscription = Arc::new(Subscription {
        server: server.clone(),
        connection,
        sessions,
        abort: Arc::new(AtomicBool::new(false)),
        connected: Arc::new(AtomicBool::new(false)),
        idle_waiters: Mutex::new(HashMap::new()),
        connection_waiters: Mutex::new(Vec::new()),
        shell_snapshots: Mutex::new(HashMap::new()),
        tool_starts: Mutex::new(std::collections::HashSet::new()),
        permission_queues: Mutex::new(HashMap::new()),
    });
    let subscription_clone = subscription.clone();
    tokio::spawn(async move {
        subscription_clone.run().await;
    });
    subscription
}

impl Subscription {
    /// `runUntilIdle` (event.ts:74-91).
    pub async fn run_until_idle<A>(
        &self,
        session_id: &str,
        request: impl std::future::Future<Output = A>,
    ) -> A {
        self.wait_until_connected().await;
        let mut rx = self.register_idle_waiter(session_id);
        let response = request.await;
        if rx.try_recv().is_err() {
            let _ = rx.await;
        }
        response
    }

    /// The connected handshake (`waitUntilConnected`, event.ts:167-172).
    async fn wait_until_connected(&self) {
        while !self.connected.load(Ordering::SeqCst) {
            if self.abort.load(Ordering::SeqCst) {
                return;
            }
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            self.connection_waiters.lock().unwrap().push(tx);
            let _ = rx.await;
        }
    }

    fn register_idle_waiter(&self, session_id: &str) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.idle_waiters
            .lock()
            .unwrap()
            .entry(session_id.to_string())
            .or_default()
            .push(tx);
        rx
    }

    fn idle(&self, session_id: &str) {
        let mut waiters = self.idle_waiters.lock().unwrap();
        if let Some(waiters) = waiters.remove(session_id) {
            for waiter in waiters {
                let _ = waiter.send(());
            }
        }
    }

    /// The reconnect loop (`run`, event.ts:144-150).
    async fn run(&self) {
        while !self.abort.load(Ordering::SeqCst) {
            self.consume().await;
            if self.abort.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    /// The SSE consumer (`consume`, event.ts:152-165).
    async fn consume(&self) {
        let http = reqwest::Client::new();
        let Ok(response) = http
            .get(self.server.global_event_url())
            .header("accept", "text/event-stream")
            .send()
            .await
        else {
            return;
        };
        if !response.status().is_success() {
            return;
        }
        self.connected.store(true, Ordering::SeqCst);
        let waiters = std::mem::take(&mut *self.connection_waiters.lock().unwrap());
        for waiter in waiters {
            let _ = waiter.send(());
        }
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            if self.abort.load(Ordering::SeqCst) {
                return;
            }
            let Ok(chunk) = chunk else {
                break;
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(index) = buffer.find("\n\n") {
                let block = buffer[..index].to_string();
                buffer.drain(..index + 2);
                if let Some(data) = sse_block_data(&block) {
                    if let Ok(envelope) = serde_json::from_str::<Value>(&data) {
                        if let Some(payload) = envelope.get("payload") {
                            self.handle(payload).await;
                        }
                    }
                }
            }
        }
    }

    /// `handle` (event.ts:93-106).
    async fn handle(&self, payload: &EventPayload) {
        match payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "session.status" => {
                if let Some(session_id) = payload
                    .pointer("/properties/sessionID")
                    .and_then(Value::as_str)
                    .filter(|_| payload["properties"]["status"]["type"] == json!("idle"))
                {
                    self.idle(session_id);
                }
            }
            "permission.asked" => {
                self.queue_permission(payload);
            }
            "message.part.updated" => {
                self.handle_part_updated(payload).await;
            }
            "message.part.delta" => {
                self.handle_part_delta(payload).await;
            }
            _ => {}
        }
    }

    /// Per-session FIFO permission queue (`Handler.handle`,
    /// permission.ts:37-49).
    fn queue_permission(&self, permission: &Value) {
        let Some(session_id) = permission
            .get("properties")
            .and_then(|properties| properties.get("sessionID"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return;
        };
        let mut queues = self.permission_queues.lock().unwrap();
        let previous = queues.remove(&session_id);
        let task = tokio::spawn({
            let permission = permission.clone();
            let server = self.server.clone();
            let connection = self.connection.clone();
            let sessions = self.sessions.clone();
            async move {
                if let Some(previous) = previous {
                    let _ = previous.await;
                }
                permission::handle(
                    connection,
                    &server,
                    &permission["properties"],
                    |session_id| sessions.try_get(session_id).map(|session| session.cwd),
                )
                .await;
            }
        });
        queues.insert(session_id, task);
    }

    /// `handlePartUpdated` (event.ts:191-212).
    async fn handle_part_updated(&self, event: &Value) {
        let properties = &event["properties"];
        let part = &properties["part"];
        let Some(session_id) = part
            .get("sessionID")
            .and_then(Value::as_str)
            .or_else(|| properties.get("sessionID").and_then(Value::as_str))
        else {
            return;
        };
        if self.sessions.try_get(session_id).is_none() {
            return;
        }
        self.record_part(
            session_id,
            part.get("messageID")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            part,
            (part.get("type").and_then(Value::as_str) == Some("reasoning"))
                .then(|| "assistant".to_string()),
        );
        if part.get("type") == Some(&json!("tool")) {
            let cwd = self
                .sessions
                .try_get(session_id)
                .map(|session| session.cwd)
                .unwrap_or_default();
            self.handle_tool_part(session_id, part, &cwd).await;
        }
    }

    async fn handle_part_delta(&self, event: &Value) {
        let properties = &event["properties"];
        let session_id = properties.get("sessionID").and_then(Value::as_str);
        let Some(session_id) = session_id else {
            return;
        };
        let session = match self.sessions.try_get(session_id) {
            Some(session) => session,
            None => return,
        };
        let message_id = properties
            .get("messageID")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let part_id = properties
            .get("partID")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let known = self
            .sessions
            .get_part_metadata(session_id, message_id, part_id);
        let metadata = match known.filter(|known| known.role.is_some() && known.part_type.is_some())
        {
            Some(known) => Some(known),
            None => {
                self.fetch_part_metadata(&session.id, &session.cwd, message_id, part_id)
                    .await
            }
        };
        let Some(metadata) = metadata else {
            return;
        };
        if metadata.role.as_deref() != Some("assistant") {
            return;
        }
        let delta_field = properties.get("field").and_then(Value::as_str);
        if delta_field != Some("text") {
            return;
        }
        let Some(delta) = properties.get("delta").and_then(Value::as_str) else {
            return;
        };
        if metadata.part_type.as_deref() == Some("text") && metadata.ignored != Some(true) {
            let _ = self
                .connection
                .send_notification(
                    "session/update",
                    json!({
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "messageId": message_id,
                            "content": { "type": "text", "text": delta },
                        },
                    }),
                )
                .await;
            return;
        }
        if metadata.part_type.as_deref() == Some("reasoning") {
            let _ = self
                .connection
                .send_notification(
                    "session/update",
                    json!({
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "agent_thought_chunk",
                            "messageId": part_id,
                            "content": { "type": "text", "text": delta },
                        },
                    }),
                )
                .await;
        }
    }

    /// `fetchPartMetadata` (event.ts:261-278).
    async fn fetch_part_metadata(
        &self,
        session_id: &str,
        cwd: &str,
        message_id: &str,
        part_id: &str,
    ) -> Option<crate::acp::session::KnownPartMetadata> {
        let message = self
            .server
            .session_message(cwd, session_id, message_id)
            .await
            .ok()?;
        let part = message.get("parts").and_then(Value::as_array)?;
        let part = part
            .iter()
            .find(|part| part.get("id").and_then(Value::as_str) == Some(part_id))?;
        self.record_part(
            session_id,
            message
                .get("info")
                .and_then(|info| info.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default(),
            part,
            message
                .get("info")
                .and_then(|info| info.get("role"))
                .and_then(Value::as_str)
                .map(str::to_string),
        )
    }

    /// `recordFetchedPart` (event.ts:280-293).
    fn record_part(
        &self,
        session_id: &str,
        message_id: &str,
        part: &Value,
        role: Option<String>,
    ) -> Option<crate::acp::session::KnownPartMetadata> {
        self.sessions
            .record_part_metadata(crate::acp::session::RecordPartMetadata {
                session_id: session_id.to_string(),
                message_id: message_id.to_string(),
                part_id: part
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                part_type: part.get("type").and_then(Value::as_str).map(str::to_string),
                role,
                ignored: part.get("ignored").and_then(Value::as_bool),
                tool_call_id: part
                    .get("callID")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                metadata: part.get("metadata").cloned(),
            })
    }

    /// `handleToolPart` (event.ts:295-339).
    async fn handle_tool_part(&self, session_id: &str, part: &Value, cwd: &str) {
        self.tool_start(session_id, part, cwd).await;

        let call_id = part
            .get("callID")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let tool_name = part.get("tool").and_then(Value::as_str).unwrap_or_default();
        let state = &part["state"];
        let status = state
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match status {
            "pending" => {
                self.shell_snapshots.lock().unwrap().remove(call_id);
            }
            "running" => {
                self.running_tool(session_id, part, cwd).await;
            }
            "completed" => {
                self.clear_tool(call_id);
                let mut update = tool::completed_tool_update(
                    call_id,
                    tool_name,
                    &state["input"],
                    state,
                    state.get("title").and_then(Value::as_str),
                    Some(cwd),
                );
                update["sessionUpdate"] = json!("tool_call_update");
                let _ = self
                    .connection
                    .send_notification(
                        "session/update",
                        json!({ "sessionId": session_id, "update": update }),
                    )
                    .await;
            }
            "error" => {
                self.clear_tool(call_id);
                let mut update = tool::error_tool_update(
                    call_id,
                    tool_name,
                    &state["input"],
                    state
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    state,
                    Some(cwd),
                );
                update["sessionUpdate"] = json!("tool_call_update");
                let _ = self
                    .connection
                    .send_notification(
                        "session/update",
                        json!({ "sessionId": session_id, "update": update }),
                    )
                    .await;
            }
            _ => {}
        }
    }

    /// `runningTool` (event.ts:341-377) with the bash output snapshot
    /// dedup.
    async fn running_tool(&self, session_id: &str, part: &Value, cwd: &str) {
        let call_id = part
            .get("callID")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let tool_name = part.get("tool").and_then(Value::as_str).unwrap_or_default();
        let state = &part["state"];
        let output = if tool_name == "bash" {
            tool::shell_output_snapshot(state)
        } else {
            None
        };
        if let Some(output) = &output {
            if self.shell_snapshots.lock().unwrap().get(call_id) == Some(output) {
                let mut update = tool::duplicate_running_tool_update(
                    call_id,
                    tool_name,
                    &state["input"],
                    state.get("title").and_then(Value::as_str),
                    Some(cwd),
                );
                update["sessionUpdate"] = json!("tool_call_update");
                let _ = self
                    .connection
                    .send_notification(
                        "session/update",
                        json!({ "sessionId": session_id, "update": update }),
                    )
                    .await;
                return;
            }
            self.shell_snapshots
                .lock()
                .unwrap()
                .insert(call_id.to_string(), output.clone());
        }

        let mut update = tool::running_tool_update(
            call_id,
            tool_name,
            &state["input"],
            state.get("title").and_then(Value::as_str),
            output.as_deref(),
            Some(cwd),
        );
        update["sessionUpdate"] = json!("tool_call_update");
        let _ = self
            .connection
            .send_notification(
                "session/update",
                json!({ "sessionId": session_id, "update": update }),
            )
            .await;
    }

    /// `toolStart` (event.ts:379-394).
    async fn tool_start(&self, session_id: &str, part: &Value, cwd: &str) {
        let call_id = part
            .get("callID")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !self.tool_starts.lock().unwrap().insert(call_id.to_string()) {
            return;
        }
        let tool_name = part.get("tool").and_then(Value::as_str).unwrap_or_default();
        let state = &part["state"];
        let mut update = tool::pending_tool_call(
            call_id,
            tool_name,
            &state["input"],
            state.get("title").and_then(Value::as_str),
            Some(cwd),
        );
        update["sessionUpdate"] = json!("tool_call");
        let _ = self
            .connection
            .send_notification(
                "session/update",
                json!({ "sessionId": session_id, "update": update }),
            )
            .await;
    }

    fn clear_tool(&self, call_id: &str) {
        self.tool_starts.lock().unwrap().remove(call_id);
        self.shell_snapshots.lock().unwrap().remove(call_id);
    }

    /// `replayMessage` (event.ts:108-141) — history replay for load/fork.
    pub async fn replay_message(&self, message: &Value) {
        let role = message
            .get("info")
            .and_then(|info| info.get("role"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if role != "assistant" && role != "user" {
            return;
        }
        let session_id = message["info"]["sessionID"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let cwd = if role == "assistant" {
            message["info"]["path"]["cwd"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        } else {
            String::new()
        };
        let parts = match message.get("parts").and_then(Value::as_array) {
            Some(parts) => parts.clone(),
            None => return,
        };
        for part in parts {
            self.record_part(
                &session_id,
                message["info"]["id"].as_str().unwrap_or_default(),
                &part,
                Some(role.to_string()),
            );
            if part.get("type") == Some(&json!("tool")) {
                let cwd = if cwd.is_empty() {
                    std::env::current_dir()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_default()
                } else {
                    cwd.clone()
                };
                self.handle_tool_part(&session_id, &part, &cwd).await;
                continue;
            }
            self.replay_content_part(message, &part).await;
        }
    }

    /// `replayContentPart` (event.ts:122-141).
    async fn replay_content_part(&self, message: &Value, part: &Value) {
        let part_type = part.get("type").and_then(Value::as_str).unwrap_or_default();
        if !matches!(part_type, "text" | "file" | "reasoning") {
            return;
        }
        let session_update = match part_type {
            "reasoning" => "agent_thought_chunk",
            _ => {
                if message["info"]["role"] == json!("user") {
                    "user_message_chunk"
                } else {
                    "agent_message_chunk"
                }
            }
        };
        let replay = match part_type {
            "text" => crate::acp::content::ReplayPart::Text {
                text: part
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                synthetic: false,
                ignored: false,
            },
            "file" => crate::acp::content::ReplayPart::File {
                url: part
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                mime: part
                    .get("mime")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                filename: part
                    .get("filename")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            _ => crate::acp::content::ReplayPart::Reasoning {
                text: part
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
        };
        let replay = match replay {
            crate::acp::content::ReplayPart::Text { text, .. } if part_type == "text" => {
                crate::acp::content::ReplayPart::Text {
                    text,
                    synthetic: part
                        .get("synthetic")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    ignored: part
                        .get("ignored")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }
            }
            other => other,
        };
        let message_id = if part_type == "reasoning" {
            part.get("id").cloned().unwrap_or(Value::Null)
        } else {
            message["info"]["id"].clone()
        };
        for chunk in crate::acp::content::parts_to_content_chunks(&[replay]) {
            let mut update = chunk;
            update["sessionUpdate"] = json!(session_update);
            update["messageId"] = message_id.clone();
            let _ = self
                .connection
                .send_notification(
                    "session/update",
                    json!({
                        "sessionId": message["info"]["sessionID"],
                        "update": update,
                    }),
                )
                .await;
        }
    }
}

/// Parse one SSE block into its data payload (`data:` lines joined).
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

/// Strip an unused-field warning for `ReplayPart` re-exports.
#[allow(dead_code)]
type _ReplayPart = ReplayPart;
