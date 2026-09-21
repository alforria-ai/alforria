//! Production [`TaskOps`] — `ops()` (prompt.ts:144-150):
//! `{cancel: state.cancel, resolvePromptParts, prompt: prompt().orDie}`.
//!
//! The M4 task tool calls back into the session engine through this seam:
//! child sessions are created with [`crate::session::store::SessionStore`]
//! and driven by the [`crate::session::prompt::SessionPrompt`] facade
//! (prompt re-entrancy is the whole point of the run state).

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use opencode_schema::permission_v1::{PermissionV1Action, PermissionV1Rule};
use opencode_schema::session_v1::{AssistantError, V1Message, V1Part, V1ToolState};

use crate::session::error::SessionError;
use crate::session::message::MessageStore;
use crate::session::store::{CreateInput, SessionContext, SessionStore};
use crate::tool::error::ToolError;

/// `Effect.orDie` — the session seam defects into the tool failure.
fn tool_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::Failed(error.to_string())
}
use crate::tool::task::{ModelRef, ParentMessage, PromptOutcome, Rule, SubagentInfo, TaskOps};

/// The `TaskPromptOps` surface (prompt.ts:144-150) — the facade half the
/// task tool drives: `cancel`, `resolvePromptParts`, `prompt`.
pub trait PromptFacade: Send + Sync {
    /// `cancel` (prompt.ts:152-155): `state.cancel(sessionID)`.
    fn cancel<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<(), SessionError>>;
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
}

impl ProductionTaskOps {
    pub fn new(
        sessions: SessionStore,
        messages: MessageStore,
        agents: crate::session::agents::AgentRegistry,
        context: SessionContext,
    ) -> Arc<Self> {
        Arc::new(ProductionTaskOps {
            facade: Mutex::new(None),
            sessions,
            messages,
            agents,
            context,
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
            let facade = self.facade()?;
            let parts = facade
                .resolve_prompt_parts(prompt)
                .await
                .map_err(tool_error)?;
            let input = crate::session::prompt_input::PromptInput {
                session_id: session_id.to_string(),
                message_id: Some(
                    crate::session::ids::MessageId::ascending(None).map_err(tool_error)?,
                ),
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
            let result = facade.prompt(input).await.map_err(tool_error)?;
            let mut error = None;
            if let V1Message::Assistant {
                error: Some(message_error),
                ..
            } = &result.info
            {
                error = Some(error_text(message_error));
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
            Ok(PromptOutcome { text, error })
        })
    }

    fn cancel<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Ok(facade) = self.facade() {
                let _ = facade.cancel(session_id).await;
            }
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
