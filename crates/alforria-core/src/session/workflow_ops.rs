//! Production [`WorkflowOps`] — child sessions + the prompt facade.
//!
//! A workflow run is a normal parent session; each `agent()` call creates a
//! child session and prompts it through the same engine as the `task` tool.
//! The facade is shared, so an agent's transcript is a first-class session the
//! web client can open.

use std::sync::{Arc, Mutex};

use alforria_schema::schema::JsonMap;
use alforria_schema::session_v1::{AssistantError, V1Message, V1Part, V1ToolState};
use serde_json::json;

use crate::session::agents::AgentRegistry;
use crate::session::prompt_input::PromptInput;
use crate::session::store::{CreateInput, SessionContext, SessionStore};
use crate::session::task_ops::PromptFacade;
use crate::tool::def::BoxFuture;
use crate::workflow::{WorkflowAgentOutcome, WorkflowAgentRequest, WorkflowOps};

/// The session-engine implementation of [`WorkflowOps`].
pub struct ProductionWorkflowOps {
    sessions: SessionStore,
    context: SessionContext,
    agents: AgentRegistry,
    facade: Mutex<Option<Arc<dyn PromptFacade>>>,
}

impl ProductionWorkflowOps {
    pub fn new(
        sessions: SessionStore,
        context: SessionContext,
        agents: AgentRegistry,
    ) -> Arc<Self> {
        Arc::new(ProductionWorkflowOps {
            sessions,
            context,
            agents,
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
            if self.agents.get(&request.agent).is_none() {
                return Err(format!("unknown workflow agent: {}", request.agent));
            }
            let permission = self
                .sessions
                .get(&request.parent_session_id)
                .ok()
                .and_then(|session| session.permission.clone());
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
                        permission,
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
    ) -> BoxFuture<'a, Result<WorkflowAgentOutcome, String>> {
        Box::pin(async move {
            let facade = self.facade()?;
            let parts = facade
                .resolve_prompt_parts(prompt)
                .await
                .map_err(|error| error.to_string())?;
            let input = PromptInput {
                session_id: session_id.to_string(),
                message_id: Some(
                    crate::session::ids::MessageId::ascending(None)
                        .map_err(|error| error.to_string())?,
                ),
                model: None,
                agent: Some(agent.to_string()),
                no_reply: None,
                tools: None,
                format: None,
                system: None,
                variant: None,
                parts,
            };
            let result = facade
                .prompt(input)
                .await
                .map_err(|error| error.to_string())?;
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
            Ok(WorkflowAgentOutcome { text, error })
        })
    }

    fn cancel_agent<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Ok(facade) = self.facade() {
                let _ = facade.cancel(session_id).await;
            }
        })
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
