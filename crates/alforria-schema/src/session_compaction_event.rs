//! Wire DTOs for `schema-src/session-compaction-event.ts` — openapi
//! `SessionCompacted`.

use serde::{Deserialize, Serialize};

use crate::ids::SessionId;

/// `session.compacted` payload — openapi `SessionCompacted.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactedData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
}
