//! `SessionRetry` — port of `session/retry.ts`: retryable-error
//! classification, retry delay/backoff, and the schedule policy the
//! processor's stream retry loop runs under.

use std::sync::LazyLock;

use opencode_schema::session_v1::AssistantError;
use regex::Regex;
use serde_json::Value;

/// `GO_UPSELL_MESSAGE` (retry.ts:15).
pub const GO_UPSELL_MESSAGE: &str = "Free usage exceeded, subscribe to Go";
/// `GO_UPSELL_URL` (retry.ts:16).
pub const GO_UPSELL_URL: &str = "https://opencode.ai/go";

pub const RETRY_INITIAL_DELAY: f64 = 2000.0;
pub const RETRY_BACKOFF_FACTOR: f64 = 2.0;
pub const RETRY_JITTER_FACTOR: f64 = 0.25;
/// 30 seconds (retry.ts:28).
pub const RETRY_MAX_DELAY_NO_HEADERS: f64 = 30_000.0;
/// Max 32-bit signed integer for setTimeout (retry.ts:30).
pub const RETRY_MAX_DELAY: f64 = 2_147_483_647.0;
pub const RETRY_MAX_RETRIES: u64 = 5;

/// The 7 retryable-message regexes (retry.ts:33-41), verbatim.
static RETRYABLE_MESSAGE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    #[rustfmt::skip]
    let patterns = [
        r"429|500|502|503|504|524",
        r"rate increased too quickly|rate limit|rate-limit|rate_limit|too many requests",
        r"overloaded|service unavailable|service_unavailable|service-unavailable|internal error|internal_error|internal server error|server error|server_error|server-error|provider returned error|provider_returned_error|provider-returned-error",
        r"terminated|fetch failed|failed to fetch|network[-_\s]error|upstream connect|connection error|connection refused|connection lost|socket connection was closed|socket hang up|reset before headers|getaddrinfo|enotfound|eai_again|econnrefused|econnreset|etimedout",
        r"^timeout$|\b(?:request|response|connection|network|stream|read) (?:timeout|timed out|time out)\b",
        r"try your request again|retry your request|resource exhausted|resource_exhausted",
        r"\btry again (?:later|in\b)|\b(?:currently|temporarily) at capacity\b",
    ];
    patterns
        .iter()
        .map(|source| Regex::new(&format!("(?i){source}")).expect("retryable regex"))
        .collect()
});

/// `Retryable` (retry.ts:23-33) — the message (and optional upsell action) a
/// retry attempt surfaces.
#[derive(Debug, Clone, PartialEq)]
pub struct Retryable {
    pub message: String,
    pub action: Option<RetryAction>,
}

/// `Retryable["action"]` — the upsell entry attached to usage-limit errors.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryAction {
    pub reason: String,
    pub provider: String,
    pub title: String,
    pub message: String,
    pub label: String,
    pub link: Option<String>,
}

/// `cap()` (retry.ts:43-45).
fn cap(ms: f64) -> f64 {
    ms.min(RETRY_MAX_DELAY)
}

/// `exponential()` (retry.ts:80-83): `ceil(base + base * JITTER * random)`.
fn exponential(attempt: u64, random: f64) -> f64 {
    let base = RETRY_INITIAL_DELAY * RETRY_BACKOFF_FACTOR.powi(attempt as i32 - 1);
    (base + base * RETRY_JITTER_FACTOR * random).ceil()
}

/// `delay()` (retry.ts:47-78): the wait before `attempt`.
///
/// Rust additions vs TS: `now_ms` replaces `Date.now()` for the HTTP-date
/// branch (fake-clock tests), and `error` takes the already-parsed
/// [`AssistantError`] whose `Api` variant carries the response headers.
pub fn delay(attempt: u64, error: Option<&AssistantError>, random: f64, now_ms: u64) -> f64 {
    if let Some(AssistantError::Api {
        response_headers: Some(headers),
        ..
    }) = error
    {
        // JS truthiness: an empty header value skips the branch.
        if let Some(raw) = headers.get("retry-after-ms").filter(|raw| !raw.is_empty()) {
            if let Some(parsed_ms) = js_parse_float(raw) {
                return cap(parsed_ms);
            }
        }
        if let Some(raw) = headers.get("retry-after").filter(|raw| !raw.is_empty()) {
            if let Some(parsed_seconds) = js_parse_float(raw) {
                // convert seconds to milliseconds
                return cap((parsed_seconds * 1000.0).ceil());
            }
            // Try parsing as HTTP date format
            let parsed = date_parse_ms(raw).map(|date| date - now_ms as f64);
            if let Some(delta) = parsed {
                if !delta.is_nan() && delta > 0.0 {
                    return cap(delta.ceil());
                }
            }
            return cap(exponential(attempt, random));
        }
        return cap(exponential(attempt, random));
    }
    cap(exponential(attempt, random).min(RETRY_MAX_DELAY_NO_HEADERS))
}

/// `retryable()` (retry.ts:85-155) — classification only; `error` is the
/// parsed wire error (TS matches on the `SessionV1` error class names).
pub fn retryable(error: &AssistantError, provider: &str) -> Option<Retryable> {
    match error {
        // context overflow errors should not be retried
        AssistantError::ContextOverflow { .. } => None,
        AssistantError::Api {
            message,
            status_code,
            is_retryable,
            response_headers,
            response_body,
            ..
        } => {
            // 5xx errors are transient server failures and should always be
            // retried, even when the provider SDK doesn't mark them retryable.
            if !*is_retryable
                && !status_is_5xx(*status_code)
                && !matches_retryable_message(message.as_str())
                && !matches_retryable_message(response_body.as_deref().unwrap_or_default())
            {
                return None;
            }
            if response_body
                .as_deref()
                .is_some_and(|body| body.contains("FreeUsageLimitError"))
            {
                return Some(Retryable {
                    message: GO_UPSELL_MESSAGE.to_string(),
                    action: Some(RetryAction {
                        reason: "free_tier_limit".to_string(),
                        provider: provider.to_string(),
                        title: "Free limit reached".to_string(),
                        message: "Subscribe to OpenCode Go for reliable access to the best open-source models for $10/month.".to_string(),
                        label: "subscribe".to_string(),
                        link: Some(GO_UPSELL_URL.to_string()),
                    }),
                });
            }
            if response_body
                .as_deref()
                .is_some_and(|body| body.contains("GoUsageLimitError"))
            {
                let body = parse_json(response_body.as_deref().unwrap_or_default());
                let workspace = body
                    .get("metadata")
                    .and_then(|metadata| metadata.get("workspace"))
                    .map(js_to_string)
                    .unwrap_or_default();
                let limit_name = body
                    .get("metadata")
                    .and_then(|metadata| metadata.get("limitName"))
                    .map(js_to_string)
                    .unwrap_or_default();
                let retry_after = response_headers
                    .as_ref()
                    .and_then(|headers| headers.get("retry-after"))
                    .and_then(|raw| js_parse_float(raw));
                let reset_in = match retry_after {
                    None => String::new(),
                    Some(retry_after) => {
                        let seconds = if retry_after.is_nan() {
                            0.0
                        } else {
                            retry_after.ceil().max(0.0)
                        };
                        let days = (seconds / 86_400.0).floor();
                        let hours = ((seconds % 86_400.0) / 3_600.0).floor();
                        let minutes = ((seconds % 3_600.0) / 60.0).ceil();
                        let unit = |value: f64, name: &str| {
                            format!("{value} {name}{}", if value == 1.0 { "" } else { "s" })
                        };

                        if days > 0.0 {
                            if hours > 0.0 {
                                format!("{} {}", unit(days, "day"), unit(hours, "hour"))
                            } else {
                                unit(days, "day")
                            }
                        } else if hours > 0.0 {
                            if minutes > 0.0 {
                                format!("{} {}", unit(hours, "hour"), unit(minutes, "minute"))
                            } else {
                                unit(hours, "hour")
                            }
                        } else if minutes > 0.0 {
                            unit(minutes, "minute")
                        } else {
                            "less than a minute".to_string()
                        }
                    }
                };

                let message = format!(
                    "{} reached. It will reset in {reset_in}. To continue using this model now, \
                     enable usage from your available balance",
                    if limit_name.is_empty() {
                        "Usage limit".to_string()
                    } else {
                        format!("{limit_name} usage limit")
                    }
                );
                let link = format!("https://opencode.ai/workspace/{workspace}/go");
                return Some(Retryable {
                    message: format!("{message} - {link}"),
                    action: Some(RetryAction {
                        reason: "account_rate_limit".to_string(),
                        provider: provider.to_string(),
                        title: "Go limit reached".to_string(),
                        message,
                        label: "open settings".to_string(),
                        link: Some(link),
                    }),
                });
            }
            Some(Retryable {
                message: if message.contains("Overloaded") {
                    "Provider is overloaded".to_string()
                } else {
                    message.clone()
                },
                action: None,
            })
        }
        _ => {
            let message = generic_error_message(error)?;
            let lower = message.to_lowercase();
            if lower.contains("too_many_requests") {
                return Some(Retryable {
                    message: "Too Many Requests".to_string(),
                    action: None,
                });
            }
            if lower.contains("exhausted") || lower.contains("unavailable") {
                return Some(Retryable {
                    message: "Provider is overloaded".to_string(),
                    action: None,
                });
            }
            if matches_retryable_message(&message) {
                return Some(Retryable {
                    message,
                    action: None,
                });
            }
            None
        }
    }
}

/// `status >= 500` (retry.ts:100).
fn status_is_5xx(status: Option<u64>) -> bool {
    status.is_some_and(|status| status >= 500)
}

/// `isRecord(error.data) ? error.data.message : undefined` for the
/// non-APIError branches (retry.ts:148).
fn generic_error_message(error: &AssistantError) -> Option<String> {
    match error {
        AssistantError::Auth { message, .. }
        | AssistantError::Unknown { message, .. }
        | AssistantError::Aborted { message }
        | AssistantError::StructuredOutput { message, .. }
        | AssistantError::ContentFilter { message } => Some(message.clone()),
        AssistantError::OutputLength {}
        | AssistantError::Api { .. }
        | AssistantError::ContextOverflow { .. } => None,
    }
}

/// `matchesRetryableMessage` (retry.ts:157-159).
fn matches_retryable_message(value: &str) -> bool {
    RETRYABLE_MESSAGE_PATTERNS
        .iter()
        .any(|pattern| pattern.is_match(value))
}

fn parse_json(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or(Value::Null)
}

/// `str()` (retry.ts:161-165): `undefined`/`null` → `""`, else `String(value)`.
fn js_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// JS `Number.parseFloat` — longest-valid-prefix parse; `None` is NaN.
fn js_parse_float(input: &str) -> Option<f64> {
    let s = input.trim_start();
    let bytes = s.as_bytes();
    let mut i = 0;

    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    if s[i..].starts_with("Infinity") {
        return Some(if bytes.first() == Some(&b'-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    let int_digits = count_digits(s, i);
    i += int_digits;
    let mut frac_digits = 0;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        frac_digits = count_digits(s, i);
        i += frac_digits;
    }
    if int_digits == 0 && frac_digits == 0 {
        return None; // no numeric prefix at all
    }
    let mut end = i;
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let exp_digits = count_digits(s, j);
        if exp_digits > 0 {
            end = j + exp_digits;
        }
    }
    let prefix = &s[..end];
    prefix.parse::<f64>().ok()
}

fn count_digits(s: &str, from: usize) -> usize {
    s.as_bytes()[from..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count()
}

/// JS `Date.parse` subset: RFC 2822 HTTP-dates (the `Retry-After` format)
/// plus common ISO-8601 forms. Returns epoch milliseconds.
fn date_parse_ms(value: &str) -> Option<f64> {
    let trimmed = value.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(trimmed) {
        return Some(dt.timestamp_millis() as f64);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.timestamp_millis() as f64);
    }
    if let Ok(naive) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let midnight = chrono::NaiveDateTime::new(
            naive,
            chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("midnight"),
        );
        return Some(midnight.and_utc().timestamp_millis() as f64);
    }
    None
}

/// The `SessionRetry.policy` schedule (retry.ts:183-207) as a resumable
/// step: TS hands a Schedule to `Effect.retry`; the Rust processor calls
/// [`Policy::step`] after each failed attempt.
#[derive(Debug, Clone)]
pub struct Policy {
    pub provider: String,
}

/// One policy decision over a failed attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyStep {
    /// `Cause.done` — stop retrying (not retryable, or retries exhausted).
    Done,
    /// Retry: run the `set` callback (the TS `opts.set`), then wait
    /// `wait_ms` before the next attempt.
    Retry { set: RetrySet, wait_ms: f64 },
}

/// The `opts.set({ attempt, message, action, next })` payload.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrySet {
    pub attempt: u64,
    pub message: String,
    pub action: Option<RetryAction>,
    pub next: u64,
}

impl Policy {
    pub fn new(provider: impl Into<String>) -> Policy {
        Policy {
            provider: provider.into(),
        }
    }

    /// One schedule step (retry.ts:186-205). `parse` mirrors the TS
    /// `opts.parse(meta.input)` — it maps the raw stream error onto the
    /// [`AssistantError`] wire shape; `attempt` is 1-based; `now_ms` is the
    /// `Clock.currentTimeMillis` read; `random` stands in for `Math.random()`.
    pub fn step<E>(
        &self,
        parse: impl Fn(&E) -> AssistantError,
        error: &E,
        attempt: u64,
        now_ms: u64,
        random: f64,
    ) -> PolicyStep {
        let error = parse(error);
        let Some(retry) = retryable(&error, &self.provider) else {
            return PolicyStep::Done;
        };
        if attempt > RETRY_MAX_RETRIES {
            return PolicyStep::Done;
        }
        let api = matches!(error, AssistantError::Api { .. }).then_some(error);
        let wait = delay(attempt, api.as_ref(), random, now_ms);
        PolicyStep::Retry {
            set: RetrySet {
                attempt,
                message: retry.message,
                action: retry.action,
                next: now_ms + wait as u64,
            },
            wait_ms: wait,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn api_error(
        message: &str,
        status: Option<u64>,
        is_retryable: bool,
        headers: Option<&[(&str, &str)]>,
        body: Option<&str>,
    ) -> AssistantError {
        let response_headers = headers.map(|entries| {
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<String, String>>()
        });
        AssistantError::Api {
            message: message.to_string(),
            status_code: status,
            is_retryable,
            response_headers,
            response_body: body.map(str::to_string),
            metadata: None,
        }
    }

    /// 28 delay vectors captured from the TS `delay` (pinned retry.ts). The
    /// HTTP-date pair is generated around the fixed fake clock, so the
    /// expected deltas are exact.
    #[test]
    fn ts_delay_vectors() {
        let now_ms: u64 = 1_800_000_000_000;
        let http_date = |offset_ms: i64| {
            use chrono::TimeZone;
            chrono::Utc
                .timestamp_millis_opt(now_ms as i64 + offset_ms)
                .unwrap()
                .to_rfc2822()
        };
        let future = http_date(90_000);
        let past = http_date(-90_000);

        let cases: Vec<(&str, u64, Option<AssistantError>, f64, f64)> = vec![
            ("d1", 1, None, 0.0, 2000.0),
            ("d2", 1, None, 0.5, 2250.0),
            ("d3", 1, None, 1.0, 2500.0),
            ("d4", 1, None, -0.25, 1875.0),
            ("d5", 2, None, 0.5, 4500.0),
            ("d6", 3, None, 1.0, 10000.0),
            ("d7", 5, None, 0.5, 30000.0),
            ("d8", 6, None, 0.5, 30000.0),
            (
                "d9",
                1,
                Some(api_error("x", None, false, Some(&[]), None)),
                0.5,
                2250.0,
            ),
            (
                "d10",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after-ms", "15000")]),
                    None,
                )),
                0.5,
                15000.0,
            ),
            (
                "d11",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after-ms", "15.75")]),
                    None,
                )),
                0.5,
                15.75,
            ),
            (
                "d12",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after-ms", "garbage")]),
                    None,
                )),
                0.5,
                2250.0,
            ),
            (
                "d13",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after", "5")]),
                    None,
                )),
                0.5,
                5000.0,
            ),
            (
                "d14",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after", "5.5")]),
                    None,
                )),
                0.5,
                5500.0,
            ),
            (
                "d15",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after", "garbage")]),
                    None,
                )),
                0.5,
                2250.0,
            ),
            (
                "d16",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after", "0")]),
                    None,
                )),
                0.5,
                0.0,
            ),
            (
                "d17",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after", "-5")]),
                    None,
                )),
                0.5,
                -5000.0,
            ),
            (
                "d18",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after", "3000000000")]),
                    None,
                )),
                0.5,
                2147483647.0,
            ),
            (
                "d19",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after-ms", "3000000000")]),
                    None,
                )),
                0.5,
                2147483647.0,
            ),
            (
                "d20",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after", "12abc")]),
                    None,
                )),
                0.5,
                12000.0,
            ),
            (
                "d21",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after-ms", "12abc")]),
                    None,
                )),
                0.5,
                12.0,
            ),
            (
                "d22",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after", future.as_str())]),
                    None,
                )),
                0.5,
                90000.0,
            ),
            (
                "d23",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after", past.as_str())]),
                    None,
                )),
                0.5,
                2250.0,
            ),
            (
                "d24",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after", "5"), ("retry-after-ms", "1000")]),
                    None,
                )),
                0.5,
                1000.0,
            ),
            (
                "d25",
                4,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after-ms", "")]),
                    None,
                )),
                0.5,
                18000.0,
            ),
            (
                "d26",
                4,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after", "")]),
                    None,
                )),
                0.5,
                18000.0,
            ),
            (
                "d27",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after", "1e3")]),
                    None,
                )),
                0.5,
                1000000.0,
            ),
            (
                "d28",
                1,
                Some(api_error(
                    "x",
                    None,
                    false,
                    Some(&[("retry-after", "Infinity")]),
                    None,
                )),
                0.5,
                2147483647.0,
            ),
        ];

        for (name, attempt, error, random, expected) in cases {
            let got = delay(attempt, error.as_ref(), random, now_ms);
            assert_eq!(got, expected, "delay vector {name}");
        }
    }

    /// 30 retryable vectors captured from the TS `retryable` (pinned
    /// retry.ts).
    #[test]
    fn ts_retryable_vectors() {
        let go_message = |reset: &str, limit: Option<&str>| {
            let limit_part = match limit {
                Some(limit) => format!("{limit} usage limit"),
                None => "Usage limit".to_string(),
            };
            format!(
                "{limit_part} reached. It will reset in {reset}. To continue using this model \
                 now, enable usage from your available balance"
            )
        };

        // r1: plain not-retryable API error
        assert_eq!(
            retryable(&api_error("nope", None, false, None, None), "anthropic"),
            None
        );
        // r2: isRetryable
        assert_eq!(
            retryable(&api_error("nope", None, true, None, None), "anthropic"),
            Some(Retryable {
                message: "nope".to_string(),
                action: None,
            })
        );
        // r3: 5xx
        assert_eq!(
            retryable(
                &api_error("nope", Some(503), false, None, None),
                "anthropic"
            ),
            Some(Retryable {
                message: "nope".to_string(),
                action: None,
            })
        );
        // r4: 404 not retryable
        assert_eq!(
            retryable(
                &api_error("nope", Some(404), false, None, None),
                "anthropic"
            ),
            None
        );
        // r5: message match
        assert_eq!(
            retryable(
                &api_error(
                    "service unavailable right now",
                    Some(404),
                    false,
                    None,
                    None
                ),
                "anthropic"
            ),
            Some(Retryable {
                message: "service unavailable right now".to_string(),
                action: None,
            })
        );
        // r6: responseBody match
        assert_eq!(
            retryable(
                &api_error(
                    "nope",
                    Some(404),
                    false,
                    None,
                    Some("OverloadedError: too much load")
                ),
                "anthropic"
            ),
            Some(Retryable {
                message: "nope".to_string(),
                action: None,
            })
        );
        // r7: FreeUsageLimitError upsell (gated behind retryable)
        let out = retryable(
            &api_error(
                "nope",
                Some(404),
                true,
                None,
                Some(r#"{"error":"FreeUsageLimitError","limit":10}"#),
            ),
            "anthropic",
        );
        assert_eq!(
            out.as_ref().map(|r| r.message.clone()),
            Some(GO_UPSELL_MESSAGE.to_string())
        );
        let action = out.and_then(|r| r.action).unwrap();
        assert_eq!(action.reason, "free_tier_limit");
        assert_eq!(action.provider, "anthropic");
        assert_eq!(action.title, "Free limit reached");
        assert_eq!(
            action.message,
            "Subscribe to OpenCode Go for reliable access to the best open-source models for $10/month."
        );
        assert_eq!(action.label, "subscribe");
        assert_eq!(action.link, Some(GO_UPSELL_URL.to_string()));
        // r8: Overloaded normalization
        assert_eq!(
            retryable(
                &api_error("Provider is Overloaded", Some(404), false, None, None),
                "anthropic"
            ),
            Some(Retryable {
                message: "Provider is overloaded".to_string(),
                action: None,
            })
        );
        // r9: GoUsageLimitError, no retry-after → empty resetIn
        let out = retryable(
            &api_error(
                "nope",
                Some(404),
                true,
                None,
                Some(r#"{"error":"GoUsageLimitError","metadata":{"workspace":"ws-123","limitName":"Pro"}}"#),
            ),
            "anthropic",
        )
        .unwrap();
        let expected = format!(
            "{} - https://opencode.ai/workspace/ws-123/go",
            go_message("", Some("Pro"))
        );
        assert_eq!(out.message, expected);
        assert_eq!(out.action.as_ref().unwrap().reason, "account_rate_limit");
        assert_eq!(out.action.as_ref().unwrap().title, "Go limit reached");
        assert_eq!(out.action.as_ref().unwrap().label, "open settings");
        assert_eq!(
            out.action.as_ref().unwrap().message,
            go_message("", Some("Pro"))
        );
        assert_eq!(
            out.action.as_ref().unwrap().link,
            Some("https://opencode.ai/workspace/ws-123/go".to_string())
        );
        // r10: retry-after 3600 → "1 hour"
        let out = retryable(
            &api_error(
                "nope",
                Some(404),
                true,
                Some(&[("retry-after", "3600")]),
                Some(r#"{"error":"GoUsageLimitError","metadata":{"workspace":"ws-123","limitName":"Pro"}}"#),
            ),
            "anthropic",
        )
        .unwrap();
        assert_eq!(
            out.action.as_ref().unwrap().message,
            go_message("1 hour", Some("Pro"))
        );
        // r11: retry-after 90 → "2 minutes"
        let out = retryable(
            &api_error(
                "nope",
                Some(404),
                true,
                Some(&[("retry-after", "90")]),
                Some(r#"{"error":"GoUsageLimitError","metadata":{"workspace":"ws-123"}}"#),
            ),
            "anthropic",
        )
        .unwrap();
        assert_eq!(
            out.action.as_ref().unwrap().message,
            go_message("2 minutes", None)
        );
        // r12: retry-after 90000 → "1 day 1 hour"
        let out = retryable(
            &api_error(
                "nope",
                Some(404),
                true,
                Some(&[("retry-after", "90000")]),
                Some(r#"{"error":"GoUsageLimitError","metadata":{"workspace":"ws-123"}}"#),
            ),
            "anthropic",
        )
        .unwrap();
        assert_eq!(
            out.action.as_ref().unwrap().message,
            go_message("1 day 1 hour", None)
        );
        // r13: retry-after 86400 → "1 day"
        let out = retryable(
            &api_error(
                "nope",
                Some(404),
                true,
                Some(&[("retry-after", "86400")]),
                Some(r#"{"error":"GoUsageLimitError","metadata":{"workspace":"ws-123"}}"#),
            ),
            "anthropic",
        )
        .unwrap();
        assert_eq!(
            out.action.as_ref().unwrap().message,
            go_message("1 day", None)
        );
        // r14: retry-after 172800 → "2 days"
        let out = retryable(
            &api_error(
                "nope",
                Some(404),
                true,
                Some(&[("retry-after", "172800")]),
                Some(r#"{"error":"GoUsageLimitError","metadata":{"workspace":"ws-123"}}"#),
            ),
            "anthropic",
        )
        .unwrap();
        assert_eq!(
            out.action.as_ref().unwrap().message,
            go_message("2 days", None)
        );
        // r15: retry-after 60 → "1 minute"
        let out = retryable(
            &api_error(
                "nope",
                Some(404),
                true,
                Some(&[("retry-after", "60")]),
                Some(r#"{"error":"GoUsageLimitError","metadata":{"workspace":"ws-123"}}"#),
            ),
            "anthropic",
        )
        .unwrap();
        assert_eq!(
            out.action.as_ref().unwrap().message,
            go_message("1 minute", None)
        );
        // r16: retry-after 3660 → "1 hour 1 minute"
        let out = retryable(
            &api_error(
                "nope",
                Some(404),
                true,
                Some(&[("retry-after", "3660")]),
                Some(r#"{"error":"GoUsageLimitError","metadata":{"workspace":"ws-123"}}"#),
            ),
            "anthropic",
        )
        .unwrap();
        assert_eq!(
            out.action.as_ref().unwrap().message,
            go_message("1 hour 1 minute", None)
        );
        // r17: retry-after 120 → "2 minutes"
        let out = retryable(
            &api_error(
                "nope",
                Some(404),
                true,
                Some(&[("retry-after", "120")]),
                Some(r#"{"error":"GoUsageLimitError","metadata":{"workspace":"ws-123"}}"#),
            ),
            "anthropic",
        )
        .unwrap();
        assert_eq!(
            out.action.as_ref().unwrap().message,
            go_message("2 minutes", None)
        );
        // r18: retry-after 0 → "less than a minute"; empty metadata
        let out = retryable(
            &api_error(
                "nope",
                Some(404),
                true,
                Some(&[("retry-after", "0")]),
                Some(r#"{"error":"GoUsageLimitError","metadata":{}}"#),
            ),
            "anthropic",
        )
        .unwrap();
        assert_eq!(
            out.action.as_ref().unwrap().message,
            go_message("less than a minute", None)
        );
        assert_eq!(
            out.message,
            format!(
                "{} - https://opencode.ai/workspace//go",
                go_message("less than a minute", None)
            )
        );
        // r19: garbage retry-after → empty resetIn
        let out = retryable(
            &api_error(
                "nope",
                Some(404),
                true,
                Some(&[("retry-after", "garbage")]),
                Some(r#"{"error":"GoUsageLimitError","metadata":{"workspace":"ws-123","limitName":"Pro"}}"#),
            ),
            "anthropic",
        )
        .unwrap();
        assert_eq!(
            out.action.as_ref().unwrap().message,
            go_message("", Some("Pro"))
        );
        // r20: ContextOverflowError never retried
        assert_eq!(
            retryable(
                &AssistantError::ContextOverflow {
                    message: "overflow".to_string(),
                    response_body: None,
                },
                "anthropic"
            ),
            None
        );
        // r21: too_many_requests
        assert_eq!(
            retryable(
                &AssistantError::Unknown {
                    message: "too_many_requests for user".to_string(),
                    r#ref: None,
                },
                "anthropic"
            ),
            Some(Retryable {
                message: "Too Many Requests".to_string(),
                action: None,
            })
        );
        // r22/r23: exhausted/unavailable → overloaded
        for message in ["Resource exhausted", "service unavailable"] {
            assert_eq!(
                retryable(
                    &AssistantError::Unknown {
                        message: message.to_string(),
                        r#ref: None,
                    },
                    "anthropic"
                ),
                Some(Retryable {
                    message: "Provider is overloaded".to_string(),
                    action: None,
                }),
                "{message}"
            );
        }
        // r24: totally fine message
        assert_eq!(
            retryable(
                &AssistantError::Unknown {
                    message: "Totally fine".to_string(),
                    r#ref: None,
                },
                "anthropic"
            ),
            None
        );
        // r25: 429 message passthrough
        assert_eq!(
            retryable(
                &AssistantError::Unknown {
                    message: "429 too many requests".to_string(),
                    r#ref: None,
                },
                "anthropic"
            ),
            Some(Retryable {
                message: "429 too many requests".to_string(),
                action: None,
            })
        );
        // r26: Aborted messages don't match (no generic NamedError path hit)
        assert_eq!(
            retryable(
                &AssistantError::Aborted {
                    message: "Aborted".to_string(),
                },
                "anthropic"
            ),
            None
        );
        // r27: OutputLength has no message data
        assert_eq!(
            retryable(&AssistantError::OutputLength {}, "anthropic"),
            None
        );
        // r28: plain connection message isn't retryable
        assert_eq!(
            retryable(
                &AssistantError::Unknown {
                    message: "connection reset by peer".to_string(),
                    r#ref: None,
                },
                "anthropic"
            ),
            None
        );
        // r29/r30: timeout messages match pattern 5
        for message in ["Read timed out", "request timeout exceeded"] {
            assert_eq!(
                retryable(
                    &AssistantError::Unknown {
                        message: message.to_string(),
                        r#ref: None,
                    },
                    "anthropic"
                ),
                Some(Retryable {
                    message: message.to_string(),
                    action: None,
                }),
                "{message}"
            );
        }
    }

    /// Retry-policy property test: fake clock + fake random, exact attempt
    /// counts, stop conditions, and `next` timestamps (retry.ts:183-207).
    #[test]
    fn policy_steps() {
        let policy = Policy::new("anthropic");
        let now: u64 = 1_700_000_000_000;
        let parse = |error: &AssistantError| error.clone();

        // Not retryable → Done immediately.
        let overflow = AssistantError::ContextOverflow {
            message: "too big".to_string(),
            response_body: None,
        };
        assert_eq!(policy.step(parse, &overflow, 1, now, 0.5), PolicyStep::Done);

        // Retryable → Retry with next = now + delay(1)
        let api = api_error("nope", None, true, None, None);
        assert_eq!(
            policy.step(parse, &api, 1, now, 0.5),
            PolicyStep::Retry {
                set: RetrySet {
                    attempt: 1,
                    message: "nope".to_string(),
                    action: None,
                    next: now + 2250,
                },
                wait_ms: 2250.0,
            }
        );

        // Header-driven delay feeds into next: retry-after-ms 15000.
        let api = api_error(
            "nope",
            None,
            true,
            Some(&[("retry-after-ms", "15000")]),
            None,
        );
        assert_eq!(
            policy.step(parse, &api, 3, now, 0.5),
            PolicyStep::Retry {
                set: RetrySet {
                    attempt: 3,
                    message: "nope".to_string(),
                    action: None,
                    next: now + 15000,
                },
                wait_ms: 15000.0,
            }
        );

        // Attempts 1..=5 retry, attempt 6 (RETRY_MAX_RETRIES + 1) is Done.
        let api = api_error("nope", None, true, None, None);
        let mut attempt = 1;
        while attempt <= RETRY_MAX_RETRIES {
            assert!(matches!(
                policy.step(parse, &api, attempt, now, 0.5),
                PolicyStep::Retry { .. }
            ));
            attempt += 1;
        }
        assert_eq!(
            policy.step(parse, &api, attempt, now, 0.5),
            PolicyStep::Done
        );
    }

    /// Driving the full schedule: statuses published and total sleep match
    /// the exponential ladder.
    #[test]
    fn policy_full_ladder() {
        let policy = Policy::new("anthropic");
        let mut now: u64 = 1_700_000_000_000;
        let api = api_error("nope", None, true, None, None);
        let mut total_sleep = 0.0;
        let mut published = Vec::new();
        for attempt in 1..=RETRY_MAX_RETRIES {
            let step = policy.step(|e: &AssistantError| e.clone(), &api, attempt, now, 0.0);
            let PolicyStep::Retry { set, wait_ms } = step else {
                panic!("expected retry at attempt {attempt}");
            };
            published.push((set.attempt, set.message.clone()));
            assert_eq!(set.next, now + wait_ms as u64);
            total_sleep += wait_ms;
            now += wait_ms as u64;
        }
        let expected: Vec<(u64, String)> = (1..=5)
            .map(|attempt| (attempt, "nope".to_string()))
            .collect();
        assert_eq!(published, expected);
        // exponential(attempt, random=0) with jitter factor 0.25 → ceil(base);
        // attempt 5 (32000) hits the RETRY_MAX_DELAY_NO_HEADERS cap
        assert_eq!(total_sleep, 2000.0 + 4000.0 + 8000.0 + 16000.0 + 30000.0);

        // 6th attempt stops the ladder even though the error is retryable.
        assert_eq!(
            policy.step(|e: &AssistantError| e.clone(), &api, 6, now, 0.0),
            PolicyStep::Done
        );
    }

    #[test]
    fn parse_float_prefix() {
        assert_eq!(js_parse_float("12abc"), Some(12.0));
        assert_eq!(js_parse_float("0x10"), Some(0.0));
        assert_eq!(js_parse_float("1e"), Some(1.0));
        assert_eq!(js_parse_float("1e3"), Some(1000.0));
        assert_eq!(js_parse_float("  2.5x"), Some(2.5));
        assert_eq!(js_parse_float("-5"), Some(-5.0));
        assert_eq!(js_parse_float("+7.25"), Some(7.25));
        assert_eq!(js_parse_float(".5e2"), Some(50.0));
        assert_eq!(js_parse_float("Infinity"), Some(f64::INFINITY));
        assert_eq!(js_parse_float("-Infinity"), Some(f64::NEG_INFINITY));
        assert_eq!(js_parse_float("garbage"), None);
        assert_eq!(js_parse_float("e5"), None);
        assert_eq!(js_parse_float(""), None);
    }

    #[test]
    fn date_parse_iso_fallback() {
        // RFC 2823 ISO fallback still yields plausible epoch ms.
        assert!(date_parse_ms("2026-09-17T00:00:00Z").is_some());
        assert!(date_parse_ms("2026-09-17").is_some());
        assert!(date_parse_ms("garbage").is_none());
    }
}
