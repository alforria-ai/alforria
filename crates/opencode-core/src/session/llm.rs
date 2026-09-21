//! LLM stream seam — port of `session/llm.ts` reduced to the native
//! runtime (spec §2.6: `opencode-llm`'s route client is the only provider
//! client; the ai-sdk adapter and `LLMAISDK.toLLMEvents` are not ported).
//!
//! * [`StreamInput`] / [`LlmStream`] — the seam the processor drives.
//! * [`prepare`] — the `LLMRequestPrep.prepare` port (llm/request.ts):
//!   system assembly, sampling params, tool filtering and headers.
//! * [`LlmStreamImpl`] — the production implementation over the
//!   `opencode-llm` route client, including the native-runtime tool-call
//!   dispatch (native-runtime.ts:103-140).
//!
//! Not ported (llm.ts:118-206, 226-269): GitLab workflow bridging,
//! `includeRawChunks` and `experimental_repairToolCall` — the native
//! runtime already validates tool calls, and the lowercase-name and
//! `invalid`-tool fallbacks live in the M4 registry/invalid tool.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::StreamExt;
use opencode_llm::schema::errors::LlmError;
use opencode_llm::schema::events::LlmEvent;
use opencode_llm::schema::ids::ProviderMetadata;
use opencode_llm::schema::messages::{
    LlmRequest, Message, ModelRef, ToolDefinition, ToolResultValue,
};
use opencode_llm::schema::options::{
    GenerationOptions, HttpOptions, ProviderOptions, SystemPart, SystemPartType,
};
use opencode_schema::permission_v1::PermissionV1Ruleset;
use opencode_schema::session_v1::V1Message;
use serde_json::Value;

use crate::session::agents::AgentInfo;
use crate::session::store::INSTALLATION_VERSION;
use crate::tool::def::BoxFuture;
use crate::tool::permission::{disabled as permission_disabled, merge as merge_rulesets};

/// A provider event stream (TS `Stream.Stream<LLMEvent, unknown>`).
pub type LlmEventStream = futures::stream::BoxStream<'static, Result<LlmEvent, LlmError>>;

/// The runtime `Provider.Model` slice the seam consumes (the schema
/// `ModelInfo` plus the capability flags the TS runtime model computes,
/// provider.ts:1287-1300).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmModel {
    pub id: String,
    pub provider_id: String,
    pub api_id: String,
    pub api_npm: String,
    /// Runtime `capabilities.temperature` (`model.temperature ?? false`).
    pub temperature_capable: bool,
    /// `model.headers`.
    pub headers: BTreeMap<String, String>,
    /// `model.options`.
    pub options: BTreeMap<String, Value>,
    pub context_limit: f64,
    pub output_limit: f64,
    /// `RuntimeFlags.outputTokenMax`.
    pub output_token_max: Option<f64>,
}

/// One executable session tool handed to the runtime (the ai-sdk `Tool`
/// shape reduced to what dispatch needs).
#[derive(Clone)]
pub struct LlmTool {
    pub name: String,
    pub description: String,
    /// The (provider-transformed) JSON Schema.
    pub input_schema: Value,
    /// `(args, toolCallId)` → tool result.
    #[allow(clippy::type_complexity)]
    pub execute: Arc<
        dyn Fn(Value, String) -> BoxFuture<'static, Result<LlmToolOutput, ToolFailure>>
            + Send
            + Sync,
    >,
}

/// The thrown value the runtime surfaces on `tool-error`
/// (native-runtime.ts:188-127): TS carries the raw error instance so the
/// processor can `instanceof` `PermissionV1.RejectedError` /
/// `Question.RejectedError` (processor.ts:200-201); the Rust seam
/// serializes the class into the event's `error` JSON instead
/// (documented divergence, spec §2.6).
#[derive(Debug, Clone, PartialEq)]
pub enum ToolFailure {
    /// A plain `Error` — `errorMessage(error)` is the part error text.
    Message(String),
    /// `PermissionV1.RejectedError` / `Question.RejectedError`.
    Rejected(String),
}

impl ToolFailure {
    pub fn message(&self) -> String {
        match self {
            ToolFailure::Message(message) | ToolFailure::Rejected(message) => message.clone(),
        }
    }

    /// The `tool-error` event's `error` payload — `{name, data}` per the
    /// `errorMessage` convention (util/error.ts).
    pub fn error_value(&self) -> Value {
        match self {
            ToolFailure::Message(message) => serde_json::json!({
                "name": "Error",
                "data": { "message": message },
            }),
            ToolFailure::Rejected(message) => serde_json::json!({
                "name": "RejectedError",
                "data": { "message": message },
            }),
        }
    }
}

impl std::fmt::Debug for LlmTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmTool")
            .field("name", &self.name)
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

/// The ai-sdk `Tool.execute` result (tool.ts:48-53): the record the
/// processor's `toolResultOutput` reads (`output` string required).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LlmToolOutput {
    pub title: String,
    pub metadata: Value,
    pub output: String,
    /// `SessionV1.FilePart`-shaped attachments.
    pub attachments: Option<Vec<Value>>,
}

/// `LLM.StreamInput` (llm.ts:35-48).
#[derive(Clone)]
pub struct StreamInput {
    /// The triggering user message.
    pub user: V1Message,
    pub session_id: String,
    pub parent_session_id: Option<String>,
    /// The instance project id (`InstanceState.project.id`), surfaced as
    /// the `x-opencode-project` header on opencode-hosted providers.
    pub project_id: Option<String>,
    /// `RuntimeFlags.client`.
    pub client: String,
    pub model: LlmModel,
    pub agent: AgentInfo,
    pub permission: Option<PermissionV1Ruleset>,
    pub system: Vec<String>,
    pub messages: Vec<Message>,
    pub small: bool,
    pub tools: Vec<LlmTool>,
    pub retries: Option<u32>,
    /// `"auto" | "required" | "none"`.
    pub tool_choice: Option<&'static str>,
}

/// The LLM seam (TS `LLM.Service`). The processor is agnostic to the
/// implementation; tests script a `MockLlmStream`.
pub trait LlmStream: Send + Sync {
    fn stream(&self, input: StreamInput) -> LlmEventStream;

    /// The abort-aware entry (Effect streams halt on interruption):
    /// production routes carry `cancel` into the route client so the
    /// buffered parser state flushes (`onHalt`) before the consumer
    /// drops the stream — the forked tool dispatch then runs within
    /// cleanup's grace window (route/client.ts:287-291).
    fn stream_with_cancel(
        &self,
        input: StreamInput,
        cancel: tokio_util::sync::CancellationToken,
    ) -> LlmEventStream {
        let _ = cancel;
        self.stream(input)
    }
}

// ---------------------------------------------------------------------------
// ProviderTransform sampling defaults (transform.ts:520-583)
// ---------------------------------------------------------------------------

const GEMINI_SAMPLING_DEFAULTS: [&str; 5] = [
    r"gemini-2[.-]5(?:[.-]|$)",
    r"gemini-3-(?:flash|pro)(?:[.-]|$)",
    r"gemini-3[.-]1(?:[.-]|$)",
    r"gemini-3[.-]5-flash(?!-lite)(?:[.-]|$)",
    r"gemini-3[.-]5-flash(?!-lite)$",
];

/// `ProviderTransform.temperature` (transform.ts:527-543).
pub fn provider_temperature(model: &LlmModel) -> Option<f64> {
    let id = model.api_id.to_lowercase();
    if id.contains("north-mini-code") {
        return Some(1.0);
    }
    if id.contains("claude") {
        return None;
    }
    if id.contains("gemini") {
        return gemini_sampling_defaults(&id).then_some(1.0);
    }
    if id.contains("glm-4.6") || id.contains("glm-4.7") || id.contains("minimax-m2") {
        return Some(1.0);
    }
    if id.contains("kimi-k2") {
        if ["thinking", "k2.", "k2p", "k2-5"]
            .iter()
            .any(|s| id.contains(s))
        {
            return Some(1.0);
        }
        return Some(0.6);
    }
    None
}

/// `ProviderTransform.topP` (transform.ts:546-560).
pub fn provider_top_p(model: &LlmModel) -> Option<f64> {
    let id = model.api_id.to_lowercase();
    if id.contains("gemini") {
        return gemini_sampling_defaults(&id).then_some(0.95);
    }
    if ["minimax-m2", "kimi-k2.5", "kimi-k2p5", "kimi-k2-5"]
        .iter()
        .any(|s| id.contains(s))
    {
        return Some(0.95);
    }
    if ["deepseek-v4-flash-0731", "deepseek-v4-flash:0731"]
        .iter()
        .any(|name| id.contains(name))
        || (id.contains("deepseek-v4-flash")
            && (model.provider_id == "deepseek" || model.provider_id.starts_with("opencode")))
    {
        return Some(0.95);
    }
    None
}

/// `ProviderTransform.topK` (transform.ts:562-570).
pub fn provider_top_k(model: &LlmModel) -> Option<f64> {
    let id = model.api_id.to_lowercase();
    if id.contains("minimax-m2") {
        if ["m2.", "m25", "m21"].iter().any(|s| id.contains(s)) {
            return Some(40.0);
        }
        return Some(20.0);
    }
    if id.contains("gemini") {
        return gemini_sampling_defaults(&id).then_some(64.0);
    }
    None
}

fn gemini_sampling_defaults(id: &str) -> bool {
    GEMINI_SAMPLING_DEFAULTS
        .iter()
        .any(|pattern| regex::Regex::new(pattern).is_ok_and(|re| re.is_match(id)))
}

// ---------------------------------------------------------------------------
// LLMRequestPrep.prepare (llm/request.ts:56-206)
// ---------------------------------------------------------------------------

/// The prepared request (request.ts:38-51), reduced to the native runtime
/// inputs.
#[derive(Debug, Clone, Default)]
pub struct Prepared {
    pub system: Vec<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<LlmTool>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<f64>,
    pub max_output_tokens: Option<f64>,
    pub headers: BTreeMap<String, String>,
}

/// [`prepare`] input — the borrowable slice of [`StreamInput`].
pub struct PrepareInput<'a> {
    pub user: &'a V1Message,
    pub session_id: &'a str,
    pub parent_session_id: Option<&'a str>,
    pub project_id: Option<&'a str>,
    pub client: &'a str,
    pub model: &'a LlmModel,
    pub agent: &'a AgentInfo,
    pub permission: Option<&'a PermissionV1Ruleset>,
    pub system: &'a [String],
    pub messages: &'a [Message],
    pub small: bool,
    pub tools: &'a [LlmTool],
}

/// `resolveTools` (request.ts:208-214): drop tools denied by the merged
/// ruleset and tools the user message turned off.
pub fn resolve_tools(input: &PrepareInput<'_>) -> Vec<LlmTool> {
    let mut rulesets = vec![input.agent.permission.as_ref()];
    if let Some(permission) = input.permission {
        rulesets.push(permission);
    }
    let merged = merge_rulesets(&rulesets);
    let names: Vec<&str> = input.tools.iter().map(|tool| tool.name.as_str()).collect();
    let disabled = permission_disabled(&names, &merged);
    input
        .tools
        .iter()
        .filter(|tool| !disabled.contains(&tool.name) && user_tool_enabled(input.user, &tool.name))
        .cloned()
        .collect()
}

fn user_tool_enabled(user: &V1Message, name: &str) -> bool {
    match user {
        V1Message::User { tools, .. } => tools.as_ref().and_then(|t| t.get(name)) != Some(&false),
        V1Message::Assistant { .. } => true,
    }
}

/// `LLMRequestPrep.prepare` (request.ts:56-206), reduced to the native
/// runtime. Plugin hooks are no-op seams (spec §2.6).
pub fn prepare(input: &PrepareInput<'_>) -> Prepared {
    // system (request.ts:58-78): [agent prompt or provider prompts,
    // input.system, user.system], falsy-filtered, joined by "\n" — a
    // single system element. The `system.length > 2` re-join only fires
    // when a plugin splits it; plugins are no-ops in M5.
    let mut system_blocks: Vec<String> = Vec::new();
    match &input.agent.prompt {
        Some(prompt) => system_blocks.push(prompt.clone()),
        None => system_blocks.extend(crate::session::system::provider(
            &input.model.api_id,
            &input.model.provider_id,
        )),
    }
    system_blocks.extend(input.system.iter().cloned());
    if let V1Message::User {
        system: Some(user_system),
        ..
    } = input.user
    {
        system_blocks.push(user_system.clone());
    }
    let system = vec![system_blocks
        .into_iter()
        .filter(|block| !block.is_empty())
        .collect::<Vec<_>>()
        .join("\n")];

    // params (request.ts:114-132). `capabilities.temperature` is falsy for
    // models that don't opt in, which drops the agent override too.
    let temperature = input
        .model
        .temperature_capable
        .then(|| {
            input
                .agent
                .temperature
                .or_else(|| provider_temperature(input.model))
        })
        .flatten();
    let top_p = input.agent.top_p.or_else(|| provider_top_p(input.model));
    let top_k = provider_top_k(input.model);
    let max_output_tokens = Some(crate::session::overflow::max_output_tokens(
        &crate::session::overflow::ModelLimits {
            context: input.model.context_limit,
            input: None,
            output: input.model.output_limit,
        },
        input.model.output_token_max,
    ));

    // headers (request.ts:87-99, 152-180).
    let user_agent = format!("opencode/{}", INSTALLATION_VERSION);
    let mut headers = BTreeMap::new();
    if input.model.provider_id.starts_with("opencode") {
        if let Some(project_id) = input.project_id {
            headers.insert("x-opencode-project".to_string(), project_id.to_string());
        }
        headers.insert(
            "x-opencode-session".to_string(),
            input.session_id.to_string(),
        );
        headers.insert("x-opencode-request".to_string(), message_id(input.user));
        headers.insert("x-opencode-client".to_string(), input.client.to_string());
        headers.insert("User-Agent".to_string(), user_agent);
    } else {
        headers.insert(
            "x-session-affinity".to_string(),
            input.session_id.to_string(),
        );
        headers.insert("X-Session-Id".to_string(), input.session_id.to_string());
        headers.insert("User-Agent".to_string(), user_agent);
    }
    if let Some(parent) = input.parent_session_id {
        headers.insert("x-parent-session-id".to_string(), parent.to_string());
    }
    for (name, value) in &input.model.headers {
        headers.insert(name.clone(), value.clone());
    }

    // messages (request.ts:101-112): the system blocks travel as the
    // protocol-level system parameter (`request.system`, set by
    // `build_request`), not as leading system messages — the protocol
    // layer lowers `MessageRole::System` chronologically (the
    // `<system-update>` wrapper), so pushing the system blocks here
    // would send the system prompt twice.
    let mut messages = Vec::new();
    messages.extend(input.messages.iter().cloned());

    // tools (request.ts:132-165): resolved, then sorted by name.
    let mut tools = resolve_tools(input);
    tools.sort_by(|a, b| a.name.cmp(&b.name));

    Prepared {
        system,
        messages,
        tools,
        temperature,
        top_p,
        top_k,
        max_output_tokens,
        headers,
    }
}

fn message_id(user: &V1Message) -> String {
    match user {
        V1Message::User { id, .. } | V1Message::Assistant { id, .. } => id.clone(),
    }
}

/// `hasToolCalls` (request.ts:216-224) — used by the loop.
pub fn has_tool_calls(messages: &[Message]) -> bool {
    for msg in messages {
        for part in &msg.content {
            if matches!(
                part,
                opencode_llm::schema::messages::ContentPart::ToolCall { .. }
                    | opencode_llm::schema::messages::ContentPart::ToolResult { .. }
            ) {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Production implementation (route client + tool dispatch)
// ---------------------------------------------------------------------------

/// The transport seam: hand a compiled [`LlmRequest`] to the route client.
/// `model` supplies the `ModelRef` (route handle + defaults) for the
/// current model.
pub trait LlmRequestSender: Send + Sync {
    fn model_ref(&self, model: &LlmModel) -> ModelRef;
    fn send(&self, request: LlmRequest) -> BoxFuture<'static, Result<LlmEventStream, LlmError>>;

    fn send_with_cancel(
        &self,
        request: LlmRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'static, Result<LlmEventStream, LlmError>> {
        let _ = cancel;
        self.send(request)
    }
}

/// The production [`LlmStream`]: prepare the request, stream it through
/// the route client, and dispatch non-provider-executed tool calls the
/// way the TS native runtime does (native-runtime.ts:103-140,
/// tool-runtime.ts:23-76).
pub struct LlmStreamImpl {
    sender: Arc<dyn LlmRequestSender>,
}

impl LlmStreamImpl {
    pub fn new(sender: Arc<dyn LlmRequestSender>) -> LlmStreamImpl {
        LlmStreamImpl { sender }
    }
}

impl LlmStreamImpl {
    fn stream_with_sender(&self, input: StreamInput) -> LlmEventStream {
        let sender = self.sender.clone();
        let request = build_request(sender.as_ref(), &input);
        futures::stream::once(async move {
            match sender.send(request).await {
                Ok(stream) => dispatch_tool_calls(stream, input.tools),
                Err(error) => futures::stream::once(async move { Err(error) }).boxed(),
            }
        })
        .flatten()
        .boxed()
    }
}

impl LlmStream for LlmStreamImpl {
    fn stream(&self, input: StreamInput) -> LlmEventStream {
        self.stream_with_sender(input)
    }

    fn stream_with_cancel(
        &self,
        input: StreamInput,
        cancel: tokio_util::sync::CancellationToken,
    ) -> LlmEventStream {
        let sender = self.sender.clone();
        let request = build_request(sender.as_ref(), &input);
        futures::stream::once(async move {
            match sender.send_with_cancel(request, cancel).await {
                Ok(stream) => dispatch_tool_calls(stream, input.tools),
                Err(error) => futures::stream::once(async move { Err(error) }).boxed(),
            }
        })
        .flatten()
        .boxed()
    }
}

fn build_request(sender: &dyn LlmRequestSender, input: &StreamInput) -> LlmRequest {
    let prepared = prepare(&PrepareInput {
        user: &input.user,
        session_id: &input.session_id,
        parent_session_id: input.parent_session_id.as_deref(),
        project_id: input.project_id.as_deref(),
        client: &input.client,
        model: &input.model,
        agent: &input.agent,
        permission: input.permission.as_ref(),
        system: &input.system,
        messages: &input.messages,
        small: input.small,
        tools: &input.tools,
    });
    let mut request = LlmRequest::new(sender.model_ref(&input.model));
    request.system = prepared
        .system
        .iter()
        .map(|text| SystemPart {
            r#type: SystemPartType::Text,
            text: text.clone(),
            cache: None,
            metadata: None,
        })
        .collect();
    request.messages = prepared.messages;
    // `activeTools` (llm.ts:317): the `invalid` router tool is defined but
    // never model-visible.
    request.tools = prepared
        .tools
        .iter()
        .filter(|tool| tool.name != "invalid")
        .map(|tool| ToolDefinition {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: schema_map(&tool.input_schema),
            output_schema: None,
            cache: None,
            metadata: None,
            native: None,
        })
        .collect();
    if let Some(choice) = input.tool_choice {
        request.tool_choice = Some(opencode_llm::schema::messages::ToolChoice::make(choice));
    }
    request.generation = Some(GenerationOptions {
        max_tokens: prepared.max_output_tokens,
        temperature: prepared.temperature,
        top_p: prepared.top_p,
        top_k: prepared.top_k,
        frequency_penalty: None,
        presence_penalty: None,
        seed: None,
        stop: None,
    });
    request.provider_options = provider_options(input);
    let mut headers = request
        .model
        .defaults
        .as_ref()
        .and_then(|defaults| defaults.http.as_ref())
        .and_then(|http| http.headers.clone())
        .unwrap_or_default();
    for (name, value) in &prepared.headers {
        headers.insert(name.clone(), value.clone());
    }
    request.http = Some(HttpOptions {
        body: None,
        headers: Some(headers),
        query: None,
    });
    request
}

/// Provider options merge (request.ts:84-99), reduced: the TS base
/// (`ProviderTransform.options` / `smallOptions`) is keyed almost
/// entirely on ai-sdk `api.npm` package names, which have no native
/// counterpart, so only `model.options` then `agent.options` flow
/// (documented divergence, spec §2.6).
fn provider_options(input: &StreamInput) -> Option<ProviderOptions> {
    let mut options: ProviderOptions = BTreeMap::new();
    for (key, value) in &input.model.options {
        options.insert(key.clone(), value_to_map(value));
    }
    for (key, value) in &input.agent.options {
        options.insert(key.clone(), value_to_map(value));
    }
    if options.is_empty() {
        None
    } else {
        Some(options)
    }
}

fn value_to_map(value: &Value) -> opencode_schema::schema::JsonMap {
    match value {
        Value::Object(map) => map.clone(),
        other => {
            let mut map = opencode_schema::schema::JsonMap::new();
            map.insert("value".to_string(), other.clone());
            map
        }
    }
}

fn schema_map(value: &Value) -> opencode_schema::schema::JsonMap {
    match value {
        Value::Object(map) => map.clone(),
        _ => opencode_schema::schema::JsonMap::new(),
    }
}

/// Unfold state for [`dispatch_tool_calls`].
enum Dispatch {
    /// The provider stream is live; `tx` keeps the settlements queue open.
    Streaming {
        stream: LlmEventStream,
        tx: tokio::sync::mpsc::UnboundedSender<Result<LlmEvent, LlmError>>,
        rx: tokio::sync::mpsc::UnboundedReceiver<Result<LlmEvent, LlmError>>,
    },
    /// The provider stream ended; drain the settlements queue until every
    /// forked dispatch has completed (and dropped its sender).
    Settlements {
        rx: tokio::sync::mpsc::UnboundedReceiver<Result<LlmEvent, LlmError>>,
    },
    /// The provider stream errored; the scope interrupted the settlements.
    Done,
}

/// The native-runtime tool-call dispatch: every `tool-call` event that the
/// provider did not execute itself is forked immediately —
/// `FiberSet.run(settlements, { startImmediately: true })` — so tool calls
/// run concurrently with the ongoing provider stream and each other; the
/// settlements queue is concatenated after the provider stream
/// (native-runtime.ts:103-140, tool-runtime.ts).
fn dispatch_tool_calls(stream: LlmEventStream, tools: Vec<LlmTool>) -> LlmEventStream {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let state = Dispatch::Streaming { stream, tx, rx };
    let tools = std::sync::Arc::new(tools);
    futures::stream::unfold(state, move |state| {
        let tools = tools.clone();
        async move {
            match state {
                Dispatch::Streaming { mut stream, tx, rx } => match stream.next().await {
                    Some(Ok(event)) => {
                        if let LlmEvent::ToolCall {
                            id,
                            name,
                            input,
                            provider_executed,
                            ..
                        } = &event
                        {
                            if *provider_executed != Some(true) {
                                let id = id.clone();
                                let name = name.clone();
                                let input = input.clone();
                                let tx = tx.clone();
                                let tools = tools.clone();
                                // Fork the dispatch (FiberSet.run): the tool
                                // executes concurrently with the stream and
                                // its settlement events join the queue.
                                tokio::spawn(async move {
                                    let events = dispatch_one(&tools, &id, &name, input).await;
                                    for event in events {
                                        if tx.send(Ok(event)).is_err() {
                                            break;
                                        }
                                    }
                                });
                            }
                        }
                        Some((Ok(event), Dispatch::Streaming { stream, tx, rx }))
                    }
                    // The provider stream errored: surface it and end (the
                    // TS scope interrupts the settlement fibers).
                    Some(Err(error)) => Some((Err(error), Dispatch::Done)),
                    // Provider stream complete — drop our sender so the
                    // queue ends once every settlement has finished, then
                    // drain it (Stream.concat(fromQueue(results))).
                    None => {
                        drop(tx);
                        let mut rx = rx;
                        rx.recv()
                            .await
                            .map(|item| (item, Dispatch::Settlements { rx }))
                    }
                },
                Dispatch::Settlements { mut rx } => rx
                    .recv()
                    .await
                    .map(|item| (item, Dispatch::Settlements { rx })),
                Dispatch::Done => None,
            }
        }
    })
    .boxed()
}

/// The `ToolRuntime.dispatch` port (tool-runtime.ts:23-76) for one call.
async fn dispatch_one(tools: &[LlmTool], id: &str, name: &str, input: Value) -> Vec<LlmEvent> {
    let Some(tool) = tools.iter().find(|tool| tool.name == name) else {
        let message = format!("Unknown tool: {name}");
        return vec![
            LlmEvent::ToolError {
                id: id.to_string(),
                name: name.to_string(),
                message: message.clone(),
                error: None,
                provider_metadata: None,
            },
            LlmEvent::ToolResult {
                id: id.to_string(),
                name: name.to_string(),
                result: ToolResultValue::Error {
                    value: Value::String(message),
                },
                output: None,
                provider_executed: None,
                provider_metadata: None,
            },
        ];
    };
    let execute = tool.execute.clone();
    match execute(input, id.to_string()).await {
        Ok(output) => {
            let mut value = serde_json::json!({
                "output": output.output,
                "title": output.title,
                "metadata": output.metadata,
            });
            if let Some(attachments) = output.attachments {
                value["attachments"] = Value::Array(attachments);
            }
            vec![LlmEvent::ToolResult {
                id: id.to_string(),
                name: name.to_string(),
                result: ToolResultValue::Json { value },
                output: None,
                provider_executed: None,
                provider_metadata: None,
            }]
        }
        Err(failure) => {
            let message = failure.message();
            vec![
                LlmEvent::ToolError {
                    id: id.to_string(),
                    name: name.to_string(),
                    message: message.clone(),
                    error: Some(failure.error_value()),
                    provider_metadata: None,
                },
                LlmEvent::ToolResult {
                    id: id.to_string(),
                    name: name.to_string(),
                    result: ToolResultValue::Error {
                        value: Value::String(message),
                    },
                    output: None,
                    provider_executed: None,
                    provider_metadata: None,
                },
            ]
        }
    }
}

/// Serialize `ProviderMetadata` into the wire metadata a part carries.
pub fn provider_metadata_to_map(
    metadata: &Option<ProviderMetadata>,
) -> Option<serde_json::Map<String, Value>> {
    let metadata = metadata.as_ref()?;
    let value = serde_json::to_value(metadata).ok()?;
    value.as_object().cloned()
}
