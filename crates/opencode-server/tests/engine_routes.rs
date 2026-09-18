//! M7.2 acceptance: the production engine wiring — per-instance engine
//! boot through the production instance factory, a mock-LLM end-to-end
//! prompt through the HTTP surface (`POST /session/:id/message`), the
//! `/experimental/tool` registry listing and `sessionBackground`
//! promotion.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::future::BoxFuture;
use opencode_core::session::llm::{LlmEventStream, LlmStream, StreamInput};
use opencode_core::session::prompt_input::Models as InputModels;
use opencode_core::session::r#loop::{LoopError, ModelSource, ResolvedModel};
use opencode_llm::schema::events::LlmEvent;
use opencode_llm::schema::ids::FinishReason;
use opencode_llm::LlmError;
use opencode_server::engine::EngineSeams;
use opencode_server::{routes, ListenOptions};
use tower::ServiceExt;

// -----------------------------------------------------------------------
// Mock provider runtime (M5.8 harness shape — fixed models + scripted LLM)
// -----------------------------------------------------------------------

fn test_model() -> ResolvedModel {
    use opencode_core::session::llm::LlmModel;
    use opencode_core::session::overflow::ModelLimits;
    use opencode_core::session::usage::{CacheCost, ModelCost};
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

fn input_model_info(provider_id: &str, model_id: &str) -> opencode_schema::model::ModelInfo {
    use opencode_schema::model::{
        ModelApi, ModelCapabilities, ModelLimit, ModelRequest, ModelStatus, ModelTime,
    };
    opencode_schema::model::ModelInfo {
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
    ) -> opencode_core::tool::def::BoxFuture<
        'a,
        Result<opencode_schema::model::ModelInfo, opencode_core::CoreError>,
    > {
        Box::pin(async move { Ok(input_model_info(provider_id, model_id)) })
    }

    fn default_model(
        &self,
    ) -> opencode_core::tool::def::BoxFuture<
        'static,
        Result<opencode_schema::model::ModelInfo, opencode_core::CoreError>,
    > {
        Box::pin(async { Ok(input_model_info("anthropic", "claude")) })
    }
}

/// A scripted text-only turn (`text_stream`, M5.8 harness).
fn text_stream(text: &str) -> Vec<Result<LlmEvent, LlmError>> {
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

struct ScriptLlm {
    script: Vec<Result<LlmEvent, LlmError>>,
}

impl LlmStream for ScriptLlm {
    fn stream(&self, _input: StreamInput) -> LlmEventStream {
        Box::pin(futures::stream::iter(self.script.clone()))
    }
}

// -----------------------------------------------------------------------
// Fixture
// -----------------------------------------------------------------------

struct Fixture {
    _dir: tempfile::TempDir,
    directory: std::path::PathBuf,
    router: axum::Router,
}

fn fixture(llm_text: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("repo")).unwrap();
    let paths = opencode_core::GlobalPaths::resolve(dir.path().to_path_buf());
    let seams = EngineSeams {
        llm: Some(Arc::new(ScriptLlm {
            script: text_stream(llm_text),
        })),
        models: Some(Arc::new(FixedModels)),
        input_models: Some(Arc::new(FixedInputModels)),
    };
    let ctx = opencode_server::production_context(&ListenOptions::default(), paths, seams).unwrap();
    Fixture {
        directory: dir.path().join("repo"),
        _dir: dir,
        router: routes::build_router(ctx),
    }
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: &str,
) -> axum::http::Response<Body> {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn body_string(response: axum::http::Response<Body>) -> String {
    String::from_utf8(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

fn urlencode(value: &str) -> String {
    value.replace('/', "%2F")
}

/// Create a session through the real HTTP surface (`?directory=` boots the
/// instance — and with it the production engine).
async fn create_session(f: &Fixture) -> serde_json::Value {
    let response = send(
        &f.router,
        "POST",
        &format!(
            "/session?directory={}",
            urlencode(&f.directory.display().to_string())
        ),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_str(&body_string(response).await).unwrap()
}

// -----------------------------------------------------------------------
// Acceptance
// -----------------------------------------------------------------------

/// Mock-LLM end-to-end prompt through the HTTP surface
/// (`POST /session/:id/message`, M7.2 acceptance check 1).
#[tokio::test]
async fn production_prompt_e2e() {
    let f = fixture("Hello from the mock");
    let session = create_session(&f).await;
    let session_id = session["id"].as_str().unwrap().to_string();

    let response = send(
        &f.router,
        "POST",
        &format!(
            "/session/{}/message?directory={}",
            session_id,
            urlencode(&f.directory.display().to_string())
        ),
        r#"{"parts": [{"type": "text", "text": "hi"}]}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["info"]["role"], "assistant");
    let text = serde_json::to_string(&body["parts"]).unwrap();
    assert!(
        text.contains("Hello from the mock"),
        "assistant parts carry the mock text: {text}"
    );
}

/// Tool listing from `/experimental/tool` once the registry is wired
/// (M7.2 acceptance check 4).
#[tokio::test]
async fn experimental_tool_lists_production_registry() {
    let f = fixture("unused");
    // Boot the instance via any location-resolved route first.
    let _ = create_session(&f).await;

    let response = send(
        &f.router,
        "GET",
        &format!(
            "/experimental/tool?provider=anthropic&model=claude&directory={}",
            urlencode(&f.directory.display().to_string())
        ),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let tools: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    let ids: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["id"].as_str().unwrap())
        .collect();
    // registry.ts:240-249 order. `apply_patch` is gpt-models-only and
    // `websearch` needs a search provider (registry.ts:179-183), so a
    // claude model without env keys lists neither.
    assert_eq!(
        ids,
        vec![
            "invalid",
            "question",
            "bash",
            "read",
            "glob",
            "grep",
            "edit",
            "write",
            "task",
            "webfetch",
            "todowrite",
            "skill",
        ]
    );
}

/// `/experimental/tool/ids` (handlers/experimental.ts:101-103).
#[tokio::test]
async fn experimental_tool_ids() {
    let f = fixture("unused");
    let _ = create_session(&f).await;
    let response = send(
        &f.router,
        "GET",
        &format!(
            "/experimental/tool/ids?directory={}",
            urlencode(&f.directory.display().to_string())
        ),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    let ids: Vec<&str> = serde_json::from_str(&body).unwrap();
    assert!(ids.contains(&"bash"));
    assert!(ids.contains(&"task"));
}

/// `sessionBackground` false when the flag is off (M7.2 acceptance
/// check 3), and `capabilities` reports the flag.
#[tokio::test]
async fn session_background_disabled_by_default() {
    let f = fixture("unused");
    let session = create_session(&f).await;
    let session_id = session["id"].as_str().unwrap().to_string();

    let response = send(&f.router, "GET", "/experimental/capabilities", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let capabilities: serde_json::Value =
        serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(
        capabilities["backgroundSubagents"],
        serde_json::Value::Bool(false)
    );

    let response = send(
        &f.router,
        "POST",
        &format!(
            "/experimental/session/{}/background?directory={}",
            session_id,
            urlencode(&f.directory.display().to_string())
        ),
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let promoted: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(promoted, serde_json::Value::Bool(false));
}
