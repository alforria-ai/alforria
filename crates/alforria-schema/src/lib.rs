//! Pure wire DTOs for the opencode server, events, and sessions.
//!
//! Wire compatibility is the law: every JSON field name, union tag, and
//! optional-vs-omitted semantic must match the TS reference exactly, as captured
//! in `fixtures/openapi/openapi.json`. No business logic lives here.
//!
//! Conventions (see `scratchpad/specs/M1.md` §2):
//!
//! - Default casing is camelCase: `#[serde(rename_all = "camelCase")]`.
//! - `…ID` fields need explicit `#[serde(rename = "…ID")]` renames.
//! - TS `optional()` means the key is omitted when absent: `Option<T>` with
//!   `#[serde(default, skip_serializing_if = "Option::is_none")]`.

// M1.1 — Foundation modules
pub mod file_diff;
pub mod ids;
pub mod js_number;
pub mod llm;
pub mod location;
pub mod prompt;
pub mod prompt_input;
pub mod revert;
pub mod schema;

// M1.2 — Provider & model
pub mod model;
pub mod provider;

// M1.3 — Connection, credential, integration
pub mod connection;
pub mod credential;
pub mod integration;

// M1.4 — Permission & Question (v2)
pub mod permission;
pub mod question;

// M1.5 — Agent, command, skill, project, workspace, reference, delivery
pub mod agent;
pub mod command;
pub mod project;
pub mod reference;
pub mod session_delivery;
pub mod skill;
pub mod workspace;

// M1.6 — Session v2 core
pub mod session;
pub mod session_input;
pub mod session_status;
pub mod session_todo;

// M1.7 — Session v2 messages
pub mod session_message;

// M1.8 — Session v2 events
pub mod session_event;

// M1.9 — Small event payloads & PTY
pub mod catalog;
pub mod filesystem;
pub mod filesystem_watcher;
pub mod ide_event;
pub mod installation_event;
pub mod legacy_event;
pub mod lsp_event;
pub mod mcp;
pub mod mcp_event;
pub mod models_dev;
pub mod plugin;
pub mod pty;
pub mod pty_ticket;
pub mod server_event;
pub mod session_compaction_event;
pub mod tui_event;
pub mod vcs_event;

// M1.10 — Session v1
pub mod permission_v1;
pub mod question_v1;
pub mod session_v1;

// M1.11 — Envelopes & manifest unions
pub mod event;
pub mod event_manifest;
