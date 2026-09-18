//! Basic-auth middleware — port of the v1 authorization middleware
//! (`httpapi/middleware/authorization.ts`), the v2 authorization middleware
//! (`packages/server/src/middleware/authorization.ts`) and the public-UI /
//! PTY-connect-ticket bypasses (`shared/public-ui.ts`, `shared/pty-ticket.ts`).
//!
//! TS bakes one middleware variant into each route family; this port
//! dispatches on the request surface instead:
//!
//! * v1 HttpApi routes (`routes::v1::ROUTES`) and the raw router routes
//!   (`/doc`, the UI catch-all) reject with an **empty** 401 — both
//!   `HttpApiError.Unauthorized` and `HttpServerResponse.empty({status: 401})`
//!   render an empty body;
//! * v2 HttpApi routes (`routes::v2::ROUTES`) reject with the
//!   `UnauthorizedError` JSON body;
//! * all three carry `www-authenticate: Basic realm="Secure Area"`.
//!
//! TS stack order keeps auth inside the global middleware (`errorLayer →
//! compression → corsVaryFix → fence → cors → route handlers`), so the layer is
//! applied innermost in `middleware::apply_stack`.

use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, Response, StatusCode, Uri};
use axum::response::IntoResponse;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use tower::Service;

use crate::routes::{ui, v1, v2};
use crate::state::AuthConfig;

/// `WWW_AUTHENTICATE` (v1 authorization.ts:14, v2 authorization.ts:10).
pub const WWW_AUTHENTICATE: &str = "Basic realm=\"Secure Area\"";

/// `AUTH_TOKEN_QUERY` (v1 authorization.ts:12, v2 authorization.ts:9).
const AUTH_TOKEN_QUERY: &str = "auth_token";

/// Effect `Encoding.decodeBase64` (Encoding.js:152-188): strict about length
/// and `=` placement, but lenient about non-canonical trailing bits — the
/// bits dropped by the final partial group are never validated.
static EFFECT_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::RequireCanonical)
        .with_decode_allow_trailing_bits(true),
);

/// `ServerAuth.DecodedCredentials` (`auth.ts:12-15`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Credentials {
    /// `emptyCredential()` (v1 authorization.ts:33-38).
    fn empty() -> Credentials {
        Credentials {
            username: String::new(),
            password: String::new(),
        }
    }
}

/// `decodeCredential` (v1 authorization.ts:57-71, v2 authorization.ts:16-27):
/// invalid base64 or a missing `:` separator → empty credentials. The decoded
/// bytes go through `TextDecoder` — invalid UTF-8 is replaced, not rejected.
fn decode_credential(input: &str) -> Credentials {
    let stripped: String = input
        .chars()
        .filter(|c| !matches!(c, '\n' | '\r'))
        .collect();
    let bytes = match EFFECT_BASE64.decode(stripped.as_bytes()) {
        Ok(bytes) => bytes,
        Err(_) => return Credentials::empty(),
    };
    let decoded = String::from_utf8_lossy(&bytes);
    match decoded.find(':') {
        Some(separator) => Credentials {
            username: decoded[..separator].to_string(),
            password: decoded[separator + 1..].to_string(),
        },
        None => Credentials::empty(),
    }
}

/// `^Basic\s+(.+)$/i` on the `Authorization` header (v1 authorization.ts:80).
fn basic_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if !value.as_bytes().get(..5)?.eq_ignore_ascii_case(b"basic") {
        return None;
    }
    let rest = &value[5..];
    let mut start = 0;
    for (i, c) in rest.char_indices() {
        if is_js_whitespace(c) {
            start = i + c.len_utf8();
        } else {
            break;
        }
    }
    if start == 0 {
        return None;
    }
    let token = &rest[start..];
    (!token.is_empty()).then_some(token)
}

fn is_js_whitespace(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{0b}' | '\u{feff}')
}

/// `url.searchParams.get(name)` — WHATWG `application/x-www-form-urlencoded`:
/// first value wins, `+` decodes to space, percent-escapes decode lossily.
pub(crate) fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    for pair in query?.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if url_decode(key) == name {
            return Some(url_decode(value));
        }
    }
    None
}

fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len()
                && hex(bytes[i + 1]).is_some()
                && hex(bytes[i + 2]).is_some() =>
            {
                let hi = hex(bytes[i + 1]).expect("checked");
                let lo = hex(bytes[i + 2]).expect("checked");
                out.push(hi * 16 + lo);
                i += 3;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// `credentialFromURL` (v1 authorization.ts:77-83): `?auth_token=` takes
/// precedence over the `Authorization` header; an empty token falls through.
fn credentials(uri: &Uri, headers: &HeaderMap) -> Credentials {
    if let Some(token) = query_param(uri.query(), AUTH_TOKEN_QUERY) {
        if !token.is_empty() {
            return decode_credential(&token);
        }
    }
    match basic_token(headers) {
        Some(token) => decode_credential(token),
        None => Credentials::empty(),
    }
}

/// Which TS middleware instance a request reaches: the v1 HttpApi
/// authorization middleware, its v2 counterpart, or the raw router
/// middleware wrapping `/doc` and the UI catch-all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    V1Api,
    V2Api,
    Raw,
}

fn surface(method: &Method, path: &str) -> Surface {
    let method = method.as_str();
    if routes(v2::ROUTES, method, path) {
        Surface::V2Api
    } else if routes(v1::ROUTES, method, path) {
        Surface::V1Api
    } else {
        Surface::Raw
    }
}

pub(crate) fn routes(table: &[(&str, &str)], method: &str, path: &str) -> bool {
    table
        .iter()
        .any(|(route_method, route)| *route_method == method && route_matches(route, path))
}

/// Segment-wise match against an axum `{param}` / `{*wildcard}` route pattern.
pub(crate) fn route_matches(pattern: &str, path: &str) -> bool {
    if let Some(wild) = pattern.find("{*") {
        // Wildcard segments ({*path}) always appear last; the literal prefix
        // up to (and including) the trailing '/' must match, the remainder
        // (possibly empty) is captured.
        let prefix = &pattern[..wild];
        return path.starts_with(prefix);
    }
    let mut pat = pattern.strip_prefix('/').unwrap_or(pattern);
    let mut seg = path.strip_prefix('/').unwrap_or(path);
    loop {
        match (pat.split_once('/'), seg.split_once('/')) {
            (None, None) => return segment_matches(pat, seg),
            (Some((pat_head, pat_rest)), Some((seg_head, seg_rest))) => {
                if !segment_matches(pat_head, seg_head) {
                    return false;
                }
                pat = pat_rest;
                seg = seg_rest;
            }
            _ => return false,
        }
    }
}

fn segment_matches(pattern: &str, seg: &str) -> bool {
    if pattern.starts_with('{') {
        !seg.is_empty()
    } else {
        pattern == seg
    }
}

/// `isPtyConnectPath` (`shared/pty-ticket.ts:9-11`; v2: protocol
/// `groups/pty.ts:18-21`) — `^(/api)?/pty/[^/]+/connect$`.
fn is_pty_connect_path(path: &str, prefix: &str) -> bool {
    let Some(rest) = path.strip_prefix(prefix) else {
        return false;
    };
    let Some((_, tail)) = rest.split_once('/') else {
        return false;
    };
    tail == "connect"
}

/// `isPublicUIPath` (`shared/public-ui.ts:10-12`).
fn is_public_ui_path(method: &Method, path: &str) -> bool {
    method == Method::GET && ui::PUBLIC_UI_PATHS.contains(&path)
}

/// `GET /openapi.json` is registered bare on the router (effect
/// `HttpApiBuilder.js:52-54`) — no auth middleware wraps it.
fn is_openapi(method: &Method, path: &str) -> bool {
    method == Method::GET && path == "/openapi.json"
}

/// The rejection a rejected request takes: the empty 401 (v1 + raw surfaces)
/// or the v2 `UnauthorizedError` JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejection {
    Empty,
    V2,
}

fn check(
    auth: &AuthConfig,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<(), Rejection> {
    let path = uri.path();
    if is_openapi(method, path) {
        return Ok(());
    }
    if is_public_ui_path(method, path) {
        return Ok(());
    }
    let surface = surface(method, path);
    if surface != Surface::Raw {
        let prefix = match surface {
            Surface::V1Api => "/pty/",
            Surface::V2Api => "/api/pty/",
            Surface::Raw => unreachable!(),
        };
        let ticketed = is_pty_connect_path(path, prefix)
            && query_param(uri.query(), "ticket").is_some_and(|ticket| !ticket.is_empty());
        if ticketed {
            return Ok(());
        }
    }
    if is_authorized(auth, uri, headers) {
        return Ok(());
    }
    Err(match surface {
        Surface::V2Api => Rejection::V2,
        Surface::V1Api | Surface::Raw => Rejection::Empty,
    })
}

fn is_authorized(auth: &AuthConfig, uri: &Uri, headers: &HeaderMap) -> bool {
    let credential = credentials(uri, headers);
    auth.authorized(&credential.username, &credential.password)
}

/// `HttpApiError.Unauthorized` — an empty 401 response
/// (HttpApiError.js:97-100) with the `www-authenticate` header appended by
/// `validateCredential`/`validateRawCredential`.
fn unauthorized_raw() -> Response<Body> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("www-authenticate", WWW_AUTHENTICATE)
        .body(Body::empty())
        .expect("static response parts are valid")
}

/// v2 401: `UnauthorizedError { message: "Authentication required" }` JSON
/// (v2 authorization.ts:54; protocol errors.ts:13-16).
fn unauthorized_v2() -> Response<Body> {
    let mut response = crate::error::ApiError::Unauthorized {
        message: "Authentication required".to_string(),
    }
    .into_response();
    response.headers_mut().insert(
        "www-authenticate",
        HeaderValue::from_static(WWW_AUTHENTICATE),
    );
    response
}

/// The authorization layer. When no password is configured it is a pure
/// pass-through (v1 authorization.ts:104, :122; v2 authorization.ts:42).
#[derive(Clone)]
pub struct AuthLayer {
    auth: AuthConfig,
}

impl AuthLayer {
    pub fn new(auth: AuthConfig) -> AuthLayer {
        AuthLayer { auth }
    }
}

impl<S> tower::Layer<S> for AuthLayer {
    type Service = AuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthService {
            inner,
            auth: self.auth.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AuthService<S> {
    inner: S,
    auth: AuthConfig,
}

impl<S> Service<axum::http::Request<Body>> for AuthService<S>
where
    S: Service<axum::http::Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        if !self.auth.required() {
            return Box::pin(async move { inner.call(req).await });
        }
        let result = check(&self.auth, req.method(), req.uri(), req.headers());
        Box::pin(async move {
            match result {
                Ok(()) => inner.call(req).await,
                Err(Rejection::Empty) => Ok(unauthorized_raw()),
                Err(Rejection::V2) => Ok(unauthorized_v2()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use base64::Engine;

    fn b64(input: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(input)
    }

    #[test]
    fn decode_splits_at_first_colon() {
        let credential = decode_credential(&b64("user:pa:ss"));
        assert_eq!(credential.username, "user");
        assert_eq!(credential.password, "pa:ss");
    }

    #[test]
    fn decode_rejects_invalid_base64() {
        assert_eq!(decode_credential("!!!not-base64!!!"), Credentials::empty());
    }

    #[test]
    fn decode_requires_a_separator() {
        assert_eq!(decode_credential(&b64("nocolonhere")), Credentials::empty());
    }

    #[test]
    fn decode_strips_crlf() {
        // Encoding.decodeBase64 runs stripCrlF before decoding (Encoding.js:153).
        let encoded = b64("user:pw");
        let with_newlines = format!("{}\n{}\r\n", &encoded[..4], &encoded[4..]);
        let credential = decode_credential(&with_newlines);
        assert_eq!(credential.username, "user");
        assert_eq!(credential.password, "pw");
    }

    #[test]
    fn decode_ignores_non_canonical_trailing_bits() {
        // The final data symbol carries bits that fall outside the decoded
        // byte — effect decodes them away without validating them.
        let canonical = b64("user:pw");
        let mut noncanonical = String::from(canonical.trim_end_matches('='));
        noncanonical.pop();
        // 'w' = 0b110000 → 'x' = 0b110001: same high bits (the decoded byte),
        // non-zero trailing bits.
        noncanonical.push('x');
        noncanonical.push_str(&canonical[canonical.len() - 2..]);
        let credential = decode_credential(&noncanonical);
        assert_eq!(credential.username, "user");
        assert_eq!(credential.password, "pw");
    }

    #[test]
    fn basic_token_matches_case_insensitively() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("basic dXNlcjpwdw=="),
        );
        assert_eq!(basic_token(&headers), Some("dXNlcjpwdw=="));
    }

    #[test]
    fn basic_token_requires_whitespace_and_payload() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(basic_token(&headers), Some("abc"));

        for value in ["Basictoken", "Basic", "Basic ", "Bearer xyz"] {
            headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            assert_eq!(basic_token(&headers), None, "{value} must not match");
        }
    }

    #[test]
    fn query_param_decodes_form_urlencoding() {
        assert_eq!(query_param(Some("a=1&b=2"), "b"), Some("2".into()));
        assert_eq!(query_param(Some("a=1&b"), "b"), Some(String::new()));
        assert_eq!(query_param(Some("b=1&b=2"), "b"), Some("1".into()));
        assert_eq!(query_param(None, "b"), None);
        // '+' decodes to space; percent-escapes decode.
        assert_eq!(query_param(Some("b=a+b%21"), "b"), Some("a b!".into()));
        // Escapes that are incomplete or invalid are kept as-is.
        assert_eq!(query_param(Some("b=a%2z"), "b"), Some("a%2z".into()));
    }

    #[test]
    fn route_matching_matrix() {
        assert!(route_matches(
            "/session/{sessionID}/message",
            "/session/ses_1/message"
        ));
        assert!(!route_matches(
            "/session/{sessionID}/message",
            "/session/ses_1"
        ));
        assert!(!route_matches(
            "/session/{sessionID}/message",
            "/session//message"
        ));
        assert!(!route_matches("/session", "/session/"));
        assert!(route_matches(
            "/api/fs/read/{*path}",
            "/api/fs/read/a/b.txt"
        ));
        assert!(route_matches("/api/fs/read/{*path}", "/api/fs/read/"));
        assert!(!route_matches("/api/fs/read/{*path}", "/api/fs/read"));
    }

    #[test]
    fn pty_connect_path_matrix() {
        assert!(is_pty_connect_path("/pty/pty_1/connect", "/pty/"));
        assert!(is_pty_connect_path("/api/pty/pty_1/connect", "/api/pty/"));
        assert!(!is_pty_connect_path("/pty/connect", "/pty/"));
        assert!(!is_pty_connect_path("/pty/a/b/connect", "/pty/"));
        assert!(!is_pty_connect_path("/pty/a/connect-token", "/pty/"));
        assert!(!is_pty_connect_path("/pty/a/connect/extra", "/pty/"));
    }
}
