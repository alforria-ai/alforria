//! cli/cmd/mcp.ts port — `mcp list`, `auth` (+ `auth list`), `logout`,
//! `add` and `debug` against the core MCP service.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use alforria_schema::mcp::McpStatus;
use clap::ArgMatches;
use serde_json::{json, Map, Value};

use alforria_core::config::schema::{
    Config, McpEntry, McpInfo, McpLocal, McpOAuth, McpOAuthSetting, McpRemote,
};
use alforria_core::mcp::oauth::{authorization_url, OAuthConfig};
use alforria_core::mcp::transport::LATEST_PROTOCOL_VERSION;
use alforria_core::mcp::{AuthStatus, McpBrowser, McpService, McpServiceInput};
use alforria_core::parse_jsonc;

use crate::error::{core_error, CliError, TypedError};
use crate::ui::{style, Ui};

use super::providers::{Prompter, StdinPrompter};

// ---------------------------------------------------------------------------
// Prompt frame — mirrors providers.rs (the @clack layout glyphs).
// ---------------------------------------------------------------------------

fn intro(ui: &mut Ui, message: &str) {
    ui.println(&format!("┌ {message}"));
}

fn outro(ui: &mut Ui, message: &str) {
    ui.println(&format!("└ {message}"));
}

fn log_info(ui: &mut Ui, message: &str) {
    ui.println(&format!("│ {message}"));
}

fn log_success(ui: &mut Ui, message: &str) {
    ui.println(&format!("│ {message}"));
}

fn log_warn(ui: &mut Ui, message: &str) {
    ui.println(&format!("│ {message}"));
}

fn log_error(ui: &mut Ui, message: &str) {
    ui.println(&format!("│ {message}"));
}

// ---------------------------------------------------------------------------
// Config helpers
// ---------------------------------------------------------------------------

/// `getAuthStatusIcon` / `getAuthStatusText` (mcp.ts:24-43).
fn auth_icon(status: AuthStatus) -> &'static str {
    match status {
        AuthStatus::Authenticated => "✓",
        AuthStatus::Expired => "⚠",
        AuthStatus::NotAuthenticated => "✗",
    }
}

fn auth_text(status: AuthStatus) -> &'static str {
    match status {
        AuthStatus::Authenticated => "authenticated",
        AuthStatus::Expired => "expired",
        AuthStatus::NotAuthenticated => "not authenticated",
    }
}

/// `oauth !== false` — a `{enabled: false}` marker or a local server is not
/// OAuth-capable (mcp.ts:49-66).
fn is_oauth_remote(info: &McpInfo) -> Option<&McpRemote> {
    match info {
        McpInfo::Remote(remote)
            if !matches!(remote.oauth, Some(McpOAuthSetting::Disabled(false))) =>
        {
            Some(remote)
        }
        _ => None,
    }
}

/// `configuredServers` (mcp.ts:58-59).
fn configured_servers(config: &Config) -> Vec<(String, McpInfo)> {
    config
        .mcp
        .as_ref()
        .map(|mcp| {
            mcp.iter()
                .filter_map(|(name, entry)| match entry {
                    McpEntry::Server(info) => Some((name.clone(), info.clone())),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `oauthServers` (mcp.ts:62-66).
fn oauth_servers(config: &Config) -> Vec<(String, McpRemote)> {
    configured_servers(config)
        .into_iter()
        .filter_map(|(name, info)| is_oauth_remote(&info).map(|remote| (name, remote.clone())))
        .collect()
}

// ---------------------------------------------------------------------------
// list (mcp.ts:109-170)
// ---------------------------------------------------------------------------

/// The per-server status line (mcp.ts:126-163).
pub fn status_line(name: &str, info: &McpInfo, status: Option<&McpStatus>, stored: bool) -> String {
    let (icon, text, hint) = match status {
        None => ("○", "not initialized", String::new()),
        Some(McpStatus::Connected) => {
            let oauth = matches!(info, McpInfo::Remote(remote)
                if matches!(remote.oauth, Some(McpOAuthSetting::OAuth(_))));
            (
                "✓",
                "connected",
                if oauth && stored { " (OAuth)" } else { "" }.to_string(),
            )
        }
        Some(McpStatus::Disabled) => ("○", "disabled", String::new()),
        Some(McpStatus::NeedsAuth) => ("⚠", "needs authentication", String::new()),
        Some(McpStatus::NeedsClientRegistration { error }) => {
            ("✗", "needs client registration", format!("\n    {error}"))
        }
        Some(McpStatus::Failed { error }) => ("✗", "failed", format!("\n    {error}")),
    };
    let type_hint = match info {
        McpInfo::Remote(remote) => remote.url.clone(),
        McpInfo::Local(local) => local.command.join(" "),
    };
    format!(
        "{icon} {name} {}{text}{hint}\n    {}{type_hint}",
        style::TEXT_DIM,
        style::TEXT_DIM
    )
}

pub async fn list(ui: &mut Ui, config: &Config, mcp: &McpService) -> Result<(), TypedError> {
    ui.empty();
    intro(ui, "MCP Servers");
    let servers = configured_servers(config);
    if servers.is_empty() {
        log_warn(ui, "No MCP servers configured");
        outro(ui, "Add servers with: alforria mcp add");
        return Ok(());
    }
    let statuses = mcp.status().await;
    for (name, info) in &servers {
        let Some(status) = statuses.get(name) else {
            continue;
        };
        let stored = mcp.has_stored_tokens(name);
        log_info(ui, &status_line(name, info, Some(status), stored));
    }
    outro(ui, &format!("{} server(s)", servers.len()));
    Ok(())
}

// ---------------------------------------------------------------------------
// auth (mcp.ts:171-305)
// ---------------------------------------------------------------------------

/// mcp.ts:259-263 — `spinner.stop("Authorize in your browser:")` +
/// `prompts.log.info(url)` rendered from the browser seam.
struct AuthBrowser {
    ui: Mutex<Ui>,
    inner: Arc<dyn McpBrowser>,
}

impl McpBrowser for AuthBrowser {
    fn open<'a>(
        &'a self,
        url: &'a str,
    ) -> alforria_core::tool::def::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            {
                let mut ui = self.ui.lock().expect("ui lock poisoned");
                log_info(&mut ui, "Authorize in your browser:");
                log_info(&mut ui, url);
            }
            self.inner.open(url).await
        })
    }
}

fn auth_failed(ui: &mut Ui, message: &str) {
    log_error(ui, "Authentication failed");
    log_error(ui, message);
}

pub async fn auth(
    ui: &mut Ui,
    config: &Config,
    mcp: &mut McpService,
    prompter: &mut dyn Prompter,
    arg: Option<&str>,
) -> Result<(), TypedError> {
    ui.empty();
    intro(ui, "MCP OAuth Authentication");
    let servers = oauth_servers(config);
    if servers.is_empty() {
        log_warn(ui, "No OAuth-capable MCP servers configured");
        log_info(
            ui,
            "Remote MCP servers support OAuth by default. Add a remote server in opencode.json:",
        );
        log_info(
            ui,
            r#"
  "mcp": {
    "my-server": {
      "type": "remote",
      "url": "https://example.com/mcp"
    }
  }"#,
        );
        outro(ui, "Done");
        return Ok(());
    }
    let server_name = match arg {
        Some(name) => name.to_string(),
        None => {
            let options: Vec<(String, String)> = servers
                .iter()
                .map(|(name, _)| {
                    let status = mcp.get_auth_status(name);
                    (
                        format!("{} {} ({})", auth_icon(status), name, auth_text(status)),
                        name.clone(),
                    )
                })
                .collect();
            prompter.select("Select MCP server to authenticate", &options)?
        }
    };
    let server = config
        .mcp
        .as_ref()
        .and_then(|mcp| mcp.get(&server_name))
        .cloned();
    let Some(McpEntry::Server(info)) = server else {
        log_error(ui, &format!("MCP server not found: {server_name}"));
        outro(ui, "Done");
        return Ok(());
    };
    if is_oauth_remote(&info).is_none() {
        log_error(
            ui,
            &format!("MCP server {server_name} is not an OAuth-capable remote server"),
        );
        outro(ui, "Done");
        return Ok(());
    }
    match mcp.get_auth_status(&server_name) {
        AuthStatus::Authenticated => {
            if !prompter.confirm(&format!(
                "{server_name} already has valid credentials. Re-authenticate?"
            ))? {
                outro(ui, "Cancelled");
                return Ok(());
            }
        }
        AuthStatus::Expired => log_warn(
            ui,
            &format!("{server_name} has expired credentials. Re-authenticating..."),
        ),
        AuthStatus::NotAuthenticated => {}
    }
    mcp.browser = Arc::new(AuthBrowser {
        ui: Mutex::new(ui.share()),
        inner: mcp.browser.clone(),
    });
    let result = mcp.authenticate(&server_name).await;
    match result {
        Ok(McpStatus::Connected) => log_success(ui, "Authentication successful!"),
        Ok(McpStatus::NeedsClientRegistration { error }) => {
            auth_failed(ui, &error);
            log_info(ui, "Add clientId to your MCP server config:");
            let url = match &info {
                McpInfo::Remote(remote) => remote.url.clone(),
                _ => String::new(),
            };
            log_info(
                ui,
                &format!(
                    r#"
  "mcp": {{
    "{server_name}": {{
      "type": "remote",
      "url": "{url}",
      "oauth": {{
        "clientId": "your-client-id",
        "clientSecret": "your-client-secret"
      }}
    }}
  }}"#
                ),
            );
        }
        Ok(McpStatus::Failed { error }) => auth_failed(ui, &error),
        Ok(other) => log_error(ui, &format!("Unexpected status: {}", status_slug(&other))),
        Err(err) => auth_failed(
            ui,
            &match err {
                alforria_core::mcp::StartAuthError::NotFound(_) => {
                    format!("MCP server not found: {server_name}")
                }
                alforria_core::mcp::StartAuthError::Failed(message) => message,
            },
        ),
    }
    outro(ui, "Done");
    Ok(())
}

fn status_slug(status: &McpStatus) -> &'static str {
    match status {
        McpStatus::Connected => "connected",
        McpStatus::Disabled => "disabled",
        McpStatus::Failed { .. } => "failed",
        McpStatus::NeedsAuth => "needs_auth",
        McpStatus::NeedsClientRegistration { .. } => "needs_client_registration",
    }
}

// ---------------------------------------------------------------------------
// auth list (mcp.ts:307-335)
// ---------------------------------------------------------------------------

pub async fn auth_list(ui: &mut Ui, config: &Config, mcp: &McpService) -> Result<(), TypedError> {
    ui.empty();
    intro(ui, "MCP OAuth Status");
    let servers = oauth_servers(config);
    if servers.is_empty() {
        log_warn(ui, "No OAuth-capable MCP servers configured");
        outro(ui, "Done");
        return Ok(());
    }
    for (name, remote) in &servers {
        let status = mcp.get_auth_status(name);
        log_info(
            ui,
            &format!(
                "{} {} {}{}\n    {}{}",
                auth_icon(status),
                name,
                style::TEXT_DIM,
                auth_text(status),
                style::TEXT_DIM,
                remote.url
            ),
        );
    }
    outro(ui, &format!("{} OAuth-capable server(s)", servers.len()));
    Ok(())
}

// ---------------------------------------------------------------------------
// logout (mcp.ts:337-428)
// ---------------------------------------------------------------------------

pub async fn logout(
    ui: &mut Ui,
    mcp: &McpService,
    prompter: &mut dyn Prompter,
    arg: Option<&str>,
) -> Result<(), TypedError> {
    ui.empty();
    intro(ui, "MCP OAuth Logout");
    let credentials = mcp.auth.all();
    if credentials.is_empty() {
        log_warn(ui, "No MCP OAuth credentials stored");
        outro(ui, "Done");
        return Ok(());
    }
    let server_name = match arg {
        Some(name) => name.to_string(),
        None => {
            let options: Vec<(String, String)> = credentials
                .iter()
                .map(|(name, entry)| {
                    let hint = match (&entry.tokens, &entry.client_info) {
                        (Some(_), Some(_)) => " (tokens + client)",
                        (Some(_), None) => " (tokens)",
                        (None, Some(_)) => " (client registration)",
                        _ => "",
                    };
                    (format!("{name}{hint}"), name.clone())
                })
                .collect();
            prompter.select("Select MCP server to logout", &options)?
        }
    };
    if !credentials.contains_key(&server_name) {
        log_error(ui, &format!("No credentials found for: {server_name}"));
        outro(ui, "Done");
        return Ok(());
    }
    mcp.remove_auth(&server_name).await;
    log_success(ui, &format!("Removed OAuth credentials for {server_name}"));
    outro(ui, "Done");
    Ok(())
}

// ---------------------------------------------------------------------------
// add (mcp.ts:394-657)
// ---------------------------------------------------------------------------

/// The `mcp add` flag surface (mcp.ts:432-451).
#[derive(Debug, Clone, Default)]
pub struct AddArgs {
    pub name: Option<String>,
    pub url: Option<String>,
    pub env: Vec<String>,
    pub header: Vec<String>,
    pub command: Vec<String>,
}

impl AddArgs {
    pub fn from_matches(matches: &ArgMatches) -> Self {
        AddArgs {
            name: matches.get_one::<String>("name").cloned(),
            url: matches.get_one::<String>("url").cloned(),
            env: matches
                .get_many("env")
                .map(|values| values.cloned().collect())
                .unwrap_or_default(),
            header: matches
                .get_many("header")
                .map(|values| values.cloned().collect())
                .unwrap_or_default(),
            command: matches
                .get_many("command")
                .map(|values| values.cloned().collect())
                .unwrap_or_default(),
        }
    }
}

/// `entries()` (mcp.ts:475-482): `KEY=VALUE` with a non-empty key.
fn key_value_entries(
    values: &[String],
    kind: &str,
) -> Result<BTreeMap<String, String>, TypedError> {
    let mut out = BTreeMap::new();
    for entry in values {
        let invalid = || {
            TypedError::Cli(CliError::new(format!(
                "Invalid {kind}: {entry}. Expected KEY=VALUE"
            )))
        };
        let Some((key, value)) = entry.split_once('=') else {
            return Err(invalid());
        };
        if key.is_empty() {
            return Err(invalid());
        }
        out.insert(key.to_string(), value.to_string());
    }
    Ok(out)
}

/// `resolveConfigPath` (mcp.ts:394-410): the first existing candidate, or
/// `opencode.json`.
fn resolve_config_path(base_dir: &Path, global: bool) -> PathBuf {
    let mut candidates = vec![
        base_dir.join("opencode.json"),
        base_dir.join("opencode.jsonc"),
    ];
    if !global {
        candidates.push(base_dir.join(".opencode").join("opencode.json"));
        candidates.push(base_dir.join(".opencode").join("opencode.jsonc"));
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.exists())
        .unwrap_or_else(|| base_dir.join("opencode.json"))
}

/// `addMcpToConfig` (mcp.ts:412-427). TS edits through jsonc-parser to
/// preserve comments; the Rust port re-serializes the whole document.
fn write_mcp_config(config_path: &Path, name: &str, info: &McpInfo) -> Result<(), TypedError> {
    let text = std::fs::read_to_string(config_path).unwrap_or_else(|_| "{}".to_string());
    let mut root = parse_jsonc(&text, config_path).map_err(core_error)?;
    let Some(object) = root.as_object_mut() else {
        return Err(TypedError::Cli(CliError::new(format!(
            "Config file at {} is not a JSON object",
            config_path.display()
        ))));
    };
    if !object.get("mcp").is_some_and(Value::is_object) {
        object.insert("mcp".to_string(), Value::Object(Map::new()));
    }
    let mcp = object.get_mut("mcp").expect("mcp object just inserted");
    if let Some(map) = mcp.as_object_mut() {
        map.insert(
            name.to_string(),
            serde_json::to_value(info).unwrap_or(Value::Null),
        );
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| TypedError::Unknown {
            raw: err.to_string(),
        })?;
    }
    let json = serde_json::to_string_pretty(&root).map_err(|err| TypedError::Unknown {
        raw: err.to_string(),
    })?;
    std::fs::write(config_path, format!("{json}\n")).map_err(|err| TypedError::Unknown {
        raw: err.to_string(),
    })?;
    Ok(())
}

fn add_interactive(
    ui: &mut Ui,
    prompter: &mut dyn Prompter,
    global_dir: &Path,
    project: Option<&Path>,
) -> Result<(), TypedError> {
    ui.empty();
    intro(ui, "Add MCP server");
    let config_path = match project {
        Some(project_dir) => {
            let options = vec![
                (
                    "Current project".to_string(),
                    resolve_config_path(project_dir, false)
                        .to_string_lossy()
                        .into_owned(),
                ),
                (
                    "Global".to_string(),
                    resolve_config_path(global_dir, true)
                        .to_string_lossy()
                        .into_owned(),
                ),
            ];
            PathBuf::from(prompter.select("Location", &options)?)
        }
        None => resolve_config_path(global_dir, true),
    };
    let name = loop {
        let value = prompter.text("Enter MCP server name")?;
        if !value.is_empty() {
            break value;
        }
    };
    let kind = prompter.select(
        "Select MCP server type",
        &[
            ("Local".to_string(), "local".to_string()),
            ("Remote".to_string(), "remote".to_string()),
        ],
    )?;
    let info = if kind == "local" {
        let command = loop {
            let value = prompter.text("Enter command to run")?;
            if !value.is_empty() {
                break value;
            }
        };
        McpInfo::Local(McpLocal {
            command: command.split(' ').map(String::from).collect(),
            cwd: None,
            environment: None,
            enabled: None,
            timeout: None,
        })
    } else {
        let url = loop {
            let value = prompter.text("Enter MCP server URL")?;
            if !value.is_empty() && reqwest::Url::parse(&value).is_ok() {
                break value;
            }
        };
        let mut oauth = None;
        if prompter.confirm("Does this server require OAuth authentication?")? {
            if prompter.confirm("Do you have a pre-registered client ID?")? {
                let client_id = loop {
                    let value = prompter.text("Enter client ID")?;
                    if !value.is_empty() {
                        break value;
                    }
                };
                let client_secret = if prompter.confirm("Do you have a client secret?")? {
                    Some(prompter.password("Enter client secret")?)
                } else {
                    None
                };
                oauth = Some(McpOAuthSetting::OAuth(McpOAuth {
                    client_id: Some(client_id),
                    client_secret,
                    scope: None,
                    callback_port: None,
                    redirect_uri: None,
                }));
            } else {
                oauth = Some(McpOAuthSetting::OAuth(McpOAuth {
                    client_id: None,
                    client_secret: None,
                    scope: None,
                    callback_port: None,
                    redirect_uri: None,
                }));
            }
        }
        McpInfo::Remote(McpRemote {
            url,
            enabled: None,
            headers: None,
            oauth,
            timeout: None,
        })
    };
    write_mcp_config(&config_path, &name, &info)?;
    log_success(
        ui,
        &format!("MCP server \"{name}\" added to {}", config_path.display()),
    );
    outro(ui, "MCP server added successfully");
    Ok(())
}

pub fn add(
    ui: &mut Ui,
    args: &AddArgs,
    prompter: &mut dyn Prompter,
    global_dir: &Path,
    project: Option<&Path>,
) -> Result<(), TypedError> {
    if args.name.is_none()
        && (args.url.is_some()
            || !args.env.is_empty()
            || !args.header.is_empty()
            || !args.command.is_empty())
    {
        return Err(TypedError::Cli(CliError::new(
            "A server name is required for non-interactive MCP configuration",
        )));
    }
    let Some(name) = args.name.clone() else {
        return add_interactive(ui, prompter, global_dir, project);
    };
    if args.url.is_some() != args.command.is_empty() {
        return Err(TypedError::Cli(CliError::new(
            "Provide either --url <url> or a command after --",
        )));
    }
    if let Some(url) = &args.url {
        if reqwest::Url::parse(url).is_err() {
            return Err(TypedError::Cli(CliError::new(format!(
                "Invalid URL: {url}"
            ))));
        }
        if !args.env.is_empty() {
            return Err(TypedError::Cli(CliError::new(
                "--env is only valid for local MCP servers",
            )));
        }
    }
    if !args.command.is_empty() && !args.header.is_empty() {
        return Err(TypedError::Cli(CliError::new(
            "--header is only valid for remote MCP servers",
        )));
    }
    let info = if let Some(url) = &args.url {
        let headers = key_value_entries(&args.header, "HTTP header")?;
        McpInfo::Remote(McpRemote {
            url: url.clone(),
            enabled: None,
            headers: (!headers.is_empty()).then_some(headers),
            oauth: None,
            timeout: None,
        })
    } else {
        let environment = key_value_entries(&args.env, "environment variable")?;
        McpInfo::Local(McpLocal {
            command: args.command.clone(),
            cwd: None,
            environment: (!environment.is_empty()).then_some(environment),
            enabled: None,
            timeout: None,
        })
    };
    let config_path = resolve_config_path(global_dir, true);
    write_mcp_config(&config_path, &name, &info)?;
    log_success(
        ui,
        &format!("MCP server \"{name}\" added to {}", config_path.display()),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// debug (mcp.ts:659-840)
// ---------------------------------------------------------------------------

fn iso_date(seconds_from_epoch: f64) -> String {
    chrono::DateTime::from_timestamp(seconds_from_epoch as i64, 0)
        .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

async fn debug_oauth_flow(ui: &mut Ui, mcp: &McpService, name: &str, remote: &McpRemote) {
    log_info(
        ui,
        "Initial unauthenticated check returned 401, so this server requires OAuth",
    );
    log_info(
        ui,
        "Testing OAuth flow (without completing authorization)...",
    );
    let config = OAuthConfig::from_setting(remote.oauth.as_ref()).unwrap_or_default();
    match authorization_url(name, &remote.url, &config, &mcp.auth).await {
        Ok(_) => {
            log_info(ui, "OAuth flow triggered");
            match mcp.auth.get(name).and_then(|entry| entry.client_info) {
                Some(client) => log_info(ui, &format!("Client ID available: {}", client.client_id)),
                None => log_info(ui, "No client ID - dynamic registration will be attempted"),
            }
        }
        Err(err) => log_error(ui, &format!("Connection error: {}", err.message)),
    }
}

pub async fn debug(
    ui: &mut Ui,
    config: &Config,
    mcp: &McpService,
    name: &str,
) -> Result<(), TypedError> {
    let server = config.mcp.as_ref().and_then(|mcp| mcp.get(name)).cloned();
    ui.empty();
    intro(ui, "MCP OAuth Debug");
    let Some(McpEntry::Server(info)) = server else {
        log_error(ui, &format!("MCP server not found: {name}"));
        outro(ui, "Done");
        return Ok(());
    };
    let McpInfo::Remote(remote) = info else {
        log_error(ui, &format!("MCP server {name} is not a remote server"));
        outro(ui, "Done");
        return Ok(());
    };
    if matches!(remote.oauth, Some(McpOAuthSetting::Disabled(false))) {
        log_warn(
            ui,
            &format!("MCP server {name} has OAuth explicitly disabled"),
        );
        outro(ui, "Done");
        return Ok(());
    }
    log_info(ui, &format!("Server: {name}"));
    log_info(ui, &format!("URL: {}", remote.url));
    let status = mcp.get_auth_status(name);
    log_info(
        ui,
        &format!("Auth status: {} {}", auth_icon(status), auth_text(status)),
    );
    if let Some(entry) = mcp.auth.get(name) {
        if let Some(tokens) = &entry.tokens {
            let masked = if tokens.access_token.len() > 8 {
                format!(
                    "{}***{}",
                    &tokens.access_token[..4],
                    &tokens.access_token[tokens.access_token.len() - 4..]
                )
            } else {
                "***".to_string()
            };
            log_info(ui, &format!("  Access token: {masked}"));
            if let Some(expires_at) = tokens.expires_at {
                let expired = expires_at < now_seconds();
                let suffix = if expired { " (EXPIRED)" } else { "" };
                log_info(ui, &format!("  Expires: {}{suffix}", iso_date(expires_at)));
            }
            if tokens.refresh_token.is_some() {
                log_info(ui, "  Refresh token: present");
            }
        }
        if let Some(client) = &entry.client_info {
            log_info(ui, &format!("  Client ID: {}", client.client_id));
            if let Some(expires_at) = client.client_secret_expires_at {
                log_info(
                    ui,
                    &format!("  Client secret expires: {}", iso_date(expires_at)),
                );
            }
        }
    }
    let mut request = reqwest::Client::new()
        .post(&remote.url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    if let Some(headers) = &remote.headers {
        for (key, value) in headers {
            request = request.header(key, value);
        }
    }
    let body = json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "params": {
            "protocolVersion": LATEST_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "alforria-debug", "version": env!("CARGO_PKG_VERSION") },
        },
        "id": 1,
    });
    match request.json(&body).send().await {
        Ok(response) => {
            let status = response.status();
            // StatusCode's Display already carries the canonical reason.
            log_info(ui, &format!("HTTP response: {status}"));
            if let Some(www_auth) = response
                .headers()
                .get("www-authenticate")
                .and_then(|value| value.to_str().ok())
            {
                log_info(ui, &format!("WWW-Authenticate: {www_auth}"));
            }
            if status.as_u16() == 401 {
                debug_oauth_flow(ui, mcp, name, &remote).await;
            } else if status.is_success() {
                let text = response.text().await.unwrap_or_default();
                log_success(
                    ui,
                    "Server responded successfully (no auth required or already authenticated)",
                );
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    if let Some(server_info) = value
                        .get("result")
                        .and_then(|result| result.get("serverInfo"))
                    {
                        log_info(
                            ui,
                            &format!(
                                "Server info: {}",
                                serde_json::to_string(server_info).unwrap_or_default()
                            ),
                        );
                    }
                }
            } else {
                log_warn(ui, &format!("Unexpected status: {status}"));
                let text = response.text().await.unwrap_or_default();
                if !text.is_empty() {
                    let truncated: String = text.chars().take(500).collect();
                    log_info(ui, &format!("Response body: {truncated}"));
                }
            }
        }
        Err(err) => {
            log_error(ui, "Connection failed");
            log_error(ui, &format!("Error: {err}"));
        }
    }
    outro(ui, "Debug complete");
    Ok(())
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Command entry
// ---------------------------------------------------------------------------

fn mcp_service(instance: &crate::instance::Instance) -> McpService {
    McpService::new(McpServiceInput {
        directory: instance.directory.clone(),
        data_dir: instance.paths.data.clone(),
        mcp: instance.config.mcp.clone().unwrap_or_default(),
        mcp_timeout: instance
            .config
            .experimental
            .as_ref()
            .and_then(|experimental| experimental.mcp_timeout)
            .map(|timeout| timeout.get()),
        events: Some(instance.services.events.clone()),
    })
}

/// A git project scopes `mcp add` to the worktree config; otherwise only
/// the global config is offered (mcp.ts:515-534).
fn project_dir(instance: &crate::instance::Instance) -> Option<PathBuf> {
    matches!(
        instance.location.project.vcs,
        Some(alforria_schema::project::ProjectVcs::Git)
    )
    .then(|| instance.worktree.clone())
}

pub fn run(matches: &ArgMatches, ui: &mut Ui) -> Result<(), TypedError> {
    let (name, matches) = matches.subcommand().expect("mcp requires a subcommand");
    if name == "add" {
        let instance = crate::instance::boot(None)?;
        let args = AddArgs::from_matches(matches);
        let mut prompter: Box<dyn Prompter> = Box::new(StdinPrompter);
        add(
            ui,
            &args,
            &mut *prompter,
            &instance.paths.config,
            project_dir(&instance).as_deref(),
        )
    } else {
        let instance = crate::instance::boot(None)?;
        let mut mcp = mcp_service(&instance);
        let runtime = super::runtime()?;
        match name {
            "list" => runtime.block_on(list(ui, &instance.config, &mcp)),
            "auth" => {
                if matches.subcommand_name() == Some("list") {
                    runtime.block_on(auth_list(ui, &instance.config, &mcp))
                } else {
                    let name = matches.get_one::<String>("name").map(String::as_str);
                    let mut prompter: Box<dyn Prompter> = Box::new(StdinPrompter);
                    runtime.block_on(auth(ui, &instance.config, &mut mcp, &mut *prompter, name))
                }
            }
            "logout" => {
                let name = matches.get_one::<String>("name").map(String::as_str);
                let mut prompter: Box<dyn Prompter> = Box::new(StdinPrompter);
                runtime.block_on(logout(ui, &mcp, &mut *prompter, name))
            }
            "debug" => {
                let name = matches
                    .get_one::<String>("name")
                    .cloned()
                    .expect("debug name required");
                runtime.block_on(debug(ui, &instance.config, &mcp, &name))
            }
            _ => unreachable!("unknown mcp subcommand"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};

    use alforria_core::mcp::auth::{McpAuthEntry, McpClientInfo, McpTokens};
    use alforria_core::{ConfigLoader, GlobalPaths, LoadParams};

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn load_config(mcp_json: &str) -> (Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("opencode.json"), mcp_json).unwrap();
        let paths = GlobalPaths::resolve(dir.path().join("home"));
        let (config, _) = ConfigLoader::new()
            .load(&LoadParams::new(dir.path().to_path_buf()).paths(paths))
            .unwrap();
        (config, dir)
    }

    fn service(config: &Config, dir: &Path) -> McpService {
        McpService::new(McpServiceInput {
            directory: dir.to_path_buf(),
            data_dir: dir.to_path_buf(),
            mcp: config.mcp.clone().unwrap_or_default(),
            mcp_timeout: None,
            events: None,
        })
    }

    fn seed_tokens(mcp: &McpService, name: &str, url: &str, expires_at: Option<f64>) {
        mcp.auth.set(
            name,
            McpAuthEntry {
                tokens: Some(McpTokens {
                    access_token: "token".to_string(),
                    expires_at,
                    ..Default::default()
                }),
                ..Default::default()
            },
            Some(url),
        );
    }

    /// Scripted prompter: every method pops from its queue.
    struct ScriptPrompter {
        selects: VecDeque<String>,
        texts: VecDeque<String>,
        confirms: VecDeque<bool>,
        passwords: VecDeque<String>,
    }

    impl ScriptPrompter {
        fn from(selects: &[&str], texts: &[&str], confirms: &[bool]) -> Self {
            ScriptPrompter {
                selects: selects.iter().map(|v| v.to_string()).collect(),
                texts: texts.iter().map(|v| v.to_string()).collect(),
                confirms: confirms.iter().copied().collect(),
                passwords: VecDeque::new(),
            }
        }
    }

    impl Prompter for ScriptPrompter {
        fn select(
            &mut self,
            _message: &str,
            _options: &[(String, String)],
        ) -> Result<String, TypedError> {
            Ok(self.selects.pop_front().expect("select queue empty"))
        }

        fn text(&mut self, _message: &str) -> Result<String, TypedError> {
            Ok(self.texts.pop_front().expect("text queue empty"))
        }

        fn password(&mut self, _message: &str) -> Result<String, TypedError> {
            Ok(self.passwords.pop_front().expect("password queue empty"))
        }

        fn confirm(&mut self, _message: &str) -> Result<bool, TypedError> {
            Ok(self.confirms.pop_front().expect("confirm queue empty"))
        }
    }

    const REMOTE: &str = r#"{
      "mcp": {
        "local-srv": { "type": "local", "command": ["npx", "srv"] },
        "no-oauth": { "type": "remote", "url": "https://x.test", "oauth": false },
        "off": { "type": "local", "command": ["false"], "enabled": false },
        "marker": { "enabled": false }
      }
    }"#;

    // ------------------------------------------------------------------
    // list
    // ------------------------------------------------------------------

    #[test]
    fn status_line_renders_each_status() {
        let remote = McpInfo::Remote(McpRemote {
            url: "https://mcp.test".to_string(),
            enabled: None,
            headers: None,
            oauth: None,
            timeout: None,
        });
        let (dim, rest) = (style::TEXT_DIM, style::TEXT_DIM);
        assert_eq!(
            status_line("srv", &remote, Some(&McpStatus::Connected), false),
            format!("✓ srv {dim}connected\n    {rest}https://mcp.test")
        );
        assert_eq!(
            status_line("srv", &remote, None, false),
            format!("○ srv {dim}not initialized\n    {rest}https://mcp.test")
        );
        assert_eq!(
            status_line("srv", &remote, Some(&McpStatus::Disabled), false),
            format!("○ srv {dim}disabled\n    {rest}https://mcp.test")
        );
        assert_eq!(
            status_line("srv", &remote, Some(&McpStatus::NeedsAuth), false),
            format!("⚠ srv {dim}needs authentication\n    {rest}https://mcp.test")
        );
        assert_eq!(
            status_line(
                "srv",
                &remote,
                Some(&McpStatus::Failed {
                    error: "boom".to_string()
                }),
                false
            ),
            format!("✗ srv {dim}failed\n    boom\n    {rest}https://mcp.test")
        );
        assert_eq!(
            status_line(
                "srv",
                &remote,
                Some(&McpStatus::NeedsClientRegistration {
                    error: "no client".to_string()
                }),
                false
            ),
            format!(
                "✗ srv {dim}needs client registration\n    no client\n    {rest}https://mcp.test"
            )
        );
    }

    #[test]
    fn status_line_oauth_hint_and_local_type_hint() {
        let remote = McpInfo::Remote(McpRemote {
            url: "https://mcp.test".to_string(),
            enabled: None,
            headers: None,
            oauth: Some(McpOAuthSetting::OAuth(McpOAuth {
                client_id: None,
                client_secret: None,
                scope: None,
                callback_port: None,
                redirect_uri: None,
            })),
            timeout: None,
        });
        let dim = style::TEXT_DIM;
        assert_eq!(
            status_line("srv", &remote, Some(&McpStatus::Connected), true),
            format!("✓ srv {dim}connected (OAuth)\n    {dim}https://mcp.test")
        );
        let local = McpInfo::Local(McpLocal {
            command: vec!["npx".to_string(), "-y".to_string(), "srv".to_string()],
            cwd: None,
            environment: None,
            enabled: None,
            timeout: None,
        });
        assert_eq!(
            status_line("srv", &local, Some(&McpStatus::Connected), false),
            format!("✓ srv {dim}connected\n    {dim}npx -y srv")
        );
    }

    #[tokio::test]
    async fn list_reports_no_servers() {
        let (config, _dir) = load_config("{}");
        let mcp = service(&config, Path::new("/tmp"));
        let (mut ui, captured) = Ui::capture(false);
        list(&mut ui, &config, &mcp).await.unwrap();
        let stderr = captured.stderr();
        assert!(stderr.contains("┌ MCP Servers\n"), "{stderr}");
        assert!(stderr.contains("│ No MCP servers configured\n"), "{stderr}");
        assert!(
            stderr.contains("└ Add servers with: alforria mcp add\n"),
            "{stderr}"
        );
    }

    #[tokio::test]
    async fn list_displays_disabled_server() {
        let (config, dir) = load_config(
            r#"{"mcp": {"off": {"type": "local", "command": ["false"], "enabled": false},
                          "marker": {"enabled": false}}}"#,
        );
        let mcp = service(&config, dir.path());
        let (mut ui, captured) = Ui::capture(false);
        list(&mut ui, &config, &mcp).await.unwrap();
        let stderr = captured.stderr();
        assert!(
            stderr.contains(&format!("│ ○ off {}disabled", style::TEXT_DIM)),
            "{stderr}"
        );
        assert!(stderr.contains("└ 1 server(s)\n"), "{stderr}");
        // `{enabled: false}` markers are not configured servers.
        assert!(!stderr.contains("marker"), "{stderr}");
    }

    // ------------------------------------------------------------------
    // auth + auth list
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn auth_without_oauth_servers_prints_hint() {
        let (config, dir) = load_config(REMOTE);
        let mut mcp = service(&config, dir.path());
        let mut prompter = ScriptPrompter::from(&[], &[], &[]);
        let (mut ui, captured) = Ui::capture(false);
        auth(&mut ui, &config, &mut mcp, &mut prompter, None)
            .await
            .unwrap();
        let stderr = captured.stderr();
        assert!(
            stderr.contains("│ No OAuth-capable MCP servers configured\n"),
            "{stderr}"
        );
        assert!(
            stderr.contains("│ Remote MCP servers support OAuth by default"),
            "{stderr}"
        );
        assert!(stderr.contains("\"type\": \"remote\""), "{stderr}");
        assert!(stderr.ends_with("└ Done\n"), "{stderr}");
    }

    #[tokio::test]
    async fn auth_reports_missing_and_non_oauth_servers() {
        let (config, dir) = load_config(
            r#"{
              "mcp": {
                "local-srv": { "type": "local", "command": ["npx", "srv"] },
                "remote-srv": { "type": "remote", "url": "https://mcp.test" }
              }
            }"#,
        );
        let mut mcp = service(&config, dir.path());
        let mut prompter = ScriptPrompter::from(&[], &[], &[]);
        let (mut ui, captured) = Ui::capture(false);
        auth(&mut ui, &config, &mut mcp, &mut prompter, Some("missing"))
            .await
            .unwrap();
        let stderr = captured.stderr();
        assert!(
            stderr.contains("│ MCP server not found: missing\n"),
            "{stderr}"
        );
        assert!(stderr.ends_with("└ Done\n"), "{stderr}");

        let (mut ui, captured) = Ui::capture(false);
        auth(&mut ui, &config, &mut mcp, &mut prompter, Some("local-srv"))
            .await
            .unwrap();
        let stderr = captured.stderr();
        assert!(
            stderr.contains("│ MCP server local-srv is not an OAuth-capable remote server\n"),
            "{stderr}"
        );
        assert!(stderr.ends_with("└ Done\n"), "{stderr}");
    }

    #[tokio::test]
    async fn auth_confirms_reauthentication() {
        let (config, dir) =
            load_config(r#"{"mcp": {"ok": {"type": "remote", "url": "https://ok.test"}}}"#);
        let mut mcp = service(&config, dir.path());
        seed_tokens(&mcp, "ok", "https://ok.test", Some(now_seconds() + 3600.0));
        let mut prompter = ScriptPrompter::from(&[], &[], &[false]);
        let (mut ui, captured) = Ui::capture(false);
        auth(&mut ui, &config, &mut mcp, &mut prompter, Some("ok"))
            .await
            .unwrap();
        assert_eq!(
            captured.stderr(),
            "\x1b[0m\n┌ MCP OAuth Authentication\n└ Cancelled\n"
        );
    }

    #[tokio::test]
    async fn auth_expired_credentials_attempt_flow_and_fail() {
        let url = "http://127.0.0.1:1/mcp";
        let (config, dir) = load_config(&format!(
            r#"{{"mcp": {{"ok": {{"type": "remote", "url": "{url}"}}}}}}"#
        ));
        let mut mcp = service(&config, dir.path());
        seed_tokens(&mcp, "ok", url, Some(now_seconds() - 3600.0));
        let mut prompter = ScriptPrompter::from(&[], &[], &[]);
        let (mut ui, captured) = Ui::capture(false);
        auth(&mut ui, &config, &mut mcp, &mut prompter, Some("ok"))
            .await
            .unwrap();
        let stderr = captured.stderr();
        assert!(
            stderr.contains("│ ok has expired credentials. Re-authenticating...\n"),
            "{stderr}"
        );
        assert!(stderr.contains("│ Authentication failed\n"), "{stderr}");
        assert!(stderr.ends_with("└ Done\n"), "{stderr}");
    }

    #[tokio::test]
    async fn auth_list_shows_status_per_server() {
        let (config, dir) = load_config(
            r#"{
              "mcp": {
                "ok": { "type": "remote", "url": "https://ok.test" },
                "expired": { "type": "remote", "url": "https://expired.test" },
                "none": { "type": "remote", "url": "https://none.test" },
                "local-srv": { "type": "local", "command": ["npx"] },
                "off-oauth": { "type": "remote", "url": "https://x.test", "oauth": false }
              }
            }"#,
        );
        let mcp = service(&config, dir.path());
        seed_tokens(&mcp, "ok", "https://ok.test", Some(now_seconds() + 3600.0));
        seed_tokens(&mcp, "expired", "https://expired.test", Some(1.0));
        let (mut ui, captured) = Ui::capture(false);
        auth_list(&mut ui, &config, &mcp).await.unwrap();
        let stderr = captured.stderr();
        let dim = style::TEXT_DIM;
        assert!(
            stderr.contains(&format!(
                "│ ✓ ok {dim}authenticated\n    {dim}https://ok.test"
            )),
            "{stderr}"
        );
        assert!(
            stderr.contains(&format!(
                "│ ⚠ expired {dim}expired\n    {dim}https://expired.test"
            )),
            "{stderr}"
        );
        assert!(
            stderr.contains(&format!(
                "│ ✗ none {dim}not authenticated\n    {dim}https://none.test"
            )),
            "{stderr}"
        );
        assert!(stderr.contains("└ 3 OAuth-capable server(s)\n"), "{stderr}");
    }

    // ------------------------------------------------------------------
    // logout
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn logout_removes_stored_credentials() {
        let (config, dir) =
            load_config(r#"{"mcp": {"ok": {"type": "remote", "url": "https://ok.test"}}}"#);
        let mcp = service(&config, dir.path());
        seed_tokens(&mcp, "ok", "https://ok.test", None);
        let mut prompter = ScriptPrompter::from(&[], &[], &[]);
        let (mut ui, captured) = Ui::capture(false);
        logout(&mut ui, &mcp, &mut prompter, Some("ok"))
            .await
            .unwrap();
        assert!(mcp.auth.all().is_empty());
        let stderr = captured.stderr();
        assert!(
            stderr.contains("│ Removed OAuth credentials for ok\n"),
            "{stderr}"
        );
        assert!(stderr.ends_with("└ Done\n"), "{stderr}");
    }

    #[tokio::test]
    async fn logout_without_and_with_unknown_server() {
        let (config, dir) = load_config("{}");
        let mcp = service(&config, dir.path());
        let mut prompter = ScriptPrompter::from(&[], &[], &[]);
        let (mut ui, captured) = Ui::capture(false);
        logout(&mut ui, &mcp, &mut prompter, Some("ok"))
            .await
            .unwrap();
        let stderr = captured.stderr();
        assert!(
            stderr.contains("│ No MCP OAuth credentials stored\n"),
            "{stderr}"
        );
        assert!(stderr.ends_with("└ Done\n"), "{stderr}");

        seed_tokens(&mcp, "ok", "https://ok.test", None);
        let (mut ui, captured) = Ui::capture(false);
        logout(&mut ui, &mcp, &mut prompter, Some("other"))
            .await
            .unwrap();
        let stderr = captured.stderr();
        assert!(
            stderr.contains("│ No credentials found for: other\n"),
            "{stderr}"
        );
        assert!(mcp.auth.all().contains_key("ok"));
    }

    #[tokio::test]
    async fn logout_prompts_for_selection() {
        let (config, dir) = load_config("{}");
        let mcp = service(&config, dir.path());
        seed_tokens(&mcp, "ok", "https://ok.test", None);
        let mut prompter = ScriptPrompter::from(&["ok"], &[], &[]);
        let (mut ui, captured) = Ui::capture(false);
        logout(&mut ui, &mcp, &mut prompter, None).await.unwrap();
        assert!(mcp.auth.all().is_empty());
        let stderr = captured.stderr();
        assert!(
            stderr.contains("│ Removed OAuth credentials for ok\n"),
            "{stderr}"
        );
    }

    // ------------------------------------------------------------------
    // add
    // ------------------------------------------------------------------

    fn add_args(
        name: Option<&str>,
        url: Option<&str>,
        env: &[&str],
        header: &[&str],
        command: &[&str],
    ) -> AddArgs {
        AddArgs {
            name: name.map(String::from),
            url: url.map(String::from),
            env: env.iter().map(|v| v.to_string()).collect(),
            header: header.iter().map(|v| v.to_string()).collect(),
            command: command.iter().map(|v| v.to_string()).collect(),
        }
    }

    fn written_config(dir: &Path) -> Value {
        let text = std::fs::read_to_string(dir.join("opencode.json")).expect("config written");
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn add_remote_writes_global_config() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        let mut prompter = ScriptPrompter::from(&[], &[], &[]);
        let (mut ui2, captured) = Ui::capture(false);
        add(
            &mut ui2,
            &add_args(
                Some("srv"),
                Some("https://mcp.test"),
                &[],
                &["X-Token=abc"],
                &[],
            ),
            &mut prompter,
            &global,
            None,
        )
        .unwrap();
        let written = written_config(&global);
        assert_eq!(
            written["mcp"]["srv"],
            json!({
                "type": "remote",
                "url": "https://mcp.test",
                "headers": { "X-Token": "abc" }
            })
        );
        let stderr = captured.stderr();
        assert!(
            stderr.contains("│ MCP server \"srv\" added to "),
            "{stderr}"
        );
    }

    #[test]
    fn add_local_writes_command_and_environment() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        let (mut ui, _captured) = Ui::capture(false);
        let mut prompter = ScriptPrompter::from(&[], &[], &[]);
        add(
            &mut ui,
            &add_args(
                Some("srv"),
                None,
                &["A=1"],
                &[],
                &["npx", "-y", "@modelcontextprotocol/server-filesystem"],
            ),
            &mut prompter,
            &global,
            None,
        )
        .unwrap();
        let written = written_config(&global);
        assert_eq!(
            written["mcp"]["srv"],
            json!({
                "type": "local",
                "command": ["npx", "-y", "@modelcontextprotocol/server-filesystem"],
                "environment": { "A": "1" }
            })
        );
    }

    #[test]
    fn add_preserves_existing_config_entries() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("opencode.json"),
            r#"{"model": "anthropic/x", "mcp": {"other": {"type": "local", "command": ["x"]}}}"#,
        )
        .unwrap();
        let (mut ui, _captured) = Ui::capture(false);
        let mut prompter = ScriptPrompter::from(&[], &[], &[]);
        add(
            &mut ui,
            &add_args(Some("srv"), Some("https://mcp.test"), &[], &[], &[]),
            &mut prompter,
            &global,
            None,
        )
        .unwrap();
        let written = written_config(&global);
        assert_eq!(written["model"], json!("anthropic/x"));
        assert_eq!(written["mcp"]["other"]["command"], json!(["x"]));
        assert_eq!(written["mcp"]["srv"]["url"], json!("https://mcp.test"));
    }

    #[test]
    fn add_prefers_json_and_reuses_jsonc() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(global.join("opencode.jsonc"), r#"{"model": "z"}"#).unwrap();
        let (mut ui, _captured) = Ui::capture(false);
        let mut prompter = ScriptPrompter::from(&[], &[], &[]);
        add(
            &mut ui,
            &add_args(Some("srv"), Some("https://mcp.test"), &[], &[], &[]),
            &mut prompter,
            &global,
            None,
        )
        .unwrap();
        assert!(global.join("opencode.jsonc").exists());
        assert!(!global.join("opencode.json").exists());
    }

    #[test]
    fn add_validation_errors() {
        let expected = [
            (
                add_args(None, Some("https://mcp.test"), &[], &[], &[]),
                "A server name is required for non-interactive MCP configuration",
            ),
            (
                add_args(
                    Some("srv"),
                    Some("https://mcp.test"),
                    &[],
                    &[],
                    &["npx", "x"],
                ),
                "Provide either --url <url> or a command after --",
            ),
            (
                add_args(Some("srv"), None, &[], &[], &[]),
                "Provide either --url <url> or a command after --",
            ),
            (
                add_args(Some("srv"), Some("not a url"), &[], &[], &[]),
                "Invalid URL: not a url",
            ),
            (
                add_args(Some("srv"), Some("https://mcp.test"), &["A=1"], &[], &[]),
                "--env is only valid for local MCP servers",
            ),
            (
                add_args(Some("srv"), None, &[], &["X=1"], &["npx"]),
                "--header is only valid for remote MCP servers",
            ),
            (
                add_args(Some("srv"), None, &["NOEQUALS"], &[], &["npx"]),
                "Invalid environment variable: NOEQUALS. Expected KEY=VALUE",
            ),
            (
                add_args(Some("srv"), Some("https://mcp.test"), &[], &["=x"], &[]),
                "Invalid HTTP header: =x. Expected KEY=VALUE",
            ),
        ];
        for (args, message) in expected {
            let (mut ui, _captured) = Ui::capture(false);
            let mut prompter = ScriptPrompter::from(&[], &[], &[]);
            let err = add(&mut ui, &args, &mut prompter, Path::new("/tmp"), None).unwrap_err();
            assert_eq!(
                crate::error::format_error(&err),
                Some(message.to_string()),
                "{args:?}"
            );
        }
    }

    #[test]
    fn add_interactive_local() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        let (mut ui, captured) = Ui::capture(false);
        let mut prompter =
            ScriptPrompter::from(&["local"], &["srv", "npx -y server-everything"], &[]);
        add(&mut ui, &AddArgs::default(), &mut prompter, &global, None).unwrap();
        let written = written_config(&global);
        assert_eq!(
            written["mcp"]["srv"],
            json!({ "type": "local", "command": ["npx", "-y", "server-everything"] })
        );
        let stderr = captured.stderr();
        assert!(stderr.contains("┌ Add MCP server\n"), "{stderr}");
        assert!(
            stderr.contains("└ MCP server added successfully\n"),
            "{stderr}"
        );
    }

    #[test]
    fn add_interactive_remote_with_oauth_client() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        let (mut ui, _captured) = Ui::capture(false);
        let mut prompter = ScriptPrompter::from(
            &["remote"],
            &["srv", "https://mcp.test", "my-client"],
            &[true, true, true],
        );
        prompter.passwords.push_back("secret".to_string());
        add(&mut ui, &AddArgs::default(), &mut prompter, &global, None).unwrap();
        let written = written_config(&global);
        assert_eq!(
            written["mcp"]["srv"],
            json!({
                "type": "remote",
                "url": "https://mcp.test",
                "oauth": { "clientId": "my-client", "clientSecret": "secret" }
            })
        );
    }

    #[test]
    fn add_interactive_remote_oauth_without_client_id() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        let (mut ui, _captured) = Ui::capture(false);
        let mut prompter =
            ScriptPrompter::from(&["remote"], &["srv", "https://mcp.test"], &[true, false]);
        add(&mut ui, &AddArgs::default(), &mut prompter, &global, None).unwrap();
        let written = written_config(&global);
        assert_eq!(
            written["mcp"]["srv"],
            json!({ "type": "remote", "url": "https://mcp.test", "oauth": {} })
        );
    }

    #[test]
    fn add_interactive_project_scope_uses_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        let project = dir.path().join("worktree");
        let (mut ui, _captured) = Ui::capture(false);
        let mut prompter = ScriptPrompter::from(
            &[&project.join("opencode.json").to_string_lossy(), "local"],
            &["srv", "echo hi"],
            &[],
        );
        add(
            &mut ui,
            &AddArgs::default(),
            &mut prompter,
            &global,
            Some(&project),
        )
        .unwrap();
        let written = written_config(&project);
        assert_eq!(
            written["mcp"]["srv"],
            json!({ "type": "local", "command": ["echo", "hi"] })
        );
    }

    #[test]
    fn add_flags_parse_their_surface() {
        let matches = crate::cmd::cli()
            .try_get_matches_from([
                "alforria",
                "mcp",
                "add",
                "srv",
                "--url",
                "https://mcp.test",
                "--header",
                "X-Token=abc",
                "--header",
                "Y=z",
            ])
            .unwrap();
        let add = matches
            .subcommand_matches("mcp")
            .unwrap()
            .subcommand_matches("add")
            .unwrap();
        let args = AddArgs::from_matches(add);
        assert_eq!(args.name.as_deref(), Some("srv"));
        assert_eq!(args.url.as_deref(), Some("https://mcp.test"));
        assert_eq!(args.header, vec!["X-Token=abc", "Y=z"]);

        let matches = crate::cmd::cli()
            .try_get_matches_from([
                "alforria", "mcp", "add", "srv", "--env", "A=1", "--", "npx", "-y", "server",
            ])
            .unwrap();
        let add = matches
            .subcommand_matches("mcp")
            .unwrap()
            .subcommand_matches("add")
            .unwrap();
        let args = AddArgs::from_matches(add);
        assert_eq!(args.env, vec!["A=1"]);
        assert_eq!(args.command, vec!["npx", "-y", "server"]);
    }

    // ------------------------------------------------------------------
    // debug
    // ------------------------------------------------------------------

    fn read_request(stream: &mut TcpStream) {
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .ok();
        let mut received = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    received.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&received).to_string();
                    let Some(header_end) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let content_length = text[..header_end]
                        .to_ascii_lowercase()
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if received.len() >= header_end + 4 + content_length {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }

    /// A one-response HTTP stub server; the thread accepts connections until
    /// the test process exits.
    fn spawn_stub(status_line: &str, headers: &[&str], body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let status_line = status_line.to_string();
        let headers = headers
            .iter()
            .map(|h| format!("{h}\r\n"))
            .collect::<String>();
        let body = body.to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                read_request(&mut stream);
                let response = format!(
                    "{status_line}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.shutdown(Shutdown::Both);
            }
        });
        url
    }

    #[tokio::test]
    async fn debug_reports_missing_local_and_disabled() {
        let (config, dir) = load_config(REMOTE);
        let mcp = service(&config, dir.path());
        for (name, expected) in [
            ("missing", "MCP server not found: missing"),
            ("local-srv", "MCP server local-srv is not a remote server"),
            (
                "no-oauth",
                "MCP server no-oauth has OAuth explicitly disabled",
            ),
        ] {
            let (mut ui, captured) = Ui::capture(false);
            debug(&mut ui, &config, &mcp, name).await.unwrap();
            let stderr = captured.stderr();
            assert!(stderr.contains(&format!("│ {expected}\n")), "{stderr}");
            assert!(stderr.ends_with("└ Done\n"));
        }
    }

    #[tokio::test]
    async fn debug_stub_server_responds_successfully() {
        let url = spawn_stub(
            "HTTP/1.1 200 OK",
            &["content-type: application/json"],
            r#"{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"stub","version":"1.0"}}}"#,
        );
        let (config, dir) = load_config(&format!(
            r#"{{"mcp": {{"srv": {{"type": "remote", "url": "{url}"}}}}}}"#
        ));
        let mcp = service(&config, dir.path());
        let (mut ui, captured) = Ui::capture(false);
        debug(&mut ui, &config, &mcp, "srv").await.unwrap();
        let stderr = captured.stderr();
        assert!(stderr.contains("┌ MCP OAuth Debug\n"), "{stderr}");
        assert!(stderr.contains(&format!("│ URL: {url}\n")), "{stderr}");
        assert!(
            stderr.contains("│ Auth status: ✗ not authenticated\n"),
            "{stderr}"
        );
        assert!(stderr.contains("│ HTTP response: 200 OK\n"), "{stderr}");
        assert!(
            stderr.contains(
                "│ Server responded successfully (no auth required or already authenticated)"
            ),
            "{stderr}"
        );
        assert!(
            stderr.contains(r#"│ Server info: {"name":"stub","version":"1.0"}"#),
            "{stderr}"
        );
        assert!(stderr.ends_with("└ Debug complete\n"), "{stderr}");
    }

    #[tokio::test]
    async fn debug_401_triggers_oauth_flow() {
        let url = spawn_stub(
            "HTTP/1.1 401 Unauthorized",
            &["content-type: text/plain"],
            "Unauthorized",
        );
        let (config, dir) = load_config(&format!(
            r#"{{"mcp": {{"srv": {{"type": "remote", "url": "{url}"}}}}}}"#
        ));
        let mcp = service(&config, dir.path());
        let (mut ui, captured) = Ui::capture(false);
        debug(&mut ui, &config, &mcp, "srv").await.unwrap();
        let stderr = captured.stderr();
        assert!(
            stderr.contains("│ HTTP response: 401 Unauthorized\n"),
            "{stderr}"
        );
        assert!(
            stderr.contains(
                "│ Initial unauthenticated check returned 401, so this server requires OAuth"
            ),
            "{stderr}"
        );
        assert!(
            stderr.contains("│ Connection error: Incompatible auth server: does not support dynamic client registration"),
            "{stderr}"
        );
        assert!(stderr.ends_with("└ Debug complete\n"), "{stderr}");
    }

    #[tokio::test]
    async fn debug_displays_stored_credentials() {
        let url = "http://127.0.0.1:1/mcp";
        let (config, dir) = load_config(&format!(
            r#"{{"mcp": {{"srv": {{"type": "remote", "url": "{url}"}}}}}}"#
        ));
        let mcp = service(&config, dir.path());
        mcp.auth.set(
            "srv",
            McpAuthEntry {
                tokens: Some(McpTokens {
                    access_token: "abcdefghijklmnop".to_string(),
                    refresh_token: Some("r".to_string()),
                    expires_at: Some(now_seconds() + 3600.0),
                    ..Default::default()
                }),
                client_info: Some(McpClientInfo {
                    client_id: "client-1".to_string(),
                    client_secret: None,
                    client_id_issued_at: None,
                    client_secret_expires_at: Some(now_seconds() + 86400.0),
                }),
                ..Default::default()
            },
            Some(url),
        );
        let (mut ui, captured) = Ui::capture(false);
        debug(&mut ui, &config, &mcp, "srv").await.unwrap();
        let stderr = captured.stderr();
        assert!(
            stderr.contains("│ Auth status: ✓ authenticated\n"),
            "{stderr}"
        );
        assert!(
            stderr.contains("│   Access token: abcd***mnop\n"),
            "{stderr}"
        );
        assert!(stderr.contains("│   Refresh token: present\n"), "{stderr}");
        assert!(
            stderr.contains("│   Expires: 20") && !stderr.contains("(EXPIRED)"),
            "{stderr}"
        );
        assert!(stderr.contains("│   Client ID: client-1\n"), "{stderr}");
        assert!(stderr.contains("│   Client secret expires: 20"), "{stderr}");
        assert!(stderr.contains("│ Connection failed\n"), "{stderr}");
        assert!(stderr.ends_with("└ Debug complete\n"), "{stderr}");
    }
}
