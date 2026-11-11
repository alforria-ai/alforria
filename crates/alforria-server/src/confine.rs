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
use crate::state::resolve_directory;

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
pub fn project_roots(location: &LocationContext) -> Vec<PathBuf> {
    let mut roots = vec![resolve_directory(&location.directory)];
    if let Some(instance) = location.services.instance_location() {
        if instance.worktree != Path::new("/") && !instance.worktree.as_os_str().is_empty() {
            let worktree = resolve_directory(&instance.worktree);
            if !roots.contains(&worktree) {
                roots.push(worktree);
            }
        }
    }
    roots
}

pub use alforria_core::session::caller::resolve_for_access;

/// Whether `path` (absolute) is inside the location's project once every
/// existing symlink on the way is followed.
pub fn inside_project(location: &LocationContext, path: &Path) -> bool {
    let resolved = resolve_for_access(path);
    project_roots(location)
        .iter()
        .any(|root| resolved.starts_with(root))
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
    fn resolution_follows_existing_links_and_keeps_the_missing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("project/src")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();

        assert_eq!(
            resolve_for_access(&root.join("project/src/../new/file.txt")),
            root.join("project/new/file.txt")
        );
        assert_eq!(
            resolve_for_access(&root.join("project/../outside/x")),
            root.join("outside/x")
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("outside"), root.join("project/link")).unwrap();
            assert_eq!(
                resolve_for_access(&root.join("project/link/new/file.txt")),
                root.join("outside/new/file.txt"),
                "a link in the middle of the path is followed"
            );
        }
    }
}
