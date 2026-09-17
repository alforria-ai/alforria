//! Stable identifiers and tag literals from `schema/ids.ts`.

use serde::{Deserialize, Serialize};

pub use opencode_schema::schema::JsonMap;

// TS re-exports `ProviderMetadata` from `@opencode-ai/schema/llm` here.
pub use opencode_schema::llm::ProviderMetadata;

/// Stable string identifier for a protocol implementation.
pub type ProtocolId = String;

/// Stable string identifier for the runnable route.
pub type RouteId = String;

/// Stable string identifier for a model (TS brands it `LLM.ModelID`).
pub type ModelId = String;

/// Stable string identifier for a provider (TS brands it `LLM.ProviderID`).
pub type ProviderId = String;

/// Stable string identifier for a content block within a response.
pub type ContentBlockId = String;

/// Stable string identifier for a tool call.
pub type ToolCallId = String;

/// `LLM.ReasoningEffort` literals.
pub const REASONING_EFFORTS: [&str; 7] =
    ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// `LLM.TextVerbosity` literals.
pub const TEXT_VERBOSITIES: [&str; 3] = ["low", "medium", "high"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Error,
    Unknown,
}
