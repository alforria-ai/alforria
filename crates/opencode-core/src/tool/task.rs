//! `task` tool — port of `tool/task.ts` (spec M4.8).
//!
//! Session machinery (session walking, message variants, background jobs)
//! lives behind the [`TaskOps`] seam; M5 provides the production impl.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::tool::def::{define, Agents, AskRequest, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::truncate::Truncate;

pub const BACKGROUND_DESCRIPTION: &str = "Background mode: background=true launches the subagent asynchronously and returns immediately. Foreground is the default; use it when you need the result before continuing. Use background only for independent work that can run while you continue elsewhere. You will be notified automatically when it finishes.";

pub const BACKGROUND_STARTED: &str = "The task is working in the background. You will be notified automatically when it finishes.\nDO NOT sleep, poll for progress, ask the task for status, or duplicate this task's work — avoid working with the same files or topics it is using.\nWork on non-overlapping tasks, or briefly tell the user what you launched and end your response.";

pub const BACKGROUND_UPDATED: &str = "Additional context sent to the running background task.\nThe task is still working in the background. You will be notified automatically when it finishes.\nDO NOT sleep, poll for progress, ask the task for status, or duplicate this task's work — avoid working with the same files or topics it is using.\nWork on non-overlapping tasks, or briefly tell the user what you sent and end your response.";

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
    /// Cancel a running subagent.
    fn cancel<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ()>;
}

/// `deriveSubagentSessionPermission` (subagent-permissions.ts:18-31):
/// parent external_directory + deny rules, then default `todowrite`/`task`
/// denies when the subagent's own ruleset doesn't already permit them.
pub fn derive_subagent_session_permission(
    parent_session_permission: &[Rule],
    subagent_permission: &[Rule],
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
    }
    result
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
        let mut required = vec![
            "description".to_string(),
            "prompt".to_string(),
            "subagent_type".to_string(),
        ];
        if !background {
            required.push("command".to_string());
        }
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
            Box::pin(async move {
                run(params, ctx, ops, subagent_depth, primary_tools, &background).await
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
    _primary_tools: Vec<String>,
    background: &BackgroundMode,
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
            let child_permission =
                derive_subagent_session_permission(&parent_permission, &next.permission);
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

    let outcome = ops
        .prompt(
            &session_id,
            &next.name,
            &model,
            variant.as_deref(),
            &params.prompt,
        )
        .await?;

    if let Some(error) = outcome.error {
        return Err(ToolError::Failed(format!(
            "Subagent failed (task_id: {session_id}): {error}"
        )));
    }

    Ok(ExecuteResult {
        title: params.description.clone(),
        metadata,
        output: render_output(RenderOutput {
            session_id: &session_id,
            state: "completed",
            summary: None,
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
            _prompt: &'a str,
        ) -> BoxFuture<'a, Result<PromptOutcome, ToolError>> {
            Box::pin(async {
                Ok(PromptOutcome {
                    text: "did the thing".to_string(),
                    error: None,
                })
            })
        }
        fn cancel<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async {})
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
            derive_subagent_session_permission(&[], &[]),
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
                }]
            )
            .len()
                == 1
        );
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
