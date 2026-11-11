//! Importing a chat transcript from another tool (an alforria extension):
//! `alforria import --format openai-chat`.
//!
//! The input is the lingua franca of chat history, a list of
//! `{ role, content, tool_calls?, tool_call_id?, name?, created_at? }`
//! messages, as a JSON array, a JSON object with a `messages` array, or
//! JSONL. [`plan`] turns it into the messages and parts of one session;
//! [`write`] stores them in one transaction.
//!
//! A long history must not be sent to the model whole on the first prompt
//! after the import. So everything before a cutoff is stored as ordinary,
//! readable, searchable messages, and then a compaction boundary is written
//! after the live tail, exactly as a compaction would have left it: a user
//! message carrying a `compaction` part whose `tail_start_id` names the first
//! live message, answered by a `summary: true` assistant message.
//! `filter_compacted` then gives the loop the summary and the tail, and
//! nothing older.

use std::collections::HashMap;

use alforria_schema::schema::JsonMap;
use alforria_schema::session_v1::{V1Message, V1Part};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::session::ids::{MessageId, PartId};
use crate::session::message::WithParts;
use crate::storage::Storage;
use crate::CoreError;

/// What the summary message says when the transcript came with no summary.
pub const NEEDS_SUMMARY_TEXT: &str = "Earlier messages of this conversation were imported from another tool \
and are not shown here. No summary of them was provided. They are stored in this session and can be \
searched (the session_search tool finds them when memory is enabled). Do not assume what they said: \
look it up, or ask.";

/// The error an imported tool call gets when the transcript has no result
/// for it.
pub const INTERRUPTED_ERROR: &str = "Tool execution was interrupted";

/// One message of the transcript.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct TranscriptMessage {
    pub role: String,
    #[serde(default)]
    pub content: Value,
    #[serde(default)]
    pub tool_calls: Option<Vec<Value>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Seconds or milliseconds since the epoch, or RFC 3339.
    #[serde(default, alias = "createdAt", alias = "timestamp")]
    pub created_at: Option<Value>,
}

/// A transcript and what came with it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    pub title: Option<String>,
    pub directory: Option<String>,
    pub summary: Option<String>,
    pub metadata: Option<Value>,
    /// The caller's name for this import; importing it again does nothing.
    pub import_id: Option<String>,
    /// `provider/model` for the imported messages.
    pub model: Option<String>,
    pub messages: Vec<TranscriptMessage>,
}

#[derive(Debug, Default, Deserialize)]
struct Header {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default, alias = "importID", alias = "importId")]
    import_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    messages: Option<Vec<TranscriptMessage>>,
}

impl Transcript {
    fn absorb(&mut self, header: Header) {
        self.title = header.title.or(self.title.take());
        self.directory = header.directory.or(self.directory.take());
        self.summary = header.summary.or(self.summary.take());
        self.metadata = header.metadata.or(self.metadata.take());
        self.import_id = header.import_id.or(self.import_id.take());
        self.model = header.model.or(self.model.take());
        if let Some(messages) = header.messages {
            self.messages.extend(messages);
        }
    }
}

/// Parse a transcript: a JSON array of messages, a JSON object
/// (`{ title?, directory?, summary?, metadata?, import_id?, model?, messages }`),
/// or JSONL, where a line with a `role` is a message and any other line is
/// such an object.
pub fn parse_transcript(text: &str) -> Result<Transcript, String> {
    let text = text.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return Err("the transcript is empty".to_string());
    }
    let mut transcript = Transcript::default();
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(items)) => {
            for (index, item) in items.into_iter().enumerate() {
                transcript.messages.push(
                    serde_json::from_value(item)
                        .map_err(|error| format!("message {}: {error}", index + 1))?,
                );
            }
        }
        Ok(value @ Value::Object(_)) if value.get("role").is_none() => {
            let header: Header =
                serde_json::from_value(value).map_err(|error| error.to_string())?;
            if header.messages.is_none() {
                return Err("the transcript object has no `messages` array".to_string());
            }
            transcript.absorb(header);
        }
        _ => {
            for (index, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let value: Value = serde_json::from_str(line)
                    .map_err(|error| format!("line {}: {error}", index + 1))?;
                if value.get("role").is_some() {
                    transcript.messages.push(
                        serde_json::from_value(value)
                            .map_err(|error| format!("line {}: {error}", index + 1))?,
                    );
                } else {
                    let header: Header = serde_json::from_value(value)
                        .map_err(|error| format!("line {}: {error}", index + 1))?;
                    transcript.absorb(header);
                }
            }
        }
    }
    for (index, message) in transcript.messages.iter().enumerate() {
        if !matches!(
            message.role.as_str(),
            "system" | "developer" | "user" | "assistant" | "tool" | "function"
        ) {
            return Err(format!(
                "message {}: unknown role \"{}\" (expected system, user, assistant or tool)",
                index + 1,
                message.role
            ));
        }
    }
    if transcript.messages.is_empty() {
        return Err("the transcript has no messages".to_string());
    }
    Ok(transcript)
}

/// Where the history ends and the live conversation begins. Counts are in
/// imported messages (user and assistant; tool results join their call).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cutoff {
    /// Archive the first `n` messages.
    ArchiveBefore(usize),
    /// Archive the messages written before this time (unix milliseconds).
    ArchiveBeforeTime(u64),
    /// Keep the last `n` messages live.
    LiveTail(usize),
}

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub agent: String,
    pub provider_id: String,
    pub model_id: String,
    /// The session directory and worktree, for assistant messages' `path`.
    pub cwd: String,
    pub root: String,
    pub cutoff: Option<Cutoff>,
    pub now_ms: u64,
}

/// What an import did, or would do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    /// Messages in the session (user and assistant), the boundary excluded.
    pub messages: usize,
    pub user: usize,
    pub assistant: usize,
    pub parts: usize,
    #[serde(rename = "toolCalls")]
    pub tool_calls: usize,
    /// Tool calls with no result in the transcript, stored as interrupted.
    #[serde(rename = "interruptedToolCalls")]
    pub interrupted_tool_calls: usize,
    /// Tool results that answer no call; dropped.
    #[serde(rename = "orphanToolResults")]
    pub orphan_tool_results: usize,
    pub images: usize,
    /// System messages; not imported (the session has its own system prompt).
    #[serde(rename = "systemSkipped")]
    pub system_skipped: usize,
    /// Messages behind the compaction boundary.
    pub archived: usize,
    /// Messages after it, sent to the model on the next prompt.
    pub live: usize,
    /// `"summary"`, `"needs_summary"`, or absent when nothing is archived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<&'static str>,
}

/// The messages of the session to be, in order.
#[derive(Debug, Clone)]
pub struct ImportPlan {
    pub messages: Vec<WithParts>,
    pub report: ImportReport,
}

fn timestamp(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => {
            let raw = number.as_f64()?;
            if raw <= 0.0 {
                return None;
            }
            // Seconds until the year 33658; milliseconds otherwise.
            Some(if raw < 1e12 {
                (raw * 1000.0) as u64
            } else {
                raw as u64
            })
        }
        Value::String(text) => chrono::DateTime::parse_from_rfc3339(text.trim())
            .ok()
            .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
            .or_else(|| {
                text.trim()
                    .parse::<f64>()
                    .ok()
                    .and_then(|raw| timestamp(&json!(raw)))
            }),
        _ => None,
    }
}

/// Parse `--archive-before`: a count, or an RFC 3339 time.
pub fn parse_archive_before(text: &str) -> Result<Cutoff, String> {
    let text = text.trim();
    if let Ok(count) = text.parse::<usize>() {
        return Ok(Cutoff::ArchiveBefore(count));
    }
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
        .map(Cutoff::ArchiveBeforeTime)
        .ok_or_else(|| {
            format!(
                "\"{text}\" is neither a message count nor an RFC 3339 time \
                 (for example 2026-06-01T00:00:00Z)"
            )
        })
}

fn mime_of(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("data:") {
        let mime = rest.split([';', ',']).next().unwrap_or_default();
        if !mime.is_empty() {
            return mime.to_string();
        }
    }
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    match path.rsplit('.').next() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        _ => "image/png",
    }
    .to_string()
}

struct Builder<'a> {
    session_id: &'a str,
    options: &'a ImportOptions,
    report: ImportReport,
    messages: Vec<WithParts>,
    times: Vec<u64>,
    /// `tool_call_id` → (message index, part index).
    calls: HashMap<String, (usize, usize)>,
    last_user: Option<String>,
    clock: u64,
}

impl Builder<'_> {
    fn part(&mut self, message_id: &str, mut fields: Value) -> Result<V1Part, String> {
        let id = PartId::ascending(None).map_err(|error| error.to_string())?;
        fields["id"] = json!(id);
        fields["sessionID"] = json!(self.session_id);
        fields["messageID"] = json!(message_id);
        self.report.parts += 1;
        serde_json::from_value(fields).map_err(|error| error.to_string())
    }

    /// The text and file parts of a message's `content`.
    fn content(&mut self, message_id: &str, content: &Value) -> Result<Vec<V1Part>, String> {
        let mut parts = Vec::new();
        let items: Vec<Value> = match content {
            Value::Null => Vec::new(),
            Value::String(text) => vec![json!({ "type": "text", "text": text })],
            Value::Array(items) => items.clone(),
            other => vec![json!({ "type": "text", "text": other.to_string() })],
        };
        for item in items {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("text");
            match kind {
                "text" | "input_text" | "output_text" => {
                    let text = item.get("text").and_then(Value::as_str).unwrap_or_default();
                    if !text.trim().is_empty() {
                        parts.push(self.part(message_id, json!({ "type": "text", "text": text }))?);
                    }
                }
                "image_url" | "input_image" | "image" => {
                    let url = match item.get("image_url") {
                        Some(Value::String(url)) => Some(url.clone()),
                        Some(object) => object.get("url").and_then(Value::as_str).map(String::from),
                        None => item.get("url").and_then(Value::as_str).map(String::from),
                    };
                    let Some(url) = url.filter(|url| !url.is_empty()) else {
                        continue;
                    };
                    self.report.images += 1;
                    let filename = (!url.starts_with("data:"))
                        .then(|| url.split(['?', '#']).next().unwrap_or(&url))
                        .and_then(|path| path.rsplit('/').next())
                        .filter(|name| !name.is_empty())
                        .map(String::from);
                    let mut fields = json!({ "type": "file", "mime": mime_of(&url), "url": url });
                    if let Some(filename) = filename {
                        fields["filename"] = json!(filename);
                    }
                    parts.push(self.part(message_id, fields)?);
                }
                // A content type this format does not define is kept as text
                // rather than dropped.
                _ => {
                    parts.push(self.part(
                        message_id,
                        json!({ "type": "text", "text": item.to_string() }),
                    )?);
                }
            }
        }
        Ok(parts)
    }

    fn tick(&mut self, given: Option<&Value>) -> u64 {
        let at = given.and_then(timestamp).unwrap_or(0);
        // Order in the file is the order of the conversation: a missing or
        // out-of-order time follows its predecessor.
        self.clock = at.max(self.clock + 1);
        self.clock
    }

    fn user(
        &mut self,
        at: u64,
        synthetic_text: Option<&str>,
        content: &Value,
    ) -> Result<(), String> {
        let id = MessageId::ascending(None).map_err(|error| error.to_string())?;
        let info: V1Message = serde_json::from_value(json!({
            "role": "user",
            "id": id,
            "sessionID": self.session_id,
            "time": { "created": at },
            "agent": self.options.agent,
            "model": {
                "providerID": self.options.provider_id,
                "modelID": self.options.model_id,
            },
        }))
        .map_err(|error| error.to_string())?;
        let parts = match synthetic_text {
            Some(text) => vec![self.part(
                &id,
                json!({ "type": "text", "text": text, "synthetic": true }),
            )?],
            None => self.content(&id, content)?,
        };
        self.report.user += 1;
        self.last_user = Some(id);
        self.messages.push(WithParts { info, parts });
        self.times.push(at);
        Ok(())
    }

    fn assistant_info(
        &self,
        id: &str,
        parent: &str,
        at: u64,
        finish: &str,
        summary: bool,
    ) -> Result<V1Message, String> {
        let mut info = json!({
            "role": "assistant",
            "id": id,
            "sessionID": self.session_id,
            "time": { "created": at, "completed": at },
            "parentID": parent,
            "modelID": self.options.model_id,
            "providerID": self.options.provider_id,
            "mode": self.options.agent,
            "agent": self.options.agent,
            "path": { "cwd": self.options.cwd, "root": self.options.root },
            "cost": 0,
            "tokens": {
                "input": 0, "output": 0, "reasoning": 0,
                "cache": { "read": 0, "write": 0 },
            },
            "finish": finish,
        });
        if summary {
            info["summary"] = json!(true);
        }
        serde_json::from_value(info).map_err(|error| error.to_string())
    }

    fn assistant(&mut self, at: u64, message: &TranscriptMessage) -> Result<(), String> {
        if self.last_user.is_none() {
            // An assistant turn answers a user message; a transcript that
            // opens with one gets a stand-in.
            self.user(
                at.saturating_sub(1).max(1),
                Some("(The imported conversation starts with the assistant.)"),
                &Value::Null,
            )?;
        }
        let parent = self.last_user.clone().expect("set above");
        let id = MessageId::ascending(None).map_err(|error| error.to_string())?;
        let mut parts = self.content(&id, &message.content)?;
        let calls = message.tool_calls.clone().unwrap_or_default();
        for call in &calls {
            let function = call.get("function").unwrap_or(call);
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or("unknown");
            let input: JsonMap = match function.get("arguments") {
                Some(Value::Object(object)) => object.clone(),
                Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
                    Ok(Value::Object(object)) => object,
                    _ if text.trim().is_empty() => JsonMap::new(),
                    _ => JsonMap::from_iter([("raw".to_string(), json!(text))]),
                },
                _ => JsonMap::new(),
            };
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(String::from)
                .unwrap_or_else(|| format!("call_{}", self.report.tool_calls + 1));
            // Until a `tool` message answers it, the call was interrupted.
            let part = self.part(
                &id,
                json!({
                    "type": "tool",
                    "callID": call_id,
                    "tool": name,
                    "state": {
                        "status": "error",
                        "input": input,
                        "error": INTERRUPTED_ERROR,
                        "metadata": { "interrupted": true, "imported": true },
                        "time": { "start": at, "end": at },
                    },
                }),
            )?;
            self.calls
                .insert(call_id, (self.messages.len(), parts.len()));
            parts.push(part);
            self.report.tool_calls += 1;
        }
        let finish = if calls.is_empty() {
            "stop"
        } else {
            "tool-calls"
        };
        let info = self.assistant_info(&id, &parent, at, finish, false)?;
        self.report.assistant += 1;
        self.messages.push(WithParts { info, parts });
        self.times.push(at);
        Ok(())
    }

    fn tool_result(&mut self, at: u64, message: &TranscriptMessage) {
        let found = message
            .tool_call_id
            .as_deref()
            .and_then(|id| self.calls.remove(id));
        let Some((message_index, part_index)) = found else {
            self.report.orphan_tool_results += 1;
            return;
        };
        let output = match &message.content {
            Value::String(text) => text.clone(),
            Value::Array(items) => items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            Value::Null => String::new(),
            other => other.to_string(),
        };
        let part = &mut self.messages[message_index].parts[part_index];
        if let V1Part::Tool { tool, state, .. } = part {
            let (input, start) = match state {
                alforria_schema::session_v1::V1ToolState::Error { input, time, .. } => {
                    (input.clone(), time.start)
                }
                _ => (JsonMap::new(), at),
            };
            let completed = serde_json::from_value(json!({
                "status": "completed",
                "input": input,
                "output": output,
                "title": tool,
                "metadata": { "imported": true },
                "time": { "start": start, "end": at.max(start) },
            }));
            if let Ok(completed) = completed {
                *state = completed;
            }
        }
    }
}

/// Lay the transcript out as the messages of `session_id`.
pub fn plan(
    transcript: &Transcript,
    session_id: &str,
    options: &ImportOptions,
) -> Result<ImportPlan, String> {
    let mut builder = Builder {
        session_id,
        options,
        report: ImportReport::default(),
        messages: Vec::new(),
        times: Vec::new(),
        calls: HashMap::new(),
        last_user: None,
        clock: 0,
    };
    // Without any time in the transcript, the conversation ends now.
    if !transcript
        .messages
        .iter()
        .any(|message| message.created_at.as_ref().and_then(timestamp).is_some())
    {
        builder.clock = options
            .now_ms
            .saturating_sub(transcript.messages.len() as u64 + 2);
    }
    for message in &transcript.messages {
        match message.role.as_str() {
            "system" | "developer" => builder.report.system_skipped += 1,
            "user" => {
                let at = builder.tick(message.created_at.as_ref());
                builder.user(at, None, &message.content)?;
            }
            "assistant" => {
                let at = builder.tick(message.created_at.as_ref());
                builder.assistant(at, message)?;
            }
            _ => {
                let at = builder.tick(message.created_at.as_ref());
                builder.tool_result(at, message);
            }
        }
    }
    builder.report.interrupted_tool_calls = builder.calls.len();
    builder.report.messages = builder.messages.len();
    if builder.messages.is_empty() {
        return Err("the transcript has no user or assistant messages".to_string());
    }

    let total = builder.messages.len();
    let wanted = match options.cutoff {
        None => 0,
        Some(Cutoff::ArchiveBefore(count)) => count.min(total),
        Some(Cutoff::LiveTail(count)) => total.saturating_sub(count),
        Some(Cutoff::ArchiveBeforeTime(time)) => builder
            .times
            .iter()
            .position(|at| *at >= time)
            .unwrap_or(total),
    };
    // The live part starts at a user message: half a turn cannot be sent.
    let is_user = |index: usize| matches!(builder.messages[index].info, V1Message::User { .. });
    let cut = if wanted == 0 {
        0
    } else {
        (wanted..total)
            .find(|index| is_user(*index))
            .unwrap_or(total)
    };
    builder.report.archived = cut;
    builder.report.live = total - cut;
    if cut > 0 {
        let summary = transcript
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|summary| !summary.is_empty());
        let after = builder.clock.max(*builder.times.last().unwrap_or(&0));
        let boundary_id = MessageId::ascending(None).map_err(|error| error.to_string())?;
        let boundary: V1Message = serde_json::from_value(json!({
            "role": "user",
            "id": boundary_id,
            "sessionID": session_id,
            "time": { "created": after + 1 },
            "agent": options.agent,
            "model": { "providerID": options.provider_id, "modelID": options.model_id },
        }))
        .map_err(|error| error.to_string())?;
        let mut marker = json!({ "type": "compaction", "auto": false });
        if cut < total {
            marker["tail_start_id"] = json!(crate::session::message::message_id(
                &builder.messages[cut].info
            ));
        }
        let marker = builder.part(&boundary_id, marker)?;
        let summary_id = MessageId::ascending(None).map_err(|error| error.to_string())?;
        let info = builder.assistant_info(&summary_id, &boundary_id, after + 2, "stop", true)?;
        let mut text = json!({
            "type": "text",
            "text": summary.unwrap_or(NEEDS_SUMMARY_TEXT),
            "metadata": { "import": { "needsSummary": summary.is_none() } },
        });
        if summary.is_none() {
            text["synthetic"] = json!(true);
        }
        let text = builder.part(&summary_id, text)?;
        builder.messages.push(WithParts {
            info: boundary,
            parts: vec![marker],
        });
        builder.messages.push(WithParts {
            info,
            parts: vec![text],
        });
        builder.report.boundary = Some(if summary.is_some() {
            "summary"
        } else {
            "needs_summary"
        });
    }
    Ok(ImportPlan {
        messages: builder.messages,
        report: builder.report,
    })
}

fn row_data(value: Value, drop: &[&str]) -> Result<String, CoreError> {
    let mut value = value;
    if let Some(object) = value.as_object_mut() {
        for key in drop {
            object.remove(*key);
        }
    }
    serde_json::to_string(&value).map_err(|error| CoreError::Storage(error.to_string()))
}

/// Store the planned messages and parts of `session_id`, all or nothing.
pub fn write(
    storage: &Storage,
    session_id: &str,
    plan: &ImportPlan,
    now_ms: u64,
) -> Result<(), CoreError> {
    storage.with_connection_mut(|conn| {
        let tx = conn.transaction()?;
        {
            let mut message = tx.prepare(
                "INSERT INTO message (id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            let mut part = tx.prepare(
                "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for item in &plan.messages {
                let id = crate::session::message::message_id(&item.info).to_string();
                let created = crate::session::message::message_time_created(&item.info) as i64;
                let info = serde_json::to_value(&item.info)
                    .map_err(|error| CoreError::Storage(error.to_string()))?;
                message.execute(rusqlite::params![
                    id,
                    session_id,
                    created,
                    now_ms as i64,
                    row_data(info, &["id", "sessionID"])?
                ])?;
                for stored in &item.parts {
                    let value = serde_json::to_value(stored)
                        .map_err(|error| CoreError::Storage(error.to_string()))?;
                    let part_id = value
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    part.execute(rusqlite::params![
                        part_id,
                        id,
                        session_id,
                        created,
                        now_ms as i64,
                        row_data(value, &["id", "sessionID", "messageID"])?
                    ])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    })
}

/// The session an earlier import with this `import_id` created, if any.
pub fn find_import(storage: &Storage, import_id: &str) -> Result<Option<String>, CoreError> {
    use rusqlite::OptionalExtension;
    storage.with_connection(|conn| {
        Ok(conn
            .query_row(
                "SELECT id FROM session
                 WHERE metadata IS NOT NULL AND json_extract(metadata, '$.import.id') = ?1
                 ORDER BY time_created LIMIT 1",
                [import_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?)
    })
}

/// The session metadata that records an import.
pub fn import_metadata(transcript: &Transcript, report: &ImportReport) -> JsonMap {
    let mut import = json!({
        "format": "openai-chat",
        "messages": report.messages,
        "archived": report.archived,
    });
    if let Some(id) = &transcript.import_id {
        import["id"] = json!(id);
    }
    if report.boundary == Some("needs_summary") {
        import["needsSummary"] = json!(true);
    }
    let mut metadata = match &transcript.metadata {
        Some(Value::Object(object)) => object.clone(),
        _ => JsonMap::new(),
    };
    metadata.insert("import".to_string(), import);
    metadata
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
