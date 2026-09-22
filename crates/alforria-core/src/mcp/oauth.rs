//! MCP OAuth support — port of `mcp/oauth-provider.ts`,
//! `mcp/oauth-callback.ts` and `mcp/browser.ts` over the OAuth essentials
//! of SDK `client/auth.js`.
//!
//! The TS flow relies on the SDK's `auth()` helper: protected-resource
//! discovery, authorization-server metadata, RFC 7591 dynamic client
//! registration, PKCE and the code-for-token exchange. The Rust port
//! implements the same steps against the same endpoints; the interactive
//! pieces (callback HTTP server, browser opener) are seams for tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::mcp::auth::{McpAuth, McpClientInfo};
use crate::mcp::transport::McpError;

/// `OAUTH_CALLBACK_PORT` / `OAUTH_CALLBACK_PATH`
/// (oauth-provider.ts:11-12).
pub const OAUTH_CALLBACK_PORT: u16 = 19_876;
pub const OAUTH_CALLBACK_PATH: &str = "/mcp/oauth/callback";

/// `CALLBACK_TIMEOUT_MS` (oauth-callback.ts:24).
const CALLBACK_TIMEOUT_MS: u64 = 5 * 60 * 1000;

/// `McpOAuthConfig` (oauth-provider.ts:14-20) — the `oauth` config slice.
#[derive(Debug, Clone, Default)]
pub struct OAuthConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scope: Option<String>,
    pub callback_port: Option<u16>,
    pub redirect_uri: Option<String>,
}

impl OAuthConfig {
    /// The effective `oauth` object config; `None` when disabled.
    pub fn from_setting(
        setting: Option<&crate::config::schema::McpOAuthSetting>,
    ) -> Option<OAuthConfig> {
        match setting? {
            crate::config::schema::McpOAuthSetting::OAuth(oauth) => Some(OAuthConfig {
                client_id: oauth.client_id.clone(),
                client_secret: oauth.client_secret.clone(),
                scope: oauth.scope.clone(),
                callback_port: oauth.callback_port.map(|port| port.0),
                redirect_uri: oauth.redirect_uri.clone(),
            }),
            crate::config::schema::McpOAuthSetting::Disabled(_) => None,
        }
    }
}

/// `redirectUrl` (oauth-provider.ts:35-41).
pub fn redirect_url(config: &OAuthConfig) -> String {
    if let Some(uri) = &config.redirect_uri {
        return uri.clone();
    }
    let port = config.callback_port.unwrap_or(OAUTH_CALLBACK_PORT);
    format!("http://127.0.0.1:{port}{OAUTH_CALLBACK_PATH}")
}

/// `clientMetadata` (oauth-provider.ts:43-53).
pub fn client_metadata(config: &OAuthConfig) -> Value {
    let mut metadata = json!({
        "redirect_uris": [redirect_url(config)],
        "client_name": "OpenCode",
        "client_uri": "https://opencode.ai",
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": if config.client_secret.is_some() { "client_secret_post" } else { "none" },
    });
    if let Some(scope) = &config.scope {
        metadata["scope"] = Value::from(scope.clone());
    }
    metadata
}

/// `parseRedirectUri` (oauth-provider.ts:246-259).
pub fn parse_redirect_uri(redirect_uri: Option<&str>) -> (u16, String) {
    let Some(uri) = redirect_uri else {
        return (OAUTH_CALLBACK_PORT, OAUTH_CALLBACK_PATH.to_string());
    };
    let (scheme, rest) = uri.split_once("://").unwrap_or(("http", "127.0.0.1"));
    let (authority, path_query) = match rest.split_once('/') {
        Some((authority, path)) => (authority, path),
        None => (rest, ""),
    };
    let port = match authority.rsplit_once(':').and_then(|(_, p)| p.parse().ok()) {
        Some(port) => port,
        _ if scheme == "https" => 443,
        _ => 80,
    };
    let path = path_query.split(['?', '#']).next().unwrap_or_default();
    let path = if path.is_empty() {
        OAUTH_CALLBACK_PATH.to_string()
    } else {
        format!("/{path}")
    };
    (port, path)
}

// ---------------------------------------------------------------------------
// browser
// ---------------------------------------------------------------------------

/// `McpBrowser.Service` (browser.ts:5-7).
pub trait McpBrowser: Send + Sync {
    fn open<'a>(&'a self, url: &'a str) -> crate::tool::def::BoxFuture<'a, Result<(), String>>;
}

/// The `open` package's platform behavior (browser.ts:13): spawn the
/// desktop opener and treat a fast non-zero exit as failure.
pub struct SystemBrowser;

impl McpBrowser for SystemBrowser {
    fn open<'a>(&'a self, url: &'a str) -> crate::tool::def::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let command = if cfg!(target_os = "macos") {
                "open"
            } else if cfg!(target_os = "windows") {
                "cmd"
            } else {
                "xdg-open"
            };
            let args: Vec<String> = if command == "cmd" {
                vec!["/c".to_string(), "start".to_string(), url.to_string()]
            } else {
                vec![url.to_string()]
            };
            match tokio::time::timeout(
                Duration::from_millis(500),
                tokio::process::Command::new(command)
                    .args(&args)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status(),
            )
            .await
            {
                Err(_) => Ok(()),
                Ok(Err(err)) => Err(err.to_string()),
                Ok(Ok(status)) if status.success() || status.code().is_none() => Ok(()),
                Ok(Ok(status)) => Err(format!(
                    "Browser open failed with exit code {}",
                    status.code().unwrap_or(-1)
                )),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// callback server (oauth-callback.ts)
// ---------------------------------------------------------------------------

struct PendingAuth {
    tx: tokio::sync::oneshot::Sender<Result<String, String>>,
}

#[derive(Default)]
struct CallbackState {
    pending: HashMap<String, PendingAuth>,
    by_name: HashMap<String, String>,
}

/// The OAuth callback listener: one server per process, started with the
/// redirect URI's port/path (`ensureRunning`, oauth-callback.ts:105-131).
#[derive(Clone)]
pub struct OAuthCallbackServer {
    state: Arc<Mutex<CallbackState>>,
    config: Arc<Mutex<Option<(u16, String)>>>,
    task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl Default for OAuthCallbackServer {
    fn default() -> Self {
        OAuthCallbackServer::new()
    }
}

impl OAuthCallbackServer {
    pub fn new() -> OAuthCallbackServer {
        OAuthCallbackServer {
            state: Arc::new(Mutex::new(CallbackState::default())),
            config: Arc::new(Mutex::new(None)),
            task: Arc::new(Mutex::new(None)),
        }
    }

    /// `ensureRunning(redirectUri)` — a no-op when a listener already
    /// owns the same port, or when the port is taken by another process
    /// (`isPortInUse`, oauth-callback.ts:163-174).
    pub async fn ensure_running(&self, redirect_uri: Option<&str>) -> Result<(), McpError> {
        let (port, path) = parse_redirect_uri(redirect_uri);
        let started = {
            let mut config = self.config.lock().unwrap_or_else(|p| p.into_inner());
            if config.as_ref().map(|(p, _)| *p) == Some(port) {
                return Ok(());
            }
            *config = Some((port, path.clone()));
            true
        };
        if !started {
            return Ok(());
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|err| McpError::failed(format!("Failed to bind callback server: {err}")))?;
        let state = Arc::clone(&self.state);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let state = Arc::clone(&state);
                tokio::spawn(handle_callback(stream, state, path.clone()));
            }
        });
        *self.task.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
        Ok(())
    }

    /// `waitForCallback` (oauth-callback.ts:133-147).
    pub async fn wait_for_callback(
        &self,
        oauth_state: &str,
        mcp_name: Option<&str>,
    ) -> Result<String, String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(name) = mcp_name {
                state
                    .by_name
                    .insert(name.to_string(), oauth_state.to_string());
            }
            state
                .pending
                .insert(oauth_state.to_string(), PendingAuth { tx });
        }
        match tokio::time::timeout(Duration::from_millis(CALLBACK_TIMEOUT_MS), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("Authorization cancelled".to_string()),
            Err(_) => {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                state.pending.remove(oauth_state);
                Err("OAuth callback timeout - authorization took too long".to_string())
            }
        }
    }

    /// `cancelPending` (oauth-callback.ts:149-161).
    pub fn cancel_pending(&self, mcp_name: &str) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(name) = state.by_name.remove(mcp_name) {
            state.pending.remove(&name);
        }
    }
}

async fn handle_callback(
    mut stream: tokio::net::TcpStream,
    state: Arc<Mutex<CallbackState>>,
    _path: String,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buffer = [0u8; 8192];
    let Ok(size) = stream.read(&mut buffer).await else {
        return;
    };
    let request = String::from_utf8_lossy(&buffer[..size]).to_string();
    let query = request
        .split_whitespace()
        .nth(1)
        .and_then(|target| target.split_once('?').map(|(_, q)| q.to_string()))
        .unwrap_or_default();
    let params = parse_query(&query);
    let code = params.get("code").cloned();
    let state_param = params.get("state").cloned();
    let error = params.get("error").cloned();
    let error_description = params.get("error_description").cloned();
    let status_line = if let Some(oauth_state) = state_param {
        let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(pending) = state.pending.remove(&oauth_state) {
            state.by_name.retain(|_, value| value != &oauth_state);
            if let Some(error) = error {
                let _ = pending.tx.send(Err(error_description.unwrap_or(error)));
            } else if let Some(code) = code {
                let _ = pending.tx.send(Ok(code));
            }
        }
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\n\r\n"
    } else {
        "HTTP/1.1 400 Bad Request\r\ncontent-type: text/html; charset=utf-8\r\n\r\n"
    };
    let _ = stream.write_all(status_line.as_bytes()).await;
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((url_decode(key), url_decode(value)))
        })
        .collect()
}

fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// OAuth flow (client/auth.js)
// ---------------------------------------------------------------------------

/// One authorization-server metadata document.
#[derive(Debug, Clone, Default)]
pub struct ServerMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
}

/// `discoverAuthorizationServerMetadata` (auth.js:636-645): the
/// `oauth-authorization-server` well-known first, then
/// `openid-configuration`.
async fn fetch_metadata(base: &str, client: &reqwest::Client) -> Option<Value> {
    for well_known in [
        ".well-known/oauth-authorization-server",
        ".well-known/openid-configuration",
    ] {
        let Ok(url) = format!("{base}/{well_known}").parse::<reqwest::Url>() else {
            continue;
        };
        let response = client.get(url).send().await.ok()?;
        if !response.status().is_success() {
            continue;
        }
        let value = response.json::<Value>().await.ok()?;
        if value.get("authorization_endpoint").is_some() && value.get("token_endpoint").is_some() {
            return Some(value);
        }
    }
    None
}

async fn get_json(url: &str) -> Result<Value, McpError> {
    reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|err| McpError::failed(format!("{err}")))?
        .json::<Value>()
        .await
        .map_err(|err| McpError::failed(format!("{err}")))
}

/// Protected-resource discovery (auth.js:419-451): fall back to the
/// server URL itself when the metadata document is absent.
async fn discover_authorization_server(server_url: &str) -> Result<String, McpError> {
    let base = server_url.trim_end_matches('/');
    if let Ok(value) = get_json(&format!("{base}/.well-known/oauth-protected-resource")).await {
        if let Some(servers) = value.get("authorization_servers").and_then(Value::as_array) {
            if let Some(first) = servers.first().and_then(Value::as_str) {
                return Ok(first.to_string());
            }
        }
    }
    let Ok(url) = reqwest::Url::parse(server_url) else {
        return Err(McpError::failed("Invalid server URL"));
    };
    let origin = match (url.scheme(), url.host_str(), url.port()) {
        (scheme, Some(host), Some(port)) => format!("{scheme}://{host}:{port}"),
        (scheme, Some(host), None) => format!("{scheme}://{host}"),
        _ => server_url.to_string(),
    };
    Ok(origin)
}

/// Resolve the authorization server metadata (`auth()`, auth.js:825-870).
async fn discover_metadata(server_url: &str) -> Result<ServerMetadata, McpError> {
    let client = reqwest::Client::new();
    let authorization_server = discover_authorization_server(server_url).await?;
    if let Some(value) = fetch_metadata(&authorization_server, &client).await {
        return Ok(ServerMetadata {
            authorization_endpoint: value
                .get("authorization_endpoint")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            token_endpoint: value
                .get("token_endpoint")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            registration_endpoint: value
                .get("registration_endpoint")
                .and_then(Value::as_str)
                .map(String::from),
        });
    }
    // The server URL doubles as the authorization server (auth.js:669-672).
    Ok(ServerMetadata {
        authorization_endpoint: format!("{authorization_server}/authorize"),
        token_endpoint: format!("{authorization_server}/token"),
        registration_endpoint: Some(format!("{authorization_server}/register")),
    })
}

/// `registerClient` (auth.js:901-930) — RFC 7591 dynamic registration.
async fn register_client(
    metadata: &ServerMetadata,
    authorization_server: &str,
    config: &OAuthConfig,
) -> Result<McpClientInfo, McpError> {
    let registration_endpoint = metadata
        .registration_endpoint
        .clone()
        .unwrap_or_else(|| format!("{}/register", authorization_server.trim_end_matches('/')));
    let response = reqwest::Client::new()
        .post(&registration_endpoint)
        .header("Content-Type", "application/json")
        .json(&client_metadata(config))
        .send()
        .await
        .map_err(|err| McpError::failed(format!("{err}")))?;
    if !response.status().is_success() {
        return Err(McpError::unauthorized(
            "Incompatible auth server: does not support dynamic client registration",
        ));
    }
    let value = response
        .json::<Value>()
        .await
        .map_err(|err| McpError::failed(format!("{err}")))?;
    Ok(McpClientInfo {
        client_id: value
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        client_secret: value
            .get("client_secret")
            .and_then(Value::as_str)
            .map(String::from),
        client_id_issued_at: value.get("client_id_issued_at").and_then(Value::as_f64),
        client_secret_expires_at: value
            .get("client_secret_expires_at")
            .and_then(Value::as_f64),
    })
}

/// Resolve the client information (`clientInformation`,
/// oauth-provider.ts:55-79): config first, then the stored dynamic
/// registration.
pub async fn client_information(
    mcp_name: &str,
    server_url: &str,
    metadata: &ServerMetadata,
    authorization_server: &str,
    config: &OAuthConfig,
    auth: &McpAuth,
) -> Result<McpClientInfo, McpError> {
    if let Some(client_id) = &config.client_id {
        return Ok(McpClientInfo {
            client_id: client_id.clone(),
            client_secret: config.client_secret.clone(),
            client_id_issued_at: None,
            client_secret_expires_at: None,
        });
    }
    if let Some(entry) = auth.get_for_url(mcp_name, server_url) {
        if let Some(client_info) = entry.client_info {
            if client_info.client_secret_expires_at.is_none() {
                return Ok(client_info);
            }
        }
    }
    let registered = register_client(metadata, authorization_server, config).await?;
    auth.update_client_info(mcp_name, registered.clone(), Some(server_url));
    Ok(registered)
}

fn random_state() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::thread_rng().gen();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_verifier() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 48];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `pkceChallenge` (shared/auth.js): SHA-256 S256 code challenge.
fn pkce_challenge(verifier: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// The `startAuth` preparation (index.ts:806-870): discovery +
/// registration + PKCE + the authorization URL. Returns the URL and the
/// oauth state.
pub async fn authorization_url(
    mcp_name: &str,
    server_url: &str,
    config: &OAuthConfig,
    auth: &McpAuth,
) -> Result<(String, String), McpError> {
    let authorization_server = discover_authorization_server(server_url).await?;
    let metadata = discover_metadata(server_url).await?;
    let client_info = client_information(
        mcp_name,
        server_url,
        &metadata,
        &authorization_server,
        config,
        auth,
    )
    .await?;
    let verifier = random_verifier();
    let state = random_state();
    auth.update_code_verifier(mcp_name, &verifier);
    auth.update_oauth_state(mcp_name, &state);
    let mut url = reqwest::Url::parse(&metadata.authorization_endpoint)
        .map_err(|err| McpError::failed(format!("{err}")))?;
    url.query_pairs_mut().append_pair("response_type", "code");
    url.query_pairs_mut()
        .append_pair("client_id", &client_info.client_id);
    url.query_pairs_mut()
        .append_pair("redirect_uri", &redirect_url(config));
    url.query_pairs_mut()
        .append_pair("code_challenge", &pkce_challenge(&verifier));
    url.query_pairs_mut()
        .append_pair("code_challenge_method", "S256");
    url.query_pairs_mut().append_pair("state", &state);
    if let Some(scope) = &config.scope {
        url.query_pairs_mut().append_pair("scope", scope);
    }
    Ok((url.to_string(), state))
}

/// The `finishAuth` token exchange (auth.js:838-898).
pub async fn exchange_code(
    mcp_name: &str,
    server_url: &str,
    config: &OAuthConfig,
    auth: &McpAuth,
    authorization_code: &str,
) -> Result<crate::mcp::auth::McpTokens, McpError> {
    let metadata = discover_metadata(server_url).await?;
    let Some(entry) = auth.get(mcp_name) else {
        return Err(McpError::failed("No pending OAuth flow for MCP server"));
    };
    let Some(verifier) = entry.code_verifier.clone() else {
        return Err(McpError::failed("No code verifier saved"));
    };
    let Some(client_info) = entry.client_info.clone() else {
        return Err(McpError::failed("No client information saved"));
    };
    let form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", authorization_code.to_string()),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_url(config)),
        ("client_id", client_info.client_id),
    ];
    let response = reqwest::Client::new()
        .post(&metadata.token_endpoint)
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|err| McpError::failed(format!("{err}")))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(McpError::failed(format!(
            "Token request failed: HTTP {status} {text}"
        )));
    }
    let value = response
        .json::<Value>()
        .await
        .map_err(|err| McpError::failed(format!("{err}")))?;
    let expires_at = value
        .get("expires_in")
        .and_then(Value::as_f64)
        .map(|expires| now_seconds() + expires);
    let tokens = crate::mcp::auth::McpTokens {
        access_token: value
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(String::from),
        expires_at,
        scope: value.get("scope").and_then(Value::as_str).map(String::from),
    };
    auth.update_tokens(mcp_name, tokens.clone(), Some(server_url));
    Ok(tokens)
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_url_defaults() {
        assert_eq!(
            redirect_url(&OAuthConfig::default()),
            format!("http://127.0.0.1:{OAUTH_CALLBACK_PORT}{OAUTH_CALLBACK_PATH}")
        );
        assert_eq!(
            redirect_url(&OAuthConfig {
                callback_port: Some(1234),
                ..Default::default()
            }),
            "http://127.0.0.1:1234/mcp/oauth/callback"
        );
    }

    #[test]
    fn client_metadata_shape() {
        let metadata = client_metadata(&OAuthConfig::default());
        assert_eq!(metadata["client_name"], "OpenCode");
        assert_eq!(metadata["client_uri"], "https://opencode.ai");
        assert_eq!(metadata["token_endpoint_auth_method"], "none");
        let metadata = client_metadata(&OAuthConfig {
            client_secret: Some("s".into()),
            scope: Some("read".into()),
            ..Default::default()
        });
        assert_eq!(metadata["token_endpoint_auth_method"], "client_secret_post");
        assert_eq!(metadata["scope"], "read");
    }

    #[test]
    fn parse_redirect_uri_shapes() {
        assert_eq!(
            parse_redirect_uri(None),
            (OAUTH_CALLBACK_PORT, OAUTH_CALLBACK_PATH.to_string())
        );
        assert_eq!(
            parse_redirect_uri(Some("http://127.0.0.1:8080/custom/path")),
            (8080, "/custom/path".to_string())
        );
        assert_eq!(
            parse_redirect_uri(Some("http://127.0.0.1:9/x?query=1")),
            (9, "/x".to_string())
        );
    }

    #[test]
    fn query_decodes() {
        let params = parse_query("code=abc&state=x%20y&error=a+b");
        assert_eq!(params.get("code").map(String::as_str), Some("abc"));
        assert_eq!(params.get("state").map(String::as_str), Some("x y"));
        assert_eq!(params.get("error").map(String::as_str), Some("a b"));
    }

    #[test]
    fn pkce_challenge_is_s256_base64url() {
        let challenge = pkce_challenge("verifier");
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
    }
}
