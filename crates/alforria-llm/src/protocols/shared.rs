//! Provider-shared helpers (TS `protocols/shared.ts`).
//!
//! Cross-protocol utilities every route needs: usage arithmetic, error
//! constructors, text helpers, tool-choice dispatch, media validation, and
//! SSE framing.
//!
//! Not ported (Effect/JS machinery with no Rust counterpart, or covered by
//! plain serde):
//! - `Json`/`decodeJson`/`encodeJson`/`isJson`/`JsonObject`/`optionalArray`/
//!   `optionalNull` — `serde_json` accepts/produces arbitrary JSON directly.
//! - `errorText` — the Rust error channel is typed (`LlmError`), so there is
//!   no `unknown` stream error to stringify.
//! - `validateWith`/`jsonPost` — lowering constructs typed bodies directly
//!   (spec §2.6) and HTTP posts are built by the executor.

#![allow(clippy::result_large_err)]

use std::sync::LazyLock;

use eventsource_stream::{EventStreamError, Eventsource};
use futures::StreamExt;
use regex::Regex;

use alforria_schema::llm::ToolContent;

use crate::schema::errors::{LlmError, LlmErrorReason};
use crate::schema::ids::MessageRole;
use crate::schema::messages::{ContentPart, Message, ToolChoice, ToolChoiceType, ToolResultValue};

/// `Usage.totalTokens` policy shared by every route. Honors a provider-
/// supplied total; otherwise falls back to `inputTokens + outputTokens` only
/// when at least one is defined. Returns `None` when neither input nor
/// output is known so routes don't publish a misleading `0`.
///
/// Under the additive `LLM.Usage` contract, `input_tokens` and
/// `output_tokens` are the non-cached input and visible output only. The
/// provider-supplied `total` is the source of truth when present; the
/// computed fallback under-counts cache and reasoning by design and exists
/// mainly so Anthropic-style providers (which don't surface a total) still
/// get a sensible aggregate on the input + output axes.
pub fn total_tokens(
    input_tokens: Option<f64>,
    output_tokens: Option<f64>,
    total: Option<f64>,
) -> Option<f64> {
    if total.is_some() {
        return total;
    }
    if input_tokens.is_none() && output_tokens.is_none() {
        return None;
    }
    Some(input_tokens.unwrap_or(0.0) + output_tokens.unwrap_or(0.0))
}

/// Subtract `subtrahend` from `total`, clamping to zero if the provider
/// reports a non-sensical breakdown (e.g. `cached_tokens > prompt_tokens`).
/// Used by protocol mappers when deriving a non-overlapping breakdown field
/// from a provider's inclusive total — `non_cached_input_tokens` from
/// `input_tokens - cache_read_input_tokens - cache_write_input_tokens`.
///
/// If `total` is `None`, returns `None` (we don't fabricate counts). If
/// `subtrahend` is `None`, returns `total` unchanged. The provider-native
/// breakdown stays available on `Usage.provider_metadata` for debugging.
pub fn subtract_tokens(total: Option<f64>, subtrahend: Option<f64>) -> Option<f64> {
    let total = total?;
    match subtrahend {
        Some(subtrahend) => Some((total - subtrahend).max(0.0)),
        None => Some(total),
    }
}

/// Sum a list of optional token counts, returning `None` only when every
/// value is `None` (so we don't fabricate a `0`). Used by protocol mappers
/// to derive the inclusive `input_tokens` total from a provider that
/// natively reports a non-overlapping breakdown (e.g. Anthropic, whose
/// `input_tokens` is already non-cached only).
pub fn sum_tokens(values: &[Option<f64>]) -> Option<f64> {
    if values.iter().all(Option::is_none) {
        return None;
    }
    Some(values.iter().map(|value| value.unwrap_or(0.0)).sum())
}

/// Streaming error constructor (TS `eventError`): an
/// `InvalidProviderOutput` failure on the `ProviderShared.stream` method.
pub fn event_error(route: &str, message: impl Into<String>, raw: Option<&str>) -> LlmError {
    LlmError {
        module: "ProviderShared".to_string(),
        method: "stream".to_string(),
        reason: LlmErrorReason::InvalidProviderOutput {
            message: message.into(),
            route: Some(route.to_string()),
            raw: raw.map(str::to_string),
            provider_metadata: None,
        },
    }
}

/// Parse a JSON string, mapping any decode failure onto `event_error` with
/// the given uniform `message` (TS `parseJson`).
pub fn parse_json(route: &str, input: &str, message: &str) -> Result<serde_json::Value, LlmError> {
    serde_json::from_str(input).map_err(|_| event_error(route, message, Some(input)))
}

/// Join the `text` field of a list of parts with newlines. Used by routes
/// that flatten system / message content arrays into a single provider string
/// (OpenAI Chat `system` content, OpenAI Responses `system` content, Gemini
/// `systemInstruction.parts[].text`). Rust takes an iterator of texts because
/// the TS `{ text: string }` structural type has no direct equivalent.
pub fn join_text<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for (index, part) in parts.into_iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(part);
    }
    out
}

fn escape_system_update_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Stable fallback representation for chronological `Message.system(...)`
/// updates on routes that do not support that privileged role natively. The
/// wrapper remains visibly lower-authority user text, preserves the original
/// temporal position, and XML-escapes content so it cannot close the wrapper.
pub fn wrap_system_update<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    format!(
        "<system-update>\n{}\n</system-update>",
        escape_system_update_text(&join_text(parts))
    )
}

/// Chronological system updates deliberately accept text only. Do not insert
/// raw retrieved, tool, or web content into privileged updates: keep
/// untrusted data in ordinary user/tool messages instead.
pub fn system_update_text(route: &str, message: &Message) -> Result<Vec<ContentPart>, LlmError> {
    let mut content: Vec<ContentPart> = Vec::new();
    for part in &message.content {
        if !supports_content(part, &["text"]) {
            return Err(unsupported_content(route, &message.role, &["text"]));
        }
        content.push(part.clone());
    }
    Ok(content)
}

/// Lower an unsupported privileged update into visible, in-order user text.
pub fn wrapped_system_update(route: &str, message: &Message) -> Result<ContentPart, LlmError> {
    let content = system_update_text(route, message)?;
    let cache = content.last().and_then(|part| match part {
        ContentPart::Text { cache, .. } => cache.clone(),
        _ => None,
    });
    Ok(ContentPart::Text {
        text: wrap_system_update(content.iter().map(|part| match part {
            ContentPart::Text { text, .. } => text.as_str(),
            _ => "",
        })),
        cache,
        metadata: None,
        provider_metadata: None,
    })
}

/// Parse the streamed JSON input of a tool call. Treats an empty string as
/// `"{}"` — providers occasionally finish a tool call without ever emitting
/// input deltas (e.g. zero-arg tools). The error message is uniform across
/// routes: `Invalid JSON input for <route> tool call <name>`.
pub fn parse_tool_input(route: &str, name: &str, raw: &str) -> Result<serde_json::Value, LlmError> {
    parse_json(
        route,
        if raw.is_empty() { "{}" } else { raw },
        &format!("Invalid JSON input for {route} tool call {name}"),
    )
}

pub const IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
pub const VIDEO_MIMES: [&str; 3] = ["video/mp4", "video/webm", "video/quicktime"];
pub const AUDIO_MIMES: [&str; 6] = [
    "audio/wav",
    "audio/mp3",
    "audio/aiff",
    "audio/aac",
    "audio/ogg",
    "audio/flac",
];
pub const MEDIA_MIMES: [&str; 13] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "video/mp4",
    "video/webm",
    "video/quicktime",
    "audio/wav",
    "audio/mp3",
    "audio/aiff",
    "audio/aac",
    "audio/ogg",
    "audio/flac",
];
pub const MAX_MEDIA_ENCODED_BYTES: usize = 28 * 1024 * 1024;
pub const MAX_MEDIA_DECODED_BYTES: usize = 20 * 1024 * 1024;

static BASE64_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$")
        .expect("valid base64 pattern")
});

static DATA_URL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^data:([^;,]+);base64,([A-Za-z0-9+/]*={0,2})$").expect("valid data URL pattern")
});

/// The validated media every route lowering needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedMedia {
    pub mime: String,
    pub base64: String,
    pub data_url: String,
    pub bytes: Vec<u8>,
}

/// Decode a media part for a provider body: check the MIME against the
/// route's supported set, accept raw base64 or a `data:` URL, enforce the
/// 28MB encoded / 20MB decoded limits, and verify canonical base64.
///
/// The TS Uint8Array input branch does not port: `MediaData` in this crate
/// is always a base64 string.
pub fn validate_media(
    route: &str,
    part: &ContentPart,
    supported_mimes: &[&str],
) -> Result<ValidatedMedia, LlmError> {
    let ContentPart::Media {
        media_type, data, ..
    } = part
    else {
        return Err(invalid_request(
            "media validation requires a media content part",
        ));
    };
    let mime = media_type.to_lowercase();
    if !supported_mimes.contains(&mime.as_str()) {
        return Err(invalid_request(format!(
            "{route} does not support media type {media_type}"
        )));
    }

    let base64 = if let Some(_prefix) = data.strip_prefix("data:") {
        let Some(captures) = DATA_URL_PATTERN.captures(data) else {
            return Err(invalid_request(format!(
                "{route} media data URL must contain valid base64"
            )));
        };
        let url_mime = captures.get(1).unwrap().as_str();
        if url_mime.to_lowercase() != mime {
            return Err(invalid_request(format!(
                "{route} media type {media_type} does not match data URL type {url_mime}"
            )));
        }
        captures.get(2).unwrap().as_str().to_string()
    } else {
        data.clone()
    };

    if base64.len() > MAX_MEDIA_ENCODED_BYTES {
        return Err(invalid_request(format!(
            "{route} media exceeds the {MAX_MEDIA_ENCODED_BYTES} byte encoded limit"
        )));
    }
    if base64.is_empty() || base64.len() % 4 != 0 || !BASE64_PATTERN.is_match(&base64) {
        return Err(invalid_request(format!(
            "{route} media must contain valid base64"
        )));
    }
    let Some(bytes) = base64_decode(&base64) else {
        return Err(invalid_request(format!(
            "{route} media must contain valid base64"
        )));
    };
    if bytes.len() > MAX_MEDIA_DECODED_BYTES {
        return Err(invalid_request(format!(
            "{route} media exceeds the {MAX_MEDIA_DECODED_BYTES} byte decoded limit"
        )));
    }
    if base64_encode(&bytes) != base64 {
        return Err(invalid_request(format!(
            "{route} media must contain canonical base64"
        )));
    }
    Ok(ValidatedMedia {
        mime: mime.clone(),
        data_url: format!("data:{mime};base64,{base64}"),
        base64,
        bytes,
    })
}

/// Validate a file-shaped tool result content part
/// (`ToolFileContent` in TS) as media.
pub fn validate_tool_file(
    route: &str,
    content: &ToolContent,
    supported_mimes: &[&str],
) -> Result<ValidatedMedia, LlmError> {
    let ToolContent::File { uri, mime, .. } = content else {
        return Err(invalid_request(
            "tool file validation requires a file content part",
        ));
    };
    validate_media(route, &ContentPart::media(mime, uri), supported_mimes)
}

pub fn trim_base_url(value: &str) -> String {
    value.trim_end_matches('/').to_string()
}

/// Stringify a tool result part for provider bodies
/// (TS `toolResultText`): text results pass through, error results keep
/// structured values as JSON, and everything else is JSON-encoded.
pub fn tool_result_text(part: &ContentPart) -> String {
    let ContentPart::ToolResult { result, .. } = part else {
        return String::new();
    };
    match result {
        ToolResultValue::Text { value } => js_string(value),
        ToolResultValue::Error { value } => {
            let structured = value.is_array() || value.is_object();
            if structured {
                value.to_string()
            } else {
                js_string(value)
            }
        }
        ToolResultValue::Json { value } => value.to_string(),
        ToolResultValue::Content { value } => serde_json::to_string(value).unwrap_or_default(),
    }
}

/// `String(value)` for a JSON value: strings pass through unquoted,
/// everything else uses its JSON text form.
fn js_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Canonical invalid-request constructor. Lift one-line
/// `const invalid = (message) => invalidRequest(message)` aliases out of
/// every route so the error constructor lives in one place. If we ever
/// extend `InvalidRequestReason` with route context or trace metadata, the
/// change lands here.
pub fn invalid_request(message: impl Into<String>) -> LlmError {
    LlmError::invalid(message)
}

/// Which arm of the tool-choice union a request selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchedToolChoice {
    Auto,
    None,
    Required,
    Tool(String),
}

/// Dispatch on a `ToolChoice` (TS `matchToolChoice`). Selecting a specific
/// tool requires a non-empty name; the error message is uniform across
/// routes: `<route> tool choice requires a tool name`.
pub fn match_tool_choice(
    tool_choice: &ToolChoice,
    route: &str,
) -> Result<MatchedToolChoice, LlmError> {
    match tool_choice.r#type {
        ToolChoiceType::Auto => Ok(MatchedToolChoice::Auto),
        ToolChoiceType::None => Ok(MatchedToolChoice::None),
        ToolChoiceType::Required => Ok(MatchedToolChoice::Required),
        ToolChoiceType::Tool => {
            let Some(name) = tool_choice.name.as_deref().filter(|name| !name.is_empty()) else {
                return Err(invalid_request(format!(
                    "{route} tool choice requires a tool name"
                )));
            };
            Ok(MatchedToolChoice::Tool(name.to_string()))
        }
    }
}

/// The neutral-schema wire tag of a content part: `"text"`, `"media"`,
/// `"tool-call"`, `"tool-result"`, or `"reasoning"`.
pub fn content_type(part: &ContentPart) -> &'static str {
    match part {
        ContentPart::Text { .. } => "text",
        ContentPart::Media { .. } => "media",
        ContentPart::ToolCall { .. } => "tool-call",
        ContentPart::ToolResult { .. } => "tool-result",
        ContentPart::Reasoning { .. } => "reasoning",
    }
}

/// TS `supportsContent` — is this content part one of the given wire tags?
pub fn supports_content(part: &ContentPart, types: &[&str]) -> bool {
    types.contains(&content_type(part))
}

fn format_content_types(types: &[&str]) -> String {
    match types {
        [] => String::new(),
        [only] => (*only).to_string(),
        [first, second] => format!("{first} and {second}"),
        _ => {
            let last = types[types.len() - 1];
            format!("{}, and {}", types[..types.len() - 1].join(", "), last)
        }
    }
}

/// The uniform unsupported-content error
/// (`<route> <role> messages only support <types> content for now`).
pub fn unsupported_content(route: &str, role: &MessageRole, types: &[&str]) -> LlmError {
    invalid_request(format!(
        "{route} {} messages only support {} content for now",
        role_name(*role),
        format_content_types(types)
    ))
}

fn role_name(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

/// `framing` step for Server-Sent Events. Decodes UTF-8, runs the SSE
/// channel decoder, and drops empty / `[DONE]` keep-alive events so the
/// downstream `decode_chunk` sees one JSON string per element. The SSE
/// channel's `Retry` control event does not port — the Rust eventsource
/// decoder surfaces retry as a field, which is naturally ignored here.
pub fn sse_framing<S, B>(bytes: S) -> impl futures::Stream<Item = Result<String, LlmError>>
where
    S: futures::Stream<Item = Result<B, LlmError>>,
    B: AsRef<[u8]>,
{
    bytes.eventsource().filter_map(|event| async move {
        match event {
            Ok(event) if !event.data.is_empty() && event.data != "[DONE]" => Some(Ok(event.data)),
            Ok(_) => None,
            Err(EventStreamError::Transport(error)) => Some(Err(error)),
            Err(error) => Some(Err(LlmError::invalid(format!(
                "invalid event stream: {error:?}"
            )))),
        }
    })
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_decode_char(byte: u8) -> Option<u32> {
    match byte {
        b'A'..=b'Z' => Some((byte - b'A') as u32),
        b'a'..=b'z' => Some((byte - b'a') as u32 + 26),
        b'0'..=b'9' => Some((byte - b'0') as u32 + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    if !input.len().is_multiple_of(4) {
        return None;
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let a = base64_decode_char(chunk[0])?;
        let b = base64_decode_char(chunk[1])?;
        if chunk[2] == b'=' {
            if chunk[3] != b'=' {
                return None;
            }
            out.push(((a << 2) | (b >> 4)) as u8);
        } else if chunk[3] == b'=' {
            let c = base64_decode_char(chunk[2])?;
            out.push(((a << 2) | (b >> 4)) as u8);
            out.push((((b & 0x0F) << 4) | (c >> 2)) as u8);
        } else {
            let c = base64_decode_char(chunk[2])?;
            let d = base64_decode_char(chunk[3])?;
            out.push(((a << 2) | (b >> 4)) as u8);
            out.push((((b & 0x0F) << 4) | (c >> 2)) as u8);
            out.push((((c & 0x03) << 6) | d) as u8);
        }
    }
    Some(out)
}

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let a = chunk[0] as u32;
        let b = *chunk.get(1).unwrap_or(&0) as u32;
        let c = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (a << 16) | (b << 8) | c;
        out.push(BASE64_ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(BASE64_ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            BASE64_ALPHABET[((triple >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64_ALPHABET[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::schema::messages::ToolResultInput;

    #[test]
    fn total_tokens_honors_provider_total_then_falls_back() {
        assert_eq!(total_tokens(Some(18.0), Some(5.0), Some(23.0)), Some(23.0));
        assert_eq!(total_tokens(Some(18.0), None, None), Some(18.0));
        assert_eq!(total_tokens(None, Some(5.0), None), Some(5.0));
        assert_eq!(total_tokens(Some(18.0), Some(5.0), None), Some(23.0));
        assert_eq!(total_tokens(None, None, None), None);
    }

    #[test]
    fn subtract_tokens_clamps_at_zero() {
        assert_eq!(subtract_tokens(Some(100.0), Some(80.0)), Some(20.0));
        assert_eq!(subtract_tokens(Some(100.0), Some(120.0)), Some(0.0));
        assert_eq!(subtract_tokens(Some(100.0), None), Some(100.0));
        assert_eq!(subtract_tokens(None, Some(80.0)), None);
    }

    #[test]
    fn sum_tokens_returns_none_only_when_every_value_is_none() {
        assert_eq!(sum_tokens(&[]), None);
        assert_eq!(sum_tokens(&[None, None]), None);
        assert_eq!(sum_tokens(&[Some(9.0), None, Some(5752.0)]), Some(5761.0));
        assert_eq!(sum_tokens(&[None, Some(0.0)]), Some(0.0));
    }

    #[test]
    fn event_error_is_a_stream_scoped_invalid_provider_output() {
        let error = event_error("anthropic-messages", "boom", Some("raw"));
        assert_eq!(error.module, "ProviderShared");
        assert_eq!(error.method, "stream");
        match error.reason {
            LlmErrorReason::InvalidProviderOutput {
                message,
                route,
                raw,
                ..
            } => {
                assert_eq!(message, "boom");
                assert_eq!(route.as_deref(), Some("anthropic-messages"));
                assert_eq!(raw.as_deref(), Some("raw"));
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
    }

    #[test]
    fn parse_json_decodes_or_maps_to_event_error() {
        let value = parse_json("openai-chat", "{\"ok\":true}", "bad").unwrap();
        assert_eq!(value, json!({"ok": true}));
        let error = parse_json("openai-chat", "{oops", "bad").unwrap_err();
        match error.reason {
            LlmErrorReason::InvalidProviderOutput { message, raw, .. } => {
                assert_eq!(message, "bad");
                assert_eq!(raw.as_deref(), Some("{oops"));
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
    }

    #[test]
    fn join_text_joins_with_newlines() {
        assert_eq!(join_text(["a", "b"]), "a\nb");
        assert_eq!(join_text(["a"]), "a");
        assert_eq!(join_text(Vec::<&str>::new()), "");
    }

    #[test]
    fn wrap_system_update_escapes_xml_and_wraps() {
        assert_eq!(
            wrap_system_update(["line one", "a<b>&c"]),
            "<system-update>\nline one\na&lt;b&gt;&amp;c\n</system-update>",
        );
    }

    #[test]
    fn wrapped_system_update_requires_text_only_content() {
        let route = "openai-chat";
        let message = Message::system("You are helpful");
        let part = wrapped_system_update(route, &message).unwrap();
        match part {
            ContentPart::Text { text, cache, .. } => {
                assert_eq!(text, "<system-update>\nYou are helpful\n</system-update>");
                assert_eq!(cache, None);
            }
            other => panic!("expected text part, got {other:?}"),
        }

        let unsupported = Message::system(vec![
            Message::text("ok"),
            ContentPart::media("image/png", "aGVsbG8="),
        ]);
        let error = wrapped_system_update(route, &unsupported).unwrap_err();
        match error.reason {
            LlmErrorReason::InvalidRequest { message, .. } => {
                assert_eq!(
                    message,
                    "openai-chat system messages only support text content for now"
                );
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
    }

    #[test]
    fn parse_tool_input_accepts_empty_and_rejects_invalid_json() {
        assert_eq!(
            parse_tool_input("openai-chat", "get_weather", "").unwrap(),
            json!({}),
        );
        assert_eq!(
            parse_tool_input("openai-chat", "get_weather", "{\"city\":\"Paris\"}").unwrap(),
            json!({"city": "Paris"}),
        );
        let error = parse_tool_input("openai-chat", "get_weather", "{oops").unwrap_err();
        match error.reason {
            LlmErrorReason::InvalidProviderOutput { message, raw, .. } => {
                assert_eq!(
                    message,
                    "Invalid JSON input for openai-chat tool call get_weather"
                );
                assert_eq!(raw.as_deref(), Some("{oops"));
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
    }

    #[test]
    fn validate_media_accepts_a_data_url() {
        let part = ContentPart::media("image/png", "data:image/png;base64,aGVsbG8=");
        let media = validate_media("OpenAI Chat", &part, &IMAGE_MIMES).unwrap();
        assert_eq!(media.mime, "image/png");
        assert_eq!(media.base64, "aGVsbG8=");
        assert_eq!(media.data_url, "data:image/png;base64,aGVsbG8=");
        assert_eq!(media.bytes, b"hello".to_vec());
    }

    #[test]
    fn validate_media_accepts_raw_base64() {
        let part = ContentPart::media("image/png", "aGVsbG8=");
        assert!(validate_media("OpenAI Chat", &part, &IMAGE_MIMES).is_ok());
    }

    #[test]
    fn validate_media_rejects_unsupported_mimes() {
        let part = ContentPart::media("image/svg+xml", "aGVsbG8=");
        let error = validate_media("OpenAI Chat", &part, &IMAGE_MIMES).unwrap_err();
        match error.reason {
            LlmErrorReason::InvalidRequest { message, .. } => {
                assert_eq!(
                    message,
                    "OpenAI Chat does not support media type image/svg+xml"
                );
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
    }

    #[test]
    fn validate_media_rejects_mismatched_data_urls() {
        let part = ContentPart::media("image/png", "data:image/jpeg;base64,aGVsbG8=");
        let error = validate_media("OpenAI Chat", &part, &IMAGE_MIMES).unwrap_err();
        match error.reason {
            LlmErrorReason::InvalidRequest { message, .. } => {
                assert_eq!(
                    message,
                    "OpenAI Chat media type image/png does not match data URL type image/jpeg"
                );
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
    }

    #[test]
    fn validate_media_rejects_non_canonical_and_invalid_base64() {
        let non_canonical = ContentPart::media("image/png", "YR==");
        let error = validate_media("OpenAI Chat", &non_canonical, &IMAGE_MIMES).unwrap_err();
        match error.reason {
            LlmErrorReason::InvalidRequest { message, .. } => {
                assert_eq!(message, "OpenAI Chat media must contain canonical base64");
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }

        for invalid in ["!!!not-base64!!!", "", "aGVsbA!"] {
            let part = ContentPart::media("image/png", invalid);
            let error = validate_media("OpenAI Chat", &part, &IMAGE_MIMES).unwrap_err();
            match error.reason {
                LlmErrorReason::InvalidRequest { message, .. } => {
                    assert_eq!(message, "OpenAI Chat media must contain valid base64");
                }
                reason => panic!("unexpected reason: {reason:?}"),
            }
        }
    }

    #[test]
    fn validate_tool_file_validates_the_file_uri() {
        let file = ToolContent::File {
            uri: "aGVsbG8=".to_string(),
            mime: "image/png".to_string(),
            name: Some("chart.png".to_string()),
        };
        let media = validate_tool_file("OpenAI Chat", &file, &IMAGE_MIMES).unwrap();
        assert_eq!(media.bytes, b"hello".to_vec());
    }

    #[test]
    fn trim_base_url_strips_trailing_slashes() {
        assert_eq!(
            trim_base_url("https://api.example.com/"),
            "https://api.example.com"
        );
        assert_eq!(
            trim_base_url("https://api.example.com///"),
            "https://api.example.com"
        );
        assert_eq!(
            trim_base_url("https://api.example.com"),
            "https://api.example.com"
        );
    }

    #[test]
    fn tool_result_text_matches_the_ts_stringification() {
        let text = ContentPart::tool_result(ToolResultInput {
            id: "t".to_string(),
            name: "lookup".to_string(),
            result: json!("18 degrees"),
            result_type: Some(crate::schema::messages::ToolResultType::Text),
            ..ToolResultInput::default()
        });
        assert_eq!(tool_result_text(&text), "18 degrees");

        let error = ContentPart::tool_result(ToolResultInput {
            id: "t".to_string(),
            name: "lookup".to_string(),
            result: json!({"code": 500, "detail": "boom"}),
            result_type: Some(crate::schema::messages::ToolResultType::Error),
            ..ToolResultInput::default()
        });
        assert_eq!(
            tool_result_text(&error),
            "{\"code\":500,\"detail\":\"boom\"}"
        );

        // Primitive error values stringify per JS `String(value)`, i.e. strings
        // pass through unquoted.
        let primitive = ContentPart::tool_result(ToolResultInput {
            id: "t".to_string(),
            name: "lookup".to_string(),
            result: json!("boom"),
            result_type: Some(crate::schema::messages::ToolResultType::Error),
            ..ToolResultInput::default()
        });
        assert_eq!(tool_result_text(&primitive), "boom");

        let json_result = ContentPart::tool_result(ToolResultInput {
            id: "t".to_string(),
            name: "lookup".to_string(),
            result: json!({"temp": 18}),
            ..ToolResultInput::default()
        });
        assert_eq!(tool_result_text(&json_result), "{\"temp\":18}");
    }

    #[test]
    fn match_tool_choice_covers_every_arm() {
        let route = "anthropic-messages";
        let make = |input: &str| ToolChoice {
            r#type: match input {
                "auto" => ToolChoiceType::Auto,
                "none" => ToolChoiceType::None,
                "required" => ToolChoiceType::Required,
                _ => ToolChoiceType::Tool,
            },
            name: None,
        };
        assert_eq!(
            match_tool_choice(&make("auto"), route).unwrap(),
            MatchedToolChoice::Auto
        );
        assert_eq!(
            match_tool_choice(&make("none"), route).unwrap(),
            MatchedToolChoice::None
        );
        assert_eq!(
            match_tool_choice(&make("required"), route).unwrap(),
            MatchedToolChoice::Required,
        );
        let error = match_tool_choice(&make("tool"), route).unwrap_err();
        match error.reason {
            LlmErrorReason::InvalidRequest { message, .. } => {
                assert_eq!(
                    message,
                    "anthropic-messages tool choice requires a tool name"
                );
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
        assert_eq!(
            match_tool_choice(&ToolChoice::named("get_weather"), route).unwrap(),
            MatchedToolChoice::Tool("get_weather".to_string()),
        );
    }

    #[test]
    fn supports_and_unsupported_content_format_types() {
        let route = "anthropic-messages";
        assert!(supports_content(&Message::text("hi"), &["text"]));
        assert!(!supports_content(&Message::text("hi"), &["media"]));

        let single = unsupported_content(route, &MessageRole::User, &["text"]);
        match single.reason {
            LlmErrorReason::InvalidRequest { message, .. } => {
                assert_eq!(
                    message,
                    "anthropic-messages user messages only support text content for now"
                );
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
        let pair = unsupported_content(route, &MessageRole::Tool, &["text", "media"]);
        match pair.reason {
            LlmErrorReason::InvalidRequest { message, .. } => {
                assert_eq!(
                    message,
                    "anthropic-messages tool messages only support text and media content for now"
                );
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
        let many =
            unsupported_content(route, &MessageRole::System, &["text", "media", "reasoning"]);
        match many.reason {
            LlmErrorReason::InvalidRequest { message, .. } => {
                assert_eq!(
                    message,
                    "anthropic-messages system messages only support text, media, and reasoning content for now"
                );
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
    }

    #[tokio::test]
    async fn sse_framing_yields_data_and_drops_keep_alives() {
        let chunks: Vec<Result<Vec<u8>, LlmError>> = vec![
            Ok(b"event: message_start\ndata: {\"a\":1}\n\n".to_vec()),
            Ok(b"data: [DONE]\n\n".to_vec()),
            Ok(b"data:\n\n".to_vec()),
            Ok(b"data: {\"b\":2}\n\n".to_vec()),
        ];
        let stream = sse_framing(futures::stream::iter(chunks));
        futures::pin_mut!(stream);
        let mut frames = Vec::new();
        while let Some(frame) = stream.next().await {
            frames.push(frame.unwrap());
        }
        assert_eq!(
            frames,
            vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string(),],
        );
    }

    #[tokio::test]
    async fn sse_framing_propagates_transport_errors() {
        let chunks: Vec<Result<Vec<u8>, LlmError>> = vec![
            Ok(b"data: ok\n\n".to_vec()),
            Err(LlmError::invalid("connection reset")),
        ];
        let stream = sse_framing(futures::stream::iter(chunks));
        futures::pin_mut!(stream);
        assert_eq!(stream.next().await.unwrap().unwrap(), "ok");
        let error = stream.next().await.unwrap().unwrap_err();
        match error.reason {
            LlmErrorReason::InvalidRequest { message, .. } => {
                assert_eq!(message, "connection reset");
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
    }

    #[test]
    fn base64_round_trips() {
        for data in [&b"hello"[..], &b"hi!"[..], &b"ab"[..], b""] {
            let encoded = base64_encode(data);
            assert_eq!(base64_decode(&encoded).unwrap(), data);
        }
    }
}
