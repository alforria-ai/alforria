//! `subagents` tool — an alforria extension (no TS reference).
//!
//! A parent's view of the sessions its `task` calls created: what each is
//! doing, how it ended, and its last output, whoever prompted it last. Together
//! with `task`'s `task_id` it lets an agent manage its subagents itself instead
//! of relying on the user to watch them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::tool::def::{define, Agents, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::task::{render_output, RenderOutput, SubagentState, SubagentStatus, TaskOps};
use crate::tool::truncate::Truncate;

pub const TOOL_ID: &str = "subagents";

const DEFAULT_WAIT: Duration = Duration::from_secs(300);
const MAX_WAIT: Duration = Duration::from_secs(3600);
const WAIT_POLL: Duration = Duration::from_millis(250);

const DESCRIPTION: &str = r#"Check on and manage the subagents you started with the task tool.

A subagent keeps its session after it stops, and it may be running or finished without you having waited for it: in the background, or because the user prompted it directly. Use this tool rather than asking the user what a subagent did.

Actions:
- `list`: every subagent of this session, with its state and tool call count.
- `status`: one subagent's state, error and latest output. Needs `task_id`.
- `wait`: block until a running subagent stops, then return the same as `status`. Needs `task_id`. `timeout_seconds` defaults to 300.
- `cancel`: stop a running subagent. Needs `task_id`.

To continue a subagent that failed or stopped, or to send it follow-up work, call the task tool with its `task_id` and a prompt saying how to proceed. It keeps its context, so prefer this over starting again. Provider errors are already retried and resumed automatically before you hear about them. Do not poll in a loop: use `wait`."#;

#[derive(Debug, Deserialize)]
pub struct SubagentsParameters {
    action: String,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["list", "status", "wait", "cancel"],
                "description": "The action to perform"
            },
            "task_id": {
                "type": "string",
                "description": "The subagent's task_id (for status, wait and cancel)"
            },
            "timeout_seconds": {
                "type": "integer",
                "description": "How long to wait before returning anyway (for wait, default 300)"
            }
        },
        "required": ["action"]
    })
}

pub fn subagents_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    ops: Arc<dyn TaskOps>,
) -> ToolDef {
    define(
        TOOL_ID,
        DESCRIPTION,
        parameters(),
        None,
        truncate,
        agents,
        move |params: SubagentsParameters, ctx: ToolCtxRef<'_>| run(params, ctx, Arc::clone(&ops)),
    )
}

/// One of this session's own subagents, by id.
async fn own_subagent(
    ops: &Arc<dyn TaskOps>,
    session_id: &str,
    params: &SubagentsParameters,
) -> Result<SubagentState, ToolError> {
    let Some(task_id) = params.task_id.as_deref() else {
        return Err(ToolError::InvalidArguments {
            tool: TOOL_ID.to_string(),
            detail: format!("{} needs a task_id", params.action),
        });
    };
    ops.inspect(task_id)
        .await
        .filter(|state| state.parent_id.as_deref() == Some(session_id))
        .ok_or_else(|| ToolError::Failed(format!("{task_id} is not a subagent of this session")))
}

fn run(
    params: SubagentsParameters,
    ctx: ToolCtxRef<'_>,
    ops: Arc<dyn TaskOps>,
) -> BoxFuture<'_, Result<ExecuteResult, ToolError>> {
    Box::pin(async move {
        match params.action.as_str() {
            "list" => {
                let children = ops.children(ctx.session_id).await;
                Ok(ExecuteResult {
                    title: "Subagents".to_string(),
                    metadata: json!({ "count": children.len() }),
                    output: render_list(&children),
                    attachments: None,
                })
            }
            "status" => {
                let state = own_subagent(&ops, ctx.session_id, &params).await?;
                Ok(state_result(&state, None))
            }
            "wait" => {
                let mut state = own_subagent(&ops, ctx.session_id, &params).await?;
                let timeout = params
                    .timeout_seconds
                    .map_or(DEFAULT_WAIT, Duration::from_secs)
                    .min(MAX_WAIT);
                let started = Instant::now();
                while state.status.is_active() && started.elapsed() < timeout {
                    tokio::select! {
                        _ = tokio::time::sleep(WAIT_POLL) => {}
                        _ = ctx.abort.cancelled() => return Err(ToolError::Aborted),
                    }
                    state = own_subagent(&ops, ctx.session_id, &params).await?;
                }
                let note = state
                    .status
                    .is_active()
                    .then_some("Still running after the wait timed out.");
                Ok(state_result(&state, note))
            }
            "cancel" => {
                let state = own_subagent(&ops, ctx.session_id, &params).await?;
                if !state.status.is_active() {
                    return Ok(state_result(&state, Some("It was not running.")));
                }
                ops.cancel(&state.session_id).await;
                let state = own_subagent(&ops, ctx.session_id, &params).await?;
                Ok(state_result(&state, Some("Cancelled.")))
            }
            other => Err(ToolError::InvalidArguments {
                tool: TOOL_ID.to_string(),
                detail: format!("unknown action: {other}"),
            }),
        }
    })
}

fn state_result(state: &SubagentState, note: Option<&str>) -> ExecuteResult {
    ExecuteResult {
        title: state.title.clone(),
        metadata: json!({
            "sessionId": state.session_id,
            "state": state.status.as_str(),
            "toolCalls": state.tool_calls,
        }),
        output: render_state(state, note),
        attachments: None,
    }
}

fn render_state(state: &SubagentState, note: Option<&str>) -> String {
    let mut lines = Vec::new();
    if let Some(note) = note {
        lines.push(note.to_string());
    }
    if let SubagentStatus::Retrying { attempt, message } = &state.status {
        lines.push(format!("Retrying (attempt {attempt}): {message}"));
    }
    if let Some(error) = &state.error {
        lines.push(format!("Error: {error}"));
    }
    if let Some(text) = &state.last_text {
        lines.push(text.clone());
    }
    let summary = format!("{} ({} tool calls)", state.title, state.tool_calls);
    render_output(RenderOutput {
        session_id: &state.session_id,
        state: state.status.as_str(),
        summary: Some(&summary),
        text: &lines.join("\n\n"),
    })
}

fn render_list(children: &[SubagentState]) -> String {
    if children.is_empty() {
        return "This session has no subagents.".to_string();
    }
    let mut lines = vec!["Subagents of this session:".to_string()];
    for child in children {
        let agent = child.agent.as_deref().unwrap_or("unknown");
        let mut line = format!(
            "- {} [{}] {} (@{agent}, {} tool calls)",
            child.session_id,
            child.status.as_str(),
            child.title,
            child.tool_calls
        );
        if let Some(error) = &child.error {
            line.push_str(&format!(": {error}"));
        }
        lines.push(line);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(status: SubagentStatus) -> SubagentState {
        SubagentState {
            session_id: "ses_1".to_string(),
            parent_id: Some("ses_0".to_string()),
            title: "Look around".to_string(),
            agent: Some("general".to_string()),
            status,
            error: None,
            last_text: None,
            tool_calls: 3,
        }
    }

    #[test]
    fn a_failure_shows_its_error_and_last_output() {
        let failed = SubagentState {
            error: Some("boom".to_string()),
            last_text: Some("half done".to_string()),
            ..state(SubagentStatus::Failed)
        };
        let rendered = render_state(&failed, None);
        assert!(rendered.contains("state=\"error\""), "{rendered}");
        assert!(rendered.contains("<task_error>"), "{rendered}");
        assert!(rendered.contains("Error: boom"), "{rendered}");
        assert!(rendered.contains("half done"), "{rendered}");
    }

    #[test]
    fn the_list_names_each_child_and_its_state() {
        assert_eq!(render_list(&[]), "This session has no subagents.");
        let listed = render_list(&[state(SubagentStatus::Running)]);
        assert!(
            listed.contains("- ses_1 [running] Look around (@general, 3 tool calls)"),
            "{listed}"
        );
    }
}
