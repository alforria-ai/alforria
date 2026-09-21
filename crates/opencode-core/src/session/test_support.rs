//! Shared M5.6 test harness — a scripted [`LlmStream`], a static
//! [`ModelSource`], fixed clock/permission doubles and a tempdir
//! service bundle, so compaction/summary/revert tests can drive the real
//! stores against SQLite (spec §6).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use opencode_llm::schema::errors::LlmError;
use opencode_llm::schema::events::LlmEvent;
use opencode_schema::session_v1::{
    AssistantTime, UserTime, V1Message, V1Path, V1StepTokens, V1TokenCache, V1UserModel,
};
use serde_json::Value;

use crate::session::agents::AgentRegistryInput;
use crate::session::llm::{LlmEventStream, LlmModel, LlmStream, StreamInput};
use crate::session::message::WithParts;
use crate::session::overflow::ModelLimits;
use crate::session::processor::{AskPermission, PermissionAsk, PermissionAskError};
use crate::session::r#loop::{ModelSource, ResolvedModel};
use crate::session::run_state::{BackgroundJobInfo, BackgroundJobs};
use crate::session::snapshot::InMemorySnapshot;
use crate::session::store::{CreateInput, SessionContext, SessionStore};
use crate::session::usage::{CacheCost, ModelCost};
use crate::session::SessionServices;
use crate::Clock;

// ---------------------------------------------------------------------------
// Clock / jobs / permission doubles
// ---------------------------------------------------------------------------

pub(crate) struct FixedClock;

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        1_761_000_000_000
    }
}

pub(crate) struct NoJobs;

impl BackgroundJobs for NoJobs {
    fn list(&self) -> Result<Vec<BackgroundJobInfo>, crate::CoreError> {
        Ok(Vec::new())
    }
    fn cancel(&self, _: &str) -> Result<(), crate::CoreError> {
        Ok(())
    }
}

/// Auto-approving permission seam.
pub(crate) struct AllowAll;

impl AskPermission for AllowAll {
    fn ask<'a>(
        &'a self,
        _request: PermissionAsk,
    ) -> futures::future::BoxFuture<'a, Result<(), PermissionAskError>> {
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Mock LLM
// ---------------------------------------------------------------------------

/// A scripted [`LlmStream`]: each `stream()` call pops the next script.
pub(crate) struct MockLlm {
    script: Mutex<VecDeque<Vec<Result<LlmEvent, LlmError>>>>,
    inputs: Mutex<Vec<StreamInput>>,
}

pub(crate) type MockScript = Vec<Vec<Result<LlmEvent, LlmError>>>;

impl MockLlm {
    pub(crate) fn new(script: MockScript) -> Arc<Self> {
        Arc::new(MockLlm {
            script: Mutex::new(script.into_iter().collect()),
            inputs: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn calls(&self) -> usize {
        self.inputs.lock().unwrap().len()
    }
}

impl LlmStream for MockLlm {
    fn stream(&self, input: StreamInput) -> LlmEventStream {
        self.inputs.lock().unwrap().push(input.clone());
        let events = self.script.lock().unwrap().pop_front().unwrap_or_default();
        futures::stream::iter(events).boxed()
    }
}

// ---------------------------------------------------------------------------
// Model source
// ---------------------------------------------------------------------------

pub(crate) fn test_model() -> ResolvedModel {
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

/// `getModel` / `getSmallModel` stub returning one fixed model.
pub(crate) struct StaticModels {
    pub(crate) model: ResolvedModel,
}

impl ModelSource for StaticModels {
    fn get_model<'a>(
        &'a self,
        _provider_id: &'a str,
        _model_id: &'a str,
        _session_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<ResolvedModel, crate::session::r#loop::LoopError>>
    {
        Box::pin(async { Ok(self.model.clone()) })
    }

    fn get_small_model<'a>(
        &'a self,
        _provider_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Option<ResolvedModel>> {
        Box::pin(async { None })
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

pub(crate) struct Harness {
    pub(crate) temp: crate::storage::test_support::TempDir,
    pub(crate) services: SessionServices,
    pub(crate) config: crate::config::schema::Config,
    pub(crate) llm: Arc<MockLlm>,
    pub(crate) snapshot: Arc<InMemorySnapshot>,
    pub(crate) worktree: std::path::PathBuf,
}

pub(crate) fn harness(name: &str, script: MockScript) -> Harness {
    harness_with_config(name, script, serde_json::json!({}))
}

pub(crate) fn harness_with_config(name: &str, script: MockScript, config: Value) -> Harness {
    let temp = crate::storage::test_support::TempDir::new(name);
    let worktree = temp.path().join("repo");
    std::fs::create_dir_all(&worktree).unwrap();
    let config: crate::config::schema::Config =
        serde_json::from_value(config).expect("valid config");
    let agent_input = AgentRegistryInput {
        config: config.clone(),
        skill_dirs: Vec::new(),
        reference_dirs: Vec::new(),
        worktree: worktree.clone(),
        data_dir: temp.path().to_path_buf(),
        tmp_dir: temp.path().to_path_buf(),
        home: temp.path().to_path_buf(),
    };
    let services = SessionServices::new(
        Arc::new(crate::storage::Storage::open(temp.path().join("db.sqlite")).unwrap()),
        Arc::new(NoJobs),
        Arc::new(FixedClock),
        &agent_input,
    );
    services
        .storage
        .with_connection(|conn| {
            conn.execute(
                "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["global", "/repo", "[]", 1, 1],
            )
        })
        .unwrap();
    Harness {
        snapshot: InMemorySnapshot::new(worktree.clone()),
        temp,
        services,
        config,
        llm: MockLlm::new(script),
        worktree,
    }
}

pub(crate) fn static_models() -> StaticModels {
    StaticModels {
        model: test_model(),
    }
}

pub(crate) fn create_session(
    store: &SessionStore,
    worktree: &std::path::Path,
) -> opencode_schema::session_v1::V1SessionInfo {
    store
        .create(
            &SessionContext {
                project_id: "global".to_string(),
                directory: worktree.to_path_buf(),
                worktree: worktree.to_path_buf(),
                workspace_id: None,
            },
            &CreateInput::default(),
        )
        .unwrap()
}

// ---------------------------------------------------------------------------
// Message / part builders
// ---------------------------------------------------------------------------

pub(crate) fn user_message(session: &str, id: &str, created: f64) -> V1Message {
    V1Message::User {
        id: id.to_string(),
        session_id: session.to_string(),
        time: UserTime { created },
        format: None,
        summary: None,
        agent: "build".to_string(),
        model: V1UserModel {
            provider_id: "anthropic".to_string(),
            model_id: "claude".to_string(),
            variant: None,
        },
        system: None,
        tools: None,
    }
}

pub(crate) fn assistant_message(
    session: &str,
    id: &str,
    parent: &str,
    created: u64,
    summary: Option<bool>,
    finish: Option<String>,
) -> V1Message {
    V1Message::Assistant {
        id: id.to_string(),
        session_id: session.to_string(),
        time: AssistantTime {
            created,
            completed: None,
        },
        error: None,
        parent_id: parent.to_string(),
        model_id: "claude".to_string(),
        provider_id: "anthropic".to_string(),
        mode: "primary".to_string(),
        agent: "build".to_string(),
        path: V1Path {
            cwd: "/repo".to_string(),
            root: "/repo".to_string(),
        },
        summary,
        cost: 0.0,
        tokens: V1StepTokens {
            total: None,
            input: 0.0,
            output: 0.0,
            reasoning: 0.0,
            cache: V1TokenCache {
                read: 0.0,
                write: 0.0,
            },
        },
        structured: None,
        variant: None,
        finish,
    }
}

/// A text part carrying the given `text`.
pub(crate) fn text_part(
    session: &str,
    message: &str,
    id: &str,
    text: &str,
) -> opencode_schema::session_v1::V1Part {
    opencode_schema::session_v1::V1Part::Text {
        id: id.to_string(),
        session_id: session.to_string(),
        message_id: message.to_string(),
        text: text.to_string(),
        synthetic: None,
        ignored: None,
        time: None,
        metadata: None,
    }
}

/// `WithParts` helper.
pub(crate) fn with_parts(
    info: V1Message,
    parts: Vec<opencode_schema::session_v1::V1Part>,
) -> WithParts {
    WithParts { info, parts }
}

/// A compaction part.
pub(crate) fn compaction_part(
    session: &str,
    message: &str,
    id: &str,
    auto: bool,
    overflow: Option<bool>,
    tail: Option<&str>,
) -> opencode_schema::session_v1::V1Part {
    opencode_schema::session_v1::V1Part::Compaction {
        id: id.to_string(),
        session_id: session.to_string(),
        message_id: message.to_string(),
        auto,
        overflow,
        tail_start_id: tail.map(|tail| tail.to_string()),
    }
}

/// A minimal text stream: text + finish.
pub(crate) fn text_stream(text: &str) -> Vec<Result<LlmEvent, LlmError>> {
    use opencode_llm::schema::ids::FinishReason;
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

// ---------------------------------------------------------------------------
// M5.7 production engine harness
// ---------------------------------------------------------------------------

use crate::session::prompt::{SessionPrompt, SessionPromptDeps, ShellInput, COMMAND_EXECUTED};
use crate::session::subtask::{SessionSubtask, SubtaskDeps};
use crate::session::task_ops::ProductionTaskOps;

/// A scripted [`LlmStream`] that emulates the runtime's tool dispatch:
/// queued tool calls execute after the provider events end, and the
/// results flow back as `ToolResult`/`ToolError` events.
pub(crate) struct DispatchLlm {
    script: Mutex<VecDeque<Vec<Result<LlmEvent, LlmError>>>>,
    inputs: Mutex<Vec<StreamInput>>,
    hang: std::sync::atomic::AtomicBool,
}

impl DispatchLlm {
    pub(crate) fn new(script: MockScript) -> Arc<Self> {
        Arc::new(DispatchLlm {
            script: Mutex::new(script.into_iter().collect()),
            inputs: Mutex::new(Vec::new()),
            hang: std::sync::atomic::AtomicBool::new(false),
        })
    }

    // M5.8 (mock-LLM full-loop E2E) exercises the abort path.
    #[allow(dead_code)]
    pub(crate) fn lock_hang(&self) {
        self.hang.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn calls(&self) -> usize {
        self.inputs.lock().unwrap().len()
    }

    pub(crate) fn input(&self, index: usize) -> StreamInput {
        self.inputs.lock().unwrap()[index].clone()
    }
}

/// Emulate the runtime's tool dispatch (loop.rs mock_dispatch).
/// Emulate the runtime's tool dispatch (llm.rs `dispatch_one`): failures
/// surface as `tool-error` (with the serialized error class) followed by
/// an error `tool-result` (llm.rs:715-769).
async fn dispatch_one(
    tools: &[crate::session::llm::LlmTool],
    id: &str,
    name: &str,
    input: Value,
) -> Vec<LlmEvent> {
    use opencode_llm::schema::events::LlmEvent;
    let Some(tool) = tools.iter().find(|tool| tool.name == name) else {
        let message = format!("Unknown tool: {name}");
        return vec![
            LlmEvent::ToolError {
                id: id.to_string(),
                name: name.to_string(),
                message: message.clone(),
                error: None,
                provider_metadata: None,
            },
            LlmEvent::ToolResult {
                id: id.to_string(),
                name: name.to_string(),
                result: opencode_llm::schema::messages::ToolResultValue::Error {
                    value: Value::String(message),
                },
                output: None,
                provider_executed: None,
                provider_metadata: None,
            },
        ];
    };
    match (tool.execute)(input, id.to_string()).await {
        Ok(output) => {
            let mut value = serde_json::json!({
                "output": output.output,
                "title": output.title,
                "metadata": output.metadata,
            });
            if let Some(attachments) = output.attachments {
                value["attachments"] = serde_json::to_value(&attachments).unwrap_or(Value::Null);
            }
            vec![LlmEvent::ToolResult {
                id: id.to_string(),
                name: name.to_string(),
                result: opencode_llm::schema::messages::ToolResultValue::Json { value },
                output: None,
                provider_executed: None,
                provider_metadata: None,
            }]
        }
        Err(failure) => {
            let message = failure.message();
            vec![
                LlmEvent::ToolError {
                    id: id.to_string(),
                    name: name.to_string(),
                    message: message.clone(),
                    error: Some(failure.error_value()),
                    provider_metadata: None,
                },
                LlmEvent::ToolResult {
                    id: id.to_string(),
                    name: name.to_string(),
                    result: opencode_llm::schema::messages::ToolResultValue::Error {
                        value: Value::String(message),
                    },
                    output: None,
                    provider_executed: None,
                    provider_metadata: None,
                },
            ]
        }
    }
}

/// The stream state: Pending provider events, in-flight settlements and
/// settled events.
///
/// Tool calls are dispatched EAGERLY with the provider event yielded
/// FIRST, exactly like the TS native runtime
/// (`FiberSet.run(settlements, { startImmediately: true })` + result
/// queue, native-runtime.ts:107-127): the processor sees the `tool-call`
/// event (creating the part) before the settlement executes, and the
/// settlement events are delivered while the remaining provider events
/// stream. Eager execution is what makes step-finish snapshots observe
/// tool edits (patch parts).
fn dispatch_stream(
    provider: VecDeque<Result<LlmEvent, LlmError>>,
    tools: Vec<crate::session::llm::LlmTool>,
    hang: bool,
) -> LlmEventStream {
    type Pending = Option<(String, String, Value)>;
    futures::stream::unfold(
        (provider, tools, VecDeque::new(), Pending::None, hang),
        |(mut provider, tools, mut queue, pending, hang)| async move {
            if let Some(event) = queue.pop_front() {
                return Some((Ok(event), (provider, tools, queue, pending, hang)));
            }
            if let Some((id, name, input)) = pending {
                let events = dispatch_one(&tools, &id, &name, input).await;
                queue.extend(events);
                let event = queue.pop_front().expect("dispatch produced events");
                return Some((Ok(event), (provider, tools, queue, None, hang)));
            }
            let Some(item) = provider.pop_front() else {
                if hang {
                    futures::future::pending::<()>().await;
                }
                return None;
            };
            let mut pending: Pending = None;
            if let Ok(LlmEvent::ToolCall {
                id,
                name,
                input,
                provider_executed,
                ..
            }) = &item
            {
                if *provider_executed != Some(true) {
                    pending = Some((id.clone(), name.clone(), input.clone()));
                }
            }
            Some((item, (provider, tools, queue, pending, hang)))
        },
    )
    .boxed()
}

impl LlmStream for DispatchLlm {
    fn stream(&self, input: StreamInput) -> LlmEventStream {
        self.inputs.lock().unwrap().push(input.clone());
        let events = self.script.lock().unwrap().pop_front().unwrap_or_default();
        let hang = self.hang.load(std::sync::atomic::Ordering::SeqCst);
        dispatch_stream(events.into(), input.tools, hang)
    }
}

/// The full M5.7 production engine: production `TaskOps`, the subtask
/// driver, compaction/summary/revert services and the prompt facade
/// wired over a mock LLM.
///
/// `temp` must stay alive for the engine's lifetime (it owns the
/// sqlite worktree); the remaining fields are M5.8 harness surface.
#[allow(dead_code)]
pub(crate) struct Engine {
    pub(crate) temp: crate::storage::test_support::TempDir,
    pub(crate) services: SessionServices,
    pub(crate) config: Arc<crate::config::schema::Config>,
    pub(crate) llm: Arc<DispatchLlm>,
    pub(crate) snapshot: Arc<InMemorySnapshot>,
    pub(crate) worktree: std::path::PathBuf,
    pub(crate) ops: Arc<ProductionTaskOps>,
    pub(crate) prompt: Arc<SessionPrompt>,
    pub(crate) revert: Arc<crate::session::revert::SessionRevert>,
    pub(crate) models: Arc<dyn crate::session::r#loop::ModelSource>,
    pub(crate) registry: crate::tool::registry::ToolRegistry,
    pub(crate) instance: crate::tool::def::InstanceContext,
}

/// The `read` tool variants the engine harness can register.
pub(crate) enum EngineDefs {
    /// The real M4 `read` tool.
    Real,
    /// A `read` tool whose execution never resolves — the abort tests'
    /// cancellation seam.
    HangingRead,
}

/// A tool def whose execution never resolves.
fn hanging_def(id: &'static str) -> crate::tool::def::ToolDef {
    crate::tool::def::ToolDef {
        id,
        description: "hanging".into(),
        parameters: serde_json::json!({ "type": "object" }),
        format_validation_error: None,
        execute: Arc::new(|_args, _ctx| {
            Box::pin(async {
                std::future::pending::<()>().await;
                unreachable!()
            })
        }),
    }
}

pub(crate) fn engine_with_config(name: &str, script: MockScript, config: Value) -> Engine {
    engine_with_flags(name, script, config, None)
}

pub(crate) fn engine_with_flags(
    name: &str,
    script: MockScript,
    config: Value,
    session_permission: Option<Value>,
) -> Engine {
    engine_build(name, script, config, session_permission, EngineDefs::Real)
}

/// An engine whose compaction summary turn runs its own script (the
/// compaction `DispatchLlm` script is otherwise a copy of the main one).
pub(crate) fn engine_with_compaction_script(
    name: &str,
    script: MockScript,
    compaction_script: MockScript,
) -> Engine {
    engine_build_with(
        name,
        script,
        compaction_script,
        serde_json::json!({}),
        None,
        EngineDefs::Real,
    )
}

/// An engine whose `read` tool never resolves — the mid-flight
/// cancellation seam (spec M5.8 scenario 10).
pub(crate) fn engine_hanging_read(name: &str, script: MockScript) -> Engine {
    engine_build(
        name,
        script,
        serde_json::json!({}),
        None,
        EngineDefs::HangingRead,
    )
}

fn engine_build(
    name: &str,
    script: MockScript,
    config: Value,
    session_permission: Option<Value>,
    defs: EngineDefs,
) -> Engine {
    engine_build_with(
        name,
        script.clone(),
        script,
        config,
        session_permission,
        defs,
    )
}

fn engine_build_with(
    name: &str,
    script: MockScript,
    compaction_script: MockScript,
    config: Value,
    session_permission: Option<Value>,
    defs: EngineDefs,
) -> Engine {
    let temp = crate::storage::test_support::TempDir::new(name);
    let worktree = temp.path().join("repo");
    std::fs::create_dir_all(&worktree).unwrap();
    let config: crate::config::schema::Config =
        serde_json::from_value(config).expect("valid config");
    let agent_input = AgentRegistryInput {
        config: config.clone(),
        skill_dirs: Vec::new(),
        reference_dirs: Vec::new(),
        worktree: worktree.clone(),
        data_dir: temp.path().to_path_buf(),
        tmp_dir: temp.path().to_path_buf(),
        home: temp.path().to_path_buf(),
    };
    let services = SessionServices::new(
        Arc::new(crate::storage::Storage::open(temp.path().join("db.sqlite")).unwrap()),
        Arc::new(NoJobs),
        Arc::new(FixedClock),
        &agent_input,
    );
    services
        .storage
        .with_connection(|conn| {
            conn.execute(
                "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["global", "/repo", "[]", 1, 1],
            )
        })
        .unwrap();

    let config = Arc::new(config);
    let clock: Arc<dyn Clock> = Arc::new(FixedClock);
    let instance = crate::tool::def::InstanceContext {
        directory: worktree.clone(),
        worktree: worktree.clone(),
    };
    let context = crate::session::store::SessionContext {
        project_id: "global".to_string(),
        directory: worktree.clone(),
        worktree: worktree.clone(),
        workspace_id: None,
    };
    let models: Arc<dyn crate::session::r#loop::ModelSource> = Arc::new(StaticModels {
        model: test_model(),
    });
    let input_models: Arc<dyn crate::session::prompt_input::Models> = Arc::new(FixedInputModels);
    let snapshot = InMemorySnapshot::new(worktree.clone());

    // Production TaskOps + registry (task tool + real file tools).
    let ops = ProductionTaskOps::new(
        services.sessions.clone(),
        services.messages.clone(),
        services.agents.clone(),
        context,
    );
    let truncate = Arc::new(crate::tool::truncate::TruncateService::default_limits(
        temp.path().to_path_buf(),
    ));
    let agents = crate::tool::ripgrep::test_support::fixed_agents();
    let task = crate::tool::task::task_tool(
        truncate.clone(),
        agents.clone(),
        ops.clone(),
        1,
        Vec::new(),
        crate::tool::task::BackgroundMode::Disabled,
        None,
    );
    let read = match defs {
        EngineDefs::Real => crate::tool::read::read_tool(truncate.clone(), agents.clone(), None),
        EngineDefs::HangingRead => hanging_def("read"),
    };
    let write = crate::tool::write::write_tool(truncate.clone(), agents, None, None, None);
    let registry = crate::tool::registry::ToolRegistry::new(
        vec![task, read, write],
        Vec::new(),
        crate::tool::registry::RuntimeFlags::default(),
        Arc::new(RegistryStubAgents),
    )
    .expect("valid registry");

    // Subtask driver.
    let subtasks: Arc<dyn crate::session::r#loop::Subtasks> =
        Arc::new(SessionSubtask::new(SubtaskDeps {
            sessions: services.sessions.clone(),
            events: services.events.clone(),
            agents: services.agents.clone(),
            models: models.clone(),
            registry: registry.clone(),
            permission: services.permission.clone(),
            clock: clock.clone(),
            instance: instance.clone(),
        }));

    // Summary + revert + compaction.
    let summary = Arc::new(crate::session::summary::SessionSummary::new(
        crate::session::summary::SummaryDeps {
            sessions: services.sessions.clone(),
            snapshot: snapshot.clone(),
            events: services.events.clone(),
            config: config.clone(),
        },
    ));
    let revert = Arc::new(crate::session::revert::SessionRevert::new(
        crate::session::revert::RevertDeps {
            sessions: services.sessions.clone(),
            events: services.events.clone(),
            snapshot: snapshot.clone(),
            summary: summary.clone(),
            state: services.run_state.clone(),
            data_dir: temp.path().to_path_buf(),
        },
    ));
    let compaction: Arc<dyn crate::session::r#loop::Compaction> =
        Arc::new(crate::session::compaction::SessionCompaction::new(
            crate::session::compaction::CompactionDeps {
                sessions: services.sessions.clone(),
                messages: services.messages.clone(),
                events: services.events.clone(),
                status: services.status.clone(),
                agents: services.agents.clone(),
                snapshot: snapshot.clone(),
                llm: DispatchLlm::new(compaction_script),
                permission: services.permission.clone(),
                summary: summary.clone(),
                models: models.clone(),
                config: config.clone(),
                clock: clock.clone(),
                instance: instance.clone(),
                output_token_max: None,
                project_id: None,
                client: "cli".to_string(),
            },
        ));

    let instruction = Arc::new(crate::session::instruction::Instruction::new(
        crate::session::instruction::InstructionFlags {
            disable_claude_code_prompt: true,
            disable_project_config: true,
        },
        crate::session::instruction::InstructionPaths {
            config: temp.path().to_path_buf(),
            home: temp.path().to_path_buf(),
            directory: worktree.clone(),
            worktree: worktree.clone(),
        },
        Vec::new(),
        Arc::new(crate::session::instruction::NoRemoteInstructions),
    ));

    let llm = DispatchLlm::new(script);
    let _ = session_permission;
    let prompt = SessionPrompt::new(SessionPromptDeps {
        services: services.clone(),
        models: models.clone(),
        input_models,
        llm: llm.clone(),
        snapshot: snapshot.clone(),
        compaction,
        subtasks,
        summary,
        instruction,
        systems: Arc::new(crate::session::r#loop::NoSystemPrompts),
        registry: registry.clone(),
        revert: revert.clone(),
        prompt_ops: ops.clone(),
        config: config.clone(),
        clock,
        instance: instance.clone(),
        mcp: Arc::new(EmptyMcp),
        mcp_tools: None,
        truncate: std::sync::Arc::new(crate::tool::truncate::TruncateService::default_limits(
            temp.path().to_path_buf(),
        )),
        lsp: Arc::new(EmptyLsp),
        images: Arc::new(crate::session::prompt_input::NoResize),
        data_dir: temp.path().to_path_buf(),
        project_id: None,
        client: "cli".to_string(),
        experimental_plan_mode: false,
        vcs: false,
    });
    ops.bind(prompt.clone());
    Engine {
        temp,
        services,
        config,
        llm,
        snapshot,
        worktree,
        ops,
        prompt,
        revert,
        models: models.clone(),
        registry: registry.clone(),
        instance: instance.clone(),
    }
}

pub(crate) fn engine(name: &str, script: MockScript) -> Engine {
    engine_with_config(name, script, serde_json::json!({}))
}

/// Create a session in the engine and set a non-default title (title
/// generation is forked on step 1 and would consume mock scripts).
pub(crate) fn create_engine_session(e: &Engine) -> opencode_schema::session_v1::V1SessionInfo {
    let session = create_session(&e.services.sessions, &e.worktree);
    e.services
        .sessions
        .set_title(&session.id, "Custom")
        .unwrap();
    session
}

pub(crate) struct FixedInputModels;

impl crate::session::prompt_input::Models for FixedInputModels {
    fn get_model<'a>(
        &'a self,
        provider_id: &'a str,
        model_id: &'a str,
    ) -> crate::tool::def::BoxFuture<'a, Result<opencode_schema::model::ModelInfo, crate::CoreError>>
    {
        Box::pin(async move { Ok(input_model_info(provider_id, model_id)) })
    }
    fn default_model(
        &self,
    ) -> crate::tool::def::BoxFuture<
        'static,
        Result<opencode_schema::model::ModelInfo, crate::CoreError>,
    > {
        Box::pin(async { Ok(input_model_info("anthropic", "claude")) })
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

/// Empty MCP seam (M5 has no MCP client).
pub(crate) struct EmptyMcp;

impl crate::session::prompt_input::McpResources for EmptyMcp {
    fn read_resource<'a>(
        &'a self,
        _client_name: &'a str,
        _uri: &'a str,
    ) -> crate::tool::def::BoxFuture<
        'a,
        Result<crate::session::prompt_input::McpReadResource, String>,
    > {
        Box::pin(async { Err("connection refused".to_string()) })
    }
}

/// Empty LSP seam.
pub(crate) struct EmptyLsp;

impl crate::session::prompt_input::LspServer for EmptyLsp {
    fn has_clients<'a>(&'a self, _file: &'a str) -> crate::tool::def::BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
    fn touch_file<'a>(&'a self, _file: &'a str) -> crate::tool::def::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
    fn definition<'a>(
        &'a self,
        _position: crate::tool::lsp::Position,
    ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
        Box::pin(async { Vec::new() })
    }
    fn references<'a>(
        &'a self,
        _position: crate::tool::lsp::Position,
    ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
        Box::pin(async { Vec::new() })
    }
    fn hover<'a>(
        &'a self,
        _position: crate::tool::lsp::Position,
    ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
        Box::pin(async { Vec::new() })
    }
    fn document_symbol<'a>(&'a self, _uri: &'a str) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
        Box::pin(async { Vec::new() })
    }
    fn workspace_symbol<'a>(
        &'a self,
        _query: &'a str,
    ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
        Box::pin(async { Vec::new() })
    }
    fn implementation<'a>(
        &'a self,
        _position: crate::tool::lsp::Position,
    ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
        Box::pin(async { Vec::new() })
    }
    fn prepare_call_hierarchy<'a>(
        &'a self,
        _position: crate::tool::lsp::Position,
    ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
        Box::pin(async { Vec::new() })
    }
    fn incoming_calls<'a>(
        &'a self,
        _position: crate::tool::lsp::Position,
    ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
        Box::pin(async { Vec::new() })
    }
    fn outgoing_calls<'a>(
        &'a self,
        _position: crate::tool::lsp::Position,
    ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
        Box::pin(async { Vec::new() })
    }
}

/// Registry agents stub for `ToolRegistry::new`.
pub(crate) struct RegistryStubAgents;

impl crate::tool::def::Agents for RegistryStubAgents {
    fn get<'a>(
        &'a self,
        _agent: &'a str,
    ) -> crate::tool::def::BoxFuture<
        'a,
        Result<crate::tool::def::AgentInfo, crate::tool::error::ToolError>,
    > {
        Box::pin(async {
            Err(crate::tool::error::ToolError::Failed(
                "no agents".to_string(),
            ))
        })
    }
    fn list<'a>(&'a self) -> crate::tool::def::BoxFuture<'a, Vec<crate::tool::def::AgentInfo>> {
        Box::pin(async { Vec::new() })
    }
}

// Keep the unused-import lint honest for optional extensions.
#[allow(unused)]
fn _unused(e: &Engine, input: &ShellInput) -> Option<&'static str> {
    let _ = (input, COMMAND_EXECUTED);
    None
}

// ---------------------------------------------------------------------------
// M5.8 permission answerer — the scripted "user" behind the real
// permission service (the only scripted seam besides the LLM, spec §6.6)
// ---------------------------------------------------------------------------

/// A scripted replier for `permission.asked` events: every ask pops the
/// next reply (falling back to `once`) and is answered through the real
/// [`PermissionService`], so the ask/reply event pair and the
/// once/always/reject semantics are exercised end-to-end.
pub(crate) struct PermissionAnswerer {
    asks: Mutex<Vec<opencode_schema::permission_v1::PermissionV1Request>>,
    replies: Mutex<std::collections::VecDeque<opencode_schema::permission_v1::PermissionV1Reply>>,
}

impl PermissionAnswerer {
    pub(crate) fn new(
        replies: Vec<opencode_schema::permission_v1::PermissionV1Reply>,
    ) -> Arc<Self> {
        Arc::new(PermissionAnswerer {
            asks: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into_iter().collect()),
        })
    }

    /// The observed `permission.asked` payloads.
    pub(crate) fn asks(&self) -> Vec<opencode_schema::permission_v1::PermissionV1Request> {
        self.asks.lock().unwrap().clone()
    }

    /// Serve asks until `count` replies have been sent. Spawn as a task
    /// alongside the prompt future.
    pub(crate) async fn serve(
        self: Arc<Self>,
        service: Arc<crate::session::permission::PermissionService>,
        mut asked: tokio::sync::broadcast::Receiver<crate::event::definition::Payload>,
        count: usize,
    ) {
        let mut sent = 0;
        while sent < count {
            let event = asked.recv().await.expect("permission.asked event");
            let request: opencode_schema::permission_v1::PermissionV1Request =
                serde_json::from_value(event.data.clone()).expect("asked payload shape");
            self.asks.lock().unwrap().push(request.clone());
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(opencode_schema::permission_v1::PermissionV1Reply::Once);
            service
                .reply(opencode_schema::permission_v1::PermissionV1ReplyInput {
                    request_id: request.id,
                    reply,
                    message: None,
                })
                .expect("reply is valid");
            sent += 1;
        }
    }
}
