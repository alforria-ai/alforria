//! Agent loop — port of the `runLoop` closure (prompt.ts:1081-1339) plus
//! `title` (prompt.ts:193-250), `isOrphanedInterruptedTool`
//! (prompt.ts:96-99) and the structured-output tool
//! (prompt.ts:1565-1589).
//!
//! Seams the not-yet-landed milestones bind to (spec §2.1):
//!
//! * [`Compaction`] — the M5.6 compaction service (process/create/prune).
//! * [`Subtasks`] — the M5.7 subtask/TaskOps driver (`handleSubtask`).
//! * [`SystemPrompts`] — `sys.environment`/`sys.skills`/`sys.mcp`.
//! * [`ModelSource`] — `getModel` / small-model resolution.
//!
//! Not ported: the `Image` normalization in `prompt` (the loop drives the
//! processor's `process`, not `prompt`); plugin hooks are no-op seams
//! (spec §2.6). `state.ensureRunning` wiring is the M5.5 engine's concern
//! — [`run_loop`] is the raw loop body.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use futures::StreamExt;
use opencode_llm::schema::messages::Message;
use opencode_schema::session_v1::{
    AssistantError, OutputFormat, V1Message, V1Part, V1SessionInfo, V1StepTokens, V1TokenCache,
    V1UserModel,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::event::bus::{EventBus, PublishOptions};
use crate::session::agents::AgentRegistry;
use crate::session::error::SessionError;
use crate::session::event_definitions::SESSION_ERROR;
use crate::session::instruction::Instruction;
use crate::session::llm::{LlmModel, LlmStream, LlmTool, LlmToolOutput, StreamInput};
use crate::session::message::{filter_compacted, latest, MessageStore, WithParts};
use crate::session::overflow::{is_overflow, IsOverflowInput, ModelLimits};
use crate::session::processor::{
    AskPermission, Handle as ProcessorHandle, ProcessResult, Processor, ProcessorDeps,
    ProcessorInput, ProcessorModel, SummarySummarize,
};
use crate::session::reminders::{apply as apply_reminders, RemindersInput};
use crate::session::render::{to_model_messages, RenderModel};
use crate::session::status::SessionStatusService;
use crate::session::store::{is_default_title, SessionStore};
use crate::session::usage::ModelCost;
use crate::tool::def::InstanceContext;
use crate::tool::registry::ToolRegistry;
use crate::tool::task::TaskOps;
use crate::Clock;

/// Vendored verbatim from `packages/core/src/session/runner/max-steps.ts`.
pub const MAX_STEPS_PROMPT: &str = "CRITICAL - MAXIMUM STEPS REACHED

The maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.

STRICT REQUIREMENTS:
1. Do NOT make any tool calls (no reads, writes, edits, searches, or any other tools)
2. MUST provide a text response summarizing work done so far
3. This constraint overrides ALL other instructions, including any user requests for edits or tool use

Response must include:
- Statement that maximum steps for this agent have been reached
- Summary of what has been accomplished so far
- List of any remaining tasks that were not completed
- Recommendations for what should be done next

Any attempt to use tools is a critical violation. Respond with text ONLY.";

const STRUCTURED_OUTPUT_DESCRIPTION: &str =
    "Use this tool to return your final response in the requested structured format.

IMPORTANT:
- You MUST call this tool exactly once at the end of your response
- The input must be valid JSON matching the required schema
- Complete all necessary research and tool calls BEFORE calling this tool
- This tool provides your final answer - no further actions are taken after calling it";

const STRUCTURED_OUTPUT_SYSTEM_PROMPT: &str = "IMPORTANT: The user has requested structured output. You MUST use the StructuredOutput tool to provide your final response. Do NOT respond with plain text - you MUST call the StructuredOutput tool with your answer formatted according to the schema.";

/// prompt.ts:244 — the think-tag strip applied to generated titles.
fn think_tag() -> &'static regex::Regex {
    static THINK_TAG: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    THINK_TAG
        .get_or_init(|| regex::Regex::new(r"(?s)<think>[\s\S]*?</think>\s*").expect("valid regex"))
}

// -------------------------------------------------------------------------
// Seams (spec §2.1)
// -------------------------------------------------------------------------

/// The resolved `Provider.Model` the loop runs with.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub llm: LlmModel,
    pub cost: ModelCost,
    pub limits: ModelLimits,
    pub output_token_max: Option<f64>,
}

impl ResolvedModel {
    /// The render-model slice `MessageV2.toModelMessages` consumes.
    pub fn render(&self) -> RenderModel {
        RenderModel {
            provider_id: self.llm.provider_id.clone(),
            id: self.llm.id.clone(),
            api_npm: self.llm.api_npm.clone(),
            api_id: self.llm.api_id.clone(),
        }
    }

    /// The processor slice (usage / overflow accounting).
    pub fn processor_model(&self) -> ProcessorModel {
        ProcessorModel {
            id: self.llm.id.clone(),
            provider_id: self.llm.provider_id.clone(),
            cost: self.cost.clone(),
            limits: self.limits,
            output_token_max: self.output_token_max,
        }
    }
}

/// `provider.getModel` / `provider.getSmallModel` (the M5.2 provider
/// service seam).
pub trait ModelSource: Send + Sync {
    /// `getModel(providerID, modelID, sessionID)` — may resolve variants.
    fn get_model<'a>(
        &'a self,
        provider_id: &'a str,
        model_id: &'a str,
        session_id: &'a str,
    ) -> BoxFuture<'a, Result<ResolvedModel, LoopError>>;

    /// `getSmallModel(providerID)` — `None` when the provider has no
    /// small-model override.
    fn get_small_model<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Option<ResolvedModel>>;
}

/// `compaction.create` input (prompt.ts:1300-1305).
#[derive(Debug, Clone)]
pub struct CompactionCreate {
    pub session_id: String,
    pub agent: String,
    pub model: V1UserModel,
    pub auto: bool,
    pub overflow: bool,
}

/// `compaction.process` input (prompt.ts:1149-1157).
#[derive(Debug, Clone)]
pub struct CompactionProcess {
    pub messages: Vec<WithParts>,
    pub parent_id: String,
    pub session_id: String,
    pub auto: bool,
    pub overflow: Option<bool>,
}

/// The M5.6 compaction service seam.
pub trait Compaction: Send + Sync {
    /// Returns `Ok(false)` for the `"stop"` outcome.
    fn process<'a>(&'a self, input: CompactionProcess)
        -> BoxFuture<'a, Result<bool, SessionError>>;
    fn create<'a>(&'a self, input: CompactionCreate) -> BoxFuture<'a, Result<(), SessionError>>;
    fn prune<'a>(&'a self, session_id: String) -> BoxFuture<'a, ()>;
}

/// `handleSubtask` input (prompt.ts:255-263) — the M5.7 seam.
#[derive(Debug, Clone)]
pub struct SubtaskInput {
    pub task: V1Part,
    pub model: ResolvedModel,
    pub last_user: V1Message,
    pub session_id: String,
    pub session: V1SessionInfo,
    pub messages: Vec<WithParts>,
    /// The loop's abort signal — drives the `Effect.onInterrupt` path
    /// (`part.state.status === "running"` → error part `Cancelled`).
    pub cancel: CancellationToken,
}

/// The M5.7 subtask driver seam.
pub trait Subtasks: Send + Sync {
    fn handle<'a>(&'a self, input: SubtaskInput) -> BoxFuture<'a, Result<(), LoopError>>;
}

/// `sys.environment` / `sys.skills` / `sys.mcp` (the M5.2 prompt-input
/// machinery).
pub trait SystemPrompts: Send + Sync {
    fn environment(&self, model: &LlmModel) -> Vec<String>;
    fn skills(&self, agent: &str) -> Option<String>;
    fn mcp(
        &self,
        agent: &str,
        permission: &Option<opencode_schema::permission_v1::PermissionV1Ruleset>,
    ) -> Option<String>;
}

/// No-op default (M5.2 not landed).
pub struct NoSystemPrompts;

impl SystemPrompts for NoSystemPrompts {
    fn environment(&self, _model: &LlmModel) -> Vec<String> {
        Vec::new()
    }
    fn skills(&self, _agent: &str) -> Option<String> {
        None
    }
    fn mcp(
        &self,
        _agent: &str,
        _permission: &Option<opencode_schema::permission_v1::PermissionV1Ruleset>,
    ) -> Option<String> {
        None
    }
}

// -------------------------------------------------------------------------
// Loop input
// -------------------------------------------------------------------------

/// The loop's failure surface.
#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("{0}")]
    Unknown(String),
}

impl From<crate::CoreError> for LoopError {
    fn from(error: crate::CoreError) -> Self {
        SessionError::from(error).into()
    }
}

/// Everything `runLoop` closes over.
pub struct LoopDeps {
    pub sessions: SessionStore,
    pub messages: MessageStore,
    pub events: Arc<EventBus>,
    pub status: Arc<SessionStatusService>,
    pub agents: AgentRegistry,
    pub models: Arc<dyn ModelSource>,
    pub llm: Arc<dyn LlmStream>,
    pub snapshot: Arc<dyn crate::session::snapshot::Snapshot>,
    pub compaction: Arc<dyn Compaction>,
    pub subtasks: Arc<dyn Subtasks>,
    pub summary: Arc<dyn SummarySummarize>,
    pub instruction: Arc<Instruction>,
    pub systems: Arc<dyn SystemPrompts>,
    pub registry: ToolRegistry,
    pub permission: Arc<dyn AskPermission>,
    pub prompt_ops: Arc<dyn TaskOps>,
    pub config: Arc<crate::config::schema::Config>,
    pub clock: Arc<dyn Clock>,
    pub instance: InstanceContext,
    /// `flags.experimentalPlanMode`.
    pub experimental_plan_mode: bool,
    /// `instance.project.vcs` (plan path).
    pub vcs: bool,
    /// `Global.Path.data` (plan path).
    pub data_dir: std::path::PathBuf,
    /// The instance project id (`x-opencode-project`).
    pub project_id: Option<String>,
    /// `RuntimeFlags.client` (`x-opencode-client`).
    pub client: String,
}

// -------------------------------------------------------------------------
// runLoop (prompt.ts:1081-1339)
// -------------------------------------------------------------------------

/// The loop outcome of one pass through the inner body.
enum Outcome {
    Break,
    Continue,
}

/// `runLoop` — drives prompt → processor → tools until the assistant
/// finishes. Returns the last assistant message.
pub async fn run_loop(
    deps: &LoopDeps,
    session_id: &str,
    cancel: &CancellationToken,
) -> Result<WithParts, LoopError> {
    let structured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let mut step: u64 = 0;
    let session = deps.sessions.get(session_id)?;

    let result: Result<WithParts, LoopError> = loop {
        deps.status.set(
            session_id,
            opencode_schema::session_status::SessionStatusInfo::Busy,
        )?;

        let msgs = filter_compacted(deps.messages.stream(session_id)?);
        let latest = latest(&msgs);

        let last_user = latest.user.clone().ok_or_else(|| {
            LoopError::Unknown(
                "No user message found in stream. This should never happen.".to_string(),
            )
        })?;
        let last_user_id = crate::session::message::message_id(&last_user).to_string();

        // Some providers return "stop" even when the assistant message
        // contains tool calls. Keep the loop running so tool results can
        // be sent back to the model, but ignore cleanup-marked interrupted
        // orphans (prompt.ts:1101-1112).
        let last_assistant_msg = latest
            .assistant
            .as_ref()
            .and_then(|assistant| msgs.iter().rev().find(|msg| msg.info == *assistant));
        let has_tool_calls = last_assistant_msg
            .map(|msg| {
                msg.parts.iter().any(|part| {
                    matches!(part, V1Part::Tool { .. })
                        && !part_provider_executed(part)
                        && !is_orphaned_interrupted_tool(part)
                })
            })
            .unwrap_or(false);

        if let Some(assistant) = latest.assistant.as_ref() {
            let finish = assistant_finish(assistant);
            let parent_id = match assistant {
                V1Message::Assistant { parent_id, .. } => parent_id.clone(),
                _ => String::new(),
            };
            if let Some(finish) = finish {
                if finish != "tool-calls"
                    && finish != "unknown"
                    && !has_tool_calls
                    && parent_id == last_user_id
                {
                    break last_assistant(deps, session_id).await;
                }
            }
        }

        step += 1;
        if step == 1 {
            fork_title(deps, session.clone(), msgs.clone(), &last_user);
        }

        let (provider_id, model_id) = match &last_user {
            V1Message::User { model, .. } => (model.provider_id.clone(), model.model_id.clone()),
            _ => return Err(LoopError::Unknown("user message required".to_string())),
        };
        let model = deps
            .models
            .get_model(&provider_id, &model_id, session_id)
            .await?;

        // Pending subtask / compaction tasks (prompt.ts:1141-1170).
        let task = latest.tasks.last().cloned();
        if let Some(task) = task {
            if matches!(task, V1Part::Subtask { .. }) {
                deps.subtasks
                    .handle(SubtaskInput {
                        task,
                        model,
                        last_user: last_user.clone(),
                        session_id: session_id.to_string(),
                        session: session.clone(),
                        messages: msgs.clone(),
                        cancel: cancel.clone(),
                    })
                    .await?;
                continue;
            }
            if let V1Part::Compaction { auto, overflow, .. } = &task {
                let keep_going = deps
                    .compaction
                    .process(CompactionProcess {
                        messages: msgs.clone(),
                        parent_id: last_user_id.clone(),
                        session_id: session_id.to_string(),
                        auto: *auto,
                        overflow: *overflow,
                    })
                    .await?;
                if !keep_going {
                    break last_assistant(deps, session_id).await;
                }
                continue;
            }
        }

        // Overflow → auto-compaction (prompt.ts:1165-1170).
        if let Some(V1Message::Assistant {
            summary, tokens, ..
        }) = latest.finished.as_ref()
        {
            if *summary != Some(true)
                && is_overflow(&IsOverflowInput {
                    cfg: &deps.config,
                    tokens,
                    model: &model.limits,
                    output_token_max: model.output_token_max,
                })
            {
                deps.compaction
                    .create(compaction_create(session_id, &last_user, true, false))
                    .await?;
                continue;
            }
        }

        // Agent lookup (prompt.ts:1172-1181).
        let user_agent = match &last_user {
            V1Message::User { agent, .. } => agent.clone(),
            _ => return Err(LoopError::Unknown("user message required".to_string())),
        };
        let Some(agent) = deps.agents.get(&user_agent).cloned() else {
            let available: Vec<String> = deps
                .agents
                .list()
                .into_iter()
                .filter(|a| a.is_visible())
                .map(|a| a.name)
                .collect();
            let hint = if available.is_empty() {
                String::new()
            } else {
                format!(" Available agents: {}", available.join(", "))
            };
            publish_unknown_error(
                &deps.events,
                session_id,
                &format!("Agent not found: \"{}\".{}", user_agent, hint),
            )?;
            return Err(LoopError::Unknown(format!(
                "Agent not found: \"{}\".{}",
                user_agent, hint
            )));
        };
        let max_steps = agent.steps.unwrap_or(f64::INFINITY);
        let is_last_step = step as f64 >= max_steps;

        let msgs = apply_reminders(
            RemindersInput {
                messages: msgs,
                agent: agent.clone(),
                session: session.clone(),
                experimental_plan_mode: deps.experimental_plan_mode,
                vcs: deps.vcs,
                worktree: deps.instance.worktree.clone(),
                data_dir: deps.data_dir.clone(),
            },
            &deps.sessions,
        )?;

        // The assistant message under construction (prompt.ts:1190-1204).
        let assistant = V1Message::Assistant {
            id: crate::session::ids::MessageId::ascending(None)?,
            session_id: session_id.to_string(),
            time: opencode_schema::session_v1::AssistantTime {
                created: deps.clock.now_ms(),
                completed: None,
            },
            error: None,
            parent_id: last_user_id.clone(),
            model_id: model.llm.id.clone(),
            provider_id: model.llm.provider_id.clone(),
            mode: agent.name.clone(),
            agent: agent.name.clone(),
            path: opencode_schema::session_v1::V1Path {
                cwd: deps.instance.directory.to_string_lossy().into_owned(),
                root: deps.instance.worktree.to_string_lossy().into_owned(),
            },
            summary: None,
            cost: 0.0,
            tokens: empty_tokens(),
            structured: None,
            variant: match &last_user {
                V1Message::User { model, .. } => model.variant.clone(),
                _ => None,
            },
            finish: None,
        };
        let assistant_id = crate::session::message::message_id(&assistant).to_string();
        deps.sessions.update_message(&assistant)?;

        let processor = Processor::create(
            ProcessorDeps {
                sessions: deps.sessions.clone(),
                messages: deps.messages.clone(),
                status: deps.status.clone(),
                events: deps.events.clone(),
                snapshot: deps.snapshot.clone(),
                agents: deps.agents.clone(),
                config: deps.config.clone(),
                llm: deps.llm.clone(),
                permission: deps.permission.clone(),
                summary: deps.summary.clone(),
                clock: deps.clock.clone(),
            },
            ProcessorInput {
                assistant_message: assistant,
                session_id: session_id.to_string(),
                model: model.processor_model(),
            },
        )
        .await;

        let outcome = {
            // `SessionTools.resolve` inputs (prompt.ts:1222-1254).
            let bypass_agent_check = msgs
                .iter()
                .rev()
                .find(|msg| matches!(msg.info, V1Message::User { .. }))
                .map(|msg| {
                    msg.parts
                        .iter()
                        .any(|part| matches!(part, V1Part::Agent { .. }))
                })
                .unwrap_or(false);

            let resolve_deps = crate::session::tools::ResolveDeps {
                registry: deps.registry.clone(),
                permission: deps.permission.clone(),
                instance: InstanceContext {
                    directory: deps.instance.directory.clone(),
                    worktree: deps.instance.worktree.clone(),
                },
                clock: deps.clock.clone(),
            };
            let mut tools = crate::session::tools::resolve(
                &resolve_deps,
                crate::session::tools::ResolveInput {
                    agent: &agent,
                    model: &model.llm,
                    session: &session,
                    processor: &processor,
                    messages: &msgs,
                    bypass_agent_check,
                    prompt_ops: deps.prompt_ops.clone(),
                    cancel: cancel.clone(),
                },
            )
            .await?;

            let format = match &last_user {
                V1Message::User { format, .. } => format.clone(),
                _ => None,
            };
            let json_schema_format = matches!(format, Some(OutputFormat::JsonSchema { .. }));
            if let Some(OutputFormat::JsonSchema { schema, .. }) = &format {
                tools.push(create_structured_output_tool(
                    Value::Object(schema.clone()),
                    structured.clone(),
                ));
            }

            if step == 1 {
                let summary = deps.summary.clone();
                let fork_session_id = session_id.to_string();
                let fork_message_id = last_user_id.clone();
                tokio::spawn(async move {
                    summary.summarize(&fork_session_id, &fork_message_id).await;
                });
            }

            // System prompts (prompt.ts:1258-1277).
            let mut system = deps.systems.environment(&model.llm);
            system.extend(deps.instruction.system());
            if let Some(mcp) = deps.systems.mcp(&agent.name, &session.permission) {
                system.push(mcp);
            }
            if let Some(skills) = deps.systems.skills(&agent.name) {
                system.push(skills);
            }
            if json_schema_format {
                system.push(STRUCTURED_OUTPUT_SYSTEM_PROMPT.to_string());
            }

            let mut model_messages = to_model_messages(&msgs, &model.render(), None);
            if is_last_step {
                model_messages.push(Message::assistant(MAX_STEPS_PROMPT));
            }

            let result = processor
                .process(
                    StreamInput {
                        user: last_user.clone(),
                        session_id: session_id.to_string(),
                        parent_session_id: session.parent_id.clone(),
                        project_id: deps.project_id.clone(),
                        client: deps.client.clone(),
                        model: model.llm.clone(),
                        agent: agent.clone(),
                        permission: session.permission.clone(),
                        system,
                        messages: model_messages,
                        small: false,
                        tools,
                        retries: None,
                        tool_choice: if json_schema_format {
                            Some("required")
                        } else {
                            None
                        },
                    },
                    cancel.clone(),
                )
                .await;

            let result = match result {
                // `Effect.onInterrupt(finalizeInterruptedAssistant)`
                // (prompt.ts:1206-1212 + 1337-1338).
                Err(_) => {
                    finalize_interrupted_assistant(deps, &processor).await?;
                    deps.instruction.clear(&assistant_id);
                    return Err(LoopError::Cancelled);
                }
                Ok(result) => result,
            };

            // Structured output captured (prompt.ts:1282-1289).
            let captured = structured.lock().unwrap().take();
            if let Some(captured) = captured {
                let mut message = processor.message();
                if let V1Message::Assistant {
                    structured: slot,
                    finish,
                    ..
                } = &mut message
                {
                    *slot = Some(captured);
                    if finish.is_none() {
                        *finish = Some("stop".to_string());
                    }
                }
                deps.sessions.update_message(&message)?;
                // `Effect.ensuring(instruction.clear(handle.message.id))`.
                deps.instruction.clear(&assistant_id);
                Outcome::Break
            } else {
                // Content-filter / structured-output failure
                // (prompt.ts:1291-1319).
                let message = processor.message();
                let outcome =
                    check_message_error(deps, session_id, &message, &last_user, json_schema_format)
                        .await?;
                match outcome {
                    Some(outcome) => {
                        // `Effect.ensuring(instruction.clear(handle.message.id))`.
                        deps.instruction.clear(&assistant_id);
                        outcome
                    }
                    None => match result {
                        ProcessResult::Stop => {
                            // `Effect.ensuring(instruction.clear(...))`.
                            deps.instruction.clear(&assistant_id);
                            Outcome::Break
                        }
                        ProcessResult::Compact => {
                            let finish = match &message {
                                V1Message::Assistant { finish, .. } => finish.clone(),
                                _ => None,
                            };
                            deps.instruction.clear(&assistant_id);
                            deps.compaction
                                .create(compaction_create(
                                    session_id,
                                    &last_user,
                                    true,
                                    finish.is_none(),
                                ))
                                .await?;
                            Outcome::Continue
                        }
                        ProcessResult::Continue => {
                            deps.instruction.clear(&assistant_id);
                            Outcome::Continue
                        }
                    },
                }
            }
        };

        match outcome {
            Outcome::Break => break last_assistant(deps, session_id).await,
            Outcome::Continue => continue,
        }
    };

    // `compaction.prune` fork (prompt.ts:1335).
    {
        let compaction = deps.compaction.clone();
        let fork_session_id = session_id.to_string();
        tokio::spawn(async move {
            compaction.prune(fork_session_id).await;
        });
    }

    result
}

// -------------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------------

/// `compaction.create` input builder (`prompt.ts:1300-1305`).
#[allow(clippy::too_many_arguments)]
fn compaction_create(
    session_id: &str,
    last_user: &V1Message,
    auto: bool,
    overflow: bool,
) -> CompactionCreate {
    let (agent, model) = match last_user {
        V1Message::User { agent, model, .. } => (agent.clone(), model.clone()),
        _ => (
            String::new(),
            V1UserModel {
                provider_id: String::new(),
                model_id: String::new(),
                variant: None,
            },
        ),
    };
    CompactionCreate {
        session_id: session_id.to_string(),
        agent,
        model,
        auto,
        overflow,
    }
}

/// The content-filter / structured-output failure checks
/// (prompt.ts:1291-1319). `Ok(Some(outcome))` — the loop takes over the
/// outcome.
async fn check_message_error(
    deps: &LoopDeps,
    session_id: &str,
    message: &V1Message,
    _last_user: &V1Message,
    json_schema_format: bool,
) -> Result<Option<Outcome>, LoopError> {
    let (finish, error) = match message {
        V1Message::Assistant { finish, error, .. } => (finish.clone(), error.clone()),
        _ => return Ok(None),
    };
    let Some(finish) = finish else {
        return Ok(None);
    };
    if finish == "tool-calls" || finish == "unknown" || error.is_some() {
        return Ok(None);
    }

    // Surface any content-filter finish (e.g. Anthropic stop_reason:
    // refusal) as an error. These turns may have produced no visible
    // output at all — previously the session went idle silently — or
    // partial text that was cut off by the provider's filter.
    if finish == "content-filter" {
        let mut message = message.clone();
        if let V1Message::Assistant { error, .. } = &mut message {
            *error = Some(AssistantError::ContentFilter {
                message: "The response was blocked by the provider's content filter".to_string(),
            });
        }
        deps.sessions.update_message(&message)?;
        if let V1Message::Assistant {
            error: Some(error), ..
        } = &message
        {
            deps.events.publish(
                &SESSION_ERROR,
                serde_json::json!({
                    "sessionID": session_id,
                    "error": serde_json::to_value(error).unwrap_or(Value::Null),
                }),
                PublishOptions::default(),
            )?;
        }
        return Ok(Some(Outcome::Break));
    }
    if json_schema_format {
        let mut message = message.clone();
        if let V1Message::Assistant { error, .. } = &mut message {
            *error = Some(AssistantError::StructuredOutput {
                message: "Model did not produce structured output".to_string(),
                retries: 0,
            });
        }
        deps.sessions.update_message(&message)?;
        return Ok(Some(Outcome::Break));
    }
    Ok(None)
}

/// `lastAssistant` (prompt.ts:1339-1349).
pub(crate) async fn last_assistant(
    deps: &LoopDeps,
    session_id: &str,
) -> Result<WithParts, LoopError> {
    // `findMessage` — the newest non-user message.
    if let Some(msg) = deps.sessions.find_message(session_id, &|msg: &WithParts| {
        !matches!(msg.info, V1Message::User { .. })
    })? {
        return Ok(msg);
    }
    // Fallback: the first message of the newest page.
    let msgs = deps.sessions.messages(session_id, Some(1))?;
    msgs.into_iter()
        .next()
        .ok_or_else(|| LoopError::Unknown("Impossible".to_string()))
}

/// `finalizeInterruptedAssistant` (prompt.ts:1206-1212): no cleanup ran —
/// stamp the abort error and completed time.
async fn finalize_interrupted_assistant(
    deps: &LoopDeps,
    processor: &ProcessorHandle,
) -> Result<(), SessionError> {
    let mut message = processor.message();
    match &mut message {
        V1Message::Assistant { error, time, .. } => {
            if error.is_none() {
                *error = Some(AssistantError::Aborted {
                    message: "Aborted".to_string(),
                });
            }
            if time.completed.is_none() {
                time.completed = Some(deps.clock.now_ms());
            }
        }
        V1Message::User { .. } => {}
    }
    deps.sessions.update_message(&message)
}

/// `isOrphanedInterruptedTool` (prompt.ts:96-99): cleanup() marks abandoned
/// tool_use blocks this way after retries/aborts. They are not pending
/// work and must not trigger an assistant-prefill request.
fn is_orphaned_interrupted_tool(part: &V1Part) -> bool {
    match part {
        V1Part::Tool {
            state: opencode_schema::session_v1::V1ToolState::Error { metadata, .. },
            ..
        } => metadata
            .as_ref()
            .and_then(|metadata| metadata.get("interrupted"))
            .map(|interrupted| *interrupted == Value::Bool(true))
            .unwrap_or(false),
        _ => false,
    }
}

/// `part.metadata?.providerExecuted` (prompt.ts:1107).
fn part_provider_executed(part: &V1Part) -> bool {
    match part {
        V1Part::Tool {
            metadata: Some(metadata),
            ..
        } => metadata
            .get("providerExecuted")
            .map(|executed| *executed == Value::Bool(true))
            .unwrap_or(false),
        _ => false,
    }
}

fn assistant_finish(message: &V1Message) -> Option<String> {
    match message {
        V1Message::Assistant { finish, .. } => finish.clone(),
        _ => None,
    }
}

/// `{ input: 0, output: 0, reasoning: 0, cache: { read: 0, write: 0 } }`.
fn empty_tokens() -> V1StepTokens {
    V1StepTokens {
        total: None,
        input: 0.0,
        output: 0.0,
        reasoning: 0.0,
        cache: V1TokenCache {
            read: 0.0,
            write: 0.0,
        },
    }
}

/// `NamedError.Unknown.toObject()` published as
/// `{ sessionID, error: { name: "Unknown", data: { message } } }` —
/// the same payload shape as `prompt_input.rs`.
fn publish_unknown_error(
    events: &EventBus,
    session_id: &str,
    message: &str,
) -> Result<(), SessionError> {
    events.publish(
        &SESSION_ERROR,
        serde_json::json!({
            "sessionID": session_id,
            "error": { "name": "Unknown", "data": { "message": message } },
        }),
        PublishOptions::default(),
    )?;
    Ok(())
}

// -------------------------------------------------------------------------
// Structured output tool (prompt.ts:1565-1589)
// -------------------------------------------------------------------------

/// `createStructuredOutputTool` (prompt.ts:1565-1589). The AI SDK
/// validates args against the schema before calling execute(); the Rust
/// runtime validates nothing — the loop checks the captured value's
/// presence only (documented divergence).
pub fn create_structured_output_tool(
    schema: Value,
    captured: Arc<Mutex<Option<Value>>>,
) -> LlmTool {
    // Remove $schema property if present (not needed for tool input).
    let mut schema = schema;
    if let Value::Object(record) = &mut schema {
        record.remove("$schema");
    }
    LlmTool {
        name: "StructuredOutput".to_string(),
        description: STRUCTURED_OUTPUT_DESCRIPTION.to_string(),
        input_schema: schema,
        execute: Arc::new(move |args: Value, _call_id: String| {
            let captured = captured.clone();
            Box::pin(async move {
                let mut slot = captured.lock().unwrap();
                *slot = Some(args.clone());
                Ok(LlmToolOutput {
                    title: "Structured Output".to_string(),
                    metadata: serde_json::json!({ "valid": true }),
                    output: "Structured output captured successfully.".to_string(),
                    attachments: None,
                })
            }) as BoxFuture<'static, Result<LlmToolOutput, String>>
        }),
    }
}

// -------------------------------------------------------------------------
// Title (prompt.ts:193-250)
// -------------------------------------------------------------------------

/// `title` input (prompt.ts:193-197).
#[derive(Debug, Clone)]
pub struct TitleInput {
    pub session: V1SessionInfo,
    pub history: Vec<WithParts>,
    pub provider_id: String,
    pub model_id: String,
}

struct TitleDeps {
    sessions: SessionStore,
    agents: AgentRegistry,
    models: Arc<dyn ModelSource>,
    llm: Arc<dyn LlmStream>,
    project_id: Option<String>,
    client: String,
}

/// `title` (prompt.ts:193-250) — generate and set the session title.
async fn generate_title(deps: &TitleDeps, input: TitleInput) {
    if input.session.parent_id.is_some() {
        return;
    }
    if !is_default_title(&input.session.title) {
        return;
    }

    let real = |msg: &WithParts| {
        matches!(msg.info, V1Message::User { .. })
            && !msg.parts.iter().all(|part| {
                matches!(
                    part,
                    V1Part::Text {
                        synthetic: Some(true),
                        ..
                    }
                )
            })
    };
    let idx = match input.history.iter().position(real) {
        Some(idx) => idx,
        None => return,
    };
    if input.history.iter().filter(|msg| real(msg)).count() != 1 {
        return;
    }
    let context = &input.history[..idx + 1];
    let first_user = &input.history[idx];
    let first_info = first_user.info.clone();

    let subtasks: Vec<&V1Part> = first_user
        .parts
        .iter()
        .filter(|part| matches!(part, V1Part::Subtask { .. }))
        .collect();
    let only_subtasks = !subtasks.is_empty() && first_user.parts.len() == subtasks.len();

    let Some(title_agent) = deps.agents.get("title").cloned() else {
        return;
    };
    let model = match &title_agent.model {
        Some(model) => {
            match deps
                .models
                .get_model(&model.provider_id, &model.model_id, &input.session.id)
                .await
            {
                Ok(model) => model,
                Err(_) => return,
            }
        }
        None => match deps.models.get_small_model(&input.provider_id).await {
            Some(model) => model,
            None => {
                match deps
                    .models
                    .get_model(&input.provider_id, &input.model_id, &input.session.id)
                    .await
                {
                    Ok(model) => model,
                    Err(_) => return,
                }
            }
        },
    };

    let msgs = if only_subtasks {
        let prompts = subtasks
            .iter()
            .map(|part| match part {
                V1Part::Subtask { prompt, .. } => prompt.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        vec![Message::user(prompts)]
    } else {
        to_model_messages(context, &model.render(), None)
    };

    let mut stream = deps.llm.stream(StreamInput {
        user: first_info,
        session_id: input.session.id.clone(),
        parent_session_id: input.session.parent_id.clone(),
        project_id: deps.project_id.clone(),
        client: deps.client.clone(),
        model: model.llm.clone(),
        agent: title_agent,
        permission: None,
        system: Vec::new(),
        messages: {
            let mut messages = vec![Message::user("Generate a title for this conversation:\n")];
            messages.extend(msgs);
            messages
        },
        small: true,
        tools: Vec::new(),
        retries: Some(2),
        tool_choice: None,
    });
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(opencode_llm::schema::events::LlmEvent::TextDelta { text: delta, .. }) => {
                text.push_str(&delta);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let Some(title) = title_from_text(&text) else {
        return;
    };
    let _ = deps.sessions.set_title(&input.session.id, &title);
}

/// `fork title` (prompt.ts:1135-1139): fires once, on step 1.
fn fork_title(
    deps: &LoopDeps,
    session: V1SessionInfo,
    history: Vec<WithParts>,
    last_user: &V1Message,
) {
    let deps = TitleDeps {
        sessions: deps.sessions.clone(),
        agents: deps.agents.clone(),
        models: deps.models.clone(),
        llm: deps.llm.clone(),
        project_id: deps.project_id.clone(),
        client: deps.client.clone(),
    };
    let (provider_id, model_id) = match last_user {
        V1Message::User { model, .. } => (model.provider_id.clone(), model.model_id.clone()),
        _ => (String::new(), String::new()),
    };
    tokio::spawn(async move {
        generate_title(
            &deps,
            TitleInput {
                session,
                history,
                provider_id,
                model_id,
            },
        )
        .await;
    });
}

/// The title text pipeline (prompt.ts:243-249): strip think blocks,
/// first non-empty trimmed line, 100-char cap (`{97}...` suffix).
pub fn title_from_text(text: &str) -> Option<String> {
    let stripped = think_tag().replace_all(text, "");
    let cleaned = stripped
        .split('\n')
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let title = if cleaned.chars().count() > 100 {
        let prefix: String = cleaned.chars().take(97).collect();
        format!("{prefix}...")
    } else {
        cleaned.to_string()
    };
    Some(title)
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::error::SessionError;
    use crate::session::llm::LlmEventStream;
    use crate::session::run_state::{BackgroundJobInfo, BackgroundJobs};
    use crate::session::store::SessionContext;
    use crate::tool::def::Agents;
    use crate::tool::error::ToolError;
    use crate::tool::registry::RuntimeFlags;
    use opencode_llm::schema::errors::{LlmError, LlmErrorReason};
    use opencode_llm::schema::events::{LlmEvent, Usage};
    use opencode_schema::session_v1::{UserTime, V1UserModel};
    use serde_json::json;
    use std::collections::{BTreeMap, VecDeque};

    // ------------------------------------------------------------------
    // Harness
    // ------------------------------------------------------------------

    struct NoJobs;

    impl BackgroundJobs for NoJobs {
        fn list(&self) -> Result<Vec<BackgroundJobInfo>, crate::CoreError> {
            Ok(Vec::new())
        }
        fn cancel(&self, _: &str) -> Result<(), crate::CoreError> {
            Ok(())
        }
    }

    struct FixedClock;

    impl Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            1_761_000_000_000
        }
    }

    /// A scripted [`LlmStream`]: each `stream()` call pops the next
    /// script (a full `Result<LlmEvent, LlmError>` list) and records the
    /// input for assertions. With `hang` set, every stream never ends —
    /// the abort tests' cancellation seam.
    struct MockLlm {
        script: Mutex<VecDeque<Vec<Result<LlmEvent, LlmError>>>>,
        inputs: Mutex<Vec<StreamInput>>,
        hang: std::sync::atomic::AtomicBool,
    }

    impl MockLlm {
        fn new(script: Vec<Vec<Result<LlmEvent, LlmError>>>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script.into_iter().collect()),
                inputs: Mutex::new(Vec::new()),
                hang: std::sync::atomic::AtomicBool::new(false),
            })
        }

        fn lock_hang(&self) {
            self.hang.store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn calls(&self) -> usize {
            self.inputs.lock().unwrap().len()
        }

        fn input(&self, index: usize) -> StreamInput {
            self.inputs.lock().unwrap()[index].clone()
        }
    }

    /// Emulate the runtime's tool dispatch (llm.rs `dispatch_tool_calls`):
    /// queued tool calls execute after the provider events end, and the
    /// results flow back as `ToolResult`/`ToolError` events.
    async fn mock_dispatch_one(
        tools: &[LlmTool],
        id: &str,
        name: &str,
        input: Value,
    ) -> Vec<LlmEvent> {
        use opencode_llm::schema::messages::ToolResultValue;

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
                    result: ToolResultValue::Error {
                        value: Value::String(message),
                    },
                    output: None,
                    provider_executed: None,
                    provider_metadata: None,
                },
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
            value["attachments"] = Value::Array(attachments);
        }
        vec![LlmEvent::ToolResult {
            id: id.to_string(),
            name: name.to_string(),
            result: ToolResultValue::Json { value },
            output: None,
            provider_executed: None,
            provider_metadata: None,
        }]
    }

    /// The dispatch state machine unfolded after the scripted events.
    fn mock_dispatch_stream(
        calls: Vec<(String, String, Value)>,
        tools: Vec<LlmTool>,
        hang: bool,
    ) -> LlmEventStream {
        struct State {
            calls: VecDeque<(String, String, Value)>,
            tools: Vec<LlmTool>,
            queue: VecDeque<LlmEvent>,
            hang: bool,
        }
        let state = State {
            calls: calls.into(),
            tools,
            queue: VecDeque::new(),
            hang,
        };
        futures::stream::unfold(state, |mut state| async move {
            if let Some(event) = state.queue.pop_front() {
                return Some((Ok(event), state));
            }
            let Some((id, name, input)) = state.calls.pop_front() else {
                if state.hang {
                    futures::future::pending::<()>().await;
                }
                return None;
            };
            let events = mock_dispatch_one(&state.tools, &id, &name, input).await;
            let mut events = events;
            let first = events.remove(0);
            state.queue.extend(events);
            Some((Ok(first), state))
        })
        .boxed()
    }

    impl LlmStream for MockLlm {
        fn stream(&self, input: StreamInput) -> LlmEventStream {
            self.inputs.lock().unwrap().push(input.clone());
            let events = self.script.lock().unwrap().pop_front().unwrap_or_default();
            let mut calls = Vec::new();
            for item in &events {
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
            let provider = futures::stream::iter(events);
            let hang = self.hang.load(std::sync::atomic::Ordering::SeqCst);
            provider
                .chain(mock_dispatch_stream(calls, input.tools, hang))
                .boxed()
        }
    }

    fn rate_limit_error() -> LlmError {
        LlmError {
            module: "ProviderShared".to_string(),
            method: "request".to_string(),
            reason: LlmErrorReason::RateLimit {
                message: "rate limited".to_string(),
                retry_after_ms: Some(1.0),
                rate_limit: None,
                provider_metadata: None,
                http: None,
            },
        }
    }

    struct AllowAll;

    impl AskPermission for AllowAll {
        fn ask<'a>(
            &'a self,
            _request: crate::session::processor::PermissionAsk,
        ) -> futures::future::BoxFuture<'a, Result<(), crate::session::processor::PermissionAskError>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    /// Records every compaction service call.
    struct RecordingCompaction {
        creates: Mutex<Vec<CompactionCreate>>,
        processes: Mutex<Vec<CompactionProcess>>,
        prunes: Mutex<Vec<String>>,
        /// The `process` result: `Ok(true)` continue, `Ok(false)` stop.
        process_result: bool,
    }

    impl Default for RecordingCompaction {
        fn default() -> Self {
            Self {
                creates: Mutex::new(Vec::new()),
                processes: Mutex::new(Vec::new()),
                prunes: Mutex::new(Vec::new()),
                process_result: true,
            }
        }
    }

    impl Compaction for RecordingCompaction {
        fn process<'a>(
            &'a self,
            input: CompactionProcess,
        ) -> BoxFuture<'a, Result<bool, SessionError>> {
            let input = input.clone();
            Box::pin(async move {
                self.processes.lock().unwrap().push(input);
                Ok(self.process_result)
            })
        }
        fn create<'a>(
            &'a self,
            input: CompactionCreate,
        ) -> BoxFuture<'a, Result<(), SessionError>> {
            Box::pin(async move {
                self.creates.lock().unwrap().push(input);
                Ok(())
            })
        }
        fn prune<'a>(&'a self, session_id: String) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.prunes.lock().unwrap().push(session_id);
            })
        }
    }

    struct RecordingSubtasks {
        handled: Mutex<Vec<SubtaskInput>>,
    }

    impl Subtasks for RecordingSubtasks {
        fn handle<'a>(&'a self, input: SubtaskInput) -> BoxFuture<'a, Result<(), LoopError>> {
            Box::pin(async move {
                self.handled.lock().unwrap().push(input);
                Ok(())
            })
        }
    }

    /// `getModel` / `getSmallModel` stub returning one fixed model.
    struct StaticModels {
        model: ResolvedModel,
    }

    fn test_model() -> ResolvedModel {
        ResolvedModel {
            llm: LlmModel {
                id: "claude".to_string(),
                provider_id: "anthropic".to_string(),
                api_id: "claude".to_string(),
                api_npm: "@ai-sdk/anthropic".to_string(),
                temperature_capable: false,
                headers: BTreeMap::new(),
                options: BTreeMap::new(),
                context_limit: 1000.0,
                output_limit: 100.0,
                output_token_max: None,
            },
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cache: crate::session::usage::CacheCost {
                    read: 0.0,
                    write: 0.0,
                },
                tiers: Vec::new(),
                experimental_over_200k: None,
            },
            limits: ModelLimits {
                context: 1000.0,
                input: None,
                output: 100.0,
            },
            output_token_max: None,
        }
    }

    impl ModelSource for StaticModels {
        fn get_model<'a>(
            &'a self,
            _provider_id: &'a str,
            _model_id: &'a str,
            _session_id: &'a str,
        ) -> BoxFuture<'a, Result<ResolvedModel, LoopError>> {
            Box::pin(async { Ok(self.model.clone()) })
        }
        fn get_small_model<'a>(
            &'a self,
            _provider_id: &'a str,
        ) -> BoxFuture<'a, Option<ResolvedModel>> {
            Box::pin(async { None })
        }
    }

    struct NoTaskOps;

    impl TaskOps for NoTaskOps {
        fn depth<'a>(
            &'a self,
            _session_id: &'a str,
        ) -> futures::future::BoxFuture<'a, Result<usize, ToolError>> {
            Box::pin(async { Ok(0) })
        }
        fn agent<'a>(
            &'a self,
            _name: &'a str,
        ) -> futures::future::BoxFuture<
            'a,
            Result<Option<crate::tool::task::SubagentInfo>, ToolError>,
        > {
            Box::pin(async { Ok(None) })
        }
        fn session_exists<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, bool> {
            Box::pin(async { false })
        }
        fn create_session<'a>(
            &'a self,
            _parent_id: &'a str,
            _title: &'a str,
            _agent: &'a str,
            _permission: Vec<crate::tool::task::Rule>,
        ) -> futures::future::BoxFuture<'a, Result<String, ToolError>> {
            Box::pin(async { Err(ToolError::Failed("not implemented".to_string())) })
        }
        fn parent_message<'a>(
            &'a self,
            _session_id: &'a str,
            _message_id: &'a str,
        ) -> futures::future::BoxFuture<'a, Result<crate::tool::task::ParentMessage, ToolError>>
        {
            Box::pin(async { Err(ToolError::Failed("not implemented".to_string())) })
        }
        fn prompt<'a>(
            &'a self,
            _session_id: &'a str,
            _agent: &'a str,
            _model: &'a crate::tool::task::ModelRef,
            _variant: Option<&'a str>,
            _prompt: &'a str,
        ) -> futures::future::BoxFuture<'a, Result<crate::tool::task::PromptOutcome, ToolError>>
        {
            Box::pin(async { Err(ToolError::Failed("not implemented".to_string())) })
        }
        fn cancel<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
    }

    // ------------------------------------------------------------------
    // Loop harness
    // ------------------------------------------------------------------

    struct Harness {
        _temp: crate::storage::test_support::TempDir,
        services: crate::session::SessionServices,
        llm: Arc<MockLlm>,
        compaction: Arc<RecordingCompaction>,
        instruction: Arc<Instruction>,
        worktree: std::path::PathBuf,
    }

    /// A `ToolDef` whose execution always returns a canned success — the
    /// registry's `task`/`read` anchors without touching the filesystem.
    fn dummy_def(id: &'static str) -> crate::tool::def::ToolDef {
        crate::tool::def::ToolDef {
            id,
            description: "dummy".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
            }),
            format_validation_error: None,
            execute: Arc::new(|_args, _ctx| {
                Box::pin(async {
                    Ok(crate::tool::def::ExecuteResult {
                        title: "dummy".to_string(),
                        metadata: serde_json::json!({}),
                        output: "ok".to_string(),
                        attachments: None,
                    })
                })
            }),
        }
    }

    struct RegistryAgents;

    impl Agents for RegistryAgents {
        fn get<'a>(
            &'a self,
            _agent: &'a str,
        ) -> futures::future::BoxFuture<'a, Result<crate::tool::def::AgentInfo, ToolError>>
        {
            Box::pin(async { Err(ToolError::Failed("no agents".to_string())) })
        }
        fn list<'a>(&'a self) -> futures::future::BoxFuture<'a, Vec<crate::tool::def::AgentInfo>> {
            Box::pin(async { Vec::new() })
        }
    }

    fn harness(name: &str, script: Vec<Vec<Result<LlmEvent, LlmError>>>) -> Harness {
        harness_with_config(name, script, serde_json::json!({}))
    }

    fn harness_with_config(
        name: &str,
        script: Vec<Vec<Result<LlmEvent, LlmError>>>,
        config: Value,
    ) -> Harness {
        let temp = crate::storage::test_support::TempDir::new(name);
        let worktree = temp.path().join("repo");
        std::fs::create_dir_all(&worktree).unwrap();
        let config: crate::config::schema::Config =
            serde_json::from_value(config).expect("valid config");
        let agent_input = crate::session::agents::AgentRegistryInput {
            config,
            skill_dirs: Vec::new(),
            reference_dirs: Vec::new(),
            worktree: worktree.clone(),
            data_dir: temp.path().to_path_buf(),
            tmp_dir: temp.path().to_path_buf(),
            home: temp.path().to_path_buf(),
        };
        let services = crate::session::SessionServices::new(
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
        let instruction = Arc::new(Instruction::new(
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
        Harness {
            _temp: temp,
            services,
            llm: MockLlm::new(script),
            compaction: Arc::new(RecordingCompaction::default()),
            instruction,
            worktree,
        }
    }

    fn create_session(h: &Harness) -> V1SessionInfo {
        h.services
            .sessions
            .create(
                &SessionContext {
                    project_id: "global".to_string(),
                    directory: h.worktree.clone(),
                    worktree: h.worktree.clone(),
                    workspace_id: None,
                },
                &Default::default(),
            )
            .unwrap()
    }

    fn user_message(session: &str, id: &str, created: f64) -> V1Message {
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

    fn push_user_message(h: &Harness, session: &str, id: &str) {
        let message = user_message(session, id, 1.0);
        h.services.sessions.update_message(&message).unwrap();
    }

    fn loop_deps(h: &Harness) -> LoopDeps {
        loop_deps_with(h, vec![dummy_def("task"), dummy_def("read")])
    }

    /// A `read` tool whose execution never resolves — the abort tests'
    /// cancellation seam.
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

    fn loop_deps_with(h: &Harness, defs: Vec<crate::tool::def::ToolDef>) -> LoopDeps {
        let registry = crate::tool::registry::ToolRegistry::new(
            defs,
            Vec::new(),
            RuntimeFlags::default(),
            Arc::new(RegistryAgents),
        )
        .expect("valid registry");
        LoopDeps {
            sessions: h.services.sessions.clone(),
            messages: h.services.messages.clone(),
            events: h.services.events.clone(),
            status: h.services.status.clone(),
            agents: h.services.agents.clone(),
            models: Arc::new(StaticModels {
                model: test_model(),
            }),
            llm: h.llm.clone(),
            snapshot: Arc::new(crate::session::snapshot::DisabledSnapshot),
            compaction: h.compaction.clone(),
            subtasks: Arc::new(RecordingSubtasks {
                handled: Mutex::new(Vec::new()),
            }),
            summary: Arc::new(crate::session::processor::NoSummary),
            instruction: h.instruction.clone(),
            systems: Arc::new(NoSystemPrompts),
            registry,
            permission: Arc::new(AllowAll),
            prompt_ops: Arc::new(NoTaskOps),
            config: Arc::new(
                serde_json::from_value(serde_json::json!({}))
                    .expect("all config fields are optional"),
            ),
            clock: Arc::new(FixedClock),
            instance: InstanceContext {
                directory: h.worktree.clone(),
                worktree: h.worktree.clone(),
            },
            experimental_plan_mode: false,
            vcs: false,
            data_dir: h.worktree.clone(),
            project_id: None,
            client: "cli".to_string(),
        }
    }

    // ------------------------------------------------------------------
    // Event-script helpers
    // ------------------------------------------------------------------

    fn text_stream(text: &str) -> Vec<Result<LlmEvent, LlmError>> {
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
                reason: opencode_llm::schema::ids::FinishReason::Stop,
                usage: None,
                provider_metadata: None,
            }),
            Ok(LlmEvent::Finish {
                reason: opencode_llm::schema::ids::FinishReason::Stop,
                usage: None,
                provider_metadata: None,
            }),
        ]
    }

    fn tool_call_event(id: &str, name: &str, args: Value) -> LlmEvent {
        LlmEvent::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input: args,
            provider_executed: None,
            provider_metadata: None,
        }
    }

    #[tokio::test]
    async fn run_loop_full_cycle() {
        let h = harness(
            "run-loop-full-cycle",
            vec![
                // Step 1: a tool call, then finish with tool-calls.
                vec![
                    Ok(tool_call_event(
                        "c1",
                        "read",
                        json!({ "filePath": "/repo/a.txt" }),
                    )),
                    Ok(LlmEvent::StepFinish {
                        index: 0.0,
                        reason: opencode_llm::schema::ids::FinishReason::ToolCalls,
                        usage: None,
                        provider_metadata: None,
                    }),
                    Ok(LlmEvent::Finish {
                        reason: opencode_llm::schema::ids::FinishReason::ToolCalls,
                        usage: None,
                        provider_metadata: None,
                    }),
                ],
                // Step 2: plain text, finish stop.
                text_stream("all done"),
            ],
        );
        let session = create_session(&h);
        h.services
            .sessions
            .set_title(&session.id, "Custom")
            .unwrap();
        push_user_message(&h, &session.id, "msg_user1");

        let deps = loop_deps(&h);
        let cancel = CancellationToken::new();
        let result = run_loop(&deps, &session.id, &cancel).await;

        match result {
            Ok(message) => {
                let V1Message::Assistant { finish, .. } = &message.info else {
                    panic!("expected an assistant message");
                };
                assert_eq!(finish.as_deref(), Some("stop"));
            }
            _ => panic!("expected Ok"),
        }
        assert_eq!(h.llm.calls(), 2);

        // The tool part completed with the dummy output.
        let msgs = h.services.messages.stream(&session.id).unwrap();
        let assistant = msgs
            .iter()
            .rev()
            .find(|msg| matches!(msg.info, V1Message::Assistant { .. }))
            .expect("assistant message");
        let tool_parts: Vec<&V1Part> = assistant
            .parts
            .iter()
            .filter(|part| matches!(part, V1Part::Tool { .. }))
            .collect();
        assert_eq!(tool_parts.len(), 1);
        match tool_parts[0] {
            V1Part::Tool { tool, state, .. } => {
                assert_eq!(tool, "read");
                assert!(
                    matches!(
                        state,
                        opencode_schema::session_v1::V1ToolState::Completed { .. }
                    ),
                    "tool part should be completed"
                );
            }
            _ => panic!("expected a tool part"),
        }
    }

    #[tokio::test]
    async fn run_loop_max_steps_prompt_appended_on_last_step() {
        // `steps: 1` — every turn is the last step.
        let config = serde_json::json!({
            "agent": { "build": { "steps": 1 } },
        });
        let h = harness_with_config(
            "run-loop-max-steps",
            vec![text_stream("max steps reached")],
            config,
        );
        let session = create_session(&h);
        h.services
            .sessions
            .set_title(&session.id, "Custom")
            .unwrap();
        push_user_message(&h, &session.id, "msg_user1");

        let deps = loop_deps(&h);
        let cancel = CancellationToken::new();
        run_loop(&deps, &session.id, &cancel).await.unwrap();

        let input = h.llm.input(0);
        let last = input.messages.last().expect("MAX_STEPS_PROMPT appended");
        let text = last
            .content
            .iter()
            .filter_map(|part| match part {
                opencode_llm::schema::messages::ContentPart::Text { text, .. } => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(
            text == MAX_STEPS_PROMPT,
            "MAX_STEPS_PROMPT missing from the model messages"
        );
    }

    #[tokio::test]
    async fn run_loop_structured_output_happy_path() {
        let h = harness(
            "structured-happy",
            vec![vec![
                Ok(tool_call_event(
                    "c1",
                    "StructuredOutput",
                    json!({ "answer": 42 }),
                )),
                Ok(LlmEvent::Finish {
                    reason: opencode_llm::schema::ids::FinishReason::ToolCalls,
                    usage: None,
                    provider_metadata: None,
                }),
            ]],
        );
        let session = create_session(&h);
        h.services
            .sessions
            .set_title(&session.id, "Custom")
            .unwrap();
        push_user_message(&h, &session.id, "msg_user1");
        // json_schema output format on the user message.
        let mut user = user_message(&session.id, "msg_user1", 1.0);
        if let V1Message::User { format, .. } = &mut user {
            *format = Some(OutputFormat::JsonSchema {
                schema: json!({
                    "type": "object",
                    "properties": { "answer": { "type": "number" } },
                })
                .as_object()
                .unwrap()
                .clone(),
                retry_count: None,
            });
        }
        h.services.sessions.update_message(&user).unwrap();

        let deps = loop_deps(&h);
        let cancel = CancellationToken::new();
        let result = run_loop(&deps, &session.id, &cancel).await.unwrap();

        let V1Message::Assistant {
            structured, finish, ..
        } = &result.info
        else {
            panic!("expected an assistant message");
        };
        assert_eq!(structured.as_ref(), Some(&json!({ "answer": 42 })));
        assert_eq!(finish.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn run_loop_structured_output_error_path() {
        let h = harness(
            "structured-error",
            vec![text_stream("no tool call, just text")],
        );
        let session = create_session(&h);
        h.services
            .sessions
            .set_title(&session.id, "Custom")
            .unwrap();
        push_user_message(&h, &session.id, "msg_user1");
        let mut user = user_message(&session.id, "msg_user1", 1.0);
        if let V1Message::User { format, .. } = &mut user {
            *format = Some(OutputFormat::JsonSchema {
                schema: json!({
                    "type": "object",
                    "properties": { "answer": { "type": "number" } },
                })
                .as_object()
                .unwrap()
                .clone(),
                retry_count: None,
            });
        }
        h.services.sessions.update_message(&user).unwrap();

        let deps = loop_deps(&h);
        let cancel = CancellationToken::new();
        let result = run_loop(&deps, &session.id, &cancel).await.unwrap();

        let V1Message::Assistant { error, .. } = &result.info else {
            panic!("expected an assistant message");
        };
        assert!(
            matches!(
                error,
                Some(AssistantError::StructuredOutput { message, .. })
                    if message == "Model did not produce structured output"
            ),
            "expected a StructuredOutput error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn run_loop_content_filter_error() {
        let h = harness(
            "content-filter",
            vec![vec![
                Ok(LlmEvent::StepFinish {
                    index: 0.0,
                    reason: opencode_llm::schema::ids::FinishReason::ContentFilter,
                    usage: None,
                    provider_metadata: None,
                }),
                Ok(LlmEvent::Finish {
                    reason: opencode_llm::schema::ids::FinishReason::ContentFilter,
                    usage: None,
                    provider_metadata: None,
                }),
            ]],
        );
        let session = create_session(&h);
        h.services
            .sessions
            .set_title(&session.id, "Custom")
            .unwrap();
        push_user_message(&h, &session.id, "msg_user1");

        let deps = loop_deps(&h);
        let cancel = CancellationToken::new();
        let result = run_loop(&deps, &session.id, &cancel).await.unwrap();

        let V1Message::Assistant { error, .. } = &result.info else {
            panic!("expected an assistant message");
        };
        assert!(
            matches!(
                error,
                Some(AssistantError::ContentFilter { message })
                    if message == "The response was blocked by the provider's content filter"
            ),
            "expected a ContentFilter error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn run_loop_retry_after_rate_limit() {
        let h = harness(
            "retry-after-rate-limit",
            vec![
                // Attempt 1 fails with a retryable rate limit.
                vec![Err(rate_limit_error())],
                // Attempt 2 succeeds (with usage so tokens are visible).
                {
                    let mut events = text_stream("recovered");
                    if let Some(Ok(LlmEvent::StepFinish { usage, .. })) = events
                        .iter_mut()
                        .find(|item| matches!(item, Ok(LlmEvent::StepFinish { .. })))
                    {
                        *usage = Some(Usage {
                            input_tokens: Some(5.0),
                            output_tokens: Some(2.0),
                            ..Usage::default()
                        });
                    }
                    events
                },
            ],
        );
        let session = create_session(&h);
        h.services
            .sessions
            .set_title(&session.id, "Custom")
            .unwrap();
        push_user_message(&h, &session.id, "msg_user1");

        // Collect `session.status` events to observe the retry status.
        let mut statuses = h.services.events.subscribe("session.status");

        let deps = loop_deps(&h);
        let cancel = CancellationToken::new();
        let result = run_loop(&deps, &session.id, &cancel).await.unwrap();

        // Two attempts, one stream each.
        assert_eq!(h.llm.calls(), 2);
        let V1Message::Assistant { tokens, .. } = &result.info else {
            panic!("expected an assistant message");
        };
        assert!(tokens.input > 0.0 || tokens.output > 0.0);

        let mut retry_seen = false;
        while let Ok(payload) = statuses.try_recv() {
            if payload
                .data
                .get("status")
                .and_then(|s| s.get("type"))
                .and_then(|t| t.as_str())
                == Some("retry")
            {
                retry_seen = true;
            }
        }
        assert!(retry_seen, "expected a retry status event");
    }

    #[tokio::test]
    async fn run_loop_overflow_creates_compaction() {
        let usage = Usage {
            input_tokens: Some(5000.0),
            output_tokens: Some(10.0),
            ..Usage::default()
        };
        let h = harness(
            "overflow-compaction",
            vec![vec![
                Ok(LlmEvent::StepStart { index: 0.0 }),
                Ok(LlmEvent::StepFinish {
                    index: 0.0,
                    reason: opencode_llm::schema::ids::FinishReason::Stop,
                    usage: Some(usage.clone()),
                    provider_metadata: None,
                }),
                Ok(LlmEvent::Finish {
                    reason: opencode_llm::schema::ids::FinishReason::Stop,
                    usage: Some(usage),
                    provider_metadata: None,
                }),
            ]],
        );
        let session = create_session(&h);
        h.services
            .sessions
            .set_title(&session.id, "Custom")
            .unwrap();
        push_user_message(&h, &session.id, "msg_user1");

        let deps = loop_deps(&h);
        let cancel = CancellationToken::new();
        run_loop(&deps, &session.id, &cancel).await.unwrap();

        let creates = h.compaction.creates.lock().unwrap();
        assert_eq!(creates.len(), 1, "expected one compaction create");
        assert!(creates[0].auto);
        assert!(!creates[0].overflow, "finish is set — not overflow");
    }

    #[tokio::test]
    async fn run_loop_abort_marks_interrupted_and_completes() {
        // The "read" tool hangs forever; the abort cancels mid-flight.
        let h = harness(
            "abort-interrupted",
            vec![vec![Ok(tool_call_event(
                "c1",
                "read",
                json!({ "filePath": "/repo/a.txt" }),
            ))]],
        );
        // Script the mock to hang after the tool call so the loop is
        // mid-flight when the cancellation fires.
        h.llm.lock_hang();
        let session = create_session(&h);
        h.services
            .sessions
            .set_title(&session.id, "Custom")
            .unwrap();
        push_user_message(&h, &session.id, "msg_user1");

        let deps = loop_deps_with(&h, vec![dummy_def("task"), hanging_def("read")]);
        let cancel = CancellationToken::new();
        let cancel_loop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel_loop.cancel();
        });
        let result = run_loop(&deps, &session.id, &cancel).await;
        assert!(matches!(result, Err(LoopError::Cancelled)));

        let msgs = h.services.messages.stream(&session.id).unwrap();
        let assistant = msgs
            .iter()
            .rev()
            .find(|msg| matches!(msg.info, V1Message::Assistant { .. }))
            .expect("assistant message");
        let V1Message::Assistant { error, time, .. } = &assistant.info else {
            panic!("expected an assistant message");
        };
        assert!(matches!(error, Some(AssistantError::Aborted { .. })));
        assert!(time.completed.is_some(), "abort must complete the message");

        // The in-flight tool part is marked interrupted.
        let interrupted = assistant.parts.iter().any(|part| match part {
            V1Part::Tool {
                state: opencode_schema::session_v1::V1ToolState::Error { metadata, .. },
                ..
            } => metadata
                .as_ref()
                .and_then(|m| m.get("interrupted"))
                .map(|v| *v == Value::Bool(true))
                .unwrap_or(false),
            _ => false,
        });
        assert!(interrupted, "tool part must be marked interrupted");
    }

    // ------------------------------------------------------------------
    // Processor-level tests
    // ------------------------------------------------------------------

    fn assistant_message(session: &str, id: &str, parent: &str) -> V1Message {
        V1Message::Assistant {
            id: id.to_string(),
            session_id: session.to_string(),
            time: opencode_schema::session_v1::AssistantTime {
                created: 1,
                completed: None,
            },
            error: None,
            parent_id: parent.to_string(),
            model_id: "claude".to_string(),
            provider_id: "anthropic".to_string(),
            mode: "primary".to_string(),
            agent: "build".to_string(),
            path: opencode_schema::session_v1::V1Path {
                cwd: "/repo".to_string(),
                root: "/repo".to_string(),
            },
            summary: None,
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
            finish: None,
        }
    }

    fn echo_tool() -> LlmTool {
        LlmTool {
            name: "echo".to_string(),
            description: "echo".to_string(),
            input_schema: serde_json::json!({ "type": "object" }),
            execute: Arc::new(|_args: Value, _call_id: String| {
                Box::pin(async {
                    Ok(LlmToolOutput {
                        title: "Echo".to_string(),
                        metadata: serde_json::json!({}),
                        output: "echoed".to_string(),
                        attachments: None,
                    })
                }) as BoxFuture<'static, Result<LlmToolOutput, String>>
            }),
        }
    }

    async fn run_processor_with(
        h: &Harness,
        tools: Vec<LlmTool>,
        permission: Arc<dyn AskPermission>,
    ) -> (Result<ProcessResult, String>, V1Message) {
        let session = create_session(h);
        h.services
            .sessions
            .update_message(&user_message(&session.id, "msg_p1", 1.0))
            .unwrap();
        let assistant = assistant_message(&session.id, "msg_a1", "msg_p1");
        h.services.sessions.update_message(&assistant).unwrap();

        let model = test_model();
        let deps = ProcessorDeps {
            sessions: h.services.sessions.clone(),
            messages: h.services.messages.clone(),
            status: h.services.status.clone(),
            events: h.services.events.clone(),
            snapshot: Arc::new(crate::session::snapshot::DisabledSnapshot),
            agents: h.services.agents.clone(),
            config: Arc::new(serde_json::from_value(serde_json::json!({})).unwrap()),
            llm: h.llm.clone(),
            permission,
            summary: Arc::new(crate::session::processor::NoSummary),
            clock: Arc::new(FixedClock),
        };
        let handle = Processor::create(
            deps,
            ProcessorInput {
                assistant_message: assistant.clone(),
                session_id: session.id.clone(),
                model: model.processor_model(),
            },
        )
        .await;
        let agent = h.services.agents.get("build").cloned().unwrap();
        let result = handle
            .process(
                StreamInput {
                    user: user_message(&session.id, "msg_p1", 1.0),
                    session_id: session.id.clone(),
                    parent_session_id: None,
                    project_id: None,
                    client: "cli".to_string(),
                    model: model.llm.clone(),
                    agent,
                    permission: session.permission.clone(),
                    system: Vec::new(),
                    messages: vec![Message::user("hi")],
                    small: false,
                    tools,
                    retries: None,
                    tool_choice: None,
                },
                CancellationToken::new(),
            )
            .await;
        let message = handle.message();
        (
            match result {
                Ok(result) => Ok(result),
                Err(error) => Err(error.to_string()),
            },
            message,
        )
    }

    async fn run_processor(
        h: &Harness,
        tools: Vec<LlmTool>,
    ) -> (Result<ProcessResult, String>, V1Message) {
        run_processor_with(h, tools, Arc::new(AllowAll)).await
    }

    #[tokio::test]
    async fn processor_event_table() {
        let h = harness(
            "processor-event-table",
            vec![vec![
                Ok(LlmEvent::StepStart { index: 0.0 }),
                Ok(LlmEvent::TextStart {
                    id: "t1".to_string(),
                    provider_metadata: None,
                }),
                Ok(LlmEvent::TextDelta {
                    id: "t1".to_string(),
                    text: "hello ".to_string(),
                    provider_metadata: None,
                }),
                Ok(LlmEvent::TextDelta {
                    id: "t1".to_string(),
                    text: "world".to_string(),
                    provider_metadata: None,
                }),
                Ok(LlmEvent::TextEnd {
                    id: "t1".to_string(),
                    provider_metadata: None,
                }),
                Ok(LlmEvent::ReasoningStart {
                    id: "r1".to_string(),
                    provider_metadata: None,
                }),
                Ok(LlmEvent::ReasoningDelta {
                    id: "r1".to_string(),
                    text: "thinking".to_string(),
                    provider_metadata: None,
                }),
                Ok(LlmEvent::ReasoningEnd {
                    id: "r1".to_string(),
                    provider_metadata: None,
                }),
                Ok(tool_call_event("c1", "echo", json!({ "msg": "hi" }))),
                Ok(LlmEvent::StepFinish {
                    index: 0.0,
                    reason: opencode_llm::schema::ids::FinishReason::ToolCalls,
                    usage: None,
                    provider_metadata: None,
                }),
                Ok(LlmEvent::Finish {
                    reason: opencode_llm::schema::ids::FinishReason::ToolCalls,
                    usage: None,
                    provider_metadata: None,
                }),
            ]],
        );
        let (result, _message) = run_processor(&h, vec![echo_tool()]).await;
        assert!(
            matches!(result, Ok(ProcessResult::Continue)),
            "expected Continue"
        );

        let session_id = h.llm.input(0).session_id.clone();
        let msgs = h.services.messages.stream(&session_id).unwrap();
        let assistant = msgs
            .iter()
            .rev()
            .find(|msg| matches!(msg.info, V1Message::Assistant { .. }))
            .expect("assistant message");
        let has_text = assistant
            .parts
            .iter()
            .any(|part| matches!(part, V1Part::Text { text, .. } if text == "hello world"));
        let has_reasoning = assistant
            .parts
            .iter()
            .any(|part| matches!(part, V1Part::Reasoning { text, .. } if text == "thinking"));
        let has_tool = assistant.parts.iter().any(|part| match part {
            V1Part::Tool { tool, state, .. } => {
                tool == "echo"
                    && matches!(
                        state,
                        opencode_schema::session_v1::V1ToolState::Completed { .. }
                    )
            }
            _ => false,
        });
        assert!(has_text, "text part missing");
        assert!(has_reasoning, "reasoning part missing");
        assert!(has_tool, "completed tool part missing");
    }

    #[tokio::test]
    async fn processor_step_finish_usage() {
        let usage = Usage {
            input_tokens: Some(100.0),
            output_tokens: Some(10.0),
            ..Usage::default()
        };
        let h = harness(
            "processor-step-finish-usage",
            vec![vec![
                Ok(LlmEvent::StepFinish {
                    index: 0.0,
                    reason: opencode_llm::schema::ids::FinishReason::Stop,
                    usage: Some(usage),
                    provider_metadata: None,
                }),
                Ok(LlmEvent::Finish {
                    reason: opencode_llm::schema::ids::FinishReason::Stop,
                    usage: None,
                    provider_metadata: None,
                }),
            ]],
        );
        let (result, message) = run_processor(&h, Vec::new()).await;
        // Stop/continue is the loop's decision (finish check) — the
        // processor defaults to `Continue` for a clean finish.
        assert!(matches!(result, Ok(ProcessResult::Continue)));
        let V1Message::Assistant { tokens, finish, .. } = &message else {
            panic!("expected an assistant message");
        };
        assert_eq!(finish.as_deref(), Some("stop"));
        assert_eq!(tokens.input, 100.0);
        assert_eq!(tokens.output, 10.0);
    }

    /// [`AskPermission`] fake that counts `doom_loop` asks.
    struct CountingAsk {
        asks: Mutex<Vec<String>>,
    }

    impl AskPermission for CountingAsk {
        fn ask<'a>(
            &'a self,
            request: crate::session::processor::PermissionAsk,
        ) -> futures::future::BoxFuture<'a, Result<(), crate::session::processor::PermissionAskError>>
        {
            Box::pin(async move {
                self.asks.lock().unwrap().push(request.permission.clone());
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn processor_doom_loop_asks_on_three_identical_triggers() {
        let call = |id: &str| tool_call_event(id, "echo", json!({ "msg": "hi" }));
        let h = harness(
            "doom-loop-identical",
            vec![vec![Ok(call("c1")), Ok(call("c2")), Ok(call("c3"))]],
        );
        let ask = Arc::new(CountingAsk {
            asks: Mutex::new(Vec::new()),
        });
        let (_result, _message) = run_processor_with(&h, vec![echo_tool()], ask.clone()).await;
        assert_eq!(ask.asks.lock().unwrap().len(), 1, "one doom_loop ask");
    }

    #[tokio::test]
    async fn processor_doom_loop_two_identical_do_not_ask() {
        let call = |id: &str| tool_call_event(id, "echo", json!({ "msg": "hi" }));
        let h = harness("doom-loop-two", vec![vec![Ok(call("c1")), Ok(call("c2"))]]);
        let ask = Arc::new(CountingAsk {
            asks: Mutex::new(Vec::new()),
        });
        let (_result, _message) = run_processor_with(&h, vec![echo_tool()], ask.clone()).await;
        assert!(
            ask.asks.lock().unwrap().is_empty(),
            "two identical triggers must not ask"
        );
    }

    #[tokio::test]
    async fn processor_doom_loop_differing_inputs_do_not_ask() {
        let call = |id: &str, msg: &str| tool_call_event(id, "echo", json!({ "msg": msg }));
        let h = harness(
            "doom-loop-differing",
            vec![vec![
                Ok(call("c1", "a")),
                Ok(call("c2", "b")),
                Ok(call("c3", "c")),
            ]],
        );
        let ask = Arc::new(CountingAsk {
            asks: Mutex::new(Vec::new()),
        });
        let (_result, _message) = run_processor_with(&h, vec![echo_tool()], ask.clone()).await;
        assert!(
            ask.asks.lock().unwrap().is_empty(),
            "differing inputs must not ask"
        );
    }

    // ------------------------------------------------------------------
    // Title tests
    // ------------------------------------------------------------------

    #[test]
    fn title_from_text_caps_at_100_chars() {
        let long = "x".repeat(150);
        let title = title_from_text(&long).unwrap();
        let chars: Vec<char> = title.chars().collect();
        assert_eq!(chars.len(), 100);
        assert!(title.ends_with("..."));
        assert_eq!(title.chars().take(97).collect::<String>(), "x".repeat(97));
    }

    #[test]
    fn title_from_text_strips_think_blocks() {
        let text = format!("{}after{}", "<think>junk</think>", "");
        let title = title_from_text(&text).unwrap();
        assert_eq!(title, "after");
    }

    #[test]
    fn title_from_text_first_non_empty_line() {
        let title = title_from_text("  \n\n  hello there  \nsecond").unwrap();
        assert_eq!(title, "hello there");
    }

    #[test]
    fn title_from_text_none_for_empty() {
        assert!(title_from_text("   \n  ").is_none());
    }

    #[tokio::test]
    async fn generate_title_sets_session_title() {
        let h = harness(
            "generate-title",
            vec![vec![
                Ok(LlmEvent::TextStart {
                    id: "t1".to_string(),
                    provider_metadata: None,
                }),
                Ok(LlmEvent::TextDelta {
                    id: "t1".to_string(),
                    text: "  My Great Title  ".to_string(),
                    provider_metadata: None,
                }),
                Ok(LlmEvent::TextEnd {
                    id: "t1".to_string(),
                    provider_metadata: None,
                }),
            ]],
        );
        let session = create_session(&h);
        // A real user message: at least one non-synthetic part.
        let user = user_message(&session.id, "msg_user1", 1.0);
        h.services.sessions.update_message(&user).unwrap();
        h.services
            .sessions
            .update_part(&V1Part::Text {
                id: crate::session::ids::PartId::ascending(None).unwrap(),
                session_id: session.id.clone(),
                message_id: "msg_user1".to_string(),
                text: "hello".to_string(),
                synthetic: None,
                ignored: None,
                time: None,
                metadata: None,
            })
            .unwrap();

        let deps = TitleDeps {
            sessions: h.services.sessions.clone(),
            agents: h.services.agents.clone(),
            models: Arc::new(StaticModels {
                model: test_model(),
            }),
            llm: h.llm.clone(),
            project_id: None,
            client: "cli".to_string(),
        };
        generate_title(
            &deps,
            TitleInput {
                session: session.clone(),
                history: h.services.messages.stream(&session.id).unwrap(),
                provider_id: "anthropic".to_string(),
                model_id: "claude".to_string(),
            },
        )
        .await;

        let updated = h.services.sessions.get(&session.id).unwrap();
        assert_eq!(updated.title, "My Great Title");
    }

    #[tokio::test]
    async fn generate_title_skips_non_default_title() {
        let h = harness("generate-title-skip", vec![]);
        let mut session = create_session(&h);
        session.title = "Custom".to_string();
        h.services
            .sessions
            .set_title(&session.id, "Custom")
            .unwrap();
        let user = user_message(&session.id, "msg_user1", 1.0);
        h.services.sessions.update_message(&user).unwrap();
        h.services
            .sessions
            .update_part(&V1Part::Text {
                id: crate::session::ids::PartId::ascending(None).unwrap(),
                session_id: session.id.clone(),
                message_id: "msg_user1".to_string(),
                text: "hello".to_string(),
                synthetic: None,
                ignored: None,
                time: None,
                metadata: None,
            })
            .unwrap();

        let deps = TitleDeps {
            sessions: h.services.sessions.clone(),
            agents: h.services.agents.clone(),
            models: Arc::new(StaticModels {
                model: test_model(),
            }),
            llm: h.llm.clone(),
            project_id: None,
            client: "cli".to_string(),
        };
        generate_title(
            &deps,
            TitleInput {
                session: session.clone(),
                history: h.services.messages.stream(&session.id).unwrap(),
                provider_id: "anthropic".to_string(),
                model_id: "claude".to_string(),
            },
        )
        .await;
        assert_eq!(h.llm.calls(), 0, "no LLM call for a non-default title");
    }
}
