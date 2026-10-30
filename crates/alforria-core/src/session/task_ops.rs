//! Production [`TaskOps`] — `ops()` (prompt.ts:144-150):
//! `{cancel: state.cancel, resolvePromptParts, prompt: prompt().orDie}`.
//!
//! The M4 task tool calls back into the session engine through this seam:
//! child sessions are created with [`crate::session::store::SessionStore`]
//! and driven by the [`crate::session::prompt::SessionPrompt`] facade
//! (prompt re-entrancy is the whole point of the run state).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use alforria_schema::permission_v1::{PermissionV1Action, PermissionV1Rule};
use alforria_schema::session_status::SessionStatusInfo;
use alforria_schema::session_v1::{AssistantError, V1Message, V1Part, V1SessionInfo, V1ToolState};
use futures::future::BoxFuture;

use crate::session::error::SessionError;
use crate::session::message::MessageStore;
use crate::session::retry::{neutralize_tags, untrusted_output};
use crate::session::status::SessionStatusService;
use crate::session::store::{CreateInput, SessionContext, SessionStore};
use crate::tool::error::ToolError;

/// `Effect.orDie` — the session seam defects into the tool failure.
fn tool_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::Failed(error.to_string())
}
use crate::tool::task::{render_output, RenderOutput, SubagentState, SubagentStatus};
use crate::tool::task::{ModelRef, ParentMessage, PromptOutcome, Rule, SubagentInfo, TaskOps};

/// The `TaskPromptOps` surface (prompt.ts:144-150) — the facade half the
/// task tool drives: `cancel`, `resolvePromptParts`, `prompt`.
pub trait PromptFacade: Send + Sync {
    /// `cancel` (prompt.ts:152-155): `state.cancel(sessionID)`.
    fn cancel<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<(), SessionError>>;
    /// How many cancels the session has been asked for.
    fn cancel_count(&self, _session_id: &str) -> u64 {
        0
    }
    /// `resolvePromptParts` (prompt.ts:157-191).
    fn resolve_prompt_parts<'a>(
        &'a self,
        template: &'a str,
    ) -> BoxFuture<'a, Result<Vec<crate::session::prompt_input::PromptPartInput>, SessionError>>;
    /// `prompt(input).pipe(Effect.catch(Effect.die))` (prompt.ts:1052-1071).
    fn prompt<'a>(
        &'a self,
        input: crate::session::prompt_input::PromptInput,
    ) -> BoxFuture<'a, Result<crate::session::message::WithParts, SessionError>>;
}

/// The production `TaskOps`: the session engine pieces the M4 task tool
/// needs. The [`crate::session::prompt::SessionPrompt`] facade is
/// late-bound (the facade owns the registry that owns this tool).
pub struct ProductionTaskOps {
    facade: Mutex<Option<Arc<dyn PromptFacade>>>,
    sessions: SessionStore,
    messages: MessageStore,
    agents: crate::session::agents::AgentRegistry,
    context: SessionContext,
    status: Arc<SessionStatusService>,
    /// Sessions a `task` call is prompting right now, and how many times over.
    /// Whoever waits on those gets the outcome directly.
    awaited: Mutex<HashMap<String, usize>>,
}

impl ProductionTaskOps {
    pub fn new(
        sessions: SessionStore,
        messages: MessageStore,
        agents: crate::session::agents::AgentRegistry,
        context: SessionContext,
        status: Arc<SessionStatusService>,
    ) -> Arc<Self> {
        Arc::new(ProductionTaskOps {
            facade: Mutex::new(None),
            sessions,
            messages,
            agents,
            context,
            status,
            awaited: Mutex::new(HashMap::new()),
        })
    }

    /// Late-bind the facade (the facade owns the tool registry that owns
    /// the task tool built with these ops).
    pub fn bind(self: &Arc<Self>, facade: Arc<dyn PromptFacade>) {
        *self.facade.lock().unwrap() = Some(facade);
    }

    fn facade(&self) -> Result<Arc<dyn PromptFacade>, ToolError> {
        self.facade
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| ToolError::Failed("TaskTool requires promptOps".to_string()))
    }
}

impl TaskOps for ProductionTaskOps {
    /// Parent-chain depth walk (task.ts:120-128).
    fn depth<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<usize, ToolError>> {
        Box::pin(async move {
            let mut current = self.sessions.get(session_id).map_err(tool_error)?;
            let mut depth = 0;
            while let Some(parent) = current.parent_id.clone() {
                depth += 1;
                current = self.sessions.get(&parent).map_err(tool_error)?;
            }
            Ok(depth)
        })
    }

    fn agent<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<SubagentInfo>, ToolError>> {
        Box::pin(async move {
            Ok(self.agents.get(name).cloned().map(|found| SubagentInfo {
                name: found.name.clone(),
                model: found.model.as_ref().map(|model| ModelRef {
                    model_id: model.model_id.clone(),
                    provider_id: model.provider_id.clone(),
                }),
                permission: found
                    .permission
                    .iter()
                    .map(|rule| Rule {
                        permission: rule.permission.clone(),
                        pattern: rule.pattern.clone(),
                        action: match rule.action {
                            PermissionV1Action::Allow => "allow",
                            PermissionV1Action::Deny => "deny",
                            PermissionV1Action::Ask => "ask",
                        },
                    })
                    .collect(),
            }))
        })
    }

    /// `sessions.get(taskID)` — a resolvable session is an existing one
    /// (task.ts:131-133).
    fn session_exists<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move { self.sessions.get(session_id).is_ok() })
    }

    /// `sessions.get(ctx.sessionID).permission ?? []` (task.ts:124-138) —
    /// the production data path the derivation feeds on.
    fn session_permission<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Vec<Rule>> {
        Box::pin(async move {
            self.sessions
                .get(session_id)
                .map(|session| {
                    session
                        .permission
                        .unwrap_or_default()
                        .iter()
                        .map(|rule| Rule {
                            permission: rule.permission.clone(),
                            pattern: rule.pattern.clone(),
                            action: match rule.action {
                                PermissionV1Action::Allow => "allow",
                                PermissionV1Action::Ask => "ask",
                                PermissionV1Action::Deny => "deny",
                            },
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    fn create_session<'a>(
        &'a self,
        parent_id: &'a str,
        title: &'a str,
        agent: &'a str,
        permission: Vec<Rule>,
    ) -> BoxFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let permission: Vec<PermissionV1Rule> = permission
                .into_iter()
                .map(|rule| {
                    let action = match rule.action {
                        "allow" => PermissionV1Action::Allow,
                        "ask" => PermissionV1Action::Ask,
                        _ => PermissionV1Action::Deny,
                    };
                    PermissionV1Rule {
                        permission: rule.permission,
                        pattern: rule.pattern,
                        action,
                    }
                })
                .collect();
            let session = self
                .sessions
                .create(
                    &self.context,
                    &CreateInput {
                        parent_id: Some(parent_id.to_string()),
                        title: Some(title.to_string()),
                        agent: Some(agent.to_string()),
                        permission: Some(permission),
                        ..Default::default()
                    },
                )
                .map_err(tool_error)?;
            Ok(session.id)
        })
    }

    fn parent_message<'a>(
        &'a self,
        session_id: &'a str,
        message_id: &'a str,
    ) -> BoxFuture<'a, Result<ParentMessage, ToolError>> {
        Box::pin(async move {
            let found = self
                .messages
                .get(session_id, message_id)
                .map_err(tool_error)?;
            match found.info {
                V1Message::Assistant {
                    variant,
                    model_id,
                    provider_id,
                    ..
                } => Ok(ParentMessage {
                    variant,
                    model: Some(ModelRef {
                        model_id,
                        provider_id,
                    }),
                }),
                _ => Err(ToolError::Failed("Not an assistant message".to_string())),
            }
        })
    }

    /// `runTask` (task.ts:203-233): `resolvePromptParts` + `prompt`, the
    /// outcome folded into the last text part and any error message.
    fn prompt<'a>(
        &'a self,
        session_id: &'a str,
        agent: &'a str,
        model: &'a ModelRef,
        variant: Option<&'a str>,
        prompt: &'a str,
    ) -> BoxFuture<'a, Result<PromptOutcome, ToolError>> {
        Box::pin(async move {
            let parts = self
                .facade()?
                .resolve_prompt_parts(prompt)
                .await
                .map_err(tool_error)?;
            self.run_prompt(session_id, agent, model, variant, parts)
                .await
        })
    }

    fn resume<'a>(
        &'a self,
        session_id: &'a str,
        agent: &'a str,
        model: &'a ModelRef,
        variant: Option<&'a str>,
        note: &'a str,
    ) -> BoxFuture<'a, Result<PromptOutcome, ToolError>> {
        Box::pin(async move {
            let parts = vec![crate::session::prompt_input::PromptPartInput::Text {
                id: None,
                text: note.to_string(),
                synthetic: Some(true),
                ignored: None,
                time: None,
                metadata: None,
            }];
            self.run_prompt(session_id, agent, model, variant, parts)
                .await
        })
    }

    fn cancel<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Ok(facade) = self.facade() {
                let _ = facade.cancel(session_id).await;
            }
        })
    }

    fn cancel_count(&self, session_id: &str) -> u64 {
        self.facade()
            .map_or(0, |facade| facade.cancel_count(session_id))
    }

    fn inspect<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Option<SubagentState>> {
        Box::pin(async move {
            let session = self.sessions.get(session_id).ok()?;
            Some(self.state_of(session))
        })
    }

    fn children<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Vec<SubagentState>> {
        Box::pin(async move {
            let mut children = self.sessions.children(session_id).unwrap_or_default();
            children.sort_by(|a, b| a.id.cmp(&b.id));
            children
                .into_iter()
                .map(|session| self.state_of(session))
                .collect()
        })
    }

    /// `injectBackgroundResult` (task.ts:236-252) — a synthetic text prompt
    /// into the parent session, fire-and-forget.
    fn inject_background_result<'a>(
        &'a self,
        parent_session_id: &'a str,
        variant: Option<&'a str>,
        text: &'a str,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let Ok(facade) = self.facade() else { return };
            let agent = self
                .sessions
                .get(parent_session_id)
                .ok()
                .and_then(|session| session.agent.clone());
            let input = crate::session::prompt_input::PromptInput {
                session_id: parent_session_id.to_string(),
                message_id: None,
                model: None,
                agent,
                no_reply: None,
                tools: None,
                format: None,
                system: None,
                variant: variant.map(|value| value.to_string()),
                parts: vec![crate::session::prompt_input::PromptPartInput::Text {
                    id: None,
                    text: text.to_string(),
                    synthetic: Some(true),
                    ignored: None,
                    time: None,
                    metadata: None,
                }],
            };
            let _ = facade.prompt(input).await;
        })
    }
}

impl ProductionTaskOps {
    /// `runTask` (task.ts:203-233), past the prompt's parts: the outcome folded
    /// into the last text part and any error message.
    async fn run_prompt(
        &self,
        session_id: &str,
        agent: &str,
        model: &ModelRef,
        variant: Option<&str>,
        parts: Vec<crate::session::prompt_input::PromptPartInput>,
    ) -> Result<PromptOutcome, ToolError> {
        let facade = self.facade()?;
        let input = crate::session::prompt_input::PromptInput {
            session_id: session_id.to_string(),
            message_id: Some(crate::session::ids::MessageId::ascending(None).map_err(tool_error)?),
            model: Some(crate::session::prompt_input::ModelRef {
                provider_id: model.provider_id.clone(),
                model_id: model.model_id.clone(),
            }),
            agent: Some(agent.to_string()),
            no_reply: None,
            tools: None,
            format: None,
            system: None,
            variant: variant.map(|value| value.to_string()),
            parts,
        };
        let result = {
            let _awaited = Awaited::new(&self.awaited, session_id);
            facade.prompt(input).await.map_err(tool_error)?
        };
        let mut error = None;
        let mut is_recoverable = false;
        if let V1Message::Assistant {
            error: Some(message_error),
            ..
        } = &result.info
        {
            error = Some(error_text(message_error));
            is_recoverable = crate::session::retry::resumable(message_error);
        }
        if error.is_none() {
            if let Some(failed) = result.parts.iter().rev().find_map(|part| match part {
                V1Part::Tool {
                    state: V1ToolState::Error { error, .. },
                    ..
                } => Some(error.clone()),
                _ => None,
            }) {
                error = Some(failed);
            }
        }
        let text = result
            .parts
            .iter()
            .rev()
            .find_map(|part| match part {
                V1Part::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        Ok(PromptOutcome {
            text,
            error,
            recoverable: is_recoverable,
        })
    }

    fn state_of(&self, session: V1SessionInfo) -> SubagentState {
        // `stream` is newest-first; each turn's outcome replaces the last one's.
        let messages = self.messages.stream(&session.id).unwrap_or_default();
        let mut error = None;
        let mut last_text = None;
        let mut tool_calls = 0;
        let mut settled = None;
        for message in messages.iter().rev() {
            if let V1Message::Assistant {
                error: message_error,
                finish,
                time,
                ..
            } = &message.info
            {
                error = message_error.clone();
                if message_error.is_some() || finish.is_some() || time.completed.is_some() {
                    settled = Some(());
                }
            }
            for part in &message.parts {
                match part {
                    V1Part::Text { text, .. } if !text.trim().is_empty() => {
                        if matches!(message.info, V1Message::Assistant { .. }) {
                            last_text = Some(text.clone());
                        }
                    }
                    V1Part::Tool { .. } => tool_calls += 1,
                    _ => {}
                }
            }
        }
        let status = match self.status.get(&session.id) {
            SessionStatusInfo::Busy => SubagentStatus::Running,
            SessionStatusInfo::Retry {
                attempt, message, ..
            } => SubagentStatus::Retrying { attempt, message },
            SessionStatusInfo::Idle => match &error {
                Some(AssistantError::Aborted { .. }) => SubagentStatus::Cancelled,
                Some(_) => SubagentStatus::Failed,
                None if settled.is_some() => SubagentStatus::Completed,
                None => SubagentStatus::NotStarted,
            },
        };
        SubagentState {
            session_id: session.id,
            parent_id: session.parent_id,
            title: session.title,
            agent: session.agent,
            status,
            error: error.as_ref().map(error_text),
            last_text,
            tool_calls,
        }
    }

    /// A prompt from outside the engine (the HTTP routes): a subagent the user
    /// prompted directly reports to its parent when it ends. A `task` call
    /// waiting on the session already gets this outcome, and when both end
    /// together it may be gone by the time this prompt returns, so that is
    /// asked up front. The parent's follow-up is a whole turn, which the caller
    /// should not wait for.
    pub async fn prompt_reporting(
        self: &Arc<Self>,
        prompt: &crate::session::prompt::SessionPrompt,
        input: crate::session::prompt_input::PromptInput,
    ) -> Result<crate::session::message::WithParts, crate::session::prompt_input::PromptError> {
        let session_id = input.session_id.clone();
        let reports = input.no_reply != Some(true) && !self.is_awaited(&session_id);
        let result = prompt.prompt(input).await;
        if reports && result.is_ok() {
            let ops = Arc::clone(self);
            tokio::spawn(async move { ops.report_to_parent(&session_id).await });
        }
        result
    }

    /// Whether a `task` call is prompting this session right now.
    pub fn is_awaited(&self, session_id: &str) -> bool {
        self.awaited
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(session_id)
    }

    /// A subagent the user prompted directly has finished: tell its parent, the
    /// way a background task does, so nobody has to carry the result over by
    /// hand. Nothing to do if a `task` call is waiting on the same session, the
    /// user cancelled it, more work is queued, or the parent never had a turn
    /// to follow up on.
    pub async fn report_to_parent(&self, session_id: &str) {
        let Ok(session) = self.sessions.get(session_id) else {
            return;
        };
        let Some(parent_id) = session.parent_id.clone() else {
            return;
        };
        if self.is_awaited(session_id) {
            return;
        }
        let has_turns = self
            .messages
            .stream(&parent_id)
            .is_ok_and(|messages| !messages.is_empty());
        if !has_turns {
            return;
        }
        // The prompt's caller is released just before the runner goes idle.
        let mut state = self.state_of(session.clone());
        for _ in 0..REPORT_IDLE_POLLS {
            if !state.status.is_active() {
                break;
            }
            tokio::time::sleep(REPORT_IDLE_POLL).await;
            state = self.state_of(session.clone());
        }
        let (rendered, text) = match state.status {
            SubagentStatus::Completed => ("completed", state.last_text.unwrap_or_default()),
            SubagentStatus::Failed => ("error", state.error.unwrap_or_default()),
            _ => return,
        };
        let text = untrusted_output(&text);
        let summary = neutralize_tags(&format!(
            "The user continued this subagent directly: {}",
            state.title
        ));
        self.inject_background_result(
            &parent_id,
            None,
            &render_output(RenderOutput {
                session_id,
                state: rendered,
                summary: Some(&summary),
                text: &text,
            }),
        )
        .await;
    }
}

/// How long a finished session gets to go idle; past that, more work is queued.
const REPORT_IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(25);
const REPORT_IDLE_POLLS: usize = 80;

/// Marks a session as awaited by a `task` call for as long as it is held, so
/// cancelling that call clears the mark too.
struct Awaited<'a> {
    awaited: &'a Mutex<HashMap<String, usize>>,
    session_id: &'a str,
}

impl<'a> Awaited<'a> {
    fn new(awaited: &'a Mutex<HashMap<String, usize>>, session_id: &'a str) -> Self {
        *awaited
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(session_id.to_string())
            .or_default() += 1;
        Awaited {
            awaited,
            session_id,
        }
    }
}

impl Drop for Awaited<'_> {
    fn drop(&mut self) {
        let mut awaited = self
            .awaited
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = awaited.get_mut(self.session_id) {
            *count -= 1;
            if *count == 0 {
                awaited.remove(self.session_id);
            }
        }
    }
}

/// `"message" in error.data.message ? error.data.message : error.name`
/// (task.ts:216-219).
fn error_text(error: &AssistantError) -> String {
    match error {
        AssistantError::Auth { message, .. }
        | AssistantError::Unknown { message, .. }
        | AssistantError::Aborted { message }
        | AssistantError::StructuredOutput { message, .. }
        | AssistantError::ContextOverflow { message, .. }
        | AssistantError::ContentFilter { message }
        | AssistantError::Api { message, .. } => message.clone(),
        AssistantError::OutputLength {} => "MessageOutputLengthError".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::{
        create_engine_session, create_session, engine, text_stream,
    };

    #[tokio::test]
    async fn agent_lookup() {
        let e = engine("task-ops-agent", vec![]);
        let build = e.ops.agent("build").await.unwrap().expect("build agent");
        assert_eq!(build.name, "build");
        assert!(e.ops.agent("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn depth_walks_parent_chain() {
        let e = engine("task-ops-depth", vec![]);
        let parent = create_session(&e.services.sessions, &e.worktree);
        let child = e
            .services
            .sessions
            .create(
                &crate::session::store::SessionContext {
                    project_id: "global".to_string(),
                    directory: e.worktree.clone(),
                    worktree: e.worktree.clone(),
                    workspace_id: None,
                },
                &CreateInput {
                    parent_id: Some(parent.id.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(e.ops.depth(&parent.id).await.unwrap(), 0);
        assert_eq!(e.ops.depth(&child.id).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn session_exists_round_trip() {
        let e = engine("task-ops-exists", vec![]);
        let session = create_session(&e.services.sessions, &e.worktree);
        assert!(e.ops.session_exists(&session.id).await);
        assert!(!e.ops.session_exists("ses_missing").await);
    }

    #[tokio::test]
    async fn parent_message_rejects_user() {
        let e = engine("task-ops-parent-user", vec![]);
        let session = create_session(&e.services.sessions, &e.worktree);
        let user = crate::session::test_support::user_message(&session.id, "msg_u1", 1.0);
        e.services.sessions.update_message(&user).unwrap();
        let error = e
            .ops
            .parent_message(&session.id, "msg_u1")
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "Not an assistant message");
    }

    #[tokio::test]
    async fn parent_message_returns_variant_and_model() {
        let e = engine("task-ops-parent-assistant", vec![]);
        let session = create_session(&e.services.sessions, &e.worktree);
        let parent = crate::session::test_support::user_message(&session.id, "msg_p1", 1.0);
        e.services.sessions.update_message(&parent).unwrap();
        let assistant = crate::session::test_support::assistant_message(
            &session.id,
            "msg_a1",
            "msg_p1",
            1,
            None,
            None,
        );
        e.services.sessions.update_message(&assistant).unwrap();
        let found = e.ops.parent_message(&session.id, "msg_a1").await.unwrap();
        assert_eq!(found.model.expect("model").model_id, "claude");
    }

    #[tokio::test]
    async fn create_session_sets_parent_title_agent() {
        let e = engine("task-ops-create", vec![]);
        let parent = create_session(&e.services.sessions, &e.worktree);
        let child = e
            .ops
            .create_session(
                &parent.id,
                "research stuff (@research subagent)",
                "research",
                Vec::new(),
            )
            .await
            .unwrap();
        let session = e.services.sessions.get(&child).unwrap();
        assert_eq!(session.parent_id, Some(parent.id));
        assert_eq!(session.title, "research stuff (@research subagent)");
        assert_eq!(session.agent, Some("research".to_string()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_round_trip() {
        let e = engine("task-ops-prompt", vec![text_stream("subagent says hi")]);
        let parent = create_engine_session(&e);
        let user = crate::session::test_support::user_message(&parent.id, "msg_p1", 1.0);
        e.services.sessions.update_message(&user).unwrap();
        let assistant = crate::session::test_support::assistant_message(
            &parent.id, "msg_a1", "msg_p1", 1, None, None,
        );
        e.services.sessions.update_message(&assistant).unwrap();

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            e.ops.prompt(
                &parent.id,
                "build",
                &crate::tool::task::ModelRef {
                    model_id: "claude".to_string(),
                    provider_id: "anthropic".to_string(),
                },
                None,
                "go research",
            ),
        )
        .await
        .expect("prompt times out")
        .unwrap();
        assert_eq!(outcome.text, "subagent says hi");
        assert_eq!(outcome.error, None);
    }
}
