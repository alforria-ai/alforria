//! Wire DTOs for `schema-src/catalog.ts` — openapi `CatalogUpdated`.

use serde::{Deserialize, Serialize};

/// `catalog.updated` payload — openapi `CatalogUpdated.data` (empty object).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogUpdatedData {}
