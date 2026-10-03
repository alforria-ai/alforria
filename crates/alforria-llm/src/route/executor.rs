//! HTTP executor with status classification, redaction, and retry.
//!
//! Port of `route/executor.ts` (M2.5). `RequestExecutor` owns a
//! [`reqwest::Client`] and exposes one method, [`RequestExecutor::execute`],
//! which runs a prepared HTTP request and returns either a streaming success
//! response or an [`LlmError`] carrying redacted HTTP diagnostics. Retryable
//! failures (`RateLimit`, `ProviderInternal`) are retried with jittered
//! exponential backoff, honoring `Retry-After` headers.

use std::collections::BTreeMap;
use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use regex::Regex;

use crate::provider_error::is_context_overflow;
use crate::schema::errors::{
    AuthKind, HttpContext, HttpRateLimitDetails, HttpRequestDetails, HttpResponseDetails, LlmError,
    LlmErrorReason, ProviderFailureClassification,
};

const BODY_LIMIT: usize = 16_384;
pub const MAX_RETRIES: usize = 2;
pub const BASE_DELAY_MS: u64 = 500;
pub const MAX_DELAY_MS: u64 = 10_000;
const REDACTED: &str = "<redacted>";

// One source of truth for what counts as a sensitive name across headers,
// URL query keys, and field names embedded inside request/response bodies.
// `SENSITIVE_NAME` is used as both a substring matcher (for free-form header
// names like `Authorization` / `X-API-Key`) and as the body-field alternation
// list. `SHORT_QUERY_NAME` covers anchored short keys like `?key=…` / `?sig=…`
// that are too generic to redact substring-style without false positives.
const SENSITIVE_NAME_SOURCE: &str = "authorization|api[-_]?key|access[-_]?token|refresh[-_]?token|id[-_]?token|token|secret|credential|signature|x-amz-signature";

static SENSITIVE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("(?i){SENSITIVE_NAME_SOURCE}")).unwrap());
static SHORT_QUERY_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new("(?i)^(key|sig)$").unwrap());
static REDACT_JSON_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        "(?i)(\"(?:{SENSITIVE_NAME_SOURCE}|key)\"\\s*:\\s*)\"[^\"]*\""
    ))
    .unwrap()
});
static REDACT_QUERY_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        "(?i)((?:{SENSITIVE_NAME_SOURCE}|key)=)[^&\\s\\\"]+"
    ))
    .unwrap()
});
static BEARER: LazyLock<Regex> = LazyLock::new(|| Regex::new("(?i)^Bearer\\s+(.+)$").unwrap());
static CONTENT_POLICY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("(?i)content[-_\\s]?policy|content_filter|safety").unwrap());
static QUOTA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("(?i)insufficient[-_\\s]?quota|quota[-_\\s]?exceeded").unwrap());

/// Retry knob set for the executor (defaults: 2 retries, 500 ms base, 10 s cap).
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: usize,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: MAX_RETRIES,
            base_delay_ms: BASE_DELAY_MS,
            max_delay_ms: MAX_DELAY_MS,
        }
    }
}

/// Injectable sleep for the retry loop. Production wires [`TokioSleeper`];
/// tests record the requested delays instead of sleeping.
pub trait Sleeper {
    fn sleep(&self, ms: u64) -> impl Future<Output = ()>;
}

/// Production [`Sleeper`] backed by `tokio::time::sleep`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioSleeper;

impl Sleeper for TokioSleeper {
    fn sleep(&self, ms: u64) -> impl Future<Output = ()> {
        tokio::time::sleep(std::time::Duration::from_millis(ms))
    }
}

/// Injectable randomness for the jittered backoff delay.
pub trait Jitter {
    /// Uniformly distributed value in `[lo, hi]`.
    fn next_between(&self, lo: f64, hi: f64) -> f64;
}

/// In-crate xorshift64-backed [`Jitter`] (no external RNG dependency).
#[derive(Debug)]
pub struct XorshiftJitter {
    state: AtomicU64,
}

impl XorshiftJitter {
    pub fn new(seed: u64) -> Self {
        Self {
            state: AtomicU64::new(seed | 1),
        }
    }

    fn next_u64(&self) -> u64 {
        let mut current = self.state.load(Ordering::Relaxed);
        loop {
            let mut x = current;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            match self
                .state
                .compare_exchange(current, x, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return x,
                Err(observed) => current = observed,
            }
        }
    }
}

impl Default for XorshiftJitter {
    fn default() -> Self {
        // Arbitrary nonzero seed.
        Self::new(0x9E37_79B9_7F4A_7C15)
    }
}

impl Jitter for XorshiftJitter {
    fn next_between(&self, lo: f64, hi: f64) -> f64 {
        // 53 uniform bits in [0, 1).
        let r = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        lo + r * (hi - lo)
    }
}

/// A prepared HTTP request handed to the executor by the route pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Successful (non-error-status) HTTP response. The body is still streaming;
/// consume it via [`HttpResponse::into_inner`] (reqwest response).
#[derive(Debug)]
pub struct HttpResponse {
    response: reqwest::Response,
}

impl HttpResponse {
    pub fn status(&self) -> u16 {
        self.response.status().as_u16()
    }

    pub fn headers(&self) -> &reqwest::header::HeaderMap {
        self.response.headers()
    }

    pub fn into_inner(self) -> reqwest::Response {
        self.response
    }
}

/// HTTP executor: sends prepared requests, classifies error statuses into
/// [`LlmError`] values (with redacted diagnostics), and retries retryable
/// failures with jittered exponential backoff.
pub struct RequestExecutor<S = TokioSleeper, J = XorshiftJitter> {
    client: reqwest::Client,
    retry: RetryConfig,
    sleeper: Arc<S>,
    jitter: Arc<J>,
}

impl RequestExecutor<TokioSleeper, XorshiftJitter> {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            retry: RetryConfig::default(),
            sleeper: Arc::new(TokioSleeper),
            jitter: Arc::new(XorshiftJitter::default()),
        }
    }
}

impl Default for RequestExecutor<TokioSleeper, XorshiftJitter> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sleeper, J: Jitter> RequestExecutor<S, J> {
    pub fn with_parts(
        client: reqwest::Client,
        retry: RetryConfig,
        sleeper: Arc<S>,
        jitter: Arc<J>,
    ) -> Self {
        Self {
            client,
            retry,
            sleeper,
            jitter,
        }
    }

    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    pub fn with_retry_config(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Execute a prepared request, retrying retryable failures.
    pub async fn execute(&self, prepared: &PreparedRequest) -> Result<HttpResponse, LlmError> {
        self.retry_status_failures(|| self.execute_once(prepared))
            .await
    }

    /// Single HTTP attempt: send, then classify non-2xx statuses.
    async fn execute_once(&self, prepared: &PreparedRequest) -> Result<HttpResponse, LlmError> {
        let response = self.send(prepared).await?;
        self.check_status(prepared, response).await
    }

    async fn send(&self, prepared: &PreparedRequest) -> Result<reqwest::Response, LlmError> {
        let method = match reqwest::Method::from_str(&prepared.method) {
            Ok(method) => method,
            Err(error) => {
                return Err(self.transport_error(prepared, ENCODE_ERROR, &error.to_string()))
            }
        };
        let mut builder = self.client.request(method, &prepared.url);
        for (name, value) in &prepared.headers {
            builder = builder.header(name, value);
        }
        let request = match builder.body(prepared.body.clone()).build() {
            Ok(request) => request,
            Err(error) => {
                return Err(self.transport_error(prepared, ENCODE_ERROR, &error.to_string()))
            }
        };
        match self.client.execute(request).await {
            Ok(response) => Ok(response),
            Err(error) => {
                let kind = if error.is_timeout() {
                    "Timeout"
                } else if error.is_connect() {
                    "ConnectError"
                } else if error.is_request() {
                    "RequestError"
                } else if error.is_redirect() {
                    "RedirectError"
                } else if error.is_body() {
                    "BodyError"
                } else if error.is_decode() {
                    "DecodeError"
                } else {
                    "TransportError"
                };
                Err(self.transport_error(prepared, kind, &error_chain(&error)))
            }
        }
    }

    fn transport_error(&self, prepared: &PreparedRequest, kind: &str, message: &str) -> LlmError {
        // Transport error messages may embed the request URL, which can carry
        // query secrets (e.g. reqwest's Display includes the full URL); the
        // message therefore goes through the same redaction as bodies.
        let message = redact_body(message, prepared);
        LlmError {
            module: "RequestExecutor".to_string(),
            method: "execute".to_string(),
            reason: LlmErrorReason::Transport {
                message,
                kind: Some(kind.to_string()),
                url: Some(redact_url(&prepared.url)),
                http: Some(HttpContext {
                    request: request_details(prepared),
                    response: None,
                    body: None,
                    body_truncated: None,
                    request_id: None,
                    rate_limit: None,
                }),
            },
        }
    }

    /// Turn `>= 400` responses into classified [`LlmError`] values carrying
    /// redacted HTTP diagnostics.
    async fn check_status(
        &self,
        prepared: &PreparedRequest,
        response: reqwest::Response,
    ) -> Result<HttpResponse, LlmError> {
        let status = response.status().as_u16();
        if status < 400 {
            return Ok(HttpResponse { response });
        }
        let response_headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_lowercase(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect::<Vec<(String, String)>>();
        let body = response.text().await.ok();
        let headers = normalized_headers(&response_headers);
        let retry_after = retry_after_ms(&headers);
        let rate_limit = rate_limit_details(&headers, retry_after);
        let details = response_body(body.as_deref(), prepared);
        let message = provider_message(status, &details);
        let http = HttpContext {
            request: request_details(prepared),
            response: Some(HttpResponseDetails {
                status: status as f64,
                headers: redact_headers(&response_headers),
            }),
            body: details.body,
            body_truncated: details.body_truncated,
            request_id: request_id(&headers),
            rate_limit,
        };
        let reason = status_reason(StatusReasonInput {
            status,
            message,
            retry_after_ms: retry_after,
            rate_limit: http.rate_limit.clone(),
            http,
        });
        Err(LlmError {
            module: "RequestExecutor".to_string(),
            method: "execute".to_string(),
            reason,
        })
    }

    /// Retry loop (port of `retryStatusFailures`): retries `effect` while the
    /// error is retryable and retries remain, sleeping between attempts.
    pub async fn retry_status_failures<F, Fut, A>(&self, mut effect: F) -> Result<A, LlmError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<A, LlmError>>,
    {
        let mut retries = self.retry.max_retries;
        let mut attempt = 0usize;
        loop {
            match effect().await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if !error.retryable() || retries == 0 {
                        return Err(error);
                    }
                    let delay = self.retry_delay(&error, attempt);
                    self.sleeper.sleep(delay).await;
                    retries -= 1;
                    attempt += 1;
                }
            }
        }
    }

    fn retry_delay(&self, error: &LlmError, attempt: usize) -> u64 {
        let max_delay_ms = self.retry.max_delay_ms as f64;
        if let Some(retry_after_ms) = error.retry_after_ms() {
            return retry_after_ms.min(max_delay_ms).round() as u64;
        }
        let factor = 2f64.powi(attempt as i32);
        let lo = (self.retry.base_delay_ms as f64 * factor * 0.8).min(max_delay_ms);
        let hi = (self.retry.base_delay_ms as f64 * factor * 1.2).min(max_delay_ms);
        self.jitter.next_between(lo, hi).round() as u64
    }
}

pub struct StatusReasonInput {
    pub status: u16,
    pub message: String,
    pub retry_after_ms: Option<f64>,
    pub rate_limit: Option<HttpRateLimitDetails>,
    pub http: HttpContext,
}

/// Status → reason classification (exact TS order: content-policy body check
/// first, then 401/403/429, invalid-request statuses, then server errors).
pub fn status_reason(input: StatusReasonInput) -> LlmErrorReason {
    let body = input.http.body.as_deref().unwrap_or("");
    if CONTENT_POLICY.is_match(body) {
        return LlmErrorReason::ContentPolicy {
            message: input.message,
            provider_metadata: None,
            http: Some(input.http),
        };
    }
    let status = input.status;
    if status == 401 {
        return LlmErrorReason::Authentication {
            message: input.message,
            kind: AuthKind::Invalid,
            provider_metadata: None,
            http: Some(input.http),
        };
    }
    if status == 403 {
        return LlmErrorReason::Authentication {
            message: input.message,
            kind: AuthKind::InsufficientPermissions,
            provider_metadata: None,
            http: Some(input.http),
        };
    }
    if status == 429 {
        if QUOTA.is_match(body) {
            return LlmErrorReason::QuotaExceeded {
                message: input.message,
                provider_metadata: None,
                http: Some(input.http),
            };
        }
        return LlmErrorReason::RateLimit {
            message: input.message,
            retry_after_ms: input.retry_after_ms,
            rate_limit: input.rate_limit,
            provider_metadata: None,
            http: Some(input.http),
        };
    }
    if matches!(status, 400 | 404 | 409 | 413 | 422) {
        return LlmErrorReason::InvalidRequest {
            message: input.message,
            parameter: None,
            classification: if is_context_overflow(body) {
                Some(ProviderFailureClassification::ContextOverflow)
            } else {
                None
            },
            provider_metadata: None,
            http: Some(input.http),
        };
    }
    if status >= 500 || retryable_status(status) {
        return LlmErrorReason::ProviderInternal {
            message: input.message,
            status: status as f64,
            retry_after_ms: input.retry_after_ms,
            provider_metadata: None,
            http: Some(input.http),
        };
    }
    LlmErrorReason::UnknownProvider {
        message: input.message,
        status: Some(status as f64),
        provider_metadata: None,
        http: Some(input.http),
    }
}

/// Transport `kind` of a request that could not be built (bad method, URL
/// or header) — it never reached the network, unlike every other kind.
pub const ENCODE_ERROR: &str = "EncodeError";

/// The causes behind a reqwest error, outermost first. reqwest's own
/// `Display` is only "error sending request for url (…)", which repeats
/// the URL and hides why the request failed ("tcp connect error:
/// Connection refused (os error 111)").
pub(crate) fn error_chain(error: &reqwest::Error) -> String {
    let mut causes = Vec::new();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        causes.push(cause.to_string());
        source = cause.source();
    }
    if causes.is_empty() {
        error.to_string()
    } else {
        causes.join(": ")
    }
}

/// 429 | 503 | 504 | 529.
fn retryable_status(status: u16) -> bool {
    matches!(status, 429 | 503 | 504 | 529)
}

fn is_sensitive_header_name(name: &str) -> bool {
    SENSITIVE_NAME.is_match(name)
}

fn is_sensitive_query_name(name: &str) -> bool {
    is_sensitive_header_name(name) || SHORT_QUERY_NAME.is_match(name)
}

fn normalized_headers(headers: &[(String, String)]) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| (name.to_lowercase(), value.clone()))
        .collect()
}

fn redact_headers(headers: &[(String, String)]) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| {
            let redacted = SENSITIVE_NAME.is_match(name);
            (
                name.to_lowercase(),
                if redacted {
                    REDACTED.to_string()
                } else {
                    value.clone()
                },
            )
        })
        .collect()
}

fn request_details(request: &PreparedRequest) -> HttpRequestDetails {
    HttpRequestDetails {
        method: request.method.clone(),
        url: redact_url(&request.url),
        headers: redact_headers(&request.headers),
    }
}

fn request_id(headers: &BTreeMap<String, String>) -> Option<String> {
    [
        "x-request-id",
        "request-id",
        "x-amzn-requestid",
        "x-amz-request-id",
        "x-goog-request-id",
        "cf-ray",
    ]
    .iter()
    .find_map(|name| headers.get(*name).cloned())
}

/// JS `Number(string)` subset: trimmed decimal/float parse; `""` → 0.
fn js_number(value: &str) -> Option<f64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Some(0.0);
    }
    trimmed.parse::<f64>().ok()
}

/// Hand-rolled RFC 1123 HTTP-date parse ("Sun, 06 Nov 1994 08:49:37 GMT") →
/// epoch milliseconds.
fn parse_http_date(value: &str) -> Option<i64> {
    // "Day, DD Mon YYYY HH:MM:SS GMT"
    let rest = value.split_once(", ").map(|(_, rest)| rest)?;
    let rest = rest
        .strip_suffix(" GMT")
        .or_else(|| rest.strip_suffix(" UTC"))
        .unwrap_or(rest);
    let mut fields = rest.split_whitespace();
    let day: i64 = fields.next()?.parse().ok()?;
    let month = match fields.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = fields.next()?.parse().ok()?;
    let mut time = fields.next()?.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: i64 = time.next()?.parse().ok()?;
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days from 1970-01-01 for a civil date (Howard E. Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// `retry-after-ms` header first; then `retry-after` as numeric seconds; then
/// HTTP-date. Clamped to `>= 0`.
fn retry_after_ms(headers: &BTreeMap<String, String>) -> Option<f64> {
    if let Some(raw) = headers.get("retry-after-ms") {
        if let Some(millis) = js_number(raw) {
            return Some(millis.max(0.0));
        }
    }
    let raw = headers.get("retry-after")?;
    if let Some(seconds) = js_number(raw) {
        return Some((seconds * 1000.0).max(0.0));
    }
    let date = parse_http_date(raw)?;
    Some((date * 1000 - now_millis()).max(0) as f64)
}

fn add_rate_limit_value(target: &mut BTreeMap<String, String>, key: &str, value: &str) {
    if !key.is_empty() {
        target.insert(key.to_string(), value.to_string());
    }
}

/// `x-ratelimit-{limit,remaining,reset}-*` and `anthropic-ratelimit-*-{…}`
/// header harvesting.
fn rate_limit_details(
    headers: &BTreeMap<String, String>,
    retry_after: Option<f64>,
) -> Option<HttpRateLimitDetails> {
    let mut limit = BTreeMap::new();
    let mut remaining = BTreeMap::new();
    let mut reset = BTreeMap::new();

    for (name, value) in headers {
        if let Some(suffix) = name.strip_prefix("x-ratelimit-limit-") {
            add_rate_limit_value(&mut limit, suffix, value);
        } else if let Some(suffix) = name.strip_prefix("x-ratelimit-remaining-") {
            add_rate_limit_value(&mut remaining, suffix, value);
        } else if let Some(suffix) = name.strip_prefix("x-ratelimit-reset-") {
            add_rate_limit_value(&mut reset, suffix, value);
        } else if let Some(rest) = name.strip_prefix("anthropic-ratelimit-") {
            let Some((bucket, kind)) = rest.rsplit_once('-') else {
                continue;
            };
            match kind {
                "limit" => add_rate_limit_value(&mut limit, bucket, value),
                "remaining" => add_rate_limit_value(&mut remaining, bucket, value),
                "reset" => add_rate_limit_value(&mut reset, bucket, value),
                _ => {}
            }
        }
    }

    if retry_after.is_none() && limit.is_empty() && remaining.is_empty() && reset.is_empty() {
        return None;
    }

    Some(HttpRateLimitDetails {
        retry_after_ms: retry_after,
        limit: (!limit.is_empty()).then_some(limit),
        remaining: (!remaining.is_empty()).then_some(remaining),
        reset: (!reset.is_empty()).then_some(reset),
    })
}

struct ResponseBody {
    body: Option<String>,
    body_truncated: Option<bool>,
}

fn response_body(body: Option<&str>, request: &PreparedRequest) -> ResponseBody {
    let Some(body) = body else {
        return ResponseBody {
            body: None,
            body_truncated: None,
        };
    };
    let redacted = redact_body(body, request);
    if redacted.len() <= BODY_LIMIT {
        ResponseBody {
            body: Some(redacted),
            body_truncated: None,
        }
    } else {
        // Truncate at BODY_LIMIT without splitting a UTF-8 code point.
        let mut end = BODY_LIMIT;
        while !redacted.is_char_boundary(end) {
            end -= 1;
        }
        ResponseBody {
            body: Some(redacted[..end].to_string()),
            body_truncated: Some(true),
        }
    }
}

fn provider_message(status: u16, details: &ResponseBody) -> String {
    // `body.body &&` (executor.ts:205) — empty strings drop the suffix.
    match &details.body {
        Some(body) if !body.is_empty() && body.len() <= 500 => {
            format!("Provider request failed with HTTP {status}: {body}")
        }
        _ => format!("Provider request failed with HTTP {status}"),
    }
}

fn can_parse_url(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return false;
    }
    !rest.is_empty()
}

/// Redact sensitive query values (`api_key`, `key`, `sig`, …) from the URL.
fn redact_url(value: &str) -> String {
    if !can_parse_url(value) {
        return REDACTED.to_string();
    }
    let Some((prefix, query)) = value.split_once('?') else {
        return value.to_string();
    };
    // Split any fragment off before processing pairs.
    let (query, fragment) = match query.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (query, None),
    };
    let redacted: Vec<String> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) if is_sensitive_query_name(&percent_decode(key)) => {
                format!("{key}=%3Credacted%3E")
            }
            Some(_) => pair.to_string(),
            None if is_sensitive_query_name(&percent_decode(pair)) => {
                format!("{pair}=%3Credacted%3E")
            }
            None => pair.to_string(),
        })
        .collect();
    let mut out = format!("{prefix}?{}", redacted.join("&"));
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                if let (Some(high), Some(low)) =
                    (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
                {
                    out.push(high * 16 + low);
                    index += 3;
                } else {
                    out.push(b'%');
                    index += 1;
                }
            }
            _ => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// JS `encodeURIComponent` (unreserved set: `A-Za-z0-9` + `-_.!~*'()`).
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(*byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Secrets sent in the request (sensitive headers and URL query values) that
/// may be echoed back by the provider.
fn secret_values(request: &PreparedRequest) -> Vec<String> {
    let mut values: Vec<String> = Vec::new();
    fn add(values: &mut Vec<String>, value: &str) {
        if value.chars().count() < 4 {
            return;
        }
        if !values.iter().any(|existing| existing == value) {
            values.push(value.to_string());
        }
        let encoded = encode_uri_component(value);
        if !values.iter().any(|existing| existing == &encoded) {
            values.push(encoded);
        }
    }

    for (name, value) in &request.headers {
        if !is_sensitive_header_name(name) {
            continue;
        }
        add(&mut values, value);
        if let Some(bearer) = BEARER.captures(value) {
            add(&mut values, &bearer[1]);
        }
    }

    if can_parse_url(&request.url) {
        if let Some((_, query)) = request.url.split_once('?') {
            for pair in query.split('&') {
                if pair.is_empty() {
                    continue;
                }
                match pair.split_once('=') {
                    Some((key, value)) if is_sensitive_query_name(&percent_decode(key)) => {
                        add(&mut values, &percent_decode(value));
                    }
                    None if is_sensitive_query_name(&percent_decode(pair)) => {
                        add(&mut values, "");
                    }
                    _ => {}
                }
            }
        }
    }
    values
}

/// Two passes: structural (redact `"name": "value"` and `name=value` patterns
/// for any field name that looks sensitive) plus literal (replace any actual
/// secret values we sent in the request, in case the response echoes one back).
fn redact_body(body: &str, request: &PreparedRequest) -> String {
    let structural = REDACT_JSON_FIELD.replace_all(body, |captures: &regex::Captures| {
        format!("{}\"{REDACTED}\"", &captures[1])
    });
    let structural = REDACT_QUERY_FIELD.replace_all(&structural, |captures: &regex::Captures| {
        format!("{}{REDACTED}", &captures[1])
    });
    let mut out = structural.into_owned();
    for secret in secret_values(request) {
        if secret.is_empty() {
            continue;
        }
        out = out.replace(&secret, REDACTED);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    use super::*;

    // ---- test HTTP server (canned responses, connection counting) ----

    struct TestServer {
        addr: String,
        attempts: std::sync::Arc<AtomicUsize>,
    }

    impl TestServer {
        fn attempts(&self) -> usize {
            self.attempts.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Canned HTTP response: `(status, reason, headers, body)`.
    type CannedResponse = (u16, &'static str, Vec<(&'static str, &'static str)>, String);

    fn test_server(responses: Vec<CannedResponse>) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = std::sync::Arc::new(AtomicUsize::new(0));
        let attempts_handle = attempts.clone();
        let responses = std::sync::Arc::new(Mutex::new(responses));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(_) => break,
                };
                if read_request(&mut stream).is_err() {
                    continue;
                }
                attempts_handle.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = {
                    let mut queue = responses.lock().unwrap();
                    if queue.len() > 1 {
                        queue.remove(0)
                    } else {
                        queue[0].clone()
                    }
                };
                write_response(&mut stream, &response);
            }
        });
        TestServer {
            addr: format!("http://{addr}"),
            attempts,
        }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
        let mut buffer = [0u8; 4096];
        let mut seen = 0usize;
        loop {
            let chunk = stream.read(&mut buffer[seen..])?;
            if chunk == 0 {
                return Ok(());
            }
            seen += chunk;
            let text = String::from_utf8_lossy(&buffer[..seen]).into_owned();
            if let Some(header_end) = text.find("\r\n\r\n") {
                let content_length = text
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.trim().eq_ignore_ascii_case("content-length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                if seen >= header_end + 4 + content_length {
                    return Ok(());
                }
            }
        }
    }

    fn write_response(stream: &mut std::net::TcpStream, response: &CannedResponse) {
        let (status, reason, headers, body) = response;
        let mut raw = format!("HTTP/1.1 {status} {reason}\r\n");
        raw.push_str("Connection: close\r\n");
        for (name, value) in headers {
            raw.push_str(name);
            raw.push_str(": ");
            raw.push_str(value);
            raw.push_str("\r\n");
        }
        raw.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        raw.push_str(body);
        let _ = stream.write_all(raw.as_bytes());
        let _ = stream.flush();
    }

    // ---- fakes ----

    #[derive(Default)]
    struct FakeSleeper(Mutex<Vec<u64>>);

    impl FakeSleeper {
        fn delays(&self) -> Vec<u64> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Sleeper for FakeSleeper {
        fn sleep(&self, ms: u64) -> impl Future<Output = ()> {
            self.0.lock().unwrap().push(ms);
            std::future::ready(())
        }
    }

    struct FakeJitter(f64);

    impl Jitter for FakeJitter {
        fn next_between(&self, lo: f64, hi: f64) -> f64 {
            lo + self.0 * (hi - lo)
        }
    }

    fn executor(sleeper: std::sync::Arc<FakeSleeper>) -> RequestExecutor<FakeSleeper, FakeJitter> {
        RequestExecutor::with_parts(
            reqwest::Client::new(),
            RetryConfig::default(),
            sleeper,
            std::sync::Arc::new(FakeJitter(0.5)),
        )
    }

    fn prepared(url: &str) -> PreparedRequest {
        PreparedRequest {
            method: "POST".to_string(),
            url: url.to_string(),
            headers: Vec::new(),
            body: String::new(),
        }
    }

    // ---- acceptance: retry loop ----

    #[tokio::test]
    async fn retries_rate_limit_then_succeeds_with_two_attempts() {
        let server = test_server(vec![
            (
                429,
                "Too Many Requests",
                vec![("retry-after-ms", "0")],
                "rate limited".to_string(),
            ),
            (200, "OK", vec![], "ok".to_string()),
        ]);
        let sleeper = std::sync::Arc::new(FakeSleeper::default());
        let response = executor(sleeper.clone())
            .execute(&prepared(&format!("{}/v1/chat", server.addr)))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(server.attempts(), 2);
        assert_eq!(sleeper.delays(), vec![0]);
    }

    #[tokio::test]
    async fn connection_refused_is_a_transport_error_carrying_the_cause() {
        // Bind then drop: nothing listens on the port any more.
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let sleeper = std::sync::Arc::new(FakeSleeper::default());
        let error = executor(sleeper.clone())
            .execute(&prepared(&format!("http://{addr}/v1/chat")))
            .await
            .unwrap_err();
        let LlmErrorReason::Transport {
            message, kind, url, ..
        } = &error.reason
        else {
            panic!("expected a transport error, got {:?}", error.reason);
        };
        assert_eq!(kind.as_deref(), Some("ConnectError"));
        assert!(message.contains("Connection refused"), "{message}");
        assert_eq!(
            url.as_deref(),
            Some(format!("http://{addr}/v1/chat").as_str())
        );
        assert!(!error.retryable());
        assert_eq!(sleeper.delays(), Vec::<u64>::new());
    }

    #[tokio::test]
    async fn unbuildable_request_is_an_encode_error() {
        let mut request = prepared("http://127.0.0.1:9/v1/chat");
        request
            .headers
            .push(("bad header".to_string(), "x".to_string()));
        let error = executor(std::sync::Arc::new(FakeSleeper::default()))
            .execute(&request)
            .await
            .unwrap_err();
        assert!(matches!(
            &error.reason,
            LlmErrorReason::Transport { kind, .. } if kind.as_deref() == Some(ENCODE_ERROR)
        ));
    }

    #[tokio::test]
    async fn does_not_retry_400_responses() {
        let server = test_server(vec![
            (400, "Bad Request", vec![], "invalid parameter".to_string()),
            (200, "OK", vec![], "should not retry".to_string()),
        ]);
        let sleeper = std::sync::Arc::new(FakeSleeper::default());
        let error = executor(sleeper.clone())
            .execute(&prepared(&format!("{}/v1/chat", server.addr)))
            .await
            .unwrap_err();
        assert!(matches!(
            error.reason,
            LlmErrorReason::InvalidRequest { .. }
        ));
        assert!(!error.retryable());
        assert_eq!(server.attempts(), 1);
        assert_eq!(sleeper.delays(), Vec::<u64>::new());
    }

    #[tokio::test]
    async fn third_consecutive_rate_limit_exhausts_max_retries() {
        let server = test_server(vec![
            (
                429,
                "Too Many Requests",
                vec![("retry-after-ms", "0")],
                "rate limited".to_string(),
            ),
            (
                429,
                "Too Many Requests",
                vec![("retry-after-ms", "0")],
                "rate limited".to_string(),
            ),
            (
                429,
                "Too Many Requests",
                vec![("retry-after-ms", "0")],
                "rate limited".to_string(),
            ),
        ]);
        let sleeper = std::sync::Arc::new(FakeSleeper::default());
        let error = executor(sleeper.clone())
            .execute(&prepared(&format!("{}/v1/chat", server.addr)))
            .await
            .unwrap_err();
        assert!(matches!(error.reason, LlmErrorReason::RateLimit { .. }));
        assert_eq!(server.attempts(), 3);
    }

    #[tokio::test]
    async fn jittered_delay_without_retry_after_is_within_bounds() {
        let server = test_server(vec![
            (429, "Too Many Requests", vec![], "rate limited".to_string()),
            (200, "OK", vec![], "ok".to_string()),
        ]);
        // FakeJitter(0.5) → midpoint of [500 * 2^0 * 0.8, 500 * 2^0 * 1.2].
        let sleeper = std::sync::Arc::new(FakeSleeper::default());
        executor(sleeper.clone())
            .execute(&prepared(&format!("{}/v1/chat", server.addr)))
            .await
            .unwrap();
        assert_eq!(sleeper.delays(), vec![500]);
    }

    #[tokio::test]
    async fn retry_after_header_drives_the_delay_and_caps_at_max() {
        let server = test_server(vec![
            (
                503,
                "Service Unavailable",
                vec![("retry-after-ms", "60000")],
                "busy".to_string(),
            ),
            (200, "OK", vec![], "ok".to_string()),
        ]);
        let sleeper = std::sync::Arc::new(FakeSleeper::default());
        executor(sleeper.clone())
            .execute(&prepared(&format!("{}/v1/chat", server.addr)))
            .await
            .unwrap();
        assert_eq!(sleeper.delays(), vec![10_000]);
    }

    #[tokio::test]
    async fn quota_exceeded_is_not_retryable() {
        let server = test_server(vec![(
            429,
            "Too Many Requests",
            vec![],
            "{\"error\": {\"type\": \"insufficient_quota\"}}".to_string(),
        )]);
        let sleeper = std::sync::Arc::new(FakeSleeper::default());
        let error = executor(sleeper.clone())
            .execute(&prepared(&format!("{}/v1/chat", server.addr)))
            .await
            .unwrap_err();
        assert!(matches!(error.reason, LlmErrorReason::QuotaExceeded { .. }));
        assert!(!error.retryable());
        assert_eq!(server.attempts(), 1);
        assert_eq!(sleeper.delays(), Vec::<u64>::new());
    }

    #[tokio::test]
    async fn returns_redacted_diagnostics_for_retryable_rate_limits() {
        let rate_limited = (
            429,
            "Too Many Requests",
            vec![
                ("retry-after-ms", "0"),
                ("x-request-id", "req_123"),
                ("x-api-key", "secret"),
            ],
            "rate limited".to_string(),
        );
        let server = test_server(vec![
            rate_limited.clone(),
            rate_limited.clone(),
            rate_limited,
        ]);
        let sleeper = std::sync::Arc::new(FakeSleeper::default());
        let request = PreparedRequest {
            method: "POST".to_string(),
            url: format!("{}/v1/chat?api_key=secret&key=secret&debug=1", server.addr),
            headers: vec![
                ("authorization".to_string(), "Bearer secret".to_string()),
                ("x-safe".to_string(), "visible".to_string()),
            ],
            body: String::new(),
        };
        let error = executor(sleeper.clone())
            .execute(&request)
            .await
            .unwrap_err();
        assert_eq!(server.attempts(), 3);
        let LlmErrorReason::RateLimit {
            retry_after_ms,
            rate_limit,
            http: Some(http),
            ..
        } = &error.reason
        else {
            panic!("expected RateLimit, got {:?}", error.reason);
        };
        assert_eq!(*retry_after_ms, Some(0.0));
        assert_eq!(rate_limit.as_ref().unwrap().retry_after_ms, Some(0.0));
        assert_eq!(http.request_id.as_deref(), Some("req_123"));
        assert_eq!(
            http.request.url,
            format!(
                "{}/v1/chat?api_key=%3Credacted%3E&key=%3Credacted%3E&debug=1",
                server.addr
            )
        );
        assert_eq!(
            http.request.headers.get("authorization"),
            Some(&REDACTED.to_string())
        );
        assert_eq!(
            http.request.headers.get("x-safe"),
            Some(&"visible".to_string())
        );
        assert_eq!(http.response.as_ref().unwrap().status, 429.0);
        assert_eq!(
            http.response.as_ref().unwrap().headers.get("x-api-key"),
            Some(&REDACTED.to_string())
        );
        assert_eq!(http.body.as_deref(), Some("rate limited"));
    }

    #[tokio::test]
    async fn truncates_large_error_bodies_without_retrying() {
        let server = test_server(vec![
            (401, "Unauthorized", vec![], "x".repeat(20_000)),
            (200, "OK", vec![], "should not retry".to_string()),
        ]);
        let sleeper = std::sync::Arc::new(FakeSleeper::default());
        let error = executor(sleeper.clone())
            .execute(&prepared(&format!("{}/v1/chat", server.addr)))
            .await
            .unwrap_err();
        assert!(matches!(
            error.reason,
            LlmErrorReason::Authentication { .. }
        ));
        assert!(!error.retryable());
        let LlmErrorReason::Authentication {
            http: Some(http), ..
        } = &error.reason
        else {
            panic!();
        };
        assert_eq!(http.body_truncated, Some(true));
        assert_eq!(http.body.as_deref().map(str::len), Some(BODY_LIMIT));
        assert_eq!(server.attempts(), 1);
        assert_eq!(sleeper.delays(), Vec::<u64>::new());
    }

    #[tokio::test]
    async fn honors_retry_after_seconds_header() {
        let server = test_server(vec![
            (
                503,
                "Service Unavailable",
                vec![("retry-after", "2")],
                "busy".to_string(),
            ),
            (200, "OK", vec![], "ok".to_string()),
        ]);
        let sleeper = std::sync::Arc::new(FakeSleeper::default());
        executor(sleeper.clone())
            .execute(&prepared(&format!("{}/v1/chat", server.addr)))
            .await
            .unwrap();
        assert_eq!(sleeper.delays(), vec![2000]);
    }

    // ---- status classification ----

    fn minimal_http(body: Option<&str>) -> HttpContext {
        HttpContext {
            request: HttpRequestDetails {
                method: "POST".to_string(),
                url: "https://provider.test/v1/chat".to_string(),
                headers: BTreeMap::new(),
            },
            response: None,
            body: body.map(str::to_string),
            body_truncated: None,
            request_id: None,
            rate_limit: None,
        }
    }

    fn classify(status: u16, body: Option<&str>) -> LlmErrorReason {
        status_reason(StatusReasonInput {
            status,
            message: format!("Provider request failed with HTTP {status}"),
            retry_after_ms: None,
            rate_limit: None,
            http: minimal_http(body),
        })
    }

    #[test]
    fn classifies_content_policy_before_status() {
        assert!(matches!(
            classify(402, Some("request rejected by safety systems")),
            LlmErrorReason::ContentPolicy { .. }
        ));
    }

    #[test]
    fn classifies_authentication_statuses() {
        assert!(matches!(
            classify(401, Some("unauthorized")),
            LlmErrorReason::Authentication {
                kind: AuthKind::Invalid,
                ..
            }
        ));
        assert!(matches!(
            classify(403, Some("forbidden")),
            LlmErrorReason::Authentication {
                kind: AuthKind::InsufficientPermissions,
                ..
            }
        ));
    }

    #[test]
    fn classifies_invalid_request_without_overflow() {
        let reason = classify(400, Some("invalid parameter"));
        assert!(matches!(
            reason,
            LlmErrorReason::InvalidRequest {
                classification: None,
                ..
            }
        ));
        let reason = classify(413, Some("request too large"));
        assert!(matches!(
            reason,
            LlmErrorReason::InvalidRequest {
                classification: None,
                ..
            }
        ));
    }

    #[test]
    fn classifies_invalid_request_with_context_overflow() {
        let reason = classify(400, Some("context_length_exceeded"));
        assert!(matches!(
            reason,
            LlmErrorReason::InvalidRequest {
                classification: Some(ProviderFailureClassification::ContextOverflow),
                ..
            }
        ));
    }

    #[test]
    fn classifies_retryable_server_errors() {
        for status in [500, 503, 504, 529] {
            assert!(matches!(
                classify(status, Some("busy")),
                LlmErrorReason::ProviderInternal { status: s, .. } if s == status as f64
            ));
        }
    }

    #[test]
    fn classifies_unknown_provider() {
        assert!(matches!(
            classify(418, Some("teapot")),
            LlmErrorReason::UnknownProvider { status, .. } if status == Some(418.0)
        ));
    }

    // ---- retry-after parsing ----

    #[test]
    fn parses_retry_after_ms_first() {
        let mut headers = BTreeMap::new();
        headers.insert("retry-after-ms".to_string(), "250".to_string());
        headers.insert("retry-after".to_string(), "9".to_string());
        assert_eq!(retry_after_ms(&headers), Some(250.0));
    }

    #[test]
    fn parses_retry_after_seconds() {
        let mut headers = BTreeMap::new();
        headers.insert("retry-after".to_string(), "2".to_string());
        assert_eq!(retry_after_ms(&headers), Some(2000.0));
    }

    #[test]
    fn parses_retry_after_http_date() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "retry-after".to_string(),
            "Sun, 06 Nov 2100 08:49:37 GMT".to_string(),
        );
        assert!(retry_after_ms(&headers).unwrap() > 0.0);
    }

    #[test]
    fn clamps_negative_retry_after() {
        let mut headers = BTreeMap::new();
        headers.insert("retry-after-ms".to_string(), "-5".to_string());
        assert_eq!(retry_after_ms(&headers), Some(0.0));
    }

    #[test]
    fn missing_retry_after_is_none() {
        let headers = BTreeMap::new();
        assert_eq!(retry_after_ms(&headers), None);
    }

    // ---- rate-limit details ----

    #[test]
    fn harvests_openai_rate_limit_headers() {
        let headers = BTreeMap::from([
            ("x-ratelimit-limit-requests".to_string(), "500".to_string()),
            ("x-ratelimit-limit-tokens".to_string(), "30000".to_string()),
            (
                "x-ratelimit-remaining-requests".to_string(),
                "499".to_string(),
            ),
            (
                "x-ratelimit-remaining-tokens".to_string(),
                "29900".to_string(),
            ),
            ("x-ratelimit-reset-requests".to_string(), "1s".to_string()),
            ("x-ratelimit-reset-tokens".to_string(), "10s".to_string()),
        ]);
        let details = rate_limit_details(&headers, Some(0.0)).unwrap();
        assert_eq!(details.retry_after_ms, Some(0.0));
        assert_eq!(
            details.limit.unwrap(),
            BTreeMap::from([
                ("requests".to_string(), "500".to_string()),
                ("tokens".to_string(), "30000".to_string()),
            ])
        );
        assert_eq!(
            details.remaining.unwrap(),
            BTreeMap::from([
                ("requests".to_string(), "499".to_string()),
                ("tokens".to_string(), "29900".to_string()),
            ])
        );
        assert_eq!(
            details.reset.unwrap(),
            BTreeMap::from([
                ("requests".to_string(), "1s".to_string()),
                ("tokens".to_string(), "10s".to_string()),
            ])
        );
    }

    #[test]
    fn harvests_anthropic_rate_limit_headers() {
        let headers = BTreeMap::from([
            (
                "anthropic-ratelimit-requests-limit".to_string(),
                "100".to_string(),
            ),
            (
                "anthropic-ratelimit-requests-remaining".to_string(),
                "12".to_string(),
            ),
            (
                "anthropic-ratelimit-input-tokens-reset".to_string(),
                "2026-05-06T12:00:10Z".to_string(),
            ),
        ]);
        let details = rate_limit_details(&headers, None).unwrap();
        assert_eq!(details.retry_after_ms, None);
        assert_eq!(
            details.limit.unwrap(),
            BTreeMap::from([("requests".to_string(), "100".to_string())])
        );
        assert_eq!(
            details.remaining.unwrap(),
            BTreeMap::from([("requests".to_string(), "12".to_string())])
        );
        assert_eq!(
            details.reset.unwrap(),
            BTreeMap::from([(
                "input-tokens".to_string(),
                "2026-05-06T12:00:10Z".to_string()
            )])
        );
    }

    #[test]
    fn rate_limit_details_none_when_empty() {
        let headers = BTreeMap::new();
        assert!(rate_limit_details(&headers, None).is_none());
    }

    // ---- redaction ----

    #[test]
    fn redacts_url_query_secrets_and_encodes_them() {
        assert_eq!(
            redact_url("https://provider.test/v1/chat?api_key=secret&key=secret&debug=1"),
            "https://provider.test/v1/chat?api_key=%3Credacted%3E&key=%3Credacted%3E&debug=1"
        );
        assert_eq!(
            redact_url("https://provider.test/v1/chat"),
            "https://provider.test/v1/chat"
        );
        assert_eq!(redact_url("provider.test/v1/chat"), REDACTED);
    }

    #[test]
    fn redacts_headers() {
        let headers = vec![
            ("Authorization".to_string(), "Bearer abc".to_string()),
            ("x-safe".to_string(), "visible".to_string()),
        ];
        let redacted = redact_headers(&headers);
        assert_eq!(redacted.get("authorization"), Some(&REDACTED.to_string()));
        assert_eq!(redacted.get("x-safe"), Some(&"visible".to_string()));
    }

    fn redaction_request() -> PreparedRequest {
        PreparedRequest {
            method: "POST".to_string(),
            url: "https://provider.test/v1/chat?api_key=query-secret-123&debug=1".to_string(),
            headers: vec![(
                "authorization".to_string(),
                "Bearer header-secret-456".to_string(),
            )],
            body: String::new(),
        }
    }

    #[test]
    fn redacts_body_fields_and_echoed_secrets() {
        let request = redaction_request();
        let body = concat!(
            "{\"error\":{\"key\":\"body-secret\",\"detail\":\"api_key=query-secret\"}} ",
            "provider echoed query-secret-123 and authorization header-secret-456"
        );
        let redacted = redact_body(body, &request);
        assert!(redacted.contains("\"key\":\"<redacted>\""));
        assert!(redacted.contains("api_key=<redacted>"));
        assert!(!redacted.contains("body-secret"));
        assert!(!redacted.contains("query-secret"));
        assert!(!redacted.contains("header-secret-456"));
    }

    #[test]
    fn truncates_large_bodies() {
        let request = prepared("https://provider.test/v1/chat");
        let details = response_body(Some(&"x".repeat(20_000)), &request);
        assert_eq!(details.body.as_deref().map(str::len), Some(BODY_LIMIT));
        assert_eq!(details.body_truncated, Some(true));
        let details = response_body(Some("short"), &request);
        assert_eq!(details.body.as_deref(), Some("short"));
        assert_eq!(details.body_truncated, None);
    }

    #[test]
    fn builds_provider_message_with_body() {
        let details = ResponseBody {
            body: Some("rate limited".to_string()),
            body_truncated: None,
        };
        assert_eq!(
            provider_message(429, &details),
            "Provider request failed with HTTP 429: rate limited"
        );
    }

    #[test]
    fn delay_respects_jitter_bounds_and_cap() {
        let low = jitter_delay(0.0);
        let high = jitter_delay(1.0);
        // attempt 0: [500 * 0.8, 500 * 1.2]
        assert_eq!(low.retry_delay(&retryable_error(), 0), 400);
        assert_eq!(high.retry_delay(&retryable_error(), 0), 600);
        // attempt 5: 500 * 2^5 = 16_000 → capped at 10_000 on both bounds.
        assert_eq!(low.retry_delay(&retryable_error(), 5), 10_000);
        assert_eq!(high.retry_delay(&retryable_error(), 5), 10_000);
    }

    #[test]
    fn delay_uses_retry_after_when_present() {
        let error = LlmError {
            module: "RequestExecutor".to_string(),
            method: "execute".to_string(),
            reason: LlmErrorReason::RateLimit {
                message: "rate limited".to_string(),
                retry_after_ms: Some(1234.0),
                rate_limit: None,
                provider_metadata: None,
                http: None,
            },
        };
        assert_eq!(jitter_delay(0.5).retry_delay(&error, 0), 1234);
        assert_eq!(jitter_delay(0.0).retry_delay(&error, 5), 1234);
    }

    fn retryable_error() -> LlmError {
        LlmError {
            module: "RequestExecutor".to_string(),
            method: "execute".to_string(),
            reason: LlmErrorReason::ProviderInternal {
                message: "busy".to_string(),
                status: 503.0,
                retry_after_ms: None,
                provider_metadata: None,
                http: None,
            },
        }
    }

    fn jitter_delay(t: f64) -> RequestExecutor<FakeSleeper, FakeJitter> {
        RequestExecutor::with_parts(
            reqwest::Client::new(),
            RetryConfig::default(),
            std::sync::Arc::new(FakeSleeper::default()),
            std::sync::Arc::new(FakeJitter(t)),
        )
    }
}
