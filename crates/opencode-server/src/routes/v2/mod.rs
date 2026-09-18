//! v2 (`/api/*`) route table — `packages/protocol/src/api.ts` group order,
//! cross-checked against the frozen OpenAPI fixture. The project-copy group
//! root is the *un-prefixed* `/experimental/project/:projectID/copy`
//! (`protocol/groups/project-copy.ts:14`).

/// `(method, path)` pairs in registration order. 61 endpoints.
pub const ROUTES: &[(&str, &str)] = &[
    // ---- health (`protocol/groups/health.ts`); handlers in M6.7 ----
    ("GET", "/api/health"),
    // ---- location (`protocol/groups/location.ts`); handlers in M6.7 ----
    ("GET", "/api/location"),
    // ---- agent (`protocol/groups/agent.ts`); handlers in M6.7 ----
    ("GET", "/api/agent"),
    // ---- session (`protocol/groups/session.ts`, 17); handlers in M6.7 ----
    ("GET", "/api/session"),
    ("POST", "/api/session"),
    ("GET", "/api/session/active"),
    ("GET", "/api/session/{sessionID}"),
    ("POST", "/api/session/{sessionID}/agent"),
    ("POST", "/api/session/{sessionID}/model"),
    ("POST", "/api/session/{sessionID}/prompt"),
    ("POST", "/api/session/{sessionID}/compact"),
    ("POST", "/api/session/{sessionID}/wait"),
    ("POST", "/api/session/{sessionID}/revert/stage"),
    ("POST", "/api/session/{sessionID}/revert/clear"),
    ("POST", "/api/session/{sessionID}/revert/commit"),
    ("GET", "/api/session/{sessionID}/context"),
    ("GET", "/api/session/{sessionID}/history"),
    ("GET", "/api/session/{sessionID}/event"),
    ("POST", "/api/session/{sessionID}/interrupt"),
    ("GET", "/api/session/{sessionID}/message/{messageID}"),
    // ---- message (`protocol/groups/message.ts`); handlers in M6.7 ----
    ("GET", "/api/session/{sessionID}/message"),
    // ---- model (`protocol/groups/model.ts`); handlers in M6.7 ----
    ("GET", "/api/model"),
    // ---- provider (`protocol/groups/provider.ts`); handlers in M6.7 ----
    ("GET", "/api/provider"),
    ("GET", "/api/provider/{providerID}"),
    // ---- integration (`protocol/groups/integration.ts`); M7 seam-deferred ----
    ("GET", "/api/integration"),
    ("GET", "/api/integration/{integrationID}"),
    ("POST", "/api/integration/{integrationID}/connect/key"),
    ("POST", "/api/integration/{integrationID}/connect/oauth"),
    ("GET", "/api/integration/attempt/{attemptID}"),
    ("DELETE", "/api/integration/attempt/{attemptID}"),
    ("POST", "/api/integration/attempt/{attemptID}/complete"),
    // ---- credential (`protocol/groups/credential.ts`); M7 seam-deferred ----
    ("DELETE", "/api/credential/{credentialID}"),
    ("PATCH", "/api/credential/{credentialID}"),
    // ---- permission (`protocol/groups/permission.ts`, 7); handlers in M6.7 ----
    ("GET", "/api/permission/request"),
    ("GET", "/api/permission/saved"),
    ("DELETE", "/api/permission/saved/{id}"),
    ("GET", "/api/session/{sessionID}/permission"),
    ("POST", "/api/session/{sessionID}/permission"),
    ("GET", "/api/session/{sessionID}/permission/{requestID}"),
    (
        "POST",
        "/api/session/{sessionID}/permission/{requestID}/reply",
    ),
    // ---- fs (`protocol/groups/fs.ts`); handlers in M6.7 ----
    ("GET", "/api/fs/read/{*path}"),
    ("GET", "/api/fs/list"),
    ("GET", "/api/fs/find"),
    // ---- command (`protocol/groups/command.ts`); handlers in M6.7 ----
    ("GET", "/api/command"),
    // ---- skill (`protocol/groups/skill.ts`); handlers in M6.7 ----
    ("GET", "/api/skill"),
    // ---- event (`protocol/groups/event.ts`; SSE); stream in M6.4 ----
    ("GET", "/api/event"),
    // ---- pty (`protocol/groups/pty.ts`); service + connect WS in M6.8 ----
    ("GET", "/api/pty"),
    ("POST", "/api/pty"),
    ("GET", "/api/pty/{ptyID}"),
    ("PUT", "/api/pty/{ptyID}"),
    ("DELETE", "/api/pty/{ptyID}"),
    ("POST", "/api/pty/{ptyID}/connect-token"),
    ("GET", "/api/pty/{ptyID}/connect"),
    // ---- question (`protocol/groups/question.ts`, 4); handlers in M6.7 ----
    ("GET", "/api/question/request"),
    ("GET", "/api/session/{sessionID}/question"),
    (
        "POST",
        "/api/session/{sessionID}/question/{requestID}/reply",
    ),
    (
        "POST",
        "/api/session/{sessionID}/question/{requestID}/reject",
    ),
    // ---- reference (`protocol/groups/reference.ts`); M7 seam-deferred ----
    ("GET", "/api/reference"),
    // ---- project-copy (`protocol/groups/project-copy.ts`; M7) — root is
    // `/experimental/project/:projectID/copy`, *not* under `/api`. ----
    ("POST", "/experimental/project/{projectID}/copy"),
    ("DELETE", "/experimental/project/{projectID}/copy"),
    ("POST", "/experimental/project/{projectID}/copy/refresh"),
];
