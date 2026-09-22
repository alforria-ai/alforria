//! The alforria-server HTTP client ("sdk" in the TS service) — the v2
//! SDK calls (`packages/sdk/js/src/v2/gen/sdk.gen.ts`) mapped onto the
//! un-prefixed route group. `directory` travels as a `?directory=` query
//! on GET and the `x-alforria-directory` header always (same convention
//! as the e2e harness client).

use serde_json::{json, Value};

fn encode_uri_component(input: &str) -> String {
    const SAFE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if SAFE.contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A request made against one location directory.
#[derive(Clone)]
pub struct ServerClient {
    http: reqwest::Client,
    base: String,
}

impl ServerClient {
    pub fn new(port: u16) -> ServerClient {
        ServerClient {
            http: reqwest::Client::new(),
            base: format!("http://127.0.0.1:{port}"),
        }
    }

    async fn request(
        &self,
        directory: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response, String> {
        let url = format!("{}{path}", self.base);
        let request = self
            .http
            .request(method.clone(), &url)
            .header("content-type", "application/json")
            .header("x-alforria-directory", encode_uri_component(directory));
        let request = if method == reqwest::Method::GET {
            request.query(&[("directory", directory.to_string())])
        } else {
            request
        };
        let request = match body {
            Some(value) => request.json(&value),
            None => request,
        };
        request.send().await.map_err(|err| err.to_string())
    }

    async fn json(
        &self,
        directory: &str,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let response = self.request(directory, method, path, body).await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("{path} failed: {} {text}", status.as_u16()));
        }
        serde_json::from_str(&text).map_err(|err| format!("{path} body: {err}"))
    }

    async fn list(
        &self,
        directory: &str,
        path: &str,
        limit: Option<u32>,
    ) -> Result<Vec<Value>, String> {
        let path = match limit {
            Some(limit) => format!("{path}?limit={limit}"),
            None => path.to_string(),
        };
        let value = self
            .json(directory, reqwest::Method::GET, &path, None)
            .await?;
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    /// `session.create` — `POST /session`.
    pub async fn session_create(&self, directory: &str, body: Value) -> Result<Value, String> {
        self.json(directory, reqwest::Method::POST, "/session", Some(body))
            .await
    }

    /// `session.get` — `GET /session/{sessionID}`.
    pub async fn session_get(&self, directory: &str, session_id: &str) -> Result<Value, String> {
        self.json(
            directory,
            reqwest::Method::GET,
            &format!("/session/{session_id}"),
            None,
        )
        .await
    }

    /// `session.messages` — `GET /session/{sessionID}/message`.
    pub async fn session_messages(
        &self,
        directory: &str,
        session_id: &str,
        limit: Option<u32>,
    ) -> Result<Vec<Value>, String> {
        self.list(directory, &format!("/session/{session_id}/message"), limit)
            .await
    }

    /// `session.list` — `GET /session?roots=true`. Without a directory
    /// filter the query param is omitted entirely.
    pub async fn session_list(&self, directory: Option<&str>) -> Result<Vec<Value>, String> {
        let value = match directory {
            Some(directory) => {
                self.json(directory, reqwest::Method::GET, "/session?roots=true", None)
                    .await?
            }
            None => {
                let url = format!("{}/session?roots=true", self.base);
                let response = self
                    .http
                    .request(reqwest::Method::GET, &url)
                    .header("content-type", "application/json")
                    .query(&[("roots", "true")])
                    .send()
                    .await
                    .map_err(|err| err.to_string())?;
                let status = response.status();
                let text = response.text().await.map_err(|err| err.to_string())?;
                if !status.is_success() {
                    return Err(format!("session list failed: {} {text}", status.as_u16()));
                }
                serde_json::from_str(&text).map_err(|err| format!("session list body: {err}"))?
            }
        };
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    /// `session.abort` — `POST /session/{sessionID}/abort`.
    pub async fn session_abort(&self, directory: &str, session_id: &str) -> Result<Value, String> {
        self.json(
            directory,
            reqwest::Method::POST,
            &format!("/session/{session_id}/abort"),
            Some(json!({})),
        )
        .await
    }

    /// `session.fork` — `POST /session/{sessionID}/fork`.
    pub async fn session_fork(&self, directory: &str, session_id: &str) -> Result<Value, String> {
        self.json(
            directory,
            reqwest::Method::POST,
            &format!("/session/{session_id}/fork"),
            Some(json!({})),
        )
        .await
    }

    /// `session.prompt` — `POST /session/{sessionID}/message`.
    pub async fn session_prompt(
        &self,
        directory: &str,
        session_id: &str,
        body: Value,
    ) -> Result<Value, String> {
        self.json(
            directory,
            reqwest::Method::POST,
            &format!("/session/{session_id}/message"),
            Some(body),
        )
        .await
    }

    /// `session.command` — `POST /session/{sessionID}/command`.
    pub async fn session_command(
        &self,
        directory: &str,
        session_id: &str,
        body: Value,
    ) -> Result<Value, String> {
        self.json(
            directory,
            reqwest::Method::POST,
            &format!("/session/{session_id}/command"),
            Some(body),
        )
        .await
    }

    /// `session.summarize` — `POST /session/{sessionID}/summarize`.
    pub async fn session_summarize(
        &self,
        directory: &str,
        session_id: &str,
        body: Value,
    ) -> Result<Value, String> {
        self.json(
            directory,
            reqwest::Method::POST,
            &format!("/session/{session_id}/summarize"),
            Some(body),
        )
        .await
    }

    /// `session.message` — `GET /session/{sessionID}/message/{messageID}`.
    pub async fn session_message(
        &self,
        directory: &str,
        session_id: &str,
        message_id: &str,
    ) -> Result<Value, String> {
        self.json(
            directory,
            reqwest::Method::GET,
            &format!("/session/{session_id}/message/{message_id}"),
            None,
        )
        .await
    }

    /// `config.providers` — `GET /config/providers`.
    pub async fn config_providers(&self, directory: &str) -> Result<Value, String> {
        self.json(directory, reqwest::Method::GET, "/config/providers", None)
            .await
    }

    /// `config.get` — `GET /config`.
    pub async fn config_get(&self, directory: &str) -> Result<Value, String> {
        self.json(directory, reqwest::Method::GET, "/config", None)
            .await
    }

    /// `app.agents` — `GET /agent`.
    pub async fn app_agents(&self, directory: &str) -> Result<Vec<Value>, String> {
        self.list(directory, "/agent", None).await
    }

    /// `command.list` — `GET /command`.
    pub async fn command_list(&self, directory: &str) -> Result<Vec<Value>, String> {
        self.list(directory, "/command", None).await
    }

    /// `app.skills` — `GET /skill`.
    pub async fn app_skills(&self, directory: &str) -> Result<Vec<Value>, String> {
        self.list(directory, "/skill", None).await
    }

    /// `mcp.add` — `POST /mcp`.
    pub async fn mcp_add(&self, directory: &str, body: Value) -> Result<Value, String> {
        self.json(directory, reqwest::Method::POST, "/mcp", Some(body))
            .await
    }

    /// `permission.reply` — `POST /permission/{requestID}/reply`.
    pub async fn permission_reply(
        &self,
        directory: &str,
        request_id: &str,
        reply: &str,
    ) -> Result<Value, String> {
        self.json(
            directory,
            reqwest::Method::POST,
            &format!("/permission/{request_id}/reply"),
            Some(json!({ "reply": reply })),
        )
        .await
    }

    /// The global SSE stream URL — `GET /global/event`.
    pub fn global_event_url(&self) -> String {
        format!("{}/global/event", self.base)
    }
}
