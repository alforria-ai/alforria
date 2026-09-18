//! Per-directory production engine assembly — M7.2.
//!
//! TS wires one engine per instance node: `ToolRegistry` + `SessionPrompt`
//! and friends are Effect layers resolved from the instance's services
//! (`tool/registry.ts:211-250`, `prompt.ts:105-135`). The Rust port builds
//! the same graph eagerly in [`build_engine`] when an instance boots and
//! keeps it in the [`EngineStore`] keyed by the services the
//! [`LocationContext`](crate::middleware::location::LocationContext)
//! carries — the server's engine seam resolves through that store.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use opencode_core::session::agents::AgentRegistry;
use opencode_core::session::compaction::{CompactionDeps, SessionCompaction};
use opencode_core::session::instruction::{Instruction, InstructionFlags, InstructionPaths};
use opencode_core::session::llm::LlmStream;
use opencode_core::session::prompt::SessionPromptDeps;
use opencode_core::session::prompt::{CommandInput, ShellInput};
use opencode_core::session::prompt_input::Models as InputModels;
use opencode_core::session::prompt_input::{PromptError, PromptInput};
use opencode_core::session::r#loop::{LoopError, ModelSource, ResolvedModel};
use opencode_core::session::revert::{RevertDeps, RevertInput, SessionRevert};
use opencode_core::session::run_state::RunnerError;
use opencode_core::session::snapshot::Snapshot;
use opencode_core::session::subtask::{SessionSubtask, SubtaskDeps};
use opencode_core::session::summary::{SessionSummary, SummaryDeps};
use opencode_core::session::task_ops::ProductionTaskOps;
use opencode_core::tool::def::InstanceContext;
use opencode_core::tool::registry::{RuntimeFlags, ToolRegistry};
use opencode_core::tool::task::BackgroundMode;
use opencode_core::{
    BackgroundJobService, Clock, CoreError, SessionError, SessionServices, WithParts,
};
use opencode_schema::file_diff::SnapshotFileDiff;
use opencode_schema::session_v1::V1SessionInfo;

use crate::error::ServerError;
use crate::middleware::location::LocationContext;
use crate::state::{EngineFactory, SessionEngine, ToolRegistrySource};

/// `Config.boolean(name).pipe(Config.withDefault(false))` — Effect accepts
/// the same truthy strings as the project registry (project/registry.rs).
pub fn bool_env(name: &str) -> bool {
    std::env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// `enabledByExperimental(name)` (runtime-flags.ts:11-14): the specific
/// flag, else `OPENCODE_EXPERIMENTAL`.
pub fn experimental_env(name: &str) -> bool {
    match std::env::var(name) {
        Ok(_) => bool_env(name),
        Err(_) => bool_env("OPENCODE_EXPERIMENTAL"),
    }
}

/// `flags.experimentalBackgroundSubagents`
/// (runtime-flags.ts:43).
pub fn background_subagents_enabled() -> bool {
    experimental_env("OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS")
}

// ---------------------------------------------------------------------------
// M7.7 seams — the provider runtime (model resolution + LLM request
// routing) lands with the provider-completion chunk. The production engine
// binds erroring seams so every HTTP surface fails loudly (and uniformly)
// until then.
// ---------------------------------------------------------------------------

const UNWIRED_PROVIDER_RUNTIME: &str = "provider runtime not wired (M7.7)";

struct UnwiredModels;

impl ModelSource for UnwiredModels {
    fn get_model<'a>(
        &'a self,
        _provider_id: &'a str,
        _model_id: &'a str,
        _session_id: &'a str,
    ) -> BoxFuture<'a, Result<ResolvedModel, LoopError>> {
        Box::pin(async {
            Err(LoopError::Session(SessionError::Core(CoreError::Storage(
                UNWIRED_PROVIDER_RUNTIME.to_string(),
            ))))
        })
    }

    fn get_small_model<'a>(
        &'a self,
        _provider_id: &'a str,
    ) -> BoxFuture<'a, Option<ResolvedModel>> {
        Box::pin(async { None })
    }
}

struct UnwiredInputModels;

impl InputModels for UnwiredInputModels {
    fn get_model<'a>(
        &'a self,
        _provider_id: &'a str,
        _model_id: &'a str,
    ) -> opencode_core::tool::def::BoxFuture<'a, Result<opencode_schema::model::ModelInfo, CoreError>>
    {
        Box::pin(async { Err(CoreError::Storage(UNWIRED_PROVIDER_RUNTIME.to_string())) })
    }

    fn default_model(
        &self,
    ) -> opencode_core::tool::def::BoxFuture<
        'static,
        Result<opencode_schema::model::ModelInfo, CoreError>,
    > {
        Box::pin(async { Err(CoreError::Storage(UNWIRED_PROVIDER_RUNTIME.to_string())) })
    }
}

struct UnwiredLlm;

impl LlmStream for UnwiredLlm {
    fn stream(
        &self,
        _input: opencode_core::session::llm::StreamInput,
    ) -> opencode_core::session::llm::LlmEventStream {
        Box::pin(futures::stream::once(async move {
            Err(opencode_llm::LlmError::invalid(UNWIRED_PROVIDER_RUNTIME))
        }))
    }
}

/// No-op MCP seam — the MCP client is later M7 surface
/// (`EmptyMcp` mirrors the M5 test-support stub).
struct NoMcp;

impl opencode_core::session::prompt_input::McpResources for NoMcp {
    fn read_resource<'a>(
        &'a self,
        _client_name: &'a str,
        _uri: &'a str,
    ) -> opencode_core::tool::def::BoxFuture<
        'a,
        Result<opencode_core::session::prompt_input::McpReadResource, String>,
    > {
        Box::pin(async { Err("connection refused".to_string()) })
    }
}

/// No-op LSP server seam — the LSP service is M7.9.
struct NoLsp;

impl opencode_core::session::prompt_input::LspServer for NoLsp {
    fn has_clients<'a>(&'a self, _file: &'a str) -> opencode_core::tool::def::BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
    fn touch_file<'a>(&'a self, _file: &'a str) -> opencode_core::tool::def::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
    fn definition<'a>(
        &'a self,
        _position: opencode_core::tool::lsp::Position,
    ) -> opencode_core::tool::def::BoxFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Vec::new() })
    }
    fn references<'a>(
        &'a self,
        _position: opencode_core::tool::lsp::Position,
    ) -> opencode_core::tool::def::BoxFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Vec::new() })
    }
    fn hover<'a>(
        &'a self,
        _position: opencode_core::tool::lsp::Position,
    ) -> opencode_core::tool::def::BoxFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Vec::new() })
    }
    fn document_symbol<'a>(
        &'a self,
        _uri: &'a str,
    ) -> opencode_core::tool::def::BoxFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Vec::new() })
    }
    fn workspace_symbol<'a>(
        &'a self,
        _query: &'a str,
    ) -> opencode_core::tool::def::BoxFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Vec::new() })
    }
    fn implementation<'a>(
        &'a self,
        _position: opencode_core::tool::lsp::Position,
    ) -> opencode_core::tool::def::BoxFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Vec::new() })
    }
    fn prepare_call_hierarchy<'a>(
        &'a self,
        _position: opencode_core::tool::lsp::Position,
    ) -> opencode_core::tool::def::BoxFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Vec::new() })
    }
    fn incoming_calls<'a>(
        &'a self,
        _position: opencode_core::tool::lsp::Position,
    ) -> opencode_core::tool::def::BoxFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Vec::new() })
    }
    fn outgoing_calls<'a>(
        &'a self,
        _position: opencode_core::tool::lsp::Position,
    ) -> opencode_core::tool::def::BoxFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Vec::new() })
    }
}

/// Production remote-instruction fetch — `https://` config instruction
/// entries fetched with a 5 s timeout (instruction.ts:95-103). The trait is
/// sync and reqwest has no blocking feature here, so fetch on a thread.
struct HttpRemoteInstructions;

impl opencode_core::session::instruction::RemoteInstructions for HttpRemoteInstructions {
    fn fetch(&self, url: &str) -> String {
        let url = url.to_string();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            runtime
                .block_on(async {
                    let client = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(5))
                        .build()?;
                    let response = client.get(&url).send().await?;
                    response.text().await
                })
                .ok()
        })
        .join()
        .ok()
        .flatten()
        .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// The per-instance engine: prompt facade + revert/summary services built
/// over the M5 graph, plus the background-job service for `sessionBackground`.
pub struct ProductionEngine {
    prompt: Arc<opencode_core::session::prompt::SessionPrompt>,
    revert: Arc<SessionRevert>,
    summary: Arc<SessionSummary>,
    /// The production tool registry — `/experimental/tool` reads this.
    registry: Arc<ToolRegistry>,
    background: Arc<BackgroundJobService>,
    config: Arc<opencode_core::config::schema::Config>,
}

/// The [`SessionEngine`] surface (`state.rs`) over the M5 services. The
/// futures are `'static` — the facade clones into them (all `Arc` handles).
impl SessionEngine for ProductionEngine {
    fn prompt(&self, input: PromptInput) -> BoxFuture<'static, Result<WithParts, PromptError>> {
        let prompt = self.prompt.clone();
        Box::pin(async move { prompt.prompt(input).await })
    }

    fn loop_(
        &self,
        session_id: String,
    ) -> BoxFuture<'static, Result<WithParts, RunnerError<SessionError>>> {
        let prompt = self.prompt.clone();
        Box::pin(async move { prompt.loop_(&session_id).await })
    }

    fn command(&self, input: CommandInput) -> BoxFuture<'static, Result<WithParts, PromptError>> {
        let prompt = self.prompt.clone();
        Box::pin(async move { prompt.command(input).await })
    }

    fn shell(&self, input: ShellInput) -> BoxFuture<'static, Result<WithParts, SessionError>> {
        let prompt = self.prompt.clone();
        Box::pin(async move { prompt.shell(input).await })
    }

    fn revert(
        &self,
        input: RevertInput,
    ) -> BoxFuture<'static, Result<V1SessionInfo, SessionError>> {
        let revert = self.revert.clone();
        Box::pin(async move { revert.revert(input).await })
    }

    fn unrevert(
        &self,
        session_id: String,
    ) -> BoxFuture<'static, Result<V1SessionInfo, SessionError>> {
        let revert = self.revert.clone();
        Box::pin(async move { revert.unrevert(&session_id).await })
    }

    fn cleanup(&self, session: &V1SessionInfo) -> Result<(), SessionError> {
        self.revert.cleanup(session)
    }

    fn diff(
        &self,
        session_id: &str,
        message_id: Option<&str>,
    ) -> Result<Vec<SnapshotFileDiff>, SessionError> {
        self.summary.diff(session_id, message_id)
    }

    fn share(&self, _session: &V1SessionInfo) -> Result<(), String> {
        // TS `enabled` gate (share/session.ts:58-60).
        if self.config.share == Some(opencode_core::config::schema::Share::Disabled) {
            Err("Sharing is disabled in configuration".to_string())
        } else {
            Err("share service not wired".to_string())
        }
    }

    fn unshare(&self, _session_id: &str) -> Result<(), String> {
        Err("share service not wired".to_string())
    }

    fn session_background<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move { self.session_background_impl(session_id).await })
    }
}

impl ProductionEngine {
    /// `sessionBackground` (`handlers/experimental.ts:178-193`): promote the
    /// session's running, non-background task jobs; `true` if any promoted.
    async fn session_background_impl(&self, session_id: &str) -> bool {
        if !background_subagents_enabled() {
            return false;
        }
        let jobs = self.background.list();
        let session = session_id;
        let promoted = futures::future::join_all(
            jobs.iter()
                .filter(|job| {
                    job.r#type == "task"
                        && job.status == opencode_core::session::background::Status::Running
                        && job
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get("parentSessionId"))
                            .and_then(serde_json::Value::as_str)
                            == Some(session)
                        && job.metadata.as_ref().and_then(|m| m.get("background"))
                            != Some(&serde_json::Value::Bool(true))
                })
                .map(|job| self.background.promote(&job.id)),
        )
        .await;
        promoted.into_iter().any(|job| job.is_some())
    }
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

/// `Agent.Service` access for tools — the registry knows the full
/// `Agent.Info` records; the tool system needs the reduced slice
/// (`registry.ts` resolves the same Effect service for both).
#[derive(Clone)]
struct RegistryAgents(AgentRegistry);

impl opencode_core::tool::def::Agents for RegistryAgents {
    fn get<'a>(
        &'a self,
        agent: &'a str,
    ) -> opencode_core::tool::def::BoxFuture<
        'a,
        Result<opencode_core::tool::def::AgentInfo, opencode_core::tool::error::ToolError>,
    > {
        let info = self.0.get(agent).cloned();
        Box::pin(async move {
            info.map(|info| opencode_core::tool::def::AgentInfo {
                name: info.name,
                description: info.description,
                mode: info.mode,
                permission: info.permission,
            })
            .ok_or_else(|| {
                opencode_core::tool::error::ToolError::Failed(format!("Unknown agent {agent}"))
            })
        })
    }

    fn list<'a>(
        &'a self,
    ) -> opencode_core::tool::def::BoxFuture<'a, Vec<opencode_core::tool::def::AgentInfo>> {
        let list = self
            .0
            .list()
            .into_iter()
            .map(|info| opencode_core::tool::def::AgentInfo {
                name: info.name,
                description: info.description,
                mode: info.mode,
                permission: info.permission,
            })
            .collect();
        Box::pin(async move { list })
    }
}

/// Everything the engine build closes over from the instance boot.
pub struct EngineInput {
    pub services: Arc<SessionServices>,
    pub background: Arc<BackgroundJobService>,
    pub config: Arc<opencode_core::config::schema::Config>,
    /// `config.directories()` — the config dirs skill discovery scans.
    pub config_dirs: Vec<PathBuf>,
    pub directory: PathBuf,
    pub worktree: PathBuf,
    pub paths: opencode_core::GlobalPaths,
    /// Injectable provider-runtime seams — production leaves them unwired
    /// (M7.7); the e2e tests script a mock LLM here.
    pub seams: EngineSeams,
}

/// Provider-runtime seams: `None` falls back to the unwired M7.7 stubs.
#[derive(Clone, Default)]
pub struct EngineSeams {
    pub llm: Option<Arc<dyn LlmStream>>,
    pub models: Option<Arc<dyn ModelSource>>,
    pub input_models: Option<Arc<dyn InputModels>>,
}

/// `Shell.preferred` (core/shell.ts:205-208) — config value, else `$SHELL`,
/// else the platform default.
fn preferred_shell(config_shell: Option<&str>) -> String {
    config_shell.map(str::to_string).unwrap_or_else(|| {
        std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| "sh".to_string())
    })
}

/// `Config.string("OPENCODE_CLIENT").pipe(Config.withDefault("cli"))`
/// (runtime-flags.ts:56).
fn client_env() -> String {
    std::env::var("OPENCODE_CLIENT").unwrap_or_else(|_| "cli".to_string())
}

/// `positiveInteger("OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS")`
/// (runtime-flags.ts:53) with the `?? 2 * 60 * 1000` default
/// (shell.ts:347).
fn bash_timeout_ms() -> u64 {
    std::env::var("OPENCODE_EXPERIMENTAL_BASH_DEFAULT_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(2 * 60 * 1000)
}

/// Build the per-instance engine (the M6 `SessionEngine` seam over the M5
/// service graph). Mirrors the M5.7 test-support harness with production
/// deps: the merged config, real tools and the background-job service.
pub fn build_engine(input: &EngineInput) -> Result<Arc<ProductionEngine>, ServerError> {
    let services = &input.services;
    let clock: Arc<dyn Clock> = Arc::new(opencode_core::catalog::SystemClock);
    let instance = InstanceContext {
        directory: input.directory.clone(),
        worktree: input.worktree.clone(),
    };
    let context = opencode_core::session::store::SessionContext {
        project_id: services
            .instance_location()
            .map(|location| location.project.id)
            .unwrap_or_else(|| "global".to_string()),
        directory: input.directory.clone(),
        worktree: input.worktree.clone(),
        workspace_id: None,
    };

    // Provider-runtime seams — unwired until M7.7 unless injected.
    let models: Arc<dyn ModelSource> = input
        .seams
        .models
        .clone()
        .unwrap_or_else(|| Arc::new(UnwiredModels));
    let input_models: Arc<dyn InputModels> = input
        .seams
        .input_models
        .clone()
        .unwrap_or_else(|| Arc::new(UnwiredInputModels));
    let llm: Arc<dyn LlmStream> = input
        .seams
        .llm
        .clone()
        .unwrap_or_else(|| Arc::new(UnwiredLlm));
    // M7.3 wires the production git snapshot.
    let snapshot: Arc<dyn Snapshot> = Arc::new(opencode_core::session::snapshot::DisabledSnapshot);

    // Production TaskOps + the full tool registry (registry.ts:240-249).
    let ops = ProductionTaskOps::new(
        services.sessions.clone(),
        services.messages.clone(),
        services.agents.clone(),
        context,
    );
    let truncate = Arc::new(
        opencode_core::tool::truncate::TruncateService::default_limits(
            input.paths.data.join("tool-output"),
        ),
    );
    let agents: Arc<dyn opencode_core::tool::def::Agents> =
        Arc::new(RegistryAgents(services.agents.clone()));
    let background_enabled = background_subagents_enabled();
    let task = opencode_core::tool::task::task_tool(
        truncate.clone(),
        agents.clone(),
        ops.clone(),
        input
            .config
            .subagent_depth
            .map(|depth| depth.max(1) as usize)
            .unwrap_or(1),
        input
            .config
            .experimental
            .as_ref()
            .and_then(|experimental| experimental.primary_tools.clone())
            .unwrap_or_default(),
        if background_enabled {
            BackgroundMode::Enabled
        } else {
            BackgroundMode::Disabled
        },
    );
    let question = opencode_core::tool::question::question_tool(
        truncate.clone(),
        agents.clone(),
        services.question.clone(),
    );
    let ripgrep: Arc<dyn opencode_core::tool::ripgrep::Ripgrep> =
        Arc::new(opencode_core::tool::ripgrep::RipgrepService);
    let todo = opencode_core::tool::todo::todo_tool(
        truncate.clone(),
        agents.clone(),
        Arc::new(
            opencode_core::tool::todo::TodoService::new(services.storage.clone())
                .with_events(services.events.clone()),
        ),
    );
    // Websearch env keys are read once at boot (WebSearchEnv::default).
    let websearch = opencode_core::tool::websearch::websearch_tool(
        truncate.clone(),
        agents.clone(),
        Arc::new(UnwiredMcpHttpClient),
        opencode_core::tool::websearch::WebSearchFlags {
            exa: bool_env("OPENCODE_ENABLE_EXA") || experimental_env("OPENCODE_EXPERIMENTAL_EXA"),
            parallel: bool_env("OPENCODE_ENABLE_PARALLEL")
                || experimental_env("OPENCODE_EXPERIMENTAL_PARALLEL"),
        },
        Arc::new(opencode_core::tool::websearch::SystemYear),
        opencode_core::tool::websearch::WebSearchEnv::default(),
    );
    // `Tool.init(shell)` — the def renders the bash prompt; `render` is
    // pure (no runtime needed), so a plain block_on suffices.
    let shell = futures::executor::block_on(
        opencode_core::tool::shell::ShellTool::new(
            preferred_shell(input.config.shell.as_deref()),
            bash_timeout_ms(),
            truncate.clone(),
            Arc::new(opencode_core::tool::shell::TokioSpawner),
        )
        .def(agents.clone()),
    );
    let mut builtin = vec![
        opencode_core::tool::invalid::invalid_tool(truncate.clone(), agents.clone()),
        question,
        shell,
        opencode_core::tool::read::read_tool(truncate.clone(), agents.clone(), None),
        opencode_core::tool::glob::glob_tool(truncate.clone(), agents.clone(), ripgrep.clone()),
        opencode_core::tool::grep::grep_tool(truncate.clone(), agents.clone(), ripgrep.clone()),
        opencode_core::tool::edit::edit_tool(truncate.clone(), agents.clone(), None, None, None),
        opencode_core::tool::write::write_tool(truncate.clone(), agents.clone(), None, None, None),
        task,
        opencode_core::tool::webfetch::webfetch_tool(
            truncate.clone(),
            agents.clone(),
            opencode_core::tool::webfetch::reqwest_client(),
        ),
        todo,
        websearch,
        opencode_core::tool::skill::skill_tool(
            truncate.clone(),
            agents.clone(),
            Arc::new(opencode_core::tool::skill::SkillService::discover(
                &opencode_core::tool::skill::SkillDiscovery {
                    directories: input.config_dirs.clone(),
                    paths: input
                        .config
                        .skills
                        .as_ref()
                        .and_then(|skills| skills.paths.clone())
                        .unwrap_or_default(),
                    directory: input.directory.clone(),
                    home: input.paths.home.clone(),
                },
            )),
            ripgrep.clone(),
        ),
        opencode_core::tool::apply_patch::apply_patch_tool(
            truncate.clone(),
            agents.clone(),
            None,
            None,
            None,
        ),
    ];
    // `(questionEnabled ? [tool.question] : [])` — question's slot is
    // right after `invalid` (registry.ts:242).
    let question_enabled = matches!(client_env().as_str(), "app" | "cli" | "desktop")
        || bool_env("OPENCODE_ENABLE_QUESTION_TOOL");
    if !question_enabled {
        builtin.remove(1);
    }
    let registry = Arc::new(
        ToolRegistry::new(
            builtin,
            Vec::new(),
            RuntimeFlags {
                client: client_env(),
                enable_question_tool: bool_env("OPENCODE_ENABLE_QUESTION_TOOL"),
                enable_exa: bool_env("OPENCODE_ENABLE_EXA"),
                enable_parallel: bool_env("OPENCODE_ENABLE_PARALLEL"),
                experimental_code_mode: false,
                experimental_lsp_tool: false,
                experimental_plan_mode: false,
            },
            agents.clone(),
        )
        .map_err(|err| ServerError::Core(CoreError::Storage(err.to_string())))?,
    );

    // Subtask driver + summary + revert + compaction.
    let subtasks: Arc<dyn opencode_core::session::r#loop::Subtasks> =
        Arc::new(SessionSubtask::new(SubtaskDeps {
            sessions: services.sessions.clone(),
            events: services.events.clone(),
            agents: services.agents.clone(),
            models: models.clone(),
            registry: (*registry).clone(),
            permission: services.permission.clone(),
            clock: clock.clone(),
            instance: instance.clone(),
        }));
    let summary = Arc::new(SessionSummary::new(SummaryDeps {
        sessions: services.sessions.clone(),
        snapshot: snapshot.clone(),
        events: services.events.clone(),
        config: input.config.clone(),
    }));
    let revert = Arc::new(SessionRevert::new(RevertDeps {
        sessions: services.sessions.clone(),
        events: services.events.clone(),
        snapshot: snapshot.clone(),
        summary: summary.clone(),
        state: services.run_state.clone(),
        data_dir: input.paths.data.clone(),
    }));
    let compaction: Arc<dyn opencode_core::session::r#loop::Compaction> =
        Arc::new(SessionCompaction::new(CompactionDeps {
            sessions: services.sessions.clone(),
            messages: services.messages.clone(),
            events: services.events.clone(),
            status: services.status.clone(),
            agents: services.agents.clone(),
            snapshot: snapshot.clone(),
            llm: llm.clone(),
            permission: services.permission.clone(),
            summary: summary.clone(),
            models: models.clone(),
            config: input.config.clone(),
            clock: clock.clone(),
            instance: instance.clone(),
            output_token_max: None,
            project_id: None,
            client: client_env(),
        }));

    let instruction = Arc::new(Instruction::new(
        InstructionFlags {
            disable_claude_code_prompt: bool_env("OPENCODE_DISABLE_CLAUDE_CODE")
                || bool_env("OPENCODE_DISABLE_CLAUDE_CODE_PROMPT"),
            disable_project_config: bool_env("OPENCODE_DISABLE_PROJECT_CONFIG"),
        },
        InstructionPaths {
            config: input.paths.config.clone(),
            home: input.paths.home.clone(),
            directory: input.directory.clone(),
            worktree: input.worktree.clone(),
        },
        input.config.instructions.clone().unwrap_or_default(),
        Arc::new(HttpRemoteInstructions),
    ));

    let prompt = opencode_core::session::prompt::SessionPrompt::new(SessionPromptDeps {
        services: (**services).clone(),
        models: models.clone(),
        input_models,
        llm,
        snapshot,
        compaction,
        subtasks,
        summary: summary.clone(),
        instruction,
        systems: Arc::new(opencode_core::session::r#loop::NoSystemPrompts),
        registry: (*registry).clone(),
        revert: revert.clone(),
        prompt_ops: ops.clone(),
        config: input.config.clone(),
        clock,
        instance,
        mcp: Arc::new(NoMcp),
        lsp: Arc::new(NoLsp),
        images: Arc::new(opencode_core::session::prompt_input::NoResize),
        data_dir: input.paths.data.clone(),
        project_id: None,
        client: client_env(),
        experimental_plan_mode: false,
        vcs: false,
    });
    ops.bind(prompt.clone());

    Ok(Arc::new(ProductionEngine {
        prompt,
        revert,
        summary,
        registry,
        background: input.background.clone(),
        config: input.config.clone(),
    }))
}

// ---------------------------------------------------------------------------
// Store + seams
// ---------------------------------------------------------------------------

/// Per-instance engines keyed by the `Arc<SessionServices>` a
/// [`LocationContext`] carries. The value keeps the services `Arc` alive so
/// the address key can't be reused while the entry exists.
#[derive(Default)]
pub struct EngineStore {
    entries: Mutex<HashMap<usize, EngineEntry>>,
}

struct EngineEntry {
    /// Keeps the key (the services address) alive and unique — never read.
    _services: Arc<SessionServices>,
    engine: Arc<ProductionEngine>,
}

impl EngineStore {
    fn engine(&self, services: &Arc<SessionServices>) -> Option<Arc<ProductionEngine>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(Arc::as_ptr(services) as usize))
            .map(|entry| entry.engine.clone())
    }

    fn insert(
        &self,
        services: Arc<SessionServices>,
        engine: Arc<ProductionEngine>,
    ) -> Result<(), ServerError> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                Arc::as_ptr(&services) as usize,
                EngineEntry {
                    _services: services,
                    engine,
                },
            );
        Ok(())
    }

    /// The `EngineFactory` seam — resolves the engine for a request's
    /// instance, or a defect-500 when the instance has no engine (the
    /// engine build failed at boot).
    pub fn factory(self: &Arc<Self>) -> EngineFactory {
        Arc::new({
            let store = Arc::clone(self);
            move |location: &LocationContext| {
                store
                    .engine(&location.services)
                    .map(|engine| engine as Arc<dyn SessionEngine>)
                    .ok_or_else(|| engine_missing(&location.directory))
            }
        })
    }

    /// The `ToolRegistrySource` seam — `/experimental/tool` resolves the
    /// instance's registry (fixes the M6 defect-500).
    pub fn tools(self: &Arc<Self>) -> Arc<dyn ToolRegistrySource> {
        Arc::new(StoreTools(Arc::clone(self)))
    }

    /// Build the engine for a booted instance and register it.
    pub fn boot(self: &Arc<Self>, input: &EngineInput) -> Result<(), ServerError> {
        let engine = build_engine(input)?;
        self.insert(input.services.clone(), engine)
    }
}

fn engine_missing(directory: &std::path::Path) -> ServerError {
    ServerError::Core(CoreError::Storage(format!(
        "session engine not booted for directory {} (M7.2)",
        directory.display()
    )))
}

struct StoreTools(Arc<EngineStore>);

impl ToolRegistrySource for StoreTools {
    fn registry(&self, location: &LocationContext) -> Result<Arc<ToolRegistry>, ServerError> {
        self.0
            .engine(&location.services)
            .map(|engine| engine.registry.clone())
            .ok_or_else(|| engine_missing(&location.directory))
    }
}

/// Production MCP HTTP client for the websearch tool — the real MCP client
/// is later M7 surface; the tool's HTTP seam stays stubbed.
struct UnwiredMcpHttpClient;

impl opencode_core::tool::mcp_websearch::McpHttpClient for UnwiredMcpHttpClient {
    fn post<'a>(
        &'a self,
        _url: &'a str,
        _headers: Vec<(String, String)>,
        _body: &'a str,
    ) -> opencode_core::tool::def::BoxFuture<
        'a,
        Result<
            opencode_core::tool::mcp_websearch::McpHttpResponse,
            opencode_core::tool::error::ToolError,
        >,
    > {
        Box::pin(async {
            Err(opencode_core::tool::error::ToolError::Failed(
                "MCP client not wired".to_string(),
            ))
        })
    }
}
