//! Session tools — port of `session/tools.ts` `SessionTools.resolve`
//! (tools.ts:41-138), reduced to the registry tools and their context
//! adapter.
//!
//! The MCP resource tools (tools.ts:140-380) need the MCP service and are
//! not ported; the MCP tool loop (tools.ts:389-482) goes through the
//! [`McpToolSource`] seam. `ProviderTransform.schema` (ai-sdk
//! provider-compat transforms) is not ported either — hand-authored
//! schemas pass through unchanged (documented divergence, spec §2.6).
//! The M4 registry binds `TaskOps` at construction, so `promptOps` has no
//! per-resolution wiring here.

use std::sync::Arc;

use alforria_schema::permission_v1::PermissionV1Ruleset;
use alforria_schema::session_v1::{V1Part, V1SessionInfo, V1ToolState};
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
use crate::tool::truncate::Options as TruncateOptions;
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
    /// TS `yield* mcp.tools()` — the engine's MCP tool seam
    /// (tools.ts:389). `None` in unit tests.
    pub mcp: Option<Arc<dyn McpToolSource>>,
    /// `truncate.output` (tools.ts:463) applied to MCP tool output.
    pub truncate: Arc<dyn crate::tool::truncate::Truncate>,
}

/// The engine's MCP tool seam — one entry per `{client}_{tool}` from the
/// connected MCP servers (TS `mcp.tools()`, tools.ts:390).
pub trait McpToolSource: Send + Sync {
    fn mcp_tools<'a>(&'a self) -> crate::tool::def::BoxFuture<'a, Vec<McpToolEntry>>;
}

/// One MCP tool exposed to the model.
pub struct McpToolEntry {
    /// `{sanitized client}_{tool}` (`McpCatalog.toolName`).
    pub key: String,
    pub description: String,
    pub input_schema: Value,
    /// Runs `tools/call`; returns the raw content array plus the
    /// call-level metadata (`catalog.ts:50-81`).
    pub execute: McpToolExecute,
}

pub type McpToolExecute = Arc<
    dyn Fn(Value) -> crate::tool::def::BoxFuture<'static, Result<McpRawResult, String>>
        + Send
        + Sync,
>;

/// The raw `tools/call` result fields the session maps onto tool output
/// (`catalog.ts:53-81`, `tools.ts:445-470`).
pub struct McpRawResult {
    pub content: Vec<Value>,
    pub structured_content: Option<Value>,
    pub metadata: Value,
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
        alforria_schema::session_v1::V1Message::User { id, .. }
        | alforria_schema::session_v1::V1Message::Assistant { id, .. } => id,
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
                    // see the tool-result event, complete it here with the
                    // full output, attachments included (tools.ts:121-127).
                    if cancel.is_cancelled() {
                        let attachments = output.attachments.clone().map(|parts| {
                            parts
                                .iter()
                                .filter_map(|part| {
                                    serde_json::from_value::<
                                        alforria_schema::session_v1::V1FilePart,
                                    >(part.clone())
                                    .ok()
                                })
                                .collect()
                        });
                        let _ = processor
                            .complete_tool_call(
                                &call_id,
                                ToolCallOutput {
                                    title: output.title.clone(),
                                    metadata: output.metadata.clone(),
                                    output: output.output.clone(),
                                    attachments,
                                },
                            )
                            .await;
                    }
                    Ok(output)
                })
            }),
        });
    }

    // MCP tools (tools.ts:389-482) — one LLM tool per connected-server
    // tool, keyed `{sanitized client}_{tool}`.
    if let Some(mcp) = deps.mcp.clone() {
        for entry in mcp.mcp_tools().await {
            let entry = Arc::new(entry);
            let permission = deps.permission.clone();
            let ruleset = ruleset.clone();
            let session_id = session_id.clone();
            let message_id = message_id.clone();
            let agent = ToolAgentInfo {
                name: input.agent.name.clone(),
                description: input.agent.description.clone(),
                mode: input.agent.mode,
                permission: input.agent.permission.clone(),
            };
            let agent = crate::tool::def::AgentInfo {
                name: agent.name,
                description: agent.description,
                mode: agent.mode,
                permission: agent.permission,
            };
            let cancel = input.cancel.clone();
            let truncate = deps.truncate.clone();
            let processor = input.processor.clone();
            let key = entry.key.clone();
            tools.push(LlmTool {
                name: entry.key.clone(),
                description: entry.description.clone(),
                input_schema: entry.input_schema.clone(),
                execute: Arc::new(move |args: Value, call_id: String| {
                    let entry = entry.clone();
                    let permission = permission.clone();
                    let ruleset = ruleset.clone();
                    let session_id = session_id.clone();
                    let message_id = message_id.clone();
                    let agent = agent.clone();
                    let cancel = cancel.clone();
                    let truncate = truncate.clone();
                    let processor = processor.clone();
                    let key = key.clone();
                    Box::pin(async move {
                        // TS: `ctx.ask({ permission: key, metadata: {},
                        // patterns: ["*"], always: ["*"] })`
                        // (tools.ts:422).
                        let ask = PermissionAdapter {
                            permission: permission.clone(),
                            session_id: session_id.clone(),
                            message_id: message_id.clone(),
                            call_id: call_id.clone(),
                            ruleset: ruleset.clone(),
                        };
                        ask.ask(AskRequest {
                            permission: key.clone(),
                            patterns: vec!["*".to_string()],
                            always: vec!["*".to_string()],
                            metadata: serde_json::json!({}),
                        })
                        .await
                        .map_err(|err| {
                            crate::session::llm::ToolFailure::Rejected(err.to_string())
                        })?;

                        let result = match (entry.execute)(args).await {
                            Ok(result) => result,
                            Err(message) => {
                                return Err(crate::session::llm::ToolFailure::Message(message))
                            }
                        };
                        // `structuredContent` fallback (catalog.ts:75-80):
                        // empty content with structured content renders as
                        // its JSON text.
                        let mut content = result.content;
                        if content.is_empty() {
                            match &result.structured_content {
                                Some(structured) if !structured.is_null() => {
                                    content = vec![serde_json::json!({
                                        "type": "text",
                                        "text": serde_json::to_string(structured)
                                            .unwrap_or_default(),
                                    })];
                                }
                                _ => {}
                            }
                        }
                        // Content mapping (tools.ts:445-460): text joins the
                        // output, image and resource blobs become
                        // attachments.
                        let mut text_parts: Vec<String> = Vec::new();
                        let mut attachments: Vec<Attachment> = Vec::new();
                        for item in &content {
                            match item.get("type").and_then(Value::as_str) {
                                Some("text") => {
                                    text_parts.push(
                                        item.get("text")
                                            .and_then(Value::as_str)
                                            .unwrap_or_default()
                                            .to_string(),
                                    );
                                }
                                Some("image") => {
                                    let mime = item
                                        .get("mimeType")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    let data = item
                                        .get("data")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    attachments.push(Attachment {
                                        kind: "file",
                                        mime: mime.to_string(),
                                        url: format!("data:{mime};base64,{data}"),
                                        filename: None,
                                    });
                                }
                                Some("resource") => {
                                    let Some(resource) = item.get("resource") else {
                                        continue;
                                    };
                                    if let Some(text) = resource.get("text").and_then(Value::as_str)
                                    {
                                        text_parts.push(text.to_string());
                                    }
                                    if let Some(blob) = resource.get("blob").and_then(Value::as_str)
                                    {
                                        let mime = resource
                                            .get("mimeType")
                                            .and_then(Value::as_str)
                                            .unwrap_or("application/octet-stream");
                                        attachments.push(Attachment {
                                            kind: "file",
                                            mime: mime.to_string(),
                                            url: format!("data:{mime};base64,{blob}"),
                                            filename: resource
                                                .get("uri")
                                                .and_then(Value::as_str)
                                                .map(str::to_string),
                                        });
                                    }
                                }
                                _ => {}
                            }
                        }
                        let truncated = truncate
                            .output(
                                &text_parts.join("\n\n"),
                                TruncateOptions::default(),
                                Some(&agent),
                            )
                            .await;
                        let output_content = match &truncated {
                            crate::tool::truncate::TruncResult::Unchanged { content } => {
                                content.clone()
                            }
                            crate::tool::truncate::TruncResult::Truncated { content, .. } => {
                                content.clone()
                            }
                        };
                        let mut metadata = result.metadata.clone();
                        if !metadata.is_object() {
                            metadata = serde_json::json!({});
                        }
                        match truncated {
                            crate::tool::truncate::TruncResult::Unchanged { .. } => {
                                if let Some(fields) = metadata.as_object_mut() {
                                    fields
                                        .insert("truncated".to_string(), serde_json::json!(false));
                                }
                            }
                            crate::tool::truncate::TruncResult::Truncated {
                                output_path, ..
                            } => {
                                if let Some(fields) = metadata.as_object_mut() {
                                    fields.insert("truncated".to_string(), serde_json::json!(true));
                                    fields.insert(
                                        "outputPath".to_string(),
                                        serde_json::json!(output_path.display().to_string()),
                                    );
                                }
                            }
                        }
                        let output = LlmToolOutput {
                            title: String::new(),
                            metadata,
                            output: output_content,
                            attachments: (!attachments.is_empty()).then(|| {
                                attachments
                                    .iter()
                                    .map(|attachment| {
                                        attachment_value(attachment, &session_id, &message_id)
                                    })
                                    .collect::<Vec<Value>>()
                            }),
                        };
                        // Aborted mid-execution — same completion contract
                        // as the builtin loop (tools.ts:121-127).
                        if cancel.is_cancelled() {
                            let attachments = output.attachments.clone().map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(|part| {
                                        serde_json::from_value::<
                                            alforria_schema::session_v1::V1FilePart,
                                        >(part.clone())
                                        .ok()
                                    })
                                    .collect()
                            });
                            let _ = processor
                                .complete_tool_call(
                                    &call_id,
                                    ToolCallOutput {
                                        title: output.title.clone(),
                                        metadata: output.metadata.clone(),
                                        output: output.output.clone(),
                                        attachments,
                                    },
                                )
                                .await;
                        }
                        Ok(output)
                    })
                }),
            });
        }
    }
    Ok(tools)
}

fn attachment_value(attachment: &Attachment, session_id: &str, message_id: &str) -> Value {
    let part = alforria_schema::session_v1::V1FilePart::File {
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
            time: alforria_schema::session_v1::ToolStateRunningTime { start: now },
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

fn json_map(value: &Value) -> alforria_schema::schema::JsonMap {
    match value {
        Value::Object(map) => map.clone(),
        other => {
            let mut map = alforria_schema::schema::JsonMap::new();
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

#[cfg(test)]
mod mcp_tests {
    use super::*;
    use crate::session::processor::Handle as ProcessorHandle;
    use crate::session::processor::{NoSummary, Processor, ProcessorDeps, ProcessorInput};
    use crate::session::test_support::user_message;
    use crate::session::test_support::{create_session as make_session, harness, test_model};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct FixedMcp(Vec<McpToolEntry>);

    impl McpToolSource for FixedMcp {
        fn mcp_tools<'a>(&'a self) -> crate::tool::def::BoxFuture<'a, Vec<McpToolEntry>> {
            Box::pin(async {
                self.0
                    .iter()
                    .map(|entry| McpToolEntry {
                        key: entry.key.clone(),
                        description: entry.description.clone(),
                        input_schema: entry.input_schema.clone(),
                        execute: entry.execute.clone(),
                    })
                    .collect()
            })
        }
    }

    fn mcp_entry() -> McpToolEntry {
        McpToolEntry {
            key: "srv_echo".to_string(),
            description: "Echo".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
            execute: Arc::new(|_args| {
                Box::pin(async move {
                    Ok(McpRawResult {
                        content: vec![
                            serde_json::json!({"type": "text", "text": "hello"}),
                            serde_json::json!({
                                "type": "resource",
                                "resource": {
                                    "text": "from resource",
                                    "blob": "aGk=",
                                    "uri": "file:///tmp/x",
                                },
                            }),
                        ],
                        structured_content: None,
                        metadata: serde_json::json!({"call": true}),
                    })
                })
            }),
        }
    }

    struct NoTaskOps;
    impl crate::tool::task::TaskOps for NoTaskOps {
        fn depth<'a>(
            &'a self,
            _session_id: &'a str,
        ) -> futures::future::BoxFuture<'a, Result<usize, crate::tool::error::ToolError>> {
            Box::pin(async { Ok(0) })
        }
        fn agent<'a>(
            &'a self,
            _name: &'a str,
        ) -> futures::future::BoxFuture<
            'a,
            Result<Option<crate::tool::task::SubagentInfo>, crate::tool::error::ToolError>,
        > {
            Box::pin(async { Ok(None) })
        }
        fn session_exists<'a>(
            &'a self,
            _session_id: &'a str,
        ) -> futures::future::BoxFuture<'a, bool> {
            Box::pin(async { false })
        }
        fn create_session<'a>(
            &'a self,
            _parent_id: &'a str,
            _title: &'a str,
            _agent: &'a str,
            _permission: Vec<crate::tool::task::Rule>,
        ) -> futures::future::BoxFuture<'a, Result<String, crate::tool::error::ToolError>> {
            Box::pin(async {
                Err(crate::tool::error::ToolError::Failed(
                    "not implemented".to_string(),
                ))
            })
        }
        fn parent_message<'a>(
            &'a self,
            _session_id: &'a str,
            _message_id: &'a str,
        ) -> futures::future::BoxFuture<
            'a,
            Result<crate::tool::task::ParentMessage, crate::tool::error::ToolError>,
        > {
            Box::pin(async {
                Err(crate::tool::error::ToolError::Failed(
                    "not implemented".to_string(),
                ))
            })
        }
        fn prompt<'a>(
            &'a self,
            _session_id: &'a str,
            _agent: &'a str,
            _model: &'a crate::tool::task::ModelRef,
            _variant: Option<&'a str>,
            _prompt: &'a str,
        ) -> futures::future::BoxFuture<
            'a,
            Result<crate::tool::task::PromptOutcome, crate::tool::error::ToolError>,
        > {
            Box::pin(async {
                Err(crate::tool::error::ToolError::Failed(
                    "not implemented".to_string(),
                ))
            })
        }
        fn cancel<'a>(&'a self, _session_id: &'a str) -> futures::future::BoxFuture<'a, ()> {
            Box::pin(async {})
        }
        fn inject_background_result<'a>(
            &'a self,
            _parent_session_id: &'a str,
            _variant: Option<&'a str>,
            _text: &'a str,
        ) -> futures::future::BoxFuture<'a, ()> {
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn mcp_tools_are_resolved_and_executed() {
        let h = harness("mcp-tools", Vec::new());
        let session = make_session(&h.services.sessions, &h.worktree);
        h.services
            .sessions
            .update_message(&user_message(&session.id, "msg_p1", 1.0))
            .unwrap();
        let model = test_model();
        let assistant = crate::session::test_support::assistant_message(
            &session.id,
            "msg_a1",
            "msg_p1",
            1,
            None,
            None,
        );
        h.services.sessions.update_message(&assistant).unwrap();
        let processor = Processor::create(
            ProcessorDeps {
                sessions: h.services.sessions.clone(),
                messages: h.services.messages.clone(),
                status: h.services.status.clone(),
                events: h.services.events.clone(),
                snapshot: Arc::new(crate::session::snapshot::DisabledSnapshot),
                agents: h.services.agents.clone(),
                config: Arc::new(serde_json::from_value(serde_json::json!({})).unwrap()),
                llm: h.llm.clone(),
                permission: Arc::new(crate::session::test_support::AllowAll),
                summary: Arc::new(NoSummary),
                clock: Arc::new(crate::session::test_support::FixedClock),
            },
            ProcessorInput {
                assistant_message: assistant,
                session_id: session.id.clone(),
                model: model.processor_model(),
            },
        )
        .await;
        let processor: ProcessorHandle = processor;

        struct RegistryAgents;
        impl crate::tool::def::Agents for RegistryAgents {
            fn get<'a>(
                &'a self,
                _agent: &'a str,
            ) -> futures::future::BoxFuture<
                'a,
                Result<crate::tool::def::AgentInfo, crate::tool::error::ToolError>,
            > {
                Box::pin(async {
                    Err(crate::tool::error::ToolError::Failed(
                        "no agents".to_string(),
                    ))
                })
            }
            fn list<'a>(
                &'a self,
            ) -> futures::future::BoxFuture<'a, Vec<crate::tool::def::AgentInfo>> {
                Box::pin(async { Vec::new() })
            }
        }
        let dummy_task = crate::tool::def::ToolDef {
            id: "task",
            description: "dummy".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
            }),
            format_validation_error: None,
            execute: Arc::new(|_args, _ctx| {
                Box::pin(async {
                    Ok(crate::tool::def::ExecuteResult {
                        title: "dummy".to_string(),
                        output: String::new(),
                        metadata: serde_json::json!({}),
                        attachments: None,
                    })
                })
            }),
        };
        let dummy_read = crate::tool::def::ToolDef {
            id: "read",
            description: "dummy".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
            }),
            format_validation_error: None,
            execute: Arc::new(|_args, _ctx| {
                Box::pin(async {
                    Ok(crate::tool::def::ExecuteResult {
                        title: "dummy".to_string(),
                        output: String::new(),
                        metadata: serde_json::json!({}),
                        attachments: None,
                    })
                })
            }),
        };
        let registry = crate::tool::registry::ToolRegistry::new(
            vec![dummy_task, dummy_read],
            Vec::new(),
            crate::tool::registry::RuntimeFlags::default(),
            Arc::new(RegistryAgents),
        )
        .expect("valid registry");
        let mcp: Arc<dyn McpToolSource> = Arc::new(FixedMcp(vec![mcp_entry()]));
        let deps = ResolveDeps {
            registry,
            permission: Arc::new(crate::session::test_support::AllowAll),
            instance: crate::tool::def::InstanceContext {
                directory: h.worktree.clone(),
                worktree: h.worktree.clone(),
            },
            clock: Arc::new(crate::session::test_support::FixedClock),
            mcp: Some(mcp),
            truncate: Arc::new(crate::tool::truncate::TruncateService::default_limits(
                std::env::temp_dir().join("opencode-mcp-tool-tests"),
            )),
        };
        let agent = h.services.agents.get("build").cloned().unwrap();
        let input = ResolveInput {
            agent: &agent,
            model: &model.llm,
            session: &session,
            processor: &processor,
            messages: &[],
            bypass_agent_check: false,
            prompt_ops: Arc::new(NoTaskOps),
            cancel: CancellationToken::new(),
        };
        let tools = resolve(&deps, input).await.expect("resolve succeeds");
        let tool = tools
            .iter()
            .find(|tool| tool.name == "srv_echo")
            .expect("mcp tool is resolved");
        assert_eq!(tool.description, "Echo");
        let output = (tool.execute)(serde_json::json!({}), "call_1".to_string())
            .await
            .expect("mcp execution succeeds");
        // Text parts join with a blank line (tools.ts:463).
        assert_eq!(output.output, "hello\n\nfrom resource");
        assert_eq!(output.metadata.get("call"), Some(&serde_json::json!(true)));
        assert!(output.metadata.get("truncated").is_some());
        let attachments = output
            .attachments
            .expect("resource blob becomes an attachment");
        assert_eq!(attachments.len(), 1);
        assert!(attachments[0]["url"]
            .as_str()
            .expect("attachment url")
            .starts_with("data:application/octet-stream;base64,"));
    }
}
