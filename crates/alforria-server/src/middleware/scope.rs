//! The route → scope table for bearer credentials (alforria extension; see
//! `crate::bearer` for what the scopes mean).
//!
//! HTTP Basic and an unauthenticated server are the operator and are not
//! looked up here. A bearer credential is: its scope must cover the scope
//! its route is listed with, and **a route that is not listed needs
//! `admin`**. A new route is therefore closed to `chat` and `connector`
//! tokens until someone decides otherwise and writes it down below.
//!
//! Only the v1 surface and alforria's own routes are opened to narrower
//! scopes; the whole v2 `/api` surface is `admin`.
//!
//! The rule for what may be listed: **nothing below `admin` executes
//! anything except through the model, under the operator's permission
//! policy, and nothing below `admin` changes that policy.** So the routes
//! that run things directly are absent on purpose — the session `shell`
//! command, terminals, `/tui/*` remote control, workflow scripts, MCP
//! connect and OAuth — as are the ones that rewrite what the model will
//! read as its own history (message and part edits and deletes). A listed
//! route whose *payload* could cross that line is narrowed in its handler
//! (`crate::confine`): permission rulesets, the `tools` map, agent choice,
//! `always` replies, files and inline shell in commands.

use crate::bearer::Scope;
use crate::middleware::auth::route_matches;

use Scope::{Chat, Connector};

/// `(method, route, scope)` — the routes a token narrower than `admin` may
/// call. Anything absent needs `admin`: config, provider credentials, MCP
/// management, terminals, instance and global control, sync, the v2 API,
/// the web UI.
pub const ROUTES: &[(&str, &str, Scope)] = &[
    // --- chat: talk to the agent -------------------------------------
    ("GET", "/global/health", Chat),
    ("GET", "/event", Chat),
    ("GET", "/session", Chat),
    ("POST", "/session", Chat),
    ("GET", "/session/status", Chat),
    ("GET", "/session/{sessionID}", Chat),
    ("PATCH", "/session/{sessionID}", Chat),
    ("DELETE", "/session/{sessionID}", Chat),
    ("GET", "/session/{sessionID}/children", Chat),
    ("GET", "/session/{sessionID}/todo", Chat),
    ("GET", "/session/{sessionID}/diff", Chat),
    ("GET", "/session/{sessionID}/goal", Chat),
    ("GET", "/session/{sessionID}/message", Chat),
    ("POST", "/session/{sessionID}/message", Chat),
    ("GET", "/session/{sessionID}/message/{messageID}", Chat),
    ("POST", "/session/{sessionID}/prompt_async", Chat),
    ("POST", "/session/{sessionID}/command", Chat),
    ("POST", "/session/{sessionID}/abort", Chat),
    ("POST", "/session/{sessionID}/fork", Chat),
    ("POST", "/session/{sessionID}/init", Chat),
    ("POST", "/session/{sessionID}/summarize", Chat),
    ("POST", "/session/{sessionID}/revert", Chat),
    ("POST", "/session/{sessionID}/unrevert", Chat),
    (
        "POST",
        "/session/{sessionID}/permissions/{permissionID}",
        Chat,
    ),
    ("GET", "/permission", Chat),
    ("POST", "/permission/{requestID}/reply", Chat),
    ("GET", "/attention", Chat),
    ("GET", "/question", Chat),
    ("POST", "/question/{requestID}/reply", Chat),
    ("POST", "/question/{requestID}/reject", Chat),
    // Files, inside the project.
    ("GET", "/file", Chat),
    ("GET", "/file/content", Chat),
    ("GET", "/file/status", Chat),
    ("GET", "/find", Chat),
    ("GET", "/find/file", Chat),
    ("GET", "/find/symbol", Chat),
    ("POST", "/file/ticket", Chat),
    ("GET", "/file/raw", Chat),
    ("PUT", "/file/content", Chat),
    // What a chat client needs to draw itself.
    ("GET", "/agent", Chat),
    ("GET", "/command", Chat),
    ("GET", "/skill", Chat),
    ("GET", "/path", Chat),
    ("GET", "/project/current", Chat),
    ("GET", "/vcs", Chat),
    ("GET", "/vcs/status", Chat),
    ("GET", "/vcs/diff", Chat),
    // --- connector: chat, plus local automation ------------------------
    // Every directory's events on one stream.
    ("GET", "/global/event", Connector),
    ("POST", "/log", Connector),
    // Status reads.
    ("GET", "/mcp", Connector),
    ("GET", "/lsp", Connector),
    ("GET", "/formatter", Connector),
    ("GET", "/experimental/tool", Connector),
    ("GET", "/experimental/tool/ids", Connector),
    ("GET", "/experimental/resource", Connector),
    ("GET", "/project", Connector),
    ("GET", "/project/{projectID}/directories", Connector),
    // Workflows: watch and stop. Saving or starting one takes a script.
    ("GET", "/workflow", Connector),
    ("GET", "/workflow/run", Connector),
    ("GET", "/workflow/run/{runID}", Connector),
    ("POST", "/workflow/run/{runID}/wait", Connector),
    ("POST", "/workflow/run/{runID}/abort", Connector),
];

/// alforria's own routes that read `?directory=` themselves and fall back
/// to the server's working directory. They are outside the location
/// middleware, so the directory allowlist is applied to that fallback
/// before they run.
const CWD_ROUTES: &[&str] = &[
    "/workflow",
    "/workflow/{name}",
    "/workflow/run",
    "/workflow/run/{runID}",
    "/workflow/run/{runID}/wait",
    "/workflow/run/{runID}/abort",
    "/session/{sessionID}/goal",
    "/file/ticket",
    "/file/raw",
    "/file/content",
];

fn lookup_method(method: &str) -> &str {
    // axum answers HEAD from the GET handler.
    if method == "HEAD" {
        "GET"
    } else {
        method
    }
}

/// The scope a bearer credential needs for a request.
pub fn required(method: &str, path: &str) -> Scope {
    let method = lookup_method(method);
    ROUTES
        .iter()
        .find(|(route_method, route, _)| *route_method == method && route_matches(route, path))
        .map(|(_, _, scope)| *scope)
        .unwrap_or(Scope::Admin)
}

/// Whether the route resolves a missing `?directory=` to the server's
/// working directory on its own (see [`CWD_ROUTES`]).
pub fn defaults_to_cwd(path: &str) -> bool {
    CWD_ROUTES.iter().any(|route| route_matches(route, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::{v1, v2};

    #[test]
    fn unlisted_routes_need_admin() {
        assert_eq!(required("GET", "/nope"), Scope::Admin);
        assert_eq!(required("GET", "/"), Scope::Admin);
        assert_eq!(
            required("POST", "/session/ses_1/does-not-exist"),
            Scope::Admin
        );
        // A listed path under another method is not listed.
        assert_eq!(required("DELETE", "/event"), Scope::Admin);
        assert_eq!(required("PATCH", "/config"), Scope::Admin);
    }

    #[test]
    fn head_is_looked_up_as_get() {
        assert_eq!(required("HEAD", "/session"), Scope::Chat);
        assert_eq!(required("HEAD", "/config"), Scope::Admin);
    }

    /// The families a narrow token must never reach, whatever else changes.
    #[test]
    fn sensitive_families_are_admin_only() {
        let admin_only = |method: &str, route: &str| {
            route.starts_with("/api")
                || route.starts_with("/pty")
                || route.starts_with("/config")
                || route.starts_with("/auth")
                || route.starts_with("/provider")
                || route.starts_with("/sync")
                || route.starts_with("/instance")
                || route.starts_with("/experimental/worktree")
                || route.starts_with("/experimental/control-plane")
                || route.starts_with("/experimental/console")
                || (route.starts_with("/global")
                    && route != "/global/health"
                    && route != "/global/event")
                || (route.starts_with("/mcp") && method != "GET")
                || (route.starts_with("/mcp/"))
                || route == "/vcs/apply"
                || route == "/project/git/init"
                || (method == "PATCH" && route.starts_with("/project"))
                || route.ends_with("/share")
        };
        for (method, route) in v1::ROUTES.iter().chain(v2::ROUTES.iter()) {
            // Route patterns stand in for paths: `{param}` is a fine segment.
            if admin_only(method, route) {
                assert_eq!(
                    required(method, route),
                    Scope::Admin,
                    "{method} {route} must need admin"
                );
            }
        }
    }

    /// Routes that execute directly, drive another client, or rewrite the
    /// transcript are closed below `admin` — including families this table
    /// has never heard of.
    #[test]
    fn nothing_below_admin_executes_or_rewrites_history() {
        for (method, path) in [
            ("POST", "/session/ses_1/shell"),
            ("POST", "/pty"),
            ("GET", "/pty/pty_1/connect"),
            ("POST", "/tui/execute-command"),
            ("POST", "/tui/submit-prompt"),
            ("POST", "/tui/append-prompt"),
            ("POST", "/workflow"),
            ("POST", "/workflow/run"),
            ("DELETE", "/workflow/x"),
            ("POST", "/mcp/x/connect"),
            ("POST", "/mcp/x/disconnect"),
            ("POST", "/mcp/x/auth"),
            ("POST", "/mcp/x/auth/callback"),
            ("DELETE", "/session/ses_1/message/msg_1"),
            ("PATCH", "/session/ses_1/message/msg_1/part/prt_1"),
            ("DELETE", "/session/ses_1/message/msg_1/part/prt_1"),
            ("POST", "/session/ses_1/share"),
            ("POST", "/experimental/session/ses_1/background"),
            ("POST", "/experimental/worktree"),
            // Families other branches are adding, and ones nobody has yet.
            ("GET", "/trigger"),
            ("POST", "/trigger"),
            ("POST", "/trigger/trg_1/fire"),
            ("GET", "/memory"),
            ("PUT", "/memory/note"),
            ("POST", "/anything/at/all"),
        ] {
            assert_eq!(required(method, path), Scope::Admin, "{method} {path}");
        }
        for (method, route) in v1::ROUTES.iter() {
            if route.starts_with("/tui") || route.contains("/part/") {
                assert_eq!(required(method, route), Scope::Admin, "{method} {route}");
            }
        }
    }

    /// Every listed route exists: a typo here would silently open nothing,
    /// and a removed route should take its row with it.
    #[test]
    fn every_listed_route_is_a_real_route() {
        let extensions: &[(&str, &str)] = &[
            ("GET", "/workflow"),
            ("POST", "/workflow"),
            ("DELETE", "/workflow/{name}"),
            ("GET", "/workflow/run"),
            ("POST", "/workflow/run"),
            ("DELETE", "/workflow/run"),
            ("GET", "/workflow/run/{runID}"),
            ("POST", "/workflow/run/{runID}/wait"),
            ("POST", "/workflow/run/{runID}/abort"),
            ("GET", "/session/{sessionID}/goal"),
            ("GET", "/attention"),
            ("POST", "/file/ticket"),
            ("GET", "/file/raw"),
            ("PUT", "/file/content"),
        ];
        for (method, route, _) in ROUTES {
            let known = v1::ROUTES
                .iter()
                .chain(extensions.iter())
                .any(|(known_method, known_route)| known_method == method && known_route == route);
            assert!(known, "{method} {route} is listed but is not a route");
        }
        // And no row appears twice with different scopes.
        for (index, (method, route, _)) in ROUTES.iter().enumerate() {
            assert!(
                !ROUTES[..index]
                    .iter()
                    .any(|(other_method, other_route, _)| other_method == method
                        && other_route == route),
                "{method} {route} is listed twice"
            );
        }
    }

    #[test]
    fn chat_and_connector_rows_resolve() {
        assert_eq!(required("POST", "/session/ses_1/prompt_async"), Scope::Chat);
        assert_eq!(required("GET", "/event"), Scope::Chat);
        assert_eq!(required("POST", "/permission/per_1/reply"), Scope::Chat);
        assert_eq!(required("GET", "/global/event"), Scope::Connector);
        assert_eq!(
            required("POST", "/workflow/run/run_1/abort"),
            Scope::Connector
        );
        assert!(defaults_to_cwd("/workflow/run"));
        assert!(defaults_to_cwd("/session/ses_1/goal"));
        assert!(!defaults_to_cwd("/session/ses_1"));
    }
}
