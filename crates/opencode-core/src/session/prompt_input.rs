//! Session prompt input — port of `session/prompt.ts` `createUserMessage`
//! (635-1050) and `resolvePromptParts` (157-191).
//!
//! Not ported (no M5 seam yet, per spec):
//!
//! * `Effect.addFinalizer(() => instruction.clear(info.id))` — finalizers
//!   belong to the M5.4 processor loop.
//! * `plugin.trigger("chat.message", ...)` — no plugin registry in M5.
//! * the pre-save schema validation logs (1022-1044) — in TS those re-decode
//!   dynamic objects, but here the parts are built as typed
//!   [`opencode_schema::session_v1::V1Part`] values so decode cannot fail.
//!
//! `pathToFileURL`/`fileURLToPath` follow Node's POSIX behavior (probed
//! against Node 24): the "no-escape" set is `[A-Za-z0-9]` plus
//! `!$&'()*+,-./:;=@_` — note `~`, `?` and `#` ARE escaped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::session::ids::{MessageId, PartId};
use opencode_schema::model::ModelInfo;
use opencode_schema::session_v1::{
    AgentPartSource, OutputFormat, TextPartTime, UserTime, V1FilePartSource, V1Message, V1Part,
    V1SessionModel, V1UserModel,
};
use serde_json::Value;

use crate::event::bus::EventBus;
use crate::event::bus::PublishOptions;
use crate::session::agents::{AgentInfo, AgentRegistry};
use crate::session::error::SessionError;
use crate::session::message::WithParts;
use crate::session::store::SessionStore;
use crate::tool::def::{Ask, AskRequest, ExecuteResult, Extra, MetadataSink, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::lsp::LspServer;
use crate::tool::permission::evaluate;
use crate::CoreError;

use crate::session::event_definitions::SESSION_ERROR;

const MAX_MCP_RESOURCE_BLOB_BYTES: usize = 10 * 1024 * 1024;
const SUPPORTED_MCP_RESOURCE_ATTACHMENT_MIMES: [&str; 5] = [
    "application/pdf",
    "image/gif",
    "image/jpeg",
    "image/png",
    "image/webp",
];

/// `mcpResourceBase64Size` (prompt.ts:84-88) — decoded byte size of a
/// base64 payload (whitespace stripped, padding subtracted).
fn mcp_resource_base64_size(value: &str) -> usize {
    let trimmed: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    let padding = if trimmed.ends_with("==") {
        2
    } else if trimmed.ends_with('=') {
        1
    } else {
        0
    };
    ((trimmed.len() * 3) / 4).saturating_sub(padding)
}

/// `formatMcpResourceBytes` (prompt.ts:90-94).
fn format_mcp_resource_bytes(value: usize) -> String {
    if value < 1024 {
        format!("{value} B")
    } else if value < 1024 * 1024 {
        format!("{} KB", value.div_ceil(1024))
    } else {
        format!("{} MB", value.div_ceil(1024 * 1024))
    }
}

// ---------------------------------------------------------------------------
// Inputs (prompt.ts:1497-1520, schema-src/v1/session.ts:397-451)
// ---------------------------------------------------------------------------

/// `ModelRef` (prompt.ts:1493-1496).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
}

/// `PromptInput["parts"][number]` — `TextPartInput` | `FilePartInput` |
/// `AgentPartInput` | `SubtaskPartInput` (session.ts:397-451).
#[derive(Debug, Clone, PartialEq)]
pub enum PromptPartInput {
    Text {
        id: Option<String>,
        text: String,
        synthetic: Option<bool>,
        ignored: Option<bool>,
        time: Option<TextPartTime>,
        metadata: Option<serde_json::Map<String, Value>>,
    },
    File {
        id: Option<String>,
        mime: String,
        filename: Option<String>,
        url: String,
        source: Option<V1FilePartSource>,
    },
    Agent {
        id: Option<String>,
        name: String,
        source: Option<AgentPartSource>,
    },
    Subtask {
        id: Option<String>,
        prompt: String,
        description: String,
        agent: String,
        model: Option<opencode_schema::session_v1::V1SubtaskModel>,
        command: Option<String>,
    },
}

/// `PromptInput` (prompt.ts:1504-1520).
#[derive(Debug, Clone, PartialEq)]
pub struct PromptInput {
    pub session_id: String,
    pub message_id: Option<String>,
    pub model: Option<ModelRef>,
    pub agent: Option<String>,
    pub tools: Option<BTreeMap<String, bool>>,
    pub format: Option<OutputFormat>,
    pub system: Option<String>,
    pub variant: Option<String>,
    pub parts: Vec<PromptPartInput>,
}

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

/// The `provider.getModel` / `provider.defaultModel` slice
/// (provider/provider.ts).
pub trait Models: Send + Sync {
    fn get_model<'a>(
        &'a self,
        provider_id: &'a str,
        model_id: &'a str,
    ) -> crate::tool::def::BoxFuture<'a, Result<ModelInfo, CoreError>>;
    fn default_model(&self) -> crate::tool::def::BoxFuture<'static, Result<ModelInfo, CoreError>>;
}

/// One `MCPReadResourceContents` item.
#[derive(Debug, Clone, Default)]
pub struct McpResourceItem {
    pub text: Option<String>,
    pub blob: Option<String>,
    pub mime_type: Option<String>,
    pub uri: Option<String>,
}

/// `mcp.readResource(clientName, uri)`: falsy content is a
/// `Resource not found: {clientName}/{uri}` error in TS.
#[derive(Debug, Clone)]
pub enum McpReadResource {
    NotFound,
    Contents(Vec<McpResourceItem>),
}

/// The `MCP.Service.readResource` seam.
pub trait McpResources: Send + Sync {
    fn read_resource<'a>(
        &'a self,
        client_name: &'a str,
        uri: &'a str,
    ) -> crate::tool::def::BoxFuture<'a, Result<McpReadResource, String>>;
}

/// `Image.Error` (image/image.ts:51).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// `ResizerUnavailableError` — the caller keeps the original part.
    ResizerUnavailable,
    /// Decode/size/data-url failures are fatal.
    Invalid(String),
}

/// The `Image.Service` seam (image normalize).
pub trait ImageNormalizer: Send + Sync {
    fn normalize<'a>(
        &'a self,
        part: V1Part,
    ) -> crate::tool::def::BoxFuture<'a, Result<V1Part, ImageError>>;
}

/// The M5 default image seam: the resizer is unavailable, so every
/// normalize keeps the original part (the TS
/// `catchIf(ResizerUnavailableError)` path).
pub struct NoResize;

impl ImageNormalizer for NoResize {
    fn normalize<'a>(
        &'a self,
        _part: V1Part,
    ) -> crate::tool::def::BoxFuture<'a, Result<V1Part, ImageError>> {
        Box::pin(async move { Err(ImageError::ResizerUnavailable) })
    }
}

/// `PromptInput` wired against the runtime services. `worktree` is
/// `InstanceState.context.worktree` (used by `resolvePromptParts`).
pub struct PromptDeps<'a> {
    pub events: &'a EventBus,
    pub sessions: &'a SessionStore,
    pub agents: &'a AgentRegistry,
    pub models: &'a dyn Models,
    pub read: &'a ToolDef,
    pub mcp: &'a dyn McpResources,
    pub lsp: &'a dyn LspServer,
    pub images: &'a dyn ImageNormalizer,
    pub worktree: PathBuf,
    pub now_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("{message}")]
    Unknown { message: String },
}

/// `NamedError.Unknown` — published as
/// `{ sessionID, error: { name: "Unknown", data: { message } } }`.
fn publish_session_error(
    deps: &PromptDeps<'_>,
    session_id: &str,
    message: &str,
) -> Result<(), PromptError> {
    deps.events.publish(
        &SESSION_ERROR,
        serde_json::json!({
            "sessionID": session_id,
            "error": { "name": "Unknown", "data": { "message": message } },
        }),
        PublishOptions::default(),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// URL + JSON helpers
// ---------------------------------------------------------------------------

/// Node `pathToFileURL` (POSIX): `[A-Za-z0-9]` plus `!$&'()*+,-./:;=@_`
/// stay literal, everything else percent-encodes as UTF-8 (uppercase hex).
pub fn path_to_file_url(path: &Path) -> String {
    fn is_safe(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || b"!$&'()*+,-./:;=@_".contains(&byte)
    }
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().as_bytes() {
        if is_safe(*byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

/// Percent-decode `%XX` sequences (and `+` when `plus_space`, matching
/// `URLSearchParams`).
fn percent_decode(input: &str, plus_space: bool) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16));
                let lo = bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16));
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' if plus_space => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Node `fileURLToPath` (POSIX): pathname portion, percent-decoded.
pub fn file_url_to_path(url: &str) -> PathBuf {
    let rest = url.strip_prefix("file://").unwrap_or(url);
    let path = rest.split(['?', '#']).next().unwrap_or(rest);
    PathBuf::from(percent_decode(path, false))
}

/// The `url.searchParams.get(key)` slice: first query value or `None`.
fn url_query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1.split('#').next().unwrap_or("");
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(percent_decode(v, true));
            }
        }
    }
    None
}

/// `decodeDataUrl` (util/data-url.ts:1-9): text after the first comma —
/// base64-decoded when the head carries `;base64`, percent-decoded
/// otherwise.
pub fn decode_data_url(url: &str) -> String {
    let Some(idx) = url.find(',') else {
        return String::new();
    };
    let head = &url[..idx];
    let body = &url[idx + 1..];
    if head.contains(";base64") {
        String::from_utf8_lossy(&base64_decode(body)).into_owned()
    } else {
        percent_decode(body, false)
    }
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 encode (`Buffer.toString("base64")`).
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let word = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(BASE64_ALPHABET[(word >> 18) as usize & 0x3f] as char);
        out.push(BASE64_ALPHABET[(word >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            BASE64_ALPHABET[(word >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64_ALPHABET[word as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Standard base64 decode; invalid characters (including whitespace) are
/// skipped, padding tolerated (Node `Buffer.from(body, "base64")`).
pub fn base64_decode(input: &str) -> Vec<u8> {
    let value_of = |byte: u8| -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a') as u32 + 26),
            b'0'..=b'9' => Some((byte - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let digits: Vec<u32> = input.bytes().filter_map(value_of).collect();
    let mut out = Vec::with_capacity(digits.len() * 3 / 4);
    for chunk in digits.chunks(4) {
        let word = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, d)| acc | (d << (18 - 6 * i)));
        out.push((word >> 16) as u8);
        if chunk.len() > 2 {
            out.push((word >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(word as u8);
        }
    }
    out
}

/// `JSON.stringify` for a string value.
pub fn json_string(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `JSON.stringify` for a number — integers lose their `.0`, `NaN`/`Infinity`
/// become `null`.
fn json_num(value: f64) -> String {
    if !value.is_finite() {
        return "null".to_string();
    }
    if value == value.trunc() && value.abs() < 9.007199254740992e15 {
        return format!("{}", value as i64);
    }
    format!("{value}")
}

fn json_num_value(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// JS `parseInt` (radix 10): leading whitespace, optional sign, leading
/// digits; `NaN` when no digits parse.
fn parse_int(input: &str) -> f64 {
    let trimmed = input.trim_start();
    let (sign, rest) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return f64::NAN;
    }
    sign * digits.parse::<f64>().unwrap_or(f64::NAN)
}

// ---------------------------------------------------------------------------
// resolvePromptParts (prompt.ts:157-191)
// ---------------------------------------------------------------------------

/// `ConfigMarkdown.files` (config/markdown.ts:5-10):
/// `/(?<![\w`])@(\.?[^\s`,.]*(?:\.[^\s`,.]+)*)/g` — the lookbehind is
/// scanned manually (the `regex` crate has no lookbehind). A "name char"
/// is `[^\s`,.]` — any char but whitespace, backtick, comma and dot. JS
/// `\s` is approximated with `char::is_whitespace` (documented divergence).
fn is_file_name_char(c: char) -> bool {
    !c.is_whitespace() && c != '`' && c != ',' && c != '.'
}

/// All `matchAll` captures of `FILE_REGEX` — the group-1 strings, in order.
fn file_matches(template: &str) -> Vec<String> {
    let chars: Vec<(usize, char)> = template.char_indices().collect();
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < chars.len() {
        let (_, c) = chars[idx];
        if c != '@' {
            idx += 1;
            continue;
        }
        // Lookbehind (?<![\w`]) — the char immediately before '@'.
        if idx > 0 {
            let prev = chars[idx - 1].1;
            if prev.is_ascii_alphanumeric() || prev == '_' || prev == '`' {
                idx += 1;
                continue;
            }
        }
        // Group 1: \.?[^\s`,.]*(?:\.[^\s`,.]+)*
        let mut i = idx + 1;
        if i < chars.len() && chars[i].1 == '.' {
            i += 1;
        }
        while i < chars.len() && is_file_name_char(chars[i].1) {
            i += 1;
        }
        loop {
            if i + 1 < chars.len() && chars[i].1 == '.' && is_file_name_char(chars[i + 1].1) {
                i += 1;
                while i < chars.len() && is_file_name_char(chars[i].1) {
                    i += 1;
                }
            } else {
                break;
            }
        }
        let start = chars
            .get(idx + 1)
            .map(|(p, _)| *p)
            .unwrap_or(template.len());
        let end = chars.get(i).map(|(p, _)| *p).unwrap_or(template.len());
        out.push(template[start..end].to_string());
        idx = if i == idx + 1 { idx + 1 } else { i };
    }
    out
}

/// `path.resolve(worktree, name)` — lexical resolution (absolute `name`
/// wins, `..`/`.` segments normalized).
fn resolve_from(base: &Path, name: &str) -> PathBuf {
    let joined = if name.starts_with('/') {
        PathBuf::from(name)
    } else {
        base.join(name)
    };
    crate::session::instruction::lexical_absolute(&joined)
}

/// `resolvePromptParts` (prompt.ts:157-191): the template text plus a file
/// part per `@`-mention that exists on disk, or an agent part for names
/// that resolve to a registered agent.
pub fn resolve_prompt_parts(
    agents: &AgentRegistry,
    worktree: &Path,
    template: &str,
) -> Vec<PromptPartInput> {
    let mut parts = vec![PromptPartInput::Text {
        id: None,
        text: template.to_string(),
        synthetic: None,
        ignored: None,
        time: None,
        metadata: None,
    }];
    let mut seen = std::collections::HashSet::new();
    for name in file_matches(template) {
        if name.is_empty() || !seen.insert(name.clone()) {
            continue;
        }
        let filepath = if let Some(rest) = name.strip_prefix("~/") {
            Path::new(&std::env::var("HOME").unwrap_or_default()).join(rest)
        } else {
            resolve_from(worktree, &name)
        };
        let Ok(stat) = std::fs::metadata(&filepath) else {
            if let Some(found) = agents.get(&name) {
                parts.push(PromptPartInput::Agent {
                    id: None,
                    name: found.name.clone(),
                    source: None,
                });
            }
            continue;
        };
        parts.push(PromptPartInput::File {
            id: None,
            mime: if stat.is_dir() {
                "application/x-directory".to_string()
            } else {
                "text/plain".to_string()
            },
            filename: Some(name),
            url: path_to_file_url(&filepath),
            source: None,
        });
    }
    parts
}

// ---------------------------------------------------------------------------
// createUserMessage (prompt.ts:635-1050)
// ---------------------------------------------------------------------------

/// The resolved `model` for the message: `input.model ?? ag.model ?? currentModel()`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedModel {
    provider_id: String,
    model_id: String,
    #[allow(dead_code)]
    variant: Option<String>,
}

/// `currentModel` (prompt.ts:614-633): the session's model, else the model
/// of the first user message, else the provider default.
async fn current_model(
    deps: &PromptDeps<'_>,
    session_id: &str,
) -> Result<ResolvedModel, PromptError> {
    let current = deps.sessions.get(session_id)?;
    if let Some(model) = current.model {
        return Ok(ResolvedModel {
            provider_id: model.provider_id,
            model_id: model.id,
            variant: if model.variant.as_deref() == Some("default") {
                None
            } else {
                model.variant
            },
        });
    }
    let match_ = deps
        .sessions
        .find_message(session_id, &|_: &WithParts| true)?;
    if let Some(found) = match_ {
        if let V1Message::User { model, .. } = found.info {
            return Ok(ResolvedModel {
                provider_id: model.provider_id,
                model_id: model.model_id,
                variant: model.variant,
            });
        }
    }
    let model = deps.models.default_model().await?;
    Ok(ResolvedModel {
        provider_id: model.provider_id,
        model_id: model.id,
        variant: None,
    })
}

/// `createUserMessage` output.
#[derive(Debug)]
pub struct CreateUserMessage {
    pub info: V1Message,
    pub parts: Vec<V1Part>,
}

/// `Effect.addFinalizer` / pre-save decode-validation gaps are documented
/// in the module header; everything else is a straight port.
pub async fn create_user_message(
    deps: &PromptDeps<'_>,
    input: &PromptInput,
) -> Result<CreateUserMessage, PromptError> {
    // 641-651 — agent resolution.
    let agent_name = input.agent.as_deref();
    let ag = match agent_name {
        Some(name) => match deps.agents.get(name) {
            Some(found) => found.clone(),
            None => {
                let available: Vec<String> = deps
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
                let message = format!("Agent not found: \"{name}\".{hint}");
                publish_session_error(deps, &input.session_id, &message)?;
                return Err(PromptError::Unknown { message });
            }
        },
        None => deps
            .agents
            .default_info()
            .map_err(|err| PromptError::Unknown {
                message: err.to_string(),
            })?,
    };

    // 653-663 — model resolution + variant fallback.
    let model = match (&input.model, &ag.model) {
        (Some(m), _) => ResolvedModel {
            provider_id: m.provider_id.clone(),
            model_id: m.model_id.clone(),
            variant: None,
        },
        (None, Some(m)) => ResolvedModel {
            provider_id: m.provider_id.clone(),
            model_id: m.model_id.clone(),
            variant: None,
        },
        (None, None) => current_model(deps, &input.session_id).await?,
    };
    let same = ag
        .model
        .as_ref()
        .is_some_and(|m| model.provider_id == m.provider_id && model.model_id == m.model_id);
    let full = if input.variant.is_none() && ag.variant.is_some() && same {
        deps.models
            .get_model(&model.provider_id, &model.model_id)
            .await
            .ok()
    } else {
        None
    };
    let variant = input.variant.clone().or_else(|| {
        ag.variant
            .as_ref()
            .filter(|v| {
                full.as_ref()
                    .is_some_and(|f| f.variants.iter().any(|var| var.id == **v))
            })
            .cloned()
    });

    // 665-676 — the user message record.
    let info = V1Message::User {
        id: match &input.message_id {
            Some(id) => id.clone(),
            None => MessageId::ascending(None)?,
        },
        session_id: input.session_id.clone(),
        time: UserTime {
            created: deps.now_ms as f64,
        },
        format: input.format.clone(),
        summary: None,
        agent: ag.name.clone(),
        model: V1UserModel {
            provider_id: model.provider_id.clone(),
            model_id: model.model_id.clone(),
            variant: variant.clone(),
        },
        system: input.system.clone(),
        tools: input.tools.clone(),
    };
    let message_id = match &info {
        V1Message::User { id, .. } => id.clone(),
        _ => unreachable!("just constructed as user"),
    };

    // 678-697 — agent/model sync into the session.
    let current = deps.sessions.get(&input.session_id)?;
    let current_variant = match current.model.as_ref().and_then(|m| m.variant.as_deref()) {
        Some("default") => None,
        other => other,
    };
    if current.agent.as_deref() != Some(ag.name.as_str())
        || current.model.as_ref().map(|m| m.provider_id.as_str())
            != Some(model.provider_id.as_str())
        || current.model.as_ref().map(|m| m.id.as_str()) != Some(model.model_id.as_str())
        || current_variant != variant.as_deref()
    {
        deps.sessions.set_agent_model(
            &input.session_id,
            &ag.name,
            V1SessionModel {
                id: model.model_id.clone(),
                provider_id: model.provider_id.clone(),
                variant: Some(variant.clone().unwrap_or_else(|| "default".to_string())),
            },
            deps.now_ms,
        )?;
    }

    // 699-1050 — resolvePart + assign, image normalize, persist.
    let mut resolved: Vec<V1Part> = Vec::new();
    for part in &input.parts {
        resolved.extend(resolve_part(deps, input, &ag, &model, &message_id, part).await?);
    }

    let mut parts: Vec<V1Part> = Vec::with_capacity(resolved.len());
    for part in resolved {
        if let V1Part::File { mime, .. } = &part {
            if mime.starts_with("image/") {
                match deps.images.normalize(part.clone()).await {
                    Ok(normalized) => parts.push(normalized),
                    Err(ImageError::ResizerUnavailable) => parts.push(part),
                    Err(ImageError::Invalid(message)) => {
                        return Err(PromptError::Unknown { message })
                    }
                }
                continue;
            }
        }
        parts.push(part);
    }

    deps.sessions.update_message(&info)?;
    for part in &parts {
        deps.sessions.update_part(part)?;
    }

    Ok(CreateUserMessage { info, parts })
}

/// The `ask`/`metadata` context tools get when resolved from the prompt
/// (`ask: () => Effect.void`, `metadata: () => Effect.void`).
struct NoopAsk;

impl Ask for NoopAsk {
    fn ask(&self, _request: AskRequest) -> crate::tool::def::BoxFuture<'_, Result<(), ToolError>> {
        Box::pin(async move { Ok(()) })
    }
}

impl MetadataSink for NoopAsk {
    fn metadata(
        &self,
        _input: crate::tool::def::MetadataInput,
    ) -> crate::tool::def::BoxFuture<'_, Result<(), ToolError>> {
        Box::pin(async move { Ok(()) })
    }
}

fn next_id() -> Result<String, PromptError> {
    PartId::ascending(None).map_err(PromptError::Core)
}

/// A synthetic text part (`type: "text"`, `synthetic: true`).
fn synthetic_text(session_id: &str, message_id: &str, text: String) -> Result<V1Part, PromptError> {
    Ok(V1Part::Text {
        id: next_id()?,
        session_id: session_id.to_string(),
        message_id: message_id.to_string(),
        text,
        synthetic: Some(true),
        ignored: None,
        time: None,
        metadata: None,
    })
}

/// `JSON.stringify({ filePath, offset, limit })` — undefined fields dropped,
/// NaN rendered as `null`. Returns the text and the tool-call value.
fn json_read_args(file_path: &str, offset: Option<f64>, limit: Option<f64>) -> (String, Value) {
    let mut text = format!("{{\"filePath\":{}", json_string(file_path));
    let mut value = serde_json::Map::new();
    value.insert("filePath".to_string(), Value::String(file_path.to_string()));
    if let Some(offset) = offset {
        text.push_str(&format!(",\"offset\":{}", json_num(offset)));
        value.insert("offset".to_string(), json_num_value(offset));
    }
    if let Some(limit) = limit {
        text.push_str(&format!(",\"limit\":{}", json_num(limit)));
        value.insert("limit".to_string(), json_num_value(limit));
    }
    text.push('}');
    (text, Value::Object(value))
}

/// The read-tool call context (`execRead`, prompt.ts:838-855).
async fn exec_read(
    deps: &PromptDeps<'_>,
    session_id: &str,
    message_id: &str,
    agent: &str,
    args: Value,
    extra_model: Option<Value>,
) -> Result<ExecuteResult, String> {
    let ask = NoopAsk;
    let extra = Extra {
        bypass_cwd_check: true,
        bypass_agent_check: false,
        model: extra_model,
    };
    let ctx = ToolCtxRef {
        session_id,
        message_id,
        agent,
        call_id: None,
        abort: tokio_util::sync::CancellationToken::new(),
        messages: &[],
        extra: &extra,
        instance: &crate::tool::def::InstanceContext {
            directory: deps.worktree.clone(),
            worktree: deps.worktree.clone(),
        },
        ask: &ask,
        metadata: &ask,
    };
    (deps.read.execute)(args, ctx)
        .await
        .map_err(|err| err.to_string())
}

/// `resolvePart` (prompt.ts:699-993): one input part to resolved parts.
async fn resolve_part(
    deps: &PromptDeps<'_>,
    input: &PromptInput,
    ag: &AgentInfo,
    model: &ResolvedModel,
    message_id: &str,
    part: &PromptPartInput,
) -> Result<Vec<V1Part>, PromptError> {
    let session_id = input.session_id.as_str();

    match part {
        // 702-993 — file parts.
        PromptPartInput::File {
            id,
            mime,
            filename,
            url,
            source,
        } => {
            resolve_file_part(
                deps,
                input,
                ag,
                model,
                message_id,
                id.clone(),
                mime,
                filename,
                url,
                source,
            )
            .await
        }

        // 974-990 — agent parts.
        PromptPartInput::Agent { .. } => resolve_agent_part(input, ag, message_id, part),

        // Passthrough — text parts and subtasks.
        PromptPartInput::Text {
            id,
            text,
            synthetic,
            ignored,
            time,
            metadata,
        } => Ok(vec![V1Part::Text {
            id: id.clone().map_or_else(next_id, Ok)?,
            session_id: session_id.to_string(),
            message_id: message_id.to_string(),
            text: text.clone(),
            synthetic: *synthetic,
            ignored: *ignored,
            time: *time,
            metadata: metadata.clone(),
        }]),
        PromptPartInput::Subtask {
            id,
            prompt,
            description,
            agent,
            model: subtask_model,
            command,
        } => Ok(vec![V1Part::Subtask {
            id: id.clone().map_or_else(next_id, Ok)?,
            session_id: session_id.to_string(),
            message_id: message_id.to_string(),
            prompt: prompt.clone(),
            description: description.clone(),
            agent: agent.clone(),
            model: subtask_model.clone(),
            command: command.clone(),
        }]),
    }
}

#[allow(clippy::too_many_arguments)]
async fn resolve_file_part(
    deps: &PromptDeps<'_>,
    input: &PromptInput,
    ag: &AgentInfo,
    model: &ResolvedModel,
    message_id: &str,
    id: Option<String>,
    mime: &str,
    filename: &Option<String>,
    url: &str,
    source: &Option<V1FilePartSource>,
) -> Result<Vec<V1Part>, PromptError> {
    let session_id = input.session_id.as_str();

    // MCP resource source (703-783).
    if let Some(V1FilePartSource::Resource {
        client_name, uri, ..
    }) = source
    {
        let mut pieces = vec![synthetic_text(
            session_id,
            message_id,
            format!(
                "Reading MCP resource: {} ({uri})",
                filename.as_deref().unwrap_or("undefined")
            ),
        )?];
        let contents = match deps.mcp.read_resource(client_name, uri).await {
            Ok(McpReadResource::NotFound) => {
                return Err(PromptError::Unknown {
                    message: format!("Resource not found: {client_name}/{uri}"),
                })
            }
            Ok(McpReadResource::Contents(contents)) => contents,
            Err(message) => {
                pieces.push(synthetic_text(
                    session_id,
                    message_id,
                    format!(
                        "Failed to read MCP resource {}: {message}",
                        filename.as_deref().unwrap_or("undefined")
                    ),
                )?);
                return Ok(pieces);
            }
        };
        for c in contents {
            if let Some(text) = c.text.as_deref().filter(|t| !t.is_empty()) {
                pieces.push(synthetic_text(session_id, message_id, text.to_string())?);
                continue;
            }
            if let Some(blob) = c.blob.as_deref().filter(|b| !b.is_empty()) {
                let item_mime = c.mime_type.as_deref().unwrap_or(mime);
                let item_filename = c.uri.clone().or_else(|| filename.clone());
                let size = mcp_resource_base64_size(blob);
                if !SUPPORTED_MCP_RESOURCE_ATTACHMENT_MIMES.contains(&item_mime) {
                    pieces.push(synthetic_text(
                        session_id,
                        message_id,
                        format!(
                            "[Binary MCP resource omitted: {} ({item_mime}, {}) is not a supported attachment type]",
                            item_filename.clone().unwrap_or_else(|| "undefined".to_string()),
                            format_mcp_resource_bytes(size),
                        ),
                    )?);
                    continue;
                }
                if size > MAX_MCP_RESOURCE_BLOB_BYTES {
                    pieces.push(synthetic_text(
                        session_id,
                        message_id,
                        format!(
                            "[Binary MCP resource omitted: {} ({item_mime}, {}) exceeds {}]",
                            item_filename.unwrap_or_else(|| "undefined".to_string()),
                            format_mcp_resource_bytes(size),
                            format_mcp_resource_bytes(MAX_MCP_RESOURCE_BLOB_BYTES),
                        ),
                    )?);
                    continue;
                }
                pieces.push(synthetic_text(
                    session_id,
                    message_id,
                    format!(
                        "[Binary MCP resource attached: {} ({item_mime})]",
                        item_filename
                            .clone()
                            .unwrap_or_else(|| "undefined".to_string()),
                    ),
                )?);
                pieces.push(V1Part::File {
                    id: next_id()?,
                    session_id: session_id.to_string(),
                    message_id: message_id.to_string(),
                    mime: item_mime.to_string(),
                    filename: item_filename,
                    url: format!("data:{item_mime};base64,{blob}"),
                    source: None,
                });
            }
        }
        return Ok(pieces);
    }

    let protocol = url.split(':').next().unwrap_or_default();
    match (protocol, mime) {
        // 788-818 — data: URLs.
        ("data", "text/plain") => {
            let arg = match filename {
                Some(f) => format!("{{\"filePath\":{}}}", json_string(f)),
                None => "{}".to_string(),
            };
            return Ok(vec![
                synthetic_text(
                    session_id,
                    message_id,
                    format!("Called the Read tool with the following input: {arg}"),
                )?,
                synthetic_text(session_id, message_id, decode_data_url(url))?,
                V1Part::File {
                    id: id.clone().map_or_else(next_id, Ok)?,
                    session_id: session_id.to_string(),
                    message_id: message_id.to_string(),
                    mime: mime.to_string(),
                    filename: filename.clone(),
                    url: url.to_string(),
                    source: source.clone(),
                },
            ]);
        }
        // 819-970 — file: URLs.
        ("file", _) => {
            let filepath = file_url_to_path(url);
            let part_mime = if filepath.is_dir() {
                "application/x-directory".to_string()
            } else {
                mime.to_string()
            };

            // 856-906 — text files: range params + LSP documentSymbol expansion.
            if part_mime == "text/plain" {
                let mut offset: Option<f64> = None;
                let mut limit: Option<f64> = None;
                let range_start = url_query_param(url, "start");
                let range_end = url_query_param(url, "end");
                if let Some(start_raw) = range_start {
                    let file_path_uri = url.split('?').next().unwrap_or(url).to_string();
                    let mut start = parse_int(&start_raw);
                    let mut end = range_end.map(|e| parse_int(&e));
                    if start.is_finite() && start == end.unwrap_or(f64::NAN) {
                        let symbols = deps.lsp.document_symbol(&file_path_uri).await;
                        for symbol in symbols {
                            let r = symbol
                                .get("range")
                                .or_else(|| symbol.get("location").and_then(|l| l.get("range")));
                            let line = r
                                .and_then(|r| r.get("start"))
                                .and_then(|s| s.get("line"))
                                .and_then(Value::as_f64);
                            if let Some(line) = line {
                                // `r?.start?.line &&` — 0 is falsy.
                                if line != 0.0 && line == start {
                                    start = line;
                                    end = Some(
                                        r.and_then(|r| r.get("end"))
                                            .and_then(|e| e.get("line"))
                                            .and_then(Value::as_f64)
                                            .unwrap_or(start),
                                    );
                                    break;
                                }
                            }
                        }
                    }
                    offset = Some(start.max(1.0));
                    if let Some(e) = end {
                        // `if (end)` — NaN/0 falsy.
                        if !e.is_nan() && e != 0.0 {
                            limit = Some(e - (offset.unwrap_or(f64::NAN) - 1.0));
                        }
                    }
                }
                let (args_text, args_value) =
                    json_read_args(&filepath.to_string_lossy(), offset, limit);
                let mut pieces = vec![synthetic_text(
                    session_id,
                    message_id,
                    format!("Called the Read tool with the following input: {args_text}"),
                )?];
                let full = deps
                    .models
                    .get_model(&model.provider_id, &model.model_id)
                    .await?;
                let full_value =
                    serde_json::to_value(&full).map_err(|err| PromptError::Unknown {
                        message: err.to_string(),
                    })?;
                let agent = input.agent.as_deref().unwrap_or(&ag.name).to_string();
                match exec_read(
                    deps,
                    session_id,
                    message_id,
                    &agent,
                    args_value,
                    Some(full_value),
                )
                .await
                {
                    Ok(result) => {
                        pieces.push(synthetic_text(
                            session_id,
                            message_id,
                            result.output.clone(),
                        )?);
                        if let Some(attachments) = result.attachments.filter(|a| !a.is_empty()) {
                            for a in attachments {
                                pieces.push(V1Part::File {
                                    id: next_id()?,
                                    session_id: session_id.to_string(),
                                    message_id: message_id.to_string(),
                                    mime: a.mime.clone(),
                                    filename: a.filename.clone().or_else(|| filename.clone()),
                                    url: a.url.clone(),
                                    source: None,
                                });
                            }
                        } else {
                            pieces.push(V1Part::File {
                                id: id.clone().map_or_else(next_id, Ok)?,
                                session_id: session_id.to_string(),
                                message_id: message_id.to_string(),
                                mime: part_mime.clone(),
                                filename: filename.clone(),
                                url: url.to_string(),
                                source: source.clone(),
                            });
                        }
                    }
                    Err(message) => {
                        publish_session_error(deps, session_id, &message)?;
                        pieces.push(synthetic_text(
                            session_id,
                            message_id,
                            format!(
                                "Read tool failed to read {} with the following error: {message}",
                                filepath.display()
                            ),
                        )?);
                    }
                }
                return Ok(pieces);
            }

            // 907-937 — directories.
            if part_mime == "application/x-directory" {
                let (args_text, args_value) =
                    json_read_args(&filepath.to_string_lossy(), None, None);
                let agent = input.agent.as_deref().unwrap_or(&ag.name).to_string();
                match exec_read(deps, session_id, message_id, &agent, args_value, None).await {
                    Err(message) => {
                        publish_session_error(deps, session_id, &message)?;
                        return Ok(vec![synthetic_text(
                            session_id,
                            message_id,
                            format!(
                                "Read tool failed to read {} with the following error: {message}",
                                filepath.display()
                            ),
                        )?]);
                    }
                    Ok(result) => {
                        return Ok(vec![
                            synthetic_text(
                                session_id,
                                message_id,
                                format!(
                                    "Called the Read tool with the following input: {args_text}"
                                ),
                            )?,
                            synthetic_text(session_id, message_id, result.output)?,
                            V1Part::File {
                                id: id.clone().map_or_else(next_id, Ok)?,
                                session_id: session_id.to_string(),
                                message_id: message_id.to_string(),
                                mime: part_mime.clone(),
                                filename: filename.clone(),
                                url: url.to_string(),
                                source: source.clone(),
                            },
                        ]);
                    }
                }
            }

            // 939-970 — other mimes: inline base64 data URL.
            let bytes = std::fs::read(&filepath).map_err(|err| PromptError::Unknown {
                message: err.to_string(),
            })?;
            return Ok(vec![
                synthetic_text(
                    session_id,
                    message_id,
                    format!(
                        "Called the Read tool with the following input: {{\"filePath\":\"{}\"}}",
                        filepath.display()
                    ),
                )?,
                V1Part::File {
                    id: id.clone().map_or_else(next_id, Ok)?,
                    session_id: session_id.to_string(),
                    message_id: message_id.to_string(),
                    mime: part_mime.clone(),
                    filename: filename.clone(),
                    url: format!("data:{part_mime};base64,{}", base64_encode(&bytes)),
                    source: source.clone(),
                },
            ]);
        }
        _ => {}
    }

    // Passthrough (data: URLs with other mimes fall through here too).
    Ok(vec![V1Part::File {
        id: id.clone().map_or_else(next_id, Ok)?,
        session_id: session_id.to_string(),
        message_id: message_id.to_string(),
        mime: mime.to_string(),
        filename: filename.clone(),
        url: url.to_string(),
        source: source.clone(),
    }])
}

fn resolve_agent_part(
    input: &PromptInput,
    ag: &AgentInfo,
    message_id: &str,
    part: &PromptPartInput,
) -> Result<Vec<V1Part>, PromptError> {
    let PromptPartInput::Agent { id, name, source } = part else {
        unreachable!("checked by caller");
    };
    let perm = evaluate("task", name, &[&ag.permission]);
    let hint = if perm.action == opencode_schema::permission_v1::PermissionV1Action::Deny {
        " . Invoked by user; guaranteed to exist."
    } else {
        ""
    };
    Ok(vec![
        V1Part::Agent {
            id: id.clone().map_or_else(next_id, Ok)?,
            session_id: input.session_id.clone(),
            message_id: message_id.to_string(),
            name: name.clone(),
            source: source.clone(),
        },
        synthetic_text(
            &input.session_id,
            message_id,
            format!(
                " Use the above message and context to generate a prompt and call the task tool with subagent: {name}{hint}"
            ),
        )?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tool::def::Agents;
    use crate::tool::truncate::{Truncate, TruncateService};
    use serde::Deserialize;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    // ------------------------------------------------------------------
    // Fakes
    // ------------------------------------------------------------------

    fn model_info(provider_id: &str, model_id: &str) -> ModelInfo {
        ModelInfo {
            id: model_id.to_string(),
            provider_id: provider_id.to_string(),
            family: None,
            name: model_id.to_string(),
            api: opencode_schema::model::ModelApi::Aisdk {
                id: model_id.to_string(),
                package: "@ai-sdk/anthropic".to_string(),
                url: None,
                settings: None,
            },
            capabilities: opencode_schema::model::ModelCapabilities {
                tools: true,
                input: Vec::new(),
                output: Vec::new(),
            },
            request: opencode_schema::model::ModelRequest {
                headers: BTreeMap::new(),
                body: serde_json::Map::new(),
                variant: None,
            },
            variants: Vec::new(),
            time: opencode_schema::model::ModelTime { released: 0.0 },
            cost: Vec::new(),
            status: opencode_schema::model::ModelStatus::Active,
            enabled: true,
            limit: opencode_schema::model::ModelLimit {
                context: 1000,
                input: None,
                output: 100,
            },
        }
    }

    struct FixedModels;
    impl Models for FixedModels {
        fn get_model<'a>(
            &'a self,
            provider_id: &'a str,
            model_id: &'a str,
        ) -> crate::tool::def::BoxFuture<'a, Result<ModelInfo, CoreError>> {
            Box::pin(async move { Ok(model_info(provider_id, model_id)) })
        }
        fn default_model(
            &self,
        ) -> crate::tool::def::BoxFuture<'static, Result<ModelInfo, CoreError>> {
            Box::pin(async move { Ok(model_info("anthropic", "claude-sonnet-4-5")) })
        }
    }

    struct FakeMcp(Mutex<VecDeque<McpResourceItem>>);
    impl McpResources for FakeMcp {
        fn read_resource<'a>(
            &'a self,
            _client_name: &'a str,
            _uri: &'a str,
        ) -> crate::tool::def::BoxFuture<'a, Result<McpReadResource, String>> {
            Box::pin(async move {
                if let Some(item) = self.0.lock().unwrap().pop_front() {
                    Ok(McpReadResource::Contents(vec![item]))
                } else {
                    Err("connection refused".to_string())
                }
            })
        }
    }

    struct NotFoundMcp;
    impl McpResources for NotFoundMcp {
        fn read_resource<'a>(
            &'a self,
            _client_name: &'a str,
            _uri: &'a str,
        ) -> crate::tool::def::BoxFuture<'a, Result<McpReadResource, String>> {
            Box::pin(async move { Ok(McpReadResource::NotFound) })
        }
    }

    struct NoLsp;
    impl LspServer for NoLsp {
        fn has_clients<'a>(&'a self, _file: &'a str) -> crate::tool::def::BoxFuture<'a, bool> {
            Box::pin(async move { false })
        }
        fn touch_file<'a>(&'a self, _file: &'a str) -> crate::tool::def::BoxFuture<'a, ()> {
            Box::pin(async move {})
        }
        fn definition<'a>(
            &'a self,
            _position: crate::tool::lsp::Position,
        ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
            Box::pin(async move { Vec::new() })
        }
        fn references<'a>(
            &'a self,
            _position: crate::tool::lsp::Position,
        ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
            Box::pin(async move { Vec::new() })
        }
        fn hover<'a>(
            &'a self,
            _position: crate::tool::lsp::Position,
        ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
            Box::pin(async move { Vec::new() })
        }
        fn document_symbol<'a>(
            &'a self,
            _uri: &'a str,
        ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
            Box::pin(async move { Vec::new() })
        }
        fn workspace_symbol<'a>(
            &'a self,
            _query: &'a str,
        ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
            Box::pin(async move { Vec::new() })
        }
        fn implementation<'a>(
            &'a self,
            _position: crate::tool::lsp::Position,
        ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
            Box::pin(async move { Vec::new() })
        }
        fn prepare_call_hierarchy<'a>(
            &'a self,
            _position: crate::tool::lsp::Position,
        ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
            Box::pin(async move { Vec::new() })
        }
        fn incoming_calls<'a>(
            &'a self,
            _position: crate::tool::lsp::Position,
        ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
            Box::pin(async move { Vec::new() })
        }
        fn outgoing_calls<'a>(
            &'a self,
            _position: crate::tool::lsp::Position,
        ) -> crate::tool::def::BoxFuture<'a, Vec<Value>> {
            Box::pin(async move { Vec::new() })
        }
    }

    struct NoJobs;
    impl crate::session::run_state::BackgroundJobs for NoJobs {
        fn list(&self) -> Result<Vec<crate::session::run_state::BackgroundJobInfo>, CoreError> {
            Ok(Vec::new())
        }
        fn cancel(&self, _: &str) -> Result<(), CoreError> {
            Ok(())
        }
    }

    struct FixedClock;
    impl crate::Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            1_761_000_000_000
        }
    }

    struct WrapAgents;
    impl Agents for WrapAgents {
        fn get<'a>(
            &'a self,
            _agent: &'a str,
        ) -> crate::tool::def::BoxFuture<'a, Result<crate::tool::def::AgentInfo, ToolError>>
        {
            Box::pin(async move {
                Ok(crate::tool::def::AgentInfo {
                    name: "build".to_string(),
                    description: None,
                    mode: crate::tool::def::AgentMode::Primary,
                    permission: Vec::new(),
                })
            })
        }
        fn list<'a>(&'a self) -> crate::tool::def::BoxFuture<'a, Vec<crate::tool::def::AgentInfo>> {
            Box::pin(async move { Vec::new() })
        }
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    #[allow(dead_code)]
    struct ReadParams {
        file_path: String,
        offset: Option<f64>,
        limit: Option<f64>,
    }

    /// A fake read tool: echoes `filePath` (plus offset/limit when set) as
    /// its output, and records `extra.bypassCwdCheck` in `saw_bypass`.
    fn read_tool(dir: &Path, saw_bypass: Arc<AtomicBool>) -> ToolDef {
        let truncate: Arc<dyn Truncate> =
            Arc::new(TruncateService::new(dir.to_path_buf(), 10_000, 10_000));
        let agents: Arc<dyn Agents> = Arc::new(WrapAgents);
        crate::tool::def::define::<ReadParams, _>(
            "read",
            "description",
            json!({}),
            None,
            truncate,
            agents,
            move |args: ReadParams, ctx: ToolCtxRef<'_>| {
                let saw_bypass = Arc::clone(&saw_bypass);
                Box::pin(async move {
                    if ctx.extra.bypass_cwd_check {
                        saw_bypass.store(true, Ordering::SeqCst);
                    }
                    let mut output = args.file_path.clone();
                    if let Some(offset) = args.offset {
                        output = format!("{output}#offset={}", json_num_for_echo(offset));
                    }
                    if let Some(limit) = args.limit {
                        output = format!("{output}#limit={}", json_num_for_echo(limit));
                    }
                    Ok(ExecuteResult {
                        title: args.file_path,
                        metadata: json!({}),
                        output,
                        attachments: None,
                    })
                })
            },
        )
    }

    fn json_num_for_echo(value: f64) -> String {
        if value == value.trunc() {
            format!("{}", value as i64)
        } else {
            format!("{value}")
        }
    }

    // ------------------------------------------------------------------
    // Harness
    // ------------------------------------------------------------------

    struct Harness {
        services: crate::session::SessionServices,
        _temp: crate::storage::test_support::TempDir,
        worktree: PathBuf,
    }

    fn harness(name: &str) -> Harness {
        let temp = crate::storage::test_support::TempDir::new(name);
        let worktree = temp.path().join("repo");
        std::fs::create_dir_all(&worktree).unwrap();
        let services = crate::session::SessionServices::new(
            Arc::new(crate::storage::Storage::open(temp.path().join("db.sqlite")).unwrap()),
            Arc::new(NoJobs),
            Arc::new(FixedClock),
            &crate::session::agents::AgentRegistryInput::default(),
        );
        services
            .storage
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params!["global", "/repo", "[]", 1, 1],
                )
            })
            .unwrap();
        Harness {
            services,
            _temp: temp,
            worktree,
        }
    }

    fn session_id(h: &Harness) -> String {
        h.services
            .sessions
            .create(
                &crate::session::store::SessionContext {
                    project_id: "global".to_string(),
                    directory: h.worktree.clone(),
                    worktree: h.worktree.clone(),
                    workspace_id: None,
                },
                &Default::default(),
            )
            .unwrap()
            .id
    }

    fn deps<'a>(
        h: &'a Harness,
        read: &'a ToolDef,
        mcp: &'a dyn McpResources,
        lsp: &'a dyn LspServer,
    ) -> PromptDeps<'a> {
        PromptDeps {
            events: &h.services.events,
            sessions: &h.services.sessions,
            agents: &h.services.agents,
            models: &FixedModels,
            read,
            mcp,
            lsp,
            images: &NoResize,
            worktree: h.worktree.clone(),
            now_ms: 1_761_000_000_000,
        }
    }

    fn input(session_id: &str, parts: Vec<PromptPartInput>) -> PromptInput {
        PromptInput {
            session_id: session_id.to_string(),
            message_id: Some("msg_user".to_string()),
            model: Some(ModelRef {
                provider_id: "anthropic".to_string(),
                model_id: "claude-sonnet-4-5".to_string(),
            }),
            agent: Some("build".to_string()),
            tools: None,
            format: None,
            system: None,
            variant: None,
            parts,
        }
    }

    fn text(text: &str) -> PromptPartInput {
        PromptPartInput::Text {
            id: None,
            text: text.to_string(),
            synthetic: None,
            ignored: None,
            time: None,
            metadata: None,
        }
    }

    /// (type, text-or-url) tuples for assertion convenience.
    fn shape(parts: &[V1Part]) -> Vec<(&'static str, String)> {
        parts
            .iter()
            .map(|part| match part {
                V1Part::Text { text, .. } => ("text", text.clone()),
                V1Part::File { url, .. } => ("file", url.clone()),
                V1Part::Agent { name, .. } => ("agent", name.clone()),
                V1Part::Subtask { prompt, .. } => ("subtask", prompt.clone()),
                _ => ("other", String::new()),
            })
            .collect()
    }

    // ------------------------------------------------------------------
    // URL + JSON helpers
    // ------------------------------------------------------------------

    #[test]
    fn path_to_file_url_encodes() {
        assert_eq!(
            path_to_file_url(Path::new("/a b/c~d.md")),
            "file:///a%20b/c%7Ed.md"
        );
        assert_eq!(
            path_to_file_url(Path::new("/a[b]c/x")),
            "file:///a%5Bb%5Dc/x"
        );
        assert_eq!(path_to_file_url(Path::new("/x?y#z")), "file:///x%3Fy%23z");
        assert_eq!(
            path_to_file_url(Path::new("/résumé.md")),
            "file:///r%C3%A9sum%C3%A9.md"
        );
        assert_eq!(
            path_to_file_url(Path::new("/a+b=c;d,$&@:")),
            "file:///a+b=c;d,$&@:"
        );
    }

    #[test]
    fn file_url_to_path_decodes() {
        assert_eq!(
            file_url_to_path("file:///a%20b/c.md"),
            PathBuf::from("/a b/c.md")
        );
        assert_eq!(file_url_to_path("file:///x?start=3"), PathBuf::from("/x"));
    }

    #[test]
    fn decode_data_url_branches() {
        assert_eq!(decode_data_url("no-comma"), "");
        assert_eq!(
            decode_data_url("data:text/plain,hello%20world"),
            "hello world"
        );
        assert_eq!(decode_data_url("data:text/plain;base64,aGVsbG8="), "hello");
    }

    #[test]
    fn base64_round_trip() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_decode("aGVsbG8="), b"hello");
        assert_eq!(base64_decode("aGVs bG8="), b"hello");
    }

    #[test]
    fn json_string_escapes() {
        assert_eq!(json_string("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
        assert_eq!(json_string("\u{1}"), "\"\\u0001\"");
    }

    // ------------------------------------------------------------------
    // FILE_REGEX scanner
    // ------------------------------------------------------------------

    #[test]
    fn file_matches_basic() {
        assert_eq!(file_matches("@foo"), vec!["foo"]);
        assert_eq!(file_matches("hello @world"), vec!["world"]);
        assert_eq!(file_matches("@a.b.c"), vec!["a.b.c"]);
        assert_eq!(file_matches("@.hidden"), vec![".hidden"]);
        assert_eq!(file_matches("@x."), vec!["x"]);
        assert_eq!(file_matches("@"), vec![""]);
        assert_eq!(file_matches("no mention"), Vec::<String>::new());
    }

    #[test]
    fn file_matches_lookbehind() {
        // Word chars and backtick before @ suppress the match.
        assert_eq!(file_matches("a@foo"), Vec::<String>::new());
        assert_eq!(file_matches("1@foo"), Vec::<String>::new());
        assert_eq!(file_matches("_@foo"), Vec::<String>::new());
        assert_eq!(file_matches("`@foo"), Vec::<String>::new());
        assert_eq!(file_matches("/@foo"), vec!["foo"]);
    }

    #[test]
    fn file_matches_dots() {
        // A trailing dot without a following run is not consumed.
        assert_eq!(file_matches("@a..b"), vec!["a"]);
        assert_eq!(file_matches("@a."), vec!["a"]);
        // "t@example" is blocked by the lookbehind, no "e" mail-style match.
        assert_eq!(file_matches("email test@example.com"), Vec::<String>::new());
    }

    // ------------------------------------------------------------------
    // resolvePromptParts
    // ------------------------------------------------------------------

    #[test]
    fn resolve_prompt_parts_files_and_agents() {
        let temp = crate::storage::test_support::TempDir::new("prompt-resolve-parts");
        let worktree = temp.path().join("repo");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(worktree.join("src/lib.rs"), "x").unwrap();
        let registry = crate::session::agents::AgentRegistry::new(
            &crate::session::agents::AgentRegistryInput::default(),
        );
        let parts = resolve_prompt_parts(
            &registry,
            &worktree,
            "hello @src/lib.rs and @plan and @src/lib.rs",
        );
        assert_eq!(parts.len(), 3);
        assert_eq!(
            parts[0],
            PromptPartInput::Text {
                id: None,
                text: "hello @src/lib.rs and @plan and @src/lib.rs".to_string(),
                synthetic: None,
                ignored: None,
                time: None,
                metadata: None,
            }
        );
        // The existing file becomes a file part.
        match &parts[1] {
            PromptPartInput::File {
                mime,
                filename,
                url,
                ..
            } => {
                assert_eq!(mime, "text/plain");
                assert_eq!(filename.as_deref(), Some("src/lib.rs"));
                assert_eq!(url, &path_to_file_url(&worktree.join("src/lib.rs")));
            }
            other => panic!("expected file part, got {other:?}"),
        }
        // "plan" is not a file, but it is an agent.
        match &parts[2] {
            PromptPartInput::Agent { name, .. } => assert_eq!(name, "plan"),
            other => panic!("expected agent part, got {other:?}"),
        }
    }

    #[test]
    fn resolve_prompt_parts_directories() {
        let temp = crate::storage::test_support::TempDir::new("prompt-resolve-dirs");
        let worktree = temp.path().join("repo");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        let registry = crate::session::agents::AgentRegistry::new(
            &crate::session::agents::AgentRegistryInput::default(),
        );
        let parts = resolve_prompt_parts(&registry, &worktree, "@src");
        assert_eq!(parts.len(), 2);
        match &parts[1] {
            PromptPartInput::File { mime, .. } => {
                assert_eq!(mime, "application/x-directory");
            }
            other => panic!("expected file part, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // createUserMessage
    // ------------------------------------------------------------------

    fn no_lsp() -> NoLsp {
        NoLsp
    }

    fn mcp_empty() -> FakeMcp {
        FakeMcp(Mutex::new(std::collections::VecDeque::new()))
    }

    #[tokio::test]
    async fn create_user_message_agent_not_found() {
        let h = harness("prompt-agent-not-found");
        let read = read_tool(&h.worktree, Arc::new(AtomicBool::new(false)));
        let mcp = mcp_empty();
        let lsp = no_lsp();
        let deps = deps(&h, &read, &mcp, &lsp);
        let mut input = input(&session_id(&h), vec![text("hi")]);
        input.agent = Some("nope".to_string());
        let err = create_user_message(&deps, &input).await.unwrap_err();
        match err {
            PromptError::Unknown { message } => {
                assert!(message.starts_with("Agent not found: \"nope\". Available agents:"));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_user_message_persists_and_syncs_session() {
        let h = harness("prompt-persist");
        let read = read_tool(&h.worktree, Arc::new(AtomicBool::new(false)));
        let mcp = mcp_empty();
        let lsp = no_lsp();
        let deps = deps(&h, &read, &mcp, &lsp);
        let session_id = session_id(&h);
        let out = create_user_message(&deps, &input(&session_id, vec![text("hello")]))
            .await
            .unwrap();
        assert_eq!(shape(&out.parts), vec![("text", "hello".to_string())]);

        // setAgentModel synced the session (agent + model + variant default).
        let session = h.services.sessions.get(&session_id).unwrap();
        assert_eq!(session.agent.as_deref(), Some("build"));
        let model = session.model.expect("session model");
        assert_eq!(model.id, "claude-sonnet-4-5");
        assert_eq!(model.provider_id, "anthropic");
        assert_eq!(model.variant.as_deref(), Some("default"));

        // The message and part were persisted.
        let message = h
            .services
            .sessions
            .find_message(&session_id, &|_| true)
            .unwrap()
            .expect("message persisted");
        let V1Message::User { model, .. } = &message.info else {
            panic!("user message");
        };
        assert_eq!(model.provider_id, "anthropic");
        assert_eq!(model.model_id, "claude-sonnet-4-5");
        let V1Part::Text { text, .. } = &message.parts[0] else {
            panic!("text part");
        };
        assert_eq!(text, "hello");
    }

    #[tokio::test]
    async fn create_user_message_file_url_text_plain() {
        let h = harness("prompt-file-text");
        std::fs::write(h.worktree.join("notes.md"), "contents").unwrap();
        let saw_bypass = Arc::new(AtomicBool::new(false));
        let read = read_tool(&h.worktree, Arc::clone(&saw_bypass));
        let mcp = mcp_empty();
        let lsp = no_lsp();
        let deps = deps(&h, &read, &mcp, &lsp);
        let session_id = session_id(&h);
        let part = PromptPartInput::File {
            id: None,
            mime: "text/plain".to_string(),
            filename: Some("notes.md".to_string()),
            url: path_to_file_url(&h.worktree.join("notes.md")),
            source: None,
        };
        let out = create_user_message(&deps, &input(&session_id, vec![part]))
            .await
            .unwrap();
        let expected_path = h.worktree.join("notes.md").display().to_string();
        assert_eq!(
            shape(&out.parts),
            vec![
                (
                    "text",
                    format!(
                        "Called the Read tool with the following input: {{\"filePath\":\"{expected_path}\"}}"
                    ),
                ),
                ("text", expected_path.clone()),
                ("file", path_to_file_url(&h.worktree.join("notes.md"))),
            ]
        );
        assert!(saw_bypass.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn create_user_message_file_url_range_params() {
        let h = harness("prompt-file-range");
        std::fs::write(h.worktree.join("notes.md"), "contents").unwrap();
        let read = read_tool(&h.worktree, Arc::new(AtomicBool::new(false)));
        let mcp = mcp_empty();
        let lsp = no_lsp();
        let deps = deps(&h, &read, &mcp, &lsp);
        let session_id = session_id(&h);
        let part = PromptPartInput::File {
            id: None,
            mime: "text/plain".to_string(),
            filename: Some("notes.md".to_string()),
            url: format!(
                "{}?start=3&end=5",
                path_to_file_url(&h.worktree.join("notes.md"))
            ),
            source: None,
        };
        let out = create_user_message(&deps, &input(&session_id, vec![part]))
            .await
            .unwrap();
        let expected_path = h.worktree.join("notes.md").display().to_string();
        assert_eq!(
            shape(&out.parts),
            vec![
                (
                    "text",
                    format!(
                        "Called the Read tool with the following input: {{\"filePath\":\"{expected_path}\",\"offset\":3,\"limit\":3}}",
                    ),
                ),
                ("text", format!("{expected_path}#offset=3#limit=3")),
                (
                    "file",
                    format!(
                        "{}?start=3&end=5",
                        path_to_file_url(&h.worktree.join("notes.md"))
                    ),
                ),
            ]
        );
    }

    #[tokio::test]
    async fn create_user_message_data_url_text_plain() {
        let h = harness("prompt-data-text");
        let read = read_tool(&h.worktree, Arc::new(AtomicBool::new(false)));
        let mcp = mcp_empty();
        let lsp = no_lsp();
        let deps = deps(&h, &read, &mcp, &lsp);
        let session_id = session_id(&h);
        let part = PromptPartInput::File {
            id: None,
            mime: "text/plain".to_string(),
            filename: Some("a.txt".to_string()),
            url: "data:text/plain;base64,aGVsbG8=".to_string(),
            source: None,
        };
        let out = create_user_message(&deps, &input(&session_id, vec![part]))
            .await
            .unwrap();
        assert_eq!(
            shape(&out.parts),
            vec![
                (
                    "text",
                    "Called the Read tool with the following input: {\"filePath\":\"a.txt\"}"
                        .to_string(),
                ),
                ("text", "hello".to_string()),
                ("file", "data:text/plain;base64,aGVsbG8=".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn create_user_message_data_url_other_mime_passthrough() {
        let h = harness("prompt-data-image");
        let read = read_tool(&h.worktree, Arc::new(AtomicBool::new(false)));
        let mcp = mcp_empty();
        let lsp = no_lsp();
        let deps = deps(&h, &read, &mcp, &lsp);
        let session_id = session_id(&h);
        let part = PromptPartInput::File {
            id: None,
            mime: "image/png".to_string(),
            filename: Some("a.png".to_string()),
            url: "data:image/png;base64,aGVsbG8=".to_string(),
            source: None,
        };
        let out = create_user_message(&deps, &input(&session_id, vec![part]))
            .await
            .unwrap();
        // Non-text data: URLs fall through to the passthrough — and the
        // NoResize image seam keeps them unchanged.
        assert_eq!(
            shape(&out.parts),
            vec![("file", "data:image/png;base64,aGVsbG8=".to_string())]
        );
    }

    #[tokio::test]
    async fn create_user_message_file_url_directory() {
        let h = harness("prompt-file-dir");
        let read = read_tool(&h.worktree, Arc::new(AtomicBool::new(false)));
        let mcp = mcp_empty();
        let lsp = no_lsp();
        let deps = deps(&h, &read, &mcp, &lsp);
        let session_id = session_id(&h);
        let part = PromptPartInput::File {
            id: None,
            mime: "text/plain".to_string(),
            filename: Some("repo".to_string()),
            url: path_to_file_url(&h.worktree),
            source: None,
        };
        let out = create_user_message(&deps, &input(&session_id, vec![part]))
            .await
            .unwrap();
        let expected_path = h.worktree.display().to_string();
        assert_eq!(
            shape(&out.parts),
            vec![
                (
                    "text",
                    format!(
                        "Called the Read tool with the following input: {{\"filePath\":\"{expected_path}\"}}"
                    ),
                ),
                ("text", expected_path),
                ("file", path_to_file_url(&h.worktree)),
            ]
        );
    }

    #[tokio::test]
    async fn create_user_message_file_url_other_mime_base64() {
        let h = harness("prompt-file-binary");
        std::fs::write(h.worktree.join("blob.bin"), [0u8, 1, 2]).unwrap();
        let read = read_tool(&h.worktree, Arc::new(AtomicBool::new(false)));
        let mcp = mcp_empty();
        let lsp = no_lsp();
        let deps = deps(&h, &read, &mcp, &lsp);
        let session_id = session_id(&h);
        let part = PromptPartInput::File {
            id: None,
            mime: "application/octet-stream".to_string(),
            filename: Some("blob.bin".to_string()),
            url: path_to_file_url(&h.worktree.join("blob.bin")),
            source: None,
        };
        let out = create_user_message(&deps, &input(&session_id, vec![part]))
            .await
            .unwrap();
        let expected_path = h.worktree.join("blob.bin").display().to_string();
        assert_eq!(
            shape(&out.parts),
            vec![
                (
                    "text",
                    // Template literal — filepath is not JSON-escaped here.
                    format!("Called the Read tool with the following input: {{\"filePath\":\"{expected_path}\"}}"),
                ),
                (
                    "file",
                    format!(
                        "data:application/octet-stream;base64,{}",
                        base64_encode(&[0u8, 1, 2])
                    ),
                ),
            ]
        );
    }

    #[tokio::test]
    async fn create_user_message_agent_part() {
        let h = harness("prompt-agent-part");
        let read = read_tool(&h.worktree, Arc::new(AtomicBool::new(false)));
        let mcp = mcp_empty();
        let lsp = no_lsp();
        let deps = deps(&h, &read, &mcp, &lsp);
        let session_id = session_id(&h);
        let part = PromptPartInput::Agent {
            id: None,
            name: "plan".to_string(),
            source: None,
        };
        let out = create_user_message(&deps, &input(&session_id, vec![part]))
            .await
            .unwrap();
        assert_eq!(
            shape(&out.parts),
            vec![
                ("agent", "plan".to_string()),
                (
                    "text",
                    " Use the above message and context to generate a prompt and call the task tool with subagent: plan"
                        .to_string(),
                ),
            ]
        );
    }

    #[tokio::test]
    async fn create_user_message_mcp_resource_text() {
        let h = harness("prompt-mcp-text");
        let read = read_tool(&h.worktree, Arc::new(AtomicBool::new(false)));
        let mcp = FakeMcp(Mutex::new(std::collections::VecDeque::from(vec![
            McpResourceItem {
                text: Some("resource body".to_string()),
                blob: None,
                mime_type: None,
                uri: None,
            },
        ])));
        let lsp = no_lsp();
        let deps = deps(&h, &read, &mcp, &lsp);
        let session_id = session_id(&h);
        let part = PromptPartInput::File {
            id: None,
            mime: "text/plain".to_string(),
            filename: Some("docs".to_string()),
            url: "resource://docs".to_string(),
            source: Some(V1FilePartSource::Resource {
                text: opencode_schema::session_v1::V1FilePartSourceText {
                    value: String::new(),
                    start: 0.0,
                    end: 0.0,
                },
                client_name: "server".to_string(),
                uri: "res://docs".to_string(),
            }),
        };
        let out = create_user_message(&deps, &input(&session_id, vec![part]))
            .await
            .unwrap();
        assert_eq!(
            shape(&out.parts),
            vec![
                (
                    "text",
                    "Reading MCP resource: docs (res://docs)".to_string()
                ),
                ("text", "resource body".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn create_user_message_mcp_resource_blob() {
        let h = harness("prompt-mcp-blob");
        let read = read_tool(&h.worktree, Arc::new(AtomicBool::new(false)));
        // jpeg blob: supported + attached.
        let mcp = FakeMcp(Mutex::new(std::collections::VecDeque::from(vec![
            McpResourceItem {
                text: None,
                blob: Some("aGVsbG8=".to_string()),
                mime_type: Some("image/jpeg".to_string()),
                uri: Some("res://img".to_string()),
            },
        ])));
        let lsp = no_lsp();
        let deps = deps(&h, &read, &mcp, &lsp);
        let session_id = session_id(&h);
        let part = PromptPartInput::File {
            id: None,
            mime: "text/plain".to_string(),
            filename: Some("img".to_string()),
            url: "resource://img".to_string(),
            source: Some(V1FilePartSource::Resource {
                text: opencode_schema::session_v1::V1FilePartSourceText {
                    value: String::new(),
                    start: 0.0,
                    end: 0.0,
                },
                client_name: "server".to_string(),
                uri: "res://img".to_string(),
            }),
        };
        let out = create_user_message(&deps, &input(&session_id, vec![part]))
            .await
            .unwrap();
        assert_eq!(
            shape(&out.parts),
            vec![
                ("text", "Reading MCP resource: img (res://img)".to_string()),
                (
                    "text",
                    "[Binary MCP resource attached: res://img (image/jpeg)]".to_string(),
                ),
                ("file", "data:image/jpeg;base64,aGVsbG8=".to_string(),),
            ]
        );
    }

    #[tokio::test]
    async fn create_user_message_mcp_resource_blob_unsupported_mime() {
        let h = harness("prompt-mcp-unsupported");
        let read = read_tool(&h.worktree, Arc::new(AtomicBool::new(false)));
        let mcp = FakeMcp(Mutex::new(std::collections::VecDeque::from(vec![
            McpResourceItem {
                text: None,
                blob: Some("aGVsbG8=".to_string()),
                mime_type: Some("text/csv".to_string()),
                uri: Some("res://csv".to_string()),
            },
        ])));
        let lsp = no_lsp();
        let deps = deps(&h, &read, &mcp, &lsp);
        let session_id = session_id(&h);
        let part = PromptPartInput::File {
            id: None,
            mime: "text/plain".to_string(),
            filename: Some("csv".to_string()),
            url: "resource://csv".to_string(),
            source: Some(V1FilePartSource::Resource {
                text: opencode_schema::session_v1::V1FilePartSourceText {
                    value: String::new(),
                    start: 0.0,
                    end: 0.0,
                },
                client_name: "server".to_string(),
                uri: "res://csv".to_string(),
            }),
        };
        let out = create_user_message(&deps, &input(&session_id, vec![part]))
            .await
            .unwrap();
        assert_eq!(
            shape(&out.parts),
            vec![
                ("text", "Reading MCP resource: csv (res://csv)".to_string()),
                (
                    "text",
                    "[Binary MCP resource omitted: res://csv (text/csv, 5 B) is not a supported attachment type]"
                        .to_string(),
                ),
            ]
        );
    }

    #[tokio::test]
    async fn create_user_message_mcp_resource_not_found() {
        let h = harness("prompt-mcp-not-found");
        let read = read_tool(&h.worktree, Arc::new(AtomicBool::new(false)));
        let mcp = NotFoundMcp;
        let lsp = no_lsp();
        let deps = deps(&h, &read, &mcp, &lsp);
        let session_id = session_id(&h);
        let part = PromptPartInput::File {
            id: None,
            mime: "text/plain".to_string(),
            filename: Some("docs".to_string()),
            url: "resource://docs".to_string(),
            source: Some(V1FilePartSource::Resource {
                text: opencode_schema::session_v1::V1FilePartSourceText {
                    value: String::new(),
                    start: 0.0,
                    end: 0.0,
                },
                client_name: "server".to_string(),
                uri: "res://docs".to_string(),
            }),
        };
        let err = create_user_message(&deps, &input(&session_id, vec![part]))
            .await
            .unwrap_err();
        match err {
            PromptError::Unknown { message } => {
                assert_eq!(message, "Resource not found: server/res://docs");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn mcp_blob_size_math() {
        // "aGVsbG8=" — 8 chars, 1 padding -> 3 bytes.
        assert_eq!(mcp_resource_base64_size("aGVsbG8="), 5); // "hello"
        assert_eq!(mcp_resource_base64_size("aGVsbG8="), 5); // "hello"
        assert_eq!(mcp_resource_base64_size("aGln"), 3);
        assert_eq!(format_mcp_resource_bytes(5), "5 B");
        assert_eq!(format_mcp_resource_bytes(2048), "2 KB");
        assert_eq!(format_mcp_resource_bytes(3 * 1024 * 1024), "3 MB");
    }
}
