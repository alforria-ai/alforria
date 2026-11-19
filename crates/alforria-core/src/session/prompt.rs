//! `SessionPrompt` facade — port of `prompt()` (prompt.ts:1052-1071),
//! `shell`/`shellImpl` (prompt.ts:451-592), `command`
//! (prompt.ts:1356-1481), `cancel` (prompt.ts:152-155) and the `ops()`
//! wiring (prompt.ts:144-150). This is the production engine assembly:
//! the loop runs through the run state (prompt re-entrancy is the whole
//! point — `prompt()` is what the task tool's [`TaskOps`](task_ops.rs)
//! calls back into).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alforria_schema::legacy_event::CommandExecutedData;
use alforria_schema::permission_v1::{PermissionV1Action, PermissionV1Rule};
use alforria_schema::session_v1::{
    AssistantTime, UserTime, V1Message, V1Part, V1StepTokens, V1SubtaskModel, V1TokenCache,
    V1ToolState, V1UserModel,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::event::bus::PublishOptions;
use crate::session::error::SessionError;
use crate::session::event_definitions::SESSION_ERROR;
use crate::session::ids::{MessageId, PartId};
use crate::session::instruction::Instruction;
use crate::session::llm::LlmStream;
use crate::session::message::WithParts;
use crate::session::prompt_input::{
    self, create_user_message, resolve_prompt_parts, McpResources, ModelRef, Models, PromptDeps,
    PromptError, PromptInput, PromptPartInput,
};
use crate::session::r#loop::{
    last_assistant, run_loop, Compaction, LoopDeps, LoopError, ModelSource, Subtasks, SystemPrompts,
};
use crate::session::revert::SessionRevert;
use crate::session::run_state::{RunnerError, Work};
use crate::session::snapshot::Snapshot;
use crate::tool::def::InstanceContext;
use crate::tool::registry::ToolRegistry;
use crate::tool::task::TaskOps;
use crate::Clock;

/// `command.executed` (`Command.Event.Executed` — the legacy-event
/// catalog entry).
pub const COMMAND_EXECUTED: crate::event::definition::Definition =
    crate::event::definition::Definition::ephemeral("command.executed");

/// Everything the facade closes over (the Effect layer services).
pub struct SessionPromptDeps {
    pub services: crate::session::SessionServices,
    /// `provider.getModel` for the loop.
    pub models: Arc<dyn ModelSource>,
    /// The model catalog slice `createUserMessage` resolves against.
    pub input_models: Arc<dyn Models>,
    pub llm: Arc<dyn LlmStream>,
    pub snapshot: Arc<dyn Snapshot>,
    pub compaction: Arc<dyn Compaction>,
    pub subtasks: Arc<dyn Subtasks>,
    pub summary: Arc<dyn crate::session::processor::SummarySummarize>,
    pub instruction: Arc<Instruction>,
    pub systems: Arc<dyn SystemPrompts>,
    pub registry: ToolRegistry,
    /// The engine's MCP tool seam (`mcp.tools()`, tools.ts:389).
    pub mcp_tools: Option<Arc<dyn crate::session::tools::McpToolSource>>,
    /// `truncate.output` for MCP tool output (tools.ts:463).
    pub truncate: Arc<dyn crate::tool::truncate::Truncate>,
    pub revert: Arc<SessionRevert>,
    /// The production `TaskOps` bound into the registry's `task` tool.
    pub prompt_ops: Arc<dyn TaskOps>,
    pub config: Arc<crate::config::schema::Config>,
    pub clock: Arc<dyn Clock>,
    pub instance: InstanceContext,
    pub mcp: Arc<dyn McpResources>,
    pub lsp: Arc<dyn prompt_input::LspServer>,
    pub images: Arc<dyn prompt_input::ImageNormalizer>,
    pub data_dir: PathBuf,
    pub project_id: Option<String>,
    pub client: String,
    pub experimental_plan_mode: bool,
    pub vcs: bool,
}

/// The `SessionPrompt.Service` facade (prompt.ts:105-135).
#[derive(Clone)]
pub struct SessionPrompt {
    pub(crate) deps: Arc<SessionPromptDeps>,
}

impl From<RunnerError<SessionError>> for PromptError {
    fn from(error: RunnerError<SessionError>) -> Self {
        match error {
            RunnerError::Work(error) => PromptError::Session(error),
            RunnerError::Cancelled(_) => PromptError::Unknown {
                message: "Cancelled".to_string(),
            },
        }
    }
}

impl SessionPrompt {
    pub fn new(deps: SessionPromptDeps) -> Arc<SessionPrompt> {
        // A session between goal rounds is idle, so a stop has nothing to
        // interrupt; the goal has to hear of it directly.
        let goals =
            crate::session::goal::GoalStore::new(&deps.data_dir, deps.services.events.clone());
        let clock = deps.clock.clone();
        deps.services.run_state.on_cancel(move |session_id| {
            goals.pause_active(session_id, "Stopped by the user", clock.now_ms());
        });
        Arc::new(SessionPrompt {
            deps: Arc::new(deps),
        })
    }

    /// `resolvePromptParts` (prompt.ts:157-191): the template text plus a
    /// file part per `@`-mention that exists on disk, or an agent part.
    pub fn resolve_prompt_parts(&self, template: &str) -> Vec<PromptPartInput> {
        resolve_prompt_parts(
            &self.deps.services.agents,
            &self.deps.instance.worktree,
            template,
        )
    }

    /// `cancel` (prompt.ts:152-155).
    pub async fn cancel(&self, session_id: &str) -> Result<(), SessionError> {
        self.deps
            .services
            .run_state
            .cancel(session_id)
            .await
            .map_err(SessionError::from)
    }

    /// `prompt` (prompt.ts:1052-1071).
    pub async fn prompt(&self, mut input: PromptInput) -> Result<WithParts, PromptError> {
        let session = self.deps.services.sessions.get(&input.session_id)?;
        // A subagent stays the agent it was started as unless told otherwise;
        // the default agent is for sessions a person started.
        if input.agent.is_none() && session.parent_id.is_some() {
            input.agent = session.agent.clone();
        }
        self.deps.revert.cleanup(&session)?;
        let message = create_user_message(&self.prompt_deps(), &input).await?;
        self.deps.services.sessions.touch(&input.session_id)?;

        let mut permissions: Vec<PermissionV1Rule> = Vec::new();
        if let Some(tools) = &input.tools {
            for (tool, enabled) in tools {
                permissions.push(PermissionV1Rule {
                    permission: tool.clone(),
                    pattern: "*".to_string(),
                    action: if *enabled {
                        PermissionV1Action::Allow
                    } else {
                        PermissionV1Action::Deny
                    },
                });
            }
        }
        if !permissions.is_empty() {
            self.deps
                .services
                .sessions
                .set_permission(&session.id, permissions)?;
        }

        if input.no_reply == Some(true) {
            return Ok(WithParts {
                info: message.info,
                parts: message.parts,
            });
        }
        let cancels = self.deps.services.run_state.cancel_count(&input.session_id);
        let result = self.loop_(&input.session_id).await;
        if session.parent_id.is_none() {
            self.goal_after_turn(&input.session_id, &result, cancels);
        }
        Ok(result?)
    }

    /// Tell a freshly forked session what it is (upstream `fork-notice.ts`).
    ///
    /// A fork inherits a transcript full of promises that are not its own:
    /// subagents it cannot continue, shells whose output lands elsewhere,
    /// uncommitted work the original is still changing. Without the notice
    /// the model waits for results that never arrive or redoes the work.
    /// The note is for the model only and starts no turn.
    pub async fn fork_notice(&self, session_id: &str) -> Result<(), PromptError> {
        let has_history = self.deps.services.messages.newest(session_id)?.is_some();
        if !has_history {
            return Ok(());
        }
        let agent = self.deps.services.sessions.get(session_id)?.agent.clone();
        self.prompt(PromptInput {
            caller: crate::session::caller::Caller::Session,
            session_id: session_id.to_string(),
            message_id: None,
            model: None,
            agent,
            no_reply: Some(true),
            tools: None,
            format: None,
            system: None,
            variant: None,
            parts: vec![PromptPartInput::Text {
                id: None,
                text: include_str!("prompt/fork-notice.txt")
                    .trim_end()
                    .to_string(),
                synthetic: Some(true),
                ignored: None,
                time: None,
                metadata: None,
            }],
        })
        .await
        .map(|_| ())
    }

    /// `loop` (prompt.ts:1350-1354): runLoop through the run state.
    pub async fn loop_(&self, session_id: &str) -> Result<WithParts, RunnerError<SessionError>> {
        let deps = self.deps.clone();
        let on_interrupt = last_assistant_work(deps.clone(), session_id);
        let session_id = session_id.to_string();
        let ensure_session_id = session_id.clone();
        // The token is shared with the runner so `cancel` interrupts the
        // loop cooperatively — the loop's interrupt handlers (interrupted
        // tool parts, abort error) run before the prompt resolves (TS:
        // fiber interrupt, prompt.ts:1346).
        let cancel = CancellationToken::new();
        let work: Work<WithParts, SessionError> = {
            let cancel = cancel.clone();
            Arc::new(move || {
                let deps = deps.clone();
                let session_id = session_id.clone();
                let cancel = cancel.clone();
                Box::pin(async move {
                    run_loop(&loop_deps(&deps), &session_id, &cancel)
                        .await
                        .map_err(loop_error)
                })
            })
        };
        self.deps
            .services
            .run_state
            .ensure_running(&ensure_session_id, cancel, on_interrupt, work)
            .await
    }

    /// `shell` (prompt.ts:452-459): `shellImpl` through `state.startShell`.
    pub async fn shell(&self, input: ShellInput) -> Result<WithParts, SessionError> {
        let session_id = input.session_id.clone();
        let deps = self.deps.clone();
        let on_interrupt = last_assistant_work(deps.clone(), &session_id);
        let work: Work<WithParts, SessionError> = Arc::new(move || {
            let deps = deps.clone();
            let input = input.clone();
            Box::pin(async move {
                let cancel = CancellationToken::new();
                shell_impl(&deps, input, cancel).await
            })
        });
        self.deps
            .services
            .run_state
            .start_shell(&session_id, on_interrupt, work, None)
            .await
    }

    /// `command` (prompt.ts:1361-1481).
    pub async fn command(&self, input: CommandInput) -> Result<WithParts, PromptError> {
        let deps = &self.deps;
        // Before anything else — `/goal` arms an autonomous loop before it
        // ever builds a prompt: a caller below `admin` runs commands in
        // restricted sessions only (`Caller::may_act_on`).
        input
            .caller
            .may_act_on(deps.services.sessions.is_restricted(&input.session_id)?)
            .map_err(|message| PromptError::Forbidden { message })?;
        if input.command == "goal" {
            return match self.goal_command(&input).await {
                Ok(Some(turn)) => turn,
                // Nothing was said to the model: hand back where the
                // conversation stands.
                Ok(None) => deps
                    .services
                    .messages
                    .newest(&input.session_id)?
                    .ok_or_else(|| PromptError::Unknown {
                        message: "The session has no messages.".to_string(),
                    }),
                Err(message) => {
                    publish_error(deps, &input.session_id, &message)?;
                    Err(PromptError::Unknown { message })
                }
            };
        }
        let Some(cmd) = deps
            .config
            .command
            .as_ref()
            .and_then(|commands| commands.get(&input.command))
            .cloned()
            .or_else(|| super::commands::builtin(&input.command, &deps.instance.directory))
        else {
            let mut available: Vec<String> = deps
                .config
                .command
                .as_ref()
                .map(|commands| commands.keys().cloned().collect())
                .unwrap_or_default();
            available.extend(super::commands::NAMES.iter().map(|name| name.to_string()));
            available.push("goal".to_string());
            available.sort();
            available.dedup();
            let hint = if available.is_empty() {
                String::new()
            } else {
                format!(" Available commands: {}", available.join(", "))
            };
            let message = format!("Command not found: \"{}\".{hint}", input.command);
            publish_error(deps, &input.session_id, &message)?;
            return Err(PromptError::Unknown { message });
        };
        let agent_name = cmd.agent.clone().or_else(|| input.agent.clone());

        // `$N` / `$ARGUMENTS` substitution and backtick-shell substitution
        // (prompt.ts:1378-1421), in an order upstream does not use.
        //
        // Upstream pastes the arguments into the template first and then
        // runs every `` !`…` `` it finds, so an argument that *contains*
        // `` !`…` `` — or that lands inside a configured one — is run as
        // shell, with no permission check. Here the shell spans are taken
        // from the configured template alone. Each runs as written, with
        // the arguments handed to the shell as data (`$1`…, `$ARGUMENTS`
        // are real shell parameters), and the text around them is filled
        // in one pass. Neither the arguments nor a command's output is
        // ever scanned again.
        // A command with inline shell is execution outside the model. The
        // operator wrote it and may run it; for a caller below `admin`
        // there is no reading of "the bash permission allows this" that
        // survives the arguments, so the command is refused whole, before
        // anything runs.
        let bounds = input
            .caller
            .bounds(deps.services.sessions.is_restricted(&input.session_id)?);
        if bounds.restricted && bash_regex().is_match(&cmd.template) {
            let message = format!(
                "The /{} command runs inline shell, which is not allowed in this session: it needs the admin scope and an unrestricted session.",
                input.command
            );
            publish_error(deps, &input.session_id, &message)?;
            return Err(PromptError::Forbidden { message });
        }
        let sh = preferred(deps.config.shell.as_deref());
        // Inline shell of a session that is restricted or that outside
        // input started runs in the restricted environment, whatever the
        // server's posture: the same floor as the session's bash tool.
        let outside_run = deps.services.sessions.runs_restricted(&input.session_id);
        let template =
            expand_command_template(&cmd.template, &input.arguments, &sh, outside_run).await;
        let template = template.trim().to_string();

        // Task model resolution (prompt.ts:1423-1433).
        let cmd_agent_model = cmd
            .agent
            .as_ref()
            .and_then(|agent| deps.services.agents.get(agent))
            .and_then(|agent| agent.model.clone());
        let task_model = if let Some(model) = &cmd.model {
            parse_model(model)
        } else if let Some(model) = &cmd_agent_model {
            prompt_input::ModelRef {
                provider_id: model.provider_id.clone(),
                model_id: model.model_id.clone(),
            }
        } else if let Some(model) = &input.model {
            parse_model(model)
        } else {
            self.current_model_ref(&input.session_id).await?
        };
        deps.models
            .get_model(
                &task_model.provider_id,
                &task_model.model_id,
                &input.session_id,
            )
            .await
            .map_err(|err| PromptError::Unknown {
                message: err.to_string(),
            })?;

        let agent =
            match &agent_name {
                Some(name) => deps.services.agents.get(name).cloned(),
                None => Some(deps.services.agents.default_info().map_err(|err| {
                    PromptError::Unknown {
                        message: err.to_string(),
                    }
                })?),
            };
        let Some(agent) = agent else {
            let available: Vec<String> = deps
                .services
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
            let name = agent_name.unwrap_or_default();
            let message = format!("Agent not found: \"{name}\".{hint}");
            publish_error(deps, &input.session_id, &message)?;
            return Err(PromptError::Unknown { message });
        };

        // The agent this command will run as — configured on the command,
        // named by the caller, or the default — is the one that is judged.
        bounds
            .check_agent(&agent)
            .map_err(|message| PromptError::Forbidden { message })?;

        // `@file` / `@agent` mentions. For the operator they are resolved
        // in the expanded text, arguments included (upstream). A caller
        // below `admin` only supplies text: mentions are resolved in the
        // operator's template alone, and an `@agent` there does not become
        // an agent part (which would skip the `task` permission ask).
        let template_parts = if bounds.restricted {
            let mut parts = self.resolve_prompt_parts(&cmd.template);
            parts.retain(|part| !matches!(part, PromptPartInput::Agent { .. }));
            if let Some(PromptPartInput::Text { text, .. }) = parts.first_mut() {
                *text = template.clone();
            }
            parts
        } else {
            self.resolve_prompt_parts(&template)
        };
        let input_files: Vec<String> = input
            .parts
            .iter()
            .filter(|part| part.url.starts_with("file:"))
            .map(|part| {
                prompt_input::file_url_to_path(&part.url)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let unique_template_parts: Vec<PromptPartInput> = template_parts
            .into_iter()
            .filter(|part| match part {
                PromptPartInput::File { url, .. } => !input_files.iter().any(|path| {
                    *path
                        == prompt_input::file_url_to_path(url)
                            .to_string_lossy()
                            .into_owned()
                }),
                _ => true,
            })
            .collect();

        // Subtask conversion (prompt.ts:1437-1451).
        let is_subtask = (agent.mode == crate::tool::def::AgentMode::Subagent
            && cmd.subtask != Some(false))
            || cmd.subtask == Some(true);
        let parts: Vec<PromptPartInput> = if is_subtask {
            vec![PromptPartInput::Subtask {
                id: None,
                agent: agent.name.clone(),
                description: cmd.description.clone().unwrap_or_default(),
                command: Some(input.command.clone()),
                model: Some(V1SubtaskModel {
                    provider_id: task_model.provider_id.clone(),
                    model_id: task_model.model_id.clone(),
                }),
                prompt: unique_template_parts
                    .iter()
                    .find_map(|part| match part {
                        PromptPartInput::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
            }]
        } else {
            let mut parts = unique_template_parts;
            parts.extend(input.parts.iter().map(|part| PromptPartInput::File {
                id: None,
                mime: part.mime.clone(),
                filename: part.filename.clone(),
                url: part.url.clone(),
                source: None,
            }));
            parts
        };

        let user_agent = if is_subtask {
            match &input.agent {
                Some(agent) => agent.clone(),
                None => {
                    deps.services
                        .agents
                        .default_info()
                        .map_err(|err| PromptError::Unknown {
                            message: err.to_string(),
                        })?
                        .name
                }
            }
        } else {
            agent.name.clone()
        };
        let user_model = if is_subtask {
            match &input.model {
                Some(model) => parse_model(model),
                None => self.current_model_ref(&input.session_id).await?,
            }
        } else {
            task_model.clone()
        };

        let result = self
            .prompt(PromptInput {
                session_id: input.session_id.clone(),
                message_id: input.message_id.clone(),
                model: Some(user_model),
                agent: Some(user_agent),
                no_reply: None,
                tools: None,
                format: None,
                system: None,
                variant: input.variant.clone(),
                parts,
                caller: input.caller,
            })
            .await?;

        deps.services.events.publish(
            &COMMAND_EXECUTED,
            serde_json::to_value(CommandExecutedData {
                name: input.command.clone(),
                session_id: input.session_id.clone(),
                arguments: input.arguments.clone(),
                message_id: match &result.info {
                    V1Message::User { id, .. } | V1Message::Assistant { id, .. } => id.clone(),
                },
            })
            .unwrap_or(Value::Null),
            PublishOptions::default(),
        )?;
        Ok(result)
    }

    /// `currentModel` (prompt.ts:614-633) — the provider-model slice.
    async fn current_model_ref(&self, session_id: &str) -> Result<ModelRef, PromptError> {
        let resolved = prompt_input::current_model(&self.prompt_deps(), session_id).await?;
        Ok(ModelRef {
            provider_id: resolved.provider_id,
            model_id: resolved.model_id,
        })
    }

    fn prompt_deps(&self) -> PromptDeps<'_> {
        prompt_deps_snapshot(&self.deps)
    }
}

/// The `TaskPromptOps` binding (prompt.ts:144-150) — the task tool's
/// production `TaskOps` calls back through this.
impl crate::session::task_ops::PromptFacade for SessionPrompt {
    fn cancel<'a>(
        &'a self,
        session_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move { SessionPrompt::cancel(self, session_id).await })
    }

    fn cancel_count(&self, session_id: &str) -> u64 {
        self.deps.services.run_state.cancel_count(session_id)
    }

    fn resolve_prompt_parts<'a>(
        &'a self,
        session_id: &'a str,
        agent: &'a str,
        template: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<Vec<PromptPartInput>, SessionError>> {
        // This seam serves prompts the model wrote (subagents, workflows).
        Box::pin(async move {
            crate::session::prompt_input::resolve_model_prompt_parts(
                &self.deps.services.agents,
                &self.deps.services.sessions,
                &self.deps.instance,
                session_id,
                agent,
                template,
            )
        })
    }

    fn resolve_operator_prompt_parts<'a>(
        &'a self,
        template: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<Vec<PromptPartInput>, SessionError>> {
        // What the operator wrote resolves like what the user typed.
        Box::pin(async move { Ok(SessionPrompt::resolve_prompt_parts(self, template)) })
    }

    fn prompt<'a>(
        &'a self,
        input: PromptInput,
    ) -> futures::future::BoxFuture<'a, Result<WithParts, SessionError>> {
        Box::pin(async move {
            SessionPrompt::prompt(self, input)
                .await
                .map_err(prompt_error)
        })
    }
}

fn prompt_error(error: PromptError) -> SessionError {
    match error {
        PromptError::Core(error) => error.into(),
        PromptError::Session(error) => error,
        PromptError::Unknown { message } | PromptError::Forbidden { message } => {
            crate::CoreError::Storage(message).into()
        }
    }
}

/// `PromptDeps` borrows; the facade stores `Arc`s, so snapshot per call.
fn prompt_deps_snapshot(deps: &SessionPromptDeps) -> PromptDeps<'_> {
    let (_, read) = deps.registry.named();
    PromptDeps {
        events: &deps.services.events,
        sessions: &deps.services.sessions,
        agents: &deps.services.agents,
        models: deps.input_models.as_ref(),
        read,
        mcp: deps.mcp.as_ref(),
        lsp: deps.lsp.as_ref(),
        images: deps.images.as_ref(),
        worktree: deps.instance.worktree.clone(),
        directory: deps.instance.directory.clone(),
        now_ms: deps.clock.now_ms(),
    }
}

fn loop_deps(deps: &SessionPromptDeps) -> LoopDeps {
    LoopDeps {
        sessions: deps.services.sessions.clone(),
        mcp_tools: deps.mcp_tools.clone(),
        truncate: deps.truncate.clone(),
        messages: deps.services.messages.clone(),
        events: deps.services.events.clone(),
        status: deps.services.status.clone(),
        agents: deps.services.agents.clone(),
        models: deps.models.clone(),
        llm: deps.llm.clone(),
        snapshot: deps.snapshot.clone(),
        compaction: deps.compaction.clone(),
        subtasks: deps.subtasks.clone(),
        summary: deps.summary.clone(),
        instruction: deps.instruction.clone(),
        systems: deps.systems.clone(),
        registry: deps.registry.clone(),
        permission: deps.services.permission.clone(),
        prompt_ops: deps.prompt_ops.clone(),
        config: deps.config.clone(),
        clock: deps.clock.clone(),
        instance: deps.instance.clone(),
        experimental_plan_mode: deps.experimental_plan_mode,
        vcs: deps.vcs,
        data_dir: deps.data_dir.clone(),
        project_id: deps.project_id.clone(),
        client: deps.client.clone(),
    }
}

/// `lastAssistant` as the runner's `onInterrupt` continuation
/// (prompt.ts:1350-1354).
fn last_assistant_work(
    deps: Arc<SessionPromptDeps>,
    session_id: &str,
) -> Work<WithParts, SessionError> {
    let deps = deps.clone();
    let session_id = session_id.to_string();
    Arc::new(move || {
        let deps = deps.clone();
        let session_id = session_id.clone();
        Box::pin(async move {
            last_assistant(&loop_deps(&deps), &session_id)
                .await
                .map_err(loop_error)
        })
    })
}

fn loop_error(error: LoopError) -> SessionError {
    match error {
        LoopError::Session(error) => error,
        other => crate::CoreError::Storage(other.to_string()).into(),
    }
}

fn publish_error(
    deps: &SessionPromptDeps,
    session_id: &str,
    message: &str,
) -> Result<(), SessionError> {
    deps.services.events.publish(
        &SESSION_ERROR,
        serde_json::json!({
            "sessionID": session_id,
            "error": { "name": "Unknown", "data": { "message": message } },
        }),
        PublishOptions::default(),
    )?;
    Ok(())
}

/// `Provider.parseModel` (provider/provider.ts): split at the first `/`.
pub fn parse_model(model: &str) -> ModelRef {
    match model.split_once('/') {
        Some((provider_id, model_id)) => ModelRef {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
        },
        None => ModelRef {
            provider_id: model.to_string(),
            model_id: String::new(),
        },
    }
}

// ---------------------------------------------------------------------------
// command regexes (prompt.ts:1592-1596)
// ---------------------------------------------------------------------------

const ARGS_REGEX: &str = r#"(?:\[Image\s+\d+\]|"[^"]*"|'[^']*'|[^\s"']+)"#;
const QUOTE_TRIM_REGEX: &str = r#"^["']|["']$"#;

fn args_regex() -> &'static regex::Regex {
    static ARGS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    ARGS.get_or_init(|| {
        regex::RegexBuilder::new(ARGS_REGEX)
            .case_insensitive(true)
            .build()
            .expect("valid args regex")
    })
}

fn quote_trim_regex() -> &'static regex::Regex {
    static QUOTE_TRIM: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    QUOTE_TRIM.get_or_init(|| regex::Regex::new(QUOTE_TRIM_REGEX).expect("valid quote-trim regex"))
}

fn placeholder_regex() -> &'static regex::Regex {
    static PLACEHOLDER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PLACEHOLDER.get_or_init(|| regex::Regex::new(r"\$(\d+)").expect("valid placeholder regex"))
}

/// How a command's arguments fill its template: worked out once from the
/// *configured* template, then applied to its text and handed to its inline
/// shell as parameters.
struct ArgumentPlan {
    /// The arguments as typed.
    raw: String,
    /// The arguments split and unquoted (`argsRegex` + `quoteTrimRegex`).
    args: Vec<String>,
    /// The highest `$N` the template uses; it takes every argument from
    /// there on.
    last: u64,
    /// Whether the template names any argument at all.
    placed: bool,
}

impl ArgumentPlan {
    fn new(template: &str, arguments: &str) -> ArgumentPlan {
        let args: Vec<String> = match_args(arguments)
            .iter()
            .map(|arg| quote_trim(arg))
            .collect();
        let mut last = 0u64;
        for item in placeholder_regex().find_iter(template) {
            let value: u64 = item
                .as_str()
                .get(1..)
                .and_then(|m| m.parse().ok())
                .unwrap_or(0);
            // `$0` is not a positional argument; it stays literal.
            if value > last {
                last = value;
            }
        }
        ArgumentPlan {
            raw: arguments.to_string(),
            args,
            last,
            placed: last > 0 || template.contains("$ARGUMENTS"),
        }
    }

    /// What `$position` stands for (`position >= 1`).
    fn positional(&self, position: u64) -> String {
        let index = (position as usize) - 1;
        if index >= self.args.len() {
            String::new()
        } else if position == self.last {
            self.args[index..].join(" ")
        } else {
            self.args[index].clone()
        }
    }

    /// `$1`, `$2`, … as an inline shell command receives them.
    fn positionals(&self) -> Vec<String> {
        (1..=self.last)
            .map(|position| self.positional(position))
            .collect()
    }

    /// Fill the placeholders in a stretch of template text. One pass for
    /// both `$N` and `$ARGUMENTS`: text the user typed is inserted exactly
    /// once and never re-scanned, so an argument containing `$1` or
    /// `$ARGUMENTS` stays as written.
    fn fill(&self, text: &str) -> String {
        substitution_regex()
            .replace_all(text, |captures: &regex::Captures| {
                let Some(position) = captures.get(1) else {
                    return self.raw.clone();
                };
                let position: u64 = position.as_str().parse().unwrap_or(0);
                if position == 0 {
                    return captures[0].to_string();
                }
                self.positional(position)
            })
            .into_owned()
    }

    /// A template that names no argument gets them appended.
    fn append_unplaced(&self, out: &mut String) {
        if !self.placed && !self.raw.trim().is_empty() {
            out.push_str("\n\n");
            out.push_str(&self.raw);
        }
    }
}

/// Expand a configured command template with the arguments a caller typed:
/// fill `$N` / `$ARGUMENTS` in its text and replace each inline shell span
/// with the command's output.
///
/// Only shell the *template* contains ever runs, exactly as the template
/// has it. Arguments reach it as shell parameters, and neither they nor a
/// command's output are scanned for `` !`…` `` or placeholders afterwards.
async fn expand_command_template(
    template: &str,
    arguments: &str,
    shell: &str,
    outside_run: bool,
) -> String {
    let plan = ArgumentPlan::new(template, arguments);
    let spans: Vec<(std::ops::Range<usize>, &str)> = bash_regex()
        .captures_iter(template)
        .map(|captures| {
            let whole = captures.get(0).expect("whole match");
            let command = captures.get(1).map(|m| m.as_str()).unwrap_or_default();
            (whole.range(), command)
        })
        .collect();
    let mut out = String::new();
    let mut offset = 0;
    for (range, command) in spans {
        out.push_str(&plan.fill(&template[offset..range.start]));
        out.push_str(&run_shell_text(shell, command, &plan, outside_run).await);
        offset = range.end;
    }
    out.push_str(&plan.fill(&template[offset..]));
    plan.append_unplaced(&mut out);
    out
}

/// Fill a command template's `$N` and `$ARGUMENTS` placeholders; when the
/// template has neither, the arguments are appended. (A template with
/// inline shell is expanded piecewise in `SessionPrompt::command`.)
#[cfg(test)]
fn substitute_arguments(template: &str, arguments: &str) -> String {
    let plan = ArgumentPlan::new(template, arguments);
    let mut out = plan.fill(template);
    plan.append_unplaced(&mut out);
    out
}

/// `$N` (group 1) or `$ARGUMENTS`, matched together so substitution is a
/// single pass.
fn substitution_regex() -> &'static regex::Regex {
    static SUBSTITUTION: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    SUBSTITUTION.get_or_init(|| {
        regex::Regex::new(r"\$(\d+)|\$ARGUMENTS").expect("valid substitution regex")
    })
}

fn bash_regex() -> &'static regex::Regex {
    static BASH: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    BASH.get_or_init(|| regex::Regex::new(r"!`([^`]+)`").expect("valid bash regex"))
}

/// `input.arguments.match(argsRegex)` — the full match list.
fn match_args(arguments: &str) -> Vec<String> {
    args_regex()
        .find_iter(arguments)
        .map(|m| m.as_str().to_string())
        .collect()
}

/// `arg.replace(quoteTrimRegex, "")` — quote-stripped for matching.
fn quote_trim(arg: &str) -> String {
    quote_trim_regex().replace_all(arg, "").into_owned()
}

/// `Shell.preferred` (core/shell.ts:205-208) — config value, else
/// `$SHELL`, else the platform default.
fn preferred(config_shell: Option<&str>) -> String {
    config_shell.map(str::to_string).unwrap_or_else(|| {
        std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| "sh".to_string())
    })
}

/// `Process.text([cmd], { shell: sh, nothrow: true }).text` — stdout of
/// `sh -c cmd`, the command exactly as the template has it. The command's
/// arguments reach it as shell parameters (`$1`…, and `ARGUMENTS` in the
/// environment), never as shell source.
async fn run_shell_text(
    shell: &str,
    command: &str,
    plan: &ArgumentPlan,
    outside_run: bool,
) -> String {
    let mut process = tokio::process::Command::new(shell);
    crate::shell_env::apply_shell_for(process.as_std_mut(), outside_run);
    process
        .arg("-c")
        .arg(command)
        // `$0`, as it would be without parameters.
        .arg(shell)
        .args(plan.positionals())
        .env("ARGUMENTS", &plan.raw)
        .output()
        .await
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// shell / shellImpl (prompt.ts:451-592)
// ---------------------------------------------------------------------------

/// `ShellInput` (prompt.ts:1543-1549).
#[derive(Debug, Clone)]
pub struct ShellInput {
    pub session_id: String,
    pub message_id: Option<String>,
    pub agent: String,
    pub model: Option<ModelRef>,
    pub command: String,
}

/// `CommandInput` (prompt.ts:1551-1581) — `parts` is the file-part array.
#[derive(Debug, Clone)]
pub struct CommandInput {
    pub session_id: String,
    pub message_id: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
    pub arguments: String,
    pub command: String,
    pub parts: Vec<CommandFilePart>,
    /// Where the command comes from (alforria extension,
    /// [`crate::session::caller`]). In a restricted turn a command whose
    /// template has inline shell is refused, and the arguments are text
    /// only.
    pub caller: crate::session::caller::Caller,
}

#[derive(Debug, Clone)]
pub struct CommandFilePart {
    pub mime: String,
    pub filename: Option<String>,
    pub url: String,
}

/// `shellImpl` (prompt.ts:451-592 — binding). Public so the abort path is
/// observable outside the run state (which drops the work on cancel).
pub async fn shell_impl(
    deps: &Arc<SessionPromptDeps>,
    input: ShellInput,
    cancel: CancellationToken,
) -> Result<WithParts, SessionError> {
    let session = deps.services.sessions.get(&input.session_id)?;
    if session.revert.is_some() {
        deps.revert.cleanup(&session)?;
    }

    // Agent resolution (prompt.ts:466-476).
    let Some(agent) = deps.services.agents.get(&input.agent).cloned() else {
        let available: Vec<String> = deps
            .services
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
        let message = format!("Agent not found: \"{}\".{hint}", input.agent);
        publish_error(deps, &input.session_id, &message)?;
        return Err(crate::CoreError::Storage(message).into());
    };

    // Model resolution (prompt.ts:477-479).
    let model = match &input.model {
        Some(model) => model.clone(),
        None => match &agent.model {
            Some(model) => ModelRef {
                provider_id: model.provider_id.clone(),
                model_id: model.model_id.clone(),
            },
            None => {
                let prompt_deps = prompt_deps_snapshot(deps);
                let resolved = prompt_input::current_model(&prompt_deps, &input.session_id)
                    .await
                    .map_err(|err| crate::CoreError::Storage(err.to_string()))?;
                ModelRef {
                    provider_id: resolved.provider_id,
                    model_id: resolved.model_id,
                }
            }
        },
    };

    // User message + synthetic part (prompt.ts:481-501).
    let user_msg = V1Message::User {
        id: match &input.message_id {
            Some(id) => id.clone(),
            None => MessageId::ascending(None)?,
        },
        session_id: input.session_id.clone(),
        time: UserTime {
            created: deps.clock.now_ms() as f64,
        },
        format: None,
        summary: None,
        agent: input.agent.clone(),
        model: V1UserModel {
            provider_id: model.provider_id.clone(),
            model_id: model.model_id.clone(),
            variant: None,
        },
        system: None,
        tools: None,
    };
    deps.services.sessions.update_message(&user_msg)?;
    deps.services.sessions.update_part(&V1Part::Text {
        id: PartId::ascending(None)?,
        session_id: input.session_id.clone(),
        message_id: message_id(&user_msg),
        text: "The following tool was executed by the user".to_string(),
        synthetic: Some(true),
        ignored: None,
        time: None,
        metadata: None,
    })?;

    // Assistant message + running bash part (prompt.ts:503-533).
    let msg = V1Message::Assistant {
        id: MessageId::ascending(None)?,
        session_id: input.session_id.clone(),
        time: AssistantTime {
            created: deps.clock.now_ms(),
            completed: None,
        },
        error: None,
        parent_id: message_id(&user_msg),
        model_id: model.model_id.clone(),
        provider_id: model.provider_id.clone(),
        mode: input.agent.clone(),
        agent: input.agent.clone(),
        path: alforria_schema::session_v1::V1Path {
            cwd: deps.instance.directory.to_string_lossy().into_owned(),
            root: deps.instance.worktree.to_string_lossy().into_owned(),
        },
        summary: None,
        cost: 0.0,
        tokens: empty_tokens(),
        structured: None,
        variant: None,
        finish: None,
    };
    deps.services.sessions.update_message(&msg)?;

    let started = deps.clock.now_ms();
    let mut part = V1Part::Tool {
        id: PartId::ascending(None)?,
        session_id: input.session_id.clone(),
        message_id: message_id(&msg),
        tool: crate::tool::shell::TOOL_ID.to_string(),
        call_id: Ulid::new().to_string(),
        metadata: None,
        state: V1ToolState::Running {
            input: {
                let mut input_map = serde_json::Map::new();
                input_map.insert("command".to_string(), json!(input.command));
                input_map
            },
            title: None,
            metadata: None,
            time: alforria_schema::session_v1::ToolStateRunningTime { start: started },
        },
    };
    deps.services.sessions.update_part(&part)?;

    // Spawn through the preferred shell (prompt.ts:535-585): `TERM: dumb`,
    // stdin ignored, force kill after 3 seconds.
    let sh = preferred(deps.config.shell.as_deref());
    let cwd = deps.instance.directory.clone();
    let mut command = tokio::process::Command::new(&sh);
    // The operator's own command, but in a restricted session, or one a
    // webhook or a watch trigger started, it still gets the restricted
    // environment: the same floor as the session's bash tool.
    crate::shell_env::apply_shell_for(
        command.as_std_mut(),
        deps.services.sessions.runs_restricted(&input.session_id),
    );
    command
        .arg("-c")
        .arg(&input.command)
        .current_dir(&cwd)
        .env("TERM", "dumb")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Its own process group: `kill_process_group` signals the group, which
    // only exists when the child leads one.
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|err| crate::CoreError::Storage(format!("Failed to spawn command: {err}")))?;
    let pid = child.id().unwrap_or(0);
    let _group = crate::process_group::register(pid);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<String>(64);
    spawn_reader(stdout, chunk_tx.clone());
    spawn_reader(stderr, chunk_tx.clone());
    drop(chunk_tx);

    let mut output = String::new();
    let mut aborted = false;

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                aborted = true;
                kill_process_group(pid, Duration::from_secs(3));
                break;
            }
            chunk = chunk_rx.recv() => match chunk {
                Some(chunk) => {
                    output.push_str(&chunk);
                    let input_map = tool_input(&part);
                    if let V1Part::Tool { state, .. } = &mut part {
                        if matches!(state, V1ToolState::Running { .. }) {
                            let mut metadata = serde_json::Map::new();
                            metadata.insert("output".to_string(), json!(output));
                            *state = V1ToolState::Running {
                                input: input_map,
                                title: None,
                                metadata: Some(metadata),
                                time: alforria_schema::session_v1::ToolStateRunningTime {
                                    start: started,
                                },
                            };
                            deps.services.sessions.update_part(&part)?;
                        }
                    }
                }
                None => break,
            },
        }
    }
    let _ = child.wait().await;

    // `finish` (prompt.ts:565-589) — uninterruptible.
    if aborted {
        output.push_str("\n\n<metadata>\nUser aborted the command\n</metadata>");
    }
    let completed = deps.clock.now_ms();
    let mut finished = msg.clone();
    if let V1Message::Assistant { time, .. } = &mut finished {
        if time.completed.is_none() {
            time.completed = Some(completed);
            deps.services.sessions.update_message(&finished)?;
        }
    }
    let mut updated = part.clone();
    if let V1Part::Tool { state, .. } = &mut updated {
        if matches!(state, V1ToolState::Running { .. }) {
            let mut metadata = serde_json::Map::new();
            metadata.insert("output".to_string(), json!(output));
            *state = V1ToolState::Completed {
                input: tool_input(&part),
                output: output.clone(),
                title: String::new(),
                metadata,
                time: alforria_schema::session_v1::ToolStateCompletedTime {
                    start: started,
                    end: completed,
                    compacted: None,
                },
                attachments: None,
            };
            deps.services.sessions.update_part(&updated)?;
        }
    }

    Ok(WithParts {
        info: finished,
        parts: vec![updated],
    })
}

fn spawn_reader<R>(stream: Option<R>, chunk_tx: tokio::sync::mpsc::Sender<String>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(stream) = stream else { return };
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut stream = stream;
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if chunk_tx.send(text).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// `forceKillAfter: "3 seconds"` — SIGTERM the process group now,
/// SIGKILL after the grace window (best effort; the spawned timer dies
/// with the process reaper). On Windows there is no graceful signal, so
/// the whole process tree is force-killed immediately.
fn kill_process_group(pid: u32, force_after: Duration) {
    #[cfg(unix)]
    {
        let pgid = pid as libc::pid_t;
        if pgid == 0 {
            return;
        }
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        tokio::spawn(async move {
            tokio::time::sleep(force_after).await;
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        });
    }
    #[cfg(not(unix))]
    {
        let _ = force_after;
        // shell_env: not routed. A fixed Windows system tool that kills a process
        // tree by pid; nothing the agent or a config names.
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn tool_input(part: &V1Part) -> serde_json::Map<String, Value> {
    match part {
        V1Part::Tool {
            state: V1ToolState::Running { input, .. },
            ..
        } => input.clone(),
        _ => serde_json::Map::new(),
    }
}

fn message_id(message: &V1Message) -> String {
    match message {
        V1Message::User { id, .. } | V1Message::Assistant { id, .. } => id.clone(),
    }
}

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

#[cfg(test)]
mod substitution_tests {
    use super::substitute_arguments;

    #[test]
    fn positional_and_arguments_placeholders() {
        assert_eq!(
            substitute_arguments("do $1 then $2", "a b c"),
            "do a then b c"
        );
        assert_eq!(substitute_arguments("run: $ARGUMENTS", "x y"), "run: x y");
        assert_eq!(
            substitute_arguments("no placeholders", "x y"),
            "no placeholders\n\nx y"
        );
        assert_eq!(
            substitute_arguments("no placeholders", "  "),
            "no placeholders"
        );
    }

    #[test]
    fn user_text_is_never_re_expanded() {
        // `$ARGUMENTS` typed as an argument used to be replaced a second time.
        assert_eq!(
            substitute_arguments("first: $1, then: $2 | all: $ARGUMENTS", "$ARGUMENTS x"),
            "first: $ARGUMENTS, then: x | all: $ARGUMENTS x"
        );
        assert_eq!(substitute_arguments("all: $ARGUMENTS", "$1"), "all: $1");
    }

    /// The injection this ordering closes: an argument that carries inline
    /// shell, or that lands inside a configured inline command, must reach
    /// the model (or the command) as text and never run.
    #[cfg(unix)]
    #[tokio::test]
    async fn arguments_never_run_as_shell() {
        use super::expand_command_template;
        let dir = crate::storage::test_support::TempDir::new("command-injection");
        let pwned = dir.path().join("pwned");
        let touch = format!("touch {}", pwned.display());

        // An argument that is itself an inline command.
        let out = expand_command_template(
            "Review this: $ARGUMENTS",
            &format!("!`{touch}`"),
            "sh",
            false,
        )
        .await;
        assert_eq!(out, format!("Review this: !`{touch}`"));
        assert!(!pwned.exists(), "the argument was executed");

        // The same through `$1`, and appended to a template without
        // placeholders.
        for template in ["Look at $1", "No placeholders here"] {
            let out = expand_command_template(template, &format!("!`{touch}`"), "sh", false).await;
            assert!(out.contains(&format!("!`{touch}`")), "{out}");
            assert!(!pwned.exists(), "{template}: the argument was executed");
        }

        // An argument substituted *into* a configured inline command cannot
        // break out of it: `;`, `$(…)` and backticks arrive as data.
        for payload in [
            format!("x; {touch}"),
            format!("$({touch})"),
            format!("`{touch}`"),
            format!("x\n{touch}"),
            format!("x && {touch}"),
        ] {
            for template in [
                "out: !`echo $1`",
                "out: !`echo $ARGUMENTS`",
                "out: !`echo \"$1\"`",
            ] {
                let out = expand_command_template(template, &payload, "sh", false).await;
                assert!(!pwned.exists(), "{template} ran {payload:?}: {out}");
            }
        }

        // The configured command still runs, with its arguments.
        let out = expand_command_template(
            "branch: !`printf '%s|%s' \"$1\" \"$ARGUMENTS\"` end $2",
            "main 'two words'",
            "sh",
            false,
        )
        .await;
        assert_eq!(out, "branch: main|main 'two words' end two words");

        // A command's output is not scanned again either.
        let note = dir.path().join("note.txt");
        std::fs::write(&note, format!("!`{touch}` $1 $ARGUMENTS")).unwrap();
        let out =
            expand_command_template(&format!("!`cat {}`", note.display()), "arg", "sh", false)
                .await;
        assert_eq!(out, format!("!`{touch}` $1 $ARGUMENTS\n\narg"));
        assert!(!pwned.exists(), "a command's output was executed");
    }

    /// An argument full of shell metacharacters reaches an inline command
    /// as one literal value.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_argument_reaches_inline_shell_as_a_single_literal() {
        use super::expand_command_template;
        let nasty = "a;b|c&d$(e)`f`>g<h*?~'i'\\j";
        let out =
            expand_command_template("!`printf '%s' \"$ARGUMENTS\"`", nasty, "sh", false).await;
        assert_eq!(out, nasty);
        // As `$1`, the split-and-unquoted first argument, still literal.
        let out =
            expand_command_template("!`printf '[%s]' \"$1\"`", "'x;y $(z)' tail", "sh", false)
                .await;
        assert_eq!(out, "[x;y $(z) tail]");
        let out = expand_command_template(
            "!`printf '[%s][%s]' \"$1\" \"$2\"`",
            "'x;y $(z)' `w`",
            "sh",
            false,
        )
        .await;
        assert_eq!(out, "[x;y $(z)][`w`]");
    }

    /// The per-run floor follows one predicate
    /// (`SessionStore::runs_restricted`) wherever a session starts a
    /// shell: on a standalone server the `shell` command and the inline
    /// template shell of a restricted session — one with no outside-start
    /// mark — get the restricted environment, as its bash tool does
    /// (`tool::shell`'s floor test), and so do those of a session with the
    /// mark alone. A session of the user's own keeps the user's
    /// environment.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_restricted_sessions_shell_command_and_inline_shell_get_the_restricted_environment() {
        use super::{expand_command_template, ShellInput};
        use crate::session::store::SessionMark;
        use crate::session::test_support::{create_engine_session, engine_with_config};
        use alforria_schema::session_v1::{V1Part, V1ToolState};

        std::env::set_var("FLOORSHELL_GITHUB_TOKEN", "ghp-floor-leak");
        std::env::set_var("FLOORSHELL_PLAIN", "plain-floor-leak");
        assert!(
            !crate::shell_env::current().is_restricted(),
            "the server's own posture is standalone in this test"
        );
        let e = engine_with_config(
            "prompt-floor-shell",
            Vec::new(),
            serde_json::json!({ "shell": "sh" }),
        );
        let sessions = &e.services.sessions;
        let probe = "printf '[%s][%s][%s]' \"$FLOORSHELL_GITHUB_TOKEN\" \"$FLOORSHELL_PLAIN\" \"${PATH:+path}\"";
        let template = format!("!`{probe}`");
        let shell = |session_id: String| async {
            let turn = e
                .prompt
                .shell(ShellInput {
                    session_id,
                    message_id: None,
                    agent: "build".to_string(),
                    model: None,
                    command: probe.to_string(),
                })
                .await
                .expect("the shell command runs");
            turn.parts
                .iter()
                .find_map(|part| match part {
                    V1Part::Tool {
                        state: V1ToolState::Completed { output, .. },
                        ..
                    } => Some(output.trim().to_string()),
                    _ => None,
                })
                .expect("a completed shell part")
        };
        const OWN: &str = "[ghp-floor-leak][plain-floor-leak][path]";
        const FLOOR: &str = "[][][path]";

        // The user's own session.
        let own = create_engine_session(&e);
        assert!(!sessions.runs_restricted(&own.id));
        assert_eq!(shell(own.id.clone()).await, OWN);
        assert_eq!(
            expand_command_template(&template, "", "sh", sessions.runs_restricted(&own.id)).await,
            OWN
        );

        // A restricted session that no outside input started.
        let restricted = create_engine_session(&e);
        sessions.restrict(&restricted.id).unwrap();
        assert!(!sessions.started_by_outside_input(&restricted.id));
        assert_eq!(shell(restricted.id.clone()).await, FLOOR, "shell command");
        assert_eq!(
            expand_command_template(
                &template,
                "",
                "sh",
                sessions.runs_restricted(&restricted.id)
            )
            .await,
            FLOOR,
            "inline template shell"
        );

        // The outside-start mark alone.
        let marked = create_engine_session(&e);
        sessions
            .mark(&marked.id, SessionMark::OutsideStart)
            .unwrap();
        assert!(!sessions.is_restricted(&marked.id).unwrap());
        assert_eq!(shell(marked.id.clone()).await, FLOOR, "a marked session");

        assert!(
            !crate::shell_env::current().is_restricted(),
            "the floor did not go through the server's posture"
        );
    }

    #[test]
    fn dollar_zero_stays_literal() {
        assert_eq!(substitute_arguments("echo $0 $1", "a"), "echo $0 a");
        // A template with only `$0` has no real placeholder: arguments append.
        assert_eq!(substitute_arguments("echo $0", "a"), "echo $0\n\na");
    }
}
