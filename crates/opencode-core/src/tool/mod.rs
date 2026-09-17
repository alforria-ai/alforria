//! The tool system — registry, seam, truncation, guards and all built-in
//! tools (spec M4). Ported from `packages/opencode/src/tool/`.

pub mod apply_patch;
pub mod bom;
pub mod code_mode;
pub mod def;
pub mod diff;
pub mod edit;
pub mod error;
pub mod external_directory;
pub mod glob;
pub mod grep;
pub mod invalid;
pub mod lsp;
pub mod mcp_websearch;
pub mod patch_parser;
pub mod permission;
pub mod plan;
pub mod question;
pub mod read;
pub mod registry;
pub mod ripgrep;
pub mod shell;
pub mod skill;
pub mod task;
pub mod todo;
pub mod truncate;
pub mod truncation_dir;
pub mod webfetch;
pub mod websearch;
pub mod write;

pub use def::{
    define, AgentInfo, AgentMode, Agents, Ask, AskRequest, Attachment, BoxFuture, ExecuteFn,
    ExecuteResult, Extra, FormatValidationError, InstanceContext, MetadataInput, MetadataSink,
    ToolCtxRef, ToolDef,
};
pub use error::ToolError;
pub use external_directory::{assert_external_directory, contains_path, ExternalOptions, Kind};
pub use registry::{web_search_enabled, CodeModeDescriber, RuntimeFlags, ToolModel, ToolRegistry};
pub use ripgrep::{GrepMatch, Ripgrep, RipgrepService};
pub use truncate::{
    Direction, Options, TruncResult, Truncate, TruncateService, MAX_BYTES, MAX_LINES, RETENTION,
};
pub use truncation_dir::truncation_dir;
