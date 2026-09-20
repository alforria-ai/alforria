//! `transport::api` acceptance checks (spec M8.1):
//! * `ServerApi` request shapes — paths, query params, JSON bodies —
//!   asserted against the frozen route definitions
//!   (`fixtures/openapi/openapi.json`), the M6 wire contract;
//! * typed response parsing for the `opencode-schema` DTOs;
//! * location transport — query params on GET, headers on POST
//!   (`sdk/js/src/v2/client.ts:18-48`).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use serde_json::{json, Value};

use opencode_schema::permission_v1::PermissionV1Reply;
use opencode_schema::session_status::SessionStatusInfo;
use opencode_schema::session_v1::V1Message;
use opencode_tui::transport::api::{
    HttpClientConfig, HttpServerApi, Location, MoveSession, MoveSessionDestination, ServerApi,
    SessionCommand, SessionCreate, SessionCreateModel, SessionListQuery, SessionPrompt,
    SessionShell,
};

// ------------------------------------------------------------- fixtures

fn session_json() -> Value {
    json!({
        "id": "ses_1",
        "slug": "test",
        "projectID": "prj_1",
        "directory": "/repo",
        "title": "Test",
        "version": "1",
        "time": {"created": 1, "updated": 2},
    })
}

fn user_message_json() -> Value {
    json!({
        "id": "msg_1",
        "sessionID": "ses_1",
        "role": "user",
        "time": {"created": 1.0},
        "agent": "build",
        "model": {"providerID": "anthropic", "modelID": "claude"},
    })
}

fn assistant_message_json() -> Value {
    json!({
        "id": "msg_2",
        "sessionID": "ses_1",
        "role": "assistant",
        "time": {"created": 1},
        "parentID": "msg_1",
        "modelID": "claude",
        "providerID": "anthropic",
        "mode": "primary",
        "agent": "build",
        "path": {"cwd": "/repo", "root": "/repo"},
        "cost": 0.0,
        "tokens": {
            "input": 1.0,
            "output": 1.0,
            "reasoning": 0.0,
            "cache": {"read": 0.0, "write": 0.0},
        },
    })
}

fn text_part_json() -> Value {
    json!({
        "id": "prt_1",
        "sessionID": "ses_1",
        "messageID": "msg_1",
        "type": "text",
        "text": "hi",
    })
}

fn user_message_response() -> Value {
    json!({"info": user_message_json(), "parts": [text_part_json()]})
}

fn assistant_message_response() -> Value {
    json!({"info": assistant_message_json(), "parts": [text_part_json()]})
}

// ------------------------------------------------------- recording server

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Value,
}

#[derive(Clone, Default)]
struct Recorder {
    requests: Arc<Mutex<Vec<Recorded>>>,
    responses: BTreeMap<String, Value>,
    statuses: BTreeMap<String, u16>,
}

impl Recorder {
    async fn spawn(
        responses: BTreeMap<String, Value>,
        statuses: BTreeMap<String, u16>,
    ) -> (SocketAddr, Recorder) {
        let recorder = Recorder {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses,
            statuses,
        };
        let app = axum::Router::new()
            .fallback(
                |state: axum::extract::State<Recorder>, request: Request| async move {
                    let method = request.method().to_string();
                    let path = request.uri().path().to_string();
                    let query = request.uri().query().unwrap_or_default().to_string();
                    let headers = request
                        .headers()
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.to_string(),
                                String::from_utf8_lossy(value.as_bytes()).to_string(),
                            )
                        })
                        .collect();
                    let body_bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                        .await
                        .expect("body read");
                    let body: Value = if body_bytes.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(&body_bytes).expect("body is JSON")
                    };
                    let key = format!("{} {}", method, path);
                    state.requests.lock().unwrap().push(Recorded {
                        method,
                        path,
                        query,
                        headers,
                        body,
                    });
                    let response = state
                        .responses
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    let status = state.statuses.get(&key).copied().unwrap_or(200);
                    let mut built = Response::builder()
                        .status(status)
                        .body(Body::from(response.to_string()))
                        .expect("static response");
                    built
                        .headers_mut()
                        .insert("content-type", "application/json".parse().expect("mime"));
                    built
                },
            )
            .with_state(recorder.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind port 0");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server");
        });
        (addr, recorder)
    }

    fn recorded(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

fn openai_spec() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/openapi/openapi.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("openapi fixture"))
        .expect("openapi json")
}

/// Every recorded request must hit a frozen (method, path) route, and every
/// query param must be either the location pair or a declared route param.
fn assert_against_frozen_routes(requests: &[Recorded]) {
    let spec = openai_spec();
    let paths = spec["paths"].as_object().expect("paths");
    for request in requests {
        let template = paths
            .keys()
            .find(|template| template_matches(template, &request.path))
            .unwrap_or_else(|| panic!("unknown route path {}", request.path))
            .clone();
        let path_item = paths
            .get(&template)
            .expect("template key exists")
            .as_object()
            .expect("path item");
        let method_key = request.method.to_lowercase();
        let operation = path_item
            .get(&method_key)
            .unwrap_or_else(|| panic!("frozen route has no {} {}", request.method, request.path));
        let declared: Vec<String> = operation["parameters"]
            .as_array()
            .map(|params| {
                params
                    .iter()
                    .filter(|param| param["in"] == "query")
                    .filter_map(|param| param["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        for pair in request.query.split('&').filter(|pair| !pair.is_empty()) {
            let name = pair.split('=').next().unwrap_or_default();
            assert!(
                name == "directory" || name == "workspace" || declared.contains(&name.to_string()),
                "route {} {} has no query param {name}",
                request.method,
                request.path
            );
        }
    }
}

/// Openapi path templates (`/session/{sessionID}`) match one concrete path.
fn template_matches(template: &str, path: &str) -> bool {
    if template == path {
        return true;
    }
    let template: Vec<&str> = template.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    template.len() == path.len()
        && template.iter().zip(path.iter()).all(|(template, segment)| {
            template.starts_with('{') && template.ends_with('}') || template == segment
        })
}

fn api(addr: SocketAddr) -> HttpServerApi {
    HttpServerApi::new(HttpClientConfig {
        base_url: format!("http://{addr}"),
        directory: Some("/the/repo".to_string()),
        headers: vec![(
            "authorization".to_string(),
            "Basic dXNlcjpwYXNz".to_string(),
        )],
    })
    .expect("api")
}

const EMPTY_LOC: &Location = &Location {
    directory: None,
    workspace: None,
};

/// Exercise every ServerApi method; assert each request lands on a frozen
/// route.
#[tokio::test]
async fn request_shapes_match_frozen_routes() {
    let mut responses: BTreeMap<String, Value> = BTreeMap::new();
    for path in [
        "/config/providers",
        "/config",
        "/provider",
        "/provider/auth",
        "/agent",
        "/command",
        "/lsp",
        "/mcp",
        "/formatter",
        "/vcs",
        "/experimental/capabilities",
        "/experimental/console",
        "/experimental/resource",
        "/session/status",
    ] {
        responses.insert(format!("GET {path}"), json!({}));
    }
    responses.insert(
        "POST /global/upgrade".to_string(),
        json!({"success": true, "version": "1.0.0"}),
    );
    responses.insert("POST /sync/start".to_string(), json!(true));
    responses.insert("GET /session".to_string(), json!([]));
    responses.insert("GET /session/ses_1".to_string(), session_json());
    responses.insert(
        "GET /session/ses_1/message".to_string(),
        json!([user_message_response()]),
    );
    responses.insert("GET /session/ses_1/todo".to_string(), json!([]));
    responses.insert("GET /session/ses_1/diff".to_string(), json!([]));
    responses.insert("POST /session".to_string(), session_json());
    responses.insert(
        "POST /session/ses_1/message".to_string(),
        assistant_message_response(),
    );
    responses.insert(
        "POST /session/ses_1/command".to_string(),
        assistant_message_response(),
    );
    responses.insert(
        "POST /session/ses_1/shell".to_string(),
        assistant_message_response(),
    );
    responses.insert("POST /session/ses_1/abort".to_string(), json!(true));
    responses.insert("POST /session/ses_1/revert".to_string(), session_json());
    responses.insert("POST /session/ses_1/unrevert".to_string(), session_json());
    responses.insert("POST /session/ses_1/summarize".to_string(), json!(true));
    responses.insert("POST /session/ses_1/share".to_string(), session_json());
    responses.insert("DELETE /session/ses_1/share".to_string(), session_json());
    responses.insert("POST /session/ses_1/fork".to_string(), session_json());
    responses.insert("PATCH /session/ses_1".to_string(), session_json());
    responses.insert("DELETE /session/ses_1".to_string(), json!(true));
    responses.insert("POST /permission/per_1/reply".to_string(), json!(true));
    responses.insert("POST /question/que_1/reply".to_string(), json!(true));
    responses.insert("POST /question/que_1/reject".to_string(), json!(true));
    responses.insert("POST /mcp/server/connect".to_string(), json!(true));
    responses.insert("POST /mcp/server/disconnect".to_string(), json!(true));
    responses.insert(
        "POST /experimental/session/ses_1/background".to_string(),
        json!(true),
    );
    responses.insert(
        "POST /experimental/control-plane/move-session".to_string(),
        json!({}),
    );

    let (addr, recorder) = Recorder::spawn(responses, BTreeMap::new()).await;
    let api = api(addr);
    let api = &api;

    // --- instance & metadata families ---
    api.config_providers(EMPTY_LOC).await.expect("ok");
    api.config_get(EMPTY_LOC).await.expect("ok");
    api.provider_list(EMPTY_LOC).await.expect("ok");
    api.provider_auth(EMPTY_LOC).await.expect("ok");
    api.app_agents(EMPTY_LOC).await.expect("ok");
    api.command_list(EMPTY_LOC).await.expect("ok");
    api.lsp_status(EMPTY_LOC).await.expect("ok");
    api.mcp_status(EMPTY_LOC).await.expect("ok");
    api.formatter_status(EMPTY_LOC).await.expect("ok");
    api.vcs_get(EMPTY_LOC).await.expect("ok");
    api.experimental_capabilities(EMPTY_LOC).await.expect("ok");
    api.experimental_console(EMPTY_LOC).await.expect("ok");
    api.experimental_resource_list(EMPTY_LOC).await.expect("ok");
    api.sync_start(EMPTY_LOC).await.expect("ok");
    api.global_upgrade(EMPTY_LOC, "stable").await.expect("ok");

    // --- session family ---
    api.session_list(
        EMPTY_LOC,
        SessionListQuery {
            start: Some(1_000),
            scope: Some("project".to_string()),
            path: None,
        },
    )
    .await
    .expect("ok");
    api.session_get(EMPTY_LOC, "ses_1").await.expect("ok");
    api.session_messages(EMPTY_LOC, "ses_1", Some(100))
        .await
        .expect("ok");
    api.session_todo(EMPTY_LOC, "ses_1").await.expect("ok");
    api.session_diff(EMPTY_LOC, "ses_1", Some("msg_1"))
        .await
        .expect("ok");
    api.session_create(
        EMPTY_LOC,
        SessionCreate {
            agent: Some("build".to_string()),
            model: Some(SessionCreateModel {
                id: "claude".to_string(),
                provider_id: "anthropic".to_string(),
                variant: None,
            }),
            title: None,
            parent_id: None,
        },
    )
    .await
    .expect("ok");
    api.session_prompt(
        EMPTY_LOC,
        "ses_1",
        SessionPrompt {
            agent: Some("build".to_string()),
            model_id: None,
            provider_id: None,
            model: None,
            variant: None,
            parts: vec![json!({"type": "text", "text": "hi"})],
        },
    )
    .await
    .expect("ok");
    api.session_command(
        EMPTY_LOC,
        "ses_1",
        SessionCommand {
            command: "review".to_string(),
            arguments: String::new(),
            agent: None,
            model: Some("anthropic/claude".to_string()),
            variant: None,
            parts: Vec::new(),
        },
    )
    .await
    .expect("ok");
    api.session_shell(
        EMPTY_LOC,
        "ses_1",
        SessionShell {
            command: "ls".to_string(),
            agent: "build".to_string(),
            model: None,
        },
    )
    .await
    .expect("ok");
    api.session_abort(EMPTY_LOC, "ses_1").await.expect("ok");
    api.session_revert(EMPTY_LOC, "ses_1", "msg_1", None)
        .await
        .expect("ok");
    api.session_unrevert(EMPTY_LOC, "ses_1").await.expect("ok");
    api.session_summarize(EMPTY_LOC, "ses_1", "anthropic", "claude")
        .await
        .expect("ok");
    api.session_share(EMPTY_LOC, "ses_1").await.expect("ok");
    api.session_unshare(EMPTY_LOC, "ses_1").await.expect("ok");
    api.session_fork(EMPTY_LOC, "ses_1", None)
        .await
        .expect("ok");
    api.session_rename(EMPTY_LOC, "ses_1", "Renamed")
        .await
        .expect("ok");
    api.session_delete(EMPTY_LOC, "ses_1").await.expect("ok");
    api.session_status(EMPTY_LOC).await.expect("ok");

    // --- permission & question ---
    api.permission_reply(EMPTY_LOC, "per_1", PermissionV1Reply::Once, None)
        .await
        .expect("ok");
    api.question_reply(EMPTY_LOC, "que_1", vec![vec!["yes".to_string()]])
        .await
        .expect("ok");
    api.question_reject(EMPTY_LOC, "que_1").await.expect("ok");

    // --- mcp & experimental ---
    api.mcp_connect(EMPTY_LOC, "server").await.expect("ok");
    api.mcp_disconnect(EMPTY_LOC, "server").await.expect("ok");
    api.experimental_session_background(EMPTY_LOC, "ses_1")
        .await
        .expect("ok");
    api.experimental_move_session(
        EMPTY_LOC,
        MoveSession {
            session_id: "ses_1".to_string(),
            destination: MoveSessionDestination {
                directory: "/other".to_string(),
            },
            move_changes: Some(true),
        },
    )
    .await
    .expect("ok");

    let requests = recorder.recorded();
    assert!(!requests.is_empty());
    assert_against_frozen_routes(&requests);
}

#[tokio::test]
async fn location_rides_as_query_on_get_and_headers_on_post() {
    let mut responses: BTreeMap<String, Value> = BTreeMap::new();
    responses.insert("GET /config".to_string(), json!({}));
    responses.insert("POST /session".to_string(), session_json());
    let (addr, recorder) = Recorder::spawn(responses, BTreeMap::new()).await;
    let api = api(addr);
    let api = &api;

    let loc = Location {
        directory: None,
        workspace: Some("wrk_1".to_string()),
    };
    api.config_get(&loc).await.expect("ok");
    api.session_create(&loc, SessionCreate::default())
        .await
        .expect("ok");

    let requests = recorder.recorded();
    let get = &requests[0];
    assert_eq!(get.method, "GET");
    assert_eq!(get.path, "/config");
    assert!(
        get.query.contains("directory=%2Fthe%2Frepo"),
        "config directory is an encoded query param on GET: {}",
        get.query
    );
    assert!(
        get.query.contains("workspace=wrk_1"),
        "workspace rides as a query param: {}",
        get.query
    );
    assert!(
        !get.headers
            .iter()
            .any(|(name, _)| name == "x-opencode-directory"),
        "GET requests drop the location header"
    );

    let post = &requests[1];
    assert_eq!(post.method, "POST");
    assert!(
        post.headers
            .iter()
            .any(|(name, value)| name == "x-opencode-directory" && value == "%2Fthe%2Frepo"),
        "POST requests carry the encoded directory header: {:?}",
        post.headers
    );
    assert!(
        post.query.contains("workspace=wrk_1"),
        "per-request location rides as a query param on POST: {}",
        post.query
    );

    // Custom headers from run() input reach every request.
    assert!(requests.iter().all(|request| {
        request
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Basic dXNlcjpwYXNz")
    }));
}

#[tokio::test]
async fn per_request_directory_wins_over_config() {
    let mut responses: BTreeMap<String, Value> = BTreeMap::new();
    responses.insert("POST /session".to_string(), session_json());
    let (addr, recorder) = Recorder::spawn(responses, BTreeMap::new()).await;
    let api = api(addr);

    api.session_create(
        &Location {
            directory: Some("/other/repo".to_string()),
            workspace: None,
        },
        SessionCreate::default(),
    )
    .await
    .expect("ok");

    let requests = recorder.recorded();
    let post = &requests[0];
    assert!(
        post.query.contains("directory=%2Fother%2Frepo"),
        "per-request directory wins: {}",
        post.query
    );
}

#[tokio::test]
async fn typed_responses_parse_into_m1_dtos() {
    let mut responses: BTreeMap<String, Value> = BTreeMap::new();
    responses.insert("GET /session".to_string(), json!([session_json()]));
    responses.insert("GET /session/ses_1".to_string(), session_json());
    responses.insert(
        "GET /session/ses_1/message".to_string(),
        json!([user_message_response()]),
    );
    responses.insert(
        "GET /session/ses_1/todo".to_string(),
        json!([{"content": "write tests", "status": "pending", "priority": "high"}]),
    );
    responses.insert(
        "GET /session/ses_1/diff".to_string(),
        json!([{"file": "src/lib.rs", "additions": 3.0, "deletions": 1.0}]),
    );
    responses.insert(
        "POST /session/ses_1/message".to_string(),
        assistant_message_response(),
    );
    responses.insert(
        "GET /session/status".to_string(),
        json!({"ses_1": {"type": "busy"}}),
    );
    let (addr, _recorder) = Recorder::spawn(responses, BTreeMap::new()).await;
    let api = api(addr);
    let api = &api;

    let sessions = api
        .session_list(EMPTY_LOC, SessionListQuery::default())
        .await
        .expect("ok");
    assert_eq!(sessions[0].id, "ses_1");

    let session = api.session_get(EMPTY_LOC, "ses_1").await.expect("ok");
    assert_eq!(session.title, "Test");

    let messages = api
        .session_messages(EMPTY_LOC, "ses_1", None)
        .await
        .expect("ok");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].parts.len(), 1);
    assert!(
        matches!(&messages[0].info, V1Message::User { agent, .. } if agent == "build"),
        "user message parses"
    );

    let todos = api.session_todo(EMPTY_LOC, "ses_1").await.expect("ok");
    assert_eq!(todos[0].content, "write tests");

    let diffs = api
        .session_diff(EMPTY_LOC, "ses_1", None)
        .await
        .expect("ok");
    assert_eq!(diffs[0].additions, 3.0);

    let reply = api
        .session_prompt(
            EMPTY_LOC,
            "ses_1",
            SessionPrompt {
                parts: Vec::new(),
                ..SessionPrompt::default()
            },
        )
        .await
        .expect("ok");
    assert!(
        matches!(&reply.info, V1Message::Assistant { mode, .. } if mode == "primary"),
        "assistant message parses"
    );

    let statuses = api.session_status(EMPTY_LOC).await.expect("ok");
    assert!(matches!(statuses["ses_1"], SessionStatusInfo::Busy));
}

#[tokio::test]
async fn request_bodies_match_the_openapi_shapes() {
    let mut responses: BTreeMap<String, Value> = BTreeMap::new();
    responses.insert(
        "POST /session/ses_1/message".to_string(),
        assistant_message_response(),
    );
    responses.insert(
        "POST /session/ses_1/command".to_string(),
        assistant_message_response(),
    );
    responses.insert("POST /session/ses_1/revert".to_string(), session_json());
    responses.insert("POST /session".to_string(), session_json());
    responses.insert(
        "POST /global/upgrade".to_string(),
        json!({"success": true, "version": "1.0.0"}),
    );
    responses.insert("POST /sync/start".to_string(), json!(true));
    let (addr, recorder) = Recorder::spawn(responses, BTreeMap::new()).await;
    let api = api(addr);
    let api = &api;

    api.session_prompt(
        EMPTY_LOC,
        "ses_1",
        SessionPrompt {
            agent: Some("build".to_string()),
            model_id: Some("claude".to_string()),
            provider_id: Some("anthropic".to_string()),
            model: Some(opencode_tui::transport::api::ProviderModel {
                provider_id: "anthropic".to_string(),
                model_id: "claude".to_string(),
            }),
            variant: Some("thinking".to_string()),
            parts: vec![json!({"type": "text", "text": "hi"})],
        },
    )
    .await
    .expect("ok");
    api.session_command(
        EMPTY_LOC,
        "ses_1",
        SessionCommand {
            command: "review".to_string(),
            arguments: "arg".to_string(),
            agent: Some("build".to_string()),
            model: Some("anthropic/claude".to_string()),
            variant: None,
            parts: Vec::new(),
        },
    )
    .await
    .expect("ok");
    api.session_revert(EMPTY_LOC, "ses_1", "msg_1", Some("prt_1"))
        .await
        .expect("ok");
    api.session_create(
        EMPTY_LOC,
        SessionCreate {
            agent: Some("build".to_string()),
            model: Some(SessionCreateModel {
                id: "claude".to_string(),
                provider_id: "anthropic".to_string(),
                variant: None,
            }),
            title: Some("T".to_string()),
            parent_id: Some("ses_0".to_string()),
        },
    )
    .await
    .expect("ok");
    api.global_upgrade(EMPTY_LOC, "stable").await.expect("ok");

    let requests = recorder.recorded();
    let prompt = requests
        .iter()
        .find(|request| request.path == "/session/ses_1/message")
        .expect("prompt recorded");
    // `...selectedModel` spreads `providerID`/`modelID` top-level
    // (`prompt/index.tsx:1098`) in addition to the nested `model`.
    assert_eq!(
        prompt.body,
        json!({
            "providerID": "anthropic",
            "modelID": "claude",
            "agent": "build",
            "model": {"providerID": "anthropic", "modelID": "claude"},
            "variant": "thinking",
            "parts": [{"type": "text", "text": "hi"}],
        }),
        "session.prompt body"
    );
    let command = requests
        .iter()
        .find(|request| request.path == "/session/ses_1/command")
        .expect("command recorded");
    assert_eq!(
        command.body,
        json!({
            "command": "review",
            "arguments": "arg",
            "agent": "build",
            "model": "anthropic/claude",
        }),
        "session.command body"
    );
    let revert = requests
        .iter()
        .find(|request| request.path == "/session/ses_1/revert")
        .expect("revert recorded");
    assert_eq!(
        revert.body,
        json!({"messageID": "msg_1", "partID": "prt_1"}),
        "session.revert body"
    );
    let create = requests
        .iter()
        .find(|request| request.path == "/session")
        .expect("create recorded");
    assert_eq!(
        create.body,
        json!({
            "agent": "build",
            "model": {"id": "claude", "providerID": "anthropic"},
            "title": "T",
            "parentID": "ses_0",
        }),
        "session.create body"
    );
    let upgrade = requests
        .iter()
        .find(|request| request.path == "/global/upgrade")
        .expect("upgrade recorded");
    assert_eq!(upgrade.body, json!({"target": "stable"}));
}

#[tokio::test]
async fn error_responses_are_rejected() {
    let mut statuses = BTreeMap::new();
    statuses.insert("GET /config".to_string(), 500u16);
    let (addr, _recorder) = Recorder::spawn(BTreeMap::new(), statuses).await;
    let api = api(addr);

    let error = api
        .config_get(EMPTY_LOC)
        .await
        .expect_err("500 must be an error");
    assert!(
        error.to_string().contains("/config"),
        "error names the route: {error}"
    );
}
