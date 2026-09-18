//! Session compaction — port of `session/compaction.ts` plus the
//! `buildPrompt` strings from `core/src/session/compaction.ts` (vendored
//! verbatim under `prompt/`).
//!
//! The tail-selection walk (`select`/`splitTurn`/`turns`), the serializer
//! and the prune scan are pure functions; `process` drives the M5.4
//! processor with an empty tool set over the `compaction` agent, and
//! `create` appends the compaction user message. Plugin hooks
//! (`experimental.session.compacting`,
//! `experimental.chat.messages.transform`,
//! `experimental.compaction.autocontinue`) are no-op seams (spec §2.6).

use std::collections::HashSet;
use std::sync::Arc;

use opencode_schema::session_v1::{
    AssistantError, AssistantTime, UserTime, V1FilePart, V1Message, V1Part, V1Path, V1StepTokens,
    V1TokenCache, V1UserModel,
};
use tokio_util::sync::CancellationToken;

use crate::config::schema::Config;
use crate::event::bus::{EventBus, PublishOptions};
use crate::Clock;

use crate::session::error::SessionError;
use crate::session::event_definitions::SESSION_COMPACTED;
use crate::session::ids::{MessageId, PartId};
use crate::session::llm::{LlmStream, StreamInput};
use crate::session::message::{message_id, MessageStore, WithParts};
use crate::session::overflow::{is_overflow, usable, IsOverflowInput, ModelLimits, UsableInput};
use crate::session::processor::{
    AskPermission, ProcessResult, Processor, ProcessorDeps, ProcessorInput, SummarySummarize,
};
use crate::session::r#loop::{
    Compaction as CompactionService, CompactionCreate, CompactionProcess, ModelSource,
    ResolvedModel,
};
use crate::session::render::is_media;
use crate::session::snapshot::Snapshot;
use crate::session::status::SessionStatusService;
use crate::session::store::SessionStore;
use crate::tool::def::InstanceContext;

use opencode_llm::schema::messages::Message;

// ---------------------------------------------------------------------------
// Constants (compaction.ts:28-33)
// ---------------------------------------------------------------------------

/// `PRUNE_MINIMUM` (compaction.ts:28).
pub const PRUNE_MINIMUM: f64 = 20_000.0;
/// `PRUNE_PROTECT` (compaction.ts:29).
pub const PRUNE_PROTECT: f64 = 40_000.0;
/// `TOOL_OUTPUT_MAX_CHARS` (compaction.ts:30).
pub const TOOL_OUTPUT_MAX_CHARS: usize = 2_000;
/// `PRUNE_PROTECTED_TOOLS` (compaction.ts:31).
pub const PRUNE_PROTECTED_TOOLS: [&str; 1] = ["skill"];
/// `MIN_PRESERVE_RECENT_TOKENS` (compaction.ts:32).
pub const MIN_PRESERVE_RECENT_TOKENS: f64 = 2_000.0;
/// `MAX_PRESERVE_RECENT_TOKENS` (compaction.ts:33).
pub const MAX_PRESERVE_RECENT_TOKENS: f64 = 15_000.0;

/// `SUMMARY_TEMPLATE` (core/session/compaction.ts:16-46) — vendored
/// verbatim in `prompt/compaction-summary-template.txt`.
pub const SUMMARY_TEMPLATE: &str = include_str!("prompt/compaction-summary-template.txt");

/// `SUMMARY_UPDATE_INSTRUCTIONS` (core/session/compaction.ts:47-55) —
/// vendored verbatim in `prompt/compaction-summary-update-instructions.txt`.
pub const SUMMARY_UPDATE_INSTRUCTIONS: &str =
    include_str!("prompt/compaction-summary-update-instructions.txt");

/// The overflow preamble of the `experimental.compaction.autocontinue`
/// message (compaction.ts:529).
const AUTOCONTINUE_OVERFLOW_PREAMBLE: &str = "The previous request exceeded the provider's size limit due to large media attachments. The conversation was compacted and media files were removed from context. If the user was asking about attached images or files, explain that the attachments were too large to process and suggest they try again with smaller or fewer files.\n\n";
/// The auto-continue tail (compaction.ts:531).
const AUTOCONTINUE_TEXT: &str = "Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed.";

/// `process` outcome — TS `"continue" | "stop"` (compaction.ts:177).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionOutcome {
    Continue,
    Stop,
}

// ---------------------------------------------------------------------------
// Token.estimate (core/util/token.ts:1-3)
// ---------------------------------------------------------------------------

/// `Token.estimate` — `Math.max(0, Math.round(len / 4))` over UTF-16
/// code units.
pub fn token_estimate(input: &str) -> f64 {
    let units = input.encode_utf16().count();
    (units as f64 / 4.0).round().max(0.0)
}

// ---------------------------------------------------------------------------
// Serialization (compaction.ts:51-95)
// ---------------------------------------------------------------------------

/// `truncate` (compaction.ts:51-52): `slice` counts UTF-16 code units.
fn truncate(value: &str) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    if units.len() <= TOOL_OUTPUT_MAX_CHARS {
        return value.to_string();
    }
    format!(
        "{}\n[truncated]",
        String::from_utf16_lossy(&units[..TOOL_OUTPUT_MAX_CHARS])
    )
}

/// `JSON.stringify(part.state.input)` — the JsonMap serialization also
/// used by the doom-loop check (M5.4).
fn json_input(input: &opencode_schema::schema::JsonMap) -> String {
    serde_json::to_string(input).unwrap_or_default()
}

/// `serialize` (compaction.ts:54-85).
pub fn serialize(message: &WithParts) -> String {
    if matches!(message.info, V1Message::User { .. }) {
        let text = message
            .parts
            .iter()
            .filter_map(|part| match part {
                V1Part::Text { text, ignored, .. } if *ignored != Some(true) => Some(text.clone()),
                _ => None,
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let files = message
            .parts
            .iter()
            .filter_map(|part| match part {
                V1Part::File { mime, filename, .. } => Some(format!(
                    "[Attached {mime}: {}]",
                    filename.as_deref().unwrap_or("file")
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut lines = Vec::new();
        if !text.is_empty() {
            lines.push(format!("[User]: {text}"));
        }
        lines.extend(files);
        return lines.join("\n");
    }
    message
        .parts
        .iter()
        .filter_map(|part| match part {
            V1Part::Text { text, .. } => {
                if text.is_empty() {
                    None
                } else {
                    Some(vec![format!("[Assistant]: {text}")])
                }
            }
            V1Part::Reasoning { text, .. } => {
                if text.is_empty() {
                    None
                } else {
                    Some(vec![format!("[Assistant reasoning]: {text}")])
                }
            }
            V1Part::Tool { state, tool, .. } => {
                let call = format!(
                    "[Assistant tool call]: {tool}({})",
                    match state {
                        opencode_schema::session_v1::V1ToolState::Pending { input, .. }
                        | opencode_schema::session_v1::V1ToolState::Running { input, .. }
                        | opencode_schema::session_v1::V1ToolState::Completed { input, .. }
                        | opencode_schema::session_v1::V1ToolState::Error { input, .. } => {
                            json_input(input)
                        }
                    }
                );
                match state {
                    opencode_schema::session_v1::V1ToolState::Completed {
                        output,
                        time,
                        attachments,
                        ..
                    } => {
                        let attachment_lines = attachments
                            .as_deref()
                            .unwrap_or_default()
                            .iter()
                            .map(|item| match item {
                                V1FilePart::File { mime, filename, .. } => format!(
                                    "[Attached {mime}: {}]",
                                    filename.as_deref().unwrap_or("file")
                                ),
                            })
                            .collect::<Vec<_>>();
                        let joined = [vec![output.clone()], attachment_lines].concat().join("\n");
                        let output = if time.compacted.is_some() {
                            "[Old tool result content cleared]".to_string()
                        } else {
                            truncate(&joined)
                        };
                        Some(vec![call, format!("[Tool result]: {output}")])
                    }
                    opencode_schema::session_v1::V1ToolState::Error { error, .. } => {
                        Some(vec![call, format!("[Tool error]: {error}")])
                    }
                    _ => Some(vec![call]),
                }
            }
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>()
        .join("\n")
}

/// `summaryText` (compaction.ts:87-95).
pub fn summary_text(message: &WithParts) -> Option<String> {
    let text = message
        .parts
        .iter()
        .filter_map(|part| match part {
            V1Part::Text { text, .. } => Some(text.trim().to_string()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
        .trim()
        .to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

// ---------------------------------------------------------------------------
// Turn selection (compaction.ts:97-163)
// ---------------------------------------------------------------------------

/// `CompletedCompaction` (compaction.ts:45-49).
#[derive(Debug, Clone, PartialEq)]
pub struct CompletedCompaction {
    pub user_index: usize,
    pub assistant_index: usize,
    pub summary: Option<String>,
}

/// `completedCompactions` (compaction.ts:97-113).
pub fn completed_compactions(messages: &[WithParts]) -> Vec<CompletedCompaction> {
    use std::collections::BTreeMap;
    let mut users: BTreeMap<String, usize> = BTreeMap::new();
    for (i, msg) in messages.iter().enumerate() {
        if !matches!(msg.info, V1Message::User { .. }) {
            continue;
        }
        if !msg
            .parts
            .iter()
            .any(|part| matches!(part, V1Part::Compaction { .. }))
        {
            continue;
        }
        users.insert(message_id(&msg.info).to_string(), i);
    }
    let mut result = Vec::new();
    for (assistant_index, msg) in messages.iter().enumerate() {
        let V1Message::Assistant {
            summary: Some(true),
            finish: Some(_),
            error: None,
            parent_id,
            ..
        } = &msg.info
        else {
            continue;
        };
        let Some(&user_index) = users.get(parent_id) else {
            continue;
        };
        result.push(CompletedCompaction {
            user_index,
            assistant_index,
            summary: summary_text(msg),
        });
    }
    result
}

/// `preserveRecentBudget` (compaction.ts:115-120). The TS passes
/// `{cfg, model}` to `usable`, so `outputTokenMax` is absent there.
pub fn preserve_recent_budget(cfg: &Config, model: &ModelLimits) -> f64 {
    match cfg
        .compaction
        .as_ref()
        .and_then(|compaction| compaction.preserve_recent_tokens)
    {
        Some(tokens) => tokens as f64,
        None => {
            let usable = usable(&UsableInput {
                cfg,
                model,
                output_token_max: None,
            });
            MAX_PRESERVE_RECENT_TOKENS.min(MIN_PRESERVE_RECENT_TOKENS.max((usable * 0.25).floor()))
        }
    }
}

/// `Turn` (compaction.ts:34-38).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub start: usize,
    pub end: usize,
    pub id: String,
}

/// `Tail` (compaction.ts:40-43).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    pub start: usize,
    pub id: String,
}

/// `turns` (compaction.ts:122-138).
pub fn turns(messages: &[WithParts]) -> Vec<Turn> {
    let mut result: Vec<Turn> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        if !matches!(msg.info, V1Message::User { .. }) {
            continue;
        }
        if msg
            .parts
            .iter()
            .any(|part| matches!(part, V1Part::Compaction { .. }))
        {
            continue;
        }
        result.push(Turn {
            start: i,
            end: messages.len(),
            id: message_id(&msg.info).to_string(),
        });
    }
    for i in 0..result.len().saturating_sub(1) {
        result[i].end = result[i + 1].start;
    }
    result
}

/// `estimate` input (compaction.ts:216-221).
pub struct SelectInput<'a> {
    pub messages: &'a [WithParts],
    pub cfg: &'a Config,
    pub model: &'a ResolvedModel,
}

/// `select` output (compaction.ts:223-269).
#[derive(Debug, Clone)]
pub struct Selected {
    pub head: Vec<WithParts>,
    pub tail_start_id: Option<String>,
}

/// `estimate` (compaction.ts:215-221): render to model messages and
/// `Token.estimate(JSON.stringify(msgs))`.
fn estimate(messages: &[WithParts], model: &ResolvedModel) -> f64 {
    let msgs = crate::session::render::to_model_messages(messages, &model.render(), None);
    let json = serde_json::to_string(&msgs).unwrap_or_default();
    token_estimate(&json)
}

/// `splitTurn` (compaction.ts:140-163).
fn split_turn(
    messages: &[WithParts],
    turn: &Turn,
    model: &ResolvedModel,
    budget: f64,
) -> Option<Tail> {
    if budget <= 0.0 {
        return None;
    }
    if turn.end - turn.start <= 1 {
        return None;
    }
    for start in (turn.start + 1)..turn.end {
        let size = estimate(&messages[start..turn.end], model);
        if size > budget {
            continue;
        }
        return Some(Tail {
            start,
            id: message_id(&messages[start].info).to_string(),
        });
    }
    None
}

/// `select` (compaction.ts:223-269 — binding): the tail-turn budget walk.
pub fn select(input: &SelectInput<'_>) -> Selected {
    let keep_all = || Selected {
        head: input.messages.to_vec(),
        tail_start_id: None,
    };
    let limit = input.cfg.compaction.as_ref().and_then(|c| c.tail_turns);
    if let Some(0) = limit {
        // `limit <= 0` disables tail selection.
        return keep_all();
    }
    let budget = preserve_recent_budget(input.cfg, &input.model.limits);
    let all = turns(input.messages);
    if all.is_empty() {
        return keep_all();
    }
    let recent: &[Turn] = match limit {
        None => &all,
        Some(limit) => &all[all.len().saturating_sub(limit as usize)..],
    };

    let mut total = 0.0;
    let mut keep: Option<Tail> = None;
    for turn in recent.iter().rev() {
        // estimate lazily so cost stays proportional to the retained tail,
        // not the whole session
        let size = estimate(&input.messages[turn.start..turn.end], input.model);
        if total + size <= budget {
            total += size;
            keep = Some(Tail {
                start: turn.start,
                id: turn.id.clone(),
            });
            continue;
        }
        let remaining = budget - total;
        let split = split_turn(input.messages, turn, input.model, remaining);
        if let Some(split) = split {
            keep = Some(split);
        } else if keep.is_none() {
            tracing::info!(budget, size, total, "tail fallback");
        }
        break;
    }

    match keep {
        Some(keep) if keep.start != 0 => Selected {
            head: input.messages[..keep.start].to_vec(),
            tail_start_id: Some(keep.id),
        },
        _ => keep_all(),
    }
}

// ---------------------------------------------------------------------------
// buildPrompt (core/session/compaction.ts:160-174)
// ---------------------------------------------------------------------------

/// `buildPrompt` input.
pub struct BuildPrompt<'a> {
    pub previous_summary: Option<&'a str>,
    pub context: Vec<String>,
}

/// `buildPrompt` (core/session/compaction.ts:160-174).
pub fn build_prompt(input: &BuildPrompt<'_>) -> String {
    let conversation = format!(
        "Here is the conversation so far:\n\n<conversation>\n{}\n</conversation>",
        input.context.join("\n\n")
    );
    match input.previous_summary {
        None => [
            conversation,
            "Create a new anchored summary from the conversation history in the <conversation> tags above so another coding agent can continue the work.".to_string(),
            SUMMARY_TEMPLATE.to_string(),
        ]
        .join("\n\n"),
        Some(previous) => [
            conversation,
            format!(
                "Here is the summary of the conversation before the <conversation> above:\n\n<prior-summary>\n{previous}\n</prior-summary>"
            ),
            SUMMARY_UPDATE_INSTRUCTIONS.to_string(),
            SUMMARY_TEMPLATE.to_string(),
        ]
        .join("\n\n"),
    }
}

// ---------------------------------------------------------------------------
// Service (compaction.ts:165-582)
// ---------------------------------------------------------------------------

/// Everything the compaction service closes over.
pub struct CompactionDeps {
    pub sessions: SessionStore,
    pub messages: MessageStore,
    pub events: Arc<EventBus>,
    pub status: Arc<SessionStatusService>,
    pub agents: crate::session::agents::AgentRegistry,
    pub snapshot: Arc<dyn Snapshot>,
    pub llm: Arc<dyn LlmStream>,
    pub permission: Arc<dyn AskPermission>,
    pub summary: Arc<dyn SummarySummarize>,
    pub models: Arc<dyn ModelSource>,
    pub config: Arc<Config>,
    pub clock: Arc<dyn Clock>,
    pub instance: InstanceContext,
    /// `RuntimeFlags.outputTokenMax` — the compaction `isOverflow` flag.
    pub output_token_max: Option<f64>,
    /// `RuntimeFlags.project` (`x-opencode-project` header).
    pub project_id: Option<String>,
    /// `RuntimeFlags.client` (`x-opencode-client` header).
    pub client: String,
}

/// `SessionCompaction.Service` (compaction.ts:165-191).
pub struct SessionCompaction {
    deps: CompactionDeps,
}

impl SessionCompaction {
    pub fn new(deps: CompactionDeps) -> SessionCompaction {
        SessionCompaction { deps }
    }

    fn storage_error(message: impl Into<String>) -> SessionError {
        crate::CoreError::Storage(message.into()).into()
    }

    /// `isOverflow` (compaction.ts:203-213): the overflow check with the
    /// runtime flag.
    pub fn is_overflow(&self, tokens: &V1StepTokens, model: &ModelLimits) -> bool {
        is_overflow(&IsOverflowInput {
            cfg: &self.deps.config,
            tokens,
            model,
            output_token_max: self.deps.output_token_max,
        })
    }

    /// `prune` (compaction.ts:273-317): walk backwards until
    /// `PRUNE_PROTECT` tokens of completed tool outputs, then erase
    /// (`time.compacted`) the outputs of older tool calls.
    pub fn prune(&self, session_id: &str) -> Result<(), SessionError> {
        if self
            .deps
            .config
            .compaction
            .as_ref()
            .and_then(|compaction| compaction.prune)
            != Some(true)
        {
            return Ok(());
        }
        tracing::info!("pruning");

        let msgs = match self.deps.sessions.messages(session_id, None) {
            Ok(msgs) => msgs,
            // catchIf(NotFoundError, () => undefined)
            Err(SessionError::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };

        let mut total = 0.0;
        let mut pruned = 0.0;
        let mut to_prune: Vec<V1Part> = Vec::new();
        let mut count: u64 = 0;

        'walk: for msg in msgs.iter().rev() {
            if matches!(msg.info, V1Message::User { .. }) {
                count += 1;
            }
            if count < 2 {
                continue;
            }
            if matches!(
                &msg.info,
                V1Message::Assistant {
                    summary: Some(true),
                    ..
                }
            ) {
                break;
            }
            for part in msg.parts.iter().rev() {
                let V1Part::Tool { state, tool, .. } = part else {
                    continue;
                };
                let opencode_schema::session_v1::V1ToolState::Completed { output, time, .. } =
                    state
                else {
                    continue;
                };
                if PRUNE_PROTECTED_TOOLS.contains(&tool.as_str()) {
                    continue;
                }
                if time.compacted.is_some() {
                    break 'walk;
                }
                let estimate = token_estimate(output);
                total += estimate;
                if total <= PRUNE_PROTECT {
                    continue;
                }
                pruned += estimate;
                to_prune.push(part.clone());
            }
        }

        tracing::info!(pruned, total, "found");
        if pruned > PRUNE_MINIMUM {
            let now = self.deps.clock.now_ms();
            let count = to_prune.len();
            for part in to_prune {
                let mut value = serde_json::to_value(&part).unwrap_or_default();
                if let Some(object) = value.as_object_mut() {
                    if let Some(state) = object
                        .get_mut("state")
                        .and_then(|state| state.as_object_mut())
                    {
                        if let Some(time) =
                            state.get_mut("time").and_then(|time| time.as_object_mut())
                        {
                            time.insert("compacted".to_string(), serde_json::Value::from(now));
                        }
                    }
                }
                let updated: V1Part =
                    serde_json::from_value(value).unwrap_or_else(|_| part.clone());
                self.deps.sessions.update_part(&updated)?;
            }
            tracing::info!(count, "pruned");
        }
        Ok(())
    }

    /// `process` (compaction.ts:319-557 — binding).
    pub async fn process(
        &self,
        input: CompactionProcess,
    ) -> Result<CompactionOutcome, SessionError> {
        let Some(parent) = input
            .messages
            .iter()
            .rev()
            .find(|msg| message_id(&msg.info) == input.parent_id)
        else {
            return Err(Self::storage_error(format!(
                "Compaction parent must be a user message: {}",
                input.parent_id
            )));
        };
        if !matches!(parent.info, V1Message::User { .. }) {
            return Err(Self::storage_error(format!(
                "Compaction parent must be a user message: {}",
                input.parent_id
            )));
        }
        let user_message = parent.info.clone();
        let user_model = match &user_message {
            V1Message::User { model, .. } => model.clone(),
            V1Message::Assistant { .. } => unreachable!("role checked above"),
        };
        let user_agent = match &user_message {
            V1Message::User { agent, .. } => agent.clone(),
            V1Message::Assistant { .. } => unreachable!("role checked above"),
        };
        let compaction_part = parent
            .parts
            .iter()
            .find(|part| matches!(part, V1Part::Compaction { .. }))
            .cloned();

        let mut messages = input.messages.clone();
        let mut replay: Option<WithParts> = None;
        if input.overflow == Some(true) {
            let idx = input
                .messages
                .iter()
                .position(|msg| message_id(&msg.info) == input.parent_id)
                .unwrap_or(0);
            for i in (0..idx).rev() {
                let msg = &input.messages[i];
                if matches!(msg.info, V1Message::User { .. })
                    && !msg
                        .parts
                        .iter()
                        .any(|part| matches!(part, V1Part::Compaction { .. }))
                {
                    replay = Some(msg.clone());
                    messages = input.messages[..i].to_vec();
                    break;
                }
            }
            let has_content = replay.is_some()
                && messages.iter().any(|msg| {
                    matches!(msg.info, V1Message::User { .. })
                        && !msg
                            .parts
                            .iter()
                            .any(|part| matches!(part, V1Part::Compaction { .. }))
                });
            if !has_content {
                replay = None;
                messages = input.messages.clone();
            }
        }

        let agent = self
            .deps
            .agents
            .get("compaction")
            .cloned()
            .ok_or_else(|| Self::storage_error("Agent not found: \"compaction\""))?;
        let model = match &agent.model {
            Some(model) => {
                self.deps
                    .models
                    .get_model(&model.provider_id, &model.model_id, &input.session_id)
                    .await
            }
            None => {
                self.deps
                    .models
                    .get_model(
                        &user_model.provider_id,
                        &user_model.model_id,
                        &input.session_id,
                    )
                    .await
            }
        }
        .map_err(|error| Self::storage_error(error.to_string()))?;

        let history_last = compaction_part.is_some()
            && messages
                .last()
                .map(|msg| message_id(&msg.info) == input.parent_id)
                .unwrap_or(false);
        let history: &[WithParts] = if history_last {
            &messages[..messages.len() - 1]
        } else {
            &messages[..]
        };
        let prior = completed_compactions(history);
        let hidden: HashSet<usize> = prior
            .iter()
            .flat_map(|item| [item.user_index, item.assistant_index])
            .collect();
        let previous_summary = prior.last().and_then(|item| item.summary.clone());
        let visible: Vec<WithParts> = history
            .iter()
            .enumerate()
            .filter(|(index, _)| !hidden.contains(index))
            .map(|(_, msg)| msg.clone())
            .collect();
        let selected = select(&SelectInput {
            messages: &visible,
            cfg: &self.deps.config,
            model: &model,
        });
        // Plugin seam: `experimental.session.compacting` — a no-op
        // returning `{ context: [], prompt: undefined }`.
        let compacting_prompt: Option<String> = None;
        let compacting_context: Vec<String> = Vec::new();
        let msgs = selected.head.clone();
        // Plugin seam: `experimental.chat.messages.transform` — no-op.
        let conversation = msgs
            .iter()
            .map(serialize)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let next_prompt = match compacting_prompt {
            Some(prompt) => [
                prompt,
                "The following is the conversation history:".to_string(),
                conversation.clone(),
            ]
            .join("\n\n"),
            None => [build_prompt(&BuildPrompt {
                previous_summary: previous_summary.as_deref(),
                context: vec![conversation.clone()],
            })]
            .into_iter()
            .chain(compacting_context)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        };

        let msg = V1Message::Assistant {
            id: MessageId::ascending(None)?,
            session_id: input.session_id.clone(),
            time: AssistantTime {
                created: self.deps.clock.now_ms(),
                completed: None,
            },
            error: None,
            parent_id: input.parent_id.clone(),
            model_id: model.llm.id.clone(),
            provider_id: model.llm.provider_id.clone(),
            mode: "compaction".to_string(),
            agent: "compaction".to_string(),
            path: V1Path {
                cwd: self.deps.instance.directory.to_string_lossy().into_owned(),
                root: self.deps.instance.worktree.to_string_lossy().into_owned(),
            },
            summary: Some(true),
            cost: 0.0,
            tokens: V1StepTokens {
                total: None,
                input: 0.0,
                output: 0.0,
                reasoning: 0.0,
                cache: V1TokenCache {
                    read: 0.0,
                    write: 0.0,
                },
            },
            structured: None,
            variant: user_model.variant.clone(),
            finish: None,
        };
        self.deps.sessions.update_message(&msg)?;

        let handle = Processor::create(
            ProcessorDeps {
                sessions: self.deps.sessions.clone(),
                messages: self.deps.messages.clone(),
                status: self.deps.status.clone(),
                events: self.deps.events.clone(),
                snapshot: self.deps.snapshot.clone(),
                agents: self.deps.agents.clone(),
                config: self.deps.config.clone(),
                llm: self.deps.llm.clone(),
                permission: self.deps.permission.clone(),
                summary: self.deps.summary.clone(),
                clock: self.deps.clock.clone(),
            },
            ProcessorInput {
                assistant_message: msg,
                session_id: input.session_id.clone(),
                model: model.processor_model(),
            },
        )
        .await;

        let result = handle
            .process(
                StreamInput {
                    user: user_message.clone(),
                    session_id: input.session_id.clone(),
                    parent_session_id: None,
                    project_id: self.deps.project_id.clone(),
                    client: self.deps.client.clone(),
                    model: model.llm.clone(),
                    agent: agent.clone(),
                    permission: None,
                    system: Vec::new(),
                    messages: vec![Message::user(next_prompt)],
                    small: false,
                    tools: Vec::new(),
                    retries: None,
                    tool_choice: None,
                },
                CancellationToken::new(),
            )
            .await
            .map_err(|error| Self::storage_error(error.to_string()))?;

        if result == ProcessResult::Compact {
            let mut message = handle.message();
            if let V1Message::Assistant { error, finish, .. } = &mut message {
                *error = Some(AssistantError::ContextOverflow {
                    message: if replay.is_some() {
                        "Conversation history too large to compact - exceeds model context limit"
                    } else {
                        "Session too large to compact - context exceeds model limit even after stripping media"
                    }
                    .to_string(),
                    response_body: None,
                });
                *finish = Some("error".to_string());
            }
            self.deps.sessions.update_message(&message)?;
            return Ok(CompactionOutcome::Stop);
        }

        if let (Some(compaction), Some(tail)) = (&compaction_part, &selected.tail_start_id) {
            let mut updated = compaction.clone();
            if let V1Part::Compaction { tail_start_id, .. } = &mut updated {
                *tail_start_id = Some(tail.clone());
            }
            self.deps.sessions.update_part(&updated)?;
        }

        if result == ProcessResult::Continue && input.auto {
            if let Some(replay) = &replay {
                let (agent_name, model_ref, format, tools, system) = match &replay.info {
                    V1Message::User {
                        agent,
                        model,
                        format,
                        tools,
                        system,
                        ..
                    } => (agent, model, format, tools, system),
                    V1Message::Assistant { .. } => unreachable!("replay targets user messages"),
                };
                let replay_msg = V1Message::User {
                    id: MessageId::ascending(None)?,
                    session_id: input.session_id.clone(),
                    time: UserTime {
                        created: self.deps.clock.now_ms() as f64,
                    },
                    format: format.clone(),
                    summary: None,
                    agent: agent_name.clone(),
                    model: model_ref.clone(),
                    system: system.clone(),
                    tools: tools.clone(),
                };
                self.deps.sessions.update_message(&replay_msg)?;
                let message_id_value = message_id(&replay_msg).to_string();
                for part in &replay.parts {
                    if matches!(part, V1Part::Compaction { .. }) {
                        continue;
                    }
                    let replay_part = match part {
                        V1Part::File { mime, filename, .. } if is_media(mime) => V1Part::Text {
                            id: String::new(),
                            session_id: String::new(),
                            message_id: String::new(),
                            text: format!(
                                "[Attached {mime}: {}]",
                                filename.as_deref().unwrap_or("file")
                            ),
                            synthetic: None,
                            ignored: None,
                            time: None,
                            metadata: None,
                        },
                        _ => part.clone(),
                    };
                    let replay_part = clone_part_with(
                        &replay_part,
                        &PartId::ascending(None)?,
                        &input.session_id,
                        &message_id_value,
                    );
                    self.deps.sessions.update_part(&replay_part)?;
                }
            }

            if replay.is_none() {
                // Plugin seam: `experimental.compaction.autocontinue`
                // returns `{ enabled: true }`.
                let continue_msg = V1Message::User {
                    id: MessageId::ascending(None)?,
                    session_id: input.session_id.clone(),
                    time: UserTime {
                        created: self.deps.clock.now_ms() as f64,
                    },
                    format: None,
                    summary: None,
                    agent: user_agent.clone(),
                    model: user_model.clone(),
                    system: None,
                    tools: None,
                };
                self.deps.sessions.update_message(&continue_msg)?;
                let text = format!(
                    "{}{AUTOCONTINUE_TEXT}",
                    if input.overflow == Some(true) {
                        AUTOCONTINUE_OVERFLOW_PREAMBLE
                    } else {
                        ""
                    }
                );
                let now = self.deps.clock.now_ms();
                let part = V1Part::Text {
                    id: PartId::ascending(None)?,
                    session_id: input.session_id.clone(),
                    message_id: message_id(&continue_msg).to_string(),
                    text,
                    synthetic: Some(true),
                    ignored: None,
                    time: Some(opencode_schema::session_v1::TextPartTime {
                        start: now,
                        end: Some(now),
                    }),
                    metadata: Some(
                        [(
                            "compaction_continue".to_string(),
                            serde_json::Value::Bool(true),
                        )]
                        .into_iter()
                        .collect(),
                    ),
                };
                self.deps.sessions.update_part(&part)?;
            }
        }

        let message = handle.message();
        if matches!(&message, V1Message::Assistant { error: Some(_), .. }) {
            return Ok(CompactionOutcome::Stop);
        }
        if result == ProcessResult::Continue {
            self.deps.events.publish(
                &SESSION_COMPACTED,
                serde_json::json!({ "sessionID": input.session_id }),
                PublishOptions::default(),
            )?;
        }
        Ok(match result {
            ProcessResult::Continue => CompactionOutcome::Continue,
            ProcessResult::Stop | ProcessResult::Compact => CompactionOutcome::Stop,
        })
    }

    /// `create` (compaction.ts:559-582).
    pub async fn create(&self, input: CompactionCreate) -> Result<(), SessionError> {
        let now = self.deps.clock.now_ms();
        let msg = V1Message::User {
            id: MessageId::ascending(None)?,
            session_id: input.session_id.clone(),
            time: UserTime {
                created: now as f64,
            },
            format: None,
            summary: None,
            agent: input.agent.clone(),
            model: V1UserModel {
                provider_id: input.model.provider_id.clone(),
                model_id: input.model.model_id.clone(),
                variant: input.model.variant.clone(),
            },
            system: None,
            tools: None,
        };
        self.deps.sessions.update_message(&msg)?;
        let part = V1Part::Compaction {
            id: PartId::ascending(None)?,
            session_id: input.session_id.clone(),
            message_id: message_id(&msg).to_string(),
            auto: input.auto,
            overflow: input.overflow,
            tail_start_id: None,
        };
        self.deps.sessions.update_part(&part)?;
        Ok(())
    }
}

/// `{...part, id, messageID, sessionID}` (the store's clone helpers'
/// spread).
fn clone_part_with(part: &V1Part, id: &str, session_id: &str, message_id: &str) -> V1Part {
    let mut value = serde_json::to_value(part).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.insert("id".to_string(), serde_json::Value::String(id.to_string()));
        object.insert(
            "sessionID".to_string(),
            serde_json::Value::String(session_id.to_string()),
        );
        object.insert(
            "messageID".to_string(),
            serde_json::Value::String(message_id.to_string()),
        );
    }
    serde_json::from_value(value).unwrap_or_else(|_| part.clone())
}

// ---------------------------------------------------------------------------
// M5.4 seam binding (loop.rs `Compaction`)
// ---------------------------------------------------------------------------

impl CompactionService for SessionCompaction {
    fn process<'a>(
        &'a self,
        input: CompactionProcess,
    ) -> crate::tool::def::BoxFuture<'a, Result<bool, SessionError>> {
        Box::pin(async move {
            match self.process(input).await? {
                CompactionOutcome::Continue => Ok(true),
                CompactionOutcome::Stop => Ok(false),
            }
        })
    }

    fn create<'a>(
        &'a self,
        input: CompactionCreate,
    ) -> crate::tool::def::BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(async move { self.create(input).await })
    }

    fn prune<'a>(&'a self, session_id: String) -> crate::tool::def::BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Err(error) = self.prune(&session_id) {
                tracing::warn!("prune failed: {error}");
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::store::CreateInput;
    use crate::session::test_support::{
        assistant_message, compaction_part, harness, harness_with_config, static_models, text_part,
        user_message, with_parts,
    };
    use opencode_llm::schema::errors::{LlmError, LlmErrorReason, ProviderFailureClassification};
    use opencode_schema::session_v1::{ToolStateCompletedTime, V1ToolState};
    use serde_json::json;

    fn cfg(config: &str) -> crate::config::schema::Config {
        serde_json::from_str(config).unwrap()
    }

    fn model() -> ResolvedModel {
        crate::session::test_support::test_model()
    }

    // ------------------------------------------------------------------
    // Token / serialize goldens
    // ------------------------------------------------------------------

    #[test]
    fn token_estimate_table() {
        assert_eq!(token_estimate(""), 0.0);
        assert_eq!(token_estimate("abcd"), 1.0);
        // JS Math.round: 3/4 = 0.75 → 1, 1/4 = 0.25 → 0.
        assert_eq!(token_estimate("abc"), 1.0);
        assert_eq!(token_estimate("ab"), 1.0);
        assert_eq!(token_estimate("a"), 0.0);
    }

    #[test]
    fn serialize_user() {
        let session = "ses_1";
        let user = user_message(session, "msg_u", 1.0);
        let text = text_part(session, "msg_u", "prt_t", "hello");
        let mut ignored = text_part(session, "msg_u", "prt_i", "ignored text");
        if let V1Part::Text { ignored, .. } = &mut ignored {
            *ignored = Some(true);
        }
        let file = V1Part::File {
            id: "prt_f".to_string(),
            session_id: session.to_string(),
            message_id: "msg_u".to_string(),
            mime: "text/plain".to_string(),
            filename: Some("notes.txt".to_string()),
            url: "file:///notes.txt".to_string(),
            source: None,
        };
        let msg = with_parts(user, vec![text, ignored, file]);
        assert_eq!(
            serialize(&msg),
            "[User]: hello\n[Attached text/plain: notes.txt]"
        );
    }

    #[test]
    fn serialize_assistant() {
        let session = "ses_1";
        let assistant = assistant_message(session, "msg_a", "msg_u", 1, None, None);
        let text = text_part(session, "msg_a", "prt_1", "hi there");
        let reasoning = {
            let part = text_part(session, "msg_a", "prt_2", "thinking");
            match part {
                V1Part::Text {
                    id,
                    session_id,
                    message_id,
                    text,
                    ..
                } => V1Part::Reasoning {
                    id,
                    session_id,
                    message_id,
                    text,
                    metadata: None,
                    time: opencode_schema::session_v1::ReasoningTime {
                        start: 1,
                        end: None,
                    },
                },
                _ => unreachable!("text part"),
            }
        };
        let tool_completed = V1Part::Tool {
            id: "prt_3".to_string(),
            session_id: session.to_string(),
            message_id: "msg_a".to_string(),
            call_id: "call_1".to_string(),
            tool: "read".to_string(),
            state: V1ToolState::Completed {
                input: json!({ "filePath": "/a" }).as_object().unwrap().clone(),
                output: "file contents".to_string(),
                title: "Read".to_string(),
                metadata: Default::default(),
                time: ToolStateCompletedTime {
                    start: 1,
                    end: 2,
                    compacted: None,
                },
                attachments: None,
            },
            metadata: None,
        };
        let tool_errored = V1Part::Tool {
            id: "prt_4".to_string(),
            session_id: session.to_string(),
            message_id: "msg_a".to_string(),
            call_id: "call_2".to_string(),
            tool: "bash".to_string(),
            state: V1ToolState::Error {
                input: json!({ "command": "ls" }).as_object().unwrap().clone(),
                error: "boom".to_string(),
                metadata: Default::default(),
                time: opencode_schema::session_v1::ToolStateErrorTime { start: 1, end: 2 },
            },
            metadata: None,
        };
        let msg = with_parts(
            assistant,
            vec![text, reasoning, tool_completed, tool_errored],
        );
        assert_eq!(
            serialize(&msg),
            "[Assistant]: hi there\n\
             [Assistant reasoning]: thinking\n\
             [Assistant tool call]: read({\"filePath\":\"/a\"})\n\
             [Tool result]: file contents\n\
             [Assistant tool call]: bash({\"command\":\"ls\"})\n\
             [Tool error]: boom"
        );
    }

    #[test]
    fn serialize_truncates_tool_output() {
        let session = "ses_1";
        let assistant = assistant_message(session, "msg_a", "msg_u", 1, None, None);
        let tool = V1Part::Tool {
            id: "prt_1".to_string(),
            session_id: session.to_string(),
            message_id: "msg_a".to_string(),
            call_id: "call_1".to_string(),
            tool: "read".to_string(),
            state: V1ToolState::Completed {
                input: Default::default(),
                output: "x".repeat(TOOL_OUTPUT_MAX_CHARS + 10),
                title: "Read".to_string(),
                metadata: Default::default(),
                time: ToolStateCompletedTime {
                    start: 1,
                    end: 2,
                    compacted: None,
                },
                attachments: None,
            },
            metadata: None,
        };
        let msg = with_parts(assistant, vec![tool]);
        let serialized = serialize(&msg);
        assert!(
            serialized.contains(&format!(
                "{}\n[truncated]",
                "x".repeat(TOOL_OUTPUT_MAX_CHARS)
            )),
            "tool output should be truncated: {serialized}"
        );
    }

    #[test]
    fn summary_text_joins_text_parts() {
        let session = "ses_1";
        let assistant = assistant_message(session, "msg_a", "msg_u", 1, None, None);
        let msg = with_parts(
            assistant.clone(),
            vec![
                text_part(session, "msg_a", "prt_1", " first "),
                text_part(session, "msg_a", "prt_2", "second"),
            ],
        );
        assert_eq!(summary_text(&msg).as_deref(), Some("first\n\nsecond"));
        assert_eq!(
            summary_text(&with_parts(assistant, vec![])),
            None,
            "empty summary renders as undefined"
        );
    }

    // ------------------------------------------------------------------
    // Budget / turns / select
    // ------------------------------------------------------------------

    #[test]
    fn preserve_recent_budget_table() {
        // explicit config wins
        assert_eq!(
            preserve_recent_budget(
                &cfg(r#"{"compaction": {"preserve_recent_tokens": 7}}"#),
                &model().limits,
            ),
            7.0
        );
        // usable = 200_000 - 100 (output) = 199_900 → 0.25 → 49_975 → clamped to 15_000
        assert_eq!(
            preserve_recent_budget(&cfg("{}"), &model().limits),
            MAX_PRESERVE_RECENT_TOKENS
        );
    }

    #[test]
    fn turns_marks_boundaries() {
        let session = "ses_1";
        let messages = vec![
            with_parts(user_message(session, "msg_1", 1.0), vec![]),
            with_parts(
                assistant_message(session, "msg_2", "msg_1", 1, None, None),
                vec![],
            ),
            with_parts(user_message(session, "msg_3", 2.0), vec![]),
            with_parts(user_message(session, "msg_4", 3.0), vec![]),
        ];
        let result = turns(&messages);
        assert_eq!(result.len(), 3);
        assert_eq!(
            result[0],
            Turn {
                start: 0,
                end: 2,
                id: "msg_1".into()
            }
        );
        assert_eq!(
            result[1],
            Turn {
                start: 2,
                end: 3,
                id: "msg_3".into()
            }
        );
        assert_eq!(
            result[2],
            Turn {
                start: 3,
                end: 4,
                id: "msg_4".into()
            }
        );
    }

    #[test]
    fn select_zero_tail_turns_disables() {
        let session = "ses_1";
        let messages = vec![
            with_parts(
                user_message(session, "msg_1", 1.0),
                vec![text_part(session, "msg_1", "p1", "hello")],
            ),
            with_parts(user_message(session, "msg_2", 2.0), vec![]),
        ];
        let config = cfg(r#"{"compaction": {"tail_turns": 0}}"#);
        let selected = select(&SelectInput {
            messages: &messages,
            cfg: &config,
            model: &model(),
        });
        assert_eq!(selected.head.len(), 2);
        assert_eq!(selected.tail_start_id, None);
    }

    #[test]
    fn select_keeps_recent_turns_within_budget() {
        let session = "ses_1";
        let messages = vec![
            with_parts(
                user_message(session, "msg_1", 1.0),
                vec![text_part(session, "msg_1", "p1", "first turn")],
            ),
            with_parts(user_message(session, "msg_2", 2.0), vec![]),
            with_parts(
                user_message(session, "msg_3", 3.0),
                vec![text_part(session, "msg_3", "p2", "last turn")],
            ),
        ];
        // Huge budget: everything fits; keep ends at the oldest turn, which
        // starts at 0 → keep-all.
        let config = cfg(r#"{"compaction": {"preserve_recent_tokens": 1000000}}"#);
        let selected = select(&SelectInput {
            messages: &messages,
            cfg: &config,
            model: &model(),
        });
        assert_eq!(selected.tail_start_id, None);
        assert_eq!(selected.head.len(), 3);

        // Limited to the last turn: the head is everything before it.
        let config = cfg(r#"{"compaction": {"tail_turns": 1, "preserve_recent_tokens": 1000000}}"#);
        let selected = select(&SelectInput {
            messages: &messages,
            cfg: &config,
            model: &model(),
        });
        assert_eq!(selected.tail_start_id.as_deref(), Some("msg_3"));
        assert_eq!(selected.head.len(), 2);
        assert_eq!(message_id(&selected.head[0].info), "msg_1");
    }

    #[test]
    fn select_splits_turn_when_budget_exhausted() {
        let session = "ses_1";
        // Last turn = [user(text), assistant(no parts)]: the part-less
        // assistant renders to nothing, so a tiny budget splits at it.
        let messages = vec![
            with_parts(
                user_message(session, "msg_1", 1.0),
                vec![text_part(session, "msg_1", "p1", "first turn")],
            ),
            with_parts(
                user_message(session, "msg_2", 2.0),
                vec![text_part(session, "msg_2", "p2", "second turn")],
            ),
            with_parts(
                assistant_message(session, "msg_3", "msg_2", 1, None, None),
                vec![],
            ),
        ];
        let config = cfg(r#"{"compaction": {"preserve_recent_tokens": 1}}"#);
        let selected = select(&SelectInput {
            messages: &messages,
            cfg: &config,
            model: &model(),
        });
        assert_eq!(
            selected.tail_start_id.as_deref(),
            Some("msg_3"),
            "the split point is the part-less assistant"
        );
        assert_eq!(selected.head.len(), 2);
    }

    // ------------------------------------------------------------------
    // buildPrompt goldens
    // ------------------------------------------------------------------

    #[test]
    fn build_prompt_without_previous_summary() {
        let prompt = build_prompt(&BuildPrompt {
            previous_summary: None,
            context: vec!["CONVERSATION".to_string()],
        });
        assert!(
            prompt.starts_with(
                "Here is the conversation so far:\n\n<conversation>\nCONVERSATION\n</conversation>"
            ),
            "{prompt}"
        );
        assert!(prompt.contains(SUMMARY_TEMPLATE));
        assert!(!prompt.contains("prior-summary"));
        assert!(
            prompt.contains("Create a new anchored summary from the conversation history"),
            "{prompt}"
        );
    }

    #[test]
    fn build_prompt_with_previous_summary() {
        let prompt = build_prompt(&BuildPrompt {
            previous_summary: Some("OLD"),
            context: vec!["CONVERSATION".to_string()],
        });
        assert!(
            prompt.contains("<prior-summary>\nOLD\n</prior-summary>"),
            "{prompt}"
        );
        assert!(prompt.contains(SUMMARY_UPDATE_INSTRUCTIONS));
        assert!(prompt.contains(SUMMARY_TEMPLATE));
    }

    #[test]
    fn vendored_prompt_files_match_ts() {
        // Byte-for-byte the TS constants (core/session/compaction.ts:16-55).
        assert_eq!(
            SUMMARY_TEMPLATE,
            "Output exactly the Markdown structure shown inside <template> and keep the section order unchanged. Do not include the <template> tags in your response.\n<template>\n## Objective\n- [one or two brief sentences describing what the user is trying to accomplish]\n\n## Important Details\n- [constraints/preferences, decisions and why, important facts/assumptions, exact context needed to continue, or \"(none)\"]\n\n## Work State\n### Completed\n- [finished work, verified facts, or changes made; otherwise \"(none)\"]\n\n### Active\n- [current work, partial changes, or investigation state; otherwise \"(none)\"]\n\n### Blocked\n- [blockers, failing commands, or unknowns; otherwise \"(none)\"]\n\n## Next Move\n1. [immediate concrete action, or \"(none)\"]\n2. [next action if known, or \"(none)\"]\n\n## Relevant Files\n- [file or directory path: why it matters, or \"(none)\"]\n</template>\n\nRules:\n- Keep every section, even when empty.\n- Use terse bullets, not prose paragraphs.\n- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers when known.\n- Do not mention the summary process or that context was compacted."
        );
        assert_eq!(
            SUMMARY_UPDATE_INSTRUCTIONS,
            "The <prior-summary> summarizes everything that happened before the <conversation>. Construct a new summary that combines both. The <prior-summary> is discarded after this: anything you do not carry into the new summary is lost.\n\nWhen combining:\n- Carry forward objectives, constraints, user directives, decisions, and parallel workstreams from the <prior-summary> even when the <conversation> does not mention them. Drop only what is finished and no longer needed.\n- The <conversation> is more recent than the <prior-summary>. Where they conflict, the conversation wins: state the corrected fact and drop the old claim.\n- Add new progress, decisions, constraints, and context from the conversation.\n- Move completed work from \"Active\" to \"Completed\".\n- If a blocker has been resolved, update the summary to reflect that while keeping any details still needed to continue the work.\n- Update \"Objective\" and \"Next Move\" to reflect the current work state."
        );
    }

    // ------------------------------------------------------------------
    // Service
    // ------------------------------------------------------------------

    use crate::session::r#loop::CompactionCreate as CreateInputAlias;

    fn push_message(h: &crate::session::test_support::Harness, message: &V1Message) {
        h.services.sessions.update_message(message).unwrap();
    }

    fn last_user_id(h: &crate::session::test_support::Harness, session: &str) -> String {
        let msgs = h.services.messages.stream(session).unwrap();
        // `stream()` yields newest-first, so the first user hit is the newest.
        let msg = msgs
            .iter()
            .find(|msg| matches!(msg.info, V1Message::User { .. }))
            .expect("a user message");
        message_id(&msg.info).to_string()
    }

    fn compaction_service(h: &crate::session::test_support::Harness) -> SessionCompaction {
        let summary = Arc::new(crate::session::summary::SessionSummary::new(
            crate::session::summary::SummaryDeps {
                sessions: h.services.sessions.clone(),
                snapshot: h.snapshot.clone(),
                events: h.services.events.clone(),
                config: Arc::new(h.config.clone()),
            },
        ));
        SessionCompaction::new(CompactionDeps {
            sessions: h.services.sessions.clone(),
            messages: h.services.messages.clone(),
            events: h.services.events.clone(),
            status: h.services.status.clone(),
            agents: h.services.agents.clone(),
            snapshot: h.snapshot.clone(),
            llm: h.llm.clone(),
            permission: Arc::new(crate::session::test_support::AllowAll),
            summary,
            models: Arc::new(static_models()),
            config: Arc::new(h.config.clone()),
            clock: Arc::new(crate::session::test_support::FixedClock),
            instance: crate::tool::def::InstanceContext {
                directory: h.worktree.clone(),
                worktree: h.worktree.clone(),
            },
            output_token_max: None,
            project_id: None,
            client: "cli".to_string(),
        })
    }

    async fn create_compaction(
        h: &crate::session::test_support::Harness,
        session: &str,
        auto: bool,
        overflow: bool,
    ) {
        compaction_service(h)
            .create(crate::session::r#loop::CompactionCreate {
                session_id: session.to_string(),
                agent: "build".to_string(),
                model: opencode_schema::session_v1::V1UserModel {
                    provider_id: "anthropic".to_string(),
                    model_id: "claude".to_string(),
                    variant: None,
                },
                auto,
                overflow: Some(overflow),
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_appends_compaction_user_message() {
        use crate::session::test_support::create_session;
        let h = harness("compaction-create", vec![]);
        let session = create_session(&h.services.sessions, &h.worktree);
        create_compaction(&h, &session.id, true, false).await;

        let msgs = h.services.messages.stream(&session.id).unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0].info, V1Message::User { .. }));
        assert_eq!(msgs[0].parts.len(), 1);
        match &msgs[0].parts[0] {
            V1Part::Compaction { auto, overflow, .. } => {
                assert!(*auto);
                assert_eq!(*overflow, Some(false));
            }
            other => panic!("expected a compaction part, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn process_full_cycle_summary_and_compacted_event() {
        use crate::session::message::filter_compacted;
        use crate::session::test_support::{create_session, text_stream};

        let h = harness("compaction-process", vec![text_stream("## Objective")]);
        let session = create_session(&h.services.sessions, &h.worktree);
        push_message(&h, &user_message(&session.id, "msg_u1", 1.0));
        h.services
            .sessions
            .update_part(&text_part(&session.id, "msg_u1", "prt_u1", "fix the bug"))
            .unwrap();
        create_compaction(&h, &session.id, true, false).await;
        let parent_id = last_user_id(&h, &session.id);
        let msgs = filter_compacted(h.services.messages.stream(&session.id).unwrap());

        let mut events = h.services.events.subscribe("session.compacted");
        let outcome = compaction_service(&h)
            .process(crate::session::r#loop::CompactionProcess {
                messages: msgs,
                parent_id,
                session_id: session.id.clone(),
                auto: true,
                overflow: None,
            })
            .await
            .unwrap();
        assert_eq!(outcome, CompactionOutcome::Continue);
        assert_eq!(h.llm.calls(), 1, "one compaction LLM turn");

        let msgs = h.services.messages.stream(&session.id).unwrap();
        // The summary assistant message exists with the LLM text.
        let summary = msgs
            .iter()
            .find(|msg| {
                matches!(
                    &msg.info,
                    V1Message::Assistant {
                        summary: Some(true),
                        ..
                    }
                )
            })
            .expect("summary assistant message");
        assert!(summary
            .parts
            .iter()
            .any(|part| matches!(part, V1Part::Text { text, .. } if text == "## Objective")));

        // Compacted event published.
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("session.compacted event")
            .unwrap();
        assert_eq!(event.data["sessionID"], json!(session.id));

        // Auto-continue message appended.
        let continue_count = msgs
            .iter()
            .flat_map(|msg| &msg.parts)
            .filter(|part| match part {
                V1Part::Text {
                    text,
                    metadata,
                    synthetic,
                    ..
                } => {
                    *synthetic == Some(true)
                        && metadata
                            .as_ref()
                            .and_then(|m| m.get("compaction_continue"))
                            .map(|v| *v == json!(true))
                            .unwrap_or(false)
                        && text.contains("Continue if you have next steps")
                }
                _ => false,
            })
            .count();
        assert_eq!(continue_count, 1, "auto-continue message appended");
    }

    #[tokio::test]
    async fn process_overflow_replay_clones_previous_user_message() {
        use crate::session::message::filter_compacted;
        use crate::session::test_support::{create_session, text_stream};

        let h = harness("compaction-replay", vec![text_stream("## Objective")]);
        let session = create_session(&h.services.sessions, &h.worktree);
        // Two user turns: the overflow replay needs content to remain after
        // the replayed turn is lifted out of the history.
        push_message(&h, &user_message(&session.id, "msg_u0", 1.0));
        h.services
            .sessions
            .update_part(&text_part(&session.id, "msg_u0", "prt_u0", "earlier turn"))
            .unwrap();
        push_message(&h, &user_message(&session.id, "msg_u1", 2.0));
        h.services
            .sessions
            .update_part(&text_part(&session.id, "msg_u1", "prt_u1", "replay me"))
            .unwrap();
        create_compaction(&h, &session.id, true, true).await;
        let parent_id = last_user_id(&h, &session.id);
        let msgs = filter_compacted(h.services.messages.stream(&session.id).unwrap());

        compaction_service(&h)
            .process(crate::session::r#loop::CompactionProcess {
                messages: msgs,
                parent_id,
                session_id: session.id.clone(),
                auto: true,
                overflow: Some(true),
            })
            .await
            .unwrap();

        // The replay clone of msg_u1 was appended (new id, no compaction
        // part, same text).
        let msgs = h.services.messages.stream(&session.id).unwrap();
        let replay = msgs
            .iter()
            .filter(|msg| {
                msg.parts
                    .iter()
                    .any(|part| matches!(part, V1Part::Text { text, .. } if text == "replay me"))
            })
            .collect::<Vec<_>>();
        assert_eq!(replay.len(), 2, "original + replayed user message");
        assert_ne!(
            message_id(&replay[0].info),
            message_id(&replay[1].info),
            "the replayed message has a fresh id"
        );
        assert!(replay.iter().all(|msg| msg
            .parts
            .iter()
            .all(|part| !matches!(part, V1Part::Compaction { .. }))));
    }

    #[tokio::test]
    async fn process_compact_outcome_marks_context_overflow() {
        use crate::session::message::filter_compacted;
        use crate::session::test_support::create_session;

        let h = harness_with_config(
            "compaction-overflow-outcome",
            vec![vec![Err(LlmError {
                module: "ProviderShared".to_string(),
                method: "request".to_string(),
                reason: LlmErrorReason::InvalidRequest {
                    message: "prompt is too long".to_string(),
                    parameter: None,
                    classification: Some(ProviderFailureClassification::ContextOverflow),
                    provider_metadata: None,
                    http: None,
                },
            })]],
            serde_json::json!({}),
        );
        let session = create_session(&h.services.sessions, &h.worktree);
        push_message(&h, &user_message(&session.id, "msg_u1", 1.0));
        h.services
            .sessions
            .update_part(&text_part(&session.id, "msg_u1", "prt_u1", "hello"))
            .unwrap();
        create_compaction(&h, &session.id, false, false).await;
        let parent_id = last_user_id(&h, &session.id);
        let msgs = filter_compacted(h.services.messages.stream(&session.id).unwrap());

        let outcome = compaction_service(&h)
            .process(crate::session::r#loop::CompactionProcess {
                messages: msgs,
                parent_id,
                session_id: session.id.clone(),
                auto: false,
                overflow: None,
            })
            .await
            .unwrap();
        assert_eq!(outcome, CompactionOutcome::Stop);

        let msgs = h.services.messages.stream(&session.id).unwrap();
        let summary = msgs
            .iter()
            .find(|msg| {
                matches!(
                    &msg.info,
                    V1Message::Assistant {
                        summary: Some(true),
                        ..
                    }
                )
            })
            .expect("summary assistant message");
        match &summary.info {
            V1Message::Assistant { error, finish, .. } => {
                assert!(
                    matches!(
                        error,
                        Some(AssistantError::ContextOverflow { message, .. })
                            if message == "Session too large to compact - context exceeds model limit even after stripping media"
                    ),
                    "expected a ContextOverflowError, got {error:?}"
                );
                assert_eq!(finish.as_deref(), Some("error"));
            }
            _ => panic!("expected an assistant message"),
        }
    }

    #[tokio::test]
    async fn process_requires_user_parent() {
        use crate::session::test_support::create_session;

        let h = harness("compaction-parent", vec![]);
        let session = create_session(&h.services.sessions, &h.worktree);
        push_message(&h, &user_message(&session.id, "msg_u1", 1.0));
        push_message(
            &h,
            &assistant_message(&session.id, "msg_a1", "msg_u1", 1, None, None),
        );
        let msgs = h.services.messages.stream(&session.id).unwrap();
        let error = compaction_service(&h)
            .process(crate::session::r#loop::CompactionProcess {
                messages: msgs,
                parent_id: "msg_a1".to_string(),
                session_id: session.id.clone(),
                auto: false,
                overflow: None,
            })
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Compaction parent must be a user message: msg_a1"),
            "{error}"
        );
    }

    // ------------------------------------------------------------------
    // prune
    // ------------------------------------------------------------------

    fn tool_part(session: &str, id: &str, msg: &str, tool: &str, output: String) -> V1Part {
        V1Part::Tool {
            id: format!("prt_{id}"),
            session_id: session.to_string(),
            message_id: msg.to_string(),
            call_id: format!("call_{id}"),
            tool: tool.to_string(),
            state: V1ToolState::Completed {
                input: Default::default(),
                output,
                title: "Read".to_string(),
                metadata: Default::default(),
                time: ToolStateCompletedTime {
                    start: 1,
                    end: 2,
                    compacted: None,
                },
                attachments: None,
            },
            metadata: None,
        }
    }

    async fn prune_harness(name: &str) -> crate::session::test_support::Harness {
        harness_with_config(
            name,
            vec![],
            serde_json::json!({ "compaction": { "prune": true } }),
        )
    }

    fn push_three_turns(h: &crate::session::test_support::Harness, session: &str) {
        push_message(h, &user_message(session, "msg_u1", 1.0));
        push_message(
            h,
            &assistant_message(session, "msg_a1", "msg_u1", 1, None, None),
        );
        push_message(h, &user_message(session, "msg_u2", 2.0));
        push_message(
            h,
            &assistant_message(session, "msg_a2", "msg_u2", 2, None, None),
        );
        push_message(h, &user_message(session, "msg_u3", 3.0));
        push_message(
            h,
            &assistant_message(session, "msg_a3", "msg_u3", 3, None, None),
        );
    }

    #[tokio::test]
    async fn prune_marks_old_tool_outputs() {
        use crate::session::test_support::create_session;

        let h = prune_harness("compaction-prune").await;
        let session = create_session(&h.services.sessions, &h.worktree);
        push_three_turns(&h, &session.id);
        // assistant1 is older than the second user turn → scanned.
        let big = "x".repeat(240_000);
        h.services
            .sessions
            .update_part(&tool_part(&session.id, "1", "msg_a1", "read", big.clone()))
            .unwrap();
        h.services
            .sessions
            .update_part(&tool_part(&session.id, "2", "msg_a1", "skill", big))
            .unwrap();

        compaction_service(&h).prune(&session.id).unwrap();

        let msgs = h.services.messages.stream(&session.id).unwrap();
        let compacted: Vec<&V1Part> = msgs
            .iter()
            .flat_map(|msg| &msg.parts)
            .filter(|part| {
                matches!(
                    part,
                    V1Part::Tool {
                        state: V1ToolState::Completed { time, .. },
                        ..
                    } if time.compacted.is_some()
                )
            })
            .collect();
        assert_eq!(compacted.len(), 1, "only the read output was pruned");
        match compacted[0] {
            V1Part::Tool { id, .. } => assert_eq!(id, "prt_1"),
            other => panic!("expected a tool part, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn prune_skips_below_minimum() {
        use crate::session::test_support::create_session;

        let h = prune_harness("compaction-prune-minimum").await;
        let session = create_session(&h.services.sessions, &h.worktree);
        push_three_turns(&h, &session.id);
        let part = tool_part(&session.id, "1", "msg_a1", "read", "x".repeat(100));
        h.services.sessions.update_part(&part).unwrap();

        compaction_service(&h).prune(&session.id).unwrap();

        let msgs = h.services.messages.stream(&session.id).unwrap();
        let compacted = msgs
            .iter()
            .flat_map(|msg| &msg.parts)
            .filter(|part| {
                matches!(
                    part,
                    V1Part::Tool {
                        state: V1ToolState::Completed { time, .. },
                        ..
                    } if time.compacted.is_some()
                )
            })
            .count();
        assert_eq!(compacted, 0, "small outputs are not pruned");
    }

    #[tokio::test]
    async fn prune_requires_config() {
        use crate::session::test_support::create_session;

        let h = harness("compaction-prune-disabled", vec![]);
        let session = create_session(&h.services.sessions, &h.worktree);
        push_message(&h, &user_message(&session.id, "msg_u1", 1.0));
        compaction_service(&h).prune(&session.id).unwrap();
    }

    // ------------------------------------------------------------------
    // completedCompactions
    // ------------------------------------------------------------------

    #[test]
    fn completed_compactions_pairs_user_and_assistant() {
        let session = "ses_1";
        let messages = vec![
            with_parts(
                user_message(session, "msg_u1", 1.0),
                vec![compaction_part(
                    session, "msg_u1", "prt_1", true, None, None,
                )],
            ),
            with_parts(
                assistant_message(
                    session,
                    "msg_a1",
                    "msg_u1",
                    1,
                    Some(true),
                    Some("stop".into()),
                ),
                vec![text_part(session, "msg_a1", "prt_2", "done")],
            ),
            with_parts(
                assistant_message(session, "msg_a2", "msg_u1", 2, Some(true), None),
                vec![text_part(session, "msg_a2", "prt_3", "unfinished")],
            ),
        ];
        let result = completed_compactions(&messages);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].user_index, 0);
        assert_eq!(result[0].assistant_index, 1);
        assert_eq!(result[0].summary.as_deref(), Some("done"));
    }

    #[test]
    fn unused_import_silencers() {
        // Keep import lists honest if the above tests change.
        let _: Option<CreateInputAlias> = None;
        let _: Option<CreateInput> = None;
    }
}
