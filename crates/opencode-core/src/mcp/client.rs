//! MCP protocol client — the `Client` handshake and typed requests over
//! the two transports (SDK `client/index.js`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};

use crate::mcp::catalog::{self, McpToolDef};
use crate::mcp::transport::{
    initialize_params, McpError, StdioTransport, SUPPORTED_PROTOCOL_VERSIONS,
};

/// One connected client over a transport.
pub enum Transport {
    Stdio(StdioTransport),
    Http(HttpHandle),
}

/// The HTTP transport behind a mutex: requests serialize through the
/// shared session-id/protocol-version state.
pub struct HttpHandle {
    url: String,
    transport: tokio::sync::Mutex<super::transport::HttpTransport>,
}

impl HttpHandle {
    pub fn new(url: &str, transport: super::transport::HttpTransport) -> HttpHandle {
        HttpHandle {
            url: url.to_string(),
            transport: tokio::sync::Mutex::new(transport),
        }
    }
}

pub struct McpClient {
    transport: Transport,
    next_id: AtomicU64,
    server_capabilities: Option<Value>,
    instructions: Option<String>,
    timeout_ms: u64,
}

impl McpClient {
    /// `client.connect(transport)` (client/index.js:291-322): the
    /// `initialize` handshake plus `notifications/initialized`, wrapped
    /// in `withTimeout` (index.ts:226).
    pub async fn connect(transport: Transport, timeout_ms: u64) -> Result<McpClient, McpError> {
        let client = McpClient {
            transport,
            next_id: AtomicU64::new(0),
            server_capabilities: None,
            instructions: None,
            timeout_ms,
        };
        let timeout = Duration::from_millis(timeout_ms);
        let result = tokio::time::timeout(
            timeout,
            client.request("initialize", Some(initialize_params())),
        )
        .await
        .map_err(|_| McpError::failed("timeout"))??;
        let protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&protocol_version) {
            return Err(McpError::failed(format!(
                "Server's protocol version is not supported: {protocol_version}"
            )));
        }
        client.notify("notifications/initialized", None).await?;
        let mut this = client;
        this.server_capabilities = result.get("capabilities").cloned();
        this.instructions = result
            .get("instructions")
            .and_then(Value::as_str)
            .map(String::from);
        if let Transport::Http(http) = &this.transport {
            let mut guard = http.transport.lock().await;
            guard.protocol_version = Some(protocol_version.to_string());
        }
        Ok(this)
    }

    /// `getServerCapabilities()` (client/index.js:323).
    pub fn server_capabilities(&self) -> Option<&Value> {
        self.server_capabilities.as_ref()
    }

    /// `assertCapability` (client/index.js:278-282).
    fn assert_capability(&self, capability: &str, method: &str) -> Result<(), McpError> {
        let has = self
            .server_capabilities
            .as_ref()
            .and_then(|caps| caps.get(capability))
            .is_some();
        if has {
            Ok(())
        } else {
            Err(McpError::failed(format!(
                "Server does not support {capability} (required for {method})"
            )))
        }
    }

    /// `getInstructions()` (client/index.js:597) — trimmed (index.ts:399).
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref().map(str::trim)
    }

    pub fn stdio_pid(&self) -> Option<u32> {
        match &self.transport {
            Transport::Stdio(stdio) => stdio.pid,
            Transport::Http(_) => None,
        }
    }

    async fn send_message(&self, message: Value) -> Result<(), McpError> {
        match &self.transport {
            Transport::Stdio(stdio) => stdio.send(&message).await,
            Transport::Http(http) => {
                let guard = http.transport.lock().await;
                guard.notify(&message).await
            }
        }
    }

    fn check_error(response: Value) -> Result<Value, McpError> {
        if let Some(error) = response.get("error") {
            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let unauthorized = code == -32001 || message.to_lowercase().contains("unauthorized");
            return Err(McpError {
                message: message.to_string(),
                unauthorized,
            });
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn request(&self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        let mut message = json!({
            "jsonrpc": "2.0",
            "id": self.next_id.fetch_add(1, Ordering::SeqCst),
            "method": method,
        });
        if let Some(params) = params {
            message["params"] = params;
        }
        let timeout = Duration::from_millis(self.timeout_ms);
        match &self.transport {
            Transport::Stdio(stdio) => {
                let id = message["id"].clone();
                stdio.send(&message).await?;
                let response = tokio::time::timeout(
                    timeout,
                    stdio.recv_until(|value| value.get("id").is_some() && value["id"] == id),
                )
                .await
                .map_err(|_| McpError::failed("timeout"))??;
                Self::check_error(response)
            }
            Transport::Http(http) => {
                let mut guard = http.transport.lock().await;
                let response = tokio::time::timeout(timeout, guard.send(&message))
                    .await
                    .map_err(|_| McpError::failed("timeout"))??;
                match response {
                    Some(response) => Self::check_error(response),
                    None => Err(McpError::failed("missing response")),
                }
            }
        }
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        let mut message = json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send_message(message).await
    }

    /// `listTools` (client/index.js:573-589) through `McpCatalog.defs`
    /// pagination; raw-JSON parsing already tolerates every `outputSchema`
    /// the TS schema validation rejects (catalog.ts:14-16).
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError> {
        self.assert_capability("tools", "tools/list")?;
        let listed = catalog::paginate(
            |cursor| async move {
                let params = cursor.map(|cursor| json!({ "cursor": cursor }));
                self.request("tools/list", params)
                    .await
                    .map_err(|e| e.message)
            },
            |page| {
                page.get("tools")
                    .and_then(Value::as_array)
                    .map(|tools| {
                        tools
                            .iter()
                            .map(|tool| McpToolDef {
                                name: tool
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                description: tool
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .map(String::from),
                                input_schema: tool.get("inputSchema").cloned().unwrap_or_default(),
                            })
                            .collect::<Vec<_>>()
                    })
                    .ok_or_else(|| "invalid tools page".to_string())
            },
        )
        .await;
        listed.map_err(McpError::failed)
    }

    /// `callTool` (client/index.js:496-542).
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
        self.assert_capability("tools", "tools/call")?;
        self.request(
            "tools/call",
            Some(json!({"name": name, "arguments": arguments})),
        )
        .await
    }

    /// `McpCatalog.prompts` (catalog.ts:121-127).
    pub async fn list_prompts(&self) -> Result<Vec<Value>, McpError> {
        self.assert_capability("prompts", "prompts/list")?;
        self.list_items("prompts/list", "prompts").await
    }

    /// `McpCatalog.resources` (catalog.ts:129-135).
    pub async fn list_resources(&self) -> Result<Vec<Value>, McpError> {
        self.assert_capability("resources", "resources/list")?;
        self.list_items("resources/list", "resources").await
    }

    /// `McpCatalog.resourceTemplates` (catalog.ts:137-143).
    pub async fn list_resource_templates(&self) -> Result<Vec<Value>, McpError> {
        self.assert_capability("resources", "resources/templates/list")?;
        self.list_items("resources/templates/list", "resourceTemplates")
            .await
    }

    async fn list_items(&self, method: &str, key: &str) -> Result<Vec<Value>, McpError> {
        let listed = catalog::paginate(
            |cursor| async move {
                let params = cursor.map(|cursor| json!({ "cursor": cursor }));
                self.request(method, params).await.map_err(|e| e.message)
            },
            |page| {
                page.get(key)
                    .and_then(Value::as_array)
                    .cloned()
                    .ok_or_else(|| format!("invalid {key} page"))
            },
        )
        .await;
        listed.map_err(McpError::failed)
    }

    /// `getPrompt` (client/index.js:470-472).
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<Value, McpError> {
        self.assert_capability("prompts", "prompts/get")?;
        let params = match arguments {
            Some(arguments) => json!({"name": name, "arguments": arguments}),
            None => json!({"name": name}),
        };
        self.request("prompts/get", Some(params)).await
    }

    /// `readResource` (client/index.js:482-484).
    pub async fn read_resource(&self, uri: &str) -> Result<Value, McpError> {
        self.assert_capability("resources", "resources/read")?;
        self.request("resources/read", Some(json!({ "uri": uri })))
            .await
    }

    /// `close` (client/index.js:220-238) — stdio kills the child, HTTP
    /// sends the session DELETE (`terminateSession`,
    /// streamableHttp.js:466-485).
    pub async fn close(&self) {
        match &self.transport {
            Transport::Stdio(stdio) => stdio.close().await,
            Transport::Http(http) => {
                let guard = http.transport.lock().await;
                let Some(session_id) = guard.session_id.clone() else {
                    return;
                };
                let _ = reqwest::Client::new()
                    .delete(&http.url)
                    .header("mcp-session-id", session_id)
                    .send()
                    .await;
            }
        }
    }
}
