//! Production [`WorkflowOps`] — child sessions + the prompt facade.
//!
//! A workflow run is a normal parent session; each `agent()` call creates a
//! child session and prompts it through the same engine as the `task` tool.
//! The facade is shared, so an agent's transcript is a first-class session the
//! web client can open.

use std::sync::{Arc, Mutex};

use alforria_schema::permission_v1::{PermissionV1Action, PermissionV1Rule};
use alforria_schema::schema::JsonMap;
use alforria_schema::session_v1::{AssistantError, OutputFormat, V1Message, V1Part, V1ToolState};
use serde_json::{json, Value};

use crate::session::agents::AgentRegistry;
use crate::session::message::MessageStore;
use crate::session::prompt_input::{ModelRef, PromptInput, PromptPartInput};
use crate::session::store::{CreateInput, SessionContext, SessionStore};
use crate::session::task_ops::PromptFacade;
use crate::tool::def::BoxFuture;
use crate::tool::permission::evaluate;
use crate::tool::task::{derive_subagent_session_permission, Rule};
use crate::workflow::{WorkflowAgentOutcome, WorkflowAgentRequest, WorkflowOps};

/// The session-engine implementation of [`WorkflowOps`].
pub struct ProductionWorkflowOps {
    sessions: SessionStore,
    messages: MessageStore,
    context: SessionContext,
    agents: AgentRegistry,
    primary_tools: Vec<String>,
    facade: Mutex<Option<Arc<dyn PromptFacade>>>,
}

impl ProductionWorkflowOps {
    pub fn new(
        sessions: SessionStore,
        messages: MessageStore,
        context: SessionContext,
        agents: AgentRegistry,
        primary_tools: Vec<String>,
    ) -> Arc<Self> {
        Arc::new(ProductionWorkflowOps {
            sessions,
            messages,
            context,
            agents,
            primary_tools,
            facade: Mutex::new(None),
        })
    }

    /// The prompt facade owns the tool registry that owns the workflow tool,
    /// so it is bound after the engine graph is built.
    pub fn bind(self: &Arc<Self>, facade: Arc<dyn PromptFacade>) {
        *self
            .facade
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(facade);
    }

    fn facade(&self) -> Result<Arc<dyn PromptFacade>, String> {
        self.facade
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| "workflow prompt facade is not bound".to_string())
    }

    async fn run_prompt(
        &self,
        session_id: &str,
        agent: &str,
        parts: Vec<PromptPartInput>,
        schema: Option<&Value>,
    ) -> Result<WorkflowAgentOutcome, String> {
        let facade = self.facade()?;
        let format = match schema {
            None => None,
            Some(Value::Object(schema)) => Some(OutputFormat::JsonSchema {
                schema: schema.clone(),
                retry_count: None,
            }),
            Some(_) => return Err("options.schema must be a JSON Schema object".to_string()),
        };
        // Like the `task` tool: the agent's own model, else the parent's.
        let (parent_model, variant) = self
            .sessions
            .get(session_id)
            .ok()
            .and_then(|session| session.parent_id)
            .map_or((None, None), |parent| self.session_model(&parent));
        let model = self
            .agents
            .get(agent)
            .and_then(|found| found.model.as_ref())
            .map(|model| ModelRef {
                provider_id: model.provider_id.clone(),
                model_id: model.model_id.clone(),
            })
            .or(parent_model);
        let input = PromptInput {
            session_id: session_id.to_string(),
            message_id: Some(
                crate::session::ids::MessageId::ascending(None)
                    .map_err(|error| error.to_string())?,
            ),
            model,
            agent: Some(agent.to_string()),
            no_reply: None,
            tools: None,
            format,
            system: None,
            variant,
            parts,
        };
        let result = facade
            .prompt(input)
            .await
            .map_err(|error| error.to_string())?;
        let mut error = None;
        let mut recoverable = false;
        if let V1Message::Assistant {
            error: Some(message_error),
            ..
        } = &result.info
        {
            error = Some(error_text(message_error));
            recoverable = crate::session::retry::resumable(message_error);
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
        if let V1Message::Assistant {
            structured: Some(structured),
            ..
        } = &result.info
        {
            return Ok(WorkflowAgentOutcome {
                text: structured.to_string(),
                error,
                recoverable,
            });
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
        Ok(WorkflowAgentOutcome {
            text,
            error,
            recoverable,
        })
    }

    /// The model and variant of a session's latest turn. A fresh run session
    /// has none, and its agents use the default model.
    fn session_model(&self, session_id: &str) -> (Option<ModelRef>, Option<String>) {
        // `stream` is newest-first.
        let messages = self.messages.stream(session_id).unwrap_or_default();
        match messages.first().map(|message| &message.info) {
            Some(V1Message::Assistant {
                model_id,
                provider_id,
                variant,
                ..
            }) => (
                Some(ModelRef {
                    provider_id: provider_id.clone(),
                    model_id: model_id.clone(),
                }),
                variant.clone(),
            ),
            Some(V1Message::User { model, .. }) => (
                Some(ModelRef {
                    provider_id: model.provider_id.clone(),
                    model_id: model.model_id.clone(),
                }),
                model.variant.clone(),
            ),
            None => (None, None),
        }
    }
}

fn rules_of(rules: &[PermissionV1Rule]) -> Vec<Rule> {
    rules
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
}

fn rules_into(rules: Vec<Rule>) -> Vec<PermissionV1Rule> {
    rules
        .into_iter()
        .map(|rule| PermissionV1Rule {
            action: match rule.action {
                "allow" => PermissionV1Action::Allow,
                "ask" => PermissionV1Action::Ask,
                _ => PermissionV1Action::Deny,
            },
            permission: rule.permission,
            pattern: rule.pattern,
        })
        .collect()
}

impl WorkflowOps for ProductionWorkflowOps {
    fn create_run_session<'a>(
        &'a self,
        run_id: &'a str,
        title: &'a str,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let mut metadata = JsonMap::new();
            metadata.insert("workflowRunID".to_string(), json!(run_id));
            let session = self
                .sessions
                .create(
                    &self.context,
                    &CreateInput {
                        title: Some(title.to_string()),
                        agent: Some("build".to_string()),
                        metadata: Some(metadata),
                        ..Default::default()
                    },
                )
                .map_err(|error| error.to_string())?;
            Ok(session.id)
        })
    }

    fn create_agent_session<'a>(
        &'a self,
        request: &'a WorkflowAgentRequest,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let Some(agent) = self.agents.get(&request.agent) else {
                return Err(format!("unknown workflow agent: {}", request.agent));
            };
            let parent = self.sessions.get(&request.parent_session_id).ok();
            let parent_permission = parent
                .as_ref()
                .and_then(|session| session.permission.clone())
                .unwrap_or_default();
            // The run was approved as a whole, so there is no one to ask about
            // each agent, but a `task` deny still stands (`task.rs` asks here).
            let parent_agent = parent
                .as_ref()
                .and_then(|session| session.agent.as_deref())
                .and_then(|name| self.agents.get(name));
            let no_rules = Vec::new();
            let rulesets = [
                parent_agent.map_or(&no_rules, |agent| &agent.permission),
                &parent_permission,
            ];
            if evaluate("task", &request.agent, &rulesets).action == PermissionV1Action::Deny {
                return Err(format!(
                    "permission denied: agent {} may not be run as a subagent here",
                    request.agent
                ));
            }
            let mut permission = derive_subagent_session_permission(
                &rules_of(&parent_permission),
                &rules_of(&agent.permission),
                &self.primary_tools,
            );
            permission.push(Rule {
                permission: "workflow".to_string(),
                pattern: "*".to_string(),
                action: "deny",
            });
            let mut metadata = JsonMap::new();
            metadata.insert("workflowRunID".to_string(), json!(request.run_id));
            metadata.insert("workflowAgent".to_string(), json!(request.agent));
            let session = self
                .sessions
                .create(
                    &self.context,
                    &CreateInput {
                        parent_id: Some(request.parent_session_id.clone()),
                        title: Some(request.title.clone()),
                        agent: Some(request.agent.clone()),
                        permission: Some(rules_into(permission)),
                        metadata: Some(metadata),
                        ..Default::default()
                    },
                )
                .map_err(|error| error.to_string())?;
            Ok(session.id)
        })
    }

    fn prompt_agent<'a>(
        &'a self,
        session_id: &'a str,
        agent: &'a str,
        prompt: &'a str,
        schema: Option<&'a Value>,
    ) -> BoxFuture<'a, Result<WorkflowAgentOutcome, String>> {
        Box::pin(async move {
            let parts = self
                .facade()?
                .resolve_prompt_parts(prompt)
                .await
                .map_err(|error| error.to_string())?;
            self.run_prompt(session_id, agent, parts, schema).await
        })
    }

    fn resume_agent<'a>(
        &'a self,
        session_id: &'a str,
        agent: &'a str,
        note: &'a str,
        schema: Option<&'a Value>,
    ) -> BoxFuture<'a, Result<WorkflowAgentOutcome, String>> {
        Box::pin(async move {
            let parts = vec![PromptPartInput::Text {
                id: None,
                text: note.to_string(),
                synthetic: Some(true),
                ignored: None,
                time: None,
                metadata: None,
            }];
            self.run_prompt(session_id, agent, parts, schema).await
        })
    }

    fn cancel_agent<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ()> {
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

    fn is_workflow_agent(&self, session_id: &str) -> bool {
        let mut current = self.sessions.get(session_id).ok();
        while let Some(session) = current {
            let marked = session
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("workflowAgent"));
            if marked {
                return true;
            }
            current = session
                .parent_id
                .and_then(|parent| self.sessions.get(&parent).ok());
        }
        false
    }
}

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
    use crate::session::test_support::{create_engine_session, engine};

    #[tokio::test]
    async fn agents_follow_the_model_of_the_parents_latest_turn() {
        let e = engine("workflow-parent-model", Vec::new());
        let session = create_engine_session(&e);
        let context = e.services.instance_context(&e.worktree, None).unwrap();
        let ops = ProductionWorkflowOps::new(
            e.services.sessions.clone(),
            e.services.messages.clone(),
            context,
            e.services.agents.clone(),
            Vec::new(),
        );
        assert!(ops.session_model(&session.id).0.is_none());
        for model_id in ["first-model", "second-model"] {
            e.prompt
                .prompt(PromptInput {
                    session_id: session.id.clone(),
                    message_id: None,
                    model: Some(ModelRef {
                        provider_id: "anthropic".to_string(),
                        model_id: model_id.to_string(),
                    }),
                    agent: Some("build".to_string()),
                    no_reply: Some(true),
                    tools: None,
                    format: None,
                    system: None,
                    variant: None,
                    parts: vec![PromptPartInput::Text {
                        id: None,
                        text: "hello".to_string(),
                        synthetic: None,
                        ignored: None,
                        time: None,
                        metadata: None,
                    }],
                })
                .await
                .unwrap();
        }
        let (model, _) = ops.session_model(&session.id);
        assert_eq!(model.unwrap().model_id, "second-model");
    }
}
