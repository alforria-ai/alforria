//! Schema-first LLM client (Rust port of `@opencode-ai/llm`).

// The wire-compat `LlmError` enum mirrors the TS error union field-for-field;
// boxing its large variants would ripple through every schema consumer, so
// the `Result<_, LlmError>` returns stay as they are.
#![allow(clippy::result_large_err)]

pub mod cache_policy;
pub mod protocols;
pub mod provider_error;
pub mod route;
pub mod schema;

pub use schema::*;
