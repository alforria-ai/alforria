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
use std::time::Duration;

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

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

/// The custom PKCE pair the console authorize page expects. The challenge is
/// base64url(SHA-256(verifier)) — not standard S256, and there is no
/// `code_challenge_method`; the verifier itself travels in the exchange body.
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
    format!(
        "{}/cli?redirect_uri={}&state={}&challenge={}&client={}",
        console_base(),
        urlencode(redirect_uri),
        urlencode(&pkce.state),
        urlencode(&pkce.challenge),
        urlencode(client),
    )
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

    /// Serve one GET to `/callback`, reply with a "you can close this tab"
    /// page, and return its `code` + `state` query params.
    pub fn wait(&self, timeout: Duration) -> Result<Callback, String> {
        self.listener
            .set_nonblocking(false)
            .map_err(|err| err.to_string())?;
        // Deadline-driven accept: poll the socket instead of blocking past
        // the timeout, so an abandoned login never hangs the caller.
        self.listener
            .set_nonblocking(true)
            .map_err(|err| err.to_string())?;
        let deadline = std::time::Instant::now() + timeout;
        let stream = loop {
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Err("timed out waiting for browser sign-in".to_string());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(err) => return Err(err.to_string()),
            }
        };
        stream
            .set_nonblocking(false)
            .map_err(|err| err.to_string())?;
        let (code, state, error) =
            Self::parse_request(stream.try_clone().map_err(|e| e.to_string())?);
        let _ = Self::respond(stream, error.is_none() && code.is_some());
        match error {
            Some(error) => Err(format!("login was rejected: {error}")),
            None => match (code, state) {
                (Some(code), Some(state)) => Ok(Callback { code, state }),
                _ => Err("login callback missing code/state".to_string()),
            },
        }
    }

    /// Read one HTTP request and pull `code`/`state`/`error` from the query.
    fn parse_request(mut stream: TcpStream) -> (Option<String>, Option<String>, Option<String>) {
        let mut buffer = [0u8; 4096];
        let read = stream.read(&mut buffer).unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();
        let query = request
            .split_whitespace()
            .nth(1)
            .and_then(|path| path.split_once('?'))
            .map(|(_, query)| query.to_string())
            .unwrap_or_default();
        let mut code = None;
        let mut state = None;
        let mut error = None;
        for (key, value) in parse_query(&query) {
            match key.as_str() {
                "code" => code = Some(value),
                "state" => state = Some(value),
                "error" => error = Some(value),
                _ => {}
            }
        }
        (code, state, error)
    }

    fn respond(mut stream: TcpStream, ok: bool) -> std::io::Result<()> {
        let (accent, glyph, title, message) = if ok {
            (
                "#10b981",
                "\u{2713}",
                "Signed in to LibertAI",
                "You can now close this page and return to your terminal.",
            )
        } else {
            (
                "#ef4444",
                "\u{00d7}",
                "Sign-in failed",
                "Something went wrong. Return to your terminal and log in again.",
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
fn parse_query(query: &str) -> Vec<(String, String)> {
    fn decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'+' => {
                    decoded.push(b' ');
                    index += 1;
                }
                b'%' if index + 2 < bytes.len() => {
                    let hex = &value[index + 1..index + 3];
                    if let Ok(byte) = u8::from_str_radix(hex, 16) {
                        decoded.push(byte);
                        index += 3;
                    } else {
                        decoded.push(b'%');
                        index += 1;
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

/// Exchange a one-time code (+ PKCE verifier) for the session token pair.
pub fn exchange_code(code: &str, verifier: &str) -> Result<TokenPair, String> {
    let url = format!("{}/auth/exchange", account_base());
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
pub fn create_cli_api_key(access_token: &str, host: &str) -> Result<FullApiKey, String> {
    let url = format!("{}/api-keys/cli", account_base());
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

/// Best-effort revocation of a refresh token (logout).
pub fn revoke(refresh_token: &str) -> Result<(), String> {
    let url = format!("{}/auth/logout", account_base());
    account_client()?
        .post(&url)
        .json(&serde_json::json!({"refresh_token": refresh_token}))
        .send()
        .map_err(|err| format!("POST {url}: {err}"))?;
    Ok(())
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

fn session_path() -> std::path::PathBuf {
    GlobalPaths::from_env().data.join(SESSION_FILE)
}

pub fn load_session() -> Option<StoredSession> {
    let text = std::fs::read_to_string(session_path()).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn store_session(session: &StoredSession) {
    let Ok(text) = serde_json::to_string(session) else {
        return;
    };
    let path = session_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, text.as_bytes()));
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::write(&path, &text);
    }
}

pub fn clear_session() {
    let _ = std::fs::remove_file(session_path());
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
    let command = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        return;
    } else {
        "xdg-open"
    };
    if cfg!(target_os = "windows") {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        return;
    }
    let _ = std::process::Command::new(command)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
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

        let handle = std::thread::spawn(move || server.wait(Duration::from_secs(5)).unwrap());
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(b"GET /callback?code=abc123&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.contains("Signed in to LibertAI"));
        let callback = handle.join().unwrap();
        assert_eq!(callback.code, "abc123");
        assert_eq!(callback.state, "xyz");
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
