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

use opencode_schema::legacy_event::CommandExecutedData;
use opencode_schema::permission_v1::{PermissionV1Action, PermissionV1Rule};
use opencode_schema::session_v1::{
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
    deps: Arc<SessionPromptDeps>,
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
    pub async fn prompt(&self, input: PromptInput) -> Result<WithParts, PromptError> {
        let session = self.deps.services.sessions.get(&input.session_id)?;
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
        Ok(self.loop_(&input.session_id).await?)
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
        let Some(cmd) = deps
            .config
            .command
            .as_ref()
            .and_then(|commands| commands.get(&input.command))
        else {
            let available: Vec<String> = deps
                .config
                .command
                .as_ref()
                .map(|commands| commands.keys().cloned().collect())
                .unwrap_or_default();
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

        // `$N` / `$ARGUMENTS` substitution (prompt.ts:1378-1408).
        let args: Vec<String> = match_args(&input.arguments)
            .iter()
            .map(|arg| quote_trim(arg))
            .collect();

        let template_command = &cmd.template;
        let mut placeholders: Vec<u64> = Vec::new();
        let mut last = 0u64;
        for item in placeholder_regex().find_iter(template_command) {
            let value: u64 = item
                .as_str()
                .get(1..)
                .and_then(|m| m.parse().ok())
                .unwrap_or(0);
            placeholders.push(value);
            if value > last {
                last = value;
            }
        }

        let mut template = placeholder_regex()
            .replace_all(template_command, |captures: &regex::Captures| {
                let position: u64 = captures
                    .get(1)
                    .map(|m| m.as_str())
                    .and_then(|m| m.parse().ok())
                    .unwrap_or(0);
                let arg_index = (position as usize).saturating_sub(1);
                if arg_index >= args.len() {
                    return String::new();
                }
                if position == last {
                    args[arg_index..].join(" ")
                } else {
                    args[arg_index].clone()
                }
            })
            .into_owned();
        let uses_arguments_placeholder = template_command.contains("$ARGUMENTS");
        template = template.replace("$ARGUMENTS", &input.arguments);

        if placeholders.is_empty()
            && !uses_arguments_placeholder
            && !input.arguments.trim().is_empty()
        {
            template = format!("{template}\n\n{}", input.arguments);
        }

        // Backtick-shell substitution (prompt.ts:1411-1421).
        let shell_matches: Vec<String> = bash_regex()
            .captures_iter(&template)
            .map(|captures| {
                captures
                    .get(1)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default()
            })
            .collect();
        if !shell_matches.is_empty() {
            let sh = preferred(deps.config.shell.as_deref());
            let mut results = Vec::new();
            for command in &shell_matches {
                results.push(run_shell_text(&sh, command).await);
            }
            let mut replaced = String::new();
            let mut offset = 0;
            for captures in bash_regex().captures_iter(&template) {
                let whole = captures.get(0).expect("whole match");
                let next = results
                    .get(captures_count(&template[..whole.start()]))
                    .cloned()
                    .unwrap_or_default();
                replaced.push_str(&template[offset..whole.start()]);
                replaced.push_str(&next);
                offset = whole.end();
            }
            replaced.push_str(&template[offset..]);
            template = replaced;
        }
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

        let template_parts = self.resolve_prompt_parts(&template);
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

    fn resolve_prompt_parts<'a>(
        &'a self,
        template: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<Vec<PromptPartInput>, SessionError>> {
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
        PromptError::Unknown { message } => crate::CoreError::Storage(message).into(),
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
        now_ms: deps.clock.now_ms(),
    }
}

fn loop_deps(deps: &SessionPromptDeps) -> LoopDeps {
    LoopDeps {
        sessions: deps.services.sessions.clone(),
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

/// How many `!`...`` backtick matches precede `offset` bytes of the
/// template — the index of the next shell substitution.
fn captures_count(template: &str) -> usize {
    bash_regex().find_iter(template).count()
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
/// `sh -c cmd`.
async fn run_shell_text(shell: &str, command: &str) -> String {
    tokio::process::Command::new(shell)
        .arg("-c")
        .arg(command)
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
#[derive(Debug, Clone, Default)]
pub struct CommandInput {
    pub session_id: String,
    pub message_id: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub variant: Option<String>,
    pub arguments: String,
    pub command: String,
    pub parts: Vec<CommandFilePart>,
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
        path: opencode_schema::session_v1::V1Path {
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
            time: opencode_schema::session_v1::ToolStateRunningTime { start: started },
        },
    };
    deps.services.sessions.update_part(&part)?;

    // Spawn through the preferred shell (prompt.ts:535-585): `TERM: dumb`,
    // stdin ignored, force kill after 3 seconds.
    let sh = preferred(deps.config.shell.as_deref());
    let cwd = deps.instance.directory.clone();
    let mut child = tokio::process::Command::new(&sh)
        .arg("-c")
        .arg(&input.command)
        .current_dir(&cwd)
        .env("TERM", "dumb")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| crate::CoreError::Storage(format!("Failed to spawn command: {err}")))?;
    let pid = child.id().unwrap_or(0);

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
                                time: opencode_schema::session_v1::ToolStateRunningTime {
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
                time: opencode_schema::session_v1::ToolStateCompletedTime {
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
/// with the process reaper).
fn kill_process_group(pid: u32, force_after: Duration) {
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
