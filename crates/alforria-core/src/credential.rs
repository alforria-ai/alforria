//! Credential store — port of `packages/core/src/credential.ts` (the
//! `credential` table service).

use rusqlite::OptionalExtension;
use serde_json::Value;

use crate::storage::Storage;
use crate::{Clock, CoreError};

/// `Credential.ID` — `cred_`-prefixed ascending identifiers
/// (`Credential.ID.create()`).
pub fn credential_id() -> String {
    crate::session::ids::generate_id("cred_")
}

/// `Credential.Info` — one stored credential (`credentialID` on the wire).
#[derive(Debug, Clone, PartialEq)]
pub struct Credential {
    pub id: String,
    pub integration_id: String,
    pub label: String,
    pub value: Value,
}

/// The credential service (`credential.ts:100-130`): storage-backed, with
/// `create` replacing any sibling credential for the same integration in
/// one transaction.
pub struct CredentialStore {
    storage: std::sync::Arc<Storage>,
    clock: std::sync::Arc<dyn Clock>,
}

/// One row of the `credential` table.
struct Row {
    id: String,
    integration_id: Option<String>,
    label: String,
    value: Value,
}

fn row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let value_text: String = row.get(3)?;
    Ok(Row {
        id: row.get(0)?,
        integration_id: row.get(1)?,
        label: row.get(2)?,
        value: serde_json::from_str(&value_text).unwrap_or(Value::Null),
    })
}

fn to_credential(row: Row) -> Option<Credential> {
    row.integration_id.map(|integration_id| Credential {
        id: row.id,
        integration_id,
        label: row.label,
        value: row.value,
    })
}

fn select(
    conn: &rusqlite::Connection,
    where_clause: &str,
    param: Option<&str>,
) -> rusqlite::Result<Vec<Row>> {
    let sql = format!(
        "SELECT id, integration_id, label, value FROM credential {where_clause} ORDER BY time_created ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = match param {
        Some(param) => stmt.query_map(rusqlite::params![param], row_from)?,
        None => stmt.query_map(rusqlite::params![], row_from)?,
    };
    rows.collect::<Result<Vec<_>, _>>()
}

fn to_core(err: rusqlite::Error) -> CoreError {
    CoreError::Storage(err.to_string())
}

impl CredentialStore {
    pub fn new(storage: std::sync::Arc<Storage>, clock: std::sync::Arc<dyn Clock>) -> Self {
        CredentialStore { storage, clock }
    }

    /// `all` — every stored credential, oldest first.
    pub fn all(&self) -> Result<Vec<Credential>, CoreError> {
        let rows = self
            .storage
            .with_connection(|conn| select(conn, "", None))
            .map_err(to_core)?;
        Ok(rows.into_iter().filter_map(to_credential).collect())
    }

    /// `list` — credentials belonging to one integration, oldest first.
    pub fn list(&self, integration_id: &str) -> Result<Vec<Credential>, CoreError> {
        let rows = self
            .storage
            .with_connection(|conn| select(conn, "WHERE integration_id = ?1", Some(integration_id)))
            .map_err(to_core)?;
        Ok(rows.into_iter().filter_map(to_credential).collect())
    }

    /// `get` — one stored credential by ID.
    pub fn get(&self, id: &str) -> Result<Option<Credential>, CoreError> {
        self.storage.with_connection(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, integration_id, label, value FROM credential WHERE id = ?1")?;
            let row = stmt.query_row(rusqlite::params![id], row_from).optional()?;
            Ok(row.and_then(to_credential))
        })
    }

    /// `create` — replaces any credential for the integration and returns
    /// the new record (`credential.ts:113-131`).
    pub fn create(
        &self,
        integration_id: &str,
        value: Value,
        label: Option<&str>,
    ) -> Result<Credential, CoreError> {
        let credential = Credential {
            id: credential_id(),
            integration_id: integration_id.to_string(),
            label: label.unwrap_or("default").to_string(),
            value,
        };
        let now = self.clock.now_ms() as i64;
        self.storage.with_connection_mut(|conn| {
            let result: Result<(), CoreError> = (|| {
                let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                tx.execute(
                    "DELETE FROM credential WHERE integration_id = ?1",
                    rusqlite::params![credential.integration_id],
                )?;
                tx.execute(
                    "INSERT INTO credential (id, integration_id, label, value, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                    rusqlite::params![
                        credential.id,
                        credential.integration_id,
                        credential.label,
                        serde_json::to_string(&credential.value).map_err(|e| CoreError::Storage(e.to_string()))?,
                        now,
                    ],
                )?;
                tx.commit()?;
                Ok(())
            })();
            result
        })?;
        Ok(credential)
    }

    /// `update` — the label or secret value of a stored credential.
    pub fn update(
        &self,
        id: &str,
        label: Option<&str>,
        value: Option<&Value>,
    ) -> Result<(), CoreError> {
        if label.is_none() && value.is_none() {
            return Ok(());
        }
        self.storage.with_connection(|conn| {
            if let Some(label) = label {
                conn.execute(
                    "UPDATE credential SET label = ?1 WHERE id = ?2",
                    rusqlite::params![label, id],
                )?;
            }
            if let Some(value) = value {
                conn.execute(
                    "UPDATE credential SET value = ?1 WHERE id = ?2",
                    rusqlite::params![
                        serde_json::to_string(value)
                            .map_err(|e| CoreError::Storage(e.to_string()))?,
                        id
                    ],
                )?;
            }
            Ok(())
        })
    }

    /// `remove` — delete a stored credential.
    pub fn remove(&self, id: &str) -> Result<(), CoreError> {
        self.storage.with_connection(|conn| {
            conn.execute(
                "DELETE FROM credential WHERE id = ?1",
                rusqlite::params![id],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::SystemClock;

    fn store() -> CredentialStore {
        CredentialStore::new(
            std::sync::Arc::new(Storage::open_in_memory().unwrap()),
            std::sync::Arc::new(SystemClock),
        )
    }

    #[test]
    fn create_replaces_sibling_credentials() {
        let store = store();
        store
            .create(
                "github",
                serde_json::json!({"type": "key", "key": "a"}),
                Some("first"),
            )
            .unwrap();
        let credential = store
            .create(
                "github",
                serde_json::json!({"type": "key", "key": "b"}),
                None,
            )
            .unwrap();
        assert_eq!(credential.label, "default");
        assert_eq!(
            store.list("github").unwrap(),
            vec![credential],
            "sibling replaced in one transaction"
        );
    }

    #[test]
    fn list_orders_and_get_updates() {
        let store = store();
        store
            .create(
                "github",
                serde_json::json!({"type": "key", "key": "a"}),
                Some("one"),
            )
            .unwrap();
        store
            .create(
                "linear",
                serde_json::json!({"type": "key", "key": "b"}),
                Some("two"),
            )
            .unwrap();
        assert_eq!(store.list("github").unwrap().len(), 1);
        assert_eq!(store.all().unwrap().len(), 2);

        let credential = store.list("github").unwrap()[0].clone();
        store.update(&credential.id, Some("renamed"), None).unwrap();
        assert_eq!(
            store.get(&credential.id).unwrap().unwrap().label,
            "renamed".to_string()
        );
    }

    #[test]
    fn remove_deletes() {
        let store = store();
        let credential = store
            .create(
                "github",
                serde_json::json!({"type": "key", "key": "a"}),
                None,
            )
            .unwrap();
        store.remove(&credential.id).unwrap();
        assert!(store.get(&credential.id).unwrap().is_none());
        assert!(store.all().unwrap().is_empty());
    }
}
