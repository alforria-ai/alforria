//! Wire DTOs for `schema-src/lsp-event.ts` — openapi `LspUpdated`.

use serde::{Deserialize, Serialize};

/// `lsp.updated` payload — openapi `LspUpdated.data` (empty object).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LspUpdatedData {}
