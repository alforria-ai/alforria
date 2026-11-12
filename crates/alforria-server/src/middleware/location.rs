//! Location & workspace-routing middleware — port of the v1
//! `WorkspaceRoutingMiddleware` + `InstanceContextMiddleware`
//! (`httpapi/middleware/workspace-routing.ts`, `.../instance-context.ts`)
//! and the v2 `LocationMiddleware` + `SessionLocationMiddleware`
//! (`packages/server/src/location.ts`,
//! `packages/server/src/middleware/session-location.ts`).
//!
//! TS bakes the middlewares into each route family:
//!
//! * every v1 family except control, control-plane and global resolves a
//!   workspace-routing plan and loads the instance before the handler runs;
//! * the v2 location routes resolve `location[directory]` /
//!   `location[workspace]` (or headers) and provide per-location services;
//! * the v2 session-scoped routes derive the location from the session row
//!   (400 on a malformed id, 404 when missing).
//!
//! Remote workspace proxying is M7: without a workspace registry every
//! selected workspace id is unknown (`MissingWorkspace`), and the plan always
//! resolves `Local` (`workspace-routing.ts:148-186`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll};

use alforria_core::{SessionError, SessionServices};
use alforria_schema::session_v1::V1SessionInfo;
use axum::body::Body;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use tower::Service;

use crate::middleware::auth::{query_param, route_matches, routes};
use crate::middleware::directory;
use crate::routes::{v1, v2};
use crate::state::ServerContext;

/// v1 endpoints without the workspace-routing middleware: the control,
/// control-plane and global families (`httpapi/api.ts:54-77`).
const V1_NO_LOCATION: &[(&str, &str)] = &[
    ("PUT", "/auth/{providerID}"),
    ("DELETE", "/auth/{providerID}"),
    ("POST", "/log"),
    ("POST", "/experimental/control-plane/move-session"),
    ("GET", "/global/health"),
    ("GET", "/global/event"),
    ("GET", "/global/config"),
    ("PATCH", "/global/config"),
    ("POST", "/global/dispose"),
    ("POST", "/global/upgrade"),
];

/// v2 endpoints without any location middleware: health, the three session
/// collection endpoints and the event stream
/// (`protocol/src/api.ts:38-55`, `protocol/src/groups/session.ts:106-392`).
const V2_NO_LOCATION: &[(&str, &str)] = &[
    ("GET", "/api/health"),
    ("GET", "/api/session"),
    ("POST", "/api/session"),
    ("GET", "/api/session/active"),
    ("GET", "/api/event"),
];

/// Which location scheme a request resolves through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationSurface {
    /// v1 `?directory=` / `?workspace=` + session-derived routing
    /// (`workspace-routing.ts` + `instance-context.ts`).
    V1,
    /// v2 `location[directory]` / `location[workspace]` query + headers
    /// (`location.ts:29-39`).
    V2,
    /// v2 session-scoped routes (`middleware/session-location.ts`).
    V2Session,
    /// Everything else — no location middleware applies.
    None,
}

/// The per-request location, exposed to handlers via request extensions
/// (TS `WorkspaceRouteContext` + `InstanceRef`).
#[derive(Clone)]
pub struct LocationContext {
    pub directory: PathBuf,
    pub workspace_id: Option<String>,
    pub services: Arc<SessionServices>,
}

pub fn surface(method: &Method, path: &str) -> LocationSurface {
    let method = method.as_str();
    if let Some((_, pattern)) = v2::ROUTES
        .iter()
        .find(|(m, p)| *m == method && route_matches(p, path))
    {
        if V2_NO_LOCATION
            .iter()
            .any(|(m, p)| *m == method && *p == *pattern)
        {
            return LocationSurface::None;
        }
        return if pattern.contains("{sessionID}") {
            LocationSurface::V2Session
        } else {
            LocationSurface::V2
        };
    }
    if routes(v1::ROUTES, method, path) && !routes(V1_NO_LOCATION, method, path) {
        return LocationSurface::V1;
    }
    LocationSurface::None
}

/// `isLocalWorkspaceRoute` (`shared/workspace-routing.ts:5-18`).
pub fn is_local_workspace_route(method: &str, path: &str) -> bool {
    const RULES: &[(&str, Option<&str>, bool)] = &[
        ("/experimental/workspace", None, true),
        ("/session/status", None, false),
        ("/session", Some("GET"), true),
    ];
    for (rule_path, rule_method, is_local) in RULES {
        if let Some(rule_method) = rule_method {
            if *rule_method != method {
                continue;
            }
        }
        if path == *rule_path || path.starts_with(&format!("{rule_path}/")) {
            return *is_local;
        }
    }
    false
}

/// `getWorkspaceRouteSessionID` (`shared/workspace-routing.ts:20-29`).
///
/// alforria extension: the id is returned **as the handler will read it**
/// — the path segment percent-decoded, which is what the router's `Path`
/// extractor hands a handler. This layer runs before routing, on the raw
/// path; taken raw, `/session/ses%5F1` named no session here (so the
/// request was placed by its `?directory=` instead of by the session's
/// own directory, and judged without the session) and then named `ses_1`
/// to the handler.
pub fn workspace_route_session_id(path: &str) -> Option<String> {
    if path == "/session/status" {
        return None;
    }
    let id = path
        .strip_prefix("/session/")
        .and_then(|rest| rest.split('/').next())
        .filter(|id| !id.is_empty())
        .or_else(|| {
            path.strip_prefix("/experimental/session/")
                .and_then(|rest| rest.strip_suffix("/background"))
                .filter(|id| !id.is_empty() && !id.contains('/'))
        })?;
    Some(decode_segment(id))
}

/// One path segment, percent-decoded as the router decodes a path
/// parameter: `%XX` is the byte, anything else stands for itself. A segment
/// that does not decode to UTF-8 is returned as written — the router
/// refuses it (400) before any handler reads it.
fn decode_segment(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let hex = |byte: u8| (byte as char).to_digit(16).map(|digit| digit as u8);
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let pair = (bytes[at] == b'%')
            .then(|| Some(hex(*bytes.get(at + 1)?)? << 4 | hex(*bytes.get(at + 2)?)?))
            .flatten();
        match pair {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| segment.to_string())
}

/// `WorkspaceV2.ID` — `Schema.String.check(Schema.isStartsWith("wrk"))`
/// (`packages/schema/src/workspace-id.ts:5`).
fn is_workspace_id(value: &str) -> bool {
    value.starts_with("wrk")
}

/// `SessionID` — `Schema.String.check(Schema.isStartsWith("ses"))`
/// (`packages/schema/src/session-id.ts:4`).
fn is_session_id(value: &str) -> bool {
    value.starts_with("ses")
}

/// `missingWorkspaceResponse` (`workspace-routing.ts:102-107`).
fn missing_workspace_response(id: &str) -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(format!("Workspace not found: {id}")))
        .expect("static response parts are valid")
}

/// A pre-handler rejection from the location middleware.
#[derive(Debug)]
enum Rejection {
    /// A `WorkspaceV2.ID.make`/`SessionID.make` throw — the defect-500
    /// envelope (`middleware/error.ts`).
    Defect,
    /// `missingWorkspaceResponse` (`workspace-routing.ts:102-107`).
    MissingWorkspace(String),
    /// `InvalidRequestError { message: "Invalid session ID", field:
    /// "sessionID" }` (`middleware/session-location.ts:33-41`).
    InvalidSessionId,
    /// `SessionNotFoundError` (`middleware/session-location.ts:48-52`).
    SessionNotFound { session_id: String },
    /// Instance-load failure (config errors carry their own 400 envelope).
    Load(crate::error::ServerError),
    /// The resolved directory is outside `server.directories` for a
    /// credential narrower than `admin` (alforria extension).
    Forbidden(String),
}

impl From<crate::error::ServerError> for Rejection {
    fn from(err: crate::error::ServerError) -> Rejection {
        Rejection::Load(err)
    }
}

impl Rejection {
    fn into_response(self) -> Response {
        match self {
            Rejection::Defect => crate::error::defect_response(),
            Rejection::MissingWorkspace(id) => missing_workspace_response(&id),
            Rejection::InvalidSessionId => crate::error::ApiError::InvalidRequest {
                message: "Invalid session ID".to_string(),
                kind: None,
                field: Some("sessionID".to_string()),
            }
            .into_response(),
            Rejection::SessionNotFound { session_id } => {
                let message = format!("Session not found: {session_id}");
                crate::error::ApiError::SessionNotFound {
                    session_id,
                    message,
                }
                .into_response()
            }
            Rejection::Load(err) => err.into_response(),
            Rejection::Forbidden(what) => crate::error::ApiError::Forbidden {
                message: format!("this token may not open {what}"),
            }
            .into_response(),
        }
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

/// The location & instance-context layer. TS registers it as route-family
/// middleware *inside* authorization (the group middleware list ends with
/// `Authorization`, `groups/*.ts`), so the layer runs innermost of the
/// global stack.
#[derive(Clone)]
pub struct LocationLayer {
    ctx: Arc<ServerContext>,
    /// `configuredWorkspaceID()` — `Flag.OPENCODE_WORKSPACE_ID`, captured at
    /// construction like the TS module-load flag read (`:65-67`).
    env_workspace_id: Option<String>,
}

impl LocationLayer {
    pub fn new(ctx: Arc<ServerContext>) -> LocationLayer {
        let env_workspace_id = std::env::var("OPENCODE_WORKSPACE_ID")
            .ok()
            .filter(|value| !value.is_empty());
        LocationLayer {
            ctx,
            env_workspace_id,
        }
    }

    pub fn with_env_workspace_id(
        ctx: Arc<ServerContext>,
        env_workspace_id: Option<String>,
    ) -> LocationLayer {
        LocationLayer {
            ctx,
            env_workspace_id: env_workspace_id.filter(|value| !value.is_empty()),
        }
    }
}

impl<S> tower::Layer<S> for LocationLayer {
    type Service = LocationService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LocationService {
            inner,
            ctx: self.ctx.clone(),
            env_workspace_id: self.env_workspace_id.clone(),
        }
    }
}

#[derive(Clone)]
pub struct LocationService<S> {
    inner: S,
    ctx: Arc<ServerContext>,
    env_workspace_id: Option<String>,
}

impl<S> Service<Request<Body>> for LocationService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        match self.resolve(&req) {
            Ok(location) => {
                if let Some(location) = location {
                    req.extensions_mut().insert(location);
                }
            }
            Err(rejection) => return Box::pin(async move { Ok(rejection.into_response()) }),
        }
        let mut inner = self.inner.clone();
        Box::pin(async move { inner.call(req).await })
    }
}

impl<S> LocationService<S> {
    fn resolve(&self, req: &Request<Body>) -> Result<Option<LocationContext>, Rejection> {
        match surface(req.method(), req.uri().path()) {
            LocationSurface::None => Ok(None),
            LocationSurface::V1 => self.resolve_v1(req).map(Some),
            LocationSurface::V2 => self.resolve_v2(req).map(Some),
            LocationSurface::V2Session => self.resolve_v2_session(req).map(Some),
        }
    }

    /// Load the instance for the request's resolved directory. This is
    /// where a session-derived directory meets the directory allowlist:
    /// a credential narrower than `admin` is refused before the instance
    /// (its config, its project) is touched.
    fn load(
        &self,
        req: &Request<Body>,
        directory: &str,
    ) -> Result<Arc<SessionServices>, Rejection> {
        let restricted =
            crate::bearer::Principal::confines(req.extensions().get::<crate::bearer::Principal>());
        if restricted && !self.ctx.policy.directories.allows(Path::new(directory)) {
            return Err(Rejection::Forbidden(format!(
                "{directory}: it is outside server.directories"
            )));
        }
        self.ctx
            .instances
            .load(Path::new(directory))
            .map_err(Rejection::Load)
    }

    /// `routeHttpApiWorkspace` + `planRequest` + `provideInstanceContext`
    /// (`workspace-routing.ts:148-235`, `instance-context.ts:23-35`).
    fn resolve_v1(&self, req: &Request<Body>) -> Result<LocationContext, Rejection> {
        let uri = req.uri();
        let query = uri.query();
        let session = self.route_session(uri.path())?;

        // `configuredWorkspaceID` validates through `WorkspaceV2.ID.make`
        // (`:65-67`).
        let env_workspace_id = match &self.env_workspace_id {
            Some(value) if !is_workspace_id(value) => {
                return Err(Rejection::Defect);
            }
            value => value.clone(),
        };

        // `selectedWorkspaceID` (`:69-72`): the session's workspace wins; a
        // `?workspace=` param goes through `WorkspaceV2.ID.make`, which
        // throws (defect 500) on ids without the `wrk` prefix.
        let mut workspace_id = session
            .as_ref()
            .and_then(|session| session.workspace_id.clone());
        if workspace_id.is_none() {
            if let Some(param) = non_empty(query_param(query, "workspace")) {
                if !is_workspace_id(&param) {
                    return Err(Rejection::Defect);
                }
                workspace_id = Some(param);
            }
        }

        // Without a workspace registry (M7) every selected id resolves to
        // `MissingWorkspace` — unless OPENCODE_WORKSPACE_ID bypasses the
        // lookup (`resolveWorkspace`, `:94-100`; `planRequest`, `:173-175`).
        if let Some(id) = &workspace_id {
            if env_workspace_id.is_none() {
                return Err(Rejection::MissingWorkspace(id.clone()));
            }
        }

        // Whether the caller may write to this session is not decided
        // here: this layer sees the raw path, before routing, and the
        // handler reads the session id decoded. `confine::
        // guard_session_writes` decides it on the route that matched.

        // Local plan (`:181-184`): the route session's directory wins.
        // `provideInstanceContext` decodes the resolved directory
        // (`instance-context.ts:15-21`). A request-named one comes from the
        // single resolver the allowlist gate validated.
        let directory = match session.filter(|session| !session.directory.is_empty()) {
            Some(session) => directory::decode(&session.directory),
            None => directory::resolve(directory::Family::V1, query, req.headers()).raw,
        };
        let services = self.load(req, &directory)?;
        Ok(LocationContext {
            directory: crate::state::resolve_directory(Path::new(&directory)),
            workspace_id: env_workspace_id.or(workspace_id),
            services,
        })
    }

    /// `routeHttpApiWorkspace`'s session lookup — a missing session routes
    /// like no session (`workspace-routing.ts:220-231`).
    fn route_session(&self, path: &str) -> Result<Option<V1SessionInfo>, Rejection> {
        let Some(id) = workspace_route_session_id(path) else {
            return Ok(None);
        };
        if !is_session_id(&id) {
            // `SessionID.make` throws on non-`ses` ids (defect 500).
            return Err(Rejection::Defect);
        }
        match self.ctx.sessions.get(&id) {
            Ok(session) => Ok(Some(session)),
            Err(SessionError::NotFound(_)) => Ok(None),
            Err(_) => Err(Rejection::Defect),
        }
    }

    /// v2 `LocationMiddleware` (`location.ts:29-47`).
    fn resolve_v2(&self, req: &Request<Body>) -> Result<LocationContext, Rejection> {
        let query = req.uri().query();
        let workspace_id = non_empty(query_param(query, "location[workspace]"))
            .or_else(|| non_empty(header_value(req.headers(), "x-opencode-workspace")));
        // `WorkspaceV2.ID.make` throws (defect 500) on invalid ids
        // (`location.ts:37`).
        let workspace_id = match workspace_id {
            Some(value) if is_workspace_id(&value) => Some(value),
            Some(_) => return Err(Rejection::Defect),
            None => None,
        };
        // `location[directory]`, else the header, else cwd (`:33`): the
        // single resolver the allowlist gate validated.
        let directory = directory::resolve(directory::Family::V2, query, req.headers()).raw;
        let services = self.load(req, &directory)?;
        Ok(LocationContext {
            directory: crate::state::resolve_directory(Path::new(&directory)),
            workspace_id,
            services,
        })
    }

    /// v2 `SessionLocationMiddleware` (`middleware/session-location.ts`).
    fn resolve_v2_session(&self, req: &Request<Body>) -> Result<LocationContext, Rejection> {
        let session_id = req
            .uri()
            .path()
            .strip_prefix("/api/session/")
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_default();
        if !is_session_id(session_id) {
            return Err(Rejection::InvalidSessionId);
        }
        let info = match self.ctx.sessions.get(session_id) {
            Ok(info) => info,
            Err(SessionError::NotFound(_)) => {
                return Err(Rejection::SessionNotFound {
                    session_id: session_id.to_string(),
                });
            }
            Err(_) => return Err(Rejection::Defect),
        };
        let directory = info.directory.clone();
        let workspace_id = info.workspace_id.clone();
        let services = self.load(req, &directory)?;
        Ok(LocationContext {
            directory: crate::state::resolve_directory(Path::new(&directory)),
            workspace_id,
            services,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_route_session_id_matrix() {
        let id = |path: &str| workspace_route_session_id(path);
        assert_eq!(id("/session/status"), None);
        assert_eq!(id("/session"), None);
        assert_eq!(id("/session/"), None);
        assert_eq!(id("/session/ses_1").as_deref(), Some("ses_1"));
        assert_eq!(id("/session/ses_1/message").as_deref(), Some("ses_1"));
        assert_eq!(
            id("/experimental/session/ses_1/background").as_deref(),
            Some("ses_1")
        );
        // Not an exact background match.
        assert_eq!(id("/experimental/session/ses_1/other"), None);
        assert_eq!(id("/experimental/session/a/b/background"), None);
        // The id is the one the handler will read: decoded.
        for spelled in [
            "/session/ses%5F1",
            "/session/%73es_1/message",
            "/session/s%65s%5f1",
        ] {
            assert_eq!(id(spelled).as_deref(), Some("ses_1"), "{spelled}");
        }
        assert_eq!(
            id("/experimental/session/ses%5F1/background").as_deref(),
            Some("ses_1")
        );
        // What is not an escape stands for itself, and bytes that are not
        // UTF-8 leave the segment as written (the router refuses it).
        assert_eq!(id("/session/ses%zz").as_deref(), Some("ses%zz"));
        assert_eq!(id("/session/ses%4").as_deref(), Some("ses%4"));
        assert_eq!(id("/session/ses%ff").as_deref(), Some("ses%ff"));
        assert_eq!(id("/session/ses%2Fx").as_deref(), Some("ses/x"));
    }

    #[test]
    fn local_workspace_route_matrix() {
        // Local rules.
        assert!(is_local_workspace_route("GET", "/experimental/workspace"));
        assert!(is_local_workspace_route(
            "POST",
            "/experimental/workspace/warp"
        ));
        assert!(is_local_workspace_route("GET", "/session"));
        // Forward rule.
        assert!(!is_local_workspace_route("GET", "/session/status"));
        // method-specific GET /session does not cover POST.
        assert!(!is_local_workspace_route("POST", "/session"));
        // Everything else forwards, except the GET /session prefix rule
        // (prefix match covers /session/:id/...).
        assert!(is_local_workspace_route("GET", "/session/ses_1/message"));
        assert!(!is_local_workspace_route("POST", "/session/ses_1/message"));
        assert!(!is_local_workspace_route("GET", "/agent"));
    }

    #[test]
    fn surface_matrix() {
        let get = Method::GET;
        let post = Method::POST;
        let put = Method::PUT;
        assert_eq!(surface(&get, "/agent"), LocationSurface::V1);
        assert_eq!(surface(&get, "/session"), LocationSurface::V1);
        assert_eq!(surface(&get, "/session/ses_1/message"), LocationSurface::V1);
        // Control/global families skip the middleware.
        assert_eq!(surface(&put, "/auth/prov"), LocationSurface::None);
        assert_eq!(surface(&post, "/log"), LocationSurface::None);
        assert_eq!(surface(&get, "/global/health"), LocationSurface::None);
        assert_eq!(
            surface(&post, "/experimental/control-plane/move-session"),
            LocationSurface::None
        );
        // v2.
        assert_eq!(surface(&get, "/api/agent"), LocationSurface::V2);
        assert_eq!(surface(&get, "/api/fs/read/a/b"), LocationSurface::V2);
        assert_eq!(
            surface(&get, "/api/session/ses_1"),
            LocationSurface::V2Session
        );
        assert_eq!(
            surface(&post, "/api/session/ses_1/permission/per_1/reply"),
            LocationSurface::V2Session
        );
        assert_eq!(surface(&get, "/api/health"), LocationSurface::None);
        assert_eq!(surface(&post, "/api/session"), LocationSurface::None);
        assert_eq!(surface(&get, "/api/event"), LocationSurface::None);
        assert_eq!(surface(&get, "/not-a-route"), LocationSurface::None);
    }
}
