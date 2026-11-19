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

/// `data` as the event log keeps it. A `message.updated` for a user message
/// loses its patches: the message row already has them, and the log would
/// otherwise grow by a turn's whole diff after every step. A replay sees the
/// files and their counts, which is all the wire schema requires.
pub fn log_form(r#type: &str, data: &Value) -> Value {
    let mut data = data.clone();
    if r#type == "message.updated" {
        strip_patches(&mut data);
    }
    data
}

/// Drop `patch` from a `message.updated` payload's `info.summary.diffs`.
/// Returns whether anything changed.
pub fn strip_patches(data: &mut Value) -> bool {
    let Some(diffs) = data
        .pointer_mut("/info/summary/diffs")
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let mut changed = false;
    for diff in diffs {
        if let Some(diff) = diff.as_object_mut() {
            changed |= diff.remove("patch").is_some();
        }
    }
    changed
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

/// [`list_events_after`], at most `limit` of them: a reader far behind takes
/// the log in pages instead of all at once.
pub fn list_events_page(
    conn: &Connection,
    aggregate_id: &str,
    after: i64,
    limit: usize,
) -> Result<Vec<EventRow>, CoreError> {
    let mut stmt = conn.prepare(
        "SELECT id, aggregate_id, seq, type, data FROM event
         WHERE aggregate_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
    )?;
    let mut rows = stmt.query(params![aggregate_id, after, limit as i64])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(event_from_row(row)?);
    }
    Ok(out)
}

/// The lowest sequence the log still holds for an aggregate, `None` when it
/// holds none.
pub fn earliest_sequence(conn: &Connection, aggregate_id: &str) -> Result<Option<i64>, CoreError> {
    Ok(conn.query_row(
        "SELECT MIN(seq) FROM event WHERE aggregate_id = ?1",
        params![aggregate_id],
        |row| row.get::<_, Option<i64>>(0),
    )?)
}

/// Which events the log may drop (an alforria extension; upstream keeps
/// them until their session is deleted). The messages, parts and sessions
/// the events built are untouched: only replay from before the cut is lost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventRetention {
    /// Drop events older than this many milliseconds.
    pub max_age_ms: Option<u64>,
    /// Keep this many of each aggregate's newest events whatever their age.
    /// On its own, keep only these.
    pub keep_last: Option<u64>,
}

impl EventRetention {
    /// Whether anything is ever dropped.
    pub fn is_set(&self) -> bool {
        self.max_age_ms.is_some() || self.keep_last.is_some()
    }
}

/// The id every event created before `time_ms` sorts below. Event ids are
/// `evt_` plus a ULID, whose leading characters are its creation time.
fn event_id_at(time_ms: u64) -> String {
    format!(
        "{}{}",
        crate::event::definition::ID_PREFIX,
        ulid::Ulid::from_parts(time_ms, 0)
    )
}

/// For each aggregate with something to drop, the sequence up to which its
/// events go. An event is kept when either rule keeps it.
pub fn prunable_events(
    conn: &Connection,
    retention: &EventRetention,
    now_ms: u64,
) -> Result<Vec<(String, i64)>, CoreError> {
    let keep_last = retention.keep_last.map(|keep| keep as i64);
    let mut out = Vec::new();
    match retention.max_age_ms {
        Some(max_age) => {
            // Old events are found through the primary key, so the scan
            // touches what can go and nothing else.
            let cutoff = event_id_at(now_ms.saturating_sub(max_age));
            let mut stmt = conn.prepare(
                "SELECT e.aggregate_id, MAX(e.seq), s.seq FROM event e
                 JOIN event_sequence s ON s.aggregate_id = e.aggregate_id
                 WHERE e.id < ?1 GROUP BY e.aggregate_id",
            )?;
            let mut rows = stmt.query(params![cutoff])?;
            while let Some(row) = rows.next()? {
                let aggregate: String = row.get(0)?;
                let aged: i64 = row.get(1)?;
                let latest: i64 = row.get(2)?;
                let cut = match keep_last {
                    Some(keep) => aged.min(latest - keep),
                    None => aged,
                };
                if cut >= 0 {
                    out.push((aggregate, cut));
                }
            }
        }
        None => {
            let Some(keep) = keep_last else {
                return Ok(out);
            };
            let mut stmt = conn.prepare(
                "SELECT s.aggregate_id, s.seq - ?1 FROM event_sequence s
                 WHERE s.seq - ?1 >= 0 AND EXISTS (
                   SELECT 1 FROM event e WHERE e.aggregate_id = s.aggregate_id AND e.seq <= s.seq - ?1
                 )",
            )?;
            let mut rows = stmt.query(params![keep])?;
            while let Some(row) = rows.next()? {
                out.push((row.get(0)?, row.get(1)?));
            }
        }
    }
    Ok(out)
}

/// Drop up to `limit` of an aggregate's events with `seq <= through`.
/// Returns how many went; call again until it returns less than `limit`.
/// The aggregate's sequence row stays, so new events keep their numbering.
pub fn prune_events(
    conn: &Connection,
    aggregate_id: &str,
    through: i64,
    limit: usize,
) -> Result<usize, CoreError> {
    Ok(conn.execute(
        "DELETE FROM event WHERE rowid IN (
           SELECT rowid FROM event WHERE aggregate_id = ?1 AND seq <= ?2 LIMIT ?3
         )",
        params![aggregate_id, through, limit as i64],
    )?)
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
