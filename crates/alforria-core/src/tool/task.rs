//! `task` tool — port of `tool/task.ts` (spec M4.8).
//!
//! Session machinery (session walking, message variants, background jobs)
//! lives behind the [`TaskOps`] seam; M5 provides the production impl.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::session::retry::{resume_note, untrusted_output, wait_to_resume, AUTO_RESUME_DELAYS};
use crate::tool::def::{define, Agents, AskRequest, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::truncate::Truncate;

pub const BACKGROUND_DESCRIPTION: &str = "Background mode: background=true launches the subagent asynchronously and returns immediately. Foreground is the default; use it when you need the result before continuing. Use background only for independent work that can run while you continue elsewhere. You will be notified automatically when it finishes.";

pub const BACKGROUND_STARTED: &str = "The task is working in the background. You will be notified automatically when it finishes.\nDO NOT sleep, poll for progress, ask the task for status, or duplicate this task's work — avoid working with the same files or topics it is using.\nWork on non-overlapping tasks, or briefly tell the user what you launched and end your response.";

pub const BACKGROUND_UPDATED: &str = "Additional context sent to the running background task.\nThe task is still working in the background. You will be notified automatically when it finishes.\nDO NOT sleep, poll for progress, ask the task for status, or duplicate this task's work — avoid working with the same files or topics it is using.\nWork on non-overlapping tasks, or briefly tell the user what you sent and end your response.";

/// How much of a failed subagent's last output its parent gets.
const FAILURE_OUTPUT_CHARS: usize = 2000;

/// `renderOutput` (task.ts:55-66): the exact XML wrapper.
pub fn render_output(input: RenderOutput<'_>) -> String {
    let tag = if input.state == "error" {
        "task_error"
    } else {
        "task_result"
    };
    let mut lines = vec![format!(
        "<task id=\"{}\" state=\"{}\">",
        input.session_id, input.state
    )];
    if let Some(summary) = input.summary {
        lines.push(format!("<summary>{summary}</summary>"));
    }
    lines.push(format!("<{tag}>"));
    lines.push(input.text.to_string());
    lines.push(format!("</{tag}>"));
    lines.push("</task>".to_string());
    lines.join("\n")
}

pub struct RenderOutput<'a> {
    pub session_id: &'a str,
    pub state: &'a str,
    pub summary: Option<&'a str>,
    pub text: &'a str,
}

/// Result of a completed (or failed) prompt run through [`TaskOps`].
/// The parent message's variant + model (`Not an assistant message`
/// otherwise).
#[derive(Debug, Clone)]
pub struct ParentMessage {
    pub variant: Option<String>,
    pub model: Option<ModelRef>,
}

#[derive(Debug, Clone)]
pub struct PromptOutcome {
    /// Last text part.
    pub text: String,
    /// `result.info.error.message` / failing tool error.
    pub error: Option<String>,
    /// Whether prompting again may get past the error: true for a provider or
    /// call failure, false for a rejected tool, bad credentials or an abort.
    pub recoverable: bool,
}

/// Where a subagent stands, from its session and its latest messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentStatus {
    NotStarted,
    Running,
    Retrying { attempt: u64, message: String },
    Completed,
    Failed,
    Cancelled,
}

impl SubagentStatus {
    /// The `state` attribute of [`render_output`].
    pub fn as_str(&self) -> &'static str {
        match self {
            SubagentStatus::NotStarted => "not_started",
            SubagentStatus::Running => "running",
            SubagentStatus::Retrying { .. } => "retrying",
            SubagentStatus::Completed => "completed",
            SubagentStatus::Failed => "error",
            SubagentStatus::Cancelled => "cancelled",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self,
            SubagentStatus::Running | SubagentStatus::Retrying { .. }
        )
    }
}

/// One subagent as its parent sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentState {
    pub session_id: String,
    pub parent_id: Option<String>,
    pub title: String,
    pub agent: Option<String>,
    pub status: SubagentStatus,
    pub error: Option<String>,
    /// Its latest text, from any turn: a failed turn often has none.
    pub last_text: Option<String>,
    pub tool_calls: usize,
}

/// A resolvable agent (subset of `Agent.Info` the tool needs).
#[derive(Debug, Clone)]
pub struct SubagentInfo {
    pub name: String,
    pub model: Option<ModelRef>,
    pub permission: Vec<Rule>,
}

#[derive(Debug, Clone)]
pub struct ModelRef {
    pub model_id: String,
    pub provider_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub permission: String,
    pub pattern: String,
    pub action: &'static str,
}

/// The session/prompt machinery seam (M5 implements).
pub trait TaskOps: Send + Sync {
    /// Depth of `session_id` in the parent chain (0 = top-level).
    fn depth<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<usize, ToolError>>;
    /// Fetch a subagent definition; `None` = unknown agent.
    fn agent<'a>(&'a self, name: &'a str)
        -> BoxFuture<'a, Result<Option<SubagentInfo>, ToolError>>;
    /// Look up an existing session by task id.
    fn session_exists<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, bool>;
    /// The parent session's permission ruleset (`parent.permission ?? []`,
    /// task.ts:138) — the production data path for
    /// [`derive_subagent_session_permission`]. Defaults to the M4
    /// test-double behavior (empty).
    fn session_permission<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Vec<Rule>> {
        Box::pin(async { Vec::new() })
    }
    /// Create the child session; returns the new session id.
    fn create_session<'a>(
        &'a self,
        parent_id: &'a str,
        title: &'a str,
        agent: &'a str,
        permission: Vec<Rule>,
    ) -> BoxFuture<'a, Result<String, ToolError>>;
    /// The parent message's variant + model (`Not an assistant message`
    /// error otherwise).
    fn parent_message<'a>(
        &'a self,
        session_id: &'a str,
        message_id: &'a str,
    ) -> BoxFuture<'a, Result<ParentMessage, ToolError>>;
    /// Run the subagent prompt; returns its outcome.
    fn prompt<'a>(
        &'a self,
        session_id: &'a str,
        agent: &'a str,
        model: &'a ModelRef,
        variant: Option<&'a str>,
        prompt: &'a str,
    ) -> BoxFuture<'a, Result<PromptOutcome, ToolError>>;
    /// [`Self::prompt`] with a note from the system rather than from a person:
    /// the model reads it, and clients show it as what it is.
    fn resume<'a>(
        &'a self,
        session_id: &'a str,
        agent: &'a str,
        model: &'a ModelRef,
        variant: Option<&'a str>,
        note: &'a str,
    ) -> BoxFuture<'a, Result<PromptOutcome, ToolError>> {
        self.prompt(session_id, agent, model, variant, note)
    }
    /// Cancel a running subagent.
    fn cancel<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ()>;
    /// How many cancels the session has been asked for.
    fn cancel_count(&self, _session_id: &str) -> u64 {
        0
    }
    /// Where one session stands; `None` when it does not exist.
    fn inspect<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Option<SubagentState>> {
        Box::pin(async { None })
    }
    /// The direct children of a session, oldest first.
    fn children<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Vec<SubagentState>> {
        Box::pin(async { Vec::new() })
    }
    /// `injectBackgroundResult` (task.ts:236-252) — a synthetic text prompt
    /// into the parent session (parent's agent, captured variant),
    /// fire-and-forget (`Effect.ignore`).
    fn inject_background_result<'a>(
        &'a self,
        parent_session_id: &'a str,
        variant: Option<&'a str>,
        text: &'a str,
    ) -> BoxFuture<'a, ()>;
}

/// `deriveSubagentSessionPermission` (subagent-permissions.ts:18-31):
/// parent external_directory + deny rules, then default `todowrite`/`task`
/// denies when the subagent's own ruleset doesn't already permit them.
/// `primary_tools` adds one deny rule per experimental tool (task.ts:143-155).
pub fn derive_subagent_session_permission(
    parent_session_permission: &[Rule],
    subagent_permission: &[Rule],
    primary_tools: &[String],
) -> Vec<Rule> {
    let can_task = subagent_permission
        .iter()
        .any(|rule| rule.permission == "task");
    let can_todo = subagent_permission
        .iter()
        .any(|rule| rule.permission == "todowrite");
    let mut result: Vec<Rule> = parent_session_permission
        .iter()
        .filter(|rule| rule.permission == "external_directory" || rule.action == "deny")
        .cloned()
        .collect();
    if !can_todo {
        result.push(Rule {
            permission: "todowrite".to_string(),
            pattern: "*".to_string(),
            action: "deny",
        });
    }
    if !can_task {
        result.push(Rule {
            permission: "task".to_string(),
            pattern: "*".to_string(),
            action: "deny",
        });
        // Nothing to manage without the means to start subagents.
        result.push(Rule {
            permission: crate::tool::subagents::TOOL_ID.to_string(),
            pattern: "*".to_string(),
            action: "deny",
        });
    }
    let primary_tools: Vec<Rule> = primary_tools
        .iter()
        .map(|permission| Rule {
            permission: permission.clone(),
            pattern: "*".to_string(),
            action: "deny",
        })
        .collect();
    // The child-tool denies merge into the derived ruleset deduplicated
    // (task.ts:151-160).
    for deny in primary_tools {
        if !result
            .iter()
            .any(|rule| rule.permission == deny.permission && rule.pattern == deny.pattern)
        {
            result.push(deny);
        }
    }
    result
}

/// [`TaskOps::prompt`], resuming the subagent while it stops on recoverable
/// errors, so neither the user nor the parent has to nudge it. Returns how
/// many resumes it took.
async fn prompt_recovering(
    ops: &Arc<dyn TaskOps>,
    session_id: &str,
    agent: &str,
    model: &ModelRef,
    variant: Option<&str>,
    prompt: &str,
) -> Result<(PromptOutcome, usize), ToolError> {
    let mut cancels = ops.cancel_count(session_id);
    let mut outcome = ops
        .prompt(session_id, agent, model, variant, prompt)
        .await?;
    let mut resumes = 0;
    for delay in AUTO_RESUME_DELAYS {
        let Some(error) = outcome.error.clone().filter(|_| outcome.recoverable) else {
            break;
        };
        // The session is idle now, so a stop has nothing to interrupt: it only
        // shows here.
        if !wait_to_resume(delay, || ops.cancel_count(session_id) != cancels).await {
            outcome.error = Some(format!(
                "Stopped while waiting to resume after an error: {error}"
            ));
            outcome.recoverable = false;
            break;
        }
        cancels = ops.cancel_count(session_id);
        outcome = ops
            .resume(session_id, agent, model, variant, &resume_note(&error))
            .await?;
        resumes += 1;
    }
    Ok((outcome, resumes))
}

/// What the parent needs to act on a failure itself: what went wrong, what the
/// subagent got done, and how to carry on. The first line is `task.ts`'s.
async fn failure_message(
    ops: &Arc<dyn TaskOps>,
    session_id: &str,
    error: &str,
    resumes: usize,
) -> String {
    let mut message = format!("Subagent failed (task_id: {session_id}): {error}");
    if resumes > 0 {
        message.push_str(&format!(
            "\n\nIt was resumed automatically {} and kept failing.",
            if resumes == 1 {
                "once".to_string()
            } else {
                format!("{resumes} times")
            }
        ));
    }
    let state = ops.inspect(session_id).await;
    if let Some(text) = state.as_ref().and_then(|state| state.last_text.as_deref()) {
        let skip = text.chars().count().saturating_sub(FAILURE_OUTPUT_CHARS);
        let tail: String = text.chars().skip(skip).collect();
        message.push_str(&format!("\n\nIts last output:\n{tail}"));
    }
    message.push_str(&format!(
        "\n\nIts session is intact. To continue it, call task again with task_id \"{session_id}\" and a prompt saying how to proceed. Do not start over unless its work is lost or wrong."
    ));
    message
}

fn recovery_summary(resumes: usize) -> Option<String> {
    (resumes > 0).then(|| {
        let plural = if resumes == 1 { "" } else { "s" };
        format!("Recovered from errors after {resumes} automatic resume{plural}")
    })
}

#[derive(Debug, Deserialize)]
pub struct TaskParameters {
    pub description: String,
    pub prompt: String,
    pub subagent_type: String,
    pub task_id: Option<String>,
    #[serde(default)]
    pub background: Option<bool>,
}

/// Hand-authored JSON Schema (byte-matches the Effect-generated schema; see
/// `tests/golden/toolschema/task.json`).
pub fn parameters(background: bool) -> Value {
    let mut properties = json!({
        "description": {
            "type": "string",
            "description": "A short (3-5 words) description of the task"
        },
        "prompt": {
            "type": "string",
            "description": "The task for the agent to perform"
        },
        "subagent_type": {
            "type": "string",
            "description": "The type of specialized agent to use for this task"
        },
        "task_id": {
            "type": "string",
            "description": "This should only be set if you mean to resume a previous task (you can pass a prior task_id and the task will continue the same subagent session as before instead of creating a fresh one)"
        },
        "command": {
            "type": "string",
            "description": "The command that triggered this task"
        }
    });
    if background {
        if let Some(object) = properties.as_object_mut() {
            object.insert(
                "background".to_string(),
                json!({
                    "type": "boolean",
                    "description": "Run the agent in the background. You will be notified when it completes. DO NOT sleep, poll, or proactively check on its progress"
                }),
            );
        }
    }
    let mut schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": properties,
    });
    if let Some(object) = schema.as_object_mut() {
        // `BaseParameterFields` keeps command/task_id/background optional
        // in both shapes (task.ts:47-56).
        let required = [
            "description".to_string(),
            "prompt".to_string(),
            "subagent_type".to_string(),
        ];
        object.insert(
            "required".to_string(),
            Value::Array(required.into_iter().map(Value::String).collect()),
        );
    }
    schema
}

/// Build the `task` tool. `subagent_depth` is `cfg.subagent_depth ?? 1`;
/// `primary_tools` is `cfg.experimental.primary_tools`.
pub fn task_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    ops: Arc<dyn TaskOps>,
    subagent_depth: usize,
    primary_tools: Vec<String>,
    background: BackgroundMode,
    jobs: Option<Arc<crate::session::background::BackgroundJobService>>,
) -> ToolDef {
    let description = match background {
        BackgroundMode::Enabled => {
            format!(
                "{}\n\n{BACKGROUND_DESCRIPTION}",
                include_str!("txt/task.txt")
            )
        }
        BackgroundMode::Disabled => include_str!("txt/task.txt").to_string(),
    };
    define(
        "task",
        description,
        parameters(matches!(background, BackgroundMode::Enabled)),
        None,
        truncate,
        agents,
        move |params: TaskParameters, ctx: ToolCtxRef<'_>| {
            let ops = ops.clone();
            let primary_tools = primary_tools.clone();
            let jobs = jobs.clone();
            Box::pin(async move {
                run(
                    params,
                    ctx,
                    ops,
                    subagent_depth,
                    primary_tools,
                    &background,
                    jobs,
                )
                .await
            })
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundMode {
    Enabled,
    Disabled,
}

async fn run(
    params: TaskParameters,
    ctx: ToolCtxRef<'_>,
    ops: Arc<dyn TaskOps>,
    subagent_depth: usize,
    primary_tools: Vec<String>,
    background: &BackgroundMode,
    jobs: Option<Arc<crate::session::background::BackgroundJobService>>,
) -> Result<ExecuteResult, ToolError> {
    let run_in_background = params.background == Some(true);
    if run_in_background && *background == BackgroundMode::Disabled {
        return Err(ToolError::Failed(
            "Background subagents require OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS=true"
                .to_string(),
        ));
    }

    let depth = ops.depth(ctx.session_id).await?;
    if depth >= subagent_depth {
        return Err(ToolError::Failed(format!(
            "Subagent depth limit reached ({subagent_depth}). Increase \"subagent_depth\" to allow nested subagents."
        )));
    }

    if !ctx.extra.bypass_agent_check {
        ctx.ask
            .ask(AskRequest {
                permission: "task".to_string(),
                patterns: vec![params.subagent_type.clone()],
                always: vec!["*".to_string()],
                metadata: json!({
                    "description": params.description,
                    "subagent_type": params.subagent_type,
                }),
            })
            .await?;
    }

    let Some(next) = ops.agent(&params.subagent_type).await? else {
        return Err(ToolError::Failed(format!(
            "Unknown agent type: {} is not a valid agent type",
            params.subagent_type
        )));
    };

    let existing_session = match &params.task_id {
        Some(task_id) if ops.session_exists(task_id).await => Some(task_id.clone()),
        _ => None,
    };
    let session_id = match existing_session {
        Some(session) => session,
        None => {
            let parent_permission = ops.session_permission(ctx.session_id).await;
            let child_permission = derive_subagent_session_permission(
                &parent_permission,
                &next.permission,
                &primary_tools,
            );
            ops.create_session(
                ctx.session_id,
                &format!("{} (@{} subagent)", params.description, next.name),
                &next.name,
                child_permission,
            )
            .await?
        }
    };

    let parent = ops.parent_message(ctx.session_id, ctx.message_id).await?;
    let variant = parent.variant;
    let parent_model = parent
        .model
        .ok_or_else(|| ToolError::Failed("Not an assistant message".to_string()))?;

    let model = next.model.clone().unwrap_or(parent_model);

    let metadata = json!({
        "parentSessionId": ctx.session_id,
        "sessionId": session_id,
        "model": {
            "modelID": model.model_id,
            "providerID": model.provider_id,
        },
    });
    ctx.metadata
        .metadata(crate::tool::def::MetadataInput {
            title: Some(params.description.clone()),
            metadata: Some(metadata.clone()),
        })
        .await?;

    // Background subagents (task.ts:205-358). Without the experimental
    // flag the plain foreground path below runs directly.
    if let Some(jobs) = jobs {
        use crate::session::background::{
            BackgroundJobService, ExtendInput, StartInput, Status, WaitInput,
        };

        // `runTask` (task.ts:203-233) — the job body.
        let make_run_task = || {
            let ops = ops.clone();
            let session_id = session_id.clone();
            let agent = next.name.clone();
            let model = model.clone();
            let variant = variant.clone();
            let prompt = params.prompt.clone();
            Box::pin(async move {
                let (outcome, resumes) = match prompt_recovering(
                    &ops,
                    &session_id,
                    &agent,
                    &model,
                    variant.as_deref(),
                    &prompt,
                )
                .await
                {
                    Ok(outcome) => outcome,
                    Err(error) => return Err(error.to_string()),
                };
                if let Some(error) = outcome.error {
                    return Err(failure_message(&ops, &session_id, &error, resumes).await);
                }
                Ok(outcome.text)
            }) as crate::session::background::JobFuture
        };

        // `backgroundResult` (task.ts:280-296).
        let background_result = |job_id: &str| ExecuteResult {
            title: params.description.clone(),
            metadata: json!({
                "parentSessionId": ctx.session_id,
                "sessionId": session_id,
                "model": {
                    "modelID": model.model_id,
                    "providerID": model.provider_id,
                },
                "background": true,
                "jobId": job_id,
            }),
            output: render_output(RenderOutput {
                session_id: &session_id,
                state: "running",
                summary: Some("Background task started"),
                text: BACKGROUND_STARTED,
            }),
            attachments: None,
        };

        // `notify` (task.ts:252-261) — forked: wait for the job, inject
        // the rendered result into the parent session.
        let notify = {
            let jobs = jobs.clone();
            let ops = ops.clone();
            let parent_session = ctx.session_id.to_string();
            let variant = variant.clone();
            let description = params.description.clone();
            let session_id = session_id.clone();
            move |job_id: String| {
                let jobs = jobs.clone();
                let ops = ops.clone();
                let parent_session = parent_session.clone();
                let variant = variant.clone();
                let description = description.clone();
                let session_id = session_id.clone();
                tokio::spawn(async move {
                    let jobs: Arc<BackgroundJobService> = jobs;
                    let result = jobs
                        .wait(WaitInput {
                            id: job_id,
                            timeout: None,
                        })
                        .await;
                    let Some(info) = result.info else {
                        return;
                    };
                    let (state, summary, text) = match info.status {
                        Status::Completed => (
                            "completed",
                            format!("Background task completed: {description}"),
                            info.output.unwrap_or_default(),
                        ),
                        Status::Error => (
                            "error",
                            format!("Background task failed: {description}"),
                            info.error.unwrap_or_default(),
                        ),
                        _ => return,
                    };
                    ops.inject_background_result(
                        &parent_session,
                        variant.as_deref(),
                        &render_output(RenderOutput {
                            session_id: &session_id,
                            state,
                            summary: Some(&summary),
                            text: &untrusted_output(&text),
                        }),
                    )
                    .await;
                });
            }
        };

        // `background.extend` (task.ts:261-270) — a follow-up for an
        // already-running task appends to the job.
        if jobs
            .extend(ExtendInput {
                id: session_id.clone(),
                run: make_run_task(),
            })
            .unwrap_or(false)
        {
            return Ok(ExecuteResult {
                title: params.description.clone(),
                metadata: json!({
                    "parentSessionId": ctx.session_id,
                    "sessionId": session_id,
                    "model": {
                        "modelID": model.model_id,
                        "providerID": model.provider_id,
                    },
                    "background": true,
                    "jobId": session_id,
                }),
                output: render_output(RenderOutput {
                    session_id: &session_id,
                    state: "running",
                    summary: Some("Background task updated"),
                    text: BACKGROUND_UPDATED,
                }),
                attachments: None,
            });
        }

        // `background.start` (task.ts:272-279) — the onPromote metadata
        // update needs the call-scoped metadata sink, so only the
        // notification half runs on promotion.
        let on_promote = {
            let notify = notify.clone();
            let session_id = session_id.clone();
            Box::pin(async move {
                let notify = notify;
                notify(session_id);
            }) as crate::tool::def::BoxFuture<'static, ()>
        };
        let jobs_ref = jobs.clone();
        let run_task = {
            let ops = ops.clone();
            let session_id = session_id.clone();
            move || {
                let ops = ops.clone();
                let session_id = session_id.clone();
                let run = make_run_task();
                Box::pin(async move {
                    // `Effect.onInterrupt(ops.cancel)` (task.ts:277-279) —
                    // the job runner aborts this task on cancel, so the
                    // Drop guard fires the session cancel exactly when the
                    // run is interrupted.
                    struct OnInterrupt<S> {
                        ops: Arc<dyn TaskOps>,
                        session_id: String,
                        done: bool,
                        _run: S,
                    }
                    impl<S> Drop for OnInterrupt<S> {
                        fn drop(&mut self) {
                            if self.done {
                                return;
                            }
                            let ops = self.ops.clone();
                            let session_id = self.session_id.clone();
                            tokio::spawn(async move {
                                ops.cancel(&session_id).await;
                            });
                        }
                    }
                    let mut guard = OnInterrupt {
                        ops,
                        session_id,
                        done: false,
                        _run: std::pin::pin!(run),
                    };
                    let outcome = (&mut guard._run).await;
                    guard.done = true;
                    outcome
                }) as crate::session::background::JobFuture
            }
        };
        let info = jobs_ref.start(StartInput {
            id: Some(session_id.clone()),
            r#type: "task".to_string(),
            title: Some(params.description.clone()),
            metadata: metadata.as_object().cloned(),
            run: run_task(),
            on_promote: Some(on_promote),
        });

        // `runInBackground` (task.ts:298-301).
        if run_in_background {
            if let Ok(info) = &info {
                notify(info.id.clone());
                return Ok(background_result(&info.id));
            }
            return Err(ToolError::Failed(
                "Background subagents require OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS=true"
                    .to_string(),
            ));
        }

        // Foreground (task.ts:303-358) — race the wait against promotion.
        if let Ok(started) = info {
            {
                let jobs = jobs.clone();
                let job_id = started.id.clone();
                let outcome = tokio::select! {
                    outcome = async {
                        jobs.wait(WaitInput { id: job_id.clone(), timeout: None }).await
                    } => outcome,
                    _ = ctx.abort.cancelled() => {
                        // `Effect.all([cancel, background.cancel])` on
                        // interrupt (task.ts:344-353).
                        let ops = ops.clone();
                        let session_id = session_id.clone();
                        let jobs = jobs.clone();
                        let job_id = started.id.clone();
                        tokio::spawn(async move {
                            ops.cancel(&session_id).await;
                            jobs.cancel(&job_id);
                        });
                        return Err(ToolError::Aborted);
                    }
                };
                if let Some(info) = outcome.info {
                    if info
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get("background"))
                        == Some(&serde_json::Value::Bool(true))
                    {
                        return Ok(background_result(&info.id));
                    }
                    match info.status {
                        Status::Error => {
                            return Err(ToolError::Failed(
                                info.error.unwrap_or_else(|| "Task failed".to_string()),
                            ));
                        }
                        Status::Cancelled => {
                            return Err(ToolError::Failed("Task cancelled".to_string()));
                        }
                        _ => {
                            return Ok(ExecuteResult {
                                title: params.description.clone(),
                                metadata,
                                output: render_output(RenderOutput {
                                    session_id: &session_id,
                                    state: "completed",
                                    summary: None,
                                    text: &info.output.unwrap_or_default(),
                                }),
                                attachments: None,
                            });
                        }
                    }
                }
            }
        }
        return Err(ToolError::Failed(
            "Background subagents require OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS=true"
                .to_string(),
        ));
    }

    // `Effect.acquireUseRelease` + the abort listener (task.ts:320-358):
    // an aborted parent turn cancels the subagent instead of waiting it
    // out.
    let (outcome, resumes) = tokio::select! {
        outcome = prompt_recovering(
            &ops,
            &session_id,
            &next.name,
            &model,
            variant.as_deref(),
            &params.prompt,
        ) => outcome?,
        _ = ctx.abort.cancelled() => {
            let session_id = session_id.clone();
            let ops = ops.clone();
            tokio::spawn(async move {
                ops.cancel(&session_id).await;
            });
            return Err(ToolError::Aborted);
        }
    };

    if let Some(error) = outcome.error {
        return Err(ToolError::Failed(
            failure_message(&ops, &session_id, &error, resumes).await,
        ));
    }

    let summary = recovery_summary(resumes);
    Ok(ExecuteResult {
        title: params.description.clone(),
        metadata,
        output: render_output(RenderOutput {
            session_id: &session_id,
            state: "completed",
            summary: summary.as_deref(),
            text: &outcome.text,
        }),
        attachments: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::def::Extra;
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;
    use serde_json::json;

    struct FakeOps {
        depth: usize,
        agent: Option<SubagentInfo>,
        /// Served in order; then the default success.
        outcomes: std::sync::Mutex<Vec<PromptOutcome>>,
        prompts: std::sync::Mutex<Vec<String>>,
        last_text: Option<String>,
        /// Whether a stop arrives once the first prompt has ended.
        stopped_after_first_prompt: bool,
    }

    impl FakeOps {
        fn new() -> Self {
            FakeOps {
                depth: 0,
                agent: Some(SubagentInfo {
                    name: "research".to_string(),
                    model: Some(ModelRef {
                        model_id: "m".to_string(),
                        provider_id: "p".to_string(),
                    }),
                    permission: vec![Rule {
                        permission: "read".to_string(),
                        pattern: "*".to_string(),
                        action: "allow",
                    }],
                }),
                outcomes: Default::default(),
                prompts: Default::default(),
                last_text: None,
                stopped_after_first_prompt: false,
            }
        }

        fn failing(errors: &[(&str, bool)]) -> Self {
            let outcomes = errors
                .iter()
                .rev()
                .map(|(error, recoverable)| PromptOutcome {
                    text: String::new(),
                    error: Some(error.to_string()),
                    recoverable: *recoverable,
                })
                .collect();
            FakeOps {
                outcomes: std::sync::Mutex::new(outcomes),
                ..FakeOps::new()
            }
        }
    }

    impl TaskOps for FakeOps {
        fn depth<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Result<usize, ToolError>> {
            Box::pin(async { Ok(self.depth) })
        }
        fn agent<'a>(
            &'a self,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<Option<SubagentInfo>, ToolError>> {
            Box::pin(async { Ok(self.agent.clone()) })
        }
        fn session_exists<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, bool> {
            Box::pin(async { false })
        }
        fn create_session<'a>(
            &'a self,
            _parent_id: &'a str,
            _title: &'a str,
            _agent: &'a str,
            _permission: Vec<Rule>,
        ) -> BoxFuture<'a, Result<String, ToolError>> {
            Box::pin(async { Ok("ses_child".to_string()) })
        }
        fn inject_background_result<'a>(
            &'a self,
            _parent_session_id: &'a str,
            _variant: Option<&'a str>,
            _text: &'a str,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
        fn parent_message<'a>(
            &'a self,
            _session_id: &'a str,
            _message_id: &'a str,
        ) -> BoxFuture<'a, Result<ParentMessage, ToolError>> {
            Box::pin(async {
                Ok(ParentMessage {
                    variant: Some("default".to_string()),
                    model: Some(ModelRef {
                        model_id: "parent-model".to_string(),
                        provider_id: "parent-provider".to_string(),
                    }),
                })
            })
        }

        fn prompt<'a>(
            &'a self,
            _session_id: &'a str,
            _agent: &'a str,
            _model: &'a ModelRef,
            _variant: Option<&'a str>,
            prompt: &'a str,
        ) -> BoxFuture<'a, Result<PromptOutcome, ToolError>> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            let next = self.outcomes.lock().unwrap().pop();
            Box::pin(async move {
                Ok(next.unwrap_or_else(|| PromptOutcome {
                    recoverable: false,
                    text: "did the thing".to_string(),
                    error: None,
                }))
            })
        }
        fn cancel<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
        fn cancel_count(&self, _session_id: &str) -> u64 {
            u64::from(self.stopped_after_first_prompt && !self.prompts.lock().unwrap().is_empty())
        }
        fn inspect<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Option<SubagentState>> {
            let state = SubagentState {
                session_id: session_id.to_string(),
                parent_id: None,
                title: "t".to_string(),
                agent: None,
                status: SubagentStatus::Failed,
                error: None,
                last_text: self.last_text.clone(),
                tool_calls: 0,
            };
            Box::pin(async move { Some(state) })
        }
    }

    fn tool_with(ops: Arc<dyn TaskOps>, depth: usize) -> ToolDef {
        task_tool(
            Arc::new(TruncateService::default_limits(std::path::PathBuf::from(
                "/tmp/opencode",
            ))),
            fixed_agents(),
            ops,
            depth,
            Vec::new(),
            BackgroundMode::Disabled,
            None,
        )
    }

    async fn call(
        ops: Arc<dyn TaskOps>,
        depth: usize,
        args: Value,
    ) -> (
        Result<ExecuteResult, ToolError>,
        Vec<crate::tool::def::AskRequest>,
    ) {
        let def = tool_with(ops, depth);
        let ask = RecordingAsk::new();
        let inst = instance(std::path::Path::new("/tmp/opencode"));
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(args, ctx).await;
        (result, ask.requests())
    }

    async fn call_background(
        ops: Arc<dyn TaskOps>,
        jobs: Arc<crate::session::background::BackgroundJobService>,
        args: Value,
    ) -> Result<ExecuteResult, ToolError> {
        let def = task_tool(
            Arc::new(TruncateService::default_limits(std::path::PathBuf::from(
                "/tmp/opencode",
            ))),
            fixed_agents(),
            ops,
            1,
            Vec::new(),
            BackgroundMode::Enabled,
            Some(jobs),
        );
        let ask = RecordingAsk::new();
        let inst = instance(std::path::Path::new("/tmp/opencode"));
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        (def.execute)(args, ctx).await
    }

    #[tokio::test]
    async fn background_true_returns_background_started() {
        let jobs = crate::session::background::BackgroundJobService::new(Arc::new(
            crate::catalog::SystemClock,
        ));
        let result = call_background(
            Arc::new(FakeOps::new()),
            jobs.clone(),
            json!({
                "description": "bg task",
                "prompt": "run",
                "subagent_type": "research",
                "background": true
            }),
        )
        .await
        .unwrap();
        assert_eq!(result.title, "bg task");
        assert_eq!(result.metadata["background"], json!(true));
        assert_eq!(result.metadata["jobId"], json!("ses_child"));
        assert!(result.output.contains("Background task started"));
        assert!(result.output.contains(BACKGROUND_STARTED));
        // The notify waiter shouldn't deadlock the test: let the job run.
        for _ in 0..50 {
            if jobs
                .list()
                .iter()
                .all(|job| job.status != crate::session::background::Status::Running)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn foreground_runs_through_the_job_service() {
        let jobs = crate::session::background::BackgroundJobService::new(Arc::new(
            crate::catalog::SystemClock,
        ));
        let result = call_background(
            Arc::new(FakeOps::new()),
            jobs,
            json!({
                "description": "fg task",
                "prompt": "run",
                "subagent_type": "research"
            }),
        )
        .await
        .unwrap();
        assert!(result
            .output
            .contains("<task id=\"ses_child\" state=\"completed\">"));
        assert!(result.output.contains("did the thing"));
        assert_eq!(result.metadata["background"], serde_json::Value::Null);
    }

    #[test]
    fn render_output_shapes() {
        assert_eq!(
            render_output(RenderOutput {
                session_id: "ses_1",
                state: "completed",
                summary: None,
                text: "hi"
            }),
            "<task id=\"ses_1\" state=\"completed\">\n<task_result>\nhi\n</task_result>\n</task>"
        );
        assert_eq!(
            render_output(RenderOutput {
                session_id: "ses_1",
                state: "error",
                summary: Some("boom"),
                text: "bad"
            }),
            "<task id=\"ses_1\" state=\"error\">\n<summary>boom</summary>\n<task_error>\nbad\n</task_error>\n</task>"
        );
    }

    #[test]
    fn deny_rules_derivation() {
        // No explicit permits → both denies appended.
        assert_eq!(
            derive_subagent_session_permission(&[], &[], &[]),
            vec![
                Rule {
                    permission: "todowrite".to_string(),
                    pattern: "*".to_string(),
                    action: "deny",
                },
                Rule {
                    permission: "task".to_string(),
                    pattern: "*".to_string(),
                    action: "deny",
                },
                Rule {
                    permission: "subagents".to_string(),
                    pattern: "*".to_string(),
                    action: "deny",
                },
            ]
        );
        // Explicit permits → no default denies.
        assert!(
            derive_subagent_session_permission(
                &[],
                &[Rule {
                    permission: "task".to_string(),
                    pattern: "*".to_string(),
                    action: "allow",
                }],
                &[]
            )
            .len()
                == 1
        );
    }

    fn job() -> Value {
        json!({ "description": "d", "prompt": "the job", "subagent_type": "research" })
    }

    fn failure_text(error: ToolError) -> String {
        match error {
            ToolError::Failed(text) => text,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_recoverable_error_is_resumed_without_anyone_asking() {
        let ops = Arc::new(FakeOps::failing(&[("boom", true)]));
        let result = call(ops.clone(), 1, job()).await.0.unwrap();
        assert!(
            result.output.contains("state=\"completed\""),
            "{}",
            result.output
        );
        assert!(result.output.contains("did the thing"), "{}", result.output);
        assert!(
            result.output.contains("Recovered from errors after 1"),
            "{}",
            result.output
        );
        let prompts = ops.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0], "the job");
        assert!(
            prompts[1].contains("boom") && prompts[1].contains("Continue"),
            "{prompts:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn resumes_are_capped_and_the_parent_gets_the_means_to_continue() {
        let ops = Arc::new(FakeOps {
            last_text: Some("found half of it".to_string()),
            ..FakeOps::failing(&[
                ("boom", true),
                ("boom", true),
                ("boom again", true),
                ("x", true),
            ])
        });
        let text = failure_text(call(ops.clone(), 1, job()).await.0.unwrap_err());
        assert_eq!(
            ops.prompts.lock().unwrap().len(),
            AUTO_RESUME_DELAYS.len() + 1
        );
        assert!(
            text.starts_with("Subagent failed (task_id: ses_child): boom again"),
            "{text}"
        );
        assert!(text.contains("resumed automatically 2 times"), "{text}");
        assert!(text.contains("found half of it"), "{text}");
        assert!(text.contains("task_id \"ses_child\""), "{text}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_during_the_resume_wait_is_honoured() {
        let ops = Arc::new(FakeOps {
            stopped_after_first_prompt: true,
            ..FakeOps::failing(&[("boom", true)])
        });
        let text = failure_text(call(ops.clone(), 1, job()).await.0.unwrap_err());
        assert_eq!(ops.prompts.lock().unwrap().len(), 1, "not resumed");
        assert!(text.contains("Stopped while waiting to resume"), "{text}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unrecoverable_error_is_not_resumed() {
        let ops = Arc::new(FakeOps::failing(&[("bad credentials", false)]));
        let text = failure_text(call(ops.clone(), 1, job()).await.0.unwrap_err());
        assert_eq!(ops.prompts.lock().unwrap().len(), 1);
        assert!(!text.contains("resumed automatically"), "{text}");
        assert!(text.contains("task_id \"ses_child\""), "{text}");
    }

    #[tokio::test]
    async fn foreground_success() {
        let (result, asks) = call(
            Arc::new(FakeOps::new()),
            1,
            json!({
                "description": "research stuff",
                "prompt": "go research",
                "subagent_type": "research"
            }),
        )
        .await;
        let result = result.unwrap();
        assert_eq!(result.title, "research stuff");
        assert!(result
            .output
            .contains("<task id=\"ses_child\" state=\"completed\">"));
        assert!(result.output.contains("did the thing"));
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].permission, "task");
        assert_eq!(asks[0].patterns, vec!["research".to_string()]);
    }

    #[tokio::test]
    async fn background_requires_flag() {
        let (result, _) = call(
            Arc::new(FakeOps::new()),
            1,
            json!({
                "description": "bg task",
                "prompt": "run",
                "subagent_type": "research",
                "background": true
            }),
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "Background subagents require OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS=true"
        );
    }

    #[tokio::test]
    async fn depth_limit_enforced() {
        let (result, _) = call(
            Arc::new(FakeOps {
                depth: 1,
                agent: None,
                ..FakeOps::new()
            }),
            1,
            json!({
                "description": "nested",
                "prompt": "go",
                "subagent_type": "research"
            }),
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "Subagent depth limit reached (1). Increase \"subagent_depth\" to allow nested subagents."
        );
    }

    #[tokio::test]
    async fn unknown_agent_is_an_error() {
        let (result, _) = call(
            Arc::new(FakeOps {
                depth: 0,
                agent: None,
                ..FakeOps::new()
            }),
            1,
            json!({
                "description": "d",
                "prompt": "p",
                "subagent_type": "ghost"
            }),
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "Unknown agent type: ghost is not a valid agent type"
        );
    }

    #[tokio::test]
    async fn bypass_agent_check_skips_ask() {
        let def = tool_with(Arc::new(FakeOps::new()), 1);
        let ask = RecordingAsk::new();
        let inst = instance(std::path::Path::new("/tmp/opencode"));
        let extra = Extra {
            bypass_agent_check: true,
            ..Extra::default()
        };
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(
            json!({
                "description": "d",
                "prompt": "p",
                "subagent_type": "research"
            }),
            ctx,
        )
        .await;
        result.unwrap();
        assert!(ask.requests().is_empty());
    }
}
