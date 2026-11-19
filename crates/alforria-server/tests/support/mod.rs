//! Shared fixture for the unattended-server route tests: the production
//! context over a scripted LLM, a host config written before boot, and
//! helpers to mint bearer tokens.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use alforria_core::session::llm::{LlmEventStream, LlmStream, StreamInput};
use alforria_core::session::prompt_input::Models as InputModels;
use alforria_core::session::r#loop::{LoopError, ModelSource, ResolvedModel};
use alforria_llm::schema::events::LlmEvent;
use alforria_llm::schema::ids::FinishReason;
use alforria_llm::LlmError;
use alforria_server::engine::EngineSeams;
use alforria_server::state::{AuthConfig, ServerContext};
use alforria_server::{routes, ListenOptions};
use axum::body::Body;
use axum::http::{Request, Response};
use base64::Engine;
use futures::future::BoxFuture;
use ring::signature::KeyPair;
use serde_json::{json, Value};
use tower::ServiceExt;

pub type Script = Vec<Result<LlmEvent, LlmError>>;

fn test_model() -> ResolvedModel {
    use alforria_core::session::llm::LlmModel;
    use alforria_core::session::overflow::ModelLimits;
    use alforria_core::session::usage::{CacheCost, ModelCost};
    ResolvedModel {
        llm: LlmModel {
            id: "claude".to_string(),
            provider_id: "anthropic".to_string(),
            api_id: "claude".to_string(),
            api_npm: "@ai-sdk/anthropic".to_string(),
            temperature_capable: false,
            headers: Default::default(),
            options: Default::default(),
            context_limit: 200_000.0,
            output_limit: 100.0,
            output_token_max: None,
            family: None,
        },
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache: CacheCost {
                read: 0.0,
                write: 0.0,
            },
            tiers: Vec::new(),
            experimental_over_200k: None,
        },
        limits: ModelLimits {
            context: 200_000.0,
            input: None,
            output: 100.0,
        },
        output_token_max: None,
        media: Default::default(),
    }
}

struct FixedModels;

impl ModelSource for FixedModels {
    fn get_model<'a>(
        &'a self,
        _provider_id: &'a str,
        _model_id: &'a str,
        _session_id: &'a str,
    ) -> BoxFuture<'a, Result<ResolvedModel, LoopError>> {
        Box::pin(async { Ok(test_model()) })
    }

    fn get_small_model<'a>(
        &'a self,
        _provider_id: &'a str,
    ) -> BoxFuture<'a, Option<ResolvedModel>> {
        Box::pin(async { None })
    }
}

fn input_model_info(provider_id: &str, model_id: &str) -> alforria_schema::model::ModelInfo {
    use alforria_schema::model::{
        ModelApi, ModelCapabilities, ModelLimit, ModelRequest, ModelStatus, ModelTime,
    };
    alforria_schema::model::ModelInfo {
        id: model_id.to_string(),
        provider_id: provider_id.to_string(),
        family: None,
        name: model_id.to_string(),
        api: ModelApi::Aisdk {
            id: model_id.to_string(),
            package: "@ai-sdk/anthropic".to_string(),
            url: None,
            settings: None,
        },
        capabilities: ModelCapabilities {
            tools: true,
            input: Vec::new(),
            output: Vec::new(),
        },
        request: ModelRequest {
            headers: std::collections::BTreeMap::new(),
            body: serde_json::Map::new(),
            variant: None,
        },
        variants: Vec::new(),
        time: ModelTime { released: 0.0 },
        cost: Vec::new(),
        status: ModelStatus::Active,
        enabled: true,
        limit: ModelLimit {
            context: 1000,
            input: None,
            output: 100,
        },
    }
}

struct FixedInputModels;

impl InputModels for FixedInputModels {
    fn get_model<'a>(
        &'a self,
        provider_id: &'a str,
        model_id: &'a str,
    ) -> alforria_core::tool::def::BoxFuture<
        'a,
        Result<alforria_schema::model::ModelInfo, alforria_core::CoreError>,
    > {
        Box::pin(async move { Ok(input_model_info(provider_id, model_id)) })
    }

    fn default_model(
        &self,
    ) -> alforria_core::tool::def::BoxFuture<
        'static,
        Result<alforria_schema::model::ModelInfo, alforria_core::CoreError>,
    > {
        Box::pin(async { Ok(input_model_info("anthropic", "claude")) })
    }
}

/// A scripted text-only turn.
pub fn text_turn(text: &str) -> Script {
    vec![
        Ok(LlmEvent::TextStart {
            id: "t1".to_string(),
            provider_metadata: None,
        }),
        Ok(LlmEvent::TextDelta {
            id: "t1".to_string(),
            text: text.to_string(),
            provider_metadata: None,
        }),
        Ok(LlmEvent::TextEnd {
            id: "t1".to_string(),
            provider_metadata: None,
        }),
        Ok(LlmEvent::StepFinish {
            index: 0.0,
            reason: FinishReason::Stop,
            usage: None,
            provider_metadata: None,
        }),
        Ok(LlmEvent::Finish {
            reason: FinishReason::Stop,
            usage: None,
            provider_metadata: None,
        }),
    ]
}

/// A scripted turn that calls one tool.
pub fn tool_turn(id: &str, name: &str, input: Value) -> Script {
    vec![
        Ok(LlmEvent::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input,
            provider_executed: None,
            provider_metadata: None,
        }),
        Ok(LlmEvent::StepFinish {
            index: 0.0,
            reason: FinishReason::ToolCalls,
            usage: None,
            provider_metadata: None,
        }),
        Ok(LlmEvent::Finish {
            reason: FinishReason::ToolCalls,
            usage: None,
            provider_metadata: None,
        }),
    ]
}

/// Plays one script per agent-loop request (a request that offers tools),
/// then `"done"` forever; tool-less requests (titles, summaries) get a
/// canned line without consuming a script. Records what it was sent.
pub struct QueueLlm {
    scripts: Mutex<VecDeque<Script>>,
    pub requests: Mutex<Vec<StreamInput>>,
    /// What a tool-less request (a title, a goal review) is answered with.
    toolless: Mutex<String>,
}

impl QueueLlm {
    pub fn new(scripts: Vec<Script>) -> QueueLlm {
        QueueLlm {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
            toolless: Mutex::new("title".to_string()),
        }
    }

    /// Answer tool-less requests with `text` from now on.
    /// Queue more scripts behind the ones not yet played.
    pub fn extend(&self, scripts: Vec<Script>) {
        self.scripts.lock().unwrap().extend(scripts);
    }

    pub fn set_toolless(&self, text: &str) {
        *self.toolless.lock().unwrap() = text.to_string();
    }

    /// The agent-loop requests seen so far.
    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl LlmStream for QueueLlm {
    fn stream(&self, input: StreamInput) -> LlmEventStream {
        if input.tools.is_empty() {
            let text = self.toolless.lock().unwrap().clone();
            return Box::pin(futures::stream::iter(text_turn(&text)));
        }
        let input_tools = input.tools.clone();
        self.requests.lock().unwrap().push(input);
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text_turn("done"));
        // The seam sits above tool dispatch: run the scripted tool calls
        // through the production dispatcher, as the real stream does.
        alforria_core::session::llm::dispatch_tool_calls(
            Box::pin(futures::stream::iter(script)),
            input_tools,
        )
    }
}

/// The directories a fixture works with.
pub struct Dirs {
    /// The project the server is meant to serve.
    pub repo: PathBuf,
    /// A directory outside it.
    pub other: PathBuf,
    pub home: PathBuf,
}

pub struct Fixture {
    pub _dir: tempfile::TempDir,
    pub dirs: Dirs,
    pub ctx: Arc<ServerContext>,
    pub router: axum::Router,
    pub llm: Arc<QueueLlm>,
    pub paths: alforria_core::GlobalPaths,
}

/// Boot the production context under a temporary home, with `host_config`
/// written to the global config directory first and `password` as the
/// Basic password.
pub fn fixture_with(
    host_config: impl FnOnce(&Dirs) -> Value,
    password: Option<&str>,
    scripts: Vec<Script>,
) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    // Canonical, so directories compare equal to what the server resolves.
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let dirs = Dirs {
        repo: root.join("repo"),
        other: root.join("other"),
        home: root.join("home"),
    };
    for path in [&dirs.repo, &dirs.other, &dirs.home] {
        std::fs::create_dir_all(path).unwrap();
    }
    let paths = alforria_core::GlobalPaths::resolve_with(dirs.home.clone(), |_| None);
    std::fs::create_dir_all(&paths.config).unwrap();
    std::fs::write(
        paths.config.join("opencode.json"),
        serde_json::to_string_pretty(&host_config(&dirs)).unwrap(),
    )
    .unwrap();
    let llm = Arc::new(QueueLlm::new(scripts));
    let seams = EngineSeams {
        llm: Some(llm.clone()),
        models: Some(Arc::new(FixedModels)),
        input_models: Some(Arc::new(FixedInputModels)),
        share_http: None,
        env: alforria_server::engine::no_ambient_env(),
        ..EngineSeams::default()
    };
    let mut ctx =
        alforria_server::production_context(&ListenOptions::default(), paths.clone(), seams)
            .unwrap();
    assert_eq!(
        ctx.policy.config_error, None,
        "the host config must resolve"
    );
    {
        // Ambient OPENCODE_SERVER_PASSWORD must not leak into the fixture.
        let inner = Arc::get_mut(&mut ctx).expect("sole owner");
        inner.auth = AuthConfig::new("alforria", password.map(str::to_string))
            .with_bearer(inner.policy.bearer.clone());
    }
    Fixture {
        _dir: dir,
        dirs,
        router: routes::build_router(ctx.clone()),
        ctx,
        llm,
        paths,
    }
}

/// An Ed25519 key pair standing in for the control plane's signing key.
pub struct Signer {
    pair: ring::signature::Ed25519KeyPair,
}

impl Default for Signer {
    fn default() -> Self {
        Signer::new()
    }
}

impl Signer {
    pub fn new() -> Signer {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        Signer {
            pair: ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap(),
        }
    }

    /// The public key as config takes it (base64 of the raw 32 bytes).
    pub fn public_key(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.pair.public_key().as_ref())
    }

    pub fn public_key_bytes(&self) -> Vec<u8> {
        self.pair.public_key().as_ref().to_vec()
    }

    /// Sign `claims` under an `EdDSA` header.
    pub fn sign(&self, claims: Value) -> String {
        self.sign_with_header(json!({ "alg": "EdDSA", "typ": "JWT" }), claims)
    }

    /// Sign `claims` with this key under an arbitrary header.
    pub fn sign_with_header(&self, header: Value, claims: Value) -> String {
        let signed = format!(
            "{}.{}",
            b64url(header.to_string().as_bytes()),
            b64url(claims.to_string().as_bytes())
        );
        let signature = self.pair.sign(signed.as_bytes());
        format!("{signed}.{}", b64url(signature.as_ref()))
    }

    /// A `hook` token for `audience`, bound to `trigger` (the `trg` claim).
    pub fn hook_token(&self, audience: &str, trigger: &str) -> String {
        self.sign(json!({
            "aud": audience,
            "scope": "hook",
            "trg": trigger,
            "exp": now_secs() + 300,
            "sub": "test-hook",
        }))
    }

    /// A token for `audience` with `scope`, valid for five minutes.
    pub fn token(&self, audience: &str, scope: &str) -> String {
        self.sign(json!({
            "aud": audience,
            "scope": scope,
            "exp": now_secs() + 300,
            "sub": "test-user",
        }))
    }
}

pub fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// One request against the router. `auth` is the whole `Authorization`
/// value (`Bearer …` / `Basic …`).
pub async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    auth: Option<&str>,
    body: Option<Value>,
) -> Response<Body> {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(auth) = auth {
        request = request.header("authorization", auth);
    }
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
}

pub async fn body_string(response: Response<Body>) -> String {
    String::from_utf8_lossy(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .into_owned()
}

pub async fn body_json(response: Response<Body>) -> Value {
    let text = body_string(response).await;
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("not JSON ({err}): {text}"))
}

pub fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// The `Authorization` value for Basic credentials (the operator).
pub fn basic(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
    )
}

/// Encode a query value, including spaces, Unicode and reserved characters.
pub fn encode_query_value(value: &str) -> String {
    let url = reqwest::Url::parse_with_params("http://localhost/", [("value", value)]).unwrap();
    url.query()
        .unwrap()
        .strip_prefix("value=")
        .unwrap()
        .to_owned()
}
