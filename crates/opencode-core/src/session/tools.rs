//! Session tools — port of `session/tools.ts` `SessionTools.resolve`
//! (tools.ts:41-138), reduced to the registry tools and their context
//! adapter.
//!
//! The MCP resource tools (tools.ts:140-380) need the MCP service (M6)
//! and are not ported; `ProviderTransform.schema` (ai-sdk
//! provider-compat transforms) is not ported either — hand-authored
//! schemas pass through unchanged (documented divergence, spec §2.6).
//! The M4 registry binds `TaskOps` at construction, so `promptOps` has no
//! per-resolution wiring here.

use std::sync::Arc;

use opencode_schema::permission_v1::PermissionV1Ruleset;
use opencode_schema::session_v1::{V1Part, V1SessionInfo, V1ToolState};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::session::agents::AgentInfo;
use crate::session::error::SessionError;
use crate::session::llm::{LlmModel, LlmTool, LlmToolOutput};
use crate::session::message::WithParts;
use crate::session::processor::{
    AskPermission, Handle as ProcessorHandle, PermissionAsk, PermissionAskError, PermissionAskTool,
    ToolCallOutput,
};
use crate::tool::def::{
    AgentInfo as ToolAgentInfo, Ask, AskRequest, Attachment, Extra, InstanceContext, MetadataInput,
    MetadataSink, ToolCtxRef,
};
use crate::tool::registry::{ToolModel, ToolRegistry};
use crate::tool::task::TaskOps;
use crate::Clock;

/// `resolve` input (tools.ts:41-50). Borrows the loop's live state; the
/// returned [`LlmTool`]s are `'static` closures holding clones.
pub struct ResolveInput<'a> {
    pub agent: &'a AgentInfo,
    pub model: &'a LlmModel,
    pub session: &'a V1SessionInfo,
    pub processor: &'a ProcessorHandle,
    /// `msgs` from the current loop step.
    pub messages: &'a [WithParts],
    pub bypass_agent_check: bool,
    pub prompt_ops: Arc<dyn TaskOps>,
    /// The loop's abort signal (TS `options.abortSignal`).
    pub cancel: CancellationToken,
}

pub struct ResolveDeps {
    pub registry: ToolRegistry,
    pub permission: Arc<dyn AskPermission>,
    pub instance: InstanceContext,
    pub clock: Arc<dyn Clock>,
}

/// Resolved session tools: `Record<string, AITool>` keyed by name.
pub type ResolvedTools = Vec<LlmTool>;

/// `SessionTools.resolve` (tools.ts:41-138): registry tools wired to the
/// processor handle and the permission seam.
pub async fn resolve(
    deps: &ResolveDeps,
    input: ResolveInput<'_>,
) -> Result<ResolvedTools, SessionError> {
    let tool_agent = ToolAgentInfo {
        name: input.agent.name.clone(),
        description: input.agent.description.clone(),
        mode: input.agent.mode,
        permission: input.agent.permission.clone(),
    };
    let defs = deps
        .registry
        .tools(ToolModel {
            provider_id: &input.model.provider_id,
            model_id: &input.model.id,
            agent: tool_agent,
            permission: input.session.permission.as_ref(),
        })
        .await;

    let session_id = input.session.id.clone();
    let message_id = match input.processor.message() {
        opencode_schema::session_v1::V1Message::User { id, .. }
        | opencode_schema::session_v1::V1Message::Assistant { id, .. } => id,
    }
    .to_string();
    let ruleset = {
        let mut merged = vec![input.agent.permission.as_ref()];
        if let Some(permission) = input.session.permission.as_ref() {
            merged.push(permission);
        }
        crate::tool::permission::merge(&merged)
    };
    let messages: Vec<Value> = input
        .messages
        .iter()
        .map(|msg| {
            serde_json::json!({
                "info": msg.info,
                "parts": msg.parts,
            })
        })
        .collect();
    let model_value = serde_json::to_value(input.model.clone()).unwrap_or(Value::Null);

    let mut tools: ResolvedTools = Vec::new();
    for tool in defs {
        let processor = input.processor.clone();
        let permission = deps.permission.clone();
        let instance = InstanceContext {
            directory: deps.instance.directory.clone(),
            worktree: deps.instance.worktree.clone(),
        };
        let clock = deps.clock.clone();
        let agent_name = input.agent.name.clone();
        let cancel = input.cancel.clone();
        let bypass_agent_check = input.bypass_agent_check;
        let messages = messages.clone();
        let ruleset = ruleset.clone();
        let model_value = model_value.clone();
        let session_id = session_id.clone();
        let message_id = message_id.clone();

        tools.push(LlmTool {
            name: tool.id.to_string(),
            description: tool.description.to_string(),
            input_schema: tool.parameters.clone(),
            execute: Arc::new(move |args: Value, call_id: String| {
                // The closure is `Fn` (callable repeatedly); clone the
                // captures the `async move` block takes ownership of.
                let tool = tool.clone();
                let processor = processor.clone();
                let permission = permission.clone();
                let instance = instance.clone();
                let clock = clock.clone();
                let agent_name = agent_name.clone();
                let cancel = cancel.clone();
                let bypass_agent_check = bypass_agent_check;
                let messages = messages.clone();
                let ruleset = ruleset.clone();
                let model_value = model_value.clone();
                let session_id = session_id.clone();
                let message_id = message_id.clone();
                Box::pin(async move {
                    let ask = PermissionAdapter {
                        permission: permission.clone(),
                        session_id: session_id.clone(),
                        message_id: message_id.clone(),
                        call_id: call_id.clone(),
                        ruleset: ruleset.clone(),
                    };
                    let sink = MetadataAdapter {
                        processor: processor.clone(),
                        call_id: call_id.clone(),
                        args: args.clone(),
                        clock: clock.clone(),
                    };
                    let extra = Extra {
                        bypass_cwd_check: false,
                        bypass_agent_check,
                        model: Some(model_value.clone()),
                    };
                    let ctx = ToolCtxRef {
                        session_id: &session_id,
                        message_id: &message_id,
                        agent: &agent_name,
                        call_id: Some(&call_id),
                        abort: cancel.clone(),
                        messages: &messages,
                        extra: &extra,
                        instance: &instance,
                        ask: &ask,
                        metadata: &sink,
                    };
                    let result = match (tool.execute)(args, ctx).await {
                        Ok(result) => result,
                        // TS: the native runtime surfaces tool failures as
                        // a ToolFailure value carrying the error class
                        // (native-runtime.ts:188).
                        Err(error) => {
                            return Err(match error {
                                crate::tool::error::ToolError::Rejected(message) => {
                                    crate::session::llm::ToolFailure::Rejected(message)
                                }
                                other => {
                                    crate::session::llm::ToolFailure::Message(other.to_string())
                                }
                            })
                        }
                    };
                    let output = LlmToolOutput {
                        title: result.title,
                        metadata: result.metadata,
                        output: result.output,
                        attachments: result.attachments.map(|attachments| {
                            attachments
                                .into_iter()
                                .map(|attachment| {
                                    attachment_value(&attachment, &session_id, &message_id)
                                })
                                .collect::<Vec<Value>>()
                        }),
                    };
                    // TS: aborted mid-execution — the processor will not
                    // see the tool-result event, complete it here
                    // (tools.ts:125-127).
                    if cancel.is_cancelled() {
                        let _ = processor
                            .complete_tool_call(
                                &call_id,
                                ToolCallOutput {
                                    title: output.title.clone(),
                                    metadata: output.metadata.clone(),
                                    output: output.output.clone(),
                                    attachments: None,
                                },
                            )
                            .await;
                    }
                    Ok(output)
                })
            }),
        });
    }
    Ok(tools)
}

fn attachment_value(attachment: &Attachment, session_id: &str, message_id: &str) -> Value {
    let part = opencode_schema::session_v1::V1FilePart::File {
        id: crate::session::ids::PartId::ascending(None).expect("part id generation"),
        session_id: session_id.to_string(),
        message_id: message_id.to_string(),
        mime: attachment.mime.clone(),
        filename: attachment.filename.clone(),
        url: attachment.url.clone(),
        source: None,
    };
    serde_json::to_value(part).unwrap_or(Value::Null)
}

/// `ctx.ask` (tools.ts:93-101): bound to the session permission seam.
struct PermissionAdapter {
    permission: Arc<dyn AskPermission>,
    session_id: String,
    message_id: String,
    call_id: String,
    ruleset: PermissionV1Ruleset,
}

impl Ask for PermissionAdapter {
    fn ask<'a>(
        &'a self,
        request: AskRequest,
    ) -> crate::tool::def::BoxFuture<'a, Result<(), crate::tool::error::ToolError>> {
        Box::pin(async move {
            self.permission
                .ask(PermissionAsk {
                    session_id: self.session_id.clone(),
                    permission: request.permission,
                    patterns: request.patterns,
                    always: request.always,
                    metadata: request.metadata,
                    ruleset: self.ruleset.clone(),
                    tool: PermissionAskTool {
                        message_id: self.message_id.clone(),
                        call_id: self.call_id.clone(),
                    },
                })
                .await
                .map_err(map_permission_error)
        })
    }
}

fn map_permission_error(error: PermissionAskError) -> crate::tool::error::ToolError {
    match error {
        PermissionAskError::Rejected(message) => crate::tool::error::ToolError::Rejected(message),
        PermissionAskError::Other(message) => crate::tool::error::ToolError::Permission(message),
    }
}

/// `ctx.metadata` (tools.ts:71-92): title/metadata updates on the live
/// tool part.
struct MetadataAdapter {
    processor: ProcessorHandle,
    call_id: String,
    args: Value,
    clock: Arc<dyn Clock>,
}

impl MetadataSink for MetadataAdapter {
    fn metadata<'a>(
        &'a self,
        input: MetadataInput,
    ) -> crate::tool::def::BoxFuture<'a, Result<(), crate::tool::error::ToolError>> {
        Box::pin(async move {
            let title = input.title;
            let metadata = input.metadata;
            let args = self.args.clone();
            let now = self.clock.now_ms();
            self.processor
                .update_tool_call(&self.call_id, move |part| {
                    update_running_state(part, title, metadata.as_ref(), &args, now)
                })
                .await
                .map(|_| ())
                .map_err(|error| crate::tool::error::ToolError::Failed(error.to_string()))
        })
    }
}

/// The metadata state merge (tools.ts:77-91): only running/pending parts
/// transition to running with the new title/metadata.
fn update_running_state(
    part: V1Part,
    title: Option<String>,
    metadata: Option<&Value>,
    args: &Value,
    now: u64,
) -> V1Part {
    let V1Part::Tool {
        id,
        session_id,
        message_id,
        call_id,
        tool,
        state,
        metadata: part_metadata,
    } = part
    else {
        return part;
    };
    if !matches!(
        state,
        V1ToolState::Running { .. } | V1ToolState::Pending { .. }
    ) {
        return V1Part::Tool {
            id,
            session_id,
            message_id,
            call_id,
            tool,
            state,
            metadata: part_metadata,
        };
    }
    // `title: val.title, metadata: val.metadata` — written through
    // unmodified (tools.ts:70-78); absent keys stay absent.
    let metadata = metadata.map(json_map);
    let state = match state {
        V1ToolState::Running { time, .. } => V1ToolState::Running {
            input: json_map(args),
            title,
            metadata,
            time,
        },
        _ => V1ToolState::Running {
            input: json_map(args),
            title,
            metadata,
            time: opencode_schema::session_v1::ToolStateRunningTime { start: now },
        },
    };
    V1Part::Tool {
        id,
        session_id,
        message_id,
        call_id,
        tool,
        state,
        metadata: part_metadata,
    }
}

fn json_map(value: &Value) -> opencode_schema::schema::JsonMap {
    match value {
        Value::Object(map) => map.clone(),
        other => {
            let mut map = opencode_schema::schema::JsonMap::new();
            map.insert("value".to_string(), other.clone());
            map
        }
    }
}

/// The `SessionTools` namespace (TS `SessionTools.resolve`).
pub struct SessionTools;

impl SessionTools {
    pub async fn resolve(
        deps: &ResolveDeps,
        input: ResolveInput<'_>,
    ) -> Result<ResolvedTools, SessionError> {
        resolve(deps, input).await
    }
}
