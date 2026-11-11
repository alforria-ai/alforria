//! Keeping a long-lived database in shape (an alforria extension): the
//! event log's retention, and the write-ahead log's size.
//!
//! Upstream keeps every durable event until its session is deleted, which
//! suits sessions that last an afternoon. A session that lives for months
//! never is, so its log only grows. Retention is off unless configured.

use std::sync::{Arc, Weak};
use std::time::Duration;

use crate::config::schema::Config;
use crate::event::sql::{self, EventRetention};
use crate::storage::Storage;
use crate::CoreError;

/// How often the write-ahead log is folded back into the database file.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How often retention runs, after the pass at startup.
pub const PRUNE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Events dropped per statement, so a large prune never holds the
/// connection long enough to stall a turn.
const PRUNE_BATCH: usize = 2_000;

/// `storage.retention.events` as the pruner takes it.
pub fn event_retention(config: &Config) -> EventRetention {
    let events = config
        .storage
        .as_ref()
        .and_then(|storage| storage.retention.as_ref())
        .and_then(|retention| retention.events.as_ref());
    EventRetention {
        max_age_ms: events
            .and_then(|events| events.max_age)
            .map(|age| age.as_millis()),
        keep_last: events
            .and_then(|events| events.keep_last)
            .map(|keep| keep.get()),
    }
}

/// What a retention pass dropped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub events: u64,
    /// Sessions (aggregates) that lost events.
    pub aggregates: u64,
}

/// Drop the events `retention` no longer keeps. A no-op when it keeps
/// everything.
pub fn prune_events(
    storage: &Storage,
    retention: &EventRetention,
    now_ms: u64,
) -> Result<PruneReport, CoreError> {
    let mut report = PruneReport::default();
    if !retention.is_set() {
        return Ok(report);
    }
    let cuts = storage.with_connection(|conn| sql::prunable_events(conn, retention, now_ms))?;
    for (aggregate_id, through) in cuts {
        let mut dropped = 0u64;
        loop {
            // The connection is released between batches.
            let batch = storage.with_connection(|conn| {
                sql::prune_events(conn, &aggregate_id, through, PRUNE_BATCH)
            })?;
            dropped += batch as u64;
            if batch < PRUNE_BATCH {
                break;
            }
        }
        if dropped > 0 {
            report.events += dropped;
            report.aggregates += 1;
        }
    }
    Ok(report)
}

/// Fold the write-ahead log into the database file and truncate it. With
/// readers in the way SQLite does what it can and the next pass finishes.
pub fn checkpoint(storage: &Storage) -> Result<(), CoreError> {
    storage.with_connection(|conn| {
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    })
}

/// Rebuild the database file without the space deleted rows left behind.
/// Needs free disk the size of the database, and no other process using it.
pub fn vacuum(storage: &Storage) -> Result<(), CoreError> {
    storage.with_connection(|conn| {
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        Ok(())
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// One retention pass and a checkpoint, logging rather than failing: the
/// database works without either.
pub fn run_once(storage: &Storage, retention: &EventRetention) {
    match prune_events(storage, retention, now_ms()) {
        Ok(report) if report.events > 0 => tracing::info!(
            events = report.events,
            sessions = report.aggregates,
            "pruned the event log"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!("pruning the event log failed: {error}"),
    }
    if let Err(error) = checkpoint(storage) {
        tracing::warn!("checkpointing the write-ahead log failed: {error}");
    }
}

/// Run retention now and daily, and checkpoint the write-ahead log every
/// few minutes, for as long as `storage` is in use. Must be called inside a
/// tokio runtime.
pub fn spawn(storage: &Arc<Storage>, retention: EventRetention) -> tokio::task::JoinHandle<()> {
    let storage: Weak<Storage> = Arc::downgrade(storage);
    tokio::spawn(async move {
        let mut since_prune = PRUNE_INTERVAL;
        loop {
            let Some(storage) = storage.upgrade() else {
                return;
            };
            let prune = since_prune >= PRUNE_INTERVAL;
            if prune {
                since_prune = Duration::ZERO;
            }
            let pass = tokio::task::spawn_blocking(move || {
                if prune {
                    run_once(&storage, &retention);
                } else if let Err(error) = checkpoint(&storage) {
                    tracing::warn!("checkpointing the write-ahead log failed: {error}");
                }
            });
            let _ = pass.await;
            tokio::time::sleep(CHECKPOINT_INTERVAL).await;
            since_prune += CHECKPOINT_INTERVAL;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::sql::{insert_event, upsert_event_sequence, EventRow};
    use crate::storage::test_support::TempDir;

    const DAY: u64 = 24 * 60 * 60 * 1000;
    const NOW: u64 = 1_800_000_000_000;

    fn storage() -> (Storage, TempDir) {
        let dir = TempDir::new("maintenance");
        (Storage::open(dir.path().join("db.sqlite")).unwrap(), dir)
    }

    /// `count` events for `aggregate`, one a day, the newest written today.
    fn write_daily(storage: &Storage, aggregate: &str, count: i64) {
        storage.with_connection(|conn| {
            for seq in 0..count {
                let age_days = (count - 1 - seq) as u64;
                upsert_event_sequence(conn, aggregate, seq, None, false).unwrap();
                insert_event(
                    conn,
                    &EventRow {
                        id: format!(
                            "evt_{}",
                            ulid::Ulid::from_parts(NOW - age_days * DAY, seq as u128)
                        ),
                        aggregate_id: aggregate.to_string(),
                        seq,
                        r#type: "message.updated.1".to_string(),
                        data: serde_json::json!({ "seq": seq }),
                    },
                )
                .unwrap();
            }
        });
    }

    fn seqs(storage: &Storage, aggregate: &str) -> Vec<i64> {
        storage.with_connection(|conn| {
            sql::list_events_after(conn, aggregate, -1)
                .unwrap()
                .iter()
                .map(|row| row.seq)
                .collect()
        })
    }

    fn retention(max_age_days: Option<u64>, keep_last: Option<u64>) -> EventRetention {
        EventRetention {
            max_age_ms: max_age_days.map(|days| days * DAY + DAY / 2),
            keep_last,
        }
    }

    #[test]
    fn nothing_is_pruned_unless_configured() {
        let (storage, _dir) = storage();
        write_daily(&storage, "ses_a", 10);
        let report = prune_events(&storage, &EventRetention::default(), NOW).unwrap();
        assert_eq!(report, PruneReport::default());
        assert_eq!(seqs(&storage, "ses_a").len(), 10);
        // The config's absence means the same.
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(!event_retention(&config).is_set());
    }

    #[test]
    fn max_age_drops_what_is_older() {
        let (storage, _dir) = storage();
        write_daily(&storage, "ses_a", 10);
        write_daily(&storage, "ses_b", 2);
        let report = prune_events(&storage, &retention(Some(3), None), NOW).unwrap();
        assert_eq!(
            report,
            PruneReport {
                events: 6,
                aggregates: 1
            }
        );
        // Today's and the three days before it.
        assert_eq!(seqs(&storage, "ses_a"), vec![6, 7, 8, 9]);
        assert_eq!(seqs(&storage, "ses_b"), vec![0, 1]);
        // New events keep their numbering.
        storage.with_connection(|conn| {
            assert_eq!(sql::latest_sequence(conn, "ses_a").unwrap(), 9);
            assert_eq!(sql::earliest_sequence(conn, "ses_a").unwrap(), Some(6));
        });
        // A second pass finds nothing.
        let again = prune_events(&storage, &retention(Some(3), None), NOW).unwrap();
        assert_eq!(again, PruneReport::default());
    }

    #[test]
    fn keep_last_alone_keeps_only_the_newest() {
        let (storage, _dir) = storage();
        write_daily(&storage, "ses_a", 10);
        write_daily(&storage, "ses_b", 3);
        let report = prune_events(&storage, &retention(None, Some(4)), NOW).unwrap();
        assert_eq!(report.events, 6);
        assert_eq!(seqs(&storage, "ses_a"), vec![6, 7, 8, 9]);
        assert_eq!(seqs(&storage, "ses_b"), vec![0, 1, 2]);
    }

    #[test]
    fn with_both_an_event_is_kept_when_either_rule_keeps_it() {
        let (storage, _dir) = storage();
        write_daily(&storage, "ses_a", 10);
        // Three days would keep four; keep_last keeps six.
        prune_events(&storage, &retention(Some(3), Some(6)), NOW).unwrap();
        assert_eq!(seqs(&storage, "ses_a"), vec![4, 5, 6, 7, 8, 9]);
        // keep_last would keep two; three days keeps four.
        prune_events(&storage, &retention(Some(3), Some(2)), NOW).unwrap();
        assert_eq!(seqs(&storage, "ses_a"), vec![6, 7, 8, 9]);
    }

    #[test]
    fn a_large_prune_goes_in_batches() {
        let (storage, _dir) = storage();
        let count = PRUNE_BATCH as i64 * 2 + 10;
        storage.with_connection(|conn| {
            let tx = conn.unchecked_transaction().unwrap();
            for seq in 0..count {
                upsert_event_sequence(&tx, "ses_a", seq, None, false).unwrap();
                insert_event(
                    &tx,
                    &EventRow {
                        id: format!("evt_{}", ulid::Ulid::from_parts(NOW, seq as u128)),
                        aggregate_id: "ses_a".to_string(),
                        seq,
                        r#type: "message.updated.1".to_string(),
                        data: serde_json::json!({}),
                    },
                )
                .unwrap();
            }
            tx.commit().unwrap();
        });
        let report = prune_events(&storage, &retention(None, Some(5)), NOW).unwrap();
        assert_eq!(report.events, count as u64 - 5);
        assert_eq!(seqs(&storage, "ses_a").len(), 5);
    }

    #[test]
    fn retention_reads_from_config() {
        let config: Config = serde_json::from_str(
            r#"{"storage": {"retention": {"events": {"max_age": "30d", "keep_last": 5000}}}}"#,
        )
        .unwrap();
        assert_eq!(
            event_retention(&config),
            EventRetention {
                max_age_ms: Some(30 * DAY),
                keep_last: Some(5000),
            }
        );
        // The duration is written back the way it was read.
        let text = serde_json::to_string(&config).unwrap();
        assert!(text.contains(r#""max_age":"30d""#), "{text}");
        for bad in ["30", "d", "1 fortnight", "-3d"] {
            let text =
                format!(r#"{{"storage": {{"retention": {{"events": {{"max_age": "{bad}"}}}}}}}}"#);
            assert!(serde_json::from_str::<Config>(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_checkpoint_empties_the_write_ahead_log() {
        let (storage, dir) = storage();
        write_daily(&storage, "ses_a", 200);
        let wal = dir.path().join("db.sqlite-wal");
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);
        checkpoint(&storage).unwrap();
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
        assert_eq!(seqs(&storage, "ses_a").len(), 200);
    }

    #[tokio::test]
    async fn the_spawned_job_prunes_at_startup_and_ends_with_the_storage() {
        let dir = TempDir::new("maintenance-job");
        let storage = Arc::new(Storage::open(dir.path().join("db.sqlite")).unwrap());
        write_daily(&storage, "ses_a", 10);
        let job = spawn(
            &storage,
            EventRetention {
                max_age_ms: None,
                keep_last: Some(3),
            },
        );
        for _ in 0..100 {
            if seqs(&storage, "ses_a").len() == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(seqs(&storage, "ses_a"), vec![7, 8, 9]);
        job.abort();
    }
}
