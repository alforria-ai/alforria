//! `plan_exit` tool — port of `tool/plan.ts` (spec M4.8).

use std::sync::Arc;

use serde_json::{json, Value};

use crate::tool::def::{define, Agents, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::truncate::Truncate;

/// `Session.plan(info, instance)` resolved by the M5 session service; the
/// plan-file path relative to the worktree.
pub trait PlanSessions: Send + Sync {
    /// Session plan file relative to the instance worktree.
    fn plan_path<'a>(
        &'a self,
        session_id: &'a str,
        worktree: &'a str,
    ) -> BoxFuture<'a, Result<String, ToolError>>;
    /// Insert the synthetic build-agent user message + text part.
    fn switch_to_build_agent<'a>(
        &'a self,
        session_id: &'a str,
        plan: &'a str,
    ) -> BoxFuture<'a, Result<(), ToolError>>;
}

/// The `Question.Service` seam with the plan_exit answer shape
/// (a Vec of answer-arrays, one per question).
pub trait PlanQuestion: Send + Sync {
    fn ask_plan_exit<'a>(
        &'a self,
        session_id: &'a str,
        message_id: &'a str,
        call_id: Option<&'a str>,
        plan: &'a str,
    ) -> BoxFuture<'a, Result<Vec<Vec<String>>, ToolError>>;
}

pub const PLAN_EXIT_OUTPUT: &str =
    "User approved switching to build agent. Wait for further instructions.";
pub const PLAN_EXIT_TITLE: &str = "Switching to build agent";
pub const QUESTION_HEADER: &str = "Build Agent";

/// Build the `plan_exit` tool.
pub fn plan_exit_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    sessions: Arc<dyn PlanSessions>,
    question: Arc<dyn PlanQuestion>,
) -> ToolDef {
    define(
        "plan_exit",
        include_str!("txt/plan-exit.txt"),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
        }),
        None,
        truncate,
        agents,
        move |_params: Value, ctx: ToolCtxRef<'_>| {
            let sessions = sessions.clone();
            let question = question.clone();
            Box::pin(async move { run(ctx, sessions, question).await })
        },
    )
}

async fn run(
    ctx: ToolCtxRef<'_>,
    sessions: Arc<dyn PlanSessions>,
    question: Arc<dyn PlanQuestion>,
) -> Result<ExecuteResult, ToolError> {
    let plan = sessions
        .plan_path(ctx.session_id, &ctx.instance.worktree.to_string_lossy())
        .await?;

    let answers = question
        .ask_plan_exit(ctx.session_id, ctx.message_id, ctx.call_id, &plan)
        .await?;

    if answers.first().and_then(|first| first.first()) == Some(&"No".to_string()) {
        return Err(ToolError::Failed(
            "The user dismissed this question".to_string(),
        ));
    }

    sessions
        .switch_to_build_agent(ctx.session_id, &plan)
        .await?;

    Ok(ExecuteResult {
        title: PLAN_EXIT_TITLE.to_string(),
        output: PLAN_EXIT_OUTPUT.to_string(),
        metadata: json!({}),
        attachments: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::def::Extra;
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeSessions {
        switches: AtomicUsize,
    }

    impl PlanSessions for FakeSessions {
        fn plan_path<'a>(
            &'a self,
            _session_id: &'a str,
            _worktree: &'a str,
        ) -> BoxFuture<'a, Result<String, ToolError>> {
            Box::pin(async { Ok("docs/plan.md".to_string()) })
        }
        fn switch_to_build_agent<'a>(
            &'a self,
            _session_id: &'a str,
            _plan: &'a str,
        ) -> BoxFuture<'a, Result<(), ToolError>> {
            Box::pin(async move {
                self.switches.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    struct FixedQuestion {
        answer: &'static str,
    }

    impl PlanQuestion for FixedQuestion {
        fn ask_plan_exit<'a>(
            &'a self,
            _session_id: &'a str,
            _message_id: &'a str,
            _call_id: Option<&'a str>,
            _plan: &'a str,
        ) -> BoxFuture<'a, Result<Vec<Vec<String>>, ToolError>> {
            Box::pin(async move { Ok(vec![vec![self.answer.to_string()]]) })
        }
    }

    fn tool(sessions: Arc<dyn PlanSessions>, question: Arc<dyn PlanQuestion>) -> ToolDef {
        plan_exit_tool(
            Arc::new(TruncateService::default_limits(std::path::PathBuf::from(
                "/tmp/opencode",
            ))),
            fixed_agents(),
            sessions,
            question,
        )
    }

    async fn call(
        sessions: Arc<dyn PlanSessions>,
        question: Arc<dyn PlanQuestion>,
    ) -> (
        Result<ExecuteResult, ToolError>,
        Vec<crate::tool::def::MetadataInput>,
    ) {
        let def = tool(sessions, question);
        let ask = RecordingAsk::new();
        let inst = instance(std::path::Path::new("/tmp/opencode"));
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(json!({}), ctx).await;
        let metadata = ask.metadata_calls.lock().unwrap().clone();
        (result, metadata)
    }

    #[tokio::test]
    async fn yes_path_switches_to_build_agent() {
        let sessions = Arc::new(FakeSessions {
            switches: AtomicUsize::new(0),
        });
        let (result, _) = call(sessions.clone(), Arc::new(FixedQuestion { answer: "Yes" })).await;
        let result = result.unwrap();
        assert_eq!(result.output, PLAN_EXIT_OUTPUT);
        assert_eq!(result.title, PLAN_EXIT_TITLE);
        assert_eq!(sessions.switches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn no_path_rejects() {
        let sessions = Arc::new(FakeSessions {
            switches: AtomicUsize::new(0),
        });
        let (result, _) = call(sessions.clone(), Arc::new(FixedQuestion { answer: "No" })).await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "The user dismissed this question"
        );
        assert_eq!(sessions.switches.load(Ordering::SeqCst), 0);
    }
}
