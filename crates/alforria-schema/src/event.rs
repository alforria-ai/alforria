//! Event envelope structs — openapi `Event`, `V2Event`, `GlobalEvent`.
//!
//! Three envelope shapes exist (spec M1 §2.6):
//!
//! 1. **V2 envelope** (`V2Envelope`): `{ id, metadata?, type, durable?,
//!    location?, data }` — the payload union (tag `type`, content `data`) is
//!    flattened in next to the envelope's own keys.
//! 2. **Legacy envelope** (`LegacyEnvelope`): `{ id, type, properties }` —
//!    the payload union (tag `type`, content `properties`) is flattened in.
//! 3. **Global envelope** (`GlobalEnvelope`): `{ directory, project?,
//!    workspace?, payload }` for the SSE `/global/event` stream, where
//!    `payload` is a legacy envelope.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::ids::EventId;
use crate::location::LocationRef;
use crate::schema::JsonMap;

/// Durable ref on an event envelope (all fields required when present at all).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableRef {
    #[serde(rename = "aggregateID")]
    pub aggregate_id: String,
    pub seq: i64,
    pub version: i64,
}

/// V2 envelope: `{ id, metadata?, type, durable?, location?, data }`.
///
/// The payload union (e.g. [`crate::event_manifest::V2Event`]) is flattened in
/// and supplies the `type` + `data` keys via adjacent tagging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(deserialize = "E: DeserializeOwned"))]
pub struct V2Envelope<E>
where
    E: Serialize,
    E: DeserializeOwned,
{
    pub id: EventId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable: Option<DurableRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<LocationRef>,
    #[serde(flatten)]
    pub event: E,
}

/// Legacy envelope: `{ id, type, properties }`.
///
/// The payload union (e.g. [`crate::event_manifest::Event`]) is flattened in
/// and supplies the `type` + `properties` keys via adjacent tagging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(deserialize = "E: DeserializeOwned"))]
pub struct LegacyEnvelope<E>
where
    E: Serialize,
    E: DeserializeOwned,
{
    pub id: EventId,
    #[serde(flatten)]
    pub event: E,
}

/// Global envelope for the SSE `/global/event` stream:
/// `{ directory, project?, workspace?, payload }` where `payload` is a legacy
/// envelope (openapi `GlobalEvent`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", bound(deserialize = "E: DeserializeOwned"))]
pub struct GlobalEnvelope<E>
where
    E: Serialize,
    E: DeserializeOwned,
{
    pub directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub payload: LegacyEnvelope<E>,
}
