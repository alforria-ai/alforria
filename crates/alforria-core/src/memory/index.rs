//! The transcript search index: full-text search over the text of past
//! sessions, for the `session_search` tool.
//!
//! Nothing is indexed when a message is written, so the hot path pays
//! nothing. Instead [`MemoryService::index_pending`] walks the `part` table
//! in insertion order (by rowid) from where it stopped, a small batch at a
//! time, and copies text parts into `transcript_fts`. A background loop runs
//! it; a search runs it first too, within a time budget. A database with
//! years of history is indexed gradually, each batch holding the connection
//! for a few milliseconds.
//!
//! Text that is still streaming is left for later: the walk stops at a text
//! part that changed in the last few seconds. Rows whose part or session has
//! since been deleted are filtered out when searching, and swept now and
//! then.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::memory::{fts_query, MemoryError, MemoryService};

/// How many parts one batch looks at.
pub const BATCH: usize = 200;
/// A text part untouched for this long is taken as finished.
pub const SETTLE_MS: u64 = 10_000;
/// How long a search may spend catching the index up first.
const SEARCH_BUDGET: Duration = Duration::from_millis(1500);
/// The pause between batches while there is more to index, so turns that are
/// running get the connection in between.
const BUSY_PAUSE: Duration = Duration::from_millis(50);
/// The pause once the index has caught up.
const IDLE_PAUSE: Duration = Duration::from_secs(20);

/// What one pass over the `part` table did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexProgress {
    /// Parts looked at.
    pub scanned: usize,
    /// Text parts added to the index.
    pub indexed: usize,
    /// There is more to do: the batch was full, or it stopped at text that
    /// is still being written.
    pub pending: bool,
}

/// One place a past session matched a search.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHit {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub title: String,
    #[serde(rename = "messageID")]
    pub message_id: String,
    pub role: String,
    /// When the matching text was written (unix milliseconds).
    pub time: u64,
    pub snippet: String,
    /// The session (or its parent) carried text from outside: the excerpt
    /// may be what an outsider wrote.
    #[serde(default)]
    pub untrusted: bool,
}

/// How far back [`recover_cursor`] looks for a part it recognises.
const RECOVERY_WINDOW: i64 = 5_000;

/// The part the cursor stood on is gone or is a different part: rows were
/// deleted at the end of the table, and their rowids may have been given to
/// new parts since. Walk back to the newest part that is in the index under
/// its own rowid; everything up to there has been looked at, and everything
/// after is looked at again (indexing is idempotent).
///
/// Finding none in a long stretch means the rowids were renumbered wholesale
/// (a VACUUM may do that): the index is emptied and rebuilt from the start.
fn recover_cursor(tx: &rusqlite::Transaction<'_>, cursor: i64) -> Result<i64, MemoryError> {
    let mut seen = 0;
    {
        let mut walk = tx.prepare(
            "SELECT p.rowid, p.id FROM part p
             WHERE p.rowid <= ?1 AND json_extract(p.data, '$.type') = 'text'
             ORDER BY p.rowid DESC LIMIT ?2",
        )?;
        let mut known = tx.prepare("SELECT part_id FROM transcript_fts WHERE rowid = ?1")?;
        let mut rows = walk.query(params![cursor, RECOVERY_WINDOW])?;
        while let Some(row) = rows.next()? {
            seen += 1;
            let rowid: i64 = row.get(0)?;
            let id: String = row.get(1)?;
            let indexed: Option<String> = known.query_row([rowid], |row| row.get(0)).optional()?;
            if indexed.as_deref() == Some(id.as_str()) {
                return Ok(rowid);
            }
        }
    }
    if seen >= RECOVERY_WINDOW {
        tx.execute("DELETE FROM transcript_fts", [])?;
    }
    Ok(0)
}

impl MemoryService {
    /// Index up to `limit` more parts. A text part touched within
    /// `settle_ms` stops the pass, to be picked up when it has finished.
    pub fn index_pending(
        &self,
        limit: usize,
        settle_ms: u64,
    ) -> Result<IndexProgress, MemoryError> {
        let now = self.now_ms();
        let settled_before = now.saturating_sub(settle_ms) as i64;
        self.storage()
            .with_connection_mut(|conn| -> Result<IndexProgress, MemoryError> {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let (mut cursor, anchor): (i64, Option<String>) = tx.query_row(
                    "SELECT cursor, anchor FROM transcript_index WHERE id = 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if cursor > 0 {
                    let there: Option<String> = tx
                        .query_row("SELECT id FROM part WHERE rowid = ?1", [cursor], |row| {
                            row.get(0)
                        })
                        .optional()?;
                    if there != anchor {
                        cursor = recover_cursor(&tx, cursor)?;
                    }
                }
                let mut progress = IndexProgress::default();
                let mut last: Option<String> = None;
                {
                    let mut select = tx.prepare(
                        "SELECT p.rowid, p.id, p.message_id, p.session_id, p.time_updated,
                                json_extract(p.data, '$.type'),
                                CASE WHEN json_extract(p.data, '$.type') = 'text'
                                     THEN json_extract(p.data, '$.text') END,
                                COALESCE(json_extract(p.data, '$.synthetic'), 0),
                                COALESCE(json_extract(p.data, '$.ignored'), 0),
                                json_extract(m.data, '$.role')
                         FROM part p LEFT JOIN message m ON m.id = p.message_id
                         WHERE p.rowid > ?1 ORDER BY p.rowid LIMIT ?2",
                    )?;
                    // An indexed part keeps its rowid, so indexing one twice
                    // replaces it, and a rowid that is used again takes over
                    // from the part that had it.
                    let mut forget = tx.prepare("DELETE FROM transcript_fts WHERE rowid = ?1")?;
                    let mut insert = tx.prepare(
                        "INSERT INTO transcript_fts (rowid, text, part_id, message_id, session_id, role)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    )?;
                    let mut rows = select.query(params![cursor, limit as i64])?;
                    while let Some(row) = rows.next()? {
                        let rowid: i64 = row.get(0)?;
                        let kind: Option<String> = row.get(5)?;
                        let is_text = kind.as_deref() == Some("text");
                        if is_text && row.get::<_, i64>(4)? > settled_before {
                            progress.pending = true;
                            break;
                        }
                        progress.scanned += 1;
                        cursor = rowid;
                        last = Some(row.get::<_, String>(1)?);
                        forget.execute([rowid])?;
                        if !is_text || row.get::<_, i64>(7)? != 0 || row.get::<_, i64>(8)? != 0 {
                            continue;
                        }
                        let text: Option<String> = row.get(6)?;
                        let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
                            continue;
                        };
                        insert.execute(params![
                            rowid,
                            text,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(9)?.unwrap_or_default(),
                        ])?;
                        progress.indexed += 1;
                    }
                }
                if progress.scanned == limit {
                    progress.pending = true;
                }
                if last.is_none() && cursor > 0 {
                    last = tx
                        .query_row("SELECT id FROM part WHERE rowid = ?1", [cursor], |row| {
                            row.get(0)
                        })
                        .optional()?;
                }
                tx.execute(
                    "UPDATE transcript_index SET cursor = ?1, anchor = ?2, time_updated = ?3 WHERE id = 1",
                    params![cursor, last, now as i64],
                )?;
                tx.commit()?;
                Ok(progress)
            })
    }

    /// Index until caught up or `budget` is spent. Returns whether more is
    /// left to do.
    pub fn index_within(&self, budget: Duration, settle_ms: u64) -> bool {
        let started = Instant::now();
        loop {
            match self.index_pending(BATCH, settle_ms) {
                Ok(progress) if progress.pending && progress.scanned > 0 => {
                    if started.elapsed() >= budget {
                        return true;
                    }
                }
                Ok(progress) => return progress.pending,
                Err(error) => {
                    tracing::warn!("transcript indexing failed: {error}");
                    return true;
                }
            }
        }
    }

    /// Remove index rows whose part or session no longer exists (a deleted
    /// session, a reverted message), a batch at a time. Returns how many went.
    pub fn sweep_index(&self, limit: usize) -> Result<usize, MemoryError> {
        self.storage().with_connection(|conn| {
            Ok(conn.execute(
                "DELETE FROM transcript_fts WHERE rowid IN (
                    SELECT f.rowid FROM transcript_fts f
                    LEFT JOIN part p ON p.id = f.part_id
                    LEFT JOIN session s ON s.id = f.session_id
                    WHERE p.id IS NULL OR s.id IS NULL LIMIT ?1)",
                [limit as i64],
            )?)
        })
    }

    /// Full-text search over past sessions, best match first: this project's
    /// (every project's with `scope: "global"`), without `exclude_session`.
    /// The second value says the index is still being built, so results may
    /// be incomplete.
    pub fn search_sessions(
        &self,
        query: &str,
        limit: usize,
        exclude_session: Option<&str>,
        project_id: &str,
    ) -> Result<(Vec<SessionHit>, bool), MemoryError> {
        if !self.enabled() {
            return Err(MemoryError::Disabled);
        }
        let query = query.trim();
        if query.is_empty() {
            return Err(MemoryError::Invalid("search needs a query".to_string()));
        }
        let building = self.index_within(SEARCH_BUDGET, SETTLE_MS);
        let limit = limit.clamp(1, 50);
        for operator in ["AND", "OR"] {
            let Some(expression) = fts_query(query, operator) else {
                break;
            };
            let hits = self.storage().with_connection(|conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT transcript_fts.session_id, s.title, transcript_fts.message_id,
                            transcript_fts.role, p.time_created,
                            snippet(transcript_fts, 0, '[', ']', '…', 32),
                            {untrusted}
                     FROM transcript_fts
                     JOIN part p ON p.id = transcript_fts.part_id
                     JOIN session s ON s.id = transcript_fts.session_id
                     WHERE transcript_fts MATCH ?1
                       AND (?2 OR s.project_id = ?3)
                       AND (?4 IS NULL OR transcript_fts.session_id != ?4)
                     ORDER BY rank LIMIT ?5",
                    untrusted = crate::session::store::UNTRUSTED_INPUT_SQL
                ))?;
                let rows = stmt
                    .query_map(
                        params![
                            expression,
                            self.settings().global,
                            project_id,
                            exclude_session,
                            limit as i64
                        ],
                        |row| {
                            Ok(SessionHit {
                                session_id: row.get(0)?,
                                title: row.get(1)?,
                                message_id: row.get(2)?,
                                role: row.get(3)?,
                                time: row.get::<_, i64>(4)?.max(0) as u64,
                                snippet: row.get(5)?,
                                untrusted: row.get::<_, i64>(6)? != 0,
                            })
                        },
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, MemoryError>(rows)
            })?;
            if !hits.is_empty() {
                return Ok((hits, building));
            }
        }
        Ok((Vec::new(), building))
    }

    /// Keep the index up to date in the background for as long as the
    /// service lives. Needs a tokio runtime; without one, searches still
    /// index what they need first.
    pub fn start_indexer(self: &Arc<Self>) {
        if !self.enabled() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak = Arc::downgrade(self);
        runtime.spawn(async move {
            let mut swept = false;
            loop {
                let Some(service) = weak.upgrade() else {
                    break;
                };
                if service.indexer_stopped() {
                    break;
                }
                let pass = tokio::task::spawn_blocking(move || {
                    let progress = service.index_pending(BATCH, SETTLE_MS);
                    let caught_up = matches!(&progress, Ok(progress) if progress.scanned == 0);
                    (progress, caught_up, service)
                })
                .await;
                let Ok((progress, caught_up, service)) = pass else {
                    break;
                };
                let busy = match progress {
                    Ok(progress) => progress.scanned == BATCH,
                    Err(error) => {
                        tracing::warn!("transcript indexing failed: {error}");
                        false
                    }
                };
                if caught_up && !swept {
                    // Once per start, when there is nothing else to do.
                    swept = true;
                    let _ = tokio::task::spawn_blocking(move || {
                        while matches!(service.sweep_index(500), Ok(removed) if removed == 500) {}
                    })
                    .await;
                } else {
                    drop(service);
                }
                tokio::time::sleep(if busy { BUSY_PAUSE } else { IDLE_PAUSE }).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::test_support::{service, settings};
    use crate::session::test_support::{
        assistant_message, create_session, harness, text_part, user_message, with_parts,
    };

    /// Write one message with one text part.
    fn say(
        h: &crate::session::test_support::Harness,
        session: &str,
        index: usize,
        user: bool,
        text: &str,
    ) -> String {
        let message_id = format!("msg_{session}_{index:04}");
        let part_id = format!("prt_{session}_{index:04}");
        let info = if user {
            user_message(session, &message_id, 1.0)
        } else {
            assistant_message(session, &message_id, "msg_parent", 1, None, None)
        };
        h.services.sessions.update_message(&info).unwrap();
        let part = text_part(session, &message_id, &part_id, text);
        h.services.sessions.update_part(&part).unwrap();
        let _ = with_parts;
        part_id
    }

    #[test]
    fn past_sessions_are_indexed_lazily_and_searched_within_the_project() {
        let h = harness("memory-index", Vec::new());
        let memory = service(h.services.storage.clone(), settings("global"));
        let first = create_session(&h.services.sessions, &h.worktree);
        h.services
            .sessions
            .set_title(&first.id, "Billing migration")
            .unwrap();
        say(
            &h,
            &first.id,
            1,
            true,
            "How do we move invoices to the new ledger?",
        );
        say(
            &h,
            &first.id,
            2,
            false,
            "Export them with the reconcile script, then import.",
        );
        let second = create_session(&h.services.sessions, &h.worktree);
        say(&h, &second.id, 1, true, "What colour is the ledger cover?");

        // Nothing was indexed when the messages were written.
        let rows = |memory: &MemoryService| -> i64 {
            memory
                .storage()
                .with_connection(|conn| {
                    conn.query_row("SELECT COUNT(*) FROM transcript_fts", [], |row| row.get(0))
                })
                .unwrap()
        };
        assert_eq!(rows(&memory), 0);

        // A small batch at a time, resuming where it stopped.
        let pass = memory.index_pending(2, 0).unwrap();
        assert_eq!((pass.scanned, pass.indexed, pass.pending), (2, 2, true));
        let pass = memory.index_pending(2, 0).unwrap();
        assert_eq!((pass.scanned, pass.indexed, pass.pending), (1, 1, false));
        assert_eq!(
            memory.index_pending(2, 0).unwrap(),
            IndexProgress::default()
        );
        assert_eq!(rows(&memory), 3);

        let (hits, building) = memory
            .search_sessions("reconcile", 10, None, "global")
            .unwrap();
        assert!(!building);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, first.id);
        assert_eq!(hits[0].title, "Billing migration");
        assert_eq!(hits[0].role, "assistant");
        assert!(
            hits[0].snippet.contains("[reconcile]"),
            "{}",
            hits[0].snippet
        );
        assert!(hits[0].time > 0);

        // Both sessions mention the ledger; the current one can be left out.
        let (hits, _) = memory
            .search_sessions("ledger", 10, None, "global")
            .unwrap();
        assert_eq!(hits.len(), 2);
        let (hits, _) = memory
            .search_sessions("ledger", 10, Some(&second.id), "global")
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, first.id);
        // Another project sees none of it, unless memory is global.
        assert!(memory
            .search_sessions("ledger", 10, None, "proj_other")
            .unwrap()
            .0
            .is_empty());
        let mut everywhere = settings("global");
        everywhere.global = true;
        let global = service(h.services.storage.clone(), everywhere);
        assert_eq!(
            global
                .search_sessions("ledger", 10, None, "proj_other")
                .unwrap()
                .0
                .len(),
            2
        );
        // Odd characters are text, not query syntax.
        for query in ["\"", "ledger AND (", "NEAR(a b)", "*", "session_id:x"] {
            memory.search_sessions(query, 10, None, "global").unwrap();
        }
    }

    #[test]
    fn streaming_text_synthetic_parts_and_deleted_sessions_stay_out() {
        let h = harness("memory-index-skip", Vec::new());
        // On the wall clock, which is what stamps the part rows.
        let memory = MemoryService::new(
            h.services.storage.clone(),
            None,
            Arc::new(crate::catalog::SystemClock),
            settings("global"),
        );
        let session = create_session(&h.services.sessions, &h.worktree);
        say(
            &h,
            &session.id,
            1,
            true,
            "The zebra crossing needs repainting.",
        );
        // A synthetic part is the system talking, not the conversation.
        let synthetic = match text_part(
            &session.id,
            &format!("msg_{}_0001", session.id),
            "prt_syn",
            "zebra reminder",
        ) {
            alforria_schema::session_v1::V1Part::Text {
                id,
                session_id,
                message_id,
                text,
                ignored,
                time,
                metadata,
                ..
            } => alforria_schema::session_v1::V1Part::Text {
                id,
                session_id,
                message_id,
                text,
                synthetic: Some(true),
                ignored,
                time,
                metadata,
            },
            other => other,
        };
        h.services.sessions.update_part(&synthetic).unwrap();

        // Text written a moment ago may still be streaming: not yet.
        let pass = memory.index_pending(50, 10_000).unwrap();
        assert_eq!((pass.scanned, pass.indexed, pass.pending), (0, 0, true));
        let pass = memory.index_pending(50, 0).unwrap();
        assert_eq!((pass.scanned, pass.indexed), (2, 1));
        assert_eq!(
            memory
                .search_sessions("zebra", 10, None, "global")
                .unwrap()
                .0
                .len(),
            1
        );

        // Deleting the session hides its text at once, and the sweep drops it.
        h.services.sessions.remove(&session.id).unwrap();
        assert!(memory
            .search_sessions("zebra", 10, None, "global")
            .unwrap()
            .0
            .is_empty());
        assert_eq!(memory.sweep_index(100).unwrap(), 1);
        assert_eq!(memory.sweep_index(100).unwrap(), 0);
    }

    #[test]
    fn a_large_backlog_is_indexed_in_bounded_passes() {
        let h = harness("memory-index-large", Vec::new());
        let memory = service(h.services.storage.clone(), settings("global"));
        let session = create_session(&h.services.sessions, &h.worktree);
        for index in 0..1050 {
            say(
                &h,
                &session.id,
                index,
                index % 2 == 0,
                &format!("note number {index} about walruses"),
            );
        }
        let mut passes = 0;
        loop {
            let pass = memory.index_pending(BATCH, 0).unwrap();
            assert!(pass.scanned <= BATCH);
            passes += 1;
            if !pass.pending {
                break;
            }
        }
        assert_eq!(passes, 6, "1050 parts at 200 a pass");
        let (hits, building) = memory
            .search_sessions("walruses 1049", 5, None, "global")
            .unwrap();
        assert!(!building);
        assert_eq!(hits.len(), 1);
        // The newest parts are deleted and their rowids go to new parts
        // before the indexer looks again: it notices and indexes the new ones.
        memory
            .storage()
            .with_connection(|conn| {
                conn.execute("DELETE FROM part WHERE rowid > 1040", [])?;
                conn.execute(
                    "DELETE FROM message WHERE id NOT IN (SELECT message_id FROM part)",
                    [],
                )
            })
            .unwrap();
        for index in 2000..2010 {
            say(
                &h,
                &session.id,
                index,
                true,
                &format!("note number {index} about narwhals"),
            );
        }
        assert!(!memory.index_within(Duration::from_secs(30), 0));
        let (hits, _) = memory
            .search_sessions("narwhals", 50, None, "global")
            .unwrap();
        assert_eq!(hits.len(), 10);
        assert!(memory
            .search_sessions("1049", 5, None, "global")
            .unwrap()
            .0
            .is_empty());
        let (hits, _) = memory
            .search_sessions("walruses 1039", 5, None, "global")
            .unwrap();
        assert_eq!(hits.len(), 1, "an old part is indexed once, not twice");
        // Renumbered wholesale (the index no longer matches any rowid): rebuilt.
        memory
            .storage()
            .with_connection(|conn| {
                conn.execute("UPDATE transcript_fts SET part_id = 'prt_moved'", [])?;
                conn.execute("UPDATE transcript_index SET anchor = 'prt_moved'", [])
            })
            .unwrap();
        assert!(!memory.index_within(Duration::from_secs(30), 0));
        let count: i64 = memory
            .storage()
            .with_connection(|conn| {
                conn.query_row("SELECT COUNT(*) FROM transcript_fts", [], |row| row.get(0))
            })
            .unwrap();
        assert_eq!(count, 1050);
        let (hits, _) = memory
            .search_sessions("narwhals", 50, None, "global")
            .unwrap();
        assert_eq!(hits.len(), 10);
    }
}
