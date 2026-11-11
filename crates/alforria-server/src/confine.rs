//! Keeping a credential narrower than `admin` inside the project
//! (alforria extension).
//!
//! What a prompt or command from such a credential may contain — agent,
//! tools, files, inline shell — is not decided here: a route passes
//! [`caller`] into the engine, which judges the values it actually resolves
//! (`alforria_core::session::caller`). What remains at the HTTP layer are
//! the requests that never become a prompt: setting a session's permission
//! rules, answering a permission ask `always`, listing a directory — and
//! every other write to a session, which goes through one guard
//! ([`guard_session_writes`]) and one predicate ([`may_act_on`]).

use std::path::{Path, PathBuf};

use axum::Extension;

use crate::bearer::Principal;
use crate::error::{ApiError, ServerError};
use crate::middleware::location::LocationContext;

/// The caller a request's prompts and commands are run as. This is all a
/// route does about restriction: what the caller may ask for is decided in
/// core, where the effective agent, tools and files are resolved
/// (`alforria_core::session::caller`).
pub fn caller(principal: &Option<Extension<Principal>>) -> alforria_core::session::caller::Caller {
    if restricted(principal) {
        alforria_core::session::caller::Caller::Restricted
    } else {
        alforria_core::session::caller::Caller::Operator
    }
}

/// Whether the request's credential is confined to the project. No
/// principal means the operator (no authentication, or a public route).
pub fn restricted(principal: &Option<Extension<Principal>>) -> bool {
    principal
        .as_ref()
        .is_some_and(|principal| principal.0.restricted())
}

/// The project a location stands for: its directory, and its git worktree
/// when it has one (`containsPath`, `instance-context.ts:18-24` — a non-git
/// project's worktree is `/`, which is not a boundary).
///
/// The roots are where the kernel finds those directories
/// (`protect::resolve`): a project that does not exist yet behind a
/// symlinked parent is where the link leads, which is where its files will
/// be created and where [`resolve_for_access`] reports them.
pub fn project_roots(location: &LocationContext) -> Vec<PathBuf> {
    let worktree = location
        .services
        .instance_location()
        .map(|instance| instance.worktree.clone())
        .unwrap_or_default();
    alforria_core::session::caller::project_roots(&location.directory, &worktree)
}

pub use alforria_core::session::caller::resolve_for_access;

/// Whether `path` (absolute), as the kernel would resolve that very
/// spelling, is inside the location's project. A path that cannot be
/// resolved is not.
pub fn inside_project(location: &LocationContext, path: &Path) -> bool {
    resolve_for_access(path).is_ok_and(|resolved| {
        project_roots(location)
            .iter()
            .any(|root| alforria_core::protect::within(&resolved, root))
    })
}

fn forbid(message: impl Into<String>) -> ServerError {
    ServerError::Api(ApiError::Forbidden {
        message: message.into(),
    })
}

/// A credential below `admin` cannot hand a session a permission ruleset
/// (session create and update take one): it would be a way to allow itself
/// what the operator's policy asks about or denies.
pub fn check_no_ruleset(supplied: bool) -> Result<(), ServerError> {
    if supplied {
        Err(forbid(
            "this token may not set a session's permission rules: only the admin scope can",
        ))
    } else {
        Ok(())
    }
}

/// Edge validation of an agent a caller below `admin` names on a new
/// session: one a client's picker offers, not hidden and not subagent-only.
/// The check that must be complete is the engine's, on the agent a turn
/// actually runs as (`alforria_core::session::caller`); this one turns a
/// request that could never run into an immediate 403.
pub fn check_agent(location: &LocationContext, name: Option<&str>) -> Result<(), ServerError> {
    let Some(agent) = name.and_then(|name| location.services.agents.get(name)) else {
        return Ok(());
    };
    alforria_core::session::caller::Caller::Restricted
        .bounds(true)
        .check_agent(agent)
        .map_err(forbid)
}

/// `always` on a permission reply adds allow rules that outlive the ask
/// and apply to every session of the project. A credential below `admin`
/// may answer `once` or `reject`; `always` only where the host config says
/// so (`server.auth.bearer.reply_always`).
pub fn check_reply(
    ctx: &crate::state::ServerContext,
    reply: alforria_schema::permission_v1::PermissionV1Reply,
) -> Result<(), ServerError> {
    if reply == alforria_schema::permission_v1::PermissionV1Reply::Always
        && !ctx.policy.reply_always
    {
        return Err(forbid(
            "this token may answer a permission request once or reject it, but not always: \
             a standing rule needs the admin scope (or server.auth.bearer.reply_always)",
        ));
    }
    Ok(())
}

/// **Whether a credential below `admin` may act on a session**: the one
/// predicate behind every refusal of a write to a session — the guard over
/// the session routes, an answer to a pending ask, what `/attention`,
/// `GET /permission` and `GET /question` show such a credential, and a
/// session a memory write names. It is `Caller::may_act_on`
/// (`alforria_core::session::caller`) asked of the session's stored
/// restriction: restricted sessions only. A restriction that cannot be
/// read, or a session that does not exist, is not one it may act on.
pub fn may_act_on(ctx: &crate::state::ServerContext, session_id: &str) -> Result<(), String> {
    alforria_core::session::caller::Caller::Restricted
        .may_act_on(ctx.sessions.is_restricted(session_id).unwrap_or(false))
}

/// Requests to a `{sessionID}` route that are not an act *in* that session
/// although they are not a `GET`. A credential below `admin` may make them
/// on an operator's session.
///
/// * `fork`: the source is read, not written; the fork is a session of the
///   caller's own, created restricted.
///
/// Everything else under a session id is refused there by
/// [`guard_session_writes`]. A route added later is covered without being
/// listed anywhere: it has to be listed *here* to be let through.
pub const NOT_AN_ACT_IN_THE_SESSION: &[(&str, &str)] = &[("POST", "/session/{sessionID}/fork")];

/// The name every route gives the session in its path. The guard keys on
/// it (`routes::tests` holds the tables to it).
pub const SESSION_PARAM: &str = "sessionID";

/// The guard over every route that names a session, in both generations
/// and alforria's own (`routes::build_router` puts it on the whole
/// router): a credential below `admin` writes only to sessions it may act
/// on ([`may_act_on`]). Title, metadata, archive, delete, share, revert,
/// summarize, abort, init, command, shell, a message or part edit — not
/// only prompts, which the engine refuses by itself as well.
///
/// It runs after routing, on the route that matched and the session id as
/// the handler will read it — percent-decoded by the same extractor. (The
/// check used to sit in the location middleware and looked the session up
/// under the raw path segment: `/session/ses%5F…` named no session there,
/// so nothing was refused, and then named the operator's session to the
/// handler.)
///
/// A session that does not exist is the handler's to answer (404): there
/// is nothing of the operator's to protect, and the handler reads the same
/// id.
pub async fn guard_session_writes(
    axum::extract::State(ctx): axum::extract::State<std::sync::Arc<crate::state::ServerContext>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::extract::FromRequestParts;
    use axum::response::IntoResponse;
    let is_restricted = request
        .extensions()
        .get::<Principal>()
        .is_some_and(|principal| principal.restricted());
    let method = request.method().clone();
    if !is_restricted || matches!(method, axum::http::Method::GET | axum::http::Method::HEAD) {
        return next.run(request).await;
    }
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|matched| matched.as_str().to_string())
        .unwrap_or_default();
    if !route.contains(&format!("{{{SESSION_PARAM}}}"))
        || NOT_AN_ACT_IN_THE_SESSION
            .iter()
            .any(|(exempt_method, exempt)| *exempt_method == method.as_str() && *exempt == route)
    {
        return next.run(request).await;
    }
    let (mut parts, body) = request.into_parts();
    let params =
        axum::extract::Path::<std::collections::HashMap<String, String>>::from_request_parts(
            &mut parts, &ctx,
        )
        .await;
    // A session id that cannot be read off the route is not one the caller
    // may act on.
    let session_id = params
        .ok()
        .and_then(|axum::extract::Path(mut params)| params.remove(SESSION_PARAM));
    let verdict = match &session_id {
        None => Err("this token may not act in a session that cannot be named".to_string()),
        Some(session_id) => match ctx.sessions.get(session_id) {
            Err(alforria_core::SessionError::NotFound(_)) => Ok(()),
            Err(_) => Err(format!("session {session_id}: it cannot be read")),
            Ok(_) => may_act_on(&ctx, session_id).map_err(|_| {
                format!(
                    "session {session_id}: it belongs to the operator, and this token can \
                     read or fork it but not act in it"
                )
            }),
        },
    };
    match verdict {
        Ok(()) => {
            next.run(axum::extract::Request::from_parts(parts, body))
                .await
        }
        Err(message) => forbid(message).into_response(),
    }
}

/// A credential below `admin` answers the asks of sessions it may act on
/// ([`may_act_on`]) only. An ask in an operator's session is the
/// operator's turn waiting; answering it would steer that turn. An unknown
/// session id is refused too.
pub fn check_ask_session(
    ctx: &crate::state::ServerContext,
    session_id: Option<&str>,
) -> Result<(), ServerError> {
    if session_id.is_some_and(|session_id| may_act_on(ctx, session_id).is_ok()) {
        Ok(())
    } else {
        Err(forbid(
            "this request belongs to an operator's session: this token may not answer it",
        ))
    }
}

/// What a list of pending asks or questions shows a caller: everything to
/// the operator, and to a credential below `admin` the ones it may answer
/// ([`may_act_on`] the session each belongs to) — the line `/attention`
/// draws, on the frozen `GET /permission` and `GET /question` too. The
/// lists keep their shape; an entry is there or it is not.
pub fn answerable<T>(
    ctx: &crate::state::ServerContext,
    principal: &Option<Extension<Principal>>,
    pending: Vec<T>,
    session_of: impl Fn(&T) -> &str,
) -> Vec<T> {
    if !restricted(principal) {
        return pending;
    }
    pending
        .into_iter()
        .filter(|item| may_act_on(ctx, session_of(item)).is_ok())
        .collect()
}

/// Which kind of pending request a reply names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    Permission,
    Question,
}

/// Authorise an answer to a pending permission ask or question, against
/// the session the ask actually belongs to — not against whatever session
/// the request happens to name. Every v1 reply and reject route goes
/// through this before it touches the ask:
///
/// * when the route carries a session id, the ask must belong to that
///   session; if it does not, the answer is the same 404 an unknown ask
///   gets, so the route says nothing about asks elsewhere;
/// * for a credential below `admin`, the ask's own session must be one it
///   may act in (restricted, `check_ask_session`).
///
/// An ask that is not pending anywhere is left for the handler's own 404.
pub fn authorise_answer(
    ctx: &crate::state::ServerContext,
    principal: &Option<Extension<Principal>>,
    kind: Ask,
    request_id: &str,
    path_session: Option<&str>,
) -> Result<(), ServerError> {
    let not_found = || {
        ServerError::Api(match kind {
            Ask::Permission => ApiError::PermissionNotFound {
                request_id: request_id.to_string(),
                message: format!("Permission request not found: {request_id}"),
            },
            Ask::Question => ApiError::QuestionNotFound {
                request_id: request_id.to_string(),
                message: format!("Question request not found: {request_id}"),
            },
        })
    };
    let owner = ask_session(ctx, kind, request_id);
    if let Some(path_session) = path_session {
        if owner.as_deref() != Some(path_session) {
            return Err(not_found());
        }
    }
    if restricted(principal) {
        if let Some(owner) = &owner {
            check_ask_session(ctx, Some(owner))?;
        }
    }
    Ok(())
}

/// The session a pending permission ask or question belongs to, wherever
/// it is loaded.
pub fn ask_session(
    ctx: &crate::state::ServerContext,
    kind: Ask,
    request_id: &str,
) -> Option<String> {
    ctx.instances
        .cached()
        .iter()
        .find_map(|services| match kind {
            Ask::Permission => services
                .permission
                .list()
                .into_iter()
                .find(|request| request.id == request_id)
                .map(|request| request.session_id),
            Ask::Question => services
                .question
                .list()
                .into_iter()
                .find(|request| request.id == request_id)
                .map(|request| request.session_id),
        })
}

/// Whether an instance belongs to a directory a credential below `admin`
/// may open.
pub fn instance_allowed(
    ctx: &crate::state::ServerContext,
    services: &alforria_core::SessionServices,
) -> bool {
    services
        .instance_location()
        .is_some_and(|instance| ctx.policy.directories.allows(&instance.directory))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_lands_where_the_kernel_does() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("project/src")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();

        let resolve_for_access = |path: &Path| resolve_for_access(path).ok();
        assert_eq!(
            resolve_for_access(&root.join("project/src/../new/file.txt")),
            Some(root.join("project/new/file.txt"))
        );
        assert_eq!(
            resolve_for_access(&root.join("project/../outside/x")),
            Some(root.join("outside/x"))
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("outside"), root.join("project/link")).unwrap();
            assert_eq!(
                resolve_for_access(&root.join("project/link/new/file.txt")),
                Some(root.join("outside/new/file.txt")),
                "a link in the middle of the path is followed"
            );
            // `..` after a link is taken from where the link led.
            std::fs::create_dir_all(root.join("outside/deep")).unwrap();
            std::os::unix::fs::symlink(root.join("outside/deep"), root.join("project/down"))
                .unwrap();
            assert_eq!(
                resolve_for_access(&root.join("project/down/../x")),
                Some(root.join("outside/x"))
            );
            // …and after something that does not exist it has no answer.
            assert_eq!(resolve_for_access(&root.join("project/no/../x")), None);
        }
    }
}
