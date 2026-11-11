//! Keeping a credential narrower than `admin` inside the project
//! (alforria extension).
//!
//! What a prompt or command from such a credential may contain — agent,
//! tools, files, inline shell — is not decided here: a route passes
//! [`caller`] into the engine, which judges the values it actually resolves
//! (`alforria_core::session::caller`). What remains at the HTTP layer are
//! the requests that never become a prompt: setting a session's permission
//! rules, answering a permission ask `always`, listing a directory.

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

/// A credential below `admin` answers the asks of restricted sessions
/// only. An ask in an operator's session is the operator's turn waiting;
/// answering it would steer that turn. An unknown session id is refused
/// too.
pub fn check_ask_session(
    ctx: &crate::state::ServerContext,
    session_id: Option<&str>,
) -> Result<(), ServerError> {
    let restricted = session_id
        .map(|session_id| ctx.sessions.is_restricted(session_id).unwrap_or(false))
        .unwrap_or(false);
    if restricted {
        Ok(())
    } else {
        Err(forbid(
            "this request belongs to an operator's session: this token may not answer it",
        ))
    }
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
