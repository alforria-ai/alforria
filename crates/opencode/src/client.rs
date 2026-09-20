//! Thin HTTP+SSE client for the `run`/`attach` paths — the Rust
//! counterpart of `createOpencodeClient` (`@opencode-ai/sdk/v2`) against the
//! v1 HTTP API. GET/HEAD carry the directory as a `?directory=` query param,
//! other methods as the `x-opencode-directory` header (sdk v2 `rewrite`).

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;

/// A failed HTTP exchange: transport error or a non-2xx response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientError {
    pub status: Option<u16>,
    pub body: Option<String>,
    pub message: String,
}

impl ClientError {
    fn transport(message: impl Into<String>) -> Self {
        ClientError {
            status: None,
            body: None,
            message: message.into(),
        }
    }

    fn from_status(status: reqwest::StatusCode, body: String) -> Self {
        ClientError {
            status: Some(status.as_u16()),
            body: Some(body),
            message: format!(
                "{} {}",
                status.as_str(),
                status.canonical_reason().unwrap_or("Error")
            ),
        }
    }
}

/// `encodeURIComponent` — every byte outside `A-Za-z0-9-_.!~*'()` is
/// percent-encoded.
fn encode_uri_component(input: &str) -> String {
    const SAFE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        if SAFE.contains(byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `ServerAuth.header` (server/auth.ts:22-27): password from the flag or
/// `OPENCODE_SERVER_PASSWORD`, username from the flag or
/// `OPENCODE_SERVER_USERNAME` or `opencode`.
pub fn auth_header(password: Option<&str>, username: Option<&str>) -> Option<String> {
    let password = password
        .map(str::to_string)
        .or_else(|| std::env::var("OPENCODE_SERVER_PASSWORD").ok())?;
    let username = username
        .map(str::to_string)
        .or_else(|| std::env::var("OPENCODE_SERVER_USERNAME").ok())
        .unwrap_or_else(|| "opencode".to_string());
    Some(format!(
        "Basic {}",
        b64::encode(format!("{username}:{password}"))
    ))
}

/// Minimal base64 (RFC 4648) — no new dependencies for one codec.
mod b64 {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: impl AsRef<[u8]>) -> String {
        let bytes = input.as_ref();
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let buf = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = ((buf[0] as u32) << 16) | ((buf[1] as u32) << 8) | buf[2] as u32;
            out.push(TABLE[(n >> 18) as usize & 0x3f] as char);
            out.push(TABLE[(n >> 12) as usize & 0x3f] as char);
            out.push(if chunk.len() > 1 {
                TABLE[(n >> 6) as usize & 0x3f] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[n as usize & 0x3f] as char
            } else {
                '='
            });
        }
        out
    }
}

/// Public re-export of the base64 codec (used by the run `--file` flow).
pub fn base64_encode(input: impl AsRef<[u8]>) -> String {
    b64::encode(input)
}

/// Parse one SSE block (lines between blank-line separators) into its data
/// payload: `data:` lines joined with newlines (`data: x` strips one space).
pub fn sse_block_data(block: &str) -> Option<String> {
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
        return None;
    }
    Some(data.join("\n"))
}

#[derive(Debug, Clone)]
pub struct OpencodeClient {
    http: reqwest::Client,
    base_url: String,
    directory: Option<String>,
    authorization: Option<String>,
}

impl OpencodeClient {
    pub fn new(
        base_url: impl Into<String>,
        directory: Option<String>,
        password: Option<&str>,
        username: Option<&str>,
    ) -> Self {
        OpencodeClient {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            directory,
            authorization: auth_header(password, username),
        }
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response, ClientError> {
        let mut url = format!("{}{path}", self.base_url);
        if method == reqwest::Method::GET {
            if let Some(directory) = &self.directory {
                let separator = if url.contains('?') { '&' } else { '?' };
                url = format!(
                    "{url}{separator}directory={}",
                    encode_uri_component(directory)
                );
            }
        }
        let request = self
            .http
            .request(method, &url)
            .header("content-type", "application/json");
        let request = if let Some(auth) = &self.authorization {
            request.header("authorization", auth)
        } else {
            request
        };
        let request = if let Some(directory) = &self.directory {
            if url.contains("?directory=") || url.contains("&directory=") {
                request
            } else {
                request.header("x-opencode-directory", encode_uri_component(directory))
            }
        } else {
            request
        };
        let request = match body {
            Some(value) => request.json(&value),
            None => request,
        };
        request
            .send()
            .await
            .map_err(|err| ClientError::transport(err.to_string()))
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response, ClientError> {
        self.send(reqwest::Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<reqwest::Response, ClientError> {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    async fn response_value(response: reqwest::Response) -> Result<Value, ClientError> {
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| ClientError::transport(err.to_string()))?;
        if !status.is_success() {
            return Err(ClientError::from_status(status, text));
        }
        serde_json::from_str(&text)
            .map_err(|err| ClientError::transport(format!("invalid JSON response: {err}")))
    }

    /// `GET /session` — the session list (may be empty).
    pub async fn session_list(&self) -> Result<Vec<Value>, ClientError> {
        let response = self.get("/session").await?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| ClientError::transport(err.to_string()))?;
        if !status.is_success() {
            let body = text;
            return Err(ClientError::from_status(status, body));
        }
        serde_json::from_str(&text)
            .map_err(|err| ClientError::transport(format!("invalid JSON response: {err}")))
    }

    /// `GET /session/{sessionID}` — `None` on any failure
    /// (`sdk.session.get(...).catch(() => undefined)`).
    pub async fn session_get(&self, session_id: &str) -> Result<Option<Value>, ClientError> {
        let response = self
            .get(&format!("/session/{session_id}"))
            .await
            .map_err(|_| ClientError::transport("session get failed"))?;
        let status = response.status();
        if !status.is_success() {
            return Ok(None);
        }
        let text = response
            .text()
            .await
            .map_err(|_| ClientError::transport("session get failed"))?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|err| ClientError::transport(format!("invalid JSON response: {err}")))
    }

    /// `POST /session` — create a session.
    pub async fn session_create(&self, body: Value) -> Result<Value, ClientError> {
        let response = self.post("/session", body).await?;
        Self::response_value(response).await
    }

    /// `POST /session/{sessionID}/fork`.
    pub async fn session_fork(&self, session_id: &str) -> Result<Value, ClientError> {
        let response = self
            .post(
                &format!("/session/{session_id}/fork"),
                serde_json::json!({}),
            )
            .await?;
        Self::response_value(response).await
    }

    /// `POST /session/{sessionID}/message` — send the prompt.
    pub async fn session_prompt(
        &self,
        session_id: &str,
        body: Value,
    ) -> Result<Value, ClientError> {
        let response = self
            .post(&format!("/session/{session_id}/message"), body)
            .await?;
        Self::response_value(response).await
    }

    /// `POST /session/{sessionID}/share` (run.ts:535-548 share flow).
    pub async fn session_share(&self, session_id: &str) -> Result<Value, ClientError> {
        let response = self
            .post(
                &format!("/session/{session_id}/share"),
                serde_json::json!({}),
            )
            .await?;
        Self::response_value(response).await
    }

    /// `POST /session/{sessionID}/command` (run.ts:845-861).
    pub async fn session_command(
        &self,
        session_id: &str,
        body: Value,
    ) -> Result<Value, ClientError> {
        let response = self
            .post(&format!("/session/{session_id}/command"), body)
            .await?;
        Self::response_value(response).await
    }

    /// `GET /config` — `sdk.config.get()`.
    pub async fn config_get(&self) -> Result<Value, ClientError> {
        let response = self.get("/config").await?;
        Self::response_value(response).await
    }

    /// `GET /path` — `sdk.path.get()`.
    pub async fn path_get(&self) -> Result<Value, ClientError> {
        let response = self.get("/path").await?;
        Self::response_value(response).await
    }

    /// `GET /agent` — `sdk.app.agents()`.
    pub async fn agent_list(&self) -> Result<Vec<Value>, ClientError> {
        let response = self.get("/agent").await?;
        let value = Self::response_value(response).await?;
        serde_json::from_value(value)
            .map_err(|err| ClientError::transport(format!("invalid JSON response: {err}")))
    }

    /// `POST /permission/{requestID}/reply`.
    pub async fn permission_reply(
        &self,
        request_id: &str,
        reply: &str,
    ) -> Result<Value, ClientError> {
        let response = self
            .post(
                &format!("/permission/{request_id}/reply"),
                serde_json::json!({ "reply": reply }),
            )
            .await?;
        Self::response_value(response).await
    }

    /// `GET /event` — subscribe to the SSE stream and pump parsed event
    /// frames onto the channel. The pump task ends when the stream closes
    /// (dropping the sender) — `None` then flows to the consumer.
    pub async fn subscribe_events(&self, out: mpsc::Sender<Value>) -> Result<(), ClientError> {
        let response = self.get("/event").await.map_err(|err| {
            ClientError::transport(format!("event subscribe failed: {}", err.message))
        })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ClientError::from_status(status, body));
        }
        let mut stream = response.bytes_stream();
        tokio::spawn(async move {
            let mut buffer = String::new();
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(_) => break,
                };
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(index) = buffer.find("\n\n") {
                    let block = buffer[..index].to_string();
                    buffer.drain(..index + 2);
                    if let Some(data) = sse_block_data(&block) {
                        if let Ok(value) = serde_json::from_str::<Value>(&data) {
                            let _ = out.send(value).await;
                        }
                    }
                }
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_only_unsafe_bytes() {
        assert_eq!(encode_uri_component("/a b"), "%2Fa%20b");
        assert_eq!(encode_uri_component("a+b-c_d"), "a%2Bb-c_d");
        assert_eq!(encode_uri_component("x.y~*'(z)"), "x.y~*'(z)");
    }

    #[test]
    fn base64_encodes_basic_credentials() {
        assert_eq!(b64::encode("opencode:pw"), "b3BlbmNvZGU6cHc=");
        assert_eq!(b64::encode("a"), "YQ==");
        assert_eq!(b64::encode("ab"), "YWI=");
    }

    #[test]
    fn sse_block_extracts_single_and_multi_line_data() {
        assert_eq!(
            sse_block_data("data: {\"a\":1}"),
            Some("{\"a\":1}".to_string())
        );
        assert_eq!(sse_block_data("data: a\ndata: b"), Some("a\nb".to_string()));
        assert_eq!(sse_block_data(": heartbeat"), None);
        assert_eq!(sse_block_data("data:"), Some(String::new()));
        assert_eq!(sse_block_data("id: x\ndata: y"), Some("y".to_string()));
        assert_eq!(
            sse_block_data("data: a\ndata: b\n"),
            Some("a\nb".to_string())
        );
    }

    #[test]
    fn auth_header_uses_flag_env_and_defaults() {
        std::env::remove_var("OPENCODE_SERVER_PASSWORD");
        std::env::remove_var("OPENCODE_SERVER_USERNAME");
        assert_eq!(auth_header(None, None), None);
        assert_eq!(
            auth_header(Some("pw"), Some("user")),
            Some("Basic dXNlcjpwdw==".to_string())
        );
        std::env::set_var("OPENCODE_SERVER_PASSWORD", "envpw");
        std::env::set_var("OPENCODE_SERVER_USERNAME", "envuser");
        assert_eq!(
            auth_header(Some("pw"), Some("user")),
            Some("Basic dXNlcjpwdw==".to_string())
        );
        assert_eq!(
            auth_header(None, None),
            Some("Basic ZW52dXNlcjplbnZwdw==".to_string())
        );
        std::env::remove_var("OPENCODE_SERVER_PASSWORD");
        std::env::remove_var("OPENCODE_SERVER_USERNAME");
    }

    #[test]
    fn base_url_trims_trailing_slash() {
        let client = OpencodeClient::new("http://127.0.0.1:4096/", None, None, None);
        assert_eq!(client.base_url, "http://127.0.0.1:4096");
    }
}
