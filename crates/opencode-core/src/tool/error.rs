//! `ToolError` — error taxonomy for the tool system (spec §2.1).
//!
//! Mirrors the TS taxonomy from `tool/tool.ts` and the session loop's
//! error mapping:
//!
//! * `InvalidArguments` — `ToolInvalidArgumentsError` (tool.ts:24-34), the
//!   model-facing "rewrite the input" error. The `message` body is byte-exact.
//! * `Failed` — a thrown `Error` inside execute (TS `Effect.orDie` defect);
//!   carries the TS `Error.message` string unchanged.
//! * `Permission` — `ctx.ask()` was rejected / corrected / denied upstream.
//!   Carries the exact `PermissionV1` Rejected/Corrected message text.
//! * `Aborted` — the abort signal fired mid-execution.

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// ToolInvalidArgumentsError — model-facing "rewrite the input" error
    /// (tool.ts:24-34).
    #[error(
        "The {tool} tool was called with invalid arguments: {detail}.\nPlease rewrite the input so it satisfies the expected schema."
    )]
    InvalidArguments { tool: String, detail: String },

    /// A thrown `Error` inside execute (TS Effect.orDie defect). The session
    /// loop (M5) converts this into an error tool-part carrying `message`.
    #[error("{0}")]
    Failed(String),

    /// ctx.ask() was rejected / corrected / denied upstream. Carries the exact
    /// PermissionV1 Rejected/Corrected/Denied message text.
    #[error("{0}")]
    Permission(String),

    /// `ctx.ask()` failed with `PermissionV1.RejectedError` /
    /// `Question.RejectedError` (processor.ts:200-201 blocks the loop only
    /// for these).
    #[error("{0}")]
    Rejected(String),

    /// The abort signal fired mid-execution.
    #[error("Aborted")]
    Aborted,
}
