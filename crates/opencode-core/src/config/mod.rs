//! Config loading: the `opencode.json` contract.
//!
//! * [`variable`] (M3.1) — `{env:VAR}` / `{file:path}` substitution.
//! * [`schema`] (M3.2) — the `Config` wire structs.
//! * [`precedence`] (M3.2) — source ordering + merge pipeline.
//! * [`agent`] / [`command`] (M3.3) — agent/command Markdown discovery.

pub mod agent;
pub mod command;
pub mod precedence;
pub mod schema;
pub mod variable;

pub use agent::MarkdownDiscovery;
pub use precedence::{
    AuthProvider, AuthProviders, ConfigDiscovery, ConfigFlags, ConfigLoader, LoadParams, NoAuth,
    NoopDiscovery,
};
pub use schema::Config;
pub use variable::{substitute, Missing, Source};
