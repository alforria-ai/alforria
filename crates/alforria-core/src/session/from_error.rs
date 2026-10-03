//! `MessageV2.fromError` (message-v2.ts:606-734) — maps thrown errors onto
//! the `AssistantError` wire shapes — plus the `ProviderError` helpers it
//! calls (`provider/error.ts`: `parseAPICallError`, `parseStreamError`, the
//! `message()` normalizer and Node's `STATUS_CODES`).
//!
//! Note: `JSON.stringify(body)` in `parseStreamError` reproduces the parsed
//! object; the Rust `serde_json` map is key-sorted where JS keeps insertion
//! order, so the diagnostic `responseBody` string may order keys differently.

use std::collections::BTreeMap;

use alforria_llm::provider_error::is_context_overflow;
use alforria_schema::session_v1::AssistantError;
use serde_json::Value;

/// Node's `http.STATUS_CODES` — used by the `message()` normalizer to
/// detect "the AI SDK only set the default status text".
#[rustfmt::skip]
const STATUS_CODES: &[(u16, &str)] = &[
    (100, "Continue"), (101, "Switching Protocols"), (102, "Processing"), (103, "Early Hints"),
    (200, "OK"), (201, "Created"), (202, "Accepted"), (203, "Non-Authoritative Information"),
    (204, "No Content"), (205, "Reset Content"), (206, "Partial Content"), (207, "Multi-Status"),
    (208, "Already Reported"), (226, "IM Used"),
    (300, "Multiple Choices"), (301, "Moved Permanently"), (302, "Found"), (303, "See Other"),
    (304, "Not Modified"), (305, "Use Proxy"), (307, "Temporary Redirect"), (308, "Permanent Redirect"),
    (400, "Bad Request"), (401, "Unauthorized"), (402, "Payment Required"), (403, "Forbidden"),
    (404, "Not Found"), (405, "Method Not Allowed"), (406, "Not Acceptable"),
    (407, "Proxy Authentication Required"), (408, "Request Timeout"), (409, "Conflict"),
    (410, "Gone"), (411, "Length Required"), (412, "Precondition Failed"),
    (413, "Payload Too Large"), (414, "URI Too Long"), (415, "Unsupported Media Type"),
    (416, "Range Not Satisfiable"), (417, "Expectation Failed"), (418, "I'm a Teapot"),
    (421, "Misdirected Request"), (422, "Unprocessable Entity"), (423, "Locked"),
    (424, "Failed Dependency"), (425, "Too Early"), (426, "Upgrade Required"),
    (428, "Precondition Required"), (429, "Too Many Requests"),
    (431, "Request Header Fields Too Large"), (451, "Unavailable For Legal Reasons"),
    (500, "Internal Server Error"), (501, "Not Implemented"), (502, "Bad Gateway"),
    (503, "Service Unavailable"), (504, "Gateway Timeout"), (505, "HTTP Version Not Supported"),
    (506, "Variant Also Negotiates"), (507, "Insufficient Storage"), (508, "Loop Detected"),
    (509, "Bandwidth Limit Exceeded"), (510, "Not Extended"), (511, "Network Authentication Required"),
];

pub(crate) fn status_text(code: u64) -> Option<&'static str> {
    STATUS_CODES
        .iter()
        .find(|(candidate, _)| u64::from(*candidate) == code)
        .map(|(_, text)| *text)
}

/// The fields of an ai-sdk `APICallError` that `fromError` reads.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApiCallError {
    pub message: String,
    pub status_code: Option<u64>,
    pub is_retryable: bool,
    pub response_headers: Option<BTreeMap<String, String>>,
    pub response_body: Option<String>,
    pub url: Option<String>,
}

/// The error classes `fromError` discriminates over. TS matches on
/// `instanceof` plus fields; Rust callers construct the variant that models
/// the thrown error.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceError {
    /// `DOMException` with `name === "AbortError"`.
    Abort { message: String },
    /// `OutputLengthError` (`MessageOutputLengthError`).
    OutputLength,
    /// ai-sdk `LoadAPIKeyError`.
    LoadApiKey { message: String },
    /// A Node system error with `code === "ECONNRESET"`.
    Econnreset {
        code: String,
        syscall: String,
        message: String,
    },
    /// Bun fetch gzip/br decompression failure (`code === "ZlibError"`).
    Zlib { message: String },
    /// `ProviderError.HeaderTimeoutError`.
    HeaderTimeout { ms: u64 },
    /// `ProviderError.ResponseStreamError`.
    ResponseStream { message: String },
    /// ai-sdk `APICallError`.
    ApiCall(Box<ApiCallError>),
    /// Any other `Error` — `errorMessage(e)` extracts the message.
    Error { message: String },
    /// A non-`Error` thrown value.
    Other(Value),
}

/// `errorMessage` (tui/util/error.ts:136-156), reduced to the `Error` input
/// path; non-Error values are handled by [`SourceError::Other`].
fn error_message(error: &SourceError) -> String {
    match error {
        SourceError::Error { message } if !message.is_empty() => message.clone(),
        // `if (error.name) return error.name` — the default Error name.
        SourceError::Error { .. } => "Error".to_string(),
        _ => "unknown error".to_string(),
    }
}

/// `json(input)` (provider/error.ts:73-87): JSON.parse strings, keep
/// objects, else `undefined`.
fn json(input: &Value) -> Option<Value> {
    match input {
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .filter(|parsed| parsed.is_object()),
        value if value.is_object() => Some(value.clone()),
        _ => None,
    }
}

/// JS truthiness for JSON values.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|n| n != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(_) => true,
    }
}

/// `isOpenAiErrorRetryable` (provider/error.ts:23-28).
fn is_open_ai_error_retryable(e: &ApiCallError) -> bool {
    match e.status_code {
        None => e.is_retryable,
        // openai sometimes returns 404 for models that are actually available
        Some(404) => true,
        Some(_) => e.is_retryable,
    }
}

/// `body.message || body.error || body.error?.message` — the first truthy
/// entry, which must then be a string to be used.
fn extract_error_message(body: &Value) -> Option<String> {
    for candidate in [
        body.get("message"),
        body.get("error"),
        body.get("error").and_then(|error| error.get("message")),
    ] {
        if candidate.map(truthy).unwrap_or(false) {
            if let Some(Value::String(text)) = candidate {
                return Some(text.clone());
            }
            // A truthy non-string (e.g. an error object) stops the search.
            return None;
        }
    }
    None
}

/// `/^\s*<!doctype|^\s*<html/i` (provider/error.ts:59).
fn is_html_page(body: &str) -> bool {
    let trimmed = body.trim_start().to_ascii_lowercase();
    trimmed.starts_with("<!doctype") || trimmed.starts_with("<html")
}

/// `message(providerID, e)` (provider/error.ts:32-71) — the trailing
/// `.trim()` of the iife applies to every return path.
fn api_error_message(e: &ApiCallError) -> String {
    let msg = &e.message;
    let result = if msg.is_empty() {
        match (&e.response_body, e.status_code) {
            (Some(body), _) => body.clone(),
            (None, Some(code)) => status_text(code).map(String::from).unwrap_or_default(),
            (None, None) => "Unknown error".to_string(),
        }
    } else if e
        .response_body
        .as_deref()
        .map(str::is_empty)
        .unwrap_or(true)
        || e.status_code
            .and_then(status_text)
            .map(|status| msg != status)
            .unwrap_or(true)
    {
        msg.clone()
    } else {
        let body = json(&Value::String(e.response_body.clone().unwrap_or_default()));
        if let Some(body) = body.as_ref() {
            if let Some(err_msg) = extract_error_message(body) {
                return format!("{msg}: {err_msg}").trim().to_string();
            }
        }
        // If responseBody is HTML (e.g. from a gateway or proxy error page),
        // provide a human-readable message instead of dumping raw markup.
        let body = e.response_body.as_deref().unwrap_or_default();
        if is_html_page(body) {
            return match e.status_code {
                Some(401) => "Unauthorized: request was blocked by a gateway or proxy. Your authentication token may be missing or expired — try running `alforria auth login <your provider URL>` to re-authenticate.",
                Some(403) => "Forbidden: request was blocked by a gateway or proxy. You may not have permission to access this resource — check your account and provider settings.",
                _ => msg,
            }
            .to_string();
        }
        format!("{msg}: {body}")
    };
    result.trim().to_string()
}

/// `ParsedStreamError` (provider/error.ts:89-100).
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedStreamError {
    ContextOverflow {
        message: String,
        response_body: String,
    },
    ApiError {
        message: String,
        is_retryable: bool,
        response_body: String,
    },
}

/// `parseStreamError` (provider/error.ts:102-154).
pub fn parse_stream_error(input: &Value) -> Option<ParsedStreamError> {
    let raw = json(input)?;
    // `typeof raw?.message === "string" ? (json(raw.message) ?? raw) : raw`
    let body = match raw.get("message") {
        Some(Value::String(message)) => {
            json(&Value::String(message.clone())).unwrap_or_else(|| raw.clone())
        }
        _ => raw.clone(),
    };
    let response_body = serde_json::to_string(&body).unwrap_or_default();
    if body.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let error = body.get("error");
    let code = error.and_then(|e| e.get("code")).and_then(Value::as_str);
    let error_message = error.and_then(|e| e.get("message")).and_then(Value::as_str);
    match code {
        Some("context_length_exceeded") => Some(ParsedStreamError::ContextOverflow {
            message: "Input exceeds context window of this model".to_string(),
            response_body,
        }),
        Some("insufficient_quota") => Some(ParsedStreamError::ApiError {
            message: "Quota exceeded. Check your plan and billing details.".to_string(),
            is_retryable: false,
            response_body,
        }),
        Some("usage_not_included") => Some(ParsedStreamError::ApiError {
            message:
                "To use Codex with your ChatGPT plan, upgrade to Plus: https://chatgpt.com/explore/plus."
                    .to_string(),
            is_retryable: false,
            response_body,
        }),
        Some("invalid_prompt") => Some(ParsedStreamError::ApiError {
            message: error_message.unwrap_or("Invalid prompt.").to_string(),
            is_retryable: false,
            response_body,
        }),
        // `server_is_overloaded` / `server_error` and the switch fallthrough.
        _ => Some(ParsedStreamError::ApiError {
            message: error_message.unwrap_or("Server error.").to_string(),
            is_retryable: true,
            response_body,
        }),
    }
}

/// `parseAPICallError` (provider/error.ts:172-193): context-overflow
/// detection, then the api_error mapping with the openai retryable quirk.
pub fn parse_api_call_error(provider_id: &str, e: &ApiCallError) -> AssistantError {
    let m = api_error_message(e);
    let body = e
        .response_body
        .as_ref()
        .and_then(|body| json(&Value::String(body.clone())));
    let body_code = body.and_then(|body| {
        body.get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    if is_context_overflow(&m)
        || e.status_code == Some(413)
        || body_code.as_deref() == Some("context_length_exceeded")
    {
        return AssistantError::ContextOverflow {
            message: m,
            response_body: e.response_body.clone(),
        };
    }
    let metadata = e
        .url
        .as_ref()
        .map(|url| BTreeMap::from([("url".to_string(), url.clone())]));
    AssistantError::Api {
        message: m,
        status_code: e.status_code,
        is_retryable: if provider_id.starts_with("openai") {
            is_open_ai_error_retryable(e)
        } else {
            e.is_retryable
        },
        response_headers: e.response_headers.clone(),
        response_body: e.response_body.clone(),
        metadata,
    }
}

/// `{ providerID, aborted? }` (message-v2.ts:608-609).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromErrorCtx {
    pub provider_id: String,
    pub aborted: bool,
}

/// `fromError` (message-v2.ts:606-734).
pub fn from_error(e: &SourceError, ctx: FromErrorCtx) -> AssistantError {
    match e {
        SourceError::Abort { message } => AssistantError::Aborted {
            message: message.clone(),
        },
        SourceError::OutputLength => AssistantError::OutputLength {},
        SourceError::LoadApiKey { message } => AssistantError::Auth {
            provider_id: ctx.provider_id.clone(),
            message: message.clone(),
        },
        SourceError::Econnreset {
            code,
            syscall,
            message,
        } => AssistantError::Api {
            message: "Connection reset by server".to_string(),
            status_code: None,
            is_retryable: true,
            response_headers: None,
            response_body: None,
            metadata: Some(BTreeMap::from([
                ("code".to_string(), code.clone()),
                ("syscall".to_string(), syscall.clone()),
                ("message".to_string(), message.clone()),
            ])),
        },
        SourceError::Zlib { message } if ctx.aborted => AssistantError::Aborted {
            message: message.clone(),
        },
        SourceError::Zlib { message } => AssistantError::Api {
            message: "Response decompression failed".to_string(),
            status_code: None,
            is_retryable: true,
            response_headers: None,
            response_body: None,
            metadata: Some(BTreeMap::from([
                ("code".to_string(), "ZlibError".to_string()),
                ("message".to_string(), message.clone()),
            ])),
        },
        SourceError::HeaderTimeout { ms } => AssistantError::Api {
            message: format!("Provider response headers timed out after {ms}ms"),
            status_code: None,
            is_retryable: true,
            response_headers: None,
            response_body: None,
            metadata: Some(BTreeMap::from([
                ("code".to_string(), "ProviderHeaderTimeoutError".to_string()),
                ("timeoutMs".to_string(), ms.to_string()),
            ])),
        },
        SourceError::ResponseStream { message } => AssistantError::Api {
            message: message.clone(),
            status_code: None,
            is_retryable: true,
            response_headers: None,
            response_body: None,
            metadata: Some(BTreeMap::from([(
                "code".to_string(),
                "ProviderResponseStreamError".to_string(),
            )])),
        },
        SourceError::ApiCall(api) => parse_api_call_error(&ctx.provider_id, api),
        SourceError::Error { .. } => AssistantError::Unknown {
            message: error_message(e),
            r#ref: None,
        },
        SourceError::Other(value) => match parse_stream_error(value) {
            Some(ParsedStreamError::ContextOverflow {
                message,
                response_body,
            }) => AssistantError::ContextOverflow {
                message,
                response_body: Some(response_body),
            },
            Some(ParsedStreamError::ApiError {
                message,
                is_retryable,
                response_body,
            }) => AssistantError::Api {
                message,
                status_code: None,
                is_retryable,
                response_headers: None,
                response_body: Some(response_body),
                metadata: None,
            },
            None => AssistantError::Unknown {
                message: serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
                r#ref: None,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> FromErrorCtx {
        FromErrorCtx {
            provider_id: "anthropic".to_string(),
            aborted: false,
        }
    }

    #[test]
    fn abort_maps_to_aborted_error() {
        let err = from_error(
            &SourceError::Abort {
                message: "The operation was aborted".to_string(),
            },
            ctx(),
        );
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            json!({
                "name": "MessageAbortedError",
                "data": {"message": "The operation was aborted"}
            })
        );
    }

    #[test]
    fn output_length_passes_through() {
        let err = from_error(&SourceError::OutputLength, ctx());
        assert_eq!(err, AssistantError::OutputLength {});
    }

    #[test]
    fn load_api_key_maps_to_auth_error() {
        let err = from_error(
            &SourceError::LoadApiKey {
                message: "Missing apiKey".to_string(),
            },
            ctx(),
        );
        assert_eq!(
            err,
            AssistantError::Auth {
                provider_id: "anthropic".to_string(),
                message: "Missing apiKey".to_string(),
            }
        );
    }

    #[test]
    fn econnreset_maps_to_retryable_api_error() {
        let err = from_error(
            &SourceError::Econnreset {
                code: "ECONNRESET".to_string(),
                syscall: "read".to_string(),
                message: "socket hang up".to_string(),
            },
            ctx(),
        );
        assert_eq!(
            err,
            AssistantError::Api {
                message: "Connection reset by server".to_string(),
                status_code: None,
                is_retryable: true,
                response_headers: None,
                response_body: None,
                metadata: Some(BTreeMap::from([
                    ("code".to_string(), "ECONNRESET".to_string()),
                    ("syscall".to_string(), "read".to_string()),
                    ("message".to_string(), "socket hang up".to_string()),
                ])),
            }
        );
    }

    #[test]
    fn zlib_is_abort_aware() {
        let zerr = SourceError::Zlib {
            message: "incorrect header check".to_string(),
        };
        let aborted = from_error(
            &zerr,
            FromErrorCtx {
                provider_id: "anthropic".to_string(),
                aborted: true,
            },
        );
        assert!(matches!(aborted, AssistantError::Aborted { .. }));
        match from_error(&zerr, ctx()) {
            AssistantError::Api {
                message,
                is_retryable,
                metadata,
                ..
            } => {
                assert_eq!(message, "Response decompression failed");
                assert!(is_retryable);
                assert_eq!(
                    metadata.as_ref().and_then(|m| m.get("code")),
                    Some(&"ZlibError".to_string())
                );
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn header_timeout_and_stream_errors() {
        match from_error(&SourceError::HeaderTimeout { ms: 30_000 }, ctx()) {
            AssistantError::Api {
                message, metadata, ..
            } => {
                assert_eq!(message, "Provider response headers timed out after 30000ms");
                assert_eq!(
                    metadata.and_then(|m| m.get("timeoutMs").cloned()),
                    Some("30000".to_string())
                );
            }
            other => panic!("expected Api, got {other:?}"),
        }
        match from_error(
            &SourceError::ResponseStream {
                message: "premature close".to_string(),
            },
            ctx(),
        ) {
            AssistantError::Api {
                message, metadata, ..
            } => {
                assert_eq!(message, "premature close");
                assert_eq!(
                    metadata.and_then(|m| m.get("code").cloned()),
                    Some("ProviderResponseStreamError".to_string())
                );
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn api_call_overflows_become_context_overflow() {
        let err = from_error(
            &SourceError::ApiCall(Box::new(ApiCallError {
                message: "prompt is too long".to_string(),
                status_code: Some(400),
                is_retryable: false,
                response_headers: None,
                response_body: None,
                url: None,
            })),
            ctx(),
        );
        assert_eq!(
            err,
            AssistantError::ContextOverflow {
                message: "prompt is too long".to_string(),
                response_body: None,
            }
        );
        // 413 is also overflow.
        let err = from_error(
            &SourceError::ApiCall(Box::new(ApiCallError {
                message: "too big".to_string(),
                status_code: Some(413),
                is_retryable: false,
                response_headers: None,
                response_body: None,
                url: None,
            })),
            ctx(),
        );
        assert!(matches!(err, AssistantError::ContextOverflow { .. }));
    }

    #[test]
    fn openai_404_is_retryable() {
        let err = from_error(
            &SourceError::ApiCall(Box::new(ApiCallError {
                message: "Not Found".to_string(),
                status_code: Some(404),
                is_retryable: false,
                response_headers: None,
                response_body: None,
                url: Some("https://api.openai.com/v1/chat".to_string()),
            })),
            FromErrorCtx {
                provider_id: "openai".to_string(),
                aborted: false,
            },
        );
        match err {
            AssistantError::Api {
                is_retryable,
                metadata,
                ..
            } => {
                assert!(is_retryable, "openai 404 is retryable");
                assert_eq!(
                    metadata.and_then(|m| m.get("url").cloned()),
                    Some("https://api.openai.com/v1/chat".to_string())
                );
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn status_text_bodies_are_enriched() {
        // message is the bare status text -> parse the body for details.
        let err = from_error(
            &SourceError::ApiCall(Box::new(ApiCallError {
                message: "Internal Server Error".to_string(),
                status_code: Some(500),
                is_retryable: true,
                response_headers: None,
                response_body: Some(r#"{"error":{"message":"boom"}}"#.to_string()),
                url: None,
            })),
            ctx(),
        );
        match err {
            AssistantError::Api { message, .. } => {
                // `body.message || body.error || body.error?.message` stops at the
                // truthy (object) `body.error`, so the raw body is appended
                // (error.ts:44-49).
                assert_eq!(
                    message,
                    r#"Internal Server Error: {"error":{"message":"boom"}}"#
                );
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn empty_message_falls_back_to_status_or_body() {
        let err = from_error(
            &SourceError::ApiCall(Box::new(ApiCallError {
                message: String::new(),
                status_code: Some(502),
                is_retryable: true,
                response_headers: None,
                response_body: None,
                url: None,
            })),
            ctx(),
        );
        match err {
            AssistantError::Api { message, .. } => assert_eq!(message, "Bad Gateway"),
            other => panic!("expected Api, got {other:?}"),
        }
        let err = from_error(
            &SourceError::ApiCall(Box::new(ApiCallError {
                message: String::new(),
                status_code: None,
                is_retryable: false,
                response_headers: None,
                response_body: None,
                url: None,
            })),
            ctx(),
        );
        match err {
            AssistantError::Api { message, .. } => assert_eq!(message, "Unknown error"),
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn html_error_pages_are_humanized() {
        let err = from_error(
            &SourceError::ApiCall(Box::new(ApiCallError {
                message: "Unauthorized".to_string(),
                status_code: Some(401),
                is_retryable: false,
                response_headers: None,
                response_body: Some("<!doctype html><html>403</html>".to_string()),
                url: None,
            })),
            ctx(),
        );
        match err {
            AssistantError::Api { message, .. } => {
                assert!(message.starts_with("Unauthorized: request was blocked by a gateway"));
            }
            other => panic!("expected Api, got {other:?}"),
        }
        let err = from_error(
            &SourceError::ApiCall(Box::new(ApiCallError {
                message: "Forbidden".to_string(),
                status_code: Some(403),
                is_retryable: false,
                response_headers: None,
                response_body: Some("  <HTML>nope</HTML>".to_string()),
                url: None,
            })),
            ctx(),
        );
        match err {
            AssistantError::Api { message, .. } => {
                assert!(message.starts_with("Forbidden: request was blocked by a gateway"));
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn plain_body_is_appended() {
        let err = from_error(
            &SourceError::ApiCall(Box::new(ApiCallError {
                message: "Bad Request".to_string(),
                status_code: Some(400),
                is_retryable: false,
                response_headers: None,
                response_body: Some("raw plain text".to_string()),
                url: None,
            })),
            ctx(),
        );
        match err {
            AssistantError::Api { message, .. } => {
                assert_eq!(message, "Bad Request: raw plain text");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn generic_error_uses_error_message() {
        let err = from_error(
            &SourceError::Error {
                message: "something went wrong".to_string(),
            },
            ctx(),
        );
        assert_eq!(
            err,
            AssistantError::Unknown {
                message: "something went wrong".to_string(),
                r#ref: None,
            }
        );
        // Empty message falls back to the Error name.
        match from_error(
            &SourceError::Error {
                message: String::new(),
            },
            ctx(),
        ) {
            AssistantError::Unknown { message, .. } => assert_eq!(message, "Error"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn stream_errors_are_parsed_from_plain_values() {
        let value = json!({
            "type": "error",
            "error": {"code": "insufficient_quota", "message": "quota"}
        });
        let err = from_error(&SourceError::Other(value), ctx());
        match err {
            AssistantError::Api {
                message,
                is_retryable,
                ..
            } => {
                assert_eq!(
                    message,
                    "Quota exceeded. Check your plan and billing details."
                );
                assert!(!is_retryable);
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn stream_errors_may_wrap_json_strings() {
        let value = json!({
            "message": "{\"type\":\"error\",\"error\":{\"code\":\"server_is_overloaded\"}}"
        });
        match from_error(&SourceError::Other(value), ctx()) {
            AssistantError::Api { is_retryable, .. } => assert!(is_retryable),
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn non_error_values_stringify() {
        let err = from_error(&SourceError::Other(json!({"oops": true})), ctx());
        match err {
            AssistantError::Unknown { message, .. } => {
                assert_eq!(message, r#"{"oops":true}"#);
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
        assert_eq!(
            from_error(&SourceError::Other(json!(42)), ctx()),
            AssistantError::Unknown {
                message: "42".to_string(),
                r#ref: None,
            }
        );
    }

    #[test]
    fn parse_stream_error_requires_error_type() {
        assert!(parse_stream_error(&json!({"type": "ok"})).is_none());
        // json("not json") is undefined — no object to inspect.
        assert!(parse_stream_error(&json!("not json")).is_none());
        let parsed = parse_stream_error(&json!({
            "type": "error",
            "error": {"code": "server_error", "message": "overloaded"}
        }))
        .unwrap();
        assert_eq!(
            parsed,
            ParsedStreamError::ApiError {
                message: "overloaded".to_string(),
                is_retryable: true,
                response_body:
                    r#"{"error":{"code":"server_error","message":"overloaded"},"type":"error"}"#
                        .to_string(),
            }
        );
    }

    #[test]
    fn parse_stream_error_codes() {
        let overflow = parse_stream_error(&json!({
            "type": "error",
            "error": {"code": "context_length_exceeded"}
        }))
        .unwrap();
        assert!(matches!(
            overflow,
            ParsedStreamError::ContextOverflow { .. }
        ));

        let invalid = parse_stream_error(&json!({
            "type": "error",
            "error": {"code": "invalid_prompt"}
        }))
        .unwrap();
        assert_eq!(
            invalid,
            ParsedStreamError::ApiError {
                message: "Invalid prompt.".to_string(),
                is_retryable: false,
                response_body: r#"{"error":{"code":"invalid_prompt"},"type":"error"}"#.to_string(),
            }
        );

        let usage = parse_stream_error(&json!({
            "type": "error",
            "error": {"code": "usage_not_included"}
        }))
        .unwrap();
        assert_eq!(
            usage,
            ParsedStreamError::ApiError {
                message: "To use Codex with your ChatGPT plan, upgrade to Plus: https://chatgpt.com/explore/plus.".to_string(),
                is_retryable: false,
                response_body: r#"{"error":{"code":"usage_not_included"},"type":"error"}"#.to_string(),
            }
        );
    }

    #[test]
    fn context_overflow_via_response_body() {
        let err = from_error(
            &SourceError::ApiCall(Box::new(ApiCallError {
                message: "Request failed".to_string(),
                status_code: Some(400),
                is_retryable: false,
                response_headers: None,
                response_body: Some(
                    r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#
                        .to_string(),
                ),
                url: None,
            })),
            ctx(),
        );
        assert!(matches!(err, AssistantError::ContextOverflow { .. }));
    }
}
