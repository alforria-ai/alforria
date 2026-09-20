//! M8.8 wire e2e: the production TUI seams — `HttpServerApi`,
//! `SseEventSource`, `state::update`, `execute_effect`, and the
//! rendered transcript — against the real M6 server over TCP (the
//! M5.8 mock-LLM engine seams; never the network).

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use opencode_core::session::llm::{LlmEventStream, LlmStream, StreamInput};
use opencode_core::session::prompt_input::Models as InputModels;
use opencode_core::session::r#loop::{LoopError, ModelSource, ResolvedModel};
use opencode_core::session::usage::{CacheCost, ModelCost};
use opencode_core::share::{ShareHttp, ShareHttpResponse};
use opencode_llm::schema::events::LlmEvent;
use opencode_llm::schema::ids::FinishReason;
use opencode_llm::LlmError;
use opencode_server::engine::EngineSeams;
use opencode_server::{routes, ListenOptions};
use opencode_tui::state::{self, App, Args, Effect, Msg};
use opencode_tui::transport::api::{HttpClientConfig, HttpServerApi, Location, ServerApi};
use opencode_tui::transport::events::{Clock, EventSource, SseEventSource, TokioClock};
use opencode_tui::{execute_effect, ui};

// -----------------------------------------------------------------------
// Mock provider runtime (M5.8 harness shape — fixed models + scripted LLM)
// -----------------------------------------------------------------------

fn test_model() -> ResolvedModel {
    use opencode_core::session::llm::LlmModel;
    use opencode_core::session::overflow::ModelLimits;
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

/// The fake share REST transport — the M7.5 `StubHttp` response shape,
/// so the share flow runs end-to-end without the network.
struct StubShareHttp;

impl ShareHttp for StubShareHttp {
    fn post(
        &self,
        url: &str,
        _headers: &[(String, String)],
        _body: &str,
    ) -> Result<ShareHttpResponse, String> {
        if url.ends_with("/sync") {
            return Ok(ShareHttpResponse {
                status: 200,
                body: String::new(),
            });
        }
        Ok(ShareHttpResponse {
            status: 200,
            body: r#"{"id":"shr_1","url":"https://shr.test/1","secret":"s3cret"}"#.to_string(),
        })
    }

    fn delete(
        &self,
        _url: &str,
        _headers: &[(String, String)],
        _body: &str,
    ) -> Result<ShareHttpResponse, String> {
        Ok(ShareHttpResponse {
            status: 200,
            body: String::new(),
        })
    }
}

// -----------------------------------------------------------------------
// Server fixture — the real router over TCP, on an ephemeral port.
// -----------------------------------------------------------------------

struct Server {
    base_url: String,
    directory: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

async fn spawn_server(llm_text: &'static str) -> Server {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("repo")).unwrap();
    let paths = opencode_core::GlobalPaths::resolve(dir.path().to_path_buf());
    let seams = EngineSeams {
        llm: Some(Arc::new(ScriptLlm {
            script: text_stream(llm_text),
        })),
        models: Some(Arc::new(FixedModels)),
        input_models: Some(Arc::new(FixedInputModels)),
        share_http: Some(Arc::new(StubShareHttp)),
    };
    let mut ctx = opencode_server::production_context(&ListenOptions::default(), paths, seams)
        .expect("server context");
    // Ambient auth env (an exported OPENCODE_SERVER_PASSWORD) must not
    // leak into the fixture (the M7 review-panel fix).
    Arc::get_mut(&mut ctx).expect("sole owner").auth =
        opencode_server::state::AuthConfig::new("opencode", None);
    let router = routes::build_router(ctx);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("server");
    });
    Server {
        base_url: format!("http://127.0.0.1:{port}"),
        directory: dir.path().join("repo"),
        _dir: dir,
    }
}

/// Drives one message through `state::update` and runs every effect it
/// produces (the event loop's spawn arm, inlined for determinism).
async fn drive(app: &Arc<tokio::sync::Mutex<App>>, api: &Arc<dyn ServerApi>, msg: Msg) {
    let effects = {
        let mut locked = app.lock().await;
        state::update(&mut locked, msg)
    };
    for effect in effects {
        execute_effect(app, Arc::clone(api), effect).await;
    }
}

/// Applies SSE batches until the predicate holds (the SSE pump arm of
/// the event loop, run inline).
async fn pump_until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<opencode_tui::transport::events::BusEvent>>,
    app: &Arc<tokio::sync::Mutex<App>>,
    api: &Arc<dyn ServerApi>,
    check: impl Fn(&App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if check(&*app.lock().await) {
            return;
        }
        assert!(Instant::now() < deadline, "pump: condition not met in 30s");
        let batch = tokio::select! {
            batch = rx.recv() => batch.expect("sse stream alive"),
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                panic!("pump: condition not met in 30s")
            }
        };
        let _ = drive(app, api, Msg::Bus(batch)).await;
    }
}

// -----------------------------------------------------------------------
// The scripted session: bootstrap -> prompt -> streamed parts -> share
// -> export, asserted on state + rendered frames.
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_e2e_bootstrap_prompt_stream_share_export() {
    let server = spawn_server("Hello from the mock").await;

    let http_config = HttpClientConfig {
        base_url: server.base_url.clone(),
        directory: Some(server.directory.to_string_lossy().into_owned()),
        headers: Vec::new(),
    };
    let api: Arc<dyn ServerApi> = Arc::new(HttpServerApi::new(http_config.clone()).expect("api"));
    let source: Arc<dyn EventSource> = Arc::new(SseEventSource::new(http_config).expect("sse"));

    let state_dir = tempfile::tempdir().unwrap();
    let app = Arc::new(tokio::sync::Mutex::new(App::new(
        opencode_tui::config::TuiConfig::default(),
        Args::default(),
        Some(state_dir.path()),
    )));

    // The SSE pump — batches flow through `state::update`.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    opencode_tui::transport::events::spawn_event_loop(
        Arc::clone(&source),
        Arc::new(TokioClock::new()) as Arc<dyn Clock>,
        tx,
    );

    // ---- bootstrap ------------------------------------------------
    drive(&app, &api, Msg::Bus(Vec::new())).await;
    execute_effect(&app, Arc::clone(&api), Effect::Bootstrap { fatal: true }).await;
    {
        let app = app.lock().await;
        assert!(
            matches!(
                app.state.sync.status,
                Some(opencode_tui::state::sync::SyncStatus::Complete)
            ),
            "bootstrap reaches Complete"
        );
        assert!(!app.state.sync.provider.is_empty(), "providers landed");
    }

    // ---- prompt ---------------------------------------------------
    {
        let effects = {
            let mut locked = app.lock().await;
            locked.ui.prompt.textarea.set_text("Say the thing");
            opencode_tui::state::prompt::submit(&mut locked)
        };
        for effect in effects {
            execute_effect(&app, Arc::clone(&api), effect).await;
        }
    }

    // Wait for the streamed assistant text to land in the part store,
    // applying every SSE batch along the way.
    let text_landed =
        |parts: &std::collections::BTreeMap<String, Vec<opencode_schema::session_v1::V1Part>>| {
            parts.values().flatten().any(|part| {
                serde_json::to_value(part)
                    .map(|v| v.to_string().contains("Hello from the mock"))
                    .unwrap_or(false)
            })
        };
    let app2 = Arc::clone(&app);
    pump_until(&mut rx, &app, &api, move |a| {
        text_landed(&a.state.sync.part)
    })
    .await;
    let _ = &app2;

    // ---- rendered frame -------------------------------------------
    {
        let mut app = app.lock().await;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| ui::view(&mut app, frame))
            .expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains("Hello from the mock"),
            "transcript renders the streamed text"
        );
        assert!(
            text.contains("Say the thing"),
            "transcript renders the user prompt"
        );
    }

    // The route moved onto the created session.
    let session_id = {
        let app = app.lock().await;
        let route = match &app.state.route.data {
            opencode_tui::state::route::Route::Session { session_id, .. } => session_id.clone(),
            opencode_tui::state::route::Route::Home { .. } => {
                panic!("route should have moved to the session")
            }
            opencode_tui::state::route::Route::Plugin { .. } => {
                panic!("route should have moved to the session")
            }
        };
        assert!(!route.is_empty());
        route
    };

    // ---- rename ---------------------------------------------------
    api.session_rename(&Location::default(), &session_id, "Renamed over the wire")
        .await
        .expect("rename");
    pump_until(&mut rx, &app, &api, |a| {
        a.state
            .sync
            .session
            .iter()
            .any(|s| s.id == session_id && s.title == "Renamed over the wire")
    })
    .await;

    // ---- share ----------------------------------------------------
    api.session_share(&Location::default(), &session_id)
        .await
        .expect("share");
    pump_until(&mut rx, &app, &api, |a| {
        a.state.sync.session.iter().any(|s| {
            s.id == session_id
                && s.share
                    .as_ref()
                    .is_some_and(|share| share.url.contains("shr.test"))
        })
    })
    .await;

    // ---- export ---------------------------------------------------
    let export_dir = tempfile::tempdir().unwrap();
    {
        let previous = std::env::set_current_dir(export_dir.path());
        let _ = previous;

        let effects = vec![Effect::SessionExport {
            filename: "transcript.md".to_string(),
            thinking: false,
            tool_details: false,
            assistant_metadata: false,
            open_without_saving: false,
        }];
        for effect in effects {
            execute_effect(&app, Arc::clone(&api), effect).await;
        }
        let exported = std::fs::read_to_string(export_dir.path().join("transcript.md"));
        assert!(exported.is_ok(), "export writes the transcript file");
        assert!(
            exported
                .as_deref()
                .unwrap_or_default()
                .contains("Hello from the mock"),
            "exported transcript contains the streamed text"
        );
        std::env::set_current_dir("/").ok();
    }
    let _ = server.base_url;
}
