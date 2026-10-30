//! `workflow` tool — start and inspect dynamic workflows from a session.
//!
//! The runtime, persistence, and the web progress view live in the workflow
//! service; this tool is the model's entry point. An agent can run a saved
//! workflow by name or an inline script, then check it with `status`.

use std::sync::Arc;

use alforria_schema::workflow::{WorkflowDefinition, WorkflowRun, WorkflowStartRequest};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::tool::def::{define, Agents, AskRequest, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::truncate::Truncate;

/// The workflow-service seam the tool drives.
pub trait Workflows: Send + Sync {
    fn list<'a>(&'a self) -> BoxFuture<'a, Result<Vec<WorkflowDefinition>, ToolError>>;
    fn start<'a>(
        &'a self,
        session_id: &'a str,
        request: WorkflowStartRequest,
    ) -> BoxFuture<'a, Result<WorkflowRun, ToolError>>;
    fn get<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, Result<Option<WorkflowRun>, ToolError>>;
    /// The newest run in a session, for `status` without a run id.
    fn latest_for_session<'a>(
        &'a self,
        session_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<WorkflowRun>, ToolError>>;
    fn wait<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, Result<WorkflowRun, ToolError>>;
    fn abort<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, Result<WorkflowRun, ToolError>>;
}

#[derive(Debug, Deserialize)]
pub struct WorkflowParameters {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    args: Option<Value>,
    #[serde(default, rename = "runID")]
    run_id: Option<String>,
    #[serde(default)]
    wait: Option<bool>,
}

const DESCRIPTION: &str = r#"Run and inspect dynamic workflows: saved JavaScript orchestrations that fan work out to many subagents and return one result.

Use a workflow when the user asks for one (for example "use a workflow" or includes the keyword `ultracode`), or when a task needs many independent agents coordinated from a repeatable script: a codebase-wide audit, a large migration, or cross-checked research. For a single delegated task, use `task` instead.

Saved workflows live in `.opencode/workflows` (project), the user config directory, and Claude Code-compatible `.claude/workflows` directories. A script uses `agent(prompt, options)`, `pipeline(items, fn)`, `parallel(tasks)`, `phase(title)`, `log(message)`, and the `args` global. `agent()` returns text, or parsed JSON when `options.schema` is set. It throws on failure; inside `pipeline`/`parallel` failures become `null`.

Actions:
- `list`: show saved workflows.
- `run`: start `name` (a saved workflow) or `script` (inline JavaScript), not both. Runs in the background by default; set `wait: true` to wait for the result. `args` is passed to the script.
- `status`: read a run by `runID` (omit `runID` for the most recent run in this session).
- `abort`: stop a run by `runID`.

A script may use up to 256 MB of memory and is stopped if it runs for 10 minutes without calling `agent()`; time spent waiting for agents does not count. Workflow agents cannot start workflows themselves.

After launching, tell the user what is running and end the turn. Do not poll in a loop; use `status` only when the user asks for progress or you need the result to continue."#;

fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["list", "run", "status", "abort"],
                "description": "The action to perform"
            },
            "name": {
                "type": "string",
                "description": "A saved workflow name (for run)"
            },
            "script": {
                "type": "string",
                "description": "An inline workflow script (for run when no saved name is used)"
            },
            "args": {
                "description": "Structured input passed to the workflow as the args global"
            },
            "runID": {
                "type": "string",
                "description": "The workflow run id (for status and abort)"
            },
            "wait": {
                "type": "boolean",
                "description": "Wait for the run to finish instead of returning immediately (default false)"
            }
        },
        "required": ["action"]
    })
}

pub fn workflow_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    workflows: Arc<dyn Workflows>,
) -> ToolDef {
    define(
        "workflow",
        DESCRIPTION,
        parameters(),
        None,
        truncate,
        agents,
        move |params: WorkflowParameters, ctx: ToolCtxRef<'_>| {
            run(params, ctx, Arc::clone(&workflows))
        },
    )
}

fn run(
    params: WorkflowParameters,
    ctx: ToolCtxRef<'_>,
    workflows: Arc<dyn Workflows>,
) -> BoxFuture<'_, Result<ExecuteResult, ToolError>> {
    Box::pin(async move {
        match params.action.as_str() {
            "list" => {
                let list = workflows.list().await?;
                let output = if list.is_empty() {
                    "No workflows are saved. Create one in .opencode/workflows or pass an inline script to run.".to_string()
                } else {
                    let mut lines = vec!["Saved workflows:".to_string()];
                    for workflow in &list {
                        let description = workflow
                            .description
                            .as_deref()
                            .map(|value| format!(" — {value}"))
                            .unwrap_or_default();
                        lines.push(format!(
                            "- {} ({}){}",
                            workflow.name,
                            source_label(workflow.source),
                            description
                        ));
                    }
                    lines.join("\n")
                };
                Ok(ExecuteResult {
                    title: "Workflows".to_string(),
                    metadata: json!({ "count": list.len() }),
                    output,
                    attachments: None,
                })
            }
            "run" => {
                // Approval is per name, so a name must never smuggle in a script.
                let (pattern, always, script) = match (&params.name, &params.script) {
                    (Some(_), Some(_)) => {
                        return Err(ToolError::InvalidArguments {
                            tool: "workflow".to_string(),
                            detail: "run takes a saved workflow name or an inline script, not both"
                                .to_string(),
                        });
                    }
                    (None, None) => {
                        return Err(ToolError::InvalidArguments {
                            tool: "workflow".to_string(),
                            detail: "run needs a saved workflow name or an inline script"
                                .to_string(),
                        });
                    }
                    (Some(name), None) => {
                        let found = workflows
                            .list()
                            .await?
                            .into_iter()
                            .find(|workflow| &workflow.name == name)
                            .ok_or_else(|| {
                                ToolError::Failed(format!("workflow not found: {name}"))
                            })?;
                        (name.clone(), vec![name.clone()], found.script)
                    }
                    // Each inline script is new code: one approval never covers the next.
                    (None, Some(script)) => ("inline".to_string(), Vec::new(), script.clone()),
                };
                ctx.ask
                    .ask(AskRequest {
                        permission: "workflow".to_string(),
                        patterns: vec![pattern.clone()],
                        always,
                        metadata: json!({ "name": pattern, "script": script }),
                    })
                    .await?;
                let request = WorkflowStartRequest {
                    name: params.name.clone(),
                    script: params.script.clone(),
                    description: None,
                    args: params.args.clone().unwrap_or(Value::Null),
                    session_id: Some(ctx.session_id.to_string()),
                };
                let mut run = workflows.start(ctx.session_id, request).await?;
                let waited = params.wait.unwrap_or(false);
                if waited {
                    run = tokio::select! {
                        finished = workflows.wait(&run.id) => finished?,
                        _ = ctx.abort.cancelled() => {
                            let _ = workflows.abort(&run.id).await;
                            return Err(ToolError::Aborted);
                        }
                    };
                }
                Ok(ExecuteResult {
                    title: format!("Workflow: {}", run.name),
                    metadata: json!({
                        "runID": run.id,
                        "state": run.state.as_str(),
                        "sessionID": run.session_id,
                        "waited": waited,
                    }),
                    output: render_run(&run, waited),
                    attachments: None,
                })
            }
            "status" => {
                let run = match params.run_id.as_deref() {
                    Some(run_id) => workflows.get(run_id).await?,
                    None => workflows.latest_for_session(ctx.session_id).await?,
                };
                let Some(run) = run else {
                    return Ok(ExecuteResult {
                        title: "Workflow".to_string(),
                        metadata: json!({ "found": false }),
                        output: "No workflow run found in this session.".to_string(),
                        attachments: None,
                    });
                };
                Ok(ExecuteResult {
                    title: format!("Workflow: {}", run.name),
                    metadata: json!({
                        "runID": run.id,
                        "state": run.state.as_str(),
                        "sessionID": run.session_id,
                    }),
                    output: render_run(&run, true),
                    attachments: None,
                })
            }
            "abort" => {
                let Some(run_id) = params.run_id.as_deref() else {
                    return Err(ToolError::InvalidArguments {
                        tool: "workflow".to_string(),
                        detail: "abort needs a runID".to_string(),
                    });
                };
                // A session stops its own runs freely; anyone else's needs approval.
                let own = workflows
                    .get(run_id)
                    .await?
                    .is_some_and(|run| run.session_id.as_deref() == Some(ctx.session_id));
                if !own {
                    ctx.ask
                        .ask(AskRequest {
                            permission: "workflow".to_string(),
                            patterns: vec!["abort".to_string()],
                            always: Vec::new(),
                            metadata: json!({ "name": "abort", "runID": run_id }),
                        })
                        .await?;
                }
                let run = workflows.abort(run_id).await?;
                Ok(ExecuteResult {
                    title: format!("Workflow: {}", run.name),
                    metadata: json!({
                        "runID": run.id,
                        "state": run.state.as_str(),
                    }),
                    output: format!("Workflow run {} is {}.", run.id, run.state.as_str()),
                    attachments: None,
                })
            }
            other => Err(ToolError::InvalidArguments {
                tool: "workflow".to_string(),
                detail: format!("unknown action: {other}"),
            }),
        }
    })
}

fn render_run(run: &WorkflowRun, detail: bool) -> String {
    let mut lines = vec![format!(
        "<workflow id=\"{}\" name=\"{}\" state=\"{}\">",
        run.id,
        run.name,
        run.state.as_str()
    )];
    if let Some(session) = &run.session_id {
        lines.push(format!("<session>{session}</session>"));
    }
    if let Some(error) = &run.error {
        lines.push(format!("<error>{error}</error>"));
    }
    if detail {
        for phase in &run.phases {
            lines.push(format!(
                "<phase name=\"{}\" state=\"{}\">",
                phase.title,
                phase.state.as_str()
            ));
            for agent in run.agents.iter().filter(|agent| agent.phase == phase.title) {
                lines.push(format!(
                    "<agent label=\"{}\" agent=\"{}\" state=\"{}\">",
                    agent.label,
                    agent.agent,
                    agent.state.as_str()
                ));
                if let Some(session) = &agent.session_id {
                    lines.push(format!("<session>{session}</session>"));
                }
                if let Some(result) = &agent.result {
                    lines.push(result.clone());
                }
                if let Some(error) = &agent.error {
                    lines.push(format!("<error>{error}</error>"));
                }
                lines.push("</agent>".to_string());
            }
            lines.push("</phase>".to_string());
        }
        if let Some(result) = &run.result {
            lines.push(format!(
                "<result>{}</result>",
                match result {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                }
            ));
        }
    } else {
        lines.push(format!(
            "Launched in the background with {} agents so far. Use action \"status\" with runID \"{}\" to check progress.",
            run.agents.len(),
            run.id
        ));
    }
    lines.push("</workflow>".to_string());
    lines.join("\n")
}

fn source_label(source: alforria_schema::workflow::WorkflowSource) -> &'static str {
    match source {
        alforria_schema::workflow::WorkflowSource::Project => "project",
        alforria_schema::workflow::WorkflowSource::Global => "global",
        alforria_schema::workflow::WorkflowSource::External => "claude",
        alforria_schema::workflow::WorkflowSource::Builtin => "built-in",
    }
}
