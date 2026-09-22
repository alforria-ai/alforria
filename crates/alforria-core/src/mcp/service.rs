//! MCP service — port of `mcp/index.ts` (`MCP.Service`).
//!
//! One service instance per opencode instance (TS `InstanceState.make`,
//! index.ts:492-560). The state — runtime configs, statuses, connected
//! clients, tool defs and server instructions — initializes lazily on
//! first use, connecting every configured server.
//!
//! Divergences from TS (M7.6 report): the legacy `SSEClientTransport`
//! fallback for remote servers is not ported (streamable HTTP only), live
//! `ToolListChanged`/`LoggingMessage` notifications, the process-wide
//! descendant kill and the auth-required TUI toasts are not ported.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use alforria_schema::mcp::McpStatus;
use serde_json::{json, Value};

use crate::config::schema::{McpEntry, McpInfo, McpLocal, McpOAuthSetting, McpRemote};
use crate::mcp::auth::{McpAuth, McpTokens};
use crate::mcp::catalog::{self, McpToolDef};
use crate::mcp::client::{HttpHandle, McpClient, Transport};
use crate::mcp::oauth::{
    authorization_url, exchange_code, McpBrowser, OAuthCallbackServer, OAuthConfig, SystemBrowser,
};
use crate::mcp::transport::{HttpTransport, McpError, StdioTransport};

/// `McpBrowserOpenFailed` / `McpToolsChanged` event types
/// (`McpEvent`, schema-src/mcp-event.ts).
pub const TOOLS_CHANGED: crate::event::definition::Definition =
    crate::event::definition::Definition::ephemeral("mcp.tools.changed");
pub const BROWSER_OPEN_FAILED: crate::event::definition::Definition =
    crate::event::definition::Definition::ephemeral("mcp.browser.open.failed");

/// `NotFoundError` (index.ts:69-71).
#[derive(Debug, Clone, PartialEq)]
pub struct NotFoundError {
    pub name: String,
}

/// `MCP.McpTool` (index.ts:157-162).
pub struct McpTool {
    pub server: String,
    pub def: McpToolDef,
    pub timeout: Option<u64>,
    pub client: Arc<McpClient>,
}

/// `ServerInstructions` (index.ts:150-154).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ServerInstructions {
    pub name: String,
    pub instructions: String,
    pub tools: Vec<String>,
}

/// `AuthStatus` (index.ts:996).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStatus {
    Authenticated,
    Expired,
    NotAuthenticated,
}

/// One prompt/resource item augmented with its owning client
/// (index.ts:690-714).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ClientItem {
    pub item: Value,
    pub client: String,
}

/// `ConnectRemote` URL validation (index.ts:123-125, 242-247).
fn remote_url(value: &str) -> Option<reqwest::Url> {
    reqwest::Url::parse(value)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
}

/// `isMcpConfigured` (index.ts:119-121) — `{enabled}` markers are not
/// configured servers.
fn is_mcp_configured(entry: &McpEntry) -> bool {
    matches!(entry, McpEntry::Server(_))
}

fn info_enabled(info: &McpInfo) -> Option<bool> {
    match info {
        McpInfo::Local(local) => local.enabled,
        McpInfo::Remote(remote) => remote.enabled,
    }
}

fn info_timeout(info: &McpInfo) -> Option<u64> {
    match info {
        McpInfo::Local(local) => local.timeout.map(|t| t.get()),
        McpInfo::Remote(remote) => remote.timeout.map(|t| t.get()),
    }
}

/// Local alias for the per-client tools tuple in [`McpService::tools`].
type ClientTools = (String, Arc<McpClient>, Vec<McpToolDef>, Option<u64>);

/// The state struct (index.ts:142-148).
#[derive(Default)]
struct State {
    config: BTreeMap<String, McpInfo>,
    status: BTreeMap<String, McpStatus>,
    clients: BTreeMap<String, Arc<McpClient>>,
    defs: BTreeMap<String, Vec<McpToolDef>>,
    instructions: BTreeMap<String, String>,
}

/// Service input (per instance).
pub struct McpServiceInput {
    /// The instance directory — stdio `cwd` base and the roots capability.
    pub directory: PathBuf,
    /// `Global.Path.data` — `mcp-auth.json` lives here.
    pub data_dir: PathBuf,
    /// The merged config's `mcp` map (a snapshot; config file changes need
    /// an instance reload).
    pub mcp: BTreeMap<String, McpEntry>,
    /// `cfg.experimental.mcp_timeout` — the fallback request timeout.
    pub mcp_timeout: Option<u64>,
    /// The instance bus for `mcp.tools.changed` emissions, when available.
    pub events: Option<Arc<crate::event::bus::EventBus>>,
}

/// `MCP.Service`.
#[derive(Clone)]
pub struct McpService {
    state: Arc<tokio::sync::OnceCell<Arc<tokio::sync::Mutex<State>>>>,
    input: Arc<McpServiceInput>,
    pub auth: McpAuth,
    pub browser: Arc<dyn McpBrowser>,
    pub callback: OAuthCallbackServer,
    pending_oauth: Arc<tokio::sync::Mutex<Vec<String>>>,
}

impl McpService {
    pub fn new(input: McpServiceInput) -> McpService {
        let auth = McpAuth::new(input.data_dir.join("mcp-auth.json"));
        McpService {
            state: Arc::new(tokio::sync::OnceCell::new()),
            input: Arc::new(input),
            auth,
            browser: Arc::new(SystemBrowser),
            callback: OAuthCallbackServer::new(),
            pending_oauth: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    async fn state(&self) -> Arc<tokio::sync::Mutex<State>> {
        Arc::clone(
            self.state
                .get_or_init(|| async {
                    let state = Arc::new(tokio::sync::Mutex::new(State::default()));
                    // `Effect.forEach` over the config entries (index.ts:505-529).
                    for (key, entry) in &self.input.mcp {
                        if !is_mcp_configured(entry) {
                            continue;
                        }
                        let info = match entry {
                            McpEntry::Server(info) => info.clone(),
                            _ => continue,
                        };
                        if info_enabled(&info) == Some(false) {
                            state
                                .lock()
                                .await
                                .status
                                .insert(key.clone(), McpStatus::Disabled);
                            continue;
                        }
                        let result = match create(self, key, info).await {
                            Ok(result) => result,
                            Err(err) => CreateResult {
                                status: McpStatus::Failed { error: err.message },
                                client: None,
                                defs: Vec::new(),
                                instructions: None,
                            },
                        };
                        let mut state_guard = state.lock().await;
                        state_guard.status.insert(key.clone(), result.status);
                        if let Some(client) = result.client {
                            state_guard.clients.insert(key.clone(), client);
                            state_guard.defs.insert(key.clone(), result.defs);
                            if let Some(instructions) = result.instructions {
                                state_guard.instructions.insert(key.clone(), instructions);
                            }
                        }
                    }
                    state
                })
                .await,
        )
    }

    /// `requestTimeout` (index.ts:661-664).
    fn request_timeout(&self, state: &State, name: &str) -> Option<u64> {
        if let Some(info) = state.config.get(name) {
            let timeout = info_timeout(info);
            if timeout.is_some() {
                return timeout;
            }
        }
        if let Some(entry) = self.input.mcp.get(name) {
            if is_mcp_configured(entry) {
                let info = match entry {
                    McpEntry::Server(info) => info,
                    _ => unreachable!("is_mcp_configured checked"),
                };
                if info_timeout(info).is_some() {
                    return info_timeout(info);
                }
            }
        }
        self.input.mcp_timeout
    }

    // -----------------------------------------------------------------------
    // Interface (index.ts:164-198)
    // -----------------------------------------------------------------------

    /// `status` (index.ts:591-608).
    pub async fn status(&self) -> BTreeMap<String, McpStatus> {
        let state = self.state().await;
        let state = state.lock().await;
        let mut result = BTreeMap::new();
        for (key, entry) in &self.input.mcp {
            if !is_mcp_configured(entry) {
                continue;
            }
            result.insert(
                key.clone(),
                state
                    .status
                    .get(key)
                    .cloned()
                    .unwrap_or(McpStatus::Disabled),
            );
        }
        for key in state.config.keys() {
            result.insert(
                key.clone(),
                state
                    .status
                    .get(key)
                    .cloned()
                    .unwrap_or(McpStatus::Disabled),
            );
        }
        result
    }

    /// `clients` (index.ts:610-613).
    pub async fn clients(&self) -> BTreeMap<String, Arc<McpClient>> {
        let state = self.state().await;
        let state = state.lock().await;
        state.clients.clone()
    }

    /// `instructions` (index.ts:615-625): connected servers only, sorted
    /// by name.
    pub async fn instructions(&self) -> Vec<ServerInstructions> {
        let state = self.state().await;
        let state = state.lock().await;
        let mut names: Vec<&String> = state
            .instructions
            .keys()
            .filter(|name| state.status.get(*name) == Some(&McpStatus::Connected))
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|name| ServerInstructions {
                name: name.clone(),
                instructions: state.instructions[name].clone(),
                tools: state
                    .defs
                    .get(name)
                    .map(|defs| {
                        defs.iter()
                            .map(|tool| catalog::tool_name(name, &tool.name))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            })
            .collect()
    }

    /// `tools` (index.ts:666-688).
    pub async fn tools(&self) -> BTreeMap<String, McpTool> {
        let state = self.state().await;
        let result: Vec<ClientTools> = {
            let state = state.lock().await;
            state
                .clients
                .iter()
                .filter(|(name, _)| state.status.get(*name) == Some(&McpStatus::Connected))
                .filter_map(|(name, client)| {
                    state.defs.get(name).map(|defs| {
                        (
                            name.clone(),
                            Arc::clone(client),
                            defs.clone(),
                            self.request_timeout(&state, name),
                        )
                    })
                })
                .collect()
        };
        let mut out = BTreeMap::new();
        for (client_name, client, defs, timeout) in result {
            for def in defs {
                out.insert(
                    catalog::tool_name(&client_name, &def.name),
                    McpTool {
                        server: client_name.clone(),
                        def,
                        timeout,
                        client: Arc::clone(&client),
                    },
                );
            }
        }
        out
    }

    /// `collectFromConnected` (index.ts:690-714) for prompts and
    /// resources.
    async fn collect(
        &self,
        kind: ItemKind,
        key: &'static str,
        target_client: Option<&str>,
    ) -> BTreeMap<String, ClientItem> {
        let state = self.state().await;
        let connected: Vec<(String, Arc<McpClient>)> = {
            let state = state.lock().await;
            state
                .clients
                .iter()
                .filter(|(name, _)| state.status.get(*name) == Some(&McpStatus::Connected))
                .filter(|(name, _)| {
                    target_client
                        .map(|target| name.as_str() == target)
                        .unwrap_or(true)
                })
                .map(|(name, client)| (name.clone(), Arc::clone(client)))
                .collect()
        };
        let mut result = BTreeMap::new();
        for (client_name, client) in connected {
            let listed = match kind {
                ItemKind::Prompts => client.list_prompts().await,
                ItemKind::Resources => client.list_resources().await,
                ItemKind::ResourceTemplates => client.list_resource_templates().await,
            };
            let Ok(items) = listed else {
                continue;
            };
            for item in items {
                let item_key = match kind {
                    ItemKind::ResourceTemplates => format!(
                        "{}:{}",
                        catalog::escape_client(&client_name),
                        item.get(key).and_then(Value::as_str).unwrap_or_default()
                    ),
                    _ => format!(
                        "{}:{}",
                        catalog::sanitize(&client_name),
                        catalog::sanitize(
                            item.get(key).and_then(Value::as_str).unwrap_or_default()
                        )
                    ),
                };
                result.insert(
                    item_key,
                    ClientItem {
                        item,
                        client: client_name.clone(),
                    },
                );
            }
        }
        result
    }

    /// `prompts` (index.ts:716-718).
    pub async fn prompts(&self) -> BTreeMap<String, ClientItem> {
        self.collect(ItemKind::Prompts, "name", None).await
    }

    /// `resources` (index.ts:720-729).
    pub async fn resources(&self, client_name: Option<&str>) -> BTreeMap<String, ClientItem> {
        self.collect(ItemKind::Resources, "uri", client_name).await
    }

    /// `resourceTemplates` (index.ts:730-738).
    pub async fn resource_templates(
        &self,
        client_name: Option<&str>,
    ) -> BTreeMap<String, ClientItem> {
        self.collect(ItemKind::ResourceTemplates, "uriTemplate", client_name)
            .await
    }

    /// `getPrompt` (index.ts:768-779) — `Ok(None)` for missing clients
    /// (`withClient`, index.ts:740-766).
    pub async fn get_prompt(
        &self,
        client_name: &str,
        name: &str,
        arguments: Option<BTreeMap<String, String>>,
    ) -> Option<Value> {
        let client = self.client(client_name).await;
        let client = client?;
        client
            .get_prompt(
                name,
                arguments.map(|arguments| serde_json::to_value(arguments).unwrap_or(Value::Null)),
            )
            .await
            .ok()
    }

    /// `readResource` (index.ts:781-788). `Ok(None)` — missing client
    /// (`withClient` yields undefined); `Err` — the client error message.
    pub async fn read_resource(
        &self,
        client_name: &str,
        resource_uri: &str,
    ) -> Result<Option<Value>, String> {
        let Some(client) = self.client(client_name).await else {
            return Ok(None);
        };
        client
            .read_resource(resource_uri)
            .await
            .map(Some)
            .map_err(|err| err.message)
    }

    async fn client(&self, client_name: &str) -> Option<Arc<McpClient>> {
        let state = self.state().await;
        let state = state.lock().await;
        state.clients.get(client_name).cloned()
    }

    /// `add` (index.ts:641-646).
    pub async fn add(&self, name: &str, mcp: McpInfo) -> BTreeMap<String, McpStatus> {
        let state = self.state().await;
        state
            .lock()
            .await
            .config
            .insert(name.to_string(), mcp.clone());
        let status = self.create_and_store(name, mcp).await;
        let mut result = self.status().await;
        result.insert(name.to_string(), status);
        result
    }

    /// `connect` (index.ts:648-651).
    pub async fn connect(&self, name: &str) -> Result<(), NotFoundError> {
        let state = self.state().await;
        let mcp = {
            let state = state.lock().await;
            get_mcp_config(self, &state, name)?
        };
        self.create_and_store(name, mcp).await;
        Ok(())
    }

    /// `disconnect` (index.ts:653-659).
    pub async fn disconnect(&self, name: &str) -> Result<(), NotFoundError> {
        let state = self.state().await;
        let removed = {
            let mut state = state.lock().await;
            get_mcp_config(self, &state, name)?;
            let removed = state.clients.remove(name);
            state.defs.remove(name);
            state.instructions.remove(name);
            state.status.insert(name.to_string(), McpStatus::Disabled);
            removed
        };
        if let Some(removed) = removed {
            removed.close().await;
        }
        Ok(())
    }

    /// `removeAuth` (index.ts:944-948).
    pub async fn remove_auth(&self, name: &str) {
        self.auth.remove(name);
        self.callback.cancel_pending(name);
        self.pending_oauth.lock().await.retain(|item| item != name);
    }

    /// `supportsOAuth` (index.ts:950-953).
    pub async fn supports_oauth(&self, name: &str) -> Result<bool, NotFoundError> {
        let state = self.state().await;
        let mcp = {
            let state = state.lock().await;
            get_mcp_config(self, &state, name)?
        };
        Ok(matches!(
            mcp,
            McpInfo::Remote(remote)
                if remote.oauth != Some(McpOAuthSetting::Disabled(false))
        ))
    }

    /// `hasStoredTokens` (index.ts:955-958).
    pub fn has_stored_tokens(&self, name: &str) -> bool {
        self.auth.get(name).and_then(|entry| entry.tokens).is_some()
    }

    /// `getAuthStatus` (index.ts:960-970).
    pub fn get_auth_status(&self, name: &str) -> AuthStatus {
        let remote = match self.input.mcp.get(name) {
            Some(entry) if is_mcp_configured(entry) => match entry {
                McpEntry::Server(McpInfo::Remote(remote)) => remote.clone(),
                _ => return AuthStatus::NotAuthenticated,
            },
            _ => return AuthStatus::NotAuthenticated,
        };
        let Some(entry) = self.auth.get_for_url(name, &remote.url) else {
            return AuthStatus::NotAuthenticated;
        };
        let Some(tokens) = entry.tokens else {
            return AuthStatus::NotAuthenticated;
        };
        match tokens.expires_at {
            Some(expires_at) if expires_at < now_seconds() => AuthStatus::Expired,
            _ => AuthStatus::Authenticated,
        }
    }

    /// `startAuth` (index.ts:806-870): discovery + registration + the
    /// authorization URL, with the callback server bound to the
    /// configured redirect URI.
    pub async fn start_auth(&self, name: &str) -> Result<(String, String), StartAuthError> {
        let state = self.state().await;
        let mcp = {
            let state = state.lock().await;
            get_mcp_config(self, &state, name)
                .map_err(|_| StartAuthError::NotFound(name.to_string()))?
        };
        let remote = match &mcp {
            McpInfo::Remote(remote) => remote,
            _ => {
                return Err(StartAuthError::Failed(format!(
                    "MCP server {name} is not a remote server"
                )))
            }
        };
        if remote.oauth == Some(McpOAuthSetting::Disabled(false)) {
            return Err(StartAuthError::Failed(format!(
                "MCP server {name} has OAuth explicitly disabled"
            )));
        }
        if remote_url(&remote.url).is_none() {
            return Err(StartAuthError::Failed(format!(
                "Invalid MCP URL for \"{name}\""
            )));
        }
        let config = OAuthConfig::from_setting(remote.oauth.as_ref()).unwrap_or_default();
        self.callback
            .ensure_running(config.redirect_uri.as_deref())
            .await
            .map_err(|err| StartAuthError::Failed(err.message))?;
        let (url, state_token) = authorization_url(name, &remote.url, &config, &self.auth)
            .await
            .map_err(|err| StartAuthError::Failed(err.message))?;
        self.pending_oauth.lock().await.push(name.to_string());
        Ok((url, state_token))
    }

    /// `finishAuth` (index.ts:918-942).
    pub async fn finish_auth(
        &self,
        name: &str,
        authorization_code: &str,
    ) -> Result<McpStatus, FinishAuthError> {
        let state = self.state().await;
        let mcp = {
            let state = state.lock().await;
            get_mcp_config(self, &state, name)
                .map_err(|_| FinishAuthError::NotFound(name.to_string()))?
        };
        {
            let mut pending = self.pending_oauth.lock().await;
            if !pending.iter().any(|item| item == name) {
                return Err(FinishAuthError::Failed(format!(
                    "No pending OAuth flow for MCP server: {name}"
                )));
            }
            pending.retain(|item| item != name);
        }
        let remote = match &mcp {
            McpInfo::Remote(remote) => remote,
            _ => {
                return Err(FinishAuthError::Failed(
                    "MCP server {name} is not a remote server".to_string(),
                ))
            }
        };
        let config = OAuthConfig::from_setting(remote.oauth.as_ref()).unwrap_or_default();
        if let Err(err) =
            exchange_code(name, &remote.url, &config, &self.auth, authorization_code).await
        {
            return Ok(McpStatus::Failed {
                error: format!("OAuth completion failed: {}", err.message),
            });
        }
        self.auth.clear_code_verifier(name);
        Ok(self.create_and_store(name, enabled(mcp)).await)
    }

    /// `authenticate` (index.ts:872-916): start the flow, open the
    /// browser, wait for the callback code and finish.
    pub async fn authenticate(&self, name: &str) -> Result<McpStatus, StartAuthError> {
        let (authorization_url, oauth_state) = self.start_auth(name).await?;
        let open_failed = || {
            self.input.events.clone().map(|events| {
                let _ = events.publish(
                    &BROWSER_OPEN_FAILED,
                    json!({"mcpName": name, "url": authorization_url}),
                    crate::event::bus::PublishOptions::default(),
                );
            })
        };
        if let Err(_message) = self.browser.open(&authorization_url).await {
            open_failed();
        }
        let code = self
            .callback
            .wait_for_callback(&oauth_state, Some(name))
            .await
            .map_err(StartAuthError::Failed)?;
        let stored = self.auth.get_oauth_state(name);
        self.auth.clear_oauth_state(name);
        if stored.as_deref() != Some(oauth_state.as_str()) {
            return Err(StartAuthError::Failed(
                "OAuth state mismatch - potential CSRF attack".to_string(),
            ));
        }
        self.finish_auth(name, &code)
            .await
            .map_err(|err| match err {
                FinishAuthError::Failed(message) => StartAuthError::Failed(message),
                FinishAuthError::NotFound(name) => StartAuthError::NotFound(name),
            })
    }

    /// `createAndStore` (index.ts:627-639).
    async fn create_and_store(&self, name: &str, mcp: McpInfo) -> McpStatus {
        let state = self.state().await;
        // `Effect.catchCause` (index.ts:408-414): create failures land as
        // the failed status, not an error channel.
        let result = match create(self, name, mcp).await {
            Ok(result) => result,
            Err(err) => CreateResult {
                status: McpStatus::Failed { error: err.message },
                client: None,
                defs: Vec::new(),
                instructions: None,
            },
        };
        let mut state = state.lock().await;
        state.status.insert(name.to_string(), result.status.clone());
        let Some(client) = result.client else {
            state.clients.remove(name);
            state.defs.remove(name);
            state.instructions.remove(name);
            return result.status;
        };
        let previous = state.clients.insert(name.to_string(), client);
        state.defs.insert(name.to_string(), result.defs);
        if let Some(instructions) = result.instructions {
            state.instructions.insert(name.to_string(), instructions);
        } else {
            state.instructions.remove(name);
        }
        state.status.insert(name.to_string(), McpStatus::Connected);
        if let Some(previous) = previous {
            previous.close().await;
        }
        McpStatus::Connected
    }
}

enum ItemKind {
    Prompts,
    Resources,
    ResourceTemplates,
}

/// The `startAuth` error surface: 404 (`NotFoundError`) or a plain
/// failure.
#[derive(Debug, Clone)]
pub enum StartAuthError {
    NotFound(String),
    Failed(String),
}

/// The `finishAuth` error surface.
#[derive(Debug, Clone)]
pub enum FinishAuthError {
    NotFound(String),
    Failed(String),
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// `getMcpConfig` + `requireMcpConfig` (index.ts:790-804).
fn get_mcp_config(
    service: &McpService,
    state: &State,
    name: &str,
) -> Result<McpInfo, NotFoundError> {
    if let Some(info) = state.config.get(name) {
        return Ok(info.clone());
    }
    match service.input.mcp.get(name) {
        Some(McpEntry::Server(info)) => Ok(info.clone()),
        _ => Err(NotFoundError {
            name: name.to_string(),
        }),
    }
}

fn enabled(info: McpInfo) -> McpInfo {
    match info {
        McpInfo::Local(mut local) => {
            local.enabled = Some(true);
            McpInfo::Local(local)
        }
        McpInfo::Remote(mut remote) => {
            remote.enabled = Some(true);
            McpInfo::Remote(remote)
        }
    }
}

/// `CreateResult` (index.ts:127-132).
struct CreateResult {
    status: McpStatus,
    client: Option<Arc<McpClient>>,
    defs: Vec<McpToolDef>,
    instructions: Option<String>,
}

/// `create` (index.ts:372-415): the disabled short-circuit, then the
/// transport-specific connect.
async fn create(service: &McpService, key: &str, mcp: McpInfo) -> Result<CreateResult, McpError> {
    if info_enabled(&mcp) == Some(false) {
        return Ok(CreateResult {
            status: McpStatus::Disabled,
            client: None,
            defs: Vec::new(),
            instructions: None,
        });
    }
    match mcp {
        McpInfo::Local(local) => create_local(service, local).await,
        McpInfo::Remote(remote) => create_remote(service, key, remote).await,
    }
}

/// `connectLocal` (index.ts:340-370).
async fn create_local(service: &McpService, local: McpLocal) -> Result<CreateResult, McpError> {
    let timeout = local
        .timeout
        .map(|t| t.get())
        .unwrap_or(crate::mcp::catalog::DEFAULT_TIMEOUT_MS);
    let mut parts = local.command.iter();
    let Some(command) = parts.next() else {
        return Err(McpError::failed("Command cannot be empty"));
    };
    let args: Vec<String> = parts.cloned().collect();
    let cwd = match local.cwd.as_deref() {
        Some(cwd) => absolute(&service.input.directory, cwd),
        None => service.input.directory.clone(),
    };
    let transport = StdioTransport::spawn(
        command,
        &args,
        Some(&cwd),
        local.environment.as_ref(),
        &service.input.directory,
    )
    .await?;
    let client = McpClient::connect(Transport::Stdio(transport), timeout).await;
    finish_create(client).await
}

/// `connectRemote` (index.ts:236-338) — streamable HTTP only (the legacy
/// SSE fallback is not ported).
async fn create_remote(
    service: &McpService,
    key: &str,
    remote: McpRemote,
) -> Result<CreateResult, McpError> {
    let Some(url) = remote_url(&remote.url) else {
        return Ok(CreateResult {
            status: McpStatus::Failed {
                error: format!("Invalid MCP URL for \"{key}\""),
            },
            client: None,
            defs: Vec::new(),
            instructions: None,
        });
    };
    let timeout = remote
        .timeout
        .map(|t| t.get())
        .unwrap_or(crate::mcp::catalog::DEFAULT_TIMEOUT_MS);
    let headers: Vec<(String, String)> = remote
        .headers
        .clone()
        .map(|headers| {
            headers
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut transport = HttpTransport::new(url.as_str(), headers);
    if remote.oauth != Some(McpOAuthSetting::Disabled(false)) {
        let bearer = service
            .auth
            .get_for_url(key, &remote.url)
            .and_then(|entry| entry.tokens)
            .map(|tokens: McpTokens| tokens.access_token);
        transport.set_bearer(bearer);
    }
    let handle = HttpHandle::new(url.as_str(), transport);
    let client = McpClient::connect(Transport::Http(handle), timeout).await;
    finish_create(client).await
}

/// The connected tail of both paths: list tools (`McpCatalog.defs`,
/// index.ts:391-400) and capture the instructions.
async fn finish_create(client: Result<McpClient, McpError>) -> Result<CreateResult, McpError> {
    let client = client?;
    let listed = if client
        .server_capabilities()
        .and_then(|caps| caps.get("tools"))
        .is_some()
    {
        client.list_tools().await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let instructions = client.instructions().map(String::from);
    Ok(CreateResult {
        status: McpStatus::Connected,
        client: Some(Arc::new(client)),
        defs: listed,
        instructions,
    })
}

/// `path.resolve(base, value)` (index.ts:346).
fn absolute(base: &std::path::Path, value: &str) -> PathBuf {
    if std::path::Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        base.join(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::service::McpServiceInput;
    use std::collections::BTreeMap;

    /// A minimal MCP stdio server: newline-delimited JSON-RPC answering
    /// `initialize`, `tools/list` and `prompts/list` (M7.6 acceptance
    /// fixture — a spawned subprocess, never a real endpoint).
    const FIXTURE: &str = r#"
import json, sys
def out(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method", "")
    rid = msg.get("id")
    if method == "initialize":
        out({"jsonrpc": "2.0", "id": rid, "result": {
            "protocolVersion": msg["params"]["protocolVersion"],
            "capabilities": {"tools": {}, "prompts": {}},
            "serverInfo": {"name": "fixture", "version": "1.0"},
            "instructions": "fixture instructions",
        }})
    elif method == "tools/list":
        out({"jsonrpc": "2.0", "id": rid, "result": {"tools": [
            {"name": "echo", "description": "Echo a value",
             "inputSchema": {"type": "object"}},
        ]}})
    elif method == "prompts/list":
        out({"jsonrpc": "2.0", "id": rid, "result": {"prompts": [
            {"name": "greet", "description": "Greet"},
        ]}})
    elif method == "roots/list":
        out({"jsonrpc": "2.0", "id": rid, "result": {"roots": []}})
    elif method == "tools/call":
        out({"jsonrpc": "2.0", "id": rid, "result": {"content": [
            {"type": "text", "text": "fixture output"},
        ]}})
"#;

    fn local_fixture_command() -> Vec<String> {
        vec!["python3".to_string(), "-c".to_string(), FIXTURE.to_string()]
    }

    fn local_entry(command: Vec<String>, enabled: Option<bool>) -> McpEntry {
        McpEntry::Server(McpInfo::Local(McpLocal {
            command,
            cwd: None,
            environment: None,
            enabled,
            timeout: None,
        }))
    }

    fn new_service(mcp: BTreeMap<String, McpEntry>) -> McpService {
        let dir = tempfile::tempdir().unwrap().keep();
        McpService::new(McpServiceInput {
            directory: dir.clone(),
            data_dir: dir,
            mcp,
            mcp_timeout: None,
            events: None,
        })
    }

    #[tokio::test]
    async fn status_map_reports_connected_and_disabled() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "srv".to_string(),
            local_entry(local_fixture_command(), None),
        );
        entries.insert("off".to_string(), McpEntry::EnabledOnly { enabled: false });
        let service = new_service(entries);
        let status = service.status().await;
        assert_eq!(
            status.get("srv"),
            Some(&McpStatus::Connected),
            "boot connects the configured server, got {status:?}"
        );
        assert_eq!(
            status.get("off"),
            None,
            "{{enabled: false}} markers stay outside the map (isMcpConfigured)"
        );
        assert_eq!(status.len(), 1);
    }

    #[tokio::test]
    async fn tools_feed_the_registry() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "srv".to_string(),
            local_entry(local_fixture_command(), None),
        );
        let service = new_service(entries);
        let tools = service.tools().await;
        let tool = tools.get("srv_echo").expect("tool name is {client}_{tool}");
        assert_eq!(tool.def.description, Some("Echo a value".to_string()));
        let instructions = service.instructions().await;
        assert_eq!(instructions.len(), 1);
        assert_eq!(instructions[0].name, "srv");
        assert_eq!(instructions[0].instructions, "fixture instructions");
        assert_eq!(instructions[0].tools, vec!["srv_echo".to_string()]);
    }

    #[tokio::test]
    async fn add_connect_disconnect_lifecycle() {
        let service = new_service(BTreeMap::new());
        let added = service
            .add(
                "srv",
                McpInfo::Local(McpLocal {
                    command: local_fixture_command(),
                    cwd: None,
                    environment: None,
                    enabled: None,
                    timeout: None,
                }),
            )
            .await;
        assert_eq!(added.get("srv"), Some(&McpStatus::Connected));
        service.disconnect("srv").await.unwrap();
        assert_eq!(
            service.status().await.get("srv"),
            Some(&McpStatus::Disabled)
        );
        service.connect("srv").await.unwrap();
        assert_eq!(
            service.status().await.get("srv"),
            Some(&McpStatus::Connected)
        );
    }

    #[tokio::test]
    async fn invalid_remote_url_fails() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "bad".to_string(),
            McpEntry::Server(McpInfo::Remote(McpRemote {
                url: "ftp://example.invalid".to_string(),
                enabled: None,
                headers: None,
                oauth: None,
                timeout: None,
            })),
        );
        let service = new_service(entries);
        let status = service.status().await;
        assert_eq!(
            status.get("bad"),
            Some(&McpStatus::Failed {
                error: "Invalid MCP URL for \"bad\"".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn start_auth_rejects_local_and_disabled_servers() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "local".to_string(),
            local_entry(local_fixture_command(), None),
        );
        let service = new_service(entries);
        assert!(matches!(
            service.start_auth("local").await,
            Err(StartAuthError::Failed(_))
        ));

        let mut entries = BTreeMap::new();
        entries.insert(
            "off".to_string(),
            McpEntry::Server(McpInfo::Remote(McpRemote {
                url: "https://mcp.example.invalid".to_string(),
                enabled: None,
                headers: None,
                oauth: Some(McpOAuthSetting::Disabled(false)),
                timeout: None,
            })),
        );
        let service = new_service(entries);
        assert!(matches!(
            service.start_auth("off").await,
            Err(StartAuthError::Failed(_))
        ));
    }

    #[tokio::test]
    async fn not_found_matrix() {
        let service = new_service(BTreeMap::new());
        assert!(service.connect("missing").await.is_err());
        assert!(service.disconnect("missing").await.is_err());
        assert!(service.supports_oauth("missing").await.is_err());
        assert!(matches!(
            service.start_auth("missing").await,
            Err(StartAuthError::NotFound(_))
        ));
        assert!(matches!(
            service.finish_auth("missing", "code").await,
            Err(FinishAuthError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn finish_auth_requires_pending_flow() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "remote".to_string(),
            McpEntry::Server(McpInfo::Remote(McpRemote {
                url: "https://mcp.example.invalid".to_string(),
                enabled: None,
                headers: None,
                oauth: None,
                timeout: None,
            })),
        );
        let service = new_service(entries);
        assert!(matches!(
            service.finish_auth("remote", "code").await,
            Err(FinishAuthError::Failed(_))
        ));
    }
}
