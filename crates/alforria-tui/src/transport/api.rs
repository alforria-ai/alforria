//! Server API seam — `transport::api` (M8.1).
//!
//! The TS TUI reaches the server exclusively through the generated SDK
//! client (`sdk.client.*`, constructed in `context/sdk.tsx:23-31`). This
//! module ports that client as an async-trait seam: [`ServerApi`] carries
//! the request surface the TUI actually calls (enumerated in
//! `sync.tsx:458-468`, `:520-535`, `prompt/index.tsx:1000-1119`);
//! [`HttpServerApi`] is the reqwest production impl.
//!
//! Wire rules, from the SDK request rewriter (`sdk/js/src/v2/client.ts:18-48`):
//!
//! * per-request locations (the `{ directory, workspace }` members of every
//!   generated client call) ride as query params on every method;
//! * the config-level location rides as query params on GET/HEAD and as
//!   `x-opencode-directory` / `x-opencode-workspace` headers on other
//!   methods (the rewriter only strips them from GET/HEAD);
//! * values are percent-encoded like `encodeURIComponent` (the server
//!   decodes best-effort).
//!
//! Response typing: responses covered by `alforria-schema` DTOs are typed
//! (Session, Message, Part, Todo, PermissionReply, QuestionAnswer,
//! SessionStatus, SnapshotFileDiff). Responses without an M1 DTO (openapi
//! `Config`, `Provider`, `Agent`, `Command`, `LSPStatus`, `MCPStatus`,
//! `FormatterStatus`, `VcsInfo`, `ExperimentalCapabilities`, `ConsoleState`,
//! `McpResource`) pass through as `serde_json::Value` — the TS store keeps
//! them dynamically typed the same way. Extending the schema crate with
//! those DTOs is a review decision (spec §10 S1/S7).
//!
//! Request bodies: `parts` arrays are `serde_json::Value` — the part
//! *input* schemas (`TextPartInput`, …) have no M1 counterpart yet
//! (TODO(M8.6)).

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use alforria_schema::file_diff::SnapshotFileDiff;
use alforria_schema::permission_v1::{PermissionV1Reply, PermissionV1Request};
use alforria_schema::question_v1::{QuestionV1Answer, QuestionV1Request};
use alforria_schema::session_status::SessionStatusInfo;
use alforria_schema::session_todo::TodoInfo;
use alforria_schema::session_v1::{V1Message, V1Part, V1SessionInfo};

/// `encodeURIComponent` unreserved set: everything else is percent-encoded.
pub(crate) fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => out.push(*byte as char),
            b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

/// Base URL + config-level directory + default headers — the `run()` input
/// (M8.3) shared by [`HttpServerApi`] and the SSE event source.
#[derive(Debug, Clone)]
pub struct HttpClientConfig {
    pub base_url: String,
    pub directory: Option<String>,
    pub headers: Vec<(String, String)>,
}

/// The per-request `{ directory, workspace }` members of every generated
/// SDK client call (`{ workspace }` at every TS call site, `{ directory }`
/// only in the move flow — `prompt/index.tsx:1000-1015`).
#[derive(Debug, Clone, Default)]
pub struct Location {
    pub directory: Option<String>,
    pub workspace: Option<String>,
}

/// `{ info, parts }` — the v1 message-with-parts wrapper (openapi
/// `session.messages` items and the `session.prompt`/`.command`/`.shell`
/// responses).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageWithParts {
    pub info: V1Message,
    pub parts: Vec<V1Part>,
}

/// `session.list` query (`sync.tsx:160-174`): `start` is `now - 30d`;
/// `scope: "project"` unless the directory filter resolves a worktree path.
#[derive(Debug, Clone, Default)]
pub struct SessionListQuery {
    pub start: Option<i64>,
    pub scope: Option<String>,
    pub path: Option<String>,
}

/// `session.create` body model: `{ id, providerID, variant? }` (openapi
/// `SessionCreate.model`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreateModel {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// `session.create` body (`prompt/index.tsx:1000-1011`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<SessionCreateModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "parentID", default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

/// `{ providerID, modelID }` — openapi `session.prompt`/`.shell` body model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModel {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

/// `session.prompt` body (`prompt/index.tsx:1073-1089`) —
/// `{...selectedModel, model: selectedModel, …}` carries the model both
/// top-level and nested.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPrompt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(
        rename = "providerID",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub provider_id: Option<String>,
    #[serde(rename = "modelID", default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ProviderModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// Part *input* schemas have no M1 DTO yet (TODO(M8.6)).
    pub parts: Vec<Value>,
}

/// `session.command` body (`prompt/index.tsx:1082-1095`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCommand {
    pub command: String,
    pub arguments: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<Value>,
}

/// `session.shell` body (`prompt/index.tsx:1061-1070`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionShell {
    pub command: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ProviderModel>,
}

/// `experimental.controlPlane.moveSession` body (`prompt/move.tsx:131-137`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveSession {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub destination: MoveSessionDestination,
    #[serde(
        rename = "moveChanges",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub move_changes: Option<bool>,
}

/// openapi `MoveSessionDestination`: `{ directory }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveSessionDestination {
    pub directory: String,
}

/// The server API seam — one async trait method per generated SDK client
/// call the TUI makes. Tests target `FakeApi` scriptable doubles (the
/// sync reducer drives these).
#[async_trait]
pub trait ServerApi: Send + Sync {
    async fn path_get(&self, loc: &Location) -> Result<Value>;
    async fn project_current(&self, loc: &Location) -> Result<Value>;
    async fn project_directories(&self, loc: &Location, project_id: &str) -> Result<Value>;
    async fn experimental_workspace_list(&self, loc: &Location) -> Result<Value>;
    async fn experimental_workspace_status(&self, loc: &Location) -> Result<Value>;
    async fn config_providers(&self, loc: &Location) -> Result<Value>;
    async fn config_get(&self, loc: &Location) -> Result<Value>;
    async fn provider_list(&self, loc: &Location) -> Result<Value>;
    async fn provider_auth(&self, loc: &Location) -> Result<Value>;
    async fn app_agents(&self, loc: &Location) -> Result<Value>;
    async fn command_list(&self, loc: &Location) -> Result<Value>;
    async fn lsp_status(&self, loc: &Location) -> Result<Value>;
    async fn mcp_status(&self, loc: &Location) -> Result<Value>;
    async fn mcp_connect(&self, loc: &Location, name: &str) -> Result<bool>;
    async fn mcp_disconnect(&self, loc: &Location, name: &str) -> Result<bool>;
    async fn formatter_status(&self, loc: &Location) -> Result<Value>;
    async fn vcs_get(&self, loc: &Location) -> Result<Value>;
    async fn experimental_capabilities(&self, loc: &Location) -> Result<Value>;
    async fn experimental_console(&self, loc: &Location) -> Result<Value>;
    async fn experimental_resource_list(&self, loc: &Location) -> Result<Value>;
    async fn experimental_session_background(
        &self,
        loc: &Location,
        session_id: &str,
    ) -> Result<bool>;
    async fn sync_start(&self, loc: &Location) -> Result<bool>;
    async fn global_upgrade(&self, loc: &Location, target: &str) -> Result<Value>;
    async fn experimental_move_session(&self, loc: &Location, req: MoveSession) -> Result<()>;

    async fn session_list(
        &self,
        loc: &Location,
        query: SessionListQuery,
    ) -> Result<Vec<V1SessionInfo>>;
    async fn session_get(&self, loc: &Location, session_id: &str) -> Result<V1SessionInfo>;
    async fn session_messages(
        &self,
        loc: &Location,
        session_id: &str,
        limit: Option<u64>,
    ) -> Result<Vec<MessageWithParts>>;
    async fn session_todo(&self, loc: &Location, session_id: &str) -> Result<Vec<TodoInfo>>;
    async fn session_diff(
        &self,
        loc: &Location,
        session_id: &str,
        message_id: Option<&str>,
    ) -> Result<Vec<SnapshotFileDiff>>;
    async fn session_create(&self, loc: &Location, req: SessionCreate) -> Result<V1SessionInfo>;
    async fn session_prompt(
        &self,
        loc: &Location,
        session_id: &str,
        req: SessionPrompt,
    ) -> Result<MessageWithParts>;
    async fn session_command(
        &self,
        loc: &Location,
        session_id: &str,
        req: SessionCommand,
    ) -> Result<MessageWithParts>;
    async fn session_shell(
        &self,
        loc: &Location,
        session_id: &str,
        req: SessionShell,
    ) -> Result<MessageWithParts>;
    async fn session_abort(&self, loc: &Location, session_id: &str) -> Result<bool>;
    async fn session_revert(
        &self,
        loc: &Location,
        session_id: &str,
        message_id: &str,
        part_id: Option<&str>,
    ) -> Result<V1SessionInfo>;
    async fn session_unrevert(&self, loc: &Location, session_id: &str) -> Result<V1SessionInfo>;
    async fn session_summarize(
        &self,
        loc: &Location,
        session_id: &str,
        provider_id: &str,
        model_id: &str,
    ) -> Result<bool>;
    async fn session_share(&self, loc: &Location, session_id: &str) -> Result<V1SessionInfo>;
    async fn session_unshare(&self, loc: &Location, session_id: &str) -> Result<V1SessionInfo>;
    async fn session_fork(
        &self,
        loc: &Location,
        session_id: &str,
        message_id: Option<&str>,
    ) -> Result<V1SessionInfo>;
    async fn session_rename(
        &self,
        loc: &Location,
        session_id: &str,
        title: &str,
    ) -> Result<V1SessionInfo>;
    async fn session_delete(&self, loc: &Location, session_id: &str) -> Result<bool>;
    async fn session_status(&self, loc: &Location) -> Result<BTreeMap<String, SessionStatusInfo>>;

    /// `permission.list` — the requests still waiting for a reply.
    async fn permission_list(&self, loc: &Location) -> Result<Vec<PermissionV1Request>>;
    async fn permission_reply(
        &self,
        loc: &Location,
        request_id: &str,
        reply: PermissionV1Reply,
        message: Option<&str>,
    ) -> Result<bool>;
    /// `question.list` — the questions still waiting for an answer.
    async fn question_list(&self, loc: &Location) -> Result<Vec<QuestionV1Request>>;
    async fn question_reply(
        &self,
        loc: &Location,
        request_id: &str,
        answers: Vec<QuestionV1Answer>,
    ) -> Result<bool>;
    async fn question_reject(&self, loc: &Location, request_id: &str) -> Result<bool>;
    async fn auth_set(&self, loc: &Location, provider_id: &str, key: &str) -> Result<bool>;
    /// `auth.remove`.
    async fn auth_remove(&self, loc: &Location, provider_id: &str) -> Result<bool>;
    /// `provider.oauth.authorize` — the `ProviderAuthAuthorization`, or
    /// `null` for a non-oauth method.
    async fn provider_oauth_authorize(
        &self,
        loc: &Location,
        provider_id: &str,
        method: usize,
    ) -> Result<Value>;
    /// `provider.oauth.callback` — without `code` it waits for the
    /// provider's own redirect (an `auto` flow), however long that takes.
    async fn provider_oauth_callback(
        &self,
        loc: &Location,
        provider_id: &str,
        method: usize,
        code: Option<&str>,
    ) -> Result<bool>;
}

/// reqwest production impl of [`ServerApi`].
#[derive(Debug, Clone)]
pub struct HttpServerApi {
    client: reqwest::Client,
    config: HttpClientConfig,
}

pub(crate) fn build_url(
    config: &HttpClientConfig,
    path: &str,
    method: &Method,
    loc: &Location,
    extra_query: &[(&str, String)],
) -> String {
    let mut params: Vec<(String, String)> = extra_query
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.clone()))
        .collect();
    // Per-request locations are query params on every method (generated
    // client query members); the config-level location moves to the query
    // only on GET/HEAD (`client.tsx:19`, `:34-38`).
    let mut push_param = |key: &str, value: &str| {
        if !params.iter().any(|(existing, _)| existing == key) {
            params.push((key.to_string(), percent_encode(value)));
        }
    };
    if let Some(directory) = non_empty(loc.directory.as_deref()) {
        push_param("directory", directory);
    }
    if let Some(workspace) = non_empty(loc.workspace.as_deref()) {
        push_param("workspace", workspace);
    }
    if *method == Method::GET || *method == Method::HEAD {
        if let Some(directory) = non_empty(config.directory.as_deref()) {
            push_param("directory", directory);
        }
    }
    let mut url = format!("{}{}", config.base_url.trim_end_matches('/'), path);
    if !params.is_empty() {
        url.push('?');
        url.push_str(
            &params
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("&"),
        );
    }
    url
}

impl HttpServerApi {
    pub fn new(config: HttpClientConfig) -> Result<HttpServerApi> {
        Ok(HttpServerApi {
            client: reqwest::Client::builder().build()?,
            config,
        })
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        loc: &Location,
        extra_query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<reqwest::Response> {
        let url = build_url(&self.config, path, &method, loc, extra_query);
        let mut request = self.client.request(method.clone(), &url);
        for (name, value) in &self.config.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if method != Method::GET && method != Method::HEAD {
            if let Some(directory) = non_empty(self.config.directory.as_deref()) {
                request = request.header("x-opencode-directory", percent_encode(directory));
            }
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        request
            .send()
            .await
            .with_context(|| format!("{} {path} request failed", method.as_str()))
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        loc: &Location,
        extra_query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<T> {
        let response = self.request(method, path, loc, extra_query, body).await?;
        let status = response.status();
        let text = response
            .text()
            .await
            .with_context(|| format!("{path} response body read failed"))?;
        if !status.is_success() {
            return Err(anyhow!(
                "server error on {path}: {}: {text}",
                status.as_str()
            ));
        }
        serde_json::from_str(&text).with_context(|| format!("{path} response decode failed"))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, loc: &Location) -> Result<T> {
        self.json(Method::GET, path, loc, &[], None).await
    }

    async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        loc: &Location,
        body: Option<Value>,
    ) -> Result<T> {
        self.json(Method::POST, path, loc, &[], body).await
    }

    /// POST whose response body is dropped (openapi `move-session` has an
    /// empty 200 body).
    async fn post_ignored(&self, path: &str, loc: &Location, body: Option<Value>) -> Result<()> {
        self.request(Method::POST, path, loc, &[], body)
            .await?
            .error_for_status()
            .with_context(|| format!("POST {path} failed"))?;
        Ok(())
    }
}

#[async_trait]
impl ServerApi for HttpServerApi {
    async fn path_get(&self, loc: &Location) -> Result<Value> {
        self.get("/path", loc).await
    }

    async fn project_current(&self, loc: &Location) -> Result<Value> {
        self.get("/project/current", loc).await
    }

    async fn project_directories(&self, loc: &Location, project_id: &str) -> Result<Value> {
        self.get(&format!("/project/{project_id}/directories"), loc)
            .await
    }

    async fn experimental_workspace_list(&self, loc: &Location) -> Result<Value> {
        self.get("/experimental/workspace", loc).await
    }

    async fn experimental_workspace_status(&self, loc: &Location) -> Result<Value> {
        self.get("/experimental/workspace/status", loc).await
    }

    async fn config_providers(&self, loc: &Location) -> Result<Value> {
        self.get("/config/providers", loc).await
    }

    async fn config_get(&self, loc: &Location) -> Result<Value> {
        self.get("/config", loc).await
    }

    async fn provider_list(&self, loc: &Location) -> Result<Value> {
        self.get("/provider", loc).await
    }

    async fn provider_auth(&self, loc: &Location) -> Result<Value> {
        self.get("/provider/auth", loc).await
    }

    async fn app_agents(&self, loc: &Location) -> Result<Value> {
        self.get("/agent", loc).await
    }

    async fn command_list(&self, loc: &Location) -> Result<Value> {
        self.get("/command", loc).await
    }

    async fn lsp_status(&self, loc: &Location) -> Result<Value> {
        self.get("/lsp", loc).await
    }

    async fn mcp_status(&self, loc: &Location) -> Result<Value> {
        self.get("/mcp", loc).await
    }

    async fn mcp_connect(&self, loc: &Location, name: &str) -> Result<bool> {
        self.post(&format!("/mcp/{name}/connect"), loc, None).await
    }

    async fn mcp_disconnect(&self, loc: &Location, name: &str) -> Result<bool> {
        self.post(&format!("/mcp/{name}/disconnect"), loc, None)
            .await
    }

    async fn formatter_status(&self, loc: &Location) -> Result<Value> {
        self.get("/formatter", loc).await
    }

    async fn vcs_get(&self, loc: &Location) -> Result<Value> {
        self.get("/vcs", loc).await
    }

    async fn experimental_capabilities(&self, loc: &Location) -> Result<Value> {
        self.get("/experimental/capabilities", loc).await
    }

    async fn experimental_console(&self, loc: &Location) -> Result<Value> {
        self.get("/experimental/console", loc).await
    }

    async fn experimental_resource_list(&self, loc: &Location) -> Result<Value> {
        self.get("/experimental/resource", loc).await
    }

    async fn experimental_session_background(
        &self,
        loc: &Location,
        session_id: &str,
    ) -> Result<bool> {
        self.post(
            &format!("/experimental/session/{session_id}/background"),
            loc,
            None,
        )
        .await
    }

    async fn sync_start(&self, loc: &Location) -> Result<bool> {
        self.post("/sync/start", loc, None).await
    }

    async fn global_upgrade(&self, loc: &Location, target: &str) -> Result<Value> {
        self.post("/global/upgrade", loc, Some(json!({ "target": target })))
            .await
    }

    async fn experimental_move_session(&self, loc: &Location, req: MoveSession) -> Result<()> {
        self.post_ignored(
            "/experimental/control-plane/move-session",
            loc,
            Some(serde_json::to_value(&req)?),
        )
        .await
    }

    async fn session_list(
        &self,
        loc: &Location,
        query: SessionListQuery,
    ) -> Result<Vec<V1SessionInfo>> {
        let mut extra: Vec<(&str, String)> = Vec::new();
        if let Some(start) = query.start {
            extra.push(("start", start.to_string()));
        }
        if let Some(scope) = non_empty(query.scope.as_deref()) {
            extra.push(("scope", scope.to_string()));
        }
        if let Some(path) = non_empty(query.path.as_deref()) {
            extra.push(("path", percent_encode(path)));
        }
        self.json(Method::GET, "/session", loc, &extra, None).await
    }

    async fn session_get(&self, loc: &Location, session_id: &str) -> Result<V1SessionInfo> {
        self.get(&format!("/session/{session_id}"), loc).await
    }

    async fn session_messages(
        &self,
        loc: &Location,
        session_id: &str,
        limit: Option<u64>,
    ) -> Result<Vec<MessageWithParts>> {
        let extra: Vec<(&str, String)> = limit
            .map(|limit| vec![("limit", limit.to_string())])
            .unwrap_or_default();
        self.json(
            Method::GET,
            &format!("/session/{session_id}/message"),
            loc,
            &extra,
            None,
        )
        .await
    }

    async fn session_todo(&self, loc: &Location, session_id: &str) -> Result<Vec<TodoInfo>> {
        self.get(&format!("/session/{session_id}/todo"), loc).await
    }

    async fn session_diff(
        &self,
        loc: &Location,
        session_id: &str,
        message_id: Option<&str>,
    ) -> Result<Vec<SnapshotFileDiff>> {
        let extra: Vec<(&str, String)> = message_id
            .map(|id| vec![("messageID", id.to_string())])
            .unwrap_or_default();
        self.json(
            Method::GET,
            &format!("/session/{session_id}/diff"),
            loc,
            &extra,
            None,
        )
        .await
    }

    async fn session_create(&self, loc: &Location, req: SessionCreate) -> Result<V1SessionInfo> {
        self.post("/session", loc, Some(serde_json::to_value(&req)?))
            .await
    }

    async fn session_prompt(
        &self,
        loc: &Location,
        session_id: &str,
        req: SessionPrompt,
    ) -> Result<MessageWithParts> {
        self.post(
            &format!("/session/{session_id}/message"),
            loc,
            Some(serde_json::to_value(&req)?),
        )
        .await
    }

    async fn session_command(
        &self,
        loc: &Location,
        session_id: &str,
        req: SessionCommand,
    ) -> Result<MessageWithParts> {
        self.post(
            &format!("/session/{session_id}/command"),
            loc,
            Some(serde_json::to_value(&req)?),
        )
        .await
    }

    async fn session_shell(
        &self,
        loc: &Location,
        session_id: &str,
        req: SessionShell,
    ) -> Result<MessageWithParts> {
        self.post(
            &format!("/session/{session_id}/shell"),
            loc,
            Some(serde_json::to_value(&req)?),
        )
        .await
    }

    async fn session_abort(&self, loc: &Location, session_id: &str) -> Result<bool> {
        self.post(&format!("/session/{session_id}/abort"), loc, None)
            .await
    }

    async fn session_revert(
        &self,
        loc: &Location,
        session_id: &str,
        message_id: &str,
        part_id: Option<&str>,
    ) -> Result<V1SessionInfo> {
        let mut body = Map::new();
        body.insert("messageID".to_string(), json!(message_id));
        if let Some(part_id) = part_id {
            body.insert("partID".to_string(), json!(part_id));
        }
        self.post(
            &format!("/session/{session_id}/revert"),
            loc,
            Some(Value::Object(body)),
        )
        .await
    }

    async fn session_unrevert(&self, loc: &Location, session_id: &str) -> Result<V1SessionInfo> {
        self.post(&format!("/session/{session_id}/unrevert"), loc, None)
            .await
    }

    async fn session_summarize(
        &self,
        loc: &Location,
        session_id: &str,
        provider_id: &str,
        model_id: &str,
    ) -> Result<bool> {
        let body = json!({ "providerID": provider_id, "modelID": model_id });
        self.post(&format!("/session/{session_id}/summarize"), loc, Some(body))
            .await
    }

    async fn session_share(&self, loc: &Location, session_id: &str) -> Result<V1SessionInfo> {
        self.post(&format!("/session/{session_id}/share"), loc, None)
            .await
    }

    async fn session_unshare(&self, loc: &Location, session_id: &str) -> Result<V1SessionInfo> {
        self.request(
            Method::DELETE,
            &format!("/session/{session_id}/share"),
            loc,
            &[],
            None,
        )
        .await?
        .json()
        .await
        .context("unshare response body read failed")
    }

    async fn session_fork(
        &self,
        loc: &Location,
        session_id: &str,
        message_id: Option<&str>,
    ) -> Result<V1SessionInfo> {
        let body = match message_id {
            Some(message_id) => json!({ "messageID": message_id }),
            None => json!({}),
        };
        self.post(&format!("/session/{session_id}/fork"), loc, Some(body))
            .await
    }

    async fn session_rename(
        &self,
        loc: &Location,
        session_id: &str,
        title: &str,
    ) -> Result<V1SessionInfo> {
        let body = json!({ "title": title });
        self.json(
            Method::PATCH,
            &format!("/session/{session_id}"),
            loc,
            &[],
            Some(body),
        )
        .await
    }

    async fn session_delete(&self, loc: &Location, session_id: &str) -> Result<bool> {
        self.request(
            Method::DELETE,
            &format!("/session/{session_id}"),
            loc,
            &[],
            None,
        )
        .await?
        .json()
        .await
        .context("delete response body read failed")
    }

    async fn session_status(&self, loc: &Location) -> Result<BTreeMap<String, SessionStatusInfo>> {
        self.get("/session/status", loc).await
    }

    async fn permission_list(&self, loc: &Location) -> Result<Vec<PermissionV1Request>> {
        self.get("/permission", loc).await
    }

    async fn question_list(&self, loc: &Location) -> Result<Vec<QuestionV1Request>> {
        self.get("/question", loc).await
    }

    async fn permission_reply(
        &self,
        loc: &Location,
        request_id: &str,
        reply: PermissionV1Reply,
        message: Option<&str>,
    ) -> Result<bool> {
        let mut body = Map::new();
        body.insert("reply".to_string(), serde_json::to_value(reply)?);
        if let Some(message) = message {
            body.insert("message".to_string(), json!(message));
        }
        self.post(
            &format!("/permission/{request_id}/reply"),
            loc,
            Some(Value::Object(body)),
        )
        .await
    }

    async fn question_reply(
        &self,
        loc: &Location,
        request_id: &str,
        answers: Vec<QuestionV1Answer>,
    ) -> Result<bool> {
        let body = json!({ "answers": answers });
        self.post(&format!("/question/{request_id}/reply"), loc, Some(body))
            .await
    }

    async fn question_reject(&self, loc: &Location, request_id: &str) -> Result<bool> {
        self.post(&format!("/question/{request_id}/reject"), loc, None)
            .await
    }

    async fn auth_set(&self, loc: &Location, provider_id: &str, key: &str) -> Result<bool> {
        let body = json!({ "type": "api", "key": key });
        self.request(
            Method::PUT,
            &format!("/auth/{provider_id}"),
            loc,
            &[],
            Some(body),
        )
        .await?
        .json()
        .await
        .context("auth set response body read failed")
    }

    async fn auth_remove(&self, loc: &Location, provider_id: &str) -> Result<bool> {
        self.json(
            Method::DELETE,
            &format!("/auth/{provider_id}"),
            loc,
            &[],
            None,
        )
        .await
    }

    async fn provider_oauth_authorize(
        &self,
        loc: &Location,
        provider_id: &str,
        method: usize,
    ) -> Result<Value> {
        let body = json!({ "method": method });
        self.post(
            &format!("/provider/{provider_id}/oauth/authorize"),
            loc,
            Some(body),
        )
        .await
    }

    async fn provider_oauth_callback(
        &self,
        loc: &Location,
        provider_id: &str,
        method: usize,
        code: Option<&str>,
    ) -> Result<bool> {
        let mut body = json!({ "method": method });
        if let Some(code) = code {
            body["code"] = json!(code);
        }
        self.post(
            &format!("/provider/{provider_id}/oauth/callback"),
            loc,
            Some(body),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::percent_encode;

    #[test]
    fn percent_encode_matches_encode_uri_component() {
        assert_eq!(percent_encode("/repo/sub dir"), "%2Frepo%2Fsub%20dir");
        assert_eq!(percent_encode("a-b_c.d~*!'()"), "a-b_c.d~*!'()");
        assert_eq!(percent_encode("héllo"), "h%C3%A9llo");
    }
}
