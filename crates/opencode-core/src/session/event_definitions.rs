//! `SessionV1.Event.Definitions` + the session `Durable` manifest — port of
//! `packages/schema/src/v1/session.ts:567-677` and
//! `packages/schema/src/durable-event-manifest.ts:12-15`.
//!
//! This is the prerequisite catalog M5.1 needs: 10 event definitions, of
//! which 7 are durable (`durable: { aggregate: "sessionID", version: 1 }`)
//! and 3 are ephemeral (`PartDelta`, `Diff`, `Error` — they lack the
//! `...options` spread in the TS).

use serde_json::Value;

use crate::event::definition::{Definition, DurableManifest, DurableSpec};
use opencode_schema::session_v1::{
    MessagePartRemovedData, MessagePartUpdatedData, MessageRemovedData, MessageUpdatedData,
    SessionCreatedData, SessionDeletedData, SessionUpdatedData,
};

/// `options` (session.ts:494-499): the shared durable spec.
const DURABLE: Option<DurableSpec> = Some(DurableSpec {
    aggregate: "sessionID",
    version: 1,
});

/// `events.Created` (session.ts:570-578).
pub const SESSION_CREATED: Definition = Definition {
    r#type: "session.created",
    durable: DURABLE,
};
/// `events.Updated` (session.ts:579-587).
pub const SESSION_UPDATED: Definition = Definition {
    r#type: "session.updated",
    durable: DURABLE,
};
/// `events.Deleted` (session.ts:588-596).
pub const SESSION_DELETED: Definition = Definition {
    r#type: "session.deleted",
    durable: DURABLE,
};
/// `events.MessageUpdated` (session.ts:597-604).
pub const MESSAGE_UPDATED: Definition = Definition {
    r#type: "message.updated",
    durable: DURABLE,
};
/// `events.MessageRemoved` (session.ts:605-611).
pub const MESSAGE_REMOVED: Definition = Definition {
    r#type: "message.removed",
    durable: DURABLE,
};
/// `events.PartUpdated` (session.ts:612-621).
pub const MESSAGE_PART_UPDATED: Definition = Definition {
    r#type: "message.part.updated",
    durable: DURABLE,
};
/// `events.PartRemoved` (session.ts:622-631).
pub const MESSAGE_PART_REMOVED: Definition = Definition {
    r#type: "message.part.removed",
    durable: DURABLE,
};

/// `PartDelta` (session.ts:633-642) — no `...options`: ephemeral.
pub const MESSAGE_PART_DELTA: Definition = Definition::ephemeral("message.part.delta");
/// `Diff` (session.ts:644-650) — no `...options`: ephemeral.
pub const SESSION_DIFF: Definition = Definition::ephemeral("session.diff");
/// `Error` (session.ts:652-658) — no `...options`: ephemeral.
pub const SESSION_ERROR: Definition = Definition::ephemeral("session.error");

/// `SessionCompactionEvent.Compacted`
/// (`schema/src/session-compaction-event.ts:8-12`) — ephemeral.
pub const SESSION_COMPACTED: Definition = Definition::ephemeral("session.compacted");

/// `SessionV1.Event.Definitions` (session.ts:654-677) — all ten, in order.
pub const DEFINITIONS: [Definition; 10] = [
    SESSION_CREATED,
    SESSION_UPDATED,
    SESSION_DELETED,
    MESSAGE_UPDATED,
    MESSAGE_REMOVED,
    MESSAGE_PART_UPDATED,
    MESSAGE_PART_REMOVED,
    MESSAGE_PART_DELTA,
    SESSION_DIFF,
    SESSION_ERROR,
];

/// The session `Durable` manifest — durable definitions only, keyed by
/// *versioned* type (`schema/src/durable-event-manifest.ts:12-15`:
/// `Event.durable(Definitions.filter(durable))`).
pub struct SessionManifest;

impl SessionManifest {
    pub fn new() -> Self {
        SessionManifest
    }
}

impl Default for SessionManifest {
    fn default() -> Self {
        Self::new()
    }
}

impl DurableManifest for SessionManifest {
    fn get(&self, versioned: &str) -> Option<&Definition> {
        DEFINITIONS
            .iter()
            .find(|definition| match definition.durable {
                Some(spec) => {
                    crate::event::definition::versioned_type(definition.r#type, spec.version)
                        == versioned
                }
                None => false,
            })
    }

    fn validate(&self, definition: &Definition, data: &Value) -> Result<(), crate::CoreError> {
        let ok = match definition.r#type {
            "session.created" => serde_json::from_value::<SessionCreatedData>(data.clone()).is_ok(),
            "session.updated" => serde_json::from_value::<SessionUpdatedData>(data.clone()).is_ok(),
            "session.deleted" => serde_json::from_value::<SessionDeletedData>(data.clone()).is_ok(),
            "message.updated" => serde_json::from_value::<MessageUpdatedData>(data.clone()).is_ok(),
            "message.removed" => serde_json::from_value::<MessageRemovedData>(data.clone()).is_ok(),
            "message.part.updated" => {
                serde_json::from_value::<MessagePartUpdatedData>(data.clone()).is_ok()
            }
            "message.part.removed" => {
                serde_json::from_value::<MessagePartRemovedData>(data.clone()).is_ok()
            }
            _ => true,
        };
        if ok {
            Ok(())
        } else {
            Err(crate::CoreError::InvalidDurableEvent {
                event_type: definition.r#type.to_string(),
                message: "data does not match the definition schema".to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::definition::versioned_type;

    #[test]
    fn ten_definitions_in_ts_order() {
        let types: Vec<&str> = DEFINITIONS
            .iter()
            .map(|definition| definition.r#type)
            .collect();
        assert_eq!(
            types,
            vec![
                "session.created",
                "session.updated",
                "session.deleted",
                "message.updated",
                "message.removed",
                "message.part.updated",
                "message.part.removed",
                "message.part.delta",
                "session.diff",
                "session.error",
            ]
        );
    }

    #[test]
    fn durability_matches_ts_options_spread() {
        for definition in DEFINITIONS.iter() {
            match definition.r#type {
                "message.part.delta" | "session.diff" | "session.error" => {
                    assert!(
                        definition.durable.is_none(),
                        "{} is ephemeral in TS (no ...options)",
                        definition.r#type
                    );
                }
                _ => {
                    let Some(spec) = definition.durable else {
                        panic!("{} must be durable", definition.r#type);
                    };
                    assert_eq!(spec.aggregate, "sessionID");
                    assert_eq!(spec.version, 1);
                }
            }
        }
    }

    #[test]
    fn manifest_looks_up_versioned_types() {
        let manifest = SessionManifest::new();
        assert!(manifest
            .get(&versioned_type("session.created", 1))
            .is_some());
        assert!(manifest
            .get(&versioned_type("message.part.updated", 1))
            .is_some());
        // Ephemeral events are not in the durable manifest.
        assert!(manifest.get("message.part.delta").is_none());
        assert!(manifest.get("session.created").is_none());
    }
}
