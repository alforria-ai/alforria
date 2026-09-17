//! `Definition`/`Payload` types, the `DurableManifest` trait and `evt_` ID
//! creation — port of `@opencode-ai/schema/event` (the shapes the TS
//! `packages/core/src/event.ts` bus is generic over).
//!
//! M3 ships the generic machinery only: event *data* payloads are
//! `serde_json::Value` at this layer (opencode-schema owns the concrete
//! payload DTOs); concrete durable definitions land in M4 with sessions.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ulid::Ulid;

use opencode_schema::event::DurableRef;
use opencode_schema::location::LocationRef;
use opencode_schema::schema::JsonMap;

use crate::CoreError;

/// `Event.ID` prefix (`schema/src/event.ts`: `Schema.isStartsWith("evt_")`).
pub const ID_PREFIX: &str = "evt_";

/// Durable spec of a definition: which data field holds the aggregate ID and
/// the definition's durable version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableSpec {
    /// Name of the `data` field holding the aggregate ID.
    pub aggregate: &'static str,
    pub version: u32,
}

/// An event definition (`Schema.Top & { type, durable?, data }`).
///
/// The definition's *data schema* is `serde_json::Value` at this layer;
/// validation is plugged in via the [`DurableManifest`] in M4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Definition {
    /// Unversioned wire type, e.g. `session.next.text.started`.
    pub r#type: &'static str,
    pub durable: Option<DurableSpec>,
}

impl Definition {
    /// A non-durable definition.
    pub const fn ephemeral(r#type: &'static str) -> Definition {
        Definition {
            r#type,
            durable: None,
        }
    }
}

/// The `models-dev.refreshed` event — non-durable (port of the TS catalog
/// refresh notification; the M3.4 `CatalogService` bridges to it, see
/// [`crate::event::bus::catalog_refresh_listener`]).
pub const MODELS_DEV_REFRESHED: Definition = Definition::ephemeral("models-dev.refreshed");

/// `versionedType` (`schema/src/event.ts`):
/// `"${type}.${version}"` — the string stored in the `event.type` column and
/// the key of the durable-event manifest.
pub fn versioned_type(r#type: &str, version: u32) -> String {
    format!("{type}.{version}", type = r#type, version = version)
}

/// `Event.ID.create()` — `evt_` + ULID.
///
/// The TS reference composes `evt_ + ascending()` (48-bit timestamp hex +
/// random base62); the Rust port uses a ULID for the same
/// lexicographically-sortable, time-prefixed purpose. IDs are opaque.
pub fn new_event_id() -> String {
    format!("{ID_PREFIX}{}", Ulid::new())
}

/// `DurableRef` alias for readability in event signatures.
pub use opencode_schema::event::DurableRef as DurableInfo;

/// An event payload as it flows through the bus (`Payload` in
/// `schema/src/event.ts`). Field names match the V2 wire envelope
/// (`{ id, metadata?, type, durable?, location?, data }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Payload {
    /// `evt_…` event ID.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    #[serde(rename = "type")]
    pub r#type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable: Option<DurableRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationRef>,
    pub data: Value,
}

impl Payload {
    /// The durable aggregate ID / sequence, if this is a durable event.
    pub fn durable(&self) -> Option<&DurableRef> {
        self.durable.as_ref()
    }
}

/// A serialized event as persisted in the `event` table — the unit of
/// `replay`/`replayAll` (`event.ts:34-40`). `type` is the *versioned* type
/// (see [`versioned_type`]).
#[derive(Debug, Clone, PartialEq)]
pub struct SerializedEvent {
    pub id: String,
    pub r#type: String,
    pub seq: i64,
    pub aggregate_id: String,
    pub data: Value,
}

/// `EventV2.InvalidDurableEvent` — a defect raised when the durable log
/// rejects an operation (unknown event type, aggregate mismatch, replay
/// divergence, sequence gap…).
#[derive(Debug, Clone, PartialEq)]
pub struct InvalidDurableEventError {
    pub event_type: String,
    pub message: String,
}

impl InvalidDurableEventError {
    pub fn new(event_type: impl Into<String>, message: impl Into<String>) -> Self {
        InvalidDurableEventError {
            event_type: event_type.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for InvalidDurableEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.event_type, self.message)
    }
}

impl std::error::Error for InvalidDurableEventError {}

impl From<InvalidDurableEventError> for CoreError {
    fn from(err: InvalidDurableEventError) -> CoreError {
        CoreError::InvalidDurableEvent {
            event_type: err.event_type,
            message: err.message,
        }
    }
}

/// Data-payload validation seam for concrete definitions (M4).
///
/// M3's generic machinery treats payloads as `serde_json::Value` and the
/// default validation is a no-op; M4's manifest overrides this to decode
/// against the per-definition schema DTOs (mirroring TS
/// `Schema.decodeUnknownSync(definition.data)`).
pub trait Validate: Send + Sync {
    fn validate(&self, data: &Value) -> Result<(), CoreError>;
}

/// The no-op validator: any `Value` is accepted (M3).
#[derive(Debug, Clone, Copy, Default)]
pub struct AnyValue;

impl Validate for AnyValue {
    fn validate(&self, _data: &Value) -> Result<(), CoreError> {
        Ok(())
    }
}

/// The registry of known durable definitions — port of the TS
/// `Durable` map from `schema/src/durable-event-manifest.ts`, keyed by
/// *versioned* type (`Event.durable(...)` keys on
/// [`versioned_type`] output).
pub trait DurableManifest: Send + Sync {
    /// Look up a definition by its *versioned* type.
    fn get(&self, versioned_type: &str) -> Option<&Definition>;

    /// Decode/validate `data` against the definition's schema. Errors abort
    /// the operation (TS `Schema.decodeUnknownSync`).
    fn validate(&self, definition: &Definition, data: &Value) -> Result<(), CoreError>;
}

/// An empty manifest: no durable definitions are known. Mirrors the empty
/// `Durable` map M3 runs with — concrete definitions land in M4.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyManifest;

impl DurableManifest for EmptyManifest {
    fn get(&self, _versioned_type: &str) -> Option<&Definition> {
        None
    }

    fn validate(&self, _definition: &Definition, _data: &Value) -> Result<(), CoreError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versioned_type_matches_ts_format() {
        // schema/src/event.ts: `${type}.${version}`
        assert_eq!(
            versioned_type("session.next.text.started", 1),
            "session.next.text.started.1"
        );
    }

    #[test]
    fn event_ids_are_evt_prefixed_and_unique() {
        let a = new_event_id();
        let b = new_event_id();
        assert!(a.starts_with("evt_"));
        assert_ne!(a, b);
        assert_eq!(a.len(), ID_PREFIX.len() + 26); // ULID length
    }

    #[test]
    fn invalid_durable_event_error_converts() {
        let err = CoreError::from(InvalidDurableEventError::new("t.x", "boom"));
        match err {
            CoreError::InvalidDurableEvent {
                event_type,
                message,
                ..
            } => {
                assert_eq!(event_type, "t.x");
                assert_eq!(message, "boom");
            }
            _ => panic!("expected InvalidDurableEvent"),
        }
    }

    #[test]
    fn payload_serializes_like_the_v2_envelope() {
        let payload = Payload {
            id: "evt_01J".to_string(),
            metadata: None,
            r#type: "models-dev.refreshed".to_string(),
            durable: Some(DurableRef {
                aggregate_id: "ses_1".to_string(),
                seq: 3,
                version: 1,
            }),
            location: Some(LocationRef {
                directory: "/repo".to_string(),
                workspace_id: None,
            }),
            data: serde_json::json!({"sessionID": "ses_1"}),
        };
        let wire = serde_json::to_value(&payload).unwrap();
        assert_eq!(wire["id"], "evt_01J");
        assert_eq!(wire["type"], "models-dev.refreshed");
        assert_eq!(wire["durable"]["aggregateID"], "ses_1");
        assert_eq!(wire["location"]["directory"], "/repo");
        assert_eq!(wire["data"]["sessionID"], "ses_1");
        assert!(wire.get("metadata").is_none());
        let back: Payload = serde_json::from_value(wire).unwrap();
        assert_eq!(back, payload);
    }
}
