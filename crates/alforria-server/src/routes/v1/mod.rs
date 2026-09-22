//! v1 (unprefixed) route table — every family from `httpapi/api.ts`, in
//! `RootHttpApi`/`InstanceHttpApi` registration order.
//!
//! Route inventory source: the TS group files, cross-checked against the
//! frozen OpenAPI fixture (`fixtures/openapi/openapi.json`).

pub mod config_permission_question;
pub mod global_control;
pub mod mcp;
pub mod project;
pub mod project_copy;
pub mod provider;
pub mod session;
pub mod sync;
pub mod util;
pub mod workspace;

/// `(method, path)` pairs in registration order
/// (`httpapi/api.ts:54-77`). 127 endpoints.
pub const ROUTES: &[(&str, &str)] = &[
    // ---- control (`groups/control.ts:31-34`); handlers in M6.6 ----
    ("PUT", "/auth/{providerID}"),
    ("DELETE", "/auth/{providerID}"),
    ("POST", "/log"),
    // ---- control-plane (`groups/control-plane.ts`); M7 seam-deferred ----
    ("POST", "/experimental/control-plane/move-session"),
    // ---- global (`groups/global.ts:68-74`); handlers in M6.6 ----
    ("GET", "/global/health"),
    ("GET", "/global/event"),
    ("GET", "/global/config"),
    ("PATCH", "/global/config"),
    ("POST", "/global/dispose"),
    ("POST", "/global/upgrade"),
    // ---- event SSE (`groups/event.ts:7`); stream in M6.4 ----
    ("GET", "/event"),
    // ---- config (`groups/config.ts`); handlers in M6.6 ----
    ("GET", "/config"),
    ("PATCH", "/config"),
    ("GET", "/config/providers"),
    // ---- experimental (`groups/experimental.ts:90-102`); handlers in M6.6
    //      (console/worktree stay seam-deferred to M7) ----
    ("GET", "/experimental/capabilities"),
    ("GET", "/experimental/console"),
    ("GET", "/experimental/console/orgs"),
    ("POST", "/experimental/console/switch"),
    ("GET", "/experimental/tool"),
    ("GET", "/experimental/tool/ids"),
    ("GET", "/experimental/worktree"),
    ("POST", "/experimental/worktree"),
    ("DELETE", "/experimental/worktree"),
    ("POST", "/experimental/worktree/reset"),
    ("GET", "/experimental/session"),
    ("POST", "/experimental/session/{sessionID}/background"),
    ("GET", "/experimental/resource"),
    // ---- file (`groups/file.ts:95-102`); handlers in M6.6 ----
    ("GET", "/find"),
    ("GET", "/find/file"),
    ("GET", "/find/symbol"),
    ("GET", "/file"),
    ("GET", "/file/content"),
    ("GET", "/file/status"),
    // ---- instance (`groups/instance.ts:43-56`); handlers in M6.6 ----
    ("POST", "/instance/dispose"),
    ("GET", "/path"),
    ("GET", "/vcs"),
    ("GET", "/vcs/status"),
    ("GET", "/vcs/diff"),
    ("GET", "/vcs/diff/raw"),
    ("POST", "/vcs/apply"),
    ("GET", "/command"),
    ("GET", "/agent"),
    ("GET", "/skill"),
    ("GET", "/lsp"),
    ("GET", "/formatter"),
    // ---- mcp (`groups/mcp.ts:32-39`); M7 seam-deferred ----
    ("GET", "/mcp"),
    ("POST", "/mcp"),
    ("POST", "/mcp/{name}/auth"),
    ("DELETE", "/mcp/{name}/auth"),
    ("POST", "/mcp/{name}/auth/callback"),
    ("POST", "/mcp/{name}/auth/authenticate"),
    ("POST", "/mcp/{name}/connect"),
    ("POST", "/mcp/{name}/disconnect"),
    // ---- project (`groups/project.ts`); handlers in M6.6 ----
    ("GET", "/project"),
    ("GET", "/project/current"),
    ("POST", "/project/git/init"),
    ("PATCH", "/project/{projectID}"),
    ("GET", "/project/{projectID}/directories"),
    // ---- project-copy (`groups/project-copy.ts`); M7 seam-deferred ----
    (
        "POST",
        "/experimental/project/{projectID}/copy/generate-name",
    ),
    // ---- pty (`groups/pty.ts:29-38`); service + connect WS land in M6.8.
    //      TS registers the connect endpoint via `PtyConnectApi` after the v2
    //      `ServerApi` (httpapi/api.ts:79-87) — no path conflicts, so it lives
    //      here. ----
    ("GET", "/pty/shells"),
    ("GET", "/pty"),
    ("POST", "/pty"),
    ("GET", "/pty/{ptyID}"),
    ("PUT", "/pty/{ptyID}"),
    ("DELETE", "/pty/{ptyID}"),
    ("POST", "/pty/{ptyID}/connect-token"),
    ("GET", "/pty/{ptyID}/connect"),
    // ---- question (`groups/question.ts`); handlers in M6.6 ----
    ("GET", "/question"),
    ("POST", "/question/{requestID}/reply"),
    ("POST", "/question/{requestID}/reject"),
    // ---- permission (`groups/permission.ts`); handlers in M6.6 ----
    ("GET", "/permission"),
    ("POST", "/permission/{requestID}/reply"),
    // ---- provider (`groups/provider.ts:12,38-77`); handlers in M6.6 ----
    ("GET", "/provider"),
    ("GET", "/provider/auth"),
    ("POST", "/provider/{providerID}/oauth/authorize"),
    ("POST", "/provider/{providerID}/oauth/callback"),
    // ---- session (`groups/session.ts:78-105`; 27 endpoints); handlers in M6.5 ----
    ("GET", "/session"),
    ("POST", "/session"),
    ("GET", "/session/status"),
    ("GET", "/session/{sessionID}"),
    ("DELETE", "/session/{sessionID}"),
    ("PATCH", "/session/{sessionID}"),
    ("GET", "/session/{sessionID}/children"),
    ("GET", "/session/{sessionID}/todo"),
    ("GET", "/session/{sessionID}/diff"),
    ("GET", "/session/{sessionID}/message"),
    ("POST", "/session/{sessionID}/message"),
    ("GET", "/session/{sessionID}/message/{messageID}"),
    ("DELETE", "/session/{sessionID}/message/{messageID}"),
    ("POST", "/session/{sessionID}/fork"),
    ("POST", "/session/{sessionID}/abort"),
    ("POST", "/session/{sessionID}/init"),
    ("POST", "/session/{sessionID}/share"),
    ("DELETE", "/session/{sessionID}/share"),
    ("POST", "/session/{sessionID}/summarize"),
    ("POST", "/session/{sessionID}/prompt_async"),
    ("POST", "/session/{sessionID}/command"),
    ("POST", "/session/{sessionID}/shell"),
    ("POST", "/session/{sessionID}/revert"),
    ("POST", "/session/{sessionID}/unrevert"),
    ("POST", "/session/{sessionID}/permissions/{permissionID}"),
    (
        "DELETE",
        "/session/{sessionID}/message/{messageID}/part/{partID}",
    ),
    (
        "PATCH",
        "/session/{sessionID}/message/{messageID}/part/{partID}",
    ),
    // ---- sync (`groups/sync.ts:38-43`); M7 seam-deferred (durable-bus-backed) ----
    ("POST", "/sync/start"),
    ("POST", "/sync/replay"),
    ("POST", "/sync/steal"),
    ("POST", "/sync/history"),
    // ---- tui (`groups/tui.ts:36-50`); handlers in M6.6 ----
    ("POST", "/tui/append-prompt"),
    ("POST", "/tui/open-help"),
    ("POST", "/tui/open-sessions"),
    ("POST", "/tui/open-themes"),
    ("POST", "/tui/open-models"),
    ("POST", "/tui/submit-prompt"),
    ("POST", "/tui/clear-prompt"),
    ("POST", "/tui/execute-command"),
    ("POST", "/tui/show-toast"),
    ("POST", "/tui/publish"),
    ("POST", "/tui/select-session"),
    ("GET", "/tui/control/next"),
    ("POST", "/tui/control/response"),
    // ---- workspace (`groups/workspace.ts:40-47`); M7 seam-deferred ----
    ("GET", "/experimental/workspace/adapter"),
    ("GET", "/experimental/workspace"),
    ("POST", "/experimental/workspace"),
    ("POST", "/experimental/workspace/sync-list"),
    ("GET", "/experimental/workspace/status"),
    ("DELETE", "/experimental/workspace/{id}"),
    ("POST", "/experimental/workspace/warp"),
];
