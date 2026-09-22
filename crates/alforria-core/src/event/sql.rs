//! `event`/`event_sequence` persistence — port of `packages/core/src/event/sql.ts`.
//!
//! All functions take a `&Connection` (or run inside the caller's
//! transaction) so the bus can compose them atomically. `data` is stored as
//! compact JSON text, matching drizzle's `text({ mode: "json" })`.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::CoreError;

/// Row of the `event` table.
#[derive(Debug, Clone, PartialEq)]
pub struct EventRow {
    pub id: String,
    pub aggregate_id: String,
    pub seq: i64,
    /// Versioned event type (`"{type}.{version}"`).
    pub r#type: String,
    pub data: Value,
}

/// Row of the `event_sequence` table.
#[derive(Debug, Clone, PartialEq)]
pub struct EventSequenceRow {
    pub aggregate_id: String,
    pub seq: i64,
    pub owner_id: Option<String>,
}

fn json_to_string(value: &Value) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|err| CoreError::Storage(err.to_string()))
}

fn json_from_text(text: &str) -> Result<Value, CoreError> {
    serde_json::from_str(text).map_err(|err| CoreError::Storage(err.to_string()))
}

fn event_from_row(row: &rusqlite::Row<'_>) -> Result<EventRow, CoreError> {
    Ok(EventRow {
        id: row.get(0)?,
        aggregate_id: row.get(1)?,
        seq: row.get(2)?,
        r#type: row.get(3)?,
        data: json_from_text(&row.get::<_, String>(4)?)?,
    })
}

/// `latestSequence` (`event.ts:21-32`): `row?.seq ?? -1`.
pub fn latest_sequence(conn: &Connection, aggregate_id: &str) -> Result<i64, CoreError> {
    Ok(get_event_sequence(conn, aggregate_id)?
        .map(|row| row.seq)
        .unwrap_or(-1))
}

/// Read the `event_sequence` row for `aggregate_id`.
pub fn get_event_sequence(
    conn: &Connection,
    aggregate_id: &str,
) -> Result<Option<EventSequenceRow>, CoreError> {
    conn.query_row(
        "SELECT aggregate_id, seq, owner_id FROM event_sequence WHERE aggregate_id = ?1",
        params![aggregate_id],
        |row| {
            Ok(EventSequenceRow {
                aggregate_id: row.get(0)?,
                seq: row.get(1)?,
                owner_id: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(CoreError::from)
}

/// Insert-or-update the aggregate's sequence row (`event.ts:324-335`):
///
/// * fresh inserts carry `owner_id = input.owner_id` (`Option::None` for
///   fresh publishes — TS `input?.ownerID`);
/// * on conflict the sequence always advances, while `owner_id` is only
///   assigned when the caller sets one and the row does not already have an
///   owner (`set_owner`).
pub fn upsert_event_sequence(
    conn: &Connection,
    aggregate_id: &str,
    seq: i64,
    owner_id: Option<&str>,
    set_owner: bool,
) -> Result<(), CoreError> {
    conn.execute(
        "INSERT INTO event_sequence (aggregate_id, seq, owner_id) VALUES (?1, ?2, ?3)
         ON CONFLICT (aggregate_id) DO UPDATE SET
           seq = excluded.seq,
           owner_id = CASE WHEN ?4 THEN excluded.owner_id ELSE event_sequence.owner_id END",
        params![aggregate_id, seq, owner_id, set_owner],
    )?;
    Ok(())
}

/// Assign an owner to an existing sequence row (`claim`, `event.ts:525-532`).
pub fn update_owner(
    conn: &Connection,
    aggregate_id: &str,
    owner_id: &str,
) -> Result<(), CoreError> {
    conn.execute(
        "UPDATE event_sequence SET owner_id = ?1 WHERE aggregate_id = ?2",
        params![owner_id, aggregate_id],
    )?;
    Ok(())
}

/// Insert an `event` row.
pub fn insert_event(conn: &Connection, row: &EventRow) -> Result<(), CoreError> {
    conn.execute(
        "INSERT INTO event (id, aggregate_id, seq, type, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            row.id,
            row.aggregate_id,
            row.seq,
            row.r#type,
            json_to_string(&row.data)?,
        ],
    )?;
    Ok(())
}

/// Look up an event row by its global ID (uniqueness is across aggregates).
pub fn get_event_by_id(conn: &Connection, id: &str) -> Result<Option<EventRow>, CoreError> {
    query_optional(
        conn,
        "SELECT id, aggregate_id, seq, type, data FROM event WHERE id = ?1",
        params![id],
        event_from_row,
    )
}

/// Look up an event row by (aggregate_id, seq).
pub fn get_event_by_seq(
    conn: &Connection,
    aggregate_id: &str,
    seq: i64,
) -> Result<Option<EventRow>, CoreError> {
    query_optional(
        conn,
        "SELECT id, aggregate_id, seq, type, data FROM event WHERE aggregate_id = ?1 AND seq = ?2",
        params![aggregate_id, seq],
        event_from_row,
    )
}

/// `rusqlite::query_row` requires an infallible row mapper; use the
/// prepare/next pattern so JSON failures surface as `CoreError`.
fn query_optional<T>(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
    map: fn(&rusqlite::Row<'_>) -> Result<T, CoreError>,
) -> Result<Option<T>, CoreError> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params)?;
    match rows.next()? {
        Some(row) => Ok(Some(map(row)?)),
        None => Ok(None),
    }
}

/// All events of an aggregate with `seq > after`, ordered by seq
/// (`readAfter`, `event.ts:541-549`).
pub fn list_events_after(
    conn: &Connection,
    aggregate_id: &str,
    after: i64,
) -> Result<Vec<EventRow>, CoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, aggregate_id, seq, type, data FROM event
         WHERE aggregate_id = ?1 AND seq > ?2 ORDER BY seq ASC",
    )?;
    let mut rows = stmt.query(params![aggregate_id, after])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(event_from_row(row)?);
    }
    Ok(out)
}

/// `remove` (`event.ts:514-523`): delete the sequence row, then the events.
pub fn delete_event_sequence(conn: &Connection, aggregate_id: &str) -> Result<(), CoreError> {
    conn.execute(
        "DELETE FROM event_sequence WHERE aggregate_id = ?1",
        params![aggregate_id],
    )?;
    Ok(())
}

pub fn delete_events(conn: &Connection, aggregate_id: &str) -> Result<(), CoreError> {
    conn.execute(
        "DELETE FROM event WHERE aggregate_id = ?1",
        params![aggregate_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::storage::test_support::TempDir;
    use crate::storage::Storage;

    fn setup() -> (Storage, EventRow) {
        let dir = TempDir::new("event-sql");
        let storage = Storage::open(dir.path().join("db.sqlite")).unwrap();
        let row = EventRow {
            id: "evt_1".to_string(),
            aggregate_id: "ses_1".to_string(),
            seq: 0,
            r#type: "session.started.1".to_string(),
            data: serde_json::json!({"sessionID": "ses_1"}),
        };
        storage.with_connection(|conn| {
            upsert_event_sequence(conn, "ses_1", 0, None, false).unwrap();
            insert_event(conn, &row).unwrap();
        });
        (storage, row)
    }

    #[test]
    fn latest_sequence_is_neg_one_without_row() {
        let (storage, _) = setup();
        storage.with_connection(|conn| {
            assert_eq!(latest_sequence(conn, "missing").unwrap(), -1);
            assert_eq!(latest_sequence(conn, "ses_1").unwrap(), 0);
        });
    }

    #[test]
    fn upsert_assigns_owner_only_when_requested() {
        let (storage, _) = setup();
        storage.with_connection(|conn| {
            upsert_event_sequence(conn, "ses_1", 1, Some("own_1"), false).unwrap();
            assert_eq!(
                get_event_sequence(conn, "ses_1").unwrap().unwrap().owner_id,
                None,
                "set_owner=false keeps the existing owner"
            );
            upsert_event_sequence(conn, "ses_1", 2, Some("own_1"), true).unwrap();
            assert_eq!(
                get_event_sequence(conn, "ses_1").unwrap().unwrap().owner_id,
                Some("own_1".to_string())
            );
            assert_eq!(latest_sequence(conn, "ses_1").unwrap(), 2);
        });
    }

    #[test]
    fn list_events_after_orders_and_excludes() {
        let (storage, row) = setup();
        storage.with_connection(|conn| {
            insert_event(
                conn,
                &EventRow {
                    id: "evt_2".to_string(),
                    aggregate_id: "ses_1".to_string(),
                    seq: 1,
                    r#type: "session.started.1".to_string(),
                    data: serde_json::json!({"n": 1}),
                },
            )
            .unwrap();
            let all = list_events_after(conn, "ses_1", -1).unwrap();
            assert_eq!(all.len(), 2);
            assert_eq!(all[0].id, "evt_1");
            assert_eq!(all[1].id, "evt_2");
            assert_eq!(all[0].data, row.data, "data round-trips through JSON");
            let after = list_events_after(conn, "ses_1", 0).unwrap();
            assert_eq!(after.len(), 1);
            assert_eq!(after[0].id, "evt_2");
        });
    }

    #[test]
    fn delete_sequence_cascades_events() {
        let (storage, _) = setup();
        storage.with_connection(|conn| {
            delete_event_sequence(conn, "ses_1").unwrap();
            assert!(list_events_after(conn, "ses_1", -1).unwrap().is_empty());
        });
    }
}
