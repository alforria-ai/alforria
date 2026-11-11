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

use crate::bearer::{BearerError, Principal, Via};
use crate::middleware::directory;
use crate::middleware::scope;
use crate::policy::DirectoryAllowlist;
use crate::pty::ticket::TicketCache;
use crate::routes::{ui, v1, v2};
use crate::state::AuthConfig;
use crate::ticket::{FileScope, FileTickets};

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
/// `groups/pty.ts:18-21`) — `^(/api)?/pty/[^/]+/connect$`. Returns the
/// (still percent-encoded) PTY id.
fn pty_connect_id<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(prefix)?;
    let (pty_id, tail) = rest.split_once('/')?;
    // `^/pty/[^/]+/connect$` — the id must be non-empty.
    (!pty_id.is_empty() && tail == "connect").then_some(pty_id)
}

#[cfg(test)]
fn is_pty_connect_path(path: &str, prefix: &str) -> bool {
    pty_connect_id(path, prefix).is_some()
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
/// or the v2 `UnauthorizedError` JSON for a request without Basic
/// credentials; for a bearer credential, why it was refused (401) or what
/// it may not do (403).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rejection {
    Empty,
    V2,
    Bearer(String),
    Forbidden(String),
}

/// `^Bearer\s+(.+)$/i` on the `Authorization` header (RFC 6750 §2.1).
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if !value.as_bytes().get(..6)?.eq_ignore_ascii_case(b"bearer") {
        return None;
    }
    let rest = &value[6..];
    let token = rest.trim_start_matches([' ', '\t']);
    (token.len() < rest.len() && !token.is_empty()).then_some(token)
}

/// The directory allowlist for a credential narrower than `admin`
/// (`server.directories`).
///
/// The directory asked about is the one [`directory::resolve`] returns for
/// this route — the same value the location layer or the route's handler
/// then acts in, decoded the same way; there is no second reading of the
/// request. It must be allowed when the request names it, and for one of
/// alforria's own routes also when it does not (they fall back to the
/// working directory by themselves). Besides, a directory spelled under any
/// of the three spellings must be allowed even where this route ignores
/// that spelling, and workspaces are not selectable at all. A
/// session-derived directory is checked where it is resolved
/// (`middleware::location`).
fn check_directories(
    directories: &DirectoryAllowlist,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<(), Rejection> {
    let query = uri.query();
    let outside = |directory: &str| {
        Rejection::Forbidden(format!(
            "this token may not open {directory}: it is outside server.directories"
        ))
    };
    for directory in directory::spelled(query, headers) {
        if !directories.allows(std::path::Path::new(&directory)) {
            return Err(outside(&directory));
        }
    }
    let header_text = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let workspace = query_param(query, "workspace")
        .or_else(|| query_param(query, "location[workspace]"))
        .filter(|value| !value.is_empty())
        .or_else(|| header_text("x-opencode-workspace"));
    if workspace.is_some() {
        return Err(Rejection::Forbidden(
            "selecting a workspace needs the admin scope".to_string(),
        ));
    }
    let family = directory::Family::of(uri.path());
    let resolved = directory::resolve(family, query, headers);
    if resolved.named {
        if !directories.allows(&resolved.directory) {
            return Err(outside(&resolved.raw));
        }
    } else if family == directory::Family::Own && !directories.allows(&resolved.directory) {
        return Err(Rejection::Forbidden(
            "this token may not open the server's working directory: pass ?directory= with one \
             of server.directories"
                .to_string(),
        ));
    }
    Ok(())
}

fn check_bearer(
    auth: &AuthConfig,
    directories: &DirectoryAllowlist,
    token: &str,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<Principal, Rejection> {
    let Some(bearer) = &auth.bearer else {
        return Err(Rejection::Bearer(
            "bearer authentication is not configured on this server".to_string(),
        ));
    };
    let principal = bearer
        .verify(token)
        .map_err(|err: BearerError| Rejection::Bearer(err.to_string()))?;
    let required = scope::required(method.as_str(), uri.path());
    if !principal.allows(required) {
        return Err(Rejection::Forbidden(format!(
            "{} {} needs the {} scope; this token has {}",
            method,
            uri.path(),
            required.as_str(),
            principal.scope.as_str()
        )));
    }
    // A webhook credential is bound to one trigger: checked here, before
    // the request reaches anything that would tell an existing trigger
    // from a missing one.
    if let Some(id) = scope::hook_trigger_id(method.as_str(), uri.path()) {
        if !principal.may_deliver_to(&url_decode(id)) {
            return Err(Rejection::Forbidden(
                "this token may not deliver to this trigger: a hook credential is bound to one \
                 trigger (the `trg` claim, or `trigger` on a static token)"
                    .to_string(),
            ));
        }
    } else if principal.scope == crate::bearer::Scope::Hook {
        // (Unreachable while `hook` reaches only the webhook route: kept so
        // that widening the table cannot widen an unbound token.)
        return Err(Rejection::Forbidden(
            "a hook credential delivers to its trigger and nothing else".to_string(),
        ));
    }
    if principal.restricted() {
        check_directories(directories, uri, headers)?;
    }
    Ok(principal)
}

/// The routes a file ticket opens (`routes::file_ext`): reads only.
fn is_file_read(method: &Method, path: &str) -> bool {
    (method == Method::GET || method == Method::HEAD)
        && (path == "/file/raw" || path == "/file/content")
}

/// Whether the request carries a live file ticket issued for exactly the
/// file and project it asks for. The handler checks again against the file
/// it opens.
fn file_ticketed(file_tickets: &FileTickets, uri: &Uri, headers: &HeaderMap) -> bool {
    let Some(ticket) = query_param(uri.query(), "ticket").filter(|ticket| !ticket.is_empty())
    else {
        return false;
    };
    let Some(path) = query_param(uri.query(), "path").filter(|path| !path.is_empty()) else {
        return false;
    };
    let directory =
        directory::resolve(directory::Family::of(uri.path()), uri.query(), headers).directory;
    let file = if std::path::Path::new(&path).is_absolute() {
        std::path::PathBuf::from(&path)
    } else {
        directory.join(&path)
    };
    // The file as the kernel would resolve the request's spelling — the
    // handler resolves the same way and reads what that names. A spelling
    // with no answer matches no ticket.
    let Ok(file) = crate::confine::resolve_for_access(&file) else {
        return false;
    };
    file_tickets.admits(&ticket, &FileScope { file, directory })
}

#[allow(clippy::too_many_arguments)]
fn check(
    auth: &AuthConfig,
    tickets: &TicketCache,
    file_tickets: &FileTickets,
    directories: &DirectoryAllowlist,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
) -> Result<Option<Principal>, Rejection> {
    let path = uri.path();
    if is_openapi(method, path) {
        return Ok(None);
    }
    if is_public_ui_path(method, path) {
        return Ok(None);
    }
    if is_file_read(method, path) && file_ticketed(file_tickets, uri, headers) {
        return Ok(Some(Principal::operator(Via::Ticket)));
    }
    let surface = surface(method, path);
    if surface != Surface::Raw {
        let prefix = match surface {
            Surface::V1Api => "/pty/",
            Surface::V2Api => "/api/pty/",
            Surface::Raw => unreachable!(),
        };
        // Deliberate deviation: TS lets any non-empty `?ticket=` through and
        // leaves validation to the handler; here the ticket must be live and
        // issued for this PTY before the request gets past auth (the handler
        // still consumes it against the full scope).
        let ticketed = pty_connect_id(path, prefix).is_some_and(|pty_id| {
            query_param(uri.query(), "ticket").is_some_and(|ticket| {
                !ticket.is_empty() && tickets.is_live_for(&ticket, &url_decode(pty_id))
            })
        });
        if ticketed {
            return Ok(Some(Principal::operator(Via::Ticket)));
        }
    }
    // A bearer credential is judged as one: it never falls back to Basic.
    if let Some(token) = bearer_token(headers) {
        return check_bearer(auth, directories, token, method, uri, headers).map(Some);
    }
    if is_authorized(auth, uri, headers) {
        return Ok(Some(Principal::operator(Via::Basic)));
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
fn unauthorized_raw(challenge: &'static str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("www-authenticate", challenge)
        .body(Body::empty())
        .expect("static response parts are valid")
}

/// The challenge of a server that only takes bearer credentials.
pub const WWW_AUTHENTICATE_BEARER: &str = "Bearer realm=\"alforria\"";

/// 401 for a refused bearer credential (RFC 6750 §3.1): the reason in the
/// body and in the challenge.
fn unauthorized_bearer(reason: &str) -> Response<Body> {
    let mut response = crate::error::ApiError::Unauthorized {
        message: reason.to_string(),
    }
    .into_response();
    let description: String = reason
        .chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control() && *c != '"' && *c != '\\')
        .collect();
    if let Ok(value) = HeaderValue::from_str(&format!(
        "Bearer realm=\"alforria\", error=\"invalid_token\", error_description=\"{description}\""
    )) {
        response.headers_mut().insert("www-authenticate", value);
    }
    response
}

/// 403 for a valid bearer credential that may not do this.
fn forbidden(reason: &str) -> Response<Body> {
    let mut response = crate::error::ApiError::Forbidden {
        message: reason.to_string(),
    }
    .into_response();
    response.headers_mut().insert(
        "www-authenticate",
        HeaderValue::from_static("Bearer realm=\"alforria\", error=\"insufficient_scope\""),
    );
    response
}

/// v2 401: `UnauthorizedError { message: "Authentication required" }` JSON
/// (v2 authorization.ts:54; protocol errors.ts:13-16).
fn unauthorized_v2(challenge: &'static str) -> Response<Body> {
    let mut response = crate::error::ApiError::Unauthorized {
        message: "Authentication required".to_string(),
    }
    .into_response();
    response
        .headers_mut()
        .insert("www-authenticate", HeaderValue::from_static(challenge));
    response
}

/// The authorization layer. When no password is configured it is a pure
/// pass-through (v1 authorization.ts:104, :122; v2 authorization.ts:42).
#[derive(Clone)]
pub struct AuthLayer {
    auth: AuthConfig,
    tickets: TicketCache,
    file_tickets: FileTickets,
    directories: DirectoryAllowlist,
}

impl AuthLayer {
    pub fn new(auth: AuthConfig, tickets: TicketCache) -> AuthLayer {
        AuthLayer {
            auth,
            tickets,
            file_tickets: FileTickets::default(),
            directories: DirectoryAllowlist::default(),
        }
    }

    /// The cache `POST /file/ticket` issues from.
    pub fn with_file_tickets(mut self, file_tickets: FileTickets) -> AuthLayer {
        self.file_tickets = file_tickets;
        self
    }

    /// The directories a credential narrower than `admin` may open.
    pub fn with_directories(mut self, directories: DirectoryAllowlist) -> AuthLayer {
        self.directories = directories;
        self
    }
}

impl<S> tower::Layer<S> for AuthLayer {
    type Service = AuthService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthService {
            inner,
            auth: self.auth.clone(),
            tickets: self.tickets.clone(),
            file_tickets: self.file_tickets.clone(),
            directories: self.directories.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AuthService<S> {
    inner: S,
    auth: AuthConfig,
    tickets: TicketCache,
    file_tickets: FileTickets,
    directories: DirectoryAllowlist,
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

    fn call(&mut self, mut req: axum::http::Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        // An own route learns its directory here and nowhere else: the
        // value the allowlist is about to be asked about (`directory`).
        if directory::Family::of(req.uri().path()) == directory::Family::Own {
            let resolved =
                directory::resolve(directory::Family::Own, req.uri().query(), req.headers());
            req.extensions_mut()
                .insert(directory::RequestDirectory(resolved.directory));
        }
        if !self.auth.required() {
            return Box::pin(async move { inner.call(req).await });
        }
        let result = check(
            &self.auth,
            &self.tickets,
            &self.file_tickets,
            &self.directories,
            req.method(),
            req.uri(),
            req.headers(),
        );
        // A server with no Basic password to offer challenges for Bearer.
        let challenge = if self.auth.password_required() {
            WWW_AUTHENTICATE
        } else {
            WWW_AUTHENTICATE_BEARER
        };
        Box::pin(async move {
            match result {
                Ok(principal) => {
                    // Handlers and the location layer read who is calling
                    // from here; absent means the operator.
                    if let Some(principal) = principal {
                        req.extensions_mut().insert(principal);
                    }
                    inner.call(req).await
                }
                Err(Rejection::Empty) => Ok(unauthorized_raw(challenge)),
                Err(Rejection::V2) => Ok(unauthorized_v2(challenge)),
                Err(Rejection::Bearer(reason)) => Ok(unauthorized_bearer(&reason)),
                Err(Rejection::Forbidden(reason)) => Ok(forbidden(&reason)),
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
