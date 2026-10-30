//! `alforria db trim-diffs`: shrink a database written before turn diffs had
//! a budget. Message rows keep the same patches a new turn would keep, and
//! `message.updated` events lose theirs. Each batch is its own short
//! transaction, so a running server keeps working alongside it.

use std::time::Duration;

use rusqlite::{params, Connection, ErrorCode, TransactionBehavior};
use serde_json::Value;

use crate::event::sql::strip_patches;
use crate::session::snapshot::MAX_DIFF_PATCH_BYTES;
use crate::{CoreError, Storage};

/// A batch stops after this much row data, so no write lock is held for long.
const BATCH_BYTES: usize = 64 * 1024 * 1024;
const BATCH_ROWS: i64 = 2000;
/// Smaller events aren't worth rewriting.
const EVENT_MIN_BYTES: i64 = 4096;
const PAUSE: Duration = Duration::from_millis(20);
const BUSY_RETRIES: u32 = 12;

/// What a trim changed so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrimReport {
    pub messages: u64,
    pub events: u64,
    pub bytes_freed: u64,
}

/// Trim every oversized message row and `message.updated` event. Safe to run
/// again, and to stop part way. `progress` runs after each batch.
pub fn trim_stored_diffs(
    storage: &Storage,
    mut progress: impl FnMut(&TrimReport),
) -> Result<TrimReport, CoreError> {
    let mut report = TrimReport::default();
    rewrite_rows(
        storage,
        "message",
        "1 = 1",
        MAX_DIFF_PATCH_BYTES as i64,
        cap_message_patches,
        |report, freed| {
            report.messages += 1;
            report.bytes_freed += freed;
        },
        &mut report,
        &mut progress,
    )?;
    rewrite_rows(
        storage,
        "event",
        "type = 'message.updated.1'",
        EVENT_MIN_BYTES,
        strip_patches,
        |report, freed| {
            report.events += 1;
            report.bytes_freed += freed;
        },
        &mut report,
        &mut progress,
    )?;
    Ok(report)
}

/// Keep the smallest patches of a message row's `summary.diffs` that fit the
/// budget, as a new turn would. Returns whether anything changed.
fn cap_message_patches(data: &mut Value) -> bool {
    let Some(diffs) = data
        .pointer_mut("/summary/diffs")
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let size = |diff: &Value| diff.get("patch").and_then(Value::as_str).map(str::len);
    let mut order: Vec<usize> = (0..diffs.len())
        .filter(|index| size(&diffs[*index]).is_some())
        .collect();
    order.sort_by_key(|index| size(&diffs[*index]));
    let mut kept = 0usize;
    let mut changed = false;
    for index in order {
        let length = size(&diffs[index]).unwrap_or_default();
        if kept + length <= MAX_DIFF_PATCH_BYTES {
            kept += length;
        } else if let Some(diff) = diffs[index].as_object_mut() {
            diff.remove("patch");
            changed = true;
        }
    }
    changed
}

#[allow(clippy::too_many_arguments)]
fn rewrite_rows(
    storage: &Storage,
    table: &str,
    filter: &str,
    min_bytes: i64,
    rewrite: fn(&mut Value) -> bool,
    count: fn(&mut TrimReport, u64),
    report: &mut TrimReport,
    progress: &mut impl FnMut(&TrimReport),
) -> Result<(), CoreError> {
    let sql = format!(
        "SELECT rowid, data FROM {table} WHERE rowid > ?1 AND {filter} \
         AND length(CAST(data AS BLOB)) > ?2 ORDER BY rowid LIMIT {BATCH_ROWS}"
    );
    let update = format!("UPDATE {table} SET data = ?1 WHERE rowid = ?2");
    let mut after = 0i64;
    loop {
        let mut retries = 0;
        let batch = loop {
            let result = storage.with_connection_mut(|conn| {
                trim_batch(conn, &sql, &update, after, min_bytes, rewrite)
            });
            match result {
                Err(error)
                    if retries < BUSY_RETRIES
                        && error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy) =>
                {
                    retries += 1;
                    std::thread::sleep(PAUSE * 10);
                }
                other => break other?,
            }
        };
        let Some(batch) = batch else {
            return Ok(());
        };
        after = batch.last_rowid;
        for freed in batch.freed {
            count(report, freed);
        }
        progress(report);
        std::thread::sleep(PAUSE);
    }
}

struct Batch {
    last_rowid: i64,
    freed: Vec<u64>,
}

/// One transaction's worth of rows, or `None` when no row is left.
fn trim_batch(
    conn: &mut Connection,
    sql: &str,
    update: &str,
    after: i64,
    min_bytes: i64,
    rewrite: fn(&mut Value) -> bool,
) -> Result<Option<Batch>, rusqlite::Error> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut last_rowid = None;
    let mut changes: Vec<(i64, String, u64)> = Vec::new();
    {
        let mut stmt = tx.prepare(sql)?;
        let mut rows = stmt.query(params![after, min_bytes])?;
        let mut bytes = 0usize;
        while let Some(row) = rows.next()? {
            let rowid: i64 = row.get(0)?;
            let text: String = row.get(1)?;
            last_rowid = Some(rowid);
            bytes += text.len();
            // A row that isn't JSON isn't ours to change.
            if let Ok(mut data) = serde_json::from_str::<Value>(&text) {
                if rewrite(&mut data) {
                    let next = serde_json::to_string(&data)
                        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?;
                    let freed = text.len().saturating_sub(next.len()) as u64;
                    changes.push((rowid, next, freed));
                }
            }
            if bytes >= BATCH_BYTES {
                break;
            }
        }
    }
    let Some(last_rowid) = last_rowid else {
        return Ok(None);
    };
    let mut freed = Vec::with_capacity(changes.len());
    for (rowid, text, row_freed) in changes {
        tx.execute(update, params![text, rowid])?;
        freed.push(row_freed);
    }
    tx.commit()?;
    Ok(Some(Batch { last_rowid, freed }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seed(storage: &Storage, table: &str, id: &str, kind: &str, data: &Value) {
        storage.with_connection(|conn| {
            conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
            let text = data.to_string();
            if table == "message" {
                conn.execute(
                    "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?1, 's', 1, 1, ?2)",
                    params![id, text],
                )
                .unwrap();
            } else {
                conn.execute(
                    "INSERT INTO event (id, aggregate_id, seq, type, data) VALUES (?1, 's', ?2, ?3, ?4)",
                    params![id, id.len() as i64, kind, text],
                )
                .unwrap();
            }
        });
    }

    fn data_of(storage: &Storage, table: &str, id: &str) -> Value {
        storage.with_connection(|conn| {
            let text: String = conn
                .query_row(
                    &format!("SELECT data FROM {table} WHERE id = ?1"),
                    [id],
                    |row| row.get(0),
                )
                .unwrap();
            serde_json::from_str(&text).unwrap()
        })
    }

    fn diff(file: &str, bytes: usize) -> Value {
        json!({ "file": file, "patch": "x".repeat(bytes), "additions": 1, "deletions": 2, "status": "modified" })
    }

    #[test]
    fn oversized_rows_keep_their_small_patches_and_every_count() {
        let storage = Storage::open_in_memory().unwrap();
        let big = MAX_DIFF_PATCH_BYTES - 200;
        seed(
            &storage,
            "message",
            "msg_big",
            "",
            &json!({ "role": "user", "summary": { "title": "t", "diffs": [
                diff("huge.js", big), diff("real.rs", 100), diff("huge2.js", big)
            ] } }),
        );
        let report = trim_stored_diffs(&storage, |_| {}).unwrap();
        assert_eq!(report.messages, 1);
        assert!(report.bytes_freed >= big as u64);

        let data = data_of(&storage, "message", "msg_big");
        let diffs = data["summary"]["diffs"].as_array().unwrap();
        assert_eq!(data["summary"]["title"], "t");
        assert_eq!(diffs.len(), 3);
        assert_eq!(diffs[1]["patch"].as_str().unwrap().len(), 100);
        let kept = diffs
            .iter()
            .filter(|diff| diff.get("patch").is_some())
            .count();
        assert_eq!(kept, 2, "one big patch still fits next to the small one");
        assert!(diffs
            .iter()
            .all(|diff| diff["additions"] == 1 && diff["deletions"] == 2));
    }

    #[test]
    fn message_updated_events_lose_their_patches_and_nothing_else_changes() {
        let storage = Storage::open_in_memory().unwrap();
        let event = json!({ "sessionID": "s", "info": { "role": "user", "summary": { "diffs": [diff("a.js", 8192)] } } });
        seed(&storage, "event", "e1", "message.updated.1", &event);
        seed(&storage, "event", "e22", "message.part.updated.1", &event);
        let small = json!({ "info": { "summary": { "diffs": [diff("b.js", 10)] } } });
        seed(&storage, "event", "e333", "message.updated.1", &small);

        let report = trim_stored_diffs(&storage, |_| {}).unwrap();
        assert_eq!(report.events, 1);

        let trimmed = data_of(&storage, "event", "e1");
        let entry = &trimmed["info"]["summary"]["diffs"][0];
        assert!(entry.get("patch").is_none());
        assert_eq!(entry["file"], "a.js");
        assert_eq!(entry["additions"], 1);
        assert_eq!(data_of(&storage, "event", "e22"), event);
        assert_eq!(data_of(&storage, "event", "e333"), small);
    }

    #[test]
    fn a_second_trim_changes_nothing() {
        let storage = Storage::open_in_memory().unwrap();
        seed(
            &storage,
            "message",
            "msg_big",
            "",
            &json!({ "summary": { "diffs": [diff("a", MAX_DIFF_PATCH_BYTES), diff("b", MAX_DIFF_PATCH_BYTES)] } }),
        );
        seed(
            &storage,
            "event",
            "e1",
            "message.updated.1",
            &json!({ "info": { "summary": { "diffs": [diff("a", 8192)] } } }),
        );
        assert_ne!(
            trim_stored_diffs(&storage, |_| {}).unwrap(),
            TrimReport::default()
        );
        assert_eq!(
            trim_stored_diffs(&storage, |_| {}).unwrap(),
            TrimReport::default()
        );
    }

    #[test]
    fn rows_that_are_not_message_diffs_are_left_alone() {
        let storage = Storage::open_in_memory().unwrap();
        let assistant = json!({ "role": "assistant", "summary": true, "text": "y".repeat(MAX_DIFF_PATCH_BYTES + 1) });
        seed(&storage, "message", "msg_a", "", &assistant);
        assert_eq!(
            trim_stored_diffs(&storage, |_| {}).unwrap(),
            TrimReport::default()
        );
        assert_eq!(data_of(&storage, "message", "msg_a"), assistant);
    }

    #[test]
    fn large_batches_split_without_skipping_a_row() {
        let storage = Storage::open_in_memory().unwrap();
        let mut ids = Vec::new();
        for index in 0..4 {
            let id = format!("e{}", "0".repeat(index + 1));
            seed(
                &storage,
                "event",
                &id,
                "message.updated.1",
                &json!({ "info": { "summary": { "diffs": [diff("a", BATCH_BYTES / 2 + 1)] } } }),
            );
            ids.push(id);
        }
        let mut batches = 0;
        let report = trim_stored_diffs(&storage, |_| batches += 1).unwrap();
        assert_eq!(report.events, 4);
        assert!(batches >= 2, "{batches} batches");
        for id in ids {
            let data = data_of(&storage, "event", &id);
            assert!(data["info"]["summary"]["diffs"][0].get("patch").is_none());
        }
    }
}
