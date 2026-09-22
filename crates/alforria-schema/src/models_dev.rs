//! Wire DTOs for `schema-src/models-dev.ts` — openapi `Models-devRefreshed`.

use serde::{Deserialize, Serialize};

/// `models-dev.refreshed` payload — openapi `Models-devRefreshed.data` (empty object).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelsDevRefreshedData {}
