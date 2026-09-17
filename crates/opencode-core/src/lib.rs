//! Session engine core: config loading, models.dev catalog, storage, event bus.
//!
//! This crate owns the `opencode.json` contract (JSONC parsing, variable
//! substitution, multi-source precedence, deep-merge semantics), the
//! models.dev catalog (fetch + TTL cache), SQLite storage and the durable
//! `EventV2` bus. Wire compatibility with the pinned TS commit is the law:
//! every JSON shape emitted here must match what the TS reference accepts.

pub mod catalog;
pub mod config;
pub mod event;
pub mod format;
pub mod jsonc;
pub mod merge;
pub mod paths;
pub mod session;
pub mod storage;
pub mod tool;

use std::path::PathBuf;

pub use catalog::{CatalogConfig, CatalogService, Clock, Fetcher};
pub use config::{
    substitute, AuthProvider, AuthProviders, Config, ConfigFlags, ConfigLoader, LoadParams,
    MarkdownDiscovery, Missing, Source,
};
pub use event::{
    catalog_refresh_listener, new_event_id, versioned_type, AnyValue, CommitHook, Definition,
    DurableInfo, DurableManifest, DurableSpec, DurableStream, EmptyManifest, EventBus,
    InvalidDurableEventError, Listener, Payload, Projector, PublishOptions, ReplayOpts,
    SerializedEvent, Subscription, Validate, MODELS_DEV_REFRESHED,
};
pub use format::Formatter;
pub use jsonc::parse_jsonc;
pub use merge::{merge_config_concat_arrays, merge_deep};
pub use paths::GlobalPaths;
pub use storage::Storage;
pub use tool::{
    define, AgentInfo, AgentMode, Agents, Ask, AskRequest, Attachment, BoxFuture, ExecuteFn,
    ExecuteResult, Extra, FormatValidationError, InstanceContext, MetadataInput, MetadataSink,
    ToolCtxRef, ToolDef, ToolError,
};

/// One schema violation found while decoding a config value.
///
/// Mirrors the TS `Issue` shape from `packages/core/src/v1/config/error.ts`:
/// a message plus the property path to the offending value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaIssue {
    pub path: Vec<String>,
    pub message: String,
}

/// Error surface for `opencode-core`.
///
/// Variant names follow the TS `NamedError` taxonomy from
/// `packages/core/src/v1/config/error.ts`:
///
/// * `Jsonc` — `ConfigJsonError` (JSONC syntax errors).
/// * `ConfigInvalid` — `ConfigInvalidError` (schema / substitution failures).
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// `ConfigJsonError`: JSONC text failed to parse.
    ///
    /// `line`/`column` carry the 1-based position of the first syntax error
    /// (computed with the exact algorithm the TS reference uses in
    /// `packages/opencode/src/config/parse.ts`); `message` is the full
    /// formatted error block.
    #[error("ConfigJsonError: path: {path}, line: {line:?}, column: {column:?}\n{message}")]
    Jsonc {
        path: PathBuf,
        line: Option<usize>,
        column: Option<usize>,
        message: String,
    },

    /// `ConfigInvalidError`: a config value violated the schema, or a
    /// `{file:...}` substitution failed.
    ///
    /// `message` is set for single-message failures (bad file references);
    /// `issues` collects per-path schema violations.
    #[error("ConfigInvalidError: path: {path}, message: {message:?}, issues: {issues:?}")]
    ConfigInvalid {
        path: PathBuf,
        message: Option<String>,
        issues: Vec<SchemaIssue>,
    },

    /// models.dev catalog failure (fetch failure, cache lock failure, or the
    /// fetched payload failed to parse).
    #[error("CatalogError: {0}")]
    Catalog(String),

    /// Storage / database failure (SQLite error, migration abort, invalid
    /// database state). Mirrors the TS storage layer, which dies with the
    /// underlying message — e.g. "Database is not empty and has no session
    /// table" (`database/migration.ts:25`).
    #[error("StorageError: {0}")]
    Storage(String),

    /// `EventV2.InvalidDurableEvent` — the durable event log rejected an
    /// operation (unknown event type, aggregate mismatch, replay divergence,
    /// sequence gap, …). A defect in TS (`Effect.die`), surface error here.
    #[error("EventV2.InvalidDurableEvent: type: {event_type}, message: {message}")]
    InvalidDurableEvent { event_type: String, message: String },
}

impl From<rusqlite::Error> for CoreError {
    fn from(err: rusqlite::Error) -> Self {
        CoreError::Storage(err.to_string())
    }
}

impl CoreError {
    /// `ConfigInvalidError` with a single message, no issue list.
    pub fn invalid(path: impl Into<PathBuf>, message: impl Into<String>) -> CoreError {
        CoreError::ConfigInvalid {
            path: path.into(),
            message: Some(message.into()),
            issues: Vec::new(),
        }
    }
}
