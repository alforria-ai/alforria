//! LibertAI account/SSO surface — browser login (PKCE + loopback), the
//! account API client, and the refresh-token sidecar store.
//!
//! Mirrors the libertai-cli reference implementation: the console authorize
//! page takes a custom `challenge` query param (no `code_challenge_method`),
//! the exchange returns a 30-day rotating refresh token, and the minted
//! `LTAI_` key goes into the regular auth.json store — the sidecar keeps
//! only the session refresh token (for usage/billing) and the stable device
//! id that names this install's CLI key.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::GlobalPaths;

/// Account API base (`/auth/*`, `/api-keys/*`, `/payments/*`).
pub const LIBERTAI_ACCOUNT_BASE: &str = "https://inference.api.libertai.io";

/// Console authorize page base (`LIBERTAI_CONSOLE_URL` override).
const DEFAULT_CONSOLE_BASE: &str = "https://console.libertai.io";

/// Sidecar file name inside `GlobalPaths.data`.
const SESSION_FILE: &str = "libertai-auth.json";

/// How long the loopback callback waits before giving up.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Per-connection read budget on the loopback, so a connection that never
/// sends its request (a speculative pre-connect) can't stall the wait.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// A redirect request is one short GET; anything longer is not one.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

pub fn account_base() -> String {
    env_override("LIBERTAI_ACCOUNT_BASE", LIBERTAI_ACCOUNT_BASE)
}

pub fn console_base() -> String {
    env_override("LIBERTAI_CONSOLE_URL", DEFAULT_CONSOLE_BASE)
}

fn env_override(env_var: &str, default: &str) -> String {
    std::env::var(env_var)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// The account API, console and session sidecar a login talks to — the
/// env-resolved defaults in production, fakes in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// Account API base (`/auth/*`, `/api-keys/*`).
    pub account: String,
    /// Console base serving the `/cli` authorize page.
    pub console: String,
    /// The refresh-token sidecar.
    pub session_file: PathBuf,
}

impl Endpoints {
    pub fn from_env() -> Endpoints {
        Endpoints {
            account: account_base(),
            console: console_base(),
            session_file: session_path(),
        }
    }
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

/// The custom PKCE pair the console authorize page expects: the challenge is
/// S256 (base64url of SHA-256(verifier)) but travels as a `challenge` query
/// param — no standard `code_challenge`/`code_challenge_method` fields; the
/// verifier itself travels in the exchange body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

impl Pkce {
    pub fn generate() -> Pkce {
        let mut verifier_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut verifier_bytes);
        let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
        let mut state_bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut state_bytes);
        Pkce {
            challenge: URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            state: URL_SAFE_NO_PAD.encode(state_bytes),
            verifier,
        }
    }
}

/// `{"redirect_uri", "state", "challenge", "client"}` against `{console}/cli`.
pub fn authorize_url(pkce: &Pkce, client: &str, redirect_uri: &str) -> String {
    Endpoints::from_env().authorize_url(pkce, client, redirect_uri)
}

impl Endpoints {
    /// [`authorize_url`] against this console.
    pub fn authorize_url(&self, pkce: &Pkce, client: &str, redirect_uri: &str) -> String {
        format!(
            "{}/cli?redirect_uri={}&state={}&challenge={}&client={}",
            self.console,
            urlencode(redirect_uri),
            urlencode(&pkce.state),
            urlencode(&pkce.challenge),
            urlencode(client),
        )
    }
}

fn urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

// ---------------------------------------------------------------------------
// loopback callback
// ---------------------------------------------------------------------------

/// One-shot loopback callback server on `127.0.0.1:<os-assigned>`.
pub struct CallbackServer {
    listener: TcpListener,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callback {
    pub code: String,
    pub state: String,
}

/// One request read off the loopback: its path and the redirect params.
#[derive(Debug, Default)]
struct LoopbackRequest {
    path: String,
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

impl CallbackServer {
    pub fn bind() -> std::io::Result<CallbackServer> {
        Ok(CallbackServer {
            listener: TcpListener::bind("127.0.0.1:0")?,
        })
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.port())
    }

    pub fn port(&self) -> u16 {
        self.listener
            .local_addr()
            .map(|addr| addr.port())
            .unwrap_or(0)
    }

    /// Serve the GET to `/callback` that carries `expected_state`, reply
    /// with a "you can close this tab" page, and return its `code` +
    /// `state` query params.
    pub fn wait(&self, timeout: Duration, expected_state: &str) -> Result<Callback, String> {
        self.wait_cancellable(timeout, expected_state, &AtomicBool::new(false))
    }

    /// [`wait`](Self::wait) that also gives up once `cancel` is set, so a
    /// login held by the server releases its port when the flow is replaced
    /// or finished elsewhere. Requests for any other path (a favicon probe,
    /// an empty pre-connect) get a 404 and the wait goes on, and so does a
    /// `/callback` without this flow's `state`: any local process (or a page
    /// probing loopback ports) can reach the listener, and a forged hit must
    /// not end a real sign-in. Each request gets [`REQUEST_READ_TIMEOUT`] in
    /// total, so a slow sender can't hold the listener either.
    pub fn wait_cancellable(
        &self,
        timeout: Duration,
        expected_state: &str,
        cancel: &AtomicBool,
    ) -> Result<Callback, String> {
        // Deadline-driven accept: poll the socket instead of blocking past
        // the timeout, so an abandoned login never hangs the caller.
        self.listener
            .set_nonblocking(true)
            .map_err(|err| err.to_string())?;
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err("sign-in cancelled".to_string());
            }
            let stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err("timed out waiting for browser sign-in".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(err) => return Err(err.to_string()),
            };
            stream
                .set_nonblocking(false)
                .map_err(|err| err.to_string())?;
            let request_deadline = deadline.min(Instant::now() + REQUEST_READ_TIMEOUT);
            let request = Self::parse_request(
                stream.try_clone().map_err(|e| e.to_string())?,
                request_deadline,
            );
            if request.path != "/callback" {
                let _ = Self::respond_not_found(stream);
                continue;
            }
            if request.state.as_deref() != Some(expected_state) {
                // Not this flow's redirect: refuse it and keep waiting.
                let _ = Self::respond(stream, false);
                continue;
            }
            let _ = Self::respond(stream, request.error.is_none() && request.code.is_some());
            return match (request.error, request.code, request.state) {
                (Some(error), _, _) => Err(format!("login was rejected: {error}")),
                (None, Some(code), Some(state)) => Ok(Callback { code, state }),
                _ => Err("login callback missing code/state".to_string()),
            };
        }
    }

    /// Read one HTTP request and pull `code`/`state`/`error` from the query.
    /// Reads until the end of the headers — a single read() may be partial —
    /// but never past `deadline` (for the whole request, not per read) or
    /// [`MAX_REQUEST_BYTES`].
    fn parse_request(mut stream: TcpStream, deadline: Instant) -> LoopbackRequest {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            if buffer.ends_with(b"\r\n\r\n") || buffer.len() >= MAX_REQUEST_BYTES {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
                break;
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        }
        let request = String::from_utf8_lossy(&buffer).to_string();
        let target = request.split_whitespace().nth(1).unwrap_or_default();
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let mut parsed = LoopbackRequest {
            path: path.to_string(),
            ..LoopbackRequest::default()
        };
        for (key, value) in parse_query(query) {
            match key.as_str() {
                "code" => parsed.code = Some(value),
                "state" => parsed.state = Some(value),
                "error" => parsed.error = Some(value),
                _ => {}
            }
        }
        parsed
    }

    fn respond_not_found(mut stream: TcpStream) -> std::io::Result<()> {
        stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    }

    fn respond(mut stream: TcpStream, ok: bool) -> std::io::Result<()> {
        let (accent, glyph, title, message) = if ok {
            (
                "#10b981",
                "\u{2713}",
                "Signed in to LibertAI",
                "You can close this tab and return to alforria.",
            )
        } else {
            (
                "#ef4444",
                "\u{00d7}",
                "Sign-in failed",
                "Something went wrong. Return to alforria and sign in again.",
            )
        };
        let body = format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>LibertAI</title>\
<style>html,body{{height:100%;margin:0}}body{{display:flex;align-items:center;justify-content:center;\
font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;\
background:#0b0b0f;color:#e5e7eb}}.card{{text-align:center;padding:2.5rem 3rem;max-width:24rem}}\
.badge{{width:56px;height:56px;border-radius:9999px;background:{accent};color:#fff;\
font-size:30px;line-height:56px;margin:0 auto 1.25rem}}h1{{font-size:1.25rem;font-weight:600;\
margin:0 0 .5rem}}p{{margin:0;color:#9ca3af;font-size:.95rem;line-height:1.4}}</style></head>\
<body><div class=\"card\"><div class=\"badge\">{glyph}</div><h1>{title}</h1><p>{message}</p></div></body></html>"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes())
    }
}

/// `+`-tolerant minimal query parser (browsers may send spaces as `+`).
/// Operates on bytes so a `%` followed by a multi-byte UTF-8 char can never
/// panic a str slice.
fn parse_query(query: &str) -> Vec<(String, String)> {
    fn decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'+' => {
                    decoded.push(b' ');
                    index += 1;
                }
                b'%' if index + 2 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                        .ok()
                        .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                    match hex {
                        Some(byte) => {
                            decoded.push(byte);
                            index += 3;
                        }
                        None => {
                            decoded.push(b'%');
                            index += 1;
                        }
                    }
                }
                byte => {
                    decoded.push(byte);
                    index += 1;
                }
            }
        }
        String::from_utf8_lossy(&decoded).to_string()
    }
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| pair.split_once('=').map(|(k, v)| (decode(k), decode(v))))
        .collect()
}

/// Parse the code out of text pasted at the manual prompt: a bare code, a
/// `code=…&state=…` fragment, or a full redirect URL.
pub fn parse_manual_code(input: &str) -> Option<(String, Option<String>)> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }
    let looks_structured = text.contains("://")
        || text.contains('?')
        || text.contains("code=")
        || text.contains("state=");
    if !looks_structured {
        return Some((text.to_string(), None));
    }
    let query = if text.contains("://") {
        text.split_once('?').map(|(_, query)| query)?.to_string()
    } else {
        text.rsplit('?')
            .next()
            .unwrap_or(text)
            .trim_start_matches(['?', '&'])
            .to_string()
    };
    let mut code = None;
    let mut state = None;
    for (key, value) in parse_query(&query) {
        match key.as_str() {
            "code" => code = Some(value),
            "state" => state = Some(value),
            _ => {}
        }
    }
    Some((code?, state))
}

// ---------------------------------------------------------------------------
// account API client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FullApiKey {
    pub id: String,
    pub name: String,
    pub full_key: String,
    #[serde(default)]
    pub expires_at: Option<String>,
}

fn account_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|err| err.to_string())
}

impl Endpoints {
    /// Exchange a one-time code (+ PKCE verifier) for the session token pair.
    pub fn exchange_code(&self, code: &str, verifier: &str) -> Result<TokenPair, String> {
        let url = format!("{}/auth/exchange", self.account);
        let response = account_client()?
            .post(&url)
            .json(&serde_json::json!({"code": code, "verifier": verifier}))
            .send()
            .map_err(|err| format!("POST {url}: {err}"))?;
        if !response.status().is_success() {
            return Err(format!("POST {url} → {}", response.status()));
        }
        response
            .json()
            .map_err(|err| format!("parsing /auth/exchange response: {err}"))
    }

    /// Mint (or rotate) this device's CLI API key, authenticating with the
    /// session access token.
    pub fn create_cli_api_key(&self, access_token: &str, host: &str) -> Result<FullApiKey, String> {
        let url = format!("{}/api-keys/cli", self.account);
        let response = account_client()?
            .post(&url)
            .bearer_auth(access_token)
            .json(&serde_json::json!({"host": host}))
            .send()
            .map_err(|err| format!("POST {url}: {err}"))?;
        if !response.status().is_success() {
            return Err(format!("POST {url} → {}", response.status()));
        }
        response
            .json()
            .map_err(|err| format!("parsing /api-keys/cli response: {err}"))
    }

    /// Best-effort revocation of a refresh token (logout). Surfaced errors
    /// include the HTTP status so a permanent failure isn't mistaken for
    /// success.
    pub fn revoke(&self, refresh_token: &str) -> Result<(), String> {
        let url = format!("{}/auth/logout", self.account);
        let response = account_client()?
            .post(&url)
            .json(&serde_json::json!({"refresh_token": refresh_token}))
            .send()
            .map_err(|err| format!("POST {url}: {err}"))?;
        if !response.status().is_success() {
            return Err(format!("POST {url} → {}", response.status()));
        }
        Ok(())
    }

    /// The leg after the browser redirect, shared by every login surface:
    /// exchange the code, mint this device's key, persist the session
    /// sidecar. The caller stores the returned key in auth.json.
    pub fn complete_login(&self, code: &str, verifier: &str) -> Result<FullApiKey, String> {
        let pair = self.exchange_code(code, verifier)?;
        // Per-device key: a stable id keeps this device's key name unique,
        // so logging in elsewhere mints a separate key instead of rotating
        // this one.
        let device_id = self
            .load_session()
            .map(|session| session.device_id)
            .unwrap_or_else(new_device_id);
        let host = format!("{}-{}", device_hostname(), device_id);
        let created = self.create_cli_api_key(&pair.access_token, &host)?;
        self.store_session(&StoredSession {
            refresh_token: pair.refresh_token,
            expires_at: created.expires_at.clone(),
            device_id,
        })?;
        Ok(created)
    }

    /// Logout's account side: revoke the stored refresh token (best-effort)
    /// and drop the sidecar. The auth.json key is the caller's to remove.
    pub fn logout(&self) {
        if let Some(session) = self.load_session() {
            let _ = self.revoke(&session.refresh_token);
        }
        self.clear_session();
    }
}

/// One-time-use rotation: the returned pair's refresh token REPLACES the one
/// used — persist it before anything else.
pub fn refresh(access_token_refresh: &str) -> Result<TokenPair, String> {
    let url = format!("{}/auth/refresh", account_base());
    let response = account_client()?
        .post(&url)
        .json(&serde_json::json!({"refresh_token": access_token_refresh}))
        .send()
        .map_err(|err| format!("POST {url}: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("POST {url} → {}", response.status()));
    }
    response
        .json()
        .map_err(|err| format!("parsing /auth/refresh response: {err}"))
}

/// `GET /payments/subscription` — plan tier, allowance windows, prepaid
/// balance. Requires a session access token (the `LTAI_` inference key
/// cannot authenticate account endpoints).
pub fn subscription(access_token: &str) -> Result<Subscription, String> {
    let url = format!("{}/payments/subscription", account_base());
    let response = account_client()?
        .get(&url)
        .bearer_auth(access_token)
        .send()
        .map_err(|err| format!("GET {url}: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("GET {url} → {}", response.status()));
    }
    response
        .json()
        .map_err(|err| format!("parsing /payments/subscription response: {err}"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub tier: String,
    #[serde(default)]
    pub has_subscription: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_5h_used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_5h_limit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_5h_resets_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_limit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_resets_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepaid_balance: Option<f64>,
}

/// Refresh the stored session and run a query with the rotated access token.
/// The rotated refresh token replaces the stored one BEFORE the query runs
/// (rotation is one-time-use; the old token is already invalid).
pub fn with_refreshed_session<T>(
    query: impl FnOnce(&str) -> Result<T, String>,
) -> Result<T, String> {
    let session = load_session()
        .ok_or_else(|| "not logged in — run `alforria auth login -p libertai`".to_string())?;
    let pair = refresh(&session.refresh_token)?;
    store_session(&StoredSession {
        refresh_token: pair.refresh_token.clone(),
        expires_at: session.expires_at,
        device_id: session.device_id,
    })
    .map_err(|err| {
        format!(
            "could not persist the refreshed session ({err}) — run `alforria auth login -p libertai` again"
        )
    })?;
    query(&pair.access_token)
}

// ---------------------------------------------------------------------------
// sidecar session store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub device_id: String,
}

fn session_path() -> PathBuf {
    GlobalPaths::from_env().data.join(SESSION_FILE)
}

pub fn load_session() -> Option<StoredSession> {
    Endpoints::from_env().load_session()
}

pub fn store_session(session: &StoredSession) -> Result<(), String> {
    Endpoints::from_env().store_session(session)
}

impl Endpoints {
    pub fn load_session(&self) -> Option<StoredSession> {
        let text = std::fs::read_to_string(&self.session_file).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn store_session(&self, session: &StoredSession) -> Result<(), String> {
        let text = serde_json::to_string(session).map_err(|err| err.to_string())?;
        let path = &self.session_file;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
                .and_then(|mut file| std::io::Write::write_all(&mut file, text.as_bytes()))
                .map_err(|err| format!("writing {}: {err}", path.display()))?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(path, &text)
                .map_err(|err| format!("writing {}: {err}", path.display()))?;
        }
        Ok(())
    }

    pub fn clear_session(&self) {
        let _ = std::fs::remove_file(&self.session_file);
    }
}

/// Random 8-hex-char id identifying this install (not security-sensitive);
/// keeps this device's CLI key name stable across logins.
pub fn new_device_id() -> String {
    let mut bytes = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Best-effort hostname for a recognizable key name in the console;
/// uniqueness comes from the device id, not this.
pub fn device_hostname() -> String {
    let raw = hostname_from_env()
        .or_else(hostname_from_file)
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "device".to_string());
    raw.split('.').next().unwrap_or("device").to_string()
}

fn hostname_from_env() -> Option<String> {
    if cfg!(windows) {
        std::env::var("COMPUTERNAME").ok()
    } else {
        std::env::var("HOSTNAME").ok()
    }
}

fn hostname_from_file() -> Option<String> {
    if cfg!(unix) {
        std::fs::read_to_string("/etc/hostname").ok()
    } else {
        None
    }
    .map(|host| host.trim().to_string())
}

// ---------------------------------------------------------------------------
// browser
// ---------------------------------------------------------------------------

pub fn open_browser(url: &str) {
    crate::browser::open_url(url);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_params_have_expected_shape() {
        let pkce = Pkce::generate();
        assert_eq!(pkce.verifier.len(), 43);
        assert!(pkce
            .verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_eq!(pkce.challenge.len(), 43);
        assert_ne!(pkce.verifier, pkce.challenge);
        assert_eq!(pkce.state.len(), 22);
        let regenerated = Pkce::generate();
        assert_ne!(pkce.verifier, regenerated.verifier);
    }

    #[test]
    fn challenge_is_base64url_of_sha256_verifier() {
        let pkce = Pkce::generate();
        let digest = Sha256::digest(pkce.verifier.as_bytes());
        assert_eq!(pkce.challenge, URL_SAFE_NO_PAD.encode(digest));
    }

    #[test]
    fn authorize_url_carries_custom_challenge_param() {
        let pkce = Pkce::generate();
        let url = authorize_url(&pkce, "Alforria", "http://127.0.0.1:41234/callback");
        assert!(url.starts_with("https://console.libertai.io/cli?"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A41234%2Fcallback"));
        assert!(url.contains(&format!("state={}", pkce.state)));
        assert!(url.contains(&format!("challenge={}", pkce.challenge)));
        assert!(url.contains("client=Alforria"));
    }

    #[test]
    fn callback_server_parses_the_redirect() {
        let server = CallbackServer::bind().unwrap();
        let port = server.port();
        assert!(port > 0);
        assert_eq!(
            server.redirect_uri(),
            format!("http://127.0.0.1:{port}/callback")
        );

        let handle =
            std::thread::spawn(move || server.wait(Duration::from_secs(5), "xyz").unwrap());
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(b"GET /callback?code=abc123&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.contains("Signed in to LibertAI"));
        assert!(response.contains("You can close this tab and return to alforria."));
        let callback = handle.join().unwrap();
        assert_eq!(callback.code, "abc123");
        assert_eq!(callback.state, "xyz");
    }

    #[test]
    fn callback_server_skips_other_paths_and_empty_connections() {
        let server = CallbackServer::bind().unwrap();
        let port = server.port();
        let handle =
            std::thread::spawn(move || server.wait(Duration::from_secs(10), "xyz").unwrap());

        // A pre-connect that closes without sending anything.
        drop(TcpStream::connect(("127.0.0.1", port)).unwrap());

        let mut favicon = TcpStream::connect(("127.0.0.1", port)).unwrap();
        favicon
            .write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        favicon.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 404"));

        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(b"GET /callback?code=abc&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap();
        let callback = handle.join().unwrap();
        assert_eq!(callback.code, "abc");
    }

    #[test]
    fn callback_server_stops_when_cancelled() {
        let server = CallbackServer::bind().unwrap();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let handle = std::thread::spawn(move || {
            server.wait_cancellable(Duration::from_secs(30), "s", &flag)
        });
        cancel.store(true, Ordering::SeqCst);
        assert_eq!(handle.join().unwrap().unwrap_err(), "sign-in cancelled");
    }

    #[test]
    fn callback_server_ignores_forged_redirects() {
        let server = CallbackServer::bind().unwrap();
        let port = server.port();
        let handle =
            std::thread::spawn(move || server.wait(Duration::from_secs(10), "real").unwrap());
        for forged in [
            "GET /callback?code=evil&state=forged HTTP/1.1\r\n\r\n",
            "GET /callback?error=access_denied HTTP/1.1\r\n\r\n",
            "GET /callback HTTP/1.1\r\n\r\n",
        ] {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream.write_all(forged.as_bytes()).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            assert!(response.contains("Sign-in failed"), "{forged}");
        }
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(b"GET /callback?code=good&state=real HTTP/1.1\r\n\r\n")
            .unwrap();
        let callback = handle.join().unwrap();
        assert_eq!(callback.code, "good");
    }

    #[test]
    fn callback_server_bounds_a_slow_sender() {
        let server = CallbackServer::bind().unwrap();
        let port = server.port();
        let handle =
            std::thread::spawn(move || server.wait(Duration::from_secs(30), "real").unwrap());
        // A sender that trickles a byte every second never finishes its
        // headers; it gets cut off after REQUEST_READ_TIMEOUT in total.
        let mut slow = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let started = Instant::now();
        let trickle = std::thread::spawn(move || {
            for _ in 0..20 {
                if slow.write_all(b"G").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        std::thread::sleep(Duration::from_millis(200));
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(b"GET /callback?code=good&state=real HTTP/1.1\r\n\r\n")
            .unwrap();
        let callback = handle.join().unwrap();
        assert_eq!(callback.code, "good");
        assert!(started.elapsed() < REQUEST_READ_TIMEOUT + Duration::from_secs(3));
        let _ = trickle.join();
    }

    #[test]
    fn endpoints_authorize_url_uses_their_console() {
        let endpoints = Endpoints {
            account: "http://127.0.0.1:1".to_string(),
            console: "http://127.0.0.1:2".to_string(),
            session_file: PathBuf::from("/nonexistent/libertai-auth.json"),
        };
        let url = endpoints.authorize_url(&Pkce::generate(), "Alforria", "http://x/callback");
        assert!(url.starts_with("http://127.0.0.1:2/cli?"));
    }

    #[test]
    fn parse_manual_code_accepts_all_shapes() {
        let (code, state) = parse_manual_code("abc123").unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state, None);

        let (code, state) = parse_manual_code("  code=abc123&state=xyz  ").unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("xyz"));

        let (code, state) =
            parse_manual_code("http://127.0.0.1:54321/callback?code=abc123&state=xyz").unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("xyz"));
    }

    #[test]
    fn parse_manual_code_rejects_codeless_input() {
        assert!(parse_manual_code("   ").is_none());
        assert!(parse_manual_code("http://127.0.0.1/callback?state=xyz").is_none());
    }
}
