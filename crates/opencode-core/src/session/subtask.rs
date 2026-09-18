//! Subagent task driver — port of `handleSubtask`
//! (prompt.ts:255-449) and `agent/subagent-permissions.ts`
//! (the production data path: the parent session permission feeding
//! `deriveSubagentSessionPermission`, which the M4 task tool owns).

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use opencode_schema::session_v1::{
    AssistantTime, ToolStateCompletedTime, ToolStateErrorTime, ToolStateRunningTime, V1FilePart,
    V1Message, V1Part, V1StepTokens, V1ToolState,
};
use serde_json::{json, Value};
use ulid::Ulid;

use crate::event::bus::{EventBus, PublishOptions};
use crate::session::agents::AgentRegistry;
use crate::session::error::SessionError;
use crate::session::event_definitions::SESSION_ERROR;
use crate::session::ids::{MessageId, PartId};
use crate::session::permission::SessionAsk;
use crate::session::r#loop::{LoopError, ModelSource, SubtaskInput, Subtasks};
use crate::session::store::SessionStore;
use crate::tool::def::{Attachment, Extra, MetadataInput, MetadataSink, ToolCtxRef};
use crate::tool::registry::ToolRegistry;
use crate::Clock;

/// Everything `handleSubtask` closes over (the Effect layer services).
pub struct SubtaskDeps {
    pub sessions: SessionStore,
    pub events: Arc<EventBus>,
    pub agents: AgentRegistry,
    /// `provider.getModel` (the loop's model seam).
    pub models: Arc<dyn ModelSource>,
    /// The registry the `task` tool def resolves from.
    pub registry: ToolRegistry,
    pub permission: Arc<crate::session::permission::PermissionService>,
    pub clock: Arc<dyn Clock>,
    pub instance: crate::tool::def::InstanceContext,
}

impl SubtaskDeps {
    fn publish_error(&self, session_id: &str, message: &str) -> Result<(), SessionError> {
        self.events.publish(
            &SESSION_ERROR,
            serde_json::json!({
                "sessionID": session_id,
                "error": { "name": "Unknown", "data": { "message": message } },
            }),
            PublishOptions::default(),
        )?;
        Ok(())
    }
}

/// The production [`Subtasks`] driver — `handleSubtask` (prompt.ts:255-449).
pub struct SessionSubtask {
    deps: SubtaskDeps,
}

impl SessionSubtask {
    pub fn new(deps: SubtaskDeps) -> SessionSubtask {
        SessionSubtask { deps }
    }

    async fn handle_subtask(&self, input: SubtaskInput) -> Result<(), LoopError> {
        let V1Part::Subtask {
            prompt: task_prompt,
            description: task_description,
            agent: task_agent_name,
            model: task_model_ref,
            command: task_command,
            ..
        } = &input.task
        else {
            return Ok(());
        };

        // `task.model ? getModel(...) : model` (prompt.ts:270).
        let model = match task_model_ref {
            Some(model) => {
                self.deps
                    .models
                    .get_model(&model.provider_id, &model.model_id, &input.session_id)
                    .await?
            }
            None => input.model.clone(),
        };

        // The assistant message for the subtask turn (prompt.ts:272-289).
        let (last_user_id, last_user_model) = match &input.last_user {
            V1Message::User { id, model, .. } => (id.clone(), model.clone()),
            _ => (
                String::new(),
                opencode_schema::session_v1::V1UserModel {
                    provider_id: String::new(),
                    model_id: String::new(),
                    variant: None,
                },
            ),
        };
        let now = self.deps.clock.now_ms();
        let assistant = V1Message::Assistant {
            id: MessageId::ascending(None)?,
            session_id: input.session_id.clone(),
            time: AssistantTime {
                created: now,
                completed: None,
            },
            error: None,
            parent_id: last_user_id,
            model_id: model.llm.id.clone(),
            provider_id: model.llm.provider_id.clone(),
            mode: task_agent_name.clone(),
            agent: task_agent_name.clone(),
            path: opencode_schema::session_v1::V1Path {
                cwd: self.deps.instance.directory.to_string_lossy().into_owned(),
                root: self.deps.instance.worktree.to_string_lossy().into_owned(),
            },
            summary: None,
            cost: 0.0,
            tokens: empty_tokens(),
            structured: None,
            variant: last_user_model.variant.clone(),
            finish: None,
        };
        self.deps.sessions.update_message(&assistant)?;

        // The running `task` tool part (prompt.ts:291-314).
        let mut state_input = serde_json::Map::new();
        state_input.insert("prompt".to_string(), json!(task_prompt));
        state_input.insert("description".to_string(), json!(task_description));
        state_input.insert("subagent_type".to_string(), json!(task_agent_name));
        if let Some(command) = task_command {
            state_input.insert("command".to_string(), json!(command));
        }
        let part = V1Part::Tool {
            id: PartId::ascending(None)?,
            session_id: input.session_id.clone(),
            message_id: message_id(&assistant),
            tool: "task".to_string(),
            call_id: Ulid::new().to_string(),
            metadata: None,
            state: V1ToolState::Running {
                input: state_input,
                title: None,
                metadata: None,
                time: ToolStateRunningTime {
                    start: self.deps.clock.now_ms(),
                },
            },
        };
        self.deps.sessions.update_part(&part)?;
        let shared_part: Arc<Mutex<V1Part>> = Arc::new(Mutex::new(part));

        // `agents.get(task.agent)` — the task agent (prompt.ts:323-343).
        let Some(task_agent) = self.deps.agents.get(task_agent_name).cloned() else {
            let available: Vec<String> = self
                .deps
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
            let message = format!("Agent not found: \"{task_agent_name}\".{hint}");
            self.deps.publish_error(&input.session_id, &message)?;
            return Err(LoopError::Unknown(message));
        };

        // `taskTool.execute(taskArgs, {...})` (prompt.ts:345-380).
        let task_args = json!({
            "prompt": task_prompt,
            "description": task_description,
            "subagent_type": task_agent_name,
            "command": task_command,
        });
        let (task_tool, _) = self.deps.registry.named();
        let extra = Extra {
            bypass_cwd_check: false,
            bypass_agent_check: true,
            model: None,
        };
        let messages: Vec<Value> = input
            .messages
            .iter()
            .map(|msg| {
                json!({
                    "info": msg.info,
                    "parts": msg.parts,
                })
            })
            .collect();
        let ruleset = crate::tool::permission::merge(&[
            task_agent.permission.as_ref(),
            input.session.permission.as_ref().unwrap_or(&Vec::new()),
        ]);
        let ask = SessionAsk::new(
            self.deps.permission.clone(),
            input.session_id.clone(),
            message_id(&assistant),
            match &*shared_part.lock().unwrap() {
                V1Part::Tool { call_id, .. } => call_id.clone(),
                _ => unreachable!("just constructed as tool"),
            },
            ruleset,
        );
        let metadata_sink = SubtaskMetadata {
            sessions: self.deps.sessions.clone(),
            part: shared_part.clone(),
        };
        let session_id = input.session_id.clone();
        let instance = self.deps.instance.clone();
        let abort = input.cancel.clone();
        let assistant_id = message_id(&assistant);
        let agent_name = task_agent_name.clone();
        let call_id = match &*shared_part.lock().unwrap() {
            V1Part::Tool { call_id, .. } => call_id.clone(),
            _ => unreachable!("just constructed as tool"),
        };
        let sessions = self.deps.sessions.clone();
        let clock = self.deps.clock.clone();

        let result = tokio::select! {
            biased;
            _ = input.cancel.cancelled() => {
                // `Effect.onInterrupt` (prompt.ts:350-380): abort, finish
                // the assistant, mark a still-running part `Cancelled`.
                let mut finished = assistant.clone();
                if let V1Message::Assistant { finish, time, .. } = &mut finished {
                    *finish = Some("tool-calls".to_string());
                    time.completed = Some(clock.now_ms());
                }
                sessions.update_message(&finished)?;
                let current = shared_part.lock().unwrap().clone();
                if matches!(current, V1Part::Tool { state: V1ToolState::Running { .. }, .. }) {
                    let updated = interrupted_part(&current, "Cancelled".to_string(), clock.now_ms());
                    sessions.update_part(&updated)?;
                }
                return Err(LoopError::Cancelled);
            }
            result = (task_tool.execute)(task_args, ToolCtxRef {
                session_id: &session_id,
                message_id: &assistant_id,
                agent: &agent_name,
                call_id: Some(&call_id),
                abort,
                messages: &messages,
                extra: &extra,
                instance: &instance,
                ask: &ask,
                metadata: &metadata_sink,
            }) => result,
        };

        // Result handling (prompt.ts:382-449).
        let attachments: Option<Vec<V1FilePart>> = match &result {
            Ok(result) => result.attachments.as_ref().map(|attachments| {
                attachments
                    .iter()
                    .map(|attachment| {
                        attachment_file_part(attachment, &session_id, &message_id(&assistant))
                    })
                    .collect()
            }),
            Err(_) => None,
        };

        let mut finished = assistant.clone();
        if let V1Message::Assistant { finish, time, .. } = &mut finished {
            *finish = Some("tool-calls".to_string());
            time.completed = Some(self.deps.clock.now_ms());
        }
        self.deps.sessions.update_message(&finished)?;

        match result {
            Ok(result) => {
                let current = shared_part.lock().unwrap().clone();
                if matches!(
                    current,
                    V1Part::Tool {
                        state: V1ToolState::Running { .. },
                        ..
                    }
                ) {
                    self.deps.sessions.update_part(&completed_part(
                        &current,
                        result.title.clone(),
                        result.metadata.clone(),
                        result.output.clone(),
                        attachments,
                        self.deps.clock.now_ms(),
                    ))?;
                }
            }
            Err(error) => {
                let current = shared_part.lock().unwrap().clone();
                let message = error.to_string();
                let message = if message.is_empty() {
                    "Tool execution failed".to_string()
                } else {
                    format!("Tool execution failed: {message}")
                };
                self.deps.sessions.update_part(&failed_part(
                    &current,
                    message,
                    self.deps.clock.now_ms(),
                ))?;
            }
        }

        // `if (!task.command) return` (prompt.ts:430-448).
        if task_command.is_none() {
            return Ok(());
        }

        // The synthetic summary user message.
        let summary_user = V1Message::User {
            id: MessageId::ascending(None)?,
            session_id: input.session_id.clone(),
            time: opencode_schema::session_v1::UserTime {
                created: self.deps.clock.now_ms() as f64,
            },
            format: None,
            summary: None,
            agent: match &input.last_user {
                V1Message::User { agent, .. } => agent.clone(),
                _ => String::new(),
            },
            model: last_user_model,
            system: None,
            tools: None,
        };
        self.deps.sessions.update_message(&summary_user)?;
        self.deps.sessions.update_part(&V1Part::Text {
            id: PartId::ascending(None)?,
            session_id: input.session_id.clone(),
            message_id: message_id(&summary_user),
            text: "Summarize the task tool output above and continue with your task.".to_string(),
            synthetic: Some(true),
            ignored: None,
            time: None,
            metadata: None,
        })?;

        Ok(())
    }
}

impl Subtasks for SessionSubtask {
    fn handle<'a>(&'a self, input: SubtaskInput) -> BoxFuture<'a, Result<(), LoopError>> {
        Box::pin(async move { self.handle_subtask(input).await })
    }
}

/// The metadata sink (prompt.ts:333-338): `{...part, state: {...part.state,
/// ...val}}` — re-publish the (shared) part with the update applied.
struct SubtaskMetadata {
    sessions: SessionStore,
    part: Arc<Mutex<V1Part>>,
}

impl MetadataSink for SubtaskMetadata {
    fn metadata<'a>(
        &'a self,
        input: MetadataInput,
    ) -> BoxFuture<'a, Result<(), crate::tool::error::ToolError>> {
        Box::pin(async move {
            let current = self.part.lock().unwrap().clone();
            let V1Part::Tool {
                state:
                    V1ToolState::Running {
                        input: state_input,
                        title,
                        metadata,
                        time,
                    },
                ..
            } = &current
            else {
                return Ok(());
            };
            let updated_state = V1ToolState::Running {
                input: state_input.clone(),
                title: input.title.or_else(|| title.clone()),
                metadata: match input.metadata {
                    Some(metadata) => Some(json_map(&metadata)),
                    None => metadata.clone(),
                },
                time: *time,
            };
            let updated = match_tool_state(&current, updated_state);
            self.sessions
                .update_part(&updated)
                .map_err(|err| crate::tool::error::ToolError::Failed(err.to_string()))?;
            *self.part.lock().unwrap() = updated;
            Ok(())
        })
    }
}

/// `{...part, state}` — rebuild a tool part with a new state (the TS
/// object spread).
fn match_tool_state(part: &V1Part, state: V1ToolState) -> V1Part {
    match part {
        V1Part::Tool {
            id,
            session_id,
            message_id,
            tool,
            call_id,
            metadata,
            ..
        } => V1Part::Tool {
            id: id.clone(),
            session_id: session_id.clone(),
            message_id: message_id.clone(),
            tool: tool.clone(),
            call_id: call_id.clone(),
            metadata: metadata.clone(),
            state,
        },
        _ => part.clone(),
    }
}

/// `{status: "completed", input, title, metadata, output, attachments,
/// time}` (prompt.ts:404-420).
fn completed_part(
    part: &V1Part,
    title: String,
    metadata: Value,
    output: String,
    attachments: Option<Vec<V1FilePart>>,
    end: u64,
) -> V1Part {
    let V1Part::Tool { state, .. } = part else {
        return part.clone();
    };
    let V1ToolState::Running { input, time, .. } = state else {
        return part.clone();
    };
    match_tool_state(
        part,
        V1ToolState::Completed {
            input: input.clone(),
            output,
            title,
            metadata: json_map(&metadata),
            time: ToolStateCompletedTime {
                start: time.start,
                end,
                compacted: None,
            },
            attachments,
        },
    )
}

/// The interrupted part (prompt.ts:357-373): `{status: "error", error:
/// "Cancelled", time: {start, end}, metadata, input}`.
fn interrupted_part(part: &V1Part, error: String, now: u64) -> V1Part {
    let V1Part::Tool { state, .. } = part else {
        return part.clone();
    };
    let V1ToolState::Running {
        input,
        metadata,
        time,
        ..
    } = state
    else {
        return part.clone();
    };
    match_tool_state(
        part,
        V1ToolState::Error {
            input: input.clone(),
            error,
            metadata: metadata.clone(),
            time: ToolStateErrorTime {
                start: time.start,
                end: now,
            },
        },
    )
}

/// The `!result` error part (prompt.ts:422-428): start falls back to now
/// unless the part was still running; pending parts drop their metadata.
fn failed_part(part: &V1Part, error: String, now: u64) -> V1Part {
    let V1Part::Tool { state, .. } = part else {
        return part.clone();
    };
    // Any non-result state converts; `start` falls back to now unless the
    // part was still running, and pending parts drop their metadata
    // (prompt.ts:413-428).
    let (input, metadata, start) = match state {
        V1ToolState::Running {
            input,
            metadata,
            time,
            ..
        } => (input.clone(), metadata.clone(), time.start),
        V1ToolState::Pending { input, .. } => (input.clone(), None, now),
        _ => return part.clone(),
    };
    match_tool_state(
        part,
        V1ToolState::Error {
            input,
            error,
            metadata,
            time: ToolStateErrorTime { start, end: now },
        },
    )
}

fn attachment_file_part(attachment: &Attachment, session_id: &str, message_id: &str) -> V1FilePart {
    V1FilePart::File {
        id: PartId::ascending(None).expect("valid id"),
        session_id: session_id.to_string(),
        message_id: message_id.to_string(),
        mime: attachment.mime.clone(),
        filename: attachment.filename.clone(),
        url: attachment.url.clone(),
        source: None,
    }
}

/// Non-object metadata wraps as `{value: v}` (the permission service
/// convention).
fn json_map(value: &Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(map) => map.clone(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("value".to_string(), other.clone());
            map
        }
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
        cache: opencode_schema::session_v1::V1TokenCache {
            read: 0.0,
            write: 0.0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::r#loop::Subtasks;
    use crate::session::test_support::{create_engine_session, engine, test_model, text_stream};
    use opencode_schema::session_v1::{V1Part, V1SubtaskModel};
    use tokio_util::sync::CancellationToken;

    fn subtask_part(session: &str, message: &str, agent: &str, command: Option<String>) -> V1Part {
        V1Part::Subtask {
            id: "par_01M2SUBTASKPART000000000000".to_string(),
            session_id: session.to_string(),
            message_id: message.to_string(),
            prompt: "do the subtask".to_string(),
            description: "subtask description".to_string(),
            agent: agent.to_string(),
            model: None,
            command,
        }
    }

    fn driver(e: &crate::session::test_support::Engine) -> SessionSubtask {
        SessionSubtask::new(SubtaskDeps {
            sessions: e.services.sessions.clone(),
            events: e.services.events.clone(),
            agents: e.services.agents.clone(),
            models: e.models.clone(),
            registry: e.registry.clone(),
            permission: e.services.permission.clone(),
            clock: Arc::new(crate::session::test_support::FixedClock),
            instance: e.instance.clone(),
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_subtask_runs_task_and_completes_part() {
        let e = engine("subtask-handle", vec![text_stream("subagent answer")]);
        let session = create_engine_session(&e);
        let user = crate::session::test_support::user_message(&session.id, "msg_p1", 1.0);
        e.services.sessions.update_message(&user).unwrap();

        let part = subtask_part(&session.id, "msg_p1", "build", None);
        driver(&e)
            .handle(crate::session::r#loop::SubtaskInput {
                task: part,
                model: test_model(),
                last_user: user.clone(),
                session_id: session.id.clone(),
                session: session.clone(),
                messages: vec![],
                cancel: CancellationToken::new(),
            })
            .await
            .unwrap();

        // The task tool part completed with the subagent's answer.
        let messages = e.services.messages.stream(&session.id).unwrap();
        let completed = messages
            .iter()
            .flat_map(|msg| msg.parts.iter())
            .find_map(|part| match part {
                V1Part::Tool {
                    tool,
                    state: V1ToolState::Completed { output, .. },
                    ..
                } if tool == "task" => Some(output),
                _ => None,
            })
            .expect("completed task part");
        // One nested LLM call through the engine's scripted dispatch.
        assert_eq!(e.llm.calls(), 1);
        assert_eq!(e.llm.input(0).model.id, "claude");
        assert!(
            completed.contains("state=\"completed\""),
            "expected completed state: {completed}"
        );
        assert!(
            completed.contains("subagent answer"),
            "expected subagent answer: {completed}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_subtask_unknown_agent_publishes_error() {
        let e = engine("subtask-unknown-agent", vec![]);
        let session = create_engine_session(&e);
        let user = crate::session::test_support::user_message(&session.id, "msg_p1", 1.0);
        e.services.sessions.update_message(&user).unwrap();

        let part = subtask_part(&session.id, "msg_p1", "nope", None);
        let error = driver(&e)
            .handle(crate::session::r#loop::SubtaskInput {
                task: part,
                model: test_model(),
                last_user: user.clone(),
                session_id: session.id.clone(),
                session: session.clone(),
                messages: vec![],
                cancel: CancellationToken::new(),
            })
            .await
            .unwrap_err();
        match error {
            crate::session::r#loop::LoopError::Unknown(message) => {
                assert!(message.starts_with("Agent not found"), "{message}");
            }
            _ => panic!("expected Unknown error"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handle_subtask_with_command_appends_summary_user() {
        let e = engine("subtask-command", vec![text_stream("subagent answer")]);
        let session = create_engine_session(&e);
        let user = crate::session::test_support::user_message(&session.id, "msg_p1", 1.0);
        e.services.sessions.update_message(&user).unwrap();

        let part = subtask_part(&session.id, "msg_p1", "build", Some("build".to_string()));
        driver(&e)
            .handle(crate::session::r#loop::SubtaskInput {
                task: part,
                model: test_model(),
                last_user: user.clone(),
                session_id: session.id.clone(),
                session: session.clone(),
                messages: vec![],
                cancel: CancellationToken::new(),
            })
            .await
            .unwrap();

        // The synthetic summary user message (prompt.ts:430-448).
        let messages = e.services.messages.stream(&session.id).unwrap();
        let synthetic = messages
            .iter()
            .flat_map(|msg| msg.parts.iter())
            .any(|part| {
                matches!(
                    part,
                    V1Part::Text {
                        synthetic: Some(true),
                        text,
                        ..
                    } if text.starts_with("Summarize the task tool output")
                )
            });
        assert!(synthetic, "expected synthetic summary user message");
    }

    #[test]
    fn subtask_part_model_override() {
        let model = V1SubtaskModel {
            model_id: "claude".to_string(),
            provider_id: "anthropic".to_string(),
        };
        assert_eq!(model.model_id, "claude");
    }
}
