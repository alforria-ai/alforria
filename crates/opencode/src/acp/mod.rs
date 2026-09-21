//! ACP (Agent Client Protocol) agent — Rust port of `acp/` +
//! `cli/cmd/acp.ts`. Wire behavior follows the pinned TS
//! (`.ts-ref-src/acp/`); see `scratchpad/specs/ACP.md`.

pub mod config_option;
pub mod content;
pub mod directory;
pub mod event;
pub mod jsonrpc;
pub mod permission;
pub mod server;
pub mod session;
pub mod tool;
pub mod usage;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::acp::config_option::DEFAULT_VARIANT_VALUE;
use crate::acp::directory::{DirectoryService, Snapshot};
use crate::acp::jsonrpc::{AcpError, Connection, Message, Transport};
use crate::acp::server::ServerClient;
use crate::acp::session::{SessionInfo, SessionStore};

const AUTH_METHOD_ID: &str = "opencode-login";

/// The ACP service (`acp/service.ts`).
pub struct AcpAgent {
    server: ServerClient,
    connection: Arc<Connection>,
    sessions: SessionStore,
    directories: DirectoryService,
    events: Arc<event::Subscription>,
    registered_mcp: Mutex<HashMap<String, HashSet<String>>>,
    session_snapshots: Mutex<HashMap<String, Arc<Snapshot>>>,
}

impl AcpAgent {
    pub fn new(server: ServerClient, connection: Arc<Connection>) -> Arc<AcpAgent> {
        let sessions = SessionStore::new();
        let directories = DirectoryService::new(server.clone());
        let events = event::start(server.clone(), connection.clone(), sessions.clone());
        Arc::new(AcpAgent {
            server,
            connection,
            sessions,
            directories,
            events,
            registered_mcp: Mutex::new(HashMap::new()),
            session_snapshots: Mutex::new(HashMap::new()),
        })
    }

    /// The agent loop — `cli/cmd/acp.ts:47-73` + the sdk
    /// `Connection.receive`. Returns when stdin ends.
    ///
    /// Requests are dispatched without awaiting the handler (the sdk
    /// invokes `processMessage(message)` fire-and-forget), so the read
    /// loop keeps routing while a long `session/prompt` runs. That is
    /// load-bearing: inbound responses to outbound requests
    /// (`session/request_permission`, `fs/write_text_file`) arrive
    /// mid-prompt and must be routed through `handle_response`, and
    /// `session/cancel` must interrupt a running prompt.
    pub async fn run(self: Arc<Self>) {
        while let Some(line) = read_line(&*self.connection.transport()) {
            let Some(message) = Message::parse(&line) else {
                continue;
            };
            match message {
                Message::Request { id, method, params } => {
                    let this = Arc::clone(&self);
                    tokio::spawn(async move {
                        let result = this.dispatch(&method, &params).await;
                        let response = match result {
                            Ok(result) => json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": result,
                            }),
                            Err(error) => json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": error.code,
                                    "message": error.message,
                                    "data": error.data,
                                },
                            }),
                        };
                        this.connection.transport().write(&response.to_string());
                    });
                }
                Message::Notification { method, params } => {
                    if method == "session/cancel" {
                        let this = Arc::clone(&self);
                        tokio::spawn(async move {
                            let _ = this.cancel(&params).await;
                        });
                    }
                }
                Message::Response { .. } => {
                    self.connection.handle_response(message);
                }
            }
        }
    }

    async fn dispatch(self: &Arc<Self>, method: &str, params: &Value) -> Result<Value, AcpError> {
        match method {
            "initialize" => self.initialize(params).await,
            "authenticate" => self.authenticate(params).await,
            "session/new" => self.new_session(params).await,
            "session/load" => self.load_session(params).await,
            "session/list" => self.list_sessions(params).await,
            "session/resume" => self.resume_session(params).await,
            "session/close" => self.close_session(params).await,
            "session/fork" => self.fork_session(params).await,
            "session/set_config_option" => self.set_session_config_option(params).await,
            "session/set_mode" => self.set_session_mode(params).await,
            "session/set_model" => self.set_session_model(params).await,
            "session/prompt" => self.prompt(params).await,
            _ => Err(AcpError::method_not_found(method)),
        }
    }

    // ------------------------------------------------------------------
    // initialize (service.ts:94-139)
    // ------------------------------------------------------------------

    async fn initialize(&self, params: &Value) -> Result<Value, AcpError> {
        let mut auth_method = json!({
            "description": "Run `opencode auth login` in the terminal",
            "name": "Login with opencode",
            "id": AUTH_METHOD_ID,
        });
        if params["clientCapabilities"]["_meta"]["terminal-auth"] == json!(true) {
            auth_method["_meta"] = json!({
                "terminal-auth": {
                    "command": "opencode",
                    "args": ["auth", "login"],
                    "label": "OpenCode Login",
                },
            });
        }
        Ok(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "mcpCapabilities": { "http": true, "sse": true },
                "promptCapabilities": { "embeddedContext": true, "image": true },
                "sessionCapabilities": {
                    "close": {},
                    "fork": {},
                    "list": {},
                    "resume": {},
                },
            },
            "authMethods": [auth_method],
            "agentInfo": {
                "name": "OpenCode",
                "version": opencode_core::session::store::INSTALLATION_VERSION,
            },
        }))
    }

    // ------------------------------------------------------------------
    // authenticate (service.ts:141-146)
    // ------------------------------------------------------------------

    async fn authenticate(&self, params: &Value) -> Result<Value, AcpError> {
        if params["methodId"].as_str() != Some(AUTH_METHOD_ID) {
            return Err(AcpError::invalid_params(
                json!({ "methodId": params["methodId"] }),
                None,
            ));
        }
        Ok(Value::Null)
    }

    // ------------------------------------------------------------------
    // session/new (service.ts:163-209)
    // ------------------------------------------------------------------

    async fn new_session(self: &Arc<Self>, params: &Value) -> Result<Value, AcpError> {
        let cwd = params["cwd"].as_str().unwrap_or_default().to_string();
        let snapshot = self.directory_snapshot(&cwd).await?;
        let selected = snapshot.select_default_model();
        let variant = snapshot.select_variant(&selected);
        let mode_id = if !snapshot.available_modes.is_empty() {
            Some(snapshot.default_mode_id.clone())
        } else {
            None
        };

        let mut body = json!({
            "directory": cwd,
            "model": {
                "providerID": selected["providerID"],
                "id": selected["modelID"],
            },
        });
        if let Some(variant) = &variant {
            body["model"]["variant"] = json!(variant);
        }
        if let Some(mode_id) = &mode_id {
            body["agent"] = json!(mode_id);
        }
        let created = self
            .server
            .session_create(&cwd, body)
            .await
            .map_err(internal_error)?;
        let state = self.store_session(SessionInfo {
            id: created["id"].as_str().unwrap_or_default().to_string(),
            cwd: cwd.clone(),
            mcp_servers: mcp_servers_of(params),
            created_at_ms: now_ms(),
            model: Some(selected.clone()),
            variant: variant.clone(),
            mode_id: mode_id.clone(),
            known_parts: HashMap::new(),
        });
        self.session_snapshots
            .lock()
            .unwrap()
            .insert(state.id.clone(), snapshot.clone());

        self.register_mcp_servers(&cwd, &state.id, &state.mcp_servers)
            .await;
        self.send_available_commands(&state.id, &snapshot).await;

        Ok(json!({
            "sessionId": state.id,
            "configOptions": self.config_options(&snapshot, &selected, state.variant.as_deref(), state.mode_id.as_deref()),
        }))
    }

    // ------------------------------------------------------------------
    // session/load (service.ts:211-247)
    // ------------------------------------------------------------------

    async fn load_session(self: &Arc<Self>, params: &Value) -> Result<Value, AcpError> {
        let cwd = params["cwd"].as_str().unwrap_or_default().to_string();
        let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
        let snapshot = self.directory_snapshot(&cwd).await?;
        let backing = self
            .server
            .session_get(&cwd, &session_id)
            .await
            .map_err(internal_error)?;
        let messages = self
            .server
            .session_messages(&cwd, &session_id, None)
            .await
            .map_err(internal_error)?;
        let infos: Vec<Value> = messages
            .iter()
            .map(|message| message.get("info").cloned().unwrap_or(Value::Null))
            .collect();
        let restored = restore_session(&snapshot, &backing, &infos);

        let state = self.store_session(SessionInfo {
            id: session_id.clone(),
            cwd: cwd.clone(),
            mcp_servers: mcp_servers_of(params),
            created_at_ms: now_ms(),
            model: restored.model.clone(),
            variant: restored.variant.clone(),
            mode_id: restored.mode_id.clone(),
            known_parts: HashMap::new(),
        });
        self.session_snapshots
            .lock()
            .unwrap()
            .insert(state.id.clone(), snapshot.clone());

        self.register_mcp_servers(&cwd, &state.id, &state.mcp_servers)
            .await;
        self.send_available_commands(&state.id, &snapshot).await;
        for message in &messages {
            self.events.replay_message(message).await;
        }

        Ok(json!({
            "configOptions": self.config_options(
                &snapshot,
                state.model.as_ref().or(restored.model.as_ref()).unwrap_or(&Value::Null),
                state.variant.as_deref(),
                state.mode_id.as_deref(),
            ),
        }))
    }

    // ------------------------------------------------------------------
    // session/list (service.ts:249-293)
    // ------------------------------------------------------------------

    async fn list_sessions(self: &Arc<Self>, params: &Value) -> Result<Value, AcpError> {
        let cwd = params.get("cwd").and_then(Value::as_str);
        let cursor = params
            .get("cursor")
            .and_then(Value::as_str)
            .and_then(|cursor| cursor.parse::<f64>().ok())
            .filter(|cursor| cursor.is_finite());
        let sessions = self
            .server
            .session_list(cwd)
            .await
            .map_err(internal_error)?;

        let mut entries: Vec<Value> = sessions
            .iter()
            .map(|item| {
                json!({
                    "sessionId": item["id"],
                    "cwd": item["directory"],
                    "title": item["title"],
                    "updatedAt": iso_ms(item.pointer("/time/updated")),
                })
            })
            .collect();
        for item in self.sessions.list(cwd) {
            if entries
                .iter()
                .any(|entry| entry["sessionId"] == json!(item.id))
            {
                continue;
            }
            entries.push(json!({
                "sessionId": item.id,
                "cwd": item.cwd,
                "updatedAt": iso_ms(Some(&json!(item.created_at_ms))),
            }));
        }
        entries.sort_by(|a, b| {
            let updated = |entry: &Value| {
                entry
                    .get("updatedAt")
                    .and_then(Value::as_str)
                    .and_then(|updated| {
                        updated
                            .parse::<chrono::DateTime<chrono::Utc>>()
                            .map(|at| at.timestamp_millis() as f64)
                            .ok()
                    })
                    .unwrap_or(0.0)
            };
            updated(b)
                .partial_cmp(&updated(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let filtered: Vec<Value> = match cursor {
            Some(cursor) => entries
                .iter()
                .filter(|entry| {
                    let updated = entry
                        .get("updatedAt")
                        .and_then(Value::as_str)
                        .and_then(|updated| {
                            updated
                                .parse::<chrono::DateTime<chrono::Utc>>()
                                .map(|at| at.timestamp_millis() as f64)
                                .ok()
                        })
                        .unwrap_or(0.0);
                    updated < cursor
                })
                .cloned()
                .collect(),
            None => entries,
        };
        let limit = 100;
        let page: Vec<Value> = filtered.iter().take(limit).cloned().collect();
        let next_cursor = (filtered.len() > limit)
            .then(|| {
                page.last()
                    .and_then(|entry| entry.get("updatedAt"))
                    .and_then(Value::as_str)
                    .and_then(|updated| {
                        updated
                            .parse::<chrono::DateTime<chrono::Utc>>()
                            .map(|at| at.timestamp_millis().to_string())
                            .ok()
                    })
            })
            .flatten();
        let mut response = json!({ "sessions": page });
        if let Some(next_cursor) = next_cursor {
            response["nextCursor"] = json!(next_cursor);
        }
        Ok(response)
    }

    // ------------------------------------------------------------------
    // session/resume (service.ts:295-334)
    // ------------------------------------------------------------------

    async fn resume_session(self: &Arc<Self>, params: &Value) -> Result<Value, AcpError> {
        let cwd = params["cwd"].as_str().unwrap_or_default().to_string();
        let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
        let snapshot = self.directory_snapshot(&cwd).await?;
        let backing = self
            .server
            .session_get(&cwd, &session_id)
            .await
            .map_err(internal_error)?;
        let messages = self
            .server
            .session_messages(&cwd, &session_id, Some(20))
            .await
            .map_err(internal_error)?;
        let infos: Vec<Value> = messages
            .iter()
            .map(|message| message.get("info").cloned().unwrap_or(Value::Null))
            .collect();
        let restored = restore_session(&snapshot, &backing, &infos);

        let state = self.store_session(SessionInfo {
            id: session_id.clone(),
            cwd: cwd.clone(),
            mcp_servers: params
                .get("mcpServers")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            created_at_ms: now_ms(),
            model: restored.model.clone(),
            variant: restored.variant.clone(),
            mode_id: restored.mode_id.clone(),
            known_parts: HashMap::new(),
        });
        self.session_snapshots
            .lock()
            .unwrap()
            .insert(state.id.clone(), snapshot.clone());

        self.register_mcp_servers(&cwd, &state.id, &state.mcp_servers)
            .await;
        self.send_available_commands(&state.id, &snapshot).await;

        Ok(json!({
            "configOptions": self.config_options(
                &snapshot,
                state.model.as_ref().or(restored.model.as_ref()).unwrap_or(&Value::Null),
                state.variant.as_deref(),
                state.mode_id.as_deref(),
            ),
        }))
    }

    // ------------------------------------------------------------------
    // session/close (service.ts:347-355)
    // ------------------------------------------------------------------

    async fn close_session(self: &Arc<Self>, params: &Value) -> Result<Value, AcpError> {
        let session_id = params["sessionId"].as_str().unwrap_or_default();
        let removed = self.sessions.remove(session_id);
        self.registered_mcp.lock().unwrap().remove(session_id);
        self.session_snapshots.lock().unwrap().remove(session_id);
        if let Some(removed) = removed {
            self.abort_backing_session(&removed).await;
        }
        Ok(json!({}))
    }

    // ------------------------------------------------------------------
    // session/cancel (service.ts:357-360) — a notification.
    // ------------------------------------------------------------------

    async fn cancel(&self, params: &Value) -> Result<Value, AcpError> {
        let current = self.get_session(params["sessionId"].as_str().unwrap_or_default())?;
        self.abort_backing_session(&current).await;
        Ok(Value::Null)
    }

    // ------------------------------------------------------------------
    // session/fork (service.ts:362-407)
    // ------------------------------------------------------------------

    async fn fork_session(self: &Arc<Self>, params: &Value) -> Result<Value, AcpError> {
        let cwd = params["cwd"].as_str().unwrap_or_default().to_string();
        let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
        let snapshot = self.directory_snapshot(&cwd).await?;
        let forked = self
            .server
            .session_fork(&cwd, &session_id)
            .await
            .map_err(internal_error)?;
        let forked_id = forked["id"].as_str().unwrap_or_default().to_string();
        let messages = self
            .server
            .session_messages(&cwd, &forked_id, Some(20))
            .await
            .map_err(internal_error)?;
        let infos: Vec<Value> = messages
            .iter()
            .map(|message| message.get("info").cloned().unwrap_or(Value::Null))
            .collect();
        let restored = restore_session(&snapshot, &forked, &infos);

        let state = self.store_session(SessionInfo {
            id: forked_id.clone(),
            cwd: cwd.clone(),
            mcp_servers: params
                .get("mcpServers")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            created_at_ms: now_ms(),
            model: restored.model.clone(),
            variant: restored.variant.clone(),
            mode_id: restored.mode_id.clone(),
            known_parts: HashMap::new(),
        });
        self.session_snapshots
            .lock()
            .unwrap()
            .insert(state.id.clone(), snapshot.clone());

        self.register_mcp_servers(&cwd, &state.id, &state.mcp_servers)
            .await;
        self.send_available_commands(&state.id, &snapshot).await;
        for message in &messages {
            self.events.replay_message(message).await;
        }

        Ok(json!({
            "sessionId": state.id,
            "configOptions": self.config_options(
                &snapshot,
                state.model.as_ref().or(restored.model.as_ref()).unwrap_or(&Value::Null),
                state.variant.as_deref(),
                state.mode_id.as_deref(),
            ),
        }))
    }

    // ------------------------------------------------------------------
    // session/set_config_option (service.ts:409-466)
    // ------------------------------------------------------------------

    async fn set_session_config_option(
        self: &Arc<Self>,
        params: &Value,
    ) -> Result<Value, AcpError> {
        let session_id = params["sessionId"].as_str().unwrap_or_default();
        let current = self.get_session(session_id)?;
        let snapshot = self.config_snapshot(&current).await?;
        let Some(config_id) = params.get("configId").and_then(Value::as_str) else {
            return Err(invalid_config_option("<missing>"));
        };
        let Some(value) = params.get("value").and_then(Value::as_str) else {
            return Err(invalid_config_option(config_id));
        };

        match config_id {
            "model" => {
                let selected = parse_selected_model(&snapshot, value)?;
                let variant = select_model_variant(&snapshot, &current, &selected);
                self.sessions.set_variant(session_id, variant.clone());
                self.sessions.set_model(
                    session_id,
                    Some(json!({
                        "providerID": selected["model"]["providerID"],
                        "modelID": selected["model"]["modelID"],
                    })),
                );
                let state = self.get_session(session_id)?;
                let options = self.config_options(
                    &snapshot,
                    state.model.as_ref().unwrap_or(&selected["model"]),
                    state.variant.as_deref(),
                    state.mode_id.as_deref(),
                );
                self.send_config_option_update(session_id, &options).await;
                Ok(json!({ "configOptions": options }))
            }
            "effort" => {
                let model = current
                    .model
                    .clone()
                    .unwrap_or_else(|| snapshot.select_default_model());
                let variants = snapshot.variants(&model);
                // `hasVariant` (service.ts:930-933): the value is valid
                // when it IS the persisted sentinel or names an existing
                // variant key.
                let valid = variants
                    .and_then(|variants| variants.as_object())
                    .map(|variants| {
                        value == DEFAULT_VARIANT_VALUE
                            || variants.keys().any(|variant| variant == value)
                    })
                    .unwrap_or(false);
                if !valid {
                    return Err(AcpError::invalid_params(json!({ "effort": value }), None));
                }
                self.sessions
                    .set_variant(session_id, Some(value.to_string()));
                let state = self.get_session(session_id)?;
                Ok(json!({
                    "configOptions": self.config_options(
                        &snapshot,
                        state.model.as_ref().unwrap_or(&model),
                        state.variant.as_deref(),
                        state.mode_id.as_deref(),
                    ),
                }))
            }
            "mode" => {
                if !snapshot.has_mode(value) {
                    return Err(AcpError::invalid_params(json!({ "mode": value }), None));
                }
                self.sessions.set_mode(session_id, Some(value.to_string()));
                let state = self.get_session(session_id)?;
                Ok(json!({
                    "configOptions": self.config_options(
                        &snapshot,
                        state
                            .model
                            .as_ref()
                            .unwrap_or(&snapshot.select_default_model()),
                        state.variant.as_deref(),
                        state.mode_id.as_deref(),
                    ),
                }))
            }
            _ => Err(invalid_config_option(config_id)),
        }
    }

    // ------------------------------------------------------------------
    // session/set_mode (service.ts:468-476)
    // ------------------------------------------------------------------

    async fn set_session_mode(self: &Arc<Self>, params: &Value) -> Result<Value, AcpError> {
        let session_id = params["sessionId"].as_str().unwrap_or_default();
        let current = self.get_session(session_id)?;
        let snapshot = self.config_snapshot(&current).await?;
        let mode_id = params["modeId"].as_str().unwrap_or_default();
        if !snapshot.has_mode(mode_id) {
            return Err(AcpError::invalid_params(json!({ "mode": mode_id }), None));
        }
        self.sessions
            .set_mode(session_id, Some(mode_id.to_string()));
        Ok(json!({}))
    }

    // ------------------------------------------------------------------
    // session/set_model (service.ts:478-495)
    // ------------------------------------------------------------------

    async fn set_session_model(self: &Arc<Self>, params: &Value) -> Result<Value, AcpError> {
        let session_id = params["sessionId"].as_str().unwrap_or_default();
        let current = self.get_session(session_id)?;
        let snapshot = self.config_snapshot(&current).await?;
        let model_id = params["modelId"].as_str().unwrap_or_default();
        let selected = parse_selected_model(&snapshot, model_id)?;
        let variant = select_model_variant(&snapshot, &current, &selected);
        self.sessions.set_variant(session_id, variant.clone());
        self.sessions.set_model(
            session_id,
            Some(json!({
                "providerID": selected["model"]["providerID"],
                "modelID": selected["model"]["modelID"],
            })),
        );
        let state = self.get_session(session_id)?;
        let options = self.config_options(
            &snapshot,
            state.model.as_ref().unwrap_or(&selected["model"]),
            state.variant.as_deref(),
            state.mode_id.as_deref(),
        );
        self.send_config_option_update(session_id, &options).await;
        Ok(json!({}))
    }

    // ------------------------------------------------------------------
    // session/prompt (service.ts:509-592)
    // ------------------------------------------------------------------

    async fn prompt(self: &Arc<Self>, params: &Value) -> Result<Value, AcpError> {
        let session_id = params["sessionId"].as_str().unwrap_or_default();
        let current = self.get_session(session_id)?;
        let snapshot = self.directory_snapshot(&current.cwd).await?;
        let selected = current
            .model
            .clone()
            .unwrap_or_else(|| snapshot.select_default_model());
        if current.model.is_none() {
            self.sessions.set_model(session_id, Some(selected.clone()));
        }
        let variant = current
            .variant
            .clone()
            .or_else(|| snapshot.select_variant(&selected));
        let mode_id = current.mode_id.clone().unwrap_or_else(|| {
            if !snapshot.available_modes.is_empty() {
                snapshot.default_mode_id.clone()
            } else {
                String::new()
            }
        });
        let parts = content::prompt_content_to_parts(
            params
                .get("prompt")
                .and_then(Value::as_array)
                .map(|blocks| blocks.as_slice())
                .unwrap_or_default(),
        );
        let command = detect_slash_command(&parts);

        let message_id = params.get("messageId").and_then(Value::as_str);

        if command.is_none() {
            let mut body = json!({
                "model": {
                    "providerID": selected["providerID"],
                    "modelID": selected["modelID"],
                },
                "parts": parts,
            });
            if let Some(variant) = &variant {
                body["variant"] = json!(variant);
            }
            if !mode_id.is_empty() {
                body["agent"] = json!(mode_id);
            }
            let response = self
                .events
                .run_until_idle(session_id, async {
                    self.server
                        .session_prompt(&current.cwd, session_id, body)
                        .await
                })
                .await
                .map_err(internal_error)?;
            usage::send_update(&self.connection, &self.server, &current.cwd, session_id).await;
            return prompt_response(response.get("info"), message_id).await;
        }

        let command = command.unwrap();
        let known = snapshot
            .available_commands
            .iter()
            .find(|item| item["name"].as_str() == Some(command.name.as_str()));
        if let Some(known) = known {
            let name = known["name"].as_str().unwrap_or_default().to_string();
            let mut body = json!({
                "command": name,
                "arguments": command.args,
                "model": format!("{}/{}", selected["providerID"].as_str().unwrap_or_default(), selected["modelID"].as_str().unwrap_or_default()),
            });
            if let Some(variant) = &variant {
                body["variant"] = json!(variant);
            }
            if !mode_id.is_empty() {
                body["agent"] = json!(mode_id);
            }
            let response = self
                .events
                .run_until_idle(session_id, async {
                    self.server
                        .session_command(&current.cwd, session_id, body)
                        .await
                })
                .await
                .map_err(internal_error)?;
            usage::send_update(&self.connection, &self.server, &current.cwd, session_id).await;
            return prompt_response(response.get("info"), message_id).await;
        }

        if command.name == "compact" {
            let _ = self
                .events
                .run_until_idle(session_id, async {
                    self.server
                        .session_summarize(
                            &current.cwd,
                            session_id,
                            json!({
                                "providerID": selected["providerID"],
                                "modelID": selected["modelID"],
                            }),
                        )
                        .await
                })
                .await
                .map_err(internal_error)?;
        }
        usage::send_update(&self.connection, &self.server, &current.cwd, session_id).await;
        prompt_response(None, message_id).await
    }

    // ------------------------------------------------------------------
    // helpers
    // ------------------------------------------------------------------

    fn get_session(&self, session_id: &str) -> Result<SessionInfo, AcpError> {
        self.sessions
            .get(session_id)
            .ok_or_else(|| AcpError::invalid_params(json!({ "sessionId": session_id }), None))
    }

    fn store_session(&self, session: SessionInfo) -> SessionInfo {
        self.sessions.create(session.clone());
        session
    }

    async fn directory_snapshot(&self, cwd: &str) -> Result<Arc<Snapshot>, AcpError> {
        self.directories.get(cwd).await.map_err(|_| {
            AcpError::internal_error(json!({ "details": "OpenCode service failure" }), None)
        })
    }

    /// `configSnapshot` (service.ts:155-161) — memoized per session.
    async fn config_snapshot(&self, current: &SessionInfo) -> Result<Arc<Snapshot>, AcpError> {
        if let Some(snapshot) = self.session_snapshots.lock().unwrap().get(&current.id) {
            return Ok(snapshot.clone());
        }
        self.directory_snapshot(&current.cwd).await
    }

    async fn abort_backing_session(&self, current: &SessionInfo) {
        if let Err(error) = self.server.session_abort(&current.cwd, &current.id).await {
            eprintln!(
                "failed to abort ACP backing session ({}): {}",
                current.id, error
            );
        }
    }

    async fn register_mcp_servers(&self, directory: &str, session_id: &str, servers: &[Value]) {
        let current = self
            .registered_mcp
            .lock()
            .unwrap()
            .entry(session_id.to_string())
            .or_default()
            .clone();
        for server in servers {
            let config = mcp_config(server);
            let name = server
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let key = format!("{name}:{}", stable_stringify(&config));
            if current.contains(&key) {
                continue;
            }
            if let Err(error) = self
                .server
                .mcp_add(directory, json!({ "name": name, "config": config }))
                .await
            {
                eprintln!("failed to register MCP server {name}: {error}");
            }
            self.registered_mcp
                .lock()
                .unwrap()
                .entry(session_id.to_string())
                .or_default()
                .insert(key);
        }
    }

    /// `sendAvailableCommands` (service.ts:988-1008) — `setTimeout(0)`
    /// fires the notification after the response.
    async fn send_available_commands(&self, session_id: &str, snapshot: &Snapshot) {
        let connection = self.connection.clone();
        let session_id = session_id.to_string();
        let commands = snapshot.available_commands.clone();
        tokio::spawn(async move {
            let _ = connection
                .send_notification(
                    "session/update",
                    json!({
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "available_commands_update",
                            "availableCommands": commands
                                .iter()
                                .map(|command| json!({
                                    "name": command["name"],
                                    "description": command.get("description").cloned().unwrap_or_else(|| json!("")),
                                }))
                                .collect::<Vec<_>>(),
                        },
                    }),
                )
                .await;
        });
    }

    async fn send_config_option_update(&self, session_id: &str, options: &[Value]) {
        let _ = self
            .connection
            .send_notification(
                "session/update",
                json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "config_option_update",
                        "configOptions": options,
                    },
                }),
            )
            .await;
    }

    fn config_options(
        &self,
        snapshot: &Snapshot,
        model: &Value,
        variant: Option<&str>,
        mode_id: Option<&str>,
    ) -> Vec<Value> {
        let providers: Vec<Value> = snapshot
            .providers
            .as_object()
            .map(|providers| providers.values().cloned().collect())
            .unwrap_or_default();
        let mode_id = mode_id.filter(|mode| !mode.is_empty());
        config_option::build_config_options(
            &providers,
            model,
            variant,
            Some(&snapshot.available_modes),
            mode_id,
        )
    }
}

/// The blocking line read runs on the async runtime's blocking pool.
fn read_line(transport: &dyn Transport) -> Option<String> {
    transport.read()
}

/// `promptResponse` (service.ts:839-889).
async fn prompt_response(
    info: Option<&Value>,
    message_id: Option<&str>,
) -> Result<Value, AcpError> {
    let info = info.filter(|info| !info.is_null());
    let error = info
        .and_then(|info| info.get("error"))
        .filter(|e| !e.is_null());
    let usage = info.map(usage::build_usage);

    if error.is_none() {
        let mut response = json!({
            "stopReason": "end_turn",
            "_meta": {},
        });
        if let Some(usage) = &usage {
            response["usage"] = usage.clone();
        }
        if let Some(message_id) = message_id {
            response["userMessageId"] = json!(message_id);
        }
        return Ok(response);
    }
    let error = error.unwrap();
    let name = error
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut response = json!({ "_meta": {} });
    if let Some(usage) = &usage {
        response["usage"] = usage.clone();
    }
    if let Some(message_id) = message_id {
        response["userMessageId"] = json!(message_id);
    }
    match name {
        "MessageAbortedError" => {
            response["stopReason"] = json!("cancelled");
            Ok(response)
        }
        "MessageOutputLengthError" => {
            response["stopReason"] = json!("max_tokens");
            Ok(response)
        }
        "ContentFilterError" => {
            response["stopReason"] = json!("refusal");
            Ok(response)
        }
        "ProviderAuthError" => Err(AcpError::auth_required(
            json!({ "providerId": error["data"]["providerID"] }),
            None,
        )),
        _ => {
            let message = prompt_error_message(error);
            Err(AcpError::internal_error(
                json!({
                    "service": "session",
                    "errorName": name,
                }),
                Some(&message),
            ))
        }
    }
}

fn prompt_error_message(error: &Value) -> String {
    error
        .pointer("/data/message")
        .and_then(Value::as_str)
        .unwrap_or("OpenCode prompt failed")
        .to_string()
}

/// `detectSlashCommand` (service.ts:826-837).
fn detect_slash_command(parts: &[Value]) -> Option<SlashCommand> {
    let text = parts
        .iter()
        .filter(|part| part["type"] == json!("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
        .trim()
        .to_string();
    if !text.starts_with('/') {
        return None;
    }
    let text = &text[1..];
    let mut words = text.split_whitespace();
    let name = words.next()?.to_string();
    if name.is_empty() {
        return None;
    }
    let args = words.collect::<Vec<_>>().join(" ");
    Some(SlashCommand {
        name,
        args: args.trim().to_string(),
    })
}

struct SlashCommand {
    name: String,
    args: String,
}

/// `invalidParams` shapes from error.ts:63-93.
fn invalid_config_option(config_id: &str) -> AcpError {
    AcpError::invalid_params(json!({ "configId": config_id }), None)
}

fn internal_error(message: String) -> AcpError {
    AcpError::internal_error(json!({ "details": message }), None)
}

/// `mcpConfig` (service.ts:1065-1078).
fn mcp_config(server: &Value) -> Value {
    if server.get("type").is_some() {
        let headers = server
            .get("headers")
            .and_then(Value::as_array)
            .map(|headers| {
                json!(serde_json::Map::from_iter(
                    headers
                        .iter()
                        .filter_map(|header| {
                            Some((
                                (header.get("name")?.as_str()?.to_string()),
                                json!(header.get("value")?.as_str()?),
                            ))
                        })
                        .collect::<Vec<(String, Value)>>(),
                ))
            })
            .unwrap_or_else(|| json!({}));
        return json!({
            "type": "remote",
            "url": server["url"],
            "headers": headers,
        });
    }
    let command = server
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut full_command = vec![json!(command)];
    full_command.extend(
        server
            .get("args")
            .and_then(Value::as_array)
            .map(|args| args.iter().map(|arg| json!(arg)).collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    let environment = server
        .get("env")
        .and_then(Value::as_array)
        .map(|env| {
            serde_json::Map::from_iter(
                env.iter()
                    .filter_map(|entry| {
                        Some((
                            entry.get("name")?.as_str()?.to_string(),
                            json!(entry.get("value")?.as_str()?),
                        ))
                    })
                    .collect::<Vec<(String, Value)>>(),
            )
        })
        .map(Value::Object)
        .unwrap_or_else(|| json!({}));
    json!({
        "type": "local",
        "command": full_command,
        "environment": environment,
    })
}

/// `stableStringify` (service.ts:1080-1087).
fn stable_stringify(value: &Value) -> String {
    match value {
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(stable_stringify)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(object) => {
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap(),
                        stable_stringify(&object.get(key.as_str()).cloned().unwrap_or(Value::Null))
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn mcp_servers_of(params: &Value) -> Vec<Value> {
    params
        .get("mcpServers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|at| at.as_millis() as u64)
        .unwrap_or_default()
}

/// Epoch-ms to ISO timestamp (the TS `new Date(...).toISOString()`).
fn iso_ms(value: Option<&Value>) -> Value {
    let Some(at) = value.and_then(Value::as_f64) else {
        return Value::Null;
    };
    let Some(at) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(at as i64) else {
        return Value::Null;
    };
    json!(at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// `restoreSession` (service.ts:1089-1102).
struct Restored {
    model: Option<Value>,
    variant: Option<String>,
    mode_id: Option<String>,
}

fn restore_session(snapshot: &Snapshot, backing: &Value, messages: &[Value]) -> Restored {
    let history = restore_from_messages(messages);
    let durable = restore_durable_model(backing.get("model"));
    let model = restore_model(snapshot, durable.model.clone(), history.model.clone());
    Restored {
        variant: restore_variant(snapshot, &model, &durable, &history),
        mode_id: restore_mode(snapshot, backing.get("agent"), history.mode_id),
        model: Some(model),
    }
}

/// `restoreDurableModel` (service.ts:1104-1113).
fn restore_durable_model(model: Option<&Value>) -> DurableModel {
    let Some(model) = model else {
        return DurableModel::default();
    };
    DurableModel {
        model: Some(json!({
            "providerID": model["providerID"],
            "modelID": model["id"],
        })),
        variant: model
            .get("variant")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

#[derive(Default)]
struct DurableModel {
    model: Option<Value>,
    variant: Option<String>,
}

/// `restoreModel` (service.ts:1115-1123).
fn restore_model(snapshot: &Snapshot, durable: Option<Value>, history: Option<Value>) -> Value {
    if let Some(durable) = durable {
        if snapshot.has_model(&durable) {
            return durable;
        }
    }
    if let Some(history) = history {
        if snapshot.has_model(&history) {
            return history;
        }
    }
    snapshot.select_default_model()
}

/// `restoreVariant` (service.ts:1125-1138).
fn restore_variant(
    snapshot: &Snapshot,
    model: &Value,
    durable: &DurableModel,
    history: &RestoredHistory,
) -> Option<String> {
    let variants = snapshot.variants(model)?;
    let has = |variant: &str| variants.get(variant).is_some() || variant == DEFAULT_VARIANT_VALUE;
    if let Some(durable_model) = &durable.model {
        if same_model(model, Some(durable_model)) {
            if let Some(variant) = &durable.variant {
                if has(variant) {
                    return Some(variant.clone());
                }
            }
        }
    }
    if let Some(history_model) = &history.model {
        if same_model(model, Some(history_model)) {
            if let Some(variant) = &history.variant {
                if has(variant) {
                    return Some(variant.clone());
                }
            }
        }
    }
    snapshot.select_variant(model)
}

/// `restoreMode` (service.ts:1140-1144).
fn restore_mode(
    snapshot: &Snapshot,
    durable: Option<&Value>,
    history: Option<String>,
) -> Option<String> {
    let durable = durable.and_then(Value::as_str);
    if let Some(durable) = durable {
        if snapshot.has_mode(durable) {
            return Some(durable.to_string());
        }
    }
    if let Some(history) = &history {
        if snapshot.has_mode(history) {
            return Some(history.clone());
        }
    }
    if !snapshot.available_modes.is_empty() {
        return Some(snapshot.default_mode_id.clone());
    }
    None
}

/// `restoreFromMessages` (service.ts:1158-1180).
struct RestoredHistory {
    model: Option<Value>,
    variant: Option<String>,
    mode_id: Option<String>,
}

fn restore_from_messages(messages: &[Value]) -> RestoredHistory {
    let user = messages.iter().rev().find(|message| {
        !message["model"]["providerID"].is_null()
            && !message["model"]["modelID"].is_null()
            && message["role"] == json!("user")
    });
    if let Some(message) = user {
        return RestoredHistory {
            model: Some(json!({
                "providerID": message["model"]["providerID"],
                "modelID": message["model"]["modelID"],
            })),
            variant: message
                .get("model")
                .and_then(|m| m.get("variant"))
                .and_then(Value::as_str)
                .map(str::to_string),
            mode_id: message
                .get("agent")
                .and_then(Value::as_str)
                .map(str::to_string),
        };
    }
    let assistant = messages
        .iter()
        .rev()
        .find(|message| !message["providerID"].is_null() && !message["modelID"].is_null());
    if let Some(message) = assistant {
        return RestoredHistory {
            model: Some(json!({
                "providerID": message["providerID"],
                "modelID": message["modelID"],
            })),
            variant: message
                .get("variant")
                .and_then(Value::as_str)
                .map(str::to_string),
            mode_id: message
                .get("mode")
                .or_else(|| message.get("agent"))
                .and_then(Value::as_str)
                .map(str::to_string),
        };
    }
    RestoredHistory {
        model: None,
        variant: None,
        mode_id: None,
    }
}

/// `sameModel` (service.ts:1154-1156).
fn same_model(left: &Value, right: Option<&Value>) -> bool {
    match (left, right) {
        (left, Some(right)) => {
            left["providerID"] == right["providerID"] && left["modelID"] == right["modelID"]
        }
        _ => false,
    }
}

/// `parseSelectedModel` (service.ts:964-986).
fn parse_selected_model(snapshot: &Snapshot, model_id: &str) -> Result<Value, AcpError> {
    let providers: Vec<Value> = snapshot
        .providers
        .as_object()
        .map(|providers| providers.values().cloned().collect())
        .unwrap_or_default();
    let selection = config_option::parse_model_selection(model_id, &providers);
    let provider_id = selection["model"]["providerID"]
        .as_str()
        .unwrap_or_default();
    let model_id = selection["model"]["modelID"].as_str().unwrap_or_default();
    let model = snapshot
        .providers
        .get(provider_id)
        .and_then(|provider| provider.get("models"))
        .and_then(|models| models.get(model_id));
    let Some(_model) = model else {
        return Err(AcpError::invalid_params(
            json!({ "providerId": provider_id, "modelId": model_id }),
            None,
        ));
    };
    if let Some(variant) = selection.get("variant").and_then(Value::as_str) {
        if _model
            .get("variants")
            .and_then(|variants| variants.get(variant))
            .is_none()
        {
            return Err(AcpError::invalid_params(json!({ "effort": variant }), None));
        }
    }
    Ok(selection)
}

/// `selectModelVariant` (service.ts:917-928).
fn select_model_variant(
    snapshot: &Snapshot,
    current: &SessionInfo,
    selected: &Value,
) -> Option<String> {
    let model = selected.get("model").cloned().unwrap_or(Value::Null);
    let variants = snapshot.variants(&model)?;
    if let Some(variant) = selected.get("variant").and_then(Value::as_str) {
        return Some(variant.to_string());
    }
    if same_model(&model, current.model.as_ref())
        && current
            .variant
            .as_deref()
            .map(|variant| variants.get(variant).is_some() || variant == DEFAULT_VARIANT_VALUE)
            .unwrap_or(false)
    {
        return current.variant.clone();
    }
    snapshot.select_variant(&model)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn slash_command_detection() {
        let parts = vec![json!({ "type": "text", "text": "  /compact now " })];
        let command = detect_slash_command(&parts).unwrap();
        assert_eq!(command.name, "compact");
        assert_eq!(command.args, "now");
        let parts = vec![json!({ "type": "text", "text": "plain text" })];
        assert!(detect_slash_command(&parts).is_none());
    }

    #[test]
    fn stable_stringify_sorts_keys() {
        let value = json!({ "b": 1, "a": { "d": 2, "c": 3 } });
        assert_eq!(stable_stringify(&value), r#"{"a":{"c":3,"d":2},"b":1}"#);
    }

    #[test]
    fn mcp_remote_config_maps_headers() {
        let config = mcp_config(&json!({
            "type": "remote",
            "url": "https://mcp.example.com",
            "headers": [{ "name": "auth", "value": "token" }],
        }));
        assert_eq!(config["type"], json!("remote"));
        assert_eq!(config["headers"]["auth"], json!("token"));
    }

    #[test]
    fn mcp_local_config_collects_command() {
        let config = mcp_config(&json!({
            "command": "npx",
            "args": ["server.js"],
            "env": [{ "name": "DEBUG", "value": "1" }],
        }));
        assert_eq!(config["type"], json!("local"));
        assert_eq!(config["command"], json!(["npx", "server.js"]));
        assert_eq!(config["environment"]["DEBUG"], json!("1"));
    }
}
