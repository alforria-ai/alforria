//! Wire e2e: the `question` tool round-trip with a custom ("Type your
//! own answer") reply — the M8.8 harness against the real M6 server.

use std::sync::Arc;
use std::time::{Duration, Instant};

use alforria_core::session::llm::{LlmEventStream, LlmStream, StreamInput};
use alforria_core::session::prompt_input::Models as InputModels;
use alforria_core::session::r#loop::{LoopError, ModelSource, ResolvedModel};
use alforria_core::session::usage::{CacheCost, ModelCost};
use alforria_core::share::{ShareHttp, ShareHttpResponse};
use alforria_llm::schema::events::LlmEvent;
use alforria_llm::schema::ids::FinishReason;
use alforria_server::engine::EngineSeams;
use alforria_server::{routes, ListenOptions};
use alforria_tui::state::{self, App, Args, Effect, Msg};
use alforria_tui::transport::api::{HttpClientConfig, HttpServerApi, ServerApi};
use alforria_tui::transport::events::{Clock, EventSource, SseEventSource, TokioClock};
use alforria_tui::{execute_effect, ui};
use futures::future::BoxFuture;
use futures::StreamExt;

fn test_model() -> ResolvedModel {
    use alforria_core::session::llm::LlmModel;
    use alforria_core::session::overflow::ModelLimits;
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

/// Script queue — one script per LLM stream, popped in order. Tool calls
/// in a script dispatch through the runtime (`LlmStreamImpl` chains
/// `dispatch_tool_calls`; the raw script must do the same) — without the
/// dispatch, tool-call steps end with the tool never executed.
struct ScriptLlm {
    scripts: std::sync::Mutex<Vec<Vec<Result<LlmEvent, alforria_llm::LlmError>>>>,
}

async fn dispatch_one(
    tools: &[alforria_core::session::llm::LlmTool],
    id: &str,
    name: &str,
    input: serde_json::Value,
) -> Vec<Result<LlmEvent, alforria_llm::LlmError>> {
    use alforria_llm::schema::messages::ToolResultValue;

    let Some(tool) = tools.iter().find(|tool| tool.name == name) else {
        let message = format!("Unknown tool: {name}");
        return vec![
            Ok(LlmEvent::ToolError {
                id: id.to_string(),
                name: name.to_string(),
                message: message.clone(),
                error: None,
                provider_metadata: None,
            }),
            Ok(LlmEvent::ToolResult {
                id: id.to_string(),
                name: name.to_string(),
                result: ToolResultValue::Error {
                    value: serde_json::Value::String(message),
                },
                output: None,
                provider_executed: None,
                provider_metadata: None,
            }),
        ];
    };
    let Ok(output) = (tool.execute)(input, id.to_string()).await else {
        return Vec::new();
    };
    let mut value = serde_json::json!({
        "output": output.output,
        "title": output.title,
        "metadata": output.metadata,
    });
    if let Some(attachments) = output.attachments {
        value["attachments"] = serde_json::Value::Array(attachments);
    }
    vec![Ok(LlmEvent::ToolResult {
        id: id.to_string(),
        name: name.to_string(),
        result: ToolResultValue::Json { value },
        output: None,
        provider_executed: None,
        provider_metadata: None,
    })]
}

impl LlmStream for ScriptLlm {
    fn stream(&self, input: StreamInput) -> LlmEventStream {
        let mut queue = self.scripts.lock().unwrap();
        let script = if queue.is_empty() {
            Vec::new()
        } else {
            queue.remove(0)
        };
        let mut calls = Vec::new();
        for item in &script {
            if let Ok(LlmEvent::ToolCall {
                id,
                name,
                input,
                provider_executed,
                ..
            }) = item
            {
                if provider_executed.unwrap_or(false) {
                    continue;
                }
                calls.push((id.clone(), name.clone(), input.clone()));
            }
        }
        let provider = futures::stream::iter(script);
        let tools = input.tools.clone();
        let dispatch = futures::stream::unfold(
            (calls, tools, 0usize),
            |(mut calls, tools, mut counter)| async move {
                if calls.is_empty() {
                    return None;
                }
                let (id, name, input) = calls.remove(0);
                counter += 1;
                let events = dispatch_one(&tools, &id, &name, input).await;
                Some((
                    futures::stream::iter(events).boxed(),
                    (calls, tools, counter),
                ))
            },
        )
        .flat_map(|events| events)
        .boxed();
        provider.chain(dispatch).boxed()
    }
}

fn text_stream(text: &str) -> Vec<Result<LlmEvent, alforria_llm::LlmError>> {
    vec![
        Ok(LlmEvent::StepStart { index: 0.0 }),
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

fn question_tool_stream() -> Vec<Result<LlmEvent, alforria_llm::LlmError>> {
    vec![
        Ok(LlmEvent::StepStart { index: 0.0 }),
        Ok(LlmEvent::ToolCall {
            id: "call_q1".to_string(),
            name: "question".to_string(),
            input: serde_json::json!({
                "questions": [{
                    "question": "Pick a flavor",
                    "header": "Flavor",
                    "options": [
                        {"label": "Vanilla", "description": "Plain"},
                        {"label": "Chocolate", "description": "Dark"},
                    ],
                }],
            }),
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

struct StubShareHttp;

impl ShareHttp for StubShareHttp {
    fn post(
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

struct Server {
    base_url: String,
    directory: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

async fn spawn_server(scripts: Vec<Vec<Result<LlmEvent, alforria_llm::LlmError>>>) -> Server {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("repo")).unwrap();
    let paths = alforria_core::GlobalPaths::resolve(dir.path().to_path_buf());
    let seams = EngineSeams {
        llm: Some(Arc::new(ScriptLlm {
            scripts: std::sync::Mutex::new(scripts),
        })),
        models: Some(Arc::new(FixedModels)),
        input_models: Some(Arc::new(FixedInputModels)),
        share_http: Some(Arc::new(StubShareHttp)),
    };
    let mut ctx = alforria_server::production_context(&ListenOptions::default(), paths, seams)
        .expect("server context");
    Arc::get_mut(&mut ctx).expect("sole owner").auth =
        alforria_server::state::AuthConfig::new("alforria", None);
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

async fn drive(app: &Arc<tokio::sync::Mutex<App>>, api: &Arc<dyn ServerApi>, msg: Msg) {
    let effects = {
        let mut locked = app.lock().await;
        state::update(&mut locked, msg)
    };
    // The production event loop spawns each effect (`tokio::spawn`); a
    // question pending inside the session loop holds the prompt POST
    // open, so inline awaiting would deadlock the harness.
    for effect in effects {
        let app = Arc::clone(app);
        let api = Arc::clone(api);
        tokio::spawn(async move {
            execute_effect(&app, api, effect).await;
        });
    }
}

async fn pump_until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<alforria_tui::transport::events::BusEvent>>,
    app: &Arc<tokio::sync::Mutex<App>>,
    api: &Arc<dyn ServerApi>,
    mut check: impl FnMut(&App) -> bool,
    what: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if check(&*app.lock().await) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "pump: `{what}` not reached in 30s"
        );
        let batch = tokio::select! {
            batch = rx.recv() => batch.expect("sse stream alive"),
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                let locked = app.lock().await;
                let parts: Vec<String> = locked
                    .state
                    .sync
                    .part
                    .values()
                    .flatten()
                    .map(|part| {
                        serde_json::to_value(part)
                            .map(|v| v.to_string())
                            .unwrap_or_default()
                    })
                    .collect();
                eprintln!(
                    "pump timeout `{what}`: sessions={} messages={} parts={} questions={} route={:?}\nparts: {:#?}",
                    locked.state.sync.session.len(),
                    locked.state.sync.message.len(),
                    locked.state.sync.part.len(),
                    locked.state.sync.question.len(),
                    locked.state.route.data,
                    parts,
                );
                panic!("pump: `{what}` not reached in 30s")
            }
        };
        drive(app, api, Msg::Bus(batch)).await;
    }
}

async fn press(
    app: &Arc<tokio::sync::Mutex<App>>,
    api: &Arc<dyn ServerApi>,
    code: crossterm::event::KeyCode,
) {
    drive(
        app,
        api,
        Msg::Key(crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        )),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_e2e_question_custom_answer_round_trip() {
    std::env::set_var("LIBERTAI_API_KEY", "e2e-dummy-key");
    std::env::set_var("OPENCODE_ENABLE_QUESTION_TOOL", "1");

    let server = spawn_server(vec![
        text_stream("Warmup"),
        question_tool_stream(),
        text_stream("Answer received"),
    ])
    .await;

    let http_config = HttpClientConfig {
        base_url: server.base_url.clone(),
        directory: Some(server.directory.to_string_lossy().into_owned()),
        headers: Vec::new(),
    };
    let api: Arc<dyn ServerApi> = Arc::new(HttpServerApi::new(http_config.clone()).expect("api"));
    let source: Arc<dyn EventSource> = Arc::new(SseEventSource::new(http_config).expect("sse"));

    let state_dir = tempfile::tempdir().unwrap();
    let app = Arc::new(tokio::sync::Mutex::new(App::new(
        alforria_tui::config::TuiConfig::default(),
        Args::default(),
        Some(state_dir.path()),
    )));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    alforria_tui::transport::events::spawn_event_loop(
        Arc::clone(&source),
        Arc::new(TokioClock::new()) as Arc<dyn Clock>,
        tx,
    );

    // ---- bootstrap ----
    drive(&app, &api, Msg::Bus(Vec::new())).await;
    let bootstrap_app = Arc::clone(&app);
    let bootstrap_api = Arc::clone(&api);
    // Bootstrap must complete (or the provider list stays empty); run it
    // spawned like the production loop and wait for Complete.
    tokio::spawn(async move {
        execute_effect(
            &bootstrap_app,
            bootstrap_api,
            Effect::Bootstrap { fatal: true },
        )
        .await;
    });
    {
        let providers_deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let (done, question_pending) = {
                let locked = app.lock().await;
                (
                    !locked.state.sync.provider.is_empty(),
                    locked
                        .state
                        .sync
                        .question
                        .values()
                        .map(Vec::len)
                        .sum::<usize>()
                        > 0,
                )
            };
            if question_pending {
                break;
            }
            assert!(
                Instant::now() < providers_deadline,
                "providers landed after bootstrap"
            );
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    // ---- prompt that triggers the question tool ----
    {
        let effects = {
            let mut locked = app.lock().await;
            locked.ui.prompt.textarea.set_text("Ask me");
            alforria_tui::state::prompt::submit(&mut locked)
        };
        for effect in effects {
            let app = Arc::clone(&app);
            let api = Arc::clone(&api);
            tokio::spawn(async move {
                execute_effect(&app, api, effect).await;
            });
        }
    }

    // Wait for the question to be pending.
    pump_until(
        &mut rx,
        &app,
        &api,
        |a| a.state.sync.question.values().map(Vec::len).sum::<usize>() > 0,
        "question asked",
    )
    .await;

    // ---- pick the wildcard row and type the custom answer ----
    press(&app, &api, crossterm::event::KeyCode::Down).await;
    press(&app, &api, crossterm::event::KeyCode::Down).await;
    press(&app, &api, crossterm::event::KeyCode::Enter).await;
    {
        let locked = app.lock().await;
        eprintln!(
            "after keys: question.request_id={:?} selected={} editing={} visible={:?} dialogs={}",
            locked.ui.question.request_id,
            locked.ui.question.selected,
            locked.ui.question.editing,
            alforria_tui::ui::session::question::visible(&locked).map(|q| q.id),
            locked.ui.dialogs.stack.len()
        );
        assert!(locked.ui.question.editing, "wildcard row starts editing");
    }
    for char in "salted caramel".chars() {
        press(&app, &api, crossterm::event::KeyCode::Char(char)).await;
    }
    press(&app, &api, crossterm::event::KeyCode::Enter).await;

    // ---- the reply resolves and the session continues ----
    pump_until(
        &mut rx,
        &app,
        &api,
        |a| {
            a.state.sync.part.values().flatten().any(|part| {
                serde_json::to_value(part)
                    .map(|v| v.to_string().contains("Answer received"))
                    .unwrap_or(false)
            })
        },
        "post-answer continuation",
    )
    .await;

    // The pending question is gone.
    let pending: usize = {
        let locked = app.lock().await;
        locked.state.sync.question.values().map(Vec::len).sum()
    };
    assert_eq!(pending, 0, "question cleared after the custom reply");

    // ---- the rendered frame shows the custom answer tool result ----
    let mut locked = app.lock().await;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).expect("terminal");
    terminal
        .draw(|frame| ui::view(&mut locked, frame))
        .expect("draw");
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        text.contains("salted caramel"),
        "transcript renders the custom answer: {text}"
    );
}
