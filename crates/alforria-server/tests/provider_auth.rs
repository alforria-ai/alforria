//! The built-in LibertAI provider-auth hook over the v1 provider routes:
//! methods, the browser sign-in (loopback redirect or pasted redirect) and
//! the logout side of `DELETE /auth/libertai`, against a fake account API.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use alforria_core::libertai::auth::Endpoints;
use alforria_core::{EventBus, SessionServices, Storage};
use alforria_server::routes;
use alforria_server::state::{
    AuthConfig, AuthStore, EmptyUiBackend, InstanceStore, MemoryAuthStore, ProviderAuthService,
    ServerContext,
};
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::Json;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

// -----------------------------------------------------------------------
// fake account API
// -----------------------------------------------------------------------

#[derive(Default)]
struct Account {
    exchanges: Mutex<Vec<Value>>,
    hosts: Mutex<Vec<String>>,
    revoked: Mutex<Vec<String>>,
}

async fn fake_account(account: Arc<Account>) -> String {
    use axum::extract::State;
    use axum::routing::post;
    let app = axum::Router::new()
        .route(
            "/auth/exchange",
            post(
                |State(account): State<Arc<Account>>, Json(body): Json<Value>| async move {
                    account.exchanges.lock().unwrap().push(body.clone());
                    if body["code"] == "good-code" {
                        (
                            StatusCode::OK,
                            Json(json!({"access_token": "acc", "refresh_token": "ref"})),
                        )
                    } else {
                        (StatusCode::BAD_REQUEST, Json(json!({"detail": "bad code"})))
                    }
                },
            ),
        )
        .route(
            "/api-keys/cli",
            post(
                |State(account): State<Arc<Account>>,
                 headers: HeaderMap,
                 Json(body): Json<Value>| async move {
                    if headers.get("authorization").and_then(|v| v.to_str().ok())
                        != Some("Bearer acc")
                    {
                        return (StatusCode::UNAUTHORIZED, Json(json!({})));
                    }
                    let host = body["host"].as_str().unwrap_or_default().to_string();
                    account.hosts.lock().unwrap().push(host.clone());
                    (
                        StatusCode::OK,
                        Json(json!({
                            "id": "key-1",
                            "name": host,
                            "full_key": "LTAI_fake_key",
                            "expires_at": "2026-11-02T00:00:00Z",
                        })),
                    )
                },
            ),
        )
        .route(
            "/auth/logout",
            post(
                |State(account): State<Arc<Account>>, Json(body): Json<Value>| async move {
                    let token = body["refresh_token"].as_str().unwrap_or_default();
                    account.revoked.lock().unwrap().push(token.to_string());
                    StatusCode::OK
                },
            ),
        )
        .with_state(account);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

// -----------------------------------------------------------------------
// fixture
// -----------------------------------------------------------------------

struct NoJobs;
impl alforria_core::BackgroundJobs for NoJobs {
    fn list(&self) -> Result<Vec<alforria_core::BackgroundJobInfo>, alforria_core::CoreError> {
        Ok(Vec::new())
    }
    fn cancel(&self, _id: &str) -> Result<(), alforria_core::CoreError> {
        Ok(())
    }
}

struct FixedClock;
impl alforria_core::Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        0
    }
}

const CONSOLE: &str = "http://console.test";

struct Fixture {
    _dir: tempfile::TempDir,
    router: axum::Router,
    auth: Arc<MemoryAuthStore>,
    account: Arc<Account>,
    session_file: std::path::PathBuf,
    worktree: std::path::PathBuf,
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let worktree = dir.path().join("repo");
    std::fs::create_dir_all(&worktree).unwrap();
    let storage = Arc::new(Storage::open(dir.path().join("db.sqlite")).unwrap());
    let agent_input = alforria_core::AgentRegistryInput {
        config: serde_json::from_value(json!({})).unwrap(),
        skill_dirs: Vec::new(),
        reference_dirs: Vec::new(),
        worktree: worktree.clone(),
        data_dir: dir.path().to_path_buf(),
        tmp_dir: dir.path().to_path_buf(),
        home: dir.path().to_path_buf(),
    };
    let services = Arc::new(SessionServices::new(
        storage.clone(),
        Arc::new(NoJobs),
        Arc::new(FixedClock),
        &agent_input,
    ));
    let instances = InstanceStore::new(Arc::new(move |_directory| Ok(services.clone())));
    let mut ctx = ServerContext::new(
        AuthConfig::new("alforria", None),
        instances,
        storage.clone(),
        Arc::new(EventBus::new_shared(storage, None)),
        Vec::new(),
        Arc::new(EmptyUiBackend),
    );
    let account = Arc::new(Account::default());
    let session_file = dir.path().join("libertai-auth.json");
    let auth = Arc::new(MemoryAuthStore::default());
    ctx.auth_store = auth.clone();
    ctx.provider_auth = Arc::new(ProviderAuthService::with_libertai(Endpoints {
        account: fake_account(account.clone()).await,
        console: CONSOLE.to_string(),
        session_file: session_file.clone(),
    }));
    Fixture {
        _dir: dir,
        router: routes::build_router(Arc::new(ctx)),
        auth,
        account,
        session_file,
        worktree,
    }
}

impl Fixture {
    fn uri(&self, rest: &str) -> String {
        format!(
            "/{rest}?directory={}",
            urlencode(&self.worktree.to_string_lossy())
        )
    }

    async fn send(&self, method: &str, uri: &str, body: &str) -> (StatusCode, String) {
        send(&self.router, method, uri, body).await
    }

    /// `POST /provider/libertai/oauth/authorize {"method":0}` → the parsed
    /// authorization plus its PKCE state, challenge and loopback redirect.
    async fn authorize(&self) -> Authorization {
        let (status, body) = self
            .send(
                "POST",
                &self.uri("provider/libertai/oauth/authorize"),
                r#"{"method":0}"#,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let value: Value = serde_json::from_str(&body).unwrap();
        let url = value["url"].as_str().unwrap().to_string();
        let param = |key: &str| query_param(&url, key).unwrap_or_else(|| panic!("{key} in {url}"));
        Authorization {
            state: param("state"),
            challenge: param("challenge"),
            redirect_uri: param("redirect_uri"),
            value,
            body,
            url,
        }
    }

    /// The no-code callback, in flight on its own task.
    fn wait_callback(&self) -> tokio::task::JoinHandle<(StatusCode, String)> {
        let router = self.router.clone();
        let uri = self.uri("provider/libertai/oauth/callback");
        tokio::spawn(async move { send(&router, "POST", &uri, r#"{"method":0}"#).await })
    }

    async fn paste(&self, code: &str) -> (StatusCode, String) {
        self.send(
            "POST",
            &self.uri("provider/libertai/oauth/callback"),
            &json!({"method": 0, "code": code}).to_string(),
        )
        .await
    }

    fn stored_key(&self) -> Option<Value> {
        self.auth.all().unwrap().get("libertai").cloned()
    }

    fn session(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&self.session_file).unwrap()).unwrap()
    }
}

struct Authorization {
    value: Value,
    body: String,
    url: String,
    state: String,
    challenge: String,
    redirect_uri: String,
}

impl Authorization {
    fn port(&self) -> u16 {
        self.redirect_uri
            .trim_start_matches("http://127.0.0.1:")
            .trim_end_matches("/callback")
            .parse()
            .unwrap()
    }
}

async fn send(router: &axum::Router, method: &str, uri: &str, body: &str) -> (StatusCode, String) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn urlencode(input: &str) -> String {
    let mut out = String::new();
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| {
            let bytes = v.as_bytes();
            let mut out = Vec::new();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'%' && i + 2 < bytes.len() {
                    out.push(u8::from_str_radix(&v[i + 1..i + 3], 16).unwrap());
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            String::from_utf8(out).unwrap()
        })
    })
}

/// Play the browser landing on the loopback redirect.
async fn hit_loopback(port: u16, query: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(format!("GET /callback?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

async fn assert_port_released(port: u16) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "loopback {port} still bound"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

const CALLBACK_FAILED: &str = r#"{"name":"ProviderAuthOauthCallbackFailed","data":{}}"#;
const OAUTH_MISSING: &str =
    r#"{"name":"ProviderAuthOauthMissing","data":{"providerID":"libertai"}}"#;

// -----------------------------------------------------------------------
// tests
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn methods_list_the_libertai_hook() {
    let f = fixture().await;
    let (status, body) = f.send("GET", &f.uri("provider/auth"), "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        r#"{"libertai":[{"type":"oauth","label":"Sign in with LibertAI"},{"type":"api","label":"API key"}]}"#
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authorize_returns_the_console_url_with_a_loopback_redirect() {
    let f = fixture().await;
    let authorization = f.authorize().await;
    assert_eq!(authorization.value["method"], "auto");
    assert_eq!(
        authorization.value["instructions"],
        "Finish signing in in the browser tab that opened. If your browser runs on another machine, paste the address it lands on."
    );
    // `ProviderAuthAuthorization` field order.
    let at = |key: &str| authorization.body.find(&format!("\"{key}\":")).unwrap();
    assert!(at("url") < at("method") && at("method") < at("instructions"));
    assert!(authorization.url.starts_with(&format!("{CONSOLE}/cli?")));
    assert_eq!(
        query_param(&authorization.url, "client").unwrap(),
        "Alforria"
    );
    assert_eq!(authorization.challenge.len(), 43);
    assert!(authorization.redirect_uri.starts_with("http://127.0.0.1:"));
    assert!(authorization.redirect_uri.ends_with("/callback"));
    // The loopback is listening.
    tokio::net::TcpStream::connect(("127.0.0.1", authorization.port()))
        .await
        .unwrap();

    // The api-key method resolves without a result; an unknown index or a
    // provider without a hook is a defect.
    let (status, body) = f
        .send(
            "POST",
            &f.uri("provider/libertai/oauth/authorize"),
            r#"{"method":1}"#,
        )
        .await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "null"));
    let (status, _) = f
        .send(
            "POST",
            &f.uri("provider/libertai/oauth/authorize"),
            r#"{"method":2}"#,
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (status, _) = f
        .send(
            "POST",
            &f.uri("provider/anthropic/oauth/authorize"),
            r#"{"method":0}"#,
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_redirect_completes_the_waiting_callback() {
    let f = fixture().await;
    let authorization = f.authorize().await;
    let waiter = f.wait_callback();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !waiter.is_finished(),
        "callback must block until the redirect"
    );

    let page = hit_loopback(
        authorization.port(),
        &format!("code=good-code&state={}", authorization.state),
    )
    .await;
    assert!(page.contains("You can close this tab and return to alforria."));
    assert_eq!(waiter.await.unwrap(), (StatusCode::OK, "true".to_string()));

    assert_eq!(
        f.stored_key(),
        Some(json!({"type": "api", "key": "LTAI_fake_key"}))
    );
    // The exchange carried the verifier behind the authorize challenge.
    let exchange = f.account.exchanges.lock().unwrap()[0].clone();
    let verifier = exchange["verifier"].as_str().unwrap();
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        authorization.challenge
    );
    // The sidecar keeps the refresh token; the key is named after the device.
    let session = f.session();
    assert_eq!(session["refresh_token"], "ref");
    let device_id = session["device_id"].as_str().unwrap();
    assert_eq!(
        f.account.hosts.lock().unwrap().clone(),
        vec![format!(
            "{}-{device_id}",
            alforria_core::libertai::auth::device_hostname()
        )]
    );

    // Single-use: the flow is gone and its port released.
    let (status, body) = f.paste("good-code").await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::BAD_REQUEST, OAUTH_MISSING)
    );
    assert_port_released(authorization.port()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pasted_redirect_completes_and_wakes_the_waiter() {
    let f = fixture().await;
    let authorization = f.authorize().await;
    let waiter = f.wait_callback();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let pasted = format!(
        "{}?code=good-code&state={}",
        authorization.redirect_uri, authorization.state
    );
    assert_eq!(f.paste(&pasted).await, (StatusCode::OK, "true".to_string()));
    assert_eq!(waiter.await.unwrap(), (StatusCode::OK, "true".to_string()));
    assert_eq!(
        f.stored_key(),
        Some(json!({"type": "api", "key": "LTAI_fake_key"}))
    );
    assert_eq!(f.account.exchanges.lock().unwrap().len(), 1);
    assert_port_released(authorization.port()).await;

    let (status, body) = f
        .send(
            "POST",
            &f.uri("provider/libertai/oauth/callback"),
            r#"{"method":0}"#,
        )
        .await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::BAD_REQUEST, OAUTH_MISSING)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_pastes_fail_without_settling_the_flow() {
    let f = fixture().await;
    let authorization = f.authorize().await;

    let (status, body) = f.paste("code=good-code&state=forged").await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::BAD_REQUEST, CALLBACK_FAILED)
    );
    let (status, body) = f
        .paste(&format!("code=bad-code&state={}", authorization.state))
        .await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::BAD_REQUEST, CALLBACK_FAILED)
    );
    assert_eq!(f.stored_key(), None);

    // The flow is still pending: a bare code (no state) finishes it.
    assert_eq!(
        f.paste("good-code").await,
        (StatusCode::OK, "true".to_string())
    );
    assert_eq!(
        f.stored_key(),
        Some(json!({"type": "api", "key": "LTAI_fake_key"}))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn callback_without_a_pending_flow_is_oauth_missing() {
    let f = fixture().await;
    let (status, body) = f
        .send(
            "POST",
            &f.uri("provider/libertai/oauth/callback"),
            r#"{"method":0}"#,
        )
        .await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::BAD_REQUEST, OAUTH_MISSING)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_authorize_cancels_the_previous_flow() {
    let f = fixture().await;
    let first = f.authorize().await;
    let waiter = f.wait_callback();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let second = f.authorize().await;
    assert_ne!(first.state, second.state);
    assert_eq!(
        waiter.await.unwrap(),
        (StatusCode::BAD_REQUEST, CALLBACK_FAILED.to_string())
    );
    assert_port_released(first.port()).await;

    // The replacement flow still completes.
    let pasted = format!("code=good-code&state={}", second.state);
    assert_eq!(f.paste(&pasted).await, (StatusCode::OK, "true".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_libertai_auth_revokes_the_session() {
    let f = fixture().await;
    std::fs::write(
        &f.session_file,
        r#"{"refresh_token":"stored-ref","device_id":"abcd1234"}"#,
    )
    .unwrap();
    f.auth
        .set("libertai", json!({"type": "api", "key": "LTAI_old"}))
        .unwrap();

    let (status, body) = f.send("DELETE", "/auth/libertai", "").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "true"));
    assert_eq!(
        f.account.revoked.lock().unwrap().clone(),
        vec!["stored-ref".to_string()]
    );
    assert!(!f.session_file.exists());
    assert_eq!(f.stored_key(), None);

    // Other providers skip the LibertAI logout.
    f.auth
        .set("anthropic", json!({"type": "api", "key": "sk"}))
        .unwrap();
    let (status, _) = f.send("DELETE", "/auth/anthropic", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.account.revoked.lock().unwrap().len(), 1);
}
