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
use opencode_core::session::r#loop::ModelSource;
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
// M7.7 — the provider runtime (crate::provider_runtime) supplies the
// model resolution + LLM request routing; the e2e tests script mock
// seams here.
// ---------------------------------------------------------------------------

/// The engine's MCP resource seam — the real service (M7.6).
#[derive(Clone)]
struct EngineMcp(Arc<opencode_core::mcp::McpService>);

impl opencode_core::session::prompt_input::McpResources for EngineMcp {
    fn read_resource<'a>(
        &'a self,
        client_name: &'a str,
        uri: &'a str,
    ) -> opencode_core::tool::def::BoxFuture<
        'a,
        Result<opencode_core::session::prompt_input::McpReadResource, String>,
    > {
        Box::pin(async move {
            let Some(value) = self.0.read_resource(client_name, uri).await? else {
                return Ok(opencode_core::session::prompt_input::McpReadResource::NotFound);
            };
            let contents = value
                .get("contents")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let items = contents
                .into_iter()
                .filter_map(|item| {
                    let text = item
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(String::from);
                    let blob = item
                        .get("blob")
                        .and_then(serde_json::Value::as_str)
                        .map(String::from);
                    if text.is_none() && blob.is_none() {
                        return None;
                    }
                    Some(opencode_core::session::prompt_input::McpResourceItem {
                        text,
                        blob,
                        mime_type: item
                            .get("mimeType")
                            .and_then(serde_json::Value::as_str)
                            .map(String::from),
                        uri: item
                            .get("uri")
                            .and_then(serde_json::Value::as_str)
                            .map(String::from),
                    })
                })
                .collect();
            Ok(opencode_core::session::prompt_input::McpReadResource::Contents(items))
        })
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
    share: Arc<opencode_core::share::SessionShare>,
    /// The MCP service — the `/mcp` route family reads this (M7.6).
    mcp: Arc<opencode_core::mcp::McpService>,
    /// The provider model resolution + LLM seam — the project-copy
    /// generate-name stream (M7.8).
    models: Arc<dyn opencode_core::session::r#loop::ModelSource>,
    defaults: Arc<dyn opencode_core::session::prompt_input::Models>,
    llm: Arc<dyn LlmStream>,
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

    fn share(&self, session: &V1SessionInfo) -> Result<(), String> {
        self.share.share(&session.id)
    }

    fn unshare(&self, session_id: &str) -> Result<(), String> {
        self.share.unshare(session_id)
    }

    fn auto_share(&self, session: &V1SessionInfo) {
        self.share.auto_share(session);
    }

    fn session_background<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move { self.session_background_impl(session_id).await })
    }

    fn generate_copy_name<'a>(&'a self, context: &'a str) -> BoxFuture<'a, String> {
        Box::pin(async move {
            generate_copy_name(
                self.models.clone(),
                self.defaults.clone(),
                self.llm.clone(),
                context,
            )
            .await
        })
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

/// The hidden `project-copy-name` agent (`handlers/project-copy.ts:7-14`).
fn copy_name_agent() -> opencode_core::session::agents::AgentInfo {
    opencode_core::session::agents::AgentInfo {
        name: "project-copy-name".to_string(),
        description: None,
        mode: opencode_core::tool::def::AgentMode::Primary,
        native: Some(true),
        hidden: Some(true),
        top_p: None,
        temperature: None,
        color: None,
        permission: Default::default(),
        model: None,
        variant: None,
        prompt: Some(String::new()),
        options: Default::default(),
        steps: None,
    }
}

/// `slugify` (`handlers/project-copy.ts:71-78`).
fn copy_name_slugify(input: &str) -> String {
    let mut out = String::new();
    for char in input.trim().to_lowercase().chars() {
        if char.is_ascii_lowercase() || char.is_ascii_digit() {
            out.push(char);
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// `generateName` (`handlers/project-copy.ts:22-69`) — the one-shot LLM
/// stream; every failure path degrades to `Slug.create()`.
async fn generate_copy_name(
    models: Arc<dyn opencode_core::session::r#loop::ModelSource>,
    defaults: Arc<dyn opencode_core::session::prompt_input::Models>,
    llm: Arc<dyn LlmStream>,
    context: &str,
) -> String {
    let text = context.trim();
    if text.is_empty() {
        return opencode_core::session::agents::slug_create();
    }
    let agent = copy_name_agent();
    let resolve = async {
        // `provider.defaultModel()` — the catch swallows errors into the
        // `Slug.create()` fallback.
        let fallback = defaults.default_model().await.map_err(|_| ())?;
        let model = match models.get_small_model(&fallback.provider_id).await {
            Some(model) => model,
            None => models
                .get_model(&fallback.provider_id, &fallback.id, "")
                .await
                .map_err(|_| ())?,
        };
        let session_id = opencode_core::session::ids::SessionId::descending(None).expect("ses id");
        let message_id = opencode_core::session::ids::MessageId::ascending(None).expect("msg id");
        let user = opencode_schema::session_v1::V1Message::User {
            id: message_id.clone(),
            session_id: session_id.clone(),
            time: opencode_schema::session_v1::UserTime {
                created: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or_default() as f64,
            },
            format: None,
            summary: None,
            agent: agent.name.clone(),
            model: opencode_schema::session_v1::V1UserModel {
                provider_id: model.llm.provider_id.clone(),
                model_id: model.llm.id.clone(),
                variant: None,
            },
            system: None,
            tools: None,
        };
        let input = opencode_core::session::llm::StreamInput {
            user,
            session_id,
            parent_session_id: None,
            project_id: None,
            client: "cli".to_string(),
            model: model.llm.clone(),
            agent: agent.clone(),
            permission: None,
            system: Vec::new(),
            messages: vec![opencode_llm::schema::messages::Message::user(format!(
                "Generate a short 2-3 word name that describes this task:\n{text}"
            ))],
            small: true,
            tools: Vec::new(),
            retries: Some(2),
            tool_choice: None,
        };
        Ok::<_, ()>((
            llm.stream(input),
            model.llm.id.clone(),
            model.llm.provider_id.clone(),
        ))
    };
    let Ok((stream, _, _)) = resolve.await else {
        return opencode_core::session::agents::slug_create();
    };
    // `Stream.filter(LLMEvent.is.textDelta).map((e) => e.text).mkString`
    let mut result = String::new();
    let mut stream = stream;
    use futures::StreamExt;
    while let Some(event) = stream.next().await {
        match event {
            Ok(opencode_llm::schema::events::LlmEvent::TextDelta { text, .. }) => {
                result.push_str(&text);
            }
            Err(_) => return opencode_core::session::agents::slug_create(),
            _ => {}
        }
    }
    let output = result.trim();
    if output.is_empty() {
        return opencode_core::session::agents::slug_create();
    }
    let words: Vec<&str> = output.split_whitespace().take(3).collect();
    copy_name_slugify(&words.join(" "))
}

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
    /// The provider runtime inputs — the models-dev catalog and the
    /// `auth.json` store (M7.7).
    pub runtime: EngineRuntime,
    /// Injectable provider-runtime seams — production runs the M7.7
    /// runtime; the e2e tests script a mock LLM here.
    pub seams: EngineSeams,
}

/// The M7.7 provider-runtime inputs.
#[derive(Clone)]
pub struct EngineRuntime {
    pub catalog: crate::state::CatalogSource,
    pub auth: Arc<dyn crate::state::AuthStore>,
}

/// Provider-runtime + share seams: `None` falls back to the unwired M7.7
/// stubs / the real HTTP share client.
#[derive(Clone, Default)]
pub struct EngineSeams {
    pub llm: Option<Arc<dyn LlmStream>>,
    pub models: Option<Arc<dyn ModelSource>>,
    pub input_models: Option<Arc<dyn InputModels>>,
    pub share_http: Option<Arc<dyn opencode_core::share::ShareHttp>>,
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

    // Provider-runtime seams — the M7.7 production runtime (models +
    // route sender) unless the e2e tests script a mock LLM here.
    let runtime_models = crate::provider_runtime::RuntimeModels::new(
        &crate::provider::load_catalog(&input.runtime.catalog)?,
        &input.config,
        input.runtime.auth.as_ref(),
        &input.paths,
    )?;
    let models: Arc<dyn ModelSource> = input
        .seams
        .models
        .clone()
        .unwrap_or_else(|| Arc::new(runtime_models.clone()));
    let input_models: Arc<dyn InputModels> = input
        .seams
        .input_models
        .clone()
        .unwrap_or_else(|| Arc::new(runtime_models.clone()));
    let llm: Arc<dyn LlmStream> = input.seams.llm.clone().unwrap_or_else(|| {
        Arc::new(opencode_core::session::llm::LlmStreamImpl::new(Arc::new(
            crate::provider_runtime::RouteLlmSender,
        )))
    });
    // M7.3: the production git snapshot behind the M5 `Snapshot` seam
    // (`snapshot/index.ts`) — non-git instances fall back to the no-op.
    let snapshot: Arc<dyn Snapshot> = services
        .instance_location()
        .map(|location| {
            opencode_core::session::snapshot::GitSnapshot::new(
                opencode_core::session::snapshot::GitSnapshotInput {
                    directory: input.directory.clone(),
                    worktree: input.worktree.clone(),
                    project_id: location.project.id.clone(),
                    vcs_is_git: matches!(
                        location.project.vcs,
                        Some(opencode_schema::project::ProjectVcs::Git)
                    ),
                    snapshot_enabled: input.config.snapshot != Some(false),
                    data: input.paths.data.clone(),
                },
            ) as Arc<dyn Snapshot>
        })
        .unwrap_or_else(|| Arc::new(opencode_core::session::snapshot::DisabledSnapshot));

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
    // M7.6: the per-instance MCP service (mcp/index.ts MCP.Service).
    let mcp_service = Arc::new(opencode_core::mcp::McpService::new(
        opencode_core::mcp::McpServiceInput {
            directory: input.directory.clone(),
            data_dir: input.paths.data.clone(),
            mcp: input.config.mcp.clone().unwrap_or_default(),
            mcp_timeout: input
                .config
                .experimental
                .as_ref()
                .and_then(|experimental| experimental.mcp_timeout)
                .map(|timeout| timeout.get()),
            events: Some(services.events.clone()),
        },
    ));
    // Websearch env keys are read once at boot (WebSearchEnv::default).
    let websearch = opencode_core::tool::websearch::websearch_tool(
        truncate.clone(),
        agents.clone(),
        Arc::new(ReqwestMcpHttpClient),
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
            output_token_max: crate::provider_runtime::output_token_max(),
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
        llm: llm.clone(),
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
        mcp: Arc::new(EngineMcp(mcp_service.clone())),
        lsp: Arc::new(NoLsp),
        images: Arc::new(opencode_core::session::prompt_input::NoResize),
        data_dir: input.paths.data.clone(),
        project_id: None,
        client: client_env(),
        experimental_plan_mode: false,
        vcs: false,
    });

    ops.bind(prompt.clone());

    // M7.5: the share network service (share-next.ts) behind its HTTP +
    // account seams, wrapped in the SessionShare gate/auto-share service
    // (share/session.ts). The account seam defaults to no active account;
    // the model seam is the provider runtime (M7.7).
    let share_next = opencode_core::share::ShareNext::new(opencode_core::share::ShareInput {
        storage: services.storage.clone(),
        sessions: services.sessions.clone(),
        events: services.events.clone(),
        base_url: input
            .config
            .enterprise
            .as_ref()
            .and_then(|enterprise| enterprise.url.clone())
            .unwrap_or_else(|| opencode_core::share::DEFAULT_BASE_URL.to_string()),
        disabled: opencode_core::share::share_disabled(),
        directory: input.directory.to_string_lossy().into_owned(),
        http: input
            .seams
            .share_http
            .clone()
            .unwrap_or_else(|| Arc::new(opencode_core::share::HttpShareClient)),
        account: Arc::new(opencode_core::share::NoAccount),
        models: Arc::new(runtime_models.clone()),
        flush_delay: opencode_core::share::FLUSH_DELAY,
    });
    share_next.init();
    let share = Arc::new(opencode_core::share::SessionShare::new(
        share_next,
        services.sessions.clone(),
        input.config.share,
        bool_env("OPENCODE_AUTO_SHARE"),
    ));

    Ok(Arc::new(ProductionEngine {
        prompt: prompt.clone(),
        revert: revert.clone(),
        summary: summary.clone(),
        registry: registry.clone(),
        background: input.background.clone(),
        share,
        mcp: mcp_service,
        models,
        defaults: Arc::new(runtime_models),
        llm,
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

    /// The `McpSource` seam — the `/mcp` route family and
    /// `GET /experimental/resource` resolve the instance's MCP service
    /// (M7.6).
    pub fn mcp_source(self: &Arc<Self>) -> Arc<dyn crate::state::McpSource> {
        Arc::new(StoreMcp(Arc::clone(self)))
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

struct StoreMcp(Arc<EngineStore>);

impl crate::state::McpSource for StoreMcp {
    fn service(
        &self,
        location: &LocationContext,
    ) -> Result<Arc<opencode_core::mcp::McpService>, ServerError> {
        self.0
            .engine(&location.services)
            .map(|engine| engine.mcp.clone())
            .ok_or_else(|| engine_missing(&location.directory))
    }
}

/// Production MCP HTTP client for the websearch tool (M7.6) — a plain
/// reqwest POST, mirroring TS's Effect `HttpClient`.
struct ReqwestMcpHttpClient;

impl opencode_core::tool::mcp_websearch::McpHttpClient for ReqwestMcpHttpClient {
    fn post<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(String, String)>,
        body: &'a str,
    ) -> opencode_core::tool::def::BoxFuture<
        'a,
        Result<
            opencode_core::tool::mcp_websearch::McpHttpResponse,
            opencode_core::tool::error::ToolError,
        >,
    > {
        Box::pin(async move {
            let client = reqwest::Client::new();
            let mut request = client.post(url).body(body.to_string());
            for (key, value) in headers {
                request = request.header(&key, value);
            }
            let response = request
                .send()
                .await
                .map_err(|err| opencode_core::tool::error::ToolError::Failed(err.to_string()))?;
            let status = response.status().as_u16();
            let text = response
                .text()
                .await
                .map_err(|err| opencode_core::tool::error::ToolError::Failed(err.to_string()))?;
            if !(200..300).contains(&status) {
                return Err(opencode_core::tool::error::ToolError::Failed(format!(
                    "MCP request failed with status {status}"
                )));
            }
            Ok(opencode_core::tool::mcp_websearch::McpHttpResponse { body: text })
        })
    }
}

#[cfg(test)]
mod generate_name_tests {
    use super::*;
    use opencode_core::session::llm::{LlmEventStream, LlmModel, StreamInput};
    use opencode_core::session::prompt_input::Models;
    use opencode_core::session::r#loop::{ModelSource, ResolvedModel};
    use opencode_schema::model::ModelInfo;

    struct FixedModels;

    impl Models for FixedModels {
        fn get_model<'a>(
            &'a self,
            _provider_id: &'a str,
            _model_id: &'a str,
        ) -> opencode_core::tool::def::BoxFuture<'a, Result<ModelInfo, CoreError>> {
            unreachable!("default_model drives resolution")
        }

        fn default_model(
            &self,
        ) -> opencode_core::tool::def::BoxFuture<'static, Result<ModelInfo, CoreError>> {
            Box::pin(async {
                Ok(serde_json::from_value(serde_json::json!({
                    "id": "model",
                    "providerID": "prov",
                    "name": "model",
                    "api": { "id": "", "type": "native", "settings": {} },
                    "capabilities": { "tools": false, "input": [], "output": [] },
                    "request": { "headers": {}, "body": {} },
                    "variants": [],
                    "time": { "released": 0 },
                    "cost": [],
                    "status": "active",
                    "enabled": true,
                    "limit": { "context": 1000, "output": 1000 },
                }))
                .expect("ModelInfo"))
            })
        }
    }

    struct FixedSource;

    impl ModelSource for FixedSource {
        fn get_model<'a>(
            &'a self,
            _provider_id: &'a str,
            _model_id: &'a str,
            _session_id: &'a str,
        ) -> futures::future::BoxFuture<
            'a,
            Result<ResolvedModel, opencode_core::session::r#loop::LoopError>,
        > {
            unreachable!("small model drives resolution")
        }

        fn get_small_model<'a>(
            &'a self,
            _provider_id: &'a str,
        ) -> futures::future::BoxFuture<'a, Option<ResolvedModel>> {
            Box::pin(async { Some(resolved()) })
        }
    }

    fn resolved() -> ResolvedModel {
        ResolvedModel {
            llm: LlmModel {
                id: "model".to_string(),
                provider_id: "prov".to_string(),
                api_id: String::new(),
                api_npm: String::new(),
                temperature_capable: false,
                headers: Default::default(),
                options: Default::default(),
                context_limit: 1000.0,
                output_limit: 1000.0,
                output_token_max: None,
            },
            cost: opencode_core::session::usage::ModelCost::free(),
            limits: opencode_core::session::overflow::ModelLimits {
                context: 1000.0,
                input: None,
                output: 1000.0,
            },
            output_token_max: None,
        }
    }

    struct TextLlm;

    impl LlmStream for TextLlm {
        fn stream(&self, input: StreamInput) -> LlmEventStream {
            let prompt = input
                .messages
                .first()
                .and_then(|message| {
                    message.content.first().and_then(|part| match part {
                        opencode_llm::schema::messages::ContentPart::Text { text, .. } => {
                            Some(text.clone())
                        }
                        _ => None,
                    })
                })
                .unwrap_or_default();
            Box::pin(futures::stream::iter(vec![
                Ok(opencode_llm::schema::events::LlmEvent::TextStart {
                    id: "c1".to_string(),
                    provider_metadata: None,
                }),
                Ok(opencode_llm::schema::events::LlmEvent::TextDelta {
                    id: "c1".to_string(),
                    text: format!(" {prompt}"),
                    provider_metadata: None,
                }),
                Ok(opencode_llm::schema::events::LlmEvent::TextEnd {
                    id: "c1".to_string(),
                    provider_metadata: None,
                }),
            ]))
        }
    }

    #[tokio::test]
    async fn empty_context_falls_back_to_slug() {
        let name = generate_copy_name(
            Arc::new(FixedSource),
            Arc::new(FixedModels),
            Arc::new(TextLlm),
            "   ",
        )
        .await;
        assert!(!name.is_empty());
    }

    #[tokio::test]
    async fn three_words_slugified() {
        let name = generate_copy_name(
            Arc::new(FixedSource),
            Arc::new(FixedModels),
            Arc::new(TextLlm),
            "do the thing now",
        )
        .await;
        // The mock echoes the prompt as the stream text: "Generate a short
        // 2-3 word name that describes this task:\n do the thing now" —
        // the first 3 whitespace words after the trim, slugified.
        assert_eq!(name, "generate-a-short");
    }
}
