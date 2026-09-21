//! Stream processor — port of `session/processor.ts` (processor.ts:1-732).
//!
//! Drives one [`LlmEvent`] stream into session parts, tool-call state and
//! the step usage ledger, with the doom-loop detector, the retry schedule
//! and abort cleanup.
//!
//! Seams the not-yet-landed milestones bind to (spec §2.1):
//!
//! * [`AskPermission`] — the M5.5 permission service; the processor only
//!   needs ask-and-possibly-reject.
//! * [`SummarySummarize`] — the M5.6 session summarizer
//!   (`SessionSummary.Service`); [`NoSummary`] is the no-op default.
//!
//! Not ported: the `Image` service attachment normalization
//! (processor.ts:349-366) — attachments pass through unchanged (no image
//! resizer ships with M5); plugin hooks (`experimental.text.complete`,
//! `chat.params`, `chat.headers`) are no-op seams (spec §2.6).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opencode_llm::schema::errors::{LlmError, LlmErrorReason, ProviderFailureClassification};
use opencode_llm::schema::events::{LlmEvent, Usage};
use opencode_llm::schema::ids::FinishReason;
use opencode_llm::schema::messages::ToolResultValue;
use opencode_schema::permission_v1::PermissionV1Ruleset;
use opencode_schema::session_status::SessionStatusInfo;
use opencode_schema::session_v1::{
    AssistantError, ReasoningTime, TextPartTime, ToolStateCompletedTime, ToolStateErrorTime,
    ToolStateRunningTime, V1FilePart, V1Message, V1Part, V1ToolState,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::schema::Config;
use crate::event::bus::PublishOptions;
use crate::Clock;

use crate::session::agents::AgentRegistry;
use crate::session::error::SessionError;
use crate::session::event_definitions::SESSION_ERROR;
use crate::session::from_error::{from_error, ApiCallError, FromErrorCtx, SourceError};
use crate::session::llm::{provider_metadata_to_map, LlmStream, StreamInput};
use crate::session::message::MessageStore;
use crate::session::overflow::{is_overflow, IsOverflowInput, ModelLimits};
use crate::session::retry::{Policy as RetryPolicy, PolicyStep, RetrySet};
use crate::session::run_state::Deferred;
use crate::session::snapshot::Snapshot;
use crate::session::status::SessionStatusService;
use crate::session::store::SessionStore;
use crate::session::usage::{get_usage, GetUsage, ModelCost};
use crate::tool::def::BoxFuture;

/// Identical tool calls before the user is asked to confirm
/// (processor.ts:27).
pub const DOOM_LOOP_THRESHOLD: usize = 3;

/// `Result` (processor.ts:29) — the agent-loop decision after a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessResult {
    Compact,
    Stop,
    Continue,
}

/// The failure surface of [`Handle::process`]. Stream and tool failures are
/// converted by `halt` into message state — only cancellation and
/// storage/event defects surface here.
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    /// The cancellation token fired (TS: fiber interrupt).
    #[error("cancelled")]
    Cancelled,
    /// A storage/event defect.
    #[error(transparent)]
    Session(#[from] SessionError),
}

/// The `Provider.Model` slice the processor consumes for usage and
/// overflow (the TS handle keeps the whole model).
#[derive(Debug, Clone)]
pub struct ProcessorModel {
    pub id: String,
    pub provider_id: String,
    pub cost: ModelCost,
    pub limits: ModelLimits,
    pub output_token_max: Option<f64>,
}

// -------------------------------------------------------------------------
// Seams
// -------------------------------------------------------------------------

/// `permission.ask(...)` (processor.ts:363-372) — the M5.5 permission
/// service seam.
pub struct PermissionAsk {
    pub session_id: String,
    pub permission: String,
    pub patterns: Vec<String>,
    pub always: Vec<String>,
    pub metadata: Value,
    pub ruleset: PermissionV1Ruleset,
    pub tool: PermissionAskTool,
}

/// The `tool` member of the ask metadata (`{ messageID, callID }`).
pub struct PermissionAskTool {
    pub message_id: String,
    pub call_id: String,
}

/// Permission ask failures.
#[derive(Debug, thiserror::Error)]
pub enum PermissionAskError {
    /// `PermissionV1.RejectedError` / `Question.RejectedError`.
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("{0}")]
    Other(String),
}

/// The permission seam the processor needs (ask + reject channel).
pub trait AskPermission: Send + Sync {
    fn ask<'a>(&'a self, request: PermissionAsk) -> BoxFuture<'a, Result<(), PermissionAskError>>;
}

/// The M5.6 `SessionSummary.Service` seam — `summarize` is fire-and-forget
/// (TS forks it in a scope). `reset` is the inline zeroing half the prompt
/// loop awaits at step 1 (Effect's cooperative scheduler runs the TS fork
/// at the parent's first suspension point).
pub trait SummarySummarize: Send + Sync {
    fn summarize(&self, session_id: &str, message_id: &str) -> BoxFuture<'static, ()>;
    fn reset<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<(), SessionError>>;
    fn attach_diffs<'a>(
        &'a self,
        session_id: &'a str,
        message_id: &'a str,
    ) -> BoxFuture<'static, ()>;
}

/// No-op default (M5.6 not landed).
pub struct NoSummary;

impl SummarySummarize for NoSummary {
    fn summarize(&self, _session_id: &str, _message_id: &str) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }

    fn reset<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async { Ok(()) })
    }

    fn attach_diffs<'a>(
        &'a self,
        _session_id: &'a str,
        _message_id: &'a str,
    ) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

// -------------------------------------------------------------------------
// Inputs
// -------------------------------------------------------------------------

/// The `output` of `Handle::completeToolCall` (processor.ts:57-65).
#[derive(Debug, Clone, Default)]
pub struct ToolCallOutput {
    pub title: String,
    pub metadata: Value,
    pub output: String,
    pub attachments: Option<Vec<V1FilePart>>,
}

/// `Input` (processor.ts:65-69).
pub struct ProcessorInput {
    pub assistant_message: V1Message,
    pub session_id: String,
    pub model: ProcessorModel,
}

/// Everything the processor needs from the instance. Owned by the
/// [`Handle`] after [`Processor::create`].
pub struct ProcessorDeps {
    pub sessions: SessionStore,
    pub messages: MessageStore,
    pub status: Arc<SessionStatusService>,
    pub events: Arc<crate::event::bus::EventBus>,
    pub snapshot: Arc<dyn Snapshot>,
    pub agents: AgentRegistry,
    pub config: Arc<Config>,
    pub llm: Arc<dyn LlmStream>,
    pub permission: Arc<dyn AskPermission>,
    pub summary: Arc<dyn SummarySummarize>,
    pub clock: Arc<dyn Clock>,
}

// -------------------------------------------------------------------------
// Processor state
// -------------------------------------------------------------------------

/// `ToolCall` bookkeeping (processor.ts:79-84).
#[derive(Clone)]
struct ToolCall {
    done: Arc<Deferred<()>>,
    part_id: String,
    message_id: String,
    session_id: String,
}

/// `ProcessorContext` (processor.ts:86-94).
struct Ctx {
    assistant_message: V1Message,
    toolcalls: BTreeMap<String, ToolCall>,
    should_break: bool,
    snapshot: Option<String>,
    blocked: bool,
    needs_compaction: bool,
    current_text: Option<V1Part>,
    reasoning_map: BTreeMap<String, V1Part>,
    aborted: bool,
}

/// The error handed to [`fail_tool_call`] — TS instanceof dispatch over
/// `PermissionV1.RejectedError` / `Question.RejectedError` (M5.5 threads
/// the rejection through the tool runtime).
#[derive(Debug, Clone)]
enum FailError {
    /// Constructed by the M5.5 permission flow.
    #[allow(dead_code)]
    Rejected {
        message: String,
    },
    Error {
        message: String,
    },
}

struct Inner {
    deps: ProcessorDeps,
    ctx: Mutex<Ctx>,
    model: ProcessorModel,
    session_id: String,
}

/// The processor handle (processor.ts:54-62): the message under
/// construction, live tool-call updates, and the stream driver.
#[derive(Clone)]
pub struct Handle {
    inner: Arc<Inner>,
}

/// Outcome of one attempt driving the stream.
enum RunOutcome {
    Done,
    Cancelled,
    Failed(SourceError),
}

/// `SessionProcessor.Service` (the static factory).
pub struct Processor;

impl Processor {
    /// `create` (processor.ts:100-120): pre-capture the snapshot before
    /// the LLM stream starts — the runtime may execute tools internally
    /// before emitting start-step events, so capturing inside the event
    /// handler can be too late.
    pub async fn create(deps: ProcessorDeps, input: ProcessorInput) -> Handle {
        let initial_snapshot = deps.snapshot.track().await;
        let ctx = Ctx {
            assistant_message: input.assistant_message,
            toolcalls: BTreeMap::new(),
            should_break: false,
            snapshot: initial_snapshot,
            blocked: false,
            needs_compaction: false,
            current_text: None,
            reasoning_map: BTreeMap::new(),
            aborted: false,
        };
        Handle {
            inner: Arc::new(Inner {
                deps,
                ctx: Mutex::new(ctx),
                model: input.model,
                session_id: input.session_id,
            }),
        }
    }
}

// -------------------------------------------------------------------------
// Part/message shape helpers
// -------------------------------------------------------------------------

fn part_id(part: &V1Part) -> &str {
    match part {
        V1Part::Text { id, .. }
        | V1Part::Subtask { id, .. }
        | V1Part::Reasoning { id, .. }
        | V1Part::File { id, .. }
        | V1Part::Tool { id, .. }
        | V1Part::StepStart { id, .. }
        | V1Part::StepFinish { id, .. }
        | V1Part::Snapshot { id, .. }
        | V1Part::Patch { id, .. }
        | V1Part::Agent { id, .. }
        | V1Part::Retry { id, .. }
        | V1Part::Compaction { id, .. } => id,
    }
    .as_str()
}

fn part_session_id(part: &V1Part) -> &str {
    match part {
        V1Part::Text { session_id, .. }
        | V1Part::Subtask { session_id, .. }
        | V1Part::Reasoning { session_id, .. }
        | V1Part::File { session_id, .. }
        | V1Part::Tool { session_id, .. }
        | V1Part::StepStart { session_id, .. }
        | V1Part::StepFinish { session_id, .. }
        | V1Part::Snapshot { session_id, .. }
        | V1Part::Patch { session_id, .. }
        | V1Part::Agent { session_id, .. }
        | V1Part::Retry { session_id, .. }
        | V1Part::Compaction { session_id, .. } => session_id,
    }
    .as_str()
}

fn part_message_id(part: &V1Part) -> &str {
    match part {
        V1Part::Text { message_id, .. }
        | V1Part::Subtask { message_id, .. }
        | V1Part::Reasoning { message_id, .. }
        | V1Part::File { message_id, .. }
        | V1Part::Tool { message_id, .. }
        | V1Part::StepStart { message_id, .. }
        | V1Part::StepFinish { message_id, .. }
        | V1Part::Snapshot { message_id, .. }
        | V1Part::Patch { message_id, .. }
        | V1Part::Agent { message_id, .. }
        | V1Part::Retry { message_id, .. }
        | V1Part::Compaction { message_id, .. } => message_id,
    }
    .as_str()
}

fn tool_name(part: &V1Part) -> &str {
    match part {
        V1Part::Tool { tool, .. } => tool,
        _ => "",
    }
}

fn part_metadata(part: &V1Part) -> Option<opencode_schema::schema::JsonMap> {
    match part {
        V1Part::Tool { metadata, .. } => metadata.clone(),
        _ => None,
    }
}

fn metadata_jsonmap(value: Value) -> opencode_schema::schema::JsonMap {
    match value {
        Value::Object(map) => map,
        other => {
            let mut map = opencode_schema::schema::JsonMap::new();
            map.insert("value".to_string(), other);
            map
        }
    }
}

fn assistant_location(ctx: &Ctx) -> (String, String) {
    // `(session_id, message_id)` — the message id is the parts' messageID.
    let (message_id, session_id) = message_ids(&ctx.assistant_message);
    (session_id, message_id)
}

fn message_ids(msg: &V1Message) -> (String, String) {
    match msg {
        V1Message::User { id, session_id, .. } | V1Message::Assistant { id, session_id, .. } => {
            (id.clone(), session_id.clone())
        }
    }
}

fn new_part_id() -> String {
    crate::session::ids::PartId::ascending(None).expect("part id generation")
}

fn finish_reason_string(reason: FinishReason) -> String {
    match reason {
        FinishReason::Stop => "stop".to_string(),
        FinishReason::Length => "length".to_string(),
        FinishReason::ToolCalls => "tool-calls".to_string(),
        FinishReason::ContentFilter => "content-filter".to_string(),
        FinishReason::Error => "error".to_string(),
        FinishReason::Unknown => "unknown".to_string(),
    }
}

/// The `parentID` of the assistant message under construction.
fn assistant_parent_id(ctx: &Ctx) -> String {
    match &ctx.assistant_message {
        V1Message::Assistant { parent_id, .. } => parent_id.clone(),
        V1Message::User { .. } => String::new(),
    }
}

/// `settleToolCall` (processor.ts:121-126).
fn settle_tool_call(ctx: &mut Ctx, tool_call_id: &str) {
    if let Some(call) = ctx.toolcalls.remove(tool_call_id) {
        call.done.complete(());
    }
}

/// `readToolCall` (processor.ts:123-131): resolve the live part of a
/// tracked tool call, dropping the entry when the part is gone.
fn read_tool_call(
    ctx: &mut Ctx,
    inner: &Inner,
    tool_call_id: &str,
) -> Result<Option<(ToolCall, V1Part)>, SessionError> {
    let Some(call) = ctx.toolcalls.get(tool_call_id) else {
        return Ok(None);
    };
    let (call_id, session_id, part_id) = (
        call.part_id.clone(),
        call.session_id.clone(),
        call.message_id.clone(),
    );
    let part = inner
        .deps
        .sessions
        .get_part(&session_id, &part_id, &call_id)?;
    let Some(part) = part.filter(|part| matches!(part, V1Part::Tool { .. })) else {
        ctx.toolcalls.remove(tool_call_id);
        return Ok(None);
    };
    Ok(Some((
        ctx.toolcalls
            .get(tool_call_id)
            .cloned()
            .expect("checked above"),
        part,
    )))
}

// -------------------------------------------------------------------------
// LlmError → SourceError (the observable surface of the native runtime)
// -------------------------------------------------------------------------

/// Map the native runtime's [`LlmError`] onto the ai-sdk error classes
/// `fromError` discriminates over. The native union is richer than the
/// ai-sdk one, so retryable and overflow signals are preserved
/// (documented divergence, spec §2.6).
fn source_error(error: &LlmError) -> SourceError {
    match &error.reason {
        LlmErrorReason::RateLimit { message, http, .. } => {
            let (status, headers, body, url) = http_context_parts(http.as_ref());
            SourceError::ApiCall(Box::new(ApiCallError {
                message: message.clone(),
                status_code: status.map(|s| s as u64).or(Some(429)),
                is_retryable: true,
                response_headers: headers,
                response_body: body,
                url,
            }))
        }
        LlmErrorReason::ProviderInternal {
            message,
            status,
            http,
            ..
        } => {
            let (_, headers, body, url) = http_context_parts(http.as_ref());
            SourceError::ApiCall(Box::new(ApiCallError {
                message: message.clone(),
                status_code: Some(*status as u64),
                is_retryable: true,
                response_headers: headers,
                response_body: body,
                url,
            }))
        }
        LlmErrorReason::InvalidRequest {
            message,
            classification: Some(ProviderFailureClassification::ContextOverflow),
            http,
            ..
        } => {
            let (_, headers, body, url) = http_context_parts(http.as_ref());
            SourceError::ApiCall(Box::new(ApiCallError {
                message: message.clone(),
                // 413 surfaces as ContextOverflow via `parse_api_call_error`.
                status_code: Some(413),
                is_retryable: false,
                response_headers: headers,
                response_body: body,
                url,
            }))
        }
        LlmErrorReason::Authentication { message, .. } => SourceError::LoadApiKey {
            message: message.clone(),
        },
        reason => SourceError::Error {
            message: format!(
                "{}.{}: {}",
                error.module,
                error.method,
                reason_message(reason)
            ),
        },
    }
}

#[allow(clippy::type_complexity)]
fn http_context_parts(
    http: Option<&opencode_llm::schema::errors::HttpContext>,
) -> (
    Option<f64>,
    Option<BTreeMap<String, String>>,
    Option<String>,
    Option<String>,
) {
    let (status, headers, body, url) = match http {
        None => (None, None, None, None),
        Some(http) => (
            http.response.as_ref().map(|response| response.status),
            http.response
                .as_ref()
                .map(|response| response.headers.clone()),
            http.body.clone(),
            Some(http.request.url.clone()),
        ),
    };
    (status, headers, body, url)
}

fn reason_message(reason: &LlmErrorReason) -> String {
    match reason {
        LlmErrorReason::InvalidRequest { message, .. }
        | LlmErrorReason::Authentication { message, .. }
        | LlmErrorReason::RateLimit { message, .. }
        | LlmErrorReason::QuotaExceeded { message, .. }
        | LlmErrorReason::ContentPolicy { message, .. }
        | LlmErrorReason::ProviderInternal { message, .. }
        | LlmErrorReason::Transport { message, .. }
        | LlmErrorReason::InvalidProviderOutput { message, .. }
        | LlmErrorReason::UnknownProvider { message, .. } => message.clone(),
        LlmErrorReason::NoRoute { .. } => "No LLM route".to_string(),
    }
}

/// `errorMessage` (util/error.ts) for the failure payloads the processor
/// sees: strings pass through, everything else stringifies.
fn payload_message(error: Option<&Value>) -> String {
    match error {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => "unknown error".to_string(),
        Some(other) => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// The `error instanceof PermissionV1.RejectedError ||
/// error instanceof Question.RejectedError` check (processor.ts:200-201):
/// the runtime serializes those classes as
/// `{name: "RejectedError", data: {message}}`.
fn rejected_error(error: Option<&Value>) -> Option<String> {
    let error = error?;
    if error.get("name") == Some(&Value::String("RejectedError".to_string())) {
        return error
            .get("data")
            .and_then(|data| data.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    None
}

/// Convert a storage/event defect into the `parse()` channel the
/// retry/halt pipeline understands.
fn storage_failure(error: SessionError) -> SourceError {
    SourceError::Error {
        message: error.to_string(),
    }
}

// -------------------------------------------------------------------------
// Handle — public API
// -------------------------------------------------------------------------

impl Handle {
    fn lock_ctx(&self) -> std::sync::MutexGuard<'_, Ctx> {
        self.inner
            .ctx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The assistant message under construction (live: mutations of the
    /// returned clone are not reflected; [`Handle::process`] updates it).
    pub fn message(&self) -> V1Message {
        self.lock_ctx().assistant_message.clone()
    }

    fn now_ms(&self) -> u64 {
        self.inner.deps.clock.now_ms()
    }

    /// `updateToolCall` (processor.ts:151-162).
    pub async fn update_tool_call(
        &self,
        tool_call_id: &str,
        update: impl FnOnce(V1Part) -> V1Part,
    ) -> Result<Option<V1Part>, SessionError> {
        let inner = &self.inner;
        let mut ctx = self.lock_ctx();
        let Some((call, part)) = read_tool_call(&mut ctx, inner, tool_call_id)? else {
            return Ok(None);
        };
        let part = update(part);
        inner.deps.sessions.update_part(&part)?;
        ctx.toolcalls.insert(
            tool_call_id.to_string(),
            ToolCall {
                done: call.done,
                part_id: part_id(&part).to_string(),
                message_id: part_message_id(&part).to_string(),
                session_id: part_session_id(&part).to_string(),
            },
        );
        Ok(Some(part))
    }

    /// `completeToolCall` (processor.ts:133-150).
    pub async fn complete_tool_call(
        &self,
        tool_call_id: &str,
        output: ToolCallOutput,
    ) -> Result<(), SessionError> {
        let inner = &self.inner;
        let mut ctx = self.lock_ctx();
        let Some((call, part)) = read_tool_call(&mut ctx, inner, tool_call_id)? else {
            return Ok(());
        };
        let V1Part::Tool { state, .. } = &part else {
            return Ok(());
        };
        let V1ToolState::Running { input, time, .. } = state else {
            return Ok(());
        };
        let (input, start) = (input.clone(), time.start);
        let now = self.now_ms();
        inner.deps.sessions.update_part(&V1Part::Tool {
            id: call.part_id.clone(),
            session_id: call.session_id.clone(),
            message_id: call.message_id.clone(),
            call_id: tool_call_id.to_string(),
            tool: tool_name(&part).to_string(),
            state: V1ToolState::Completed {
                input,
                output: output.output,
                title: output.title,
                metadata: metadata_jsonmap(output.metadata),
                time: ToolStateCompletedTime {
                    start,
                    end: now,
                    compacted: None,
                },
                attachments: output.attachments,
            },
            metadata: part_metadata(&part),
        })?;
        settle_tool_call(&mut ctx, tool_call_id);
        Ok(())
    }

    /// `process` (processor.ts:609-645): drive the stream. Cancellation is
    /// cooperative through the token (TS: fiber interrupt); stream and
    /// tool failures are converted by `halt` into message state.
    pub async fn process(
        &self,
        stream_input: StreamInput,
        cancel: CancellationToken,
    ) -> Result<ProcessResult, ProcessError> {
        let inner = &self.inner;
        {
            let mut ctx = self.lock_ctx();
            ctx.needs_compaction = false;
            ctx.should_break = inner
                .deps
                .config
                .experimental
                .as_ref()
                .and_then(|experimental| experimental.continue_loop_on_deny)
                != Some(true);
        }

        let policy = RetryPolicy::new(&inner.model.provider_id);
        let mut attempt: u64 = 1;
        let outcome = loop {
            match self.run_attempt(stream_input.clone(), &cancel).await {
                RunOutcome::Done => break Ok(()),
                RunOutcome::Cancelled => break Err(ProcessError::Cancelled),
                RunOutcome::Failed(source) => {
                    let step = policy.step(
                        |error| inner.parse(error),
                        &source,
                        attempt,
                        inner.deps.clock.now_ms(),
                        rand::random::<f64>(),
                    );
                    match step {
                        PolicyStep::Done => {
                            let result = inner.halt(&source).map_err(ProcessError::from);
                            break result;
                        }
                        PolicyStep::Retry { set, wait_ms } => {
                            self.set_retry_status(&set)?;
                            let slept = tokio::select! {
                                _ = tokio::time::sleep(Duration::from_millis(wait_ms.max(0.0) as u64)) => true,
                                _ = cancel.cancelled() => false,
                            };
                            if !slept {
                                break Err(ProcessError::Cancelled);
                            }
                            attempt += 1;
                            continue;
                        }
                    }
                }
            }
        };

        // `Effect.ensuring(cleanup())` — the interrupt/failure takes
        // precedence over cleanup defects.
        let cleanup_result = self.cleanup().await;
        let result = match outcome {
            Err(error) => Err(error),
            Ok(()) => cleanup_result,
        };
        result?;

        let ctx = self.lock_ctx();
        if ctx.needs_compaction {
            return Ok(ProcessResult::Compact);
        }
        let errored = match &ctx.assistant_message {
            V1Message::Assistant { error, .. } => error.is_some(),
            V1Message::User { .. } => false,
        };
        if ctx.blocked || errored {
            return Ok(ProcessResult::Stop);
        }
        Ok(ProcessResult::Continue)
    }

    fn set_retry_status(&self, set: &RetrySet) -> Result<(), ProcessError> {
        let inner = &self.inner;
        inner
            .deps
            .status
            .set(
                &inner.session_id,
                SessionStatusInfo::Retry {
                    attempt: set.attempt,
                    message: set.message.clone(),
                    action: set.action.as_ref().map(status_retry_action),
                    next: set.next,
                },
            )
            .map_err(SessionError::from)?;
        Ok(())
    }
}

fn status_retry_action(
    action: &crate::session::retry::RetryAction,
) -> opencode_schema::session_status::RetryAction {
    opencode_schema::session_status::RetryAction {
        reason: action.reason.clone(),
        provider: action.provider.clone(),
        title: action.title.clone(),
        message: action.message.clone(),
        label: action.label.clone(),
        link: action.link.clone(),
    }
}

// -------------------------------------------------------------------------
// Stream attempt (the retried Effect.gen, processor.ts:611-624)
// -------------------------------------------------------------------------

impl Handle {
    async fn run_attempt(
        &self,
        stream_input: StreamInput,
        cancel: &CancellationToken,
    ) -> RunOutcome {
        let inner = &self.inner;
        {
            let mut ctx = self.lock_ctx();
            ctx.current_text = None;
            ctx.reasoning_map = BTreeMap::new();
        }
        if let Err(error) = inner
            .deps
            .status
            .set(&inner.session_id, SessionStatusInfo::Busy)
            .map_err(SessionError::from)
        {
            return RunOutcome::Failed(storage_failure(error));
        }
        let mut stream = inner.deps.llm.stream(stream_input);
        loop {
            let item = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    // `Effect.onInterrupt` (processor.ts:625-631).
                    let halt = {
                        let mut ctx = self.lock_ctx();
                        ctx.aborted = true;
                        match &ctx.assistant_message {
                            V1Message::Assistant { error: None, .. } => Some(SourceError::Abort {
                                message: "Aborted".to_string(),
                            }),
                            _ => None,
                        }
                    };
                    if let Some(source) = halt {
                        let _ = inner.halt(&source);
                    }
                    return RunOutcome::Cancelled;
                }
                item = stream.next() => item,
            };
            match item {
                None => return RunOutcome::Done,
                Some(Ok(event)) => {
                    if let Err(error) = self.handle_event(event).await {
                        return RunOutcome::Failed(error);
                    }
                    // `Stream.takeUntil(() => ctx.needsCompaction)` — the
                    // TS chunked stream pipeline keeps pulling its tail
                    // past the takeUntil boundary, so the in-flight tool
                    // settlements still flow (the overflow tool part
                    // completes on TS). Drain to the natural end instead
                    // of dropping the settlement events mid-flight.
                }
                Some(Err(error)) => return RunOutcome::Failed(source_error(&error)),
            }
        }
    }
}

use futures::StreamExt;

// -------------------------------------------------------------------------
// Event handling (handleEvent, processor.ts:268-489)
// -------------------------------------------------------------------------

impl Handle {
    async fn handle_event(&self, event: LlmEvent) -> Result<(), SourceError> {
        match event {
            LlmEvent::ReasoningStart {
                id,
                provider_metadata,
                ..
            } => self.reasoning_start(&id, provider_metadata).await,
            LlmEvent::ReasoningDelta {
                id,
                text,
                provider_metadata,
            } => {
                self.reasoning_delta(&id, &text, provider_metadata.as_ref())
                    .await
            }
            LlmEvent::ReasoningEnd {
                id,
                provider_metadata,
            } => self.reasoning_end(&id, provider_metadata.as_ref()).await,
            LlmEvent::ToolInputStart { id, name, .. } => self.tool_input_start(&id, &name).await,
            LlmEvent::ToolInputDelta { id, name, .. } => self.tool_input(&id, &name).await,
            LlmEvent::ToolInputEnd { id, name, .. } => self.tool_input(&id, &name).await,
            LlmEvent::ToolCall {
                id,
                name,
                input,
                provider_executed,
                provider_metadata,
                ..
            } => {
                self.tool_call(&id, &name, input, provider_executed, provider_metadata)
                    .await
            }
            LlmEvent::ToolResult {
                id, name, result, ..
            } => self.tool_result(&id, &name, &result).await,
            LlmEvent::ToolError {
                id, error, message, ..
            } => {
                // processor.ts:200-201 — only `PermissionV1.RejectedError` /
                // `Question.RejectedError` block the loop; the runtime
                // serializes the class into the event's `error` value
                // (see `ToolFailure`).
                let error = match rejected_error(error.as_ref()) {
                    Some(message) => FailError::Rejected { message },
                    None => {
                        let value = error.clone().unwrap_or(Value::String(message.clone()));
                        FailError::Error {
                            message: payload_message(Some(&value)),
                        }
                    }
                };
                self.fail_tool_call_shared(&id, error).await?;
                Ok(())
            }
            LlmEvent::ProviderError { message, .. } => Err(SourceError::Error { message }),
            LlmEvent::StepStart { .. } => self.step_start().await,
            LlmEvent::StepFinish {
                reason,
                usage,
                provider_metadata,
                ..
            } => self.step_finish(reason, usage, provider_metadata).await,
            LlmEvent::TextStart {
                provider_metadata, ..
            } => self.text_start(provider_metadata).await,
            LlmEvent::TextDelta {
                text,
                provider_metadata,
                ..
            } => self.text_delta(&text, provider_metadata.as_ref()).await,
            LlmEvent::TextEnd {
                provider_metadata, ..
            } => self.text_end(provider_metadata.as_ref()).await,
            LlmEvent::Finish { .. } => Ok(()),
        }
    }

    async fn reasoning_start(
        &self,
        id: &str,
        provider_metadata: Option<opencode_llm::schema::ids::ProviderMetadata>,
    ) -> Result<(), SourceError> {
        let inner = &self.inner;
        let part = {
            let ctx = self.lock_ctx();
            if ctx.reasoning_map.contains_key(id) {
                return Ok(());
            }
            let (session_id, message_id) = assistant_location(&ctx);
            V1Part::Reasoning {
                id: new_part_id(),
                session_id,
                message_id,
                text: String::new(),
                metadata: provider_metadata_to_map(&provider_metadata),
                time: ReasoningTime {
                    start: inner.deps.clock.now_ms(),
                    end: None,
                },
            }
        };
        let mut ctx = self.lock_ctx();
        ctx.reasoning_map.insert(id.to_string(), part.clone());
        inner
            .deps
            .sessions
            .update_part(&part)
            .map_err(storage_failure)
    }

    async fn reasoning_delta(
        &self,
        id: &str,
        text: &str,
        provider_metadata: Option<&opencode_llm::schema::ids::ProviderMetadata>,
    ) -> Result<(), SourceError> {
        let inner = &self.inner;
        let (session_id, message_id, part_id) = {
            let mut ctx = self.lock_ctx();
            let Some(part) = ctx.reasoning_map.get_mut(id) else {
                // Match dev: silently drop orphan deltas.
                return Ok(());
            };
            let (session_id, message_id, part_id) = (
                part_session_id(part).to_string(),
                part_message_id(part).to_string(),
                part_id(part).to_string(),
            );
            if let V1Part::Reasoning {
                text: part_text,
                metadata: metadata_slot,
                ..
            } = part
            {
                part_text.push_str(text);
                if let Some(metadata) = provider_metadata {
                    *metadata_slot = provider_metadata_to_map(&Some(metadata.clone()));
                }
            }
            (session_id, message_id, part_id)
        };
        inner
            .deps
            .sessions
            .update_part_delta(&session_id, &message_id, &part_id, "text", text)
            .map_err(storage_failure)
    }

    async fn reasoning_end(
        &self,
        id: &str,
        provider_metadata: Option<&opencode_llm::schema::ids::ProviderMetadata>,
    ) -> Result<(), SourceError> {
        let inner = &self.inner;
        let part = {
            let mut ctx = self.lock_ctx();
            let Some(part) = ctx.reasoning_map.remove(id) else {
                return Ok(());
            };
            let mut part = part;
            if let (Some(metadata), V1Part::Reasoning { metadata: slot, .. }) =
                (provider_metadata, &mut part)
            {
                if let Some(map) = provider_metadata_to_map(&Some(metadata.clone())) {
                    *slot = Some(map);
                }
            }
            let now = inner.deps.clock.now_ms();
            finish_time(part, now)
        };
        inner
            .deps
            .sessions
            .update_part(&part)
            .map_err(storage_failure)
    }

    async fn tool_input_start(&self, id: &str, name: &str) -> Result<(), SourceError> {
        let summary = matches!(
            &self.lock_ctx().assistant_message,
            V1Message::Assistant {
                summary: Some(true),
                ..
            }
        );
        if summary {
            return Err(SourceError::Error {
                message: format!("Tool call not allowed while generating summary: {name}"),
            });
        }
        self.tool_input(id, name).await
    }

    async fn tool_input(&self, id: &str, name: &str) -> Result<(), SourceError> {
        self.ensure_tool_call(id, name, None).await
    }

    async fn tool_call(
        &self,
        call_id: &str,
        name: &str,
        input: Value,
        provider_executed: Option<bool>,
        provider_metadata: Option<opencode_llm::schema::ids::ProviderMetadata>,
    ) -> Result<(), SourceError> {
        let inner = &self.inner;
        let summary = matches!(
            &self.lock_ctx().assistant_message,
            V1Message::Assistant {
                summary: Some(true),
                ..
            }
        );
        if summary {
            return Err(SourceError::Error {
                message: format!("Tool call not allowed while generating summary: {name}"),
            });
        }
        self.ensure_tool_call(call_id, name, provider_executed)
            .await?;
        self.update_tool_call_with(call_id, name, &input, &provider_metadata)
            .await?;

        // Doom-loop detection (processor.ts:335-372).
        let (message_id, session_id, agent_name) = {
            let ctx = self.lock_ctx();
            let (message_id, session_id) = message_ids(&ctx.assistant_message);
            let agent_name = match &ctx.assistant_message {
                V1Message::Assistant { agent, .. } => agent.clone(),
                V1Message::User { .. } => String::new(),
            };
            (message_id, session_id, agent_name)
        };
        let parts = inner
            .deps
            .messages
            .parts(&message_id)
            .map_err(storage_failure)?;
        let input_record: opencode_schema::schema::JsonMap = match &input {
            Value::Object(record) => record.clone(),
            other => {
                let mut record = opencode_schema::schema::JsonMap::new();
                record.insert("value".to_string(), other.clone());
                record
            }
        };
        let input_json = serde_json::to_string(&input_record).unwrap_or_default();
        let recent = &parts[parts.len().saturating_sub(DOOM_LOOP_THRESHOLD)..];
        let mut identical = recent.len() == DOOM_LOOP_THRESHOLD;
        if identical {
            for part in recent {
                let V1Part::Tool { tool, state, .. } = part else {
                    identical = false;
                    break;
                };
                if tool != name || matches!(state, V1ToolState::Pending { .. }) {
                    identical = false;
                    break;
                }
                let state_input = match state {
                    V1ToolState::Pending { input, .. }
                    | V1ToolState::Running { input, .. }
                    | V1ToolState::Completed { input, .. }
                    | V1ToolState::Error { input, .. } => input,
                };
                if serde_json::to_string(state_input).unwrap_or_default() != input_json {
                    identical = false;
                    break;
                }
            }
        }
        if !identical {
            return Ok(());
        }
        let ruleset = inner
            .deps
            .agents
            .get(&agent_name)
            .map(|agent| agent.permission.clone())
            .unwrap_or_default();
        let ask = PermissionAsk {
            session_id: session_id.clone(),
            permission: "doom_loop".to_string(),
            patterns: vec![name.to_string()],
            always: vec![name.to_string()],
            metadata: serde_json::json!({ "tool": name, "input": input_record }),
            ruleset,
            tool: PermissionAskTool {
                message_id: message_id.clone(),
                call_id: call_id.to_string(),
            },
        };
        inner
            .deps
            .permission
            .ask(ask)
            .await
            .map_err(|error| match error {
                PermissionAskError::Rejected(message) | PermissionAskError::Other(message) => {
                    SourceError::Error { message }
                }
            })?;
        Ok(())
    }

    async fn tool_result(
        &self,
        id: &str,
        name: &str,
        result: &ToolResultValue,
    ) -> Result<(), SourceError> {
        let inner = &self.inner;
        {
            let mut ctx = self.lock_ctx();
            let existing = read_tool_call(&mut ctx, inner, id)
                .map_err(storage_failure)?
                .is_some();
            if !existing && matches!(result, ToolResultValue::Error { .. }) {
                return Ok(());
            }
        }
        if let ToolResultValue::Error { value } = result {
            let message = match value {
                Value::String(text) => text.clone(),
                other => serde_json::to_string(other).unwrap_or_default(),
            };
            self.fail_tool_call_shared(id, FailError::Error { message })
                .await?;
            return Ok(());
        }
        let output = tool_result_output(name, result);
        // No image service in M5: attachments pass through unchanged
        // (documented divergence).
        self.complete_tool_call(
            id,
            ToolCallOutput {
                title: output.title,
                metadata: output.metadata,
                output: output.output,
                attachments: output.attachments,
            },
        )
        .await
        .map_err(storage_failure)?;
        Ok(())
    }

    async fn step_start(&self) -> Result<(), SourceError> {
        let inner = &self.inner;
        if self.lock_ctx().snapshot.is_none() {
            let tracked = inner.deps.snapshot.track().await;
            let mut ctx = self.lock_ctx();
            if ctx.snapshot.is_none() {
                ctx.snapshot = tracked;
            }
        }
        let snapshot = self.lock_ctx().snapshot.clone();
        let (session_id, message_id) = {
            let ctx = self.lock_ctx();
            assistant_location(&ctx)
        };
        inner
            .deps
            .sessions
            .update_part(&V1Part::StepStart {
                id: new_part_id(),
                session_id,
                message_id,
                snapshot,
            })
            .map_err(storage_failure)
    }

    async fn step_finish(
        &self,
        reason: FinishReason,
        usage: Option<Usage>,
        provider_metadata: Option<opencode_llm::schema::ids::ProviderMetadata>,
    ) -> Result<(), SourceError> {
        let inner = &self.inner;
        let completed_snapshot = inner.deps.snapshot.track().await;

        // Finish every open reasoning part (processor.ts:443-444).
        {
            let open: Vec<String> = {
                let ctx = self.lock_ctx();
                ctx.reasoning_map.keys().cloned().collect()
            };
            for id in open {
                let part = {
                    let mut ctx = self.lock_ctx();
                    let Some(part) = ctx.reasoning_map.remove(&id) else {
                        continue;
                    };
                    finish_time(part, inner.deps.clock.now_ms())
                };
                inner
                    .deps
                    .sessions
                    .update_part(&part)
                    .map_err(storage_failure)?;
            }
        }

        let usage = get_usage(GetUsage {
            model: &inner.model.cost,
            usage: usage.as_ref().unwrap_or(&Usage::default()),
            metadata: provider_metadata.as_ref(),
        });

        {
            let mut ctx = self.lock_ctx();
            if let V1Message::Assistant {
                finish,
                cost,
                tokens,
                ..
            } = &mut ctx.assistant_message
            {
                *finish = Some(finish_reason_string(reason));
                *cost += usage.cost;
                *tokens = usage.tokens.clone();
            }
            let assistant = ctx.assistant_message.clone();
            let (session_id, message_id) = assistant_location(&ctx);
            inner
                .deps
                .sessions
                .update_part(&V1Part::StepFinish {
                    id: new_part_id(),
                    session_id,
                    message_id,
                    reason: finish_reason_string(reason),
                    snapshot: completed_snapshot,
                    cost: usage.cost,
                    tokens: usage.tokens.clone(),
                })
                .map_err(storage_failure)?;
            inner
                .deps
                .sessions
                .update_message(&assistant)
                .map_err(storage_failure)?;
        }

        // Pending snapshot → patch part (processor.ts:452-465).
        let pending_snapshot = self.lock_ctx().snapshot.take();
        if let Some(snapshot) = pending_snapshot {
            let patch = inner
                .deps
                .snapshot
                .patch(&snapshot)
                .await
                .map_err(|e| storage_failure(SessionError::from(e)))?;
            if !patch.files.is_empty() {
                let part = {
                    let ctx = self.lock_ctx();
                    let (session_id, message_id) = assistant_location(&ctx);
                    V1Part::Patch {
                        id: new_part_id(),
                        session_id,
                        message_id,
                        hash: patch.hash,
                        files: patch.files,
                    }
                };
                inner
                    .deps
                    .sessions
                    .update_part(&part)
                    .map_err(storage_failure)?;
            }
        }

        // Fork the summarizer (TS `Effect.forkIn`).
        {
            let (parent_id, session_id) = {
                let ctx = self.lock_ctx();
                (assistant_parent_id(&ctx), ctx.assistant_session_id())
            };
            let summary = inner.deps.summary.clone();
            tokio::spawn(async move {
                summary.summarize(&session_id, &parent_id).await;
            });
        }

        // Overflow (processor.ts:480-489).
        let is_summary = matches!(
            &self.lock_ctx().assistant_message,
            V1Message::Assistant {
                summary: Some(true),
                ..
            }
        );
        if !is_summary {
            let tokens = usage.tokens.clone();
            let overflow = is_overflow(&IsOverflowInput {
                cfg: &inner.deps.config,
                tokens: &tokens,
                model: &inner.model.limits,
                output_token_max: inner.model.output_token_max,
            });
            if overflow {
                self.lock_ctx().needs_compaction = true;
            }
        }
        Ok(())
    }

    async fn text_start(
        &self,
        provider_metadata: Option<opencode_llm::schema::ids::ProviderMetadata>,
    ) -> Result<(), SourceError> {
        let inner = &self.inner;
        let part = {
            let ctx = self.lock_ctx();
            let (session_id, message_id) = assistant_location(&ctx);
            V1Part::Text {
                id: new_part_id(),
                session_id,
                message_id,
                text: String::new(),
                synthetic: None,
                ignored: None,
                time: Some(TextPartTime {
                    start: inner.deps.clock.now_ms(),
                    end: None,
                }),
                metadata: provider_metadata_to_map(&provider_metadata),
            }
        };
        {
            let mut ctx = self.lock_ctx();
            ctx.current_text = Some(part.clone());
        }
        inner
            .deps
            .sessions
            .update_part(&part)
            .map_err(storage_failure)
    }

    async fn text_delta(
        &self,
        text: &str,
        provider_metadata: Option<&opencode_llm::schema::ids::ProviderMetadata>,
    ) -> Result<(), SourceError> {
        let inner = &self.inner;
        let (session_id, message_id, part_id) = {
            let mut ctx = self.lock_ctx();
            let Some(part) = ctx.current_text.as_mut() else {
                return Ok(());
            };
            if let V1Part::Text {
                text: part_text, ..
            } = part
            {
                part_text.push_str(text);
            }
            if let Some(metadata) = provider_metadata {
                if let V1Part::Text {
                    metadata: part_metadata,
                    ..
                } = part
                {
                    *part_metadata = provider_metadata_to_map(&Some(metadata.clone()));
                }
            }
            (
                part_session_id(part).to_string(),
                part_message_id(part).to_string(),
                part_id(part).to_string(),
            )
        };
        inner
            .deps
            .sessions
            .update_part_delta(&session_id, &message_id, &part_id, "text", text)
            .map_err(storage_failure)
    }

    async fn text_end(
        &self,
        provider_metadata: Option<&opencode_llm::schema::ids::ProviderMetadata>,
    ) -> Result<(), SourceError> {
        let inner = &self.inner;
        let mut part = {
            let mut ctx = self.lock_ctx();
            let Some(mut part) = ctx.current_text.take() else {
                return Ok(());
            };
            if let Some(metadata) = provider_metadata {
                if let V1Part::Text {
                    metadata: part_metadata,
                    ..
                } = &mut part
                {
                    *part_metadata = provider_metadata_to_map(&Some(metadata.clone()));
                }
            }
            part
        };
        let end = inner.deps.clock.now_ms();
        if let V1Part::Text {
            time: Some(time), ..
        } = &mut part
        {
            time.end = Some(end);
        } else if let V1Part::Text { time, .. } = &mut part {
            *time = Some(TextPartTime {
                start: end,
                end: Some(end),
            });
        }
        inner
            .deps
            .sessions
            .update_part(&part)
            .map_err(storage_failure)
    }
}

// -------------------------------------------------------------------------
// Tool-call lifecycle
// -------------------------------------------------------------------------

impl Handle {
    /// `ensureToolCall` (processor.ts:219-243).
    async fn ensure_tool_call(
        &self,
        tool_call_id: &str,
        name: &str,
        provider_executed: Option<bool>,
    ) -> Result<(), SourceError> {
        let inner = &self.inner;
        let created = {
            let mut ctx = self.lock_ctx();
            let existing =
                read_tool_call(&mut ctx, inner, tool_call_id).map_err(storage_failure)?;
            if let Some((call, part)) = existing {
                let V1Part::Tool { metadata, .. } = &part else {
                    return Ok(());
                };
                let already = metadata.as_ref().and_then(|m| m.get("providerExecuted"))
                    == Some(&Value::Bool(true));
                if provider_executed != Some(true) || already {
                    let _ = call;
                    return Ok(());
                }
                let mut merged = metadata.clone().unwrap_or_default();
                merged.insert("providerExecuted".to_string(), Value::Bool(true));
                let mut part = part.clone();
                if let V1Part::Tool { metadata, .. } = &mut part {
                    *metadata = Some(merged);
                }
                inner
                    .deps
                    .sessions
                    .update_part(&part)
                    .map_err(storage_failure)?;
                return Ok(());
            }

            let (session_id, message_id) = assistant_location(&ctx);
            let part = V1Part::Tool {
                id: new_part_id(),
                session_id,
                message_id,
                call_id: tool_call_id.to_string(),
                tool: name.to_string(),
                state: V1ToolState::Pending {
                    input: opencode_schema::schema::JsonMap::new(),
                    raw: String::new(),
                },
                metadata: if provider_executed == Some(true) {
                    Some(single_entry("providerExecuted", Value::Bool(true)))
                } else {
                    None
                },
            };
            ctx.toolcalls.insert(
                tool_call_id.to_string(),
                ToolCall {
                    done: Arc::new(Deferred::new()),
                    part_id: part_id(&part).to_string(),
                    message_id: part_message_id(&part).to_string(),
                    session_id: part_session_id(&part).to_string(),
                },
            );
            part
        };
        inner
            .deps
            .sessions
            .update_part(&created)
            .map_err(storage_failure)
    }

    /// The `tool-call` state update (processor.ts:286-306).
    async fn update_tool_call_with(
        &self,
        tool_call_id: &str,
        name: &str,
        input: &Value,
        provider_metadata: &Option<opencode_llm::schema::ids::ProviderMetadata>,
    ) -> Result<(), SourceError> {
        let input_record: opencode_schema::schema::JsonMap = match input {
            Value::Object(record) => record.clone(),
            other => {
                let mut record = opencode_schema::schema::JsonMap::new();
                record.insert("value".to_string(), other.clone());
                record
            }
        };
        self.update_tool_call(tool_call_id, |part| {
            let mut part = part;
            if let V1Part::Tool {
                tool,
                state,
                metadata,
                ..
            } = &mut part
            {
                *tool = name.to_string();
                *state = match state {
                    V1ToolState::Running {
                        title,
                        metadata,
                        time,
                        ..
                    } => V1ToolState::Running {
                        input: input_record.clone(),
                        title: title.clone(),
                        metadata: metadata.clone(),
                        time: *time,
                    },
                    _ => V1ToolState::Running {
                        input: input_record.clone(),
                        title: None,
                        metadata: None,
                        time: ToolStateRunningTime {
                            start: self.now_ms(),
                        },
                    },
                };
                *metadata = match metadata.as_ref().and_then(|m| m.get("providerExecuted"))
                    == Some(&Value::Bool(true))
                {
                    true => {
                        let mut merged =
                            provider_metadata_to_map(provider_metadata).unwrap_or_default();
                        merged.insert("providerExecuted".to_string(), Value::Bool(true));
                        Some(merged)
                    }
                    false => provider_metadata_to_map(provider_metadata),
                };
            }
            part
        })
        .await
        .map_err(storage_failure)?;
        Ok(())
    }

    /// `failToolCall` (processor.ts:166-204).
    async fn fail_tool_call_shared(
        &self,
        tool_call_id: &str,
        error: FailError,
    ) -> Result<bool, SourceError> {
        let inner = &self.inner;
        let mut ctx = self.lock_ctx();
        let Some((call, part)) =
            read_tool_call(&mut ctx, inner, tool_call_id).map_err(storage_failure)?
        else {
            return Ok(false);
        };
        let V1Part::Tool { state, .. } = &part else {
            return Ok(false);
        };
        let V1ToolState::Running { input, time, .. } = state else {
            return Ok(false);
        };
        let (input, start, state_metadata) = (input.clone(), time.start, state_metadata(state));
        inner
            .deps
            .sessions
            .update_part(&V1Part::Tool {
                id: call.part_id.clone(),
                session_id: call.session_id.clone(),
                message_id: call.message_id.clone(),
                call_id: tool_call_id.to_string(),
                tool: tool_name(&part).to_string(),
                state: V1ToolState::Error {
                    input,
                    error: match &error {
                        FailError::Rejected { message } | FailError::Error { message } => {
                            message.clone()
                        }
                    },
                    metadata: state_metadata,
                    time: ToolStateErrorTime {
                        start,
                        end: inner.deps.clock.now_ms(),
                    },
                },
                metadata: part_metadata(&part),
            })
            .map_err(storage_failure)?;
        if matches!(error, FailError::Rejected { .. }) {
            ctx.blocked = ctx.should_break;
        }
        settle_tool_call(&mut ctx, tool_call_id);
        Ok(true)
    }
}

fn single_entry(key: &str, value: Value) -> opencode_schema::schema::JsonMap {
    let mut map = opencode_schema::schema::JsonMap::new();
    map.insert(key.to_string(), value);
    map
}

fn state_metadata(state: &V1ToolState) -> Option<opencode_schema::schema::JsonMap> {
    match state {
        V1ToolState::Running { metadata, .. } => metadata.clone(),
        _ => None,
    }
}

fn finish_time(mut part: V1Part, end: u64) -> V1Part {
    if let V1Part::Reasoning { time, .. } = &mut part {
        time.end = Some(end);
    }
    part
}

/// `toolResultOutput` (processor.ts:246-265).
fn tool_result_output(name: &str, result: &ToolResultValue) -> ToolCallOutput {
    let value = match result {
        ToolResultValue::Json { value }
        | ToolResultValue::Text { value }
        | ToolResultValue::Error { value } => Some(value),
        ToolResultValue::Content { .. } => None,
    };
    // The record shape — a successful tool `execute` result.
    if let Some(Value::Object(record)) = value {
        if let Some(Value::String(output)) = record.get("output") {
            return ToolCallOutput {
                title: record
                    .get("title")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| name.to_string()),
                metadata: record
                    .get("metadata")
                    .filter(|metadata| metadata.is_object())
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default())),
                output: output.clone(),
                attachments: record
                    .get("attachments")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| {
                                serde_json::from_value::<V1FilePart>(item.clone()).ok()
                            })
                            .collect::<Vec<V1FilePart>>()
                    }),
            };
        }
    }
    ToolCallOutput {
        title: name.to_string(),
        metadata: match result {
            ToolResultValue::Json { value } if value.is_object() => value.clone(),
            _ => Value::Object(Default::default()),
        },
        output: match value {
            Some(Value::String(text)) => text.clone(),
            Some(other) => serde_json::to_string(other).unwrap_or_default(),
            None => String::new(),
        },
        attachments: None,
    }
}

// -------------------------------------------------------------------------
// Cleanup and halt
// -------------------------------------------------------------------------

impl Handle {
    /// `cleanup` (processor.ts:492-553).
    async fn cleanup(&self) -> Result<(), ProcessError> {
        let inner = &self.inner;
        // 1. Pending snapshot → patch part.
        let pending = self.lock_ctx().snapshot.take();
        if let Some(snapshot) = pending {
            let patch = inner
                .deps
                .snapshot
                .patch(&snapshot)
                .await
                .map_err(SessionError::from)?;
            if !patch.files.is_empty() {
                let part = {
                    let ctx = self.lock_ctx();
                    let (session_id, message_id) = assistant_location(&ctx);
                    V1Part::Patch {
                        id: new_part_id(),
                        session_id,
                        message_id,
                        hash: patch.hash,
                        files: patch.files,
                    }
                };
                inner.deps.sessions.update_part(&part)?;
            }
        }

        // 2. Current text.
        if let Some(part) = self.lock_ctx().current_text.take() {
            let mut part = part;
            let end = inner.deps.clock.now_ms();
            if let V1Part::Text {
                time: Some(time), ..
            } = &mut part
            {
                time.end = Some(end);
            } else if let V1Part::Text { time, .. } = &mut part {
                *time = Some(TextPartTime {
                    start: end,
                    end: Some(end),
                });
            }
            inner.deps.sessions.update_part(&part)?;
        }

        // 3. Open reasoning parts.
        let open: Vec<V1Part> = {
            let mut ctx = self.lock_ctx();
            std::mem::take(&mut ctx.reasoning_map)
                .into_values()
                .collect()
        };
        for part in open {
            let end = inner.deps.clock.now_ms();
            inner.deps.sessions.update_part(&finish_time(part, end))?;
        }

        // 4. Await tool settlement (250ms each).
        let calls: Vec<ToolCall> = {
            let ctx = self.lock_ctx();
            ctx.toolcalls.values().cloned().collect()
        };
        for call in calls {
            let _ = tokio::time::timeout(Duration::from_millis(250), call.done.get()).await;
        }

        // 5. Mark interrupted tool calls.
        let remaining: Vec<(String, ToolCall)> = {
            let mut ctx = self.lock_ctx();
            std::mem::take(&mut ctx.toolcalls).into_iter().collect()
        };
        for (tool_call_id, call) in remaining {
            let Ok(Some(part)) =
                inner
                    .deps
                    .sessions
                    .get_part(&call.session_id, &call.message_id, &call.part_id)
            else {
                continue;
            };
            let V1Part::Tool {
                state, metadata, ..
            } = &part
            else {
                continue;
            };
            let end = inner.deps.clock.now_ms();
            let interrupted = match state {
                V1ToolState::Pending { input, .. }
                | V1ToolState::Running { input, .. }
                | V1ToolState::Completed { input, .. }
                | V1ToolState::Error { input, .. } => input.clone(),
            };
            let state_metadata = state_metadata(state);
            let mut error_metadata = state_metadata.clone().unwrap_or_default();
            error_metadata.insert("interrupted".to_string(), Value::Bool(true));
            let start = match state {
                V1ToolState::Pending { .. } => end,
                V1ToolState::Running { time, .. } => time.start,
                V1ToolState::Completed { time, .. } => time.start,
                V1ToolState::Error { time, .. } => time.start,
            };
            inner.deps.sessions.update_part(&V1Part::Tool {
                id: call.part_id.clone(),
                session_id: call.session_id.clone(),
                message_id: call.message_id.clone(),
                call_id: tool_call_id,
                tool: tool_name(&part).to_string(),
                state: V1ToolState::Error {
                    input: interrupted,
                    error: "Tool execution aborted".to_string(),
                    metadata: Some(error_metadata),
                    time: ToolStateErrorTime { start, end },
                },
                metadata: metadata.clone(),
            })?;
        }

        // 6. Complete the message.
        {
            let mut ctx = self.lock_ctx();
            let now = inner.deps.clock.now_ms();
            if let V1Message::Assistant { time, .. } = &mut ctx.assistant_message {
                if time.completed.is_none() {
                    time.completed = Some(now);
                }
            }
            let assistant = ctx.assistant_message.clone();
            inner.deps.sessions.update_message(&assistant)?;
        }
        Ok(())
    }
}

impl Inner {
    fn parse(&self, source: &SourceError) -> AssistantError {
        let aborted = self
            .ctx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .aborted;
        from_error(
            source,
            FromErrorCtx {
                provider_id: self.model.provider_id.clone(),
                aborted,
            },
        )
    }

    /// `halt` (processor.ts:555-594).
    fn halt(&self, source: &SourceError) -> Result<(), SessionError> {
        let error = self.parse(source);
        let mut ctx = self
            .ctx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let publish_error =
            |session_id: &str, error: &AssistantError| -> Result<(), SessionError> {
                self.deps.events.publish(
                    &SESSION_ERROR,
                    serde_json::json!({
                        "sessionID": session_id,
                        "error": serde_json::to_value(error).unwrap_or(Value::Null),
                    }),
                    PublishOptions::default(),
                )?;
                Ok(())
            };

        if matches!(error, AssistantError::ContextOverflow { .. }) {
            let auto_disabled = self
                .deps
                .config
                .compaction
                .as_ref()
                .and_then(|compaction| compaction.auto)
                == Some(false);
            let is_summary = matches!(
                &ctx.assistant_message,
                V1Message::Assistant {
                    summary: Some(true),
                    ..
                }
            );
            if auto_disabled && !is_summary {
                if let V1Message::Assistant {
                    error: error_slot,
                    finish,
                    ..
                } = &mut ctx.assistant_message
                {
                    *error_slot = Some(error.clone());
                    *finish = Some("error".to_string());
                }
                publish_error(&self.session_id.clone(), &error)?;
                self.deps
                    .status
                    .set(&self.session_id, SessionStatusInfo::Idle)?;
                return Ok(());
            }
            ctx.needs_compaction = true;
            publish_error(&self.session_id.clone(), &error)?;
            return Ok(());
        }

        if let V1Message::Assistant {
            error: error_slot, ..
        } = &mut ctx.assistant_message
        {
            *error_slot = Some(error.clone());
        }
        publish_error(&self.session_id.clone(), &error)?;
        self.deps
            .status
            .set(&self.session_id, SessionStatusInfo::Idle)?;
        Ok(())
    }
}

impl Ctx {
    fn assistant_session_id(&self) -> String {
        match &self.assistant_message {
            V1Message::User { session_id, .. } | V1Message::Assistant { session_id, .. } => {
                session_id.clone()
            }
        }
    }
}
