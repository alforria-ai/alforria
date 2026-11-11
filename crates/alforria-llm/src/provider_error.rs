//! Context-overflow classification (port of `provider-error.ts`).
//!
//! NOTE: This is the M2.4 `isContextOverflow` port, implemented here so that
//! the M2.5 executor's classification compiles and tests green while M2.4 was
//! still a stub. The M2.4 owner should review/re-land this file.

use std::sync::LazyLock;

use regex::Regex;

const PATTERN_SOURCES: &[&str] = &[
    r"(?i)prompt is too long",
    r"(?i)request_too_large",
    r"(?i)input is too long for (?:the )?(?:requested|selected) model",
    // OpenCode Go
    r"(?i)too large for the model'?s configured input limit",
    // Cloudflare Workers AI reports this as HTTP 413.
    r"(?i)exceeded this model context window limit",
    r"(?i)exceeds the context window",
    r"(?i)exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
    r"(?i)input token count.*exceeds the maximum",
    // Amazon Nova on Bedrock reports this as a mid-stream validationException.
    r"(?i)number of input tokens exceeds maximum length",
    r"(?i)tokens in request more than max tokens allowed",
    r"(?i)maximum prompt length is \d+",
    r"(?i)reduce the length of the messages",
    // DeepInfra
    r"(?i)requested input length \d+ exceeds maximum input length",
    r"(?i)maximum context length is \d+ tokens",
    r"(?i)exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
    // Novita omits the token counts.
    r"(?i)input(?: \(\d+ tokens\))? is longer than the model'?s context length",
    r"(?i)exceeds the limit of \d+",
    r"(?i)exceeds the available context size",
    r"(?i)greater than the context length",
    // Hugging Face Text Generation Inference, e.g. Together
    r"(?i)`inputs` tokens \+ `max_new_tokens` must be <= \d+",
    r"(?i)context window exceeds limit",
    // LiteLLM's ContextWindowExceededError
    r"(?i)context window exceeded",
    r"(?i)exceeded model token limit",
    r"(?i)context[_ ]length[_ ]exceeded",
    r"(?i)request entity too large",
    r"(?i)context length is only \d+ tokens",
    r"(?i)input length.*exceeds.*context length",
    // Z.ai code 1261: `Prompt too long`, `Prompt exceeds max length`, `Prompt 超长`.
    r"(?i)prompt (?:too long|exceeds max length|超长)",
    r"(?i)too large for model with \d+ maximum context length",
    r"(?i)prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
    r"(?i)model_context_window_exceeded",
    r"(?i)range of input length should be",
    r"(?i)too many tokens",
    r"(?i)token limit exceeded",
];

const EXCLUSION_SOURCES: &[&str] = &[
    r"(?i)^(throttling error|service unavailable):",
    r"(?i)rate limit",
    r"(?i)too many requests",
    // An output limit above the model maximum (Cohere words it as "too many
    // tokens"); compaction cannot fix it.
    r"(?i)max[_ ]tokens must be less than",
    // Bedrock throttling: "Too many tokens[, per day], please wait...".
    r"(?i)too many tokens(?: per day)?, please wait",
];

fn compile(sources: &[&str]) -> Vec<Regex> {
    sources
        .iter()
        .map(|source| Regex::new(source).expect("invalid regex"))
        .collect()
}

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| compile(PATTERN_SOURCES));
static EXCLUSIONS: LazyLock<Vec<Regex>> = LazyLock::new(|| compile(EXCLUSION_SOURCES));
static NO_BODY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^4(00|13)\s*(status code)?\s*\(no body\)").unwrap());

/// Whether a provider error message indicates the request exceeded the
/// model's context window (from `provider-error.ts`).
pub fn is_context_overflow(message: &str) -> bool {
    if EXCLUSIONS.iter().any(|pattern| pattern.is_match(message)) {
        return false;
    }
    PATTERNS.iter().any(|pattern| pattern.is_match(message)) || NO_BODY.is_match(message)
}

static CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""(?:code|type|status)"\s*:\s*"([A-Za-z0-9_.\-]+)""#).unwrap());
static RATE_LIMIT_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)rate increased too quickly|rate[-_\s]?limit|too[_\s]?many[_\s]?requests")
        .unwrap()
});
static CONTENT_POLICY_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)violating our usage policy|blocked by content filtering policy|content[-_\s]?policy|rejected (?:as a result of our|by the) safety system").unwrap()
});
static SERVER_ERROR_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:try again|retry (?:the |this |your )?request|at capacity|overloaded|temporarily unavailable|service[-_\s]?unavailable|(?:server|internal)[-_\s]?error|server (?:is )?busy|provider returned (?:an )?error|resource[-_\s]?exhausted|upstream (?:connect|connection|request))\b").unwrap()
});

const QUOTA_CODES: &[&str] = &["insufficient_quota", "billing_error", "creditlimitexceeded"];
const AUTH_CODES: &[&str] = &[
    "authentication_error",
    "permission_error",
    "permission_denied",
    "api_key_invalid",
    "invalid_api_key",
];
const SERVER_CODES: &[&str] = &[
    "api_error",
    "internal_error",
    "overloaded_error",
    "server_error",
    "server_is_overloaded",
    "slow_down",
];
const CONTENT_POLICY_CODES: &[&str] = &[
    "content_filter",
    "content_moderation",
    "content_policy_violation",
    "refusal",
    "moderation_blocked",
];
const OVERFLOW_CODES: &[&str] = &[
    "context_length_exceeded",
    "model_context_window_exceeded",
    "request_too_large",
];

/// Classify a failure a provider reported **inside** a 200 response stream
/// (port of `classifyProviderFailure` in `provider-error.ts`, reduced to the
/// signals a stream frame carries: a message, an optional numeric status in
/// `error.code`, and the raw frame scanned for string codes).
///
/// Deterministic failures need positive evidence; anything unrecognised
/// stays `UnknownProvider`.
pub fn classify_stream_failure(
    message: &str,
    status: Option<u16>,
    raw: &str,
) -> crate::schema::errors::LlmErrorReason {
    use crate::schema::errors::{AuthKind, LlmErrorReason, ProviderFailureClassification};

    let codes: Vec<String> = CODE
        .captures_iter(raw)
        .map(|captures| captures[1].to_lowercase())
        .collect();
    let has = |set: &[&str]| codes.iter().any(|code| set.contains(&code.as_str()));
    let text = format!("{message}\n{raw}");
    let message = message.to_string();
    let client_scoped = status.is_none_or(|status| (400..500).contains(&status));

    if client_scoped
        && (has(OVERFLOW_CODES)
            || (status != Some(429)
                && !codes.iter().any(|code| code == "throttlingexception")
                && is_context_overflow(&text)))
    {
        return LlmErrorReason::InvalidRequest {
            message,
            parameter: None,
            classification: Some(ProviderFailureClassification::ContextOverflow),
            provider_metadata: None,
            http: None,
        };
    }
    if has(CONTENT_POLICY_CODES) || (client_scoped && CONTENT_POLICY_TEXT.is_match(&message)) {
        return LlmErrorReason::ContentPolicy {
            message,
            provider_metadata: None,
            http: None,
        };
    }
    if status == Some(402) || has(QUOTA_CODES) {
        return LlmErrorReason::QuotaExceeded {
            message,
            provider_metadata: None,
            http: None,
        };
    }
    if matches!(status, Some(401 | 403)) || has(AUTH_CODES) {
        return LlmErrorReason::Authentication {
            message,
            kind: if status == Some(403) {
                AuthKind::InsufficientPermissions
            } else {
                AuthKind::Invalid
            },
            provider_metadata: None,
            http: None,
        };
    }
    if status == Some(429)
        || codes
            .iter()
            .any(|code| code.contains("rate_limit") || code == "too_many_requests")
        || RATE_LIMIT_TEXT.is_match(&text)
    {
        return LlmErrorReason::RateLimit {
            message,
            retry_after_ms: None,
            rate_limit: None,
            provider_metadata: None,
            http: None,
        };
    }
    let invalid_code = codes.iter().any(|code| {
        matches!(
            code.as_str(),
            "model_not_found" | "invalid_prompt" | "invalid_request" | "invalid_request_error"
        )
    });
    if matches!(status, Some(408 | 409))
        || status.is_some_and(|status| status >= 500)
        || (status.is_none_or(|status| status < 400)
            && ((!invalid_code && SERVER_ERROR_TEXT.is_match(&text))
                || has(SERVER_CODES)
                || codes
                    .iter()
                    .any(|code| code.contains("exhausted") || code.contains("unavailable"))))
    {
        return LlmErrorReason::ProviderInternal {
            message,
            status: f64::from(status.filter(|status| *status >= 400).unwrap_or(500)),
            retry_after_ms: None,
            provider_metadata: None,
            http: None,
        };
    }
    if invalid_code || status.is_some_and(|status| (400..500).contains(&status)) {
        return LlmErrorReason::InvalidRequest {
            message,
            parameter: None,
            classification: None,
            provider_metadata: None,
            http: None,
        };
    }
    LlmErrorReason::UnknownProvider {
        message,
        status: status.map(f64::from),
        provider_metadata: None,
        http: None,
    }
}

#[cfg(test)]
mod tests {
    use super::is_context_overflow;

    #[test]
    fn detects_positive_samples() {
        assert!(is_context_overflow("Error: prompt is too long"));
        assert!(is_context_overflow(
            "requested 5000 tokens; maximum context length is 4096 tokens"
        ));
        assert!(is_context_overflow("400 (no body)"));
    }

    #[test]
    fn honors_exclusions() {
        assert!(!is_context_overflow("Too many requests, please wait"));
        assert!(!is_context_overflow(
            "rate limit reached: prompt is too long"
        ));
        assert!(!is_context_overflow(
            "Throttling error: the request was rejected"
        ));
    }

    #[test]
    fn detects_gateway_and_newer_provider_wording() {
        for sample in [
            "litellm.ContextWindowExceededError: context window exceeded",
            "Input is too long for the selected model",
            "Prompt exceeds max length",
            "requested input length 9000 exceeds maximum input length 8192",
            "This input is longer than the model's context length",
        ] {
            assert!(is_context_overflow(sample), "{sample}");
        }
    }

    #[test]
    fn output_limit_and_bedrock_throttle_are_not_overflow() {
        assert!(!is_context_overflow(
            "too many tokens: max_tokens must be less than 4096"
        ));
        assert!(!is_context_overflow(
            "Too many tokens, please wait before trying again."
        ));
    }

    mod stream {
        use super::super::classify_stream_failure;
        use crate::schema::errors::{LlmErrorReason, ProviderFailureClassification};

        #[test]
        fn rate_limit_is_retryable() {
            let reason = classify_stream_failure(
                "Rate limit reached for model",
                None,
                r#"{"error":{"message":"Rate limit reached for model","type":"rate_limit_error"}}"#,
            );
            assert!(
                matches!(reason, LlmErrorReason::RateLimit { .. }),
                "{reason:?}"
            );
            assert!(reason.retryable());
        }

        #[test]
        fn overflow_is_classified_for_compaction() {
            let reason = classify_stream_failure(
                "This model's maximum context length is 8192 tokens",
                Some(400),
                r#"{"error":{"message":"…","code":"context_length_exceeded"}}"#,
            );
            assert!(matches!(
                reason,
                LlmErrorReason::InvalidRequest {
                    classification: Some(ProviderFailureClassification::ContextOverflow),
                    ..
                }
            ));
        }

        #[test]
        fn overloaded_is_a_retryable_server_error() {
            let reason = classify_stream_failure(
                "Overloaded",
                None,
                r#"{"error":{"message":"Overloaded","type":"overloaded_error"}}"#,
            );
            assert!(
                matches!(reason, LlmErrorReason::ProviderInternal { .. }),
                "{reason:?}"
            );
            assert!(reason.retryable());
        }

        #[test]
        fn numeric_status_and_unknowns() {
            assert!(matches!(
                classify_stream_failure("nope", Some(401), "{}"),
                LlmErrorReason::Authentication { .. }
            ));
            assert!(matches!(
                classify_stream_failure("bad thing", Some(422), "{}"),
                LlmErrorReason::InvalidRequest {
                    classification: None,
                    ..
                }
            ));
            assert!(matches!(
                classify_stream_failure("mystery", None, "{}"),
                LlmErrorReason::UnknownProvider { .. }
            ));
        }
    }
}
