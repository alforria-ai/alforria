//! Shared wire primitives (from `schema-src/schema.ts`).

/// Wire representation of Effect's `DateTimeUtcFromMillis`: epoch milliseconds.
pub type EpochMillis = i64;

/// `Record<String, Unknown>`
pub type JsonMap = serde_json::Map<String, serde_json::Value>;
