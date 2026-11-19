//! The rusqlite connection: open + PRAGMAs + busy handling (port of
//! `packages/core/src/database/database.ts`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;

use crate::storage::migration;
use crate::CoreError;

/// PRAGMAs run on every open (`database.ts:27-32`).
const OPEN_PRAGMAS: &[&str] = &[
    "PRAGMA synchronous = NORMAL",
    "PRAGMA busy_timeout = 5000",
    "PRAGMA cache_size = -64000",
    "PRAGMA foreign_keys = ON",
];

// CASE keeps json_extract from seeing malformed JSON even if SQLite
// reorders predicates or picks a different plan.
pub(crate) const TURN_ASSISTANT: &str =
    "json_extract(CASE WHEN json_valid(data) THEN data END, '$.role') = 'assistant'";
pub(crate) const TURN_PARENT: &str =
    "json_extract(CASE WHEN json_valid(data) THEN data END, '$.parentID')";
pub(crate) const TURN_INDEX: &str = "alforria_valid_message_session_parent_idx";

/// Install the local index without changing the upstream migration journal.
fn install_turn_index(conn: &Connection) -> Result<(), CoreError> {
    conn.execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS {TURN_INDEX}
         ON message (session_id, {TURN_PARENT}, time_created, id)
         WHERE {TURN_ASSISTANT};
         DROP INDEX IF EXISTS alforria_message_session_parent_idx;"
    ))?;
    Ok(())
}

/// A `rusqlite` connection behind a `Mutex`.
///
/// Synchronous calls yield the multithreaded Tokio worker before waiting on
/// SQLite or this mutex. Single-threaded runtimes must use `spawn_blocking`;
/// ordinary synchronous callers run directly. Transactions remain on one
/// connection and never hold its lock across an await.
#[derive(Debug)]
pub struct Storage {
    conn: Mutex<Connection>,
    /// Message rows read back with their parts since this handle opened.
    message_rows_read: AtomicU64,
}

impl Storage {
    /// Open `<path>`, apply the PRAGMAs and run the migrations
    /// (`database.ts` wires `DatabaseMigration.apply` right after the
    /// PRAGMAs, so opening an incompatible database fails here).
    pub fn open(path: impl AsRef<Path>) -> Result<Storage, CoreError> {
        database_blocking(|| Self::open_impl(path.as_ref()))
    }

    fn open_impl(path: &Path) -> Result<Storage, CoreError> {
        // A test run never opens (and so never migrates) a database outside
        // a sandbox root: `paths::sandbox`.
        crate::paths::sandbox::guard_path(path, "Storage::open");
        let mut conn = Connection::open(path)?;
        Self::configure(&conn)?;
        refuse_foreign_schema(&conn, path)?;
        migration::apply(&mut conn)?;
        crate::storage::alforria_migration::apply(&mut conn)?;
        install_turn_index(&conn)?;
        Ok(Storage {
            conn: Mutex::new(conn),
            message_rows_read: AtomicU64::new(0),
        })
    }

    /// Open an in-memory database (no `WAL` journal — SQLite keeps the
    /// `memory` journal mode there).
    pub fn open_in_memory() -> Result<Storage, CoreError> {
        database_blocking(Self::open_in_memory_impl)
    }

    fn open_in_memory_impl() -> Result<Storage, CoreError> {
        let mut conn = Connection::open_in_memory()?;
        Self::configure(&conn)?;
        migration::apply(&mut conn)?;
        crate::storage::alforria_migration::apply(&mut conn)?;
        install_turn_index(&conn)?;
        Ok(Storage {
            conn: Mutex::new(conn),
            message_rows_read: AtomicU64::new(0),
        })
    }

    /// `PRAGMA synchronous`: `full` puts every commit on disk before it
    /// returns, `normal` (the default at open) leaves the write-ahead log
    /// to the operating system between checkpoints.
    pub fn set_synchronous(&self, full: bool) -> Result<(), CoreError> {
        self.with_connection(|conn| {
            conn.execute_batch(if full {
                "PRAGMA synchronous = FULL"
            } else {
                "PRAGMA synchronous = NORMAL"
            })?;
            Ok(())
        })
    }

    /// How many message rows have been read back with their parts: what a
    /// step costs in history, for tests and diagnostics.
    pub fn message_rows_read(&self) -> u64 {
        self.message_rows_read.load(Ordering::Relaxed)
    }

    pub(crate) fn count_message_rows(&self, rows: usize) {
        self.message_rows_read
            .fetch_add(rows as u64, Ordering::Relaxed);
    }

    /// Open `<data_dir>/opencode.db` (`database.ts:53`).
    pub fn open_default(data_dir: &Path) -> Result<Storage, CoreError> {
        std::fs::create_dir_all(data_dir).map_err(|err| CoreError::Storage(err.to_string()))?;
        Storage::open(db_path(data_dir))
    }

    /// Run a closure with the connection.
    pub fn with_connection<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        database_blocking(|| {
            let conn = self.lock();
            f(&conn)
        })
    }

    /// Run a closure with mutable access to the connection (needed for
    /// transactions).
    pub fn with_connection_mut<R>(&self, f: impl FnOnce(&mut Connection) -> R) -> R {
        database_blocking(|| {
            let mut conn = self.lock();
            f(&mut conn)
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A poisoned lock only means another thread panicked while holding
        // it; the connection itself is still consistent, so recover.
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn configure(conn: &Connection) -> Result<(), CoreError> {
        // journal_mode and wal_checkpoint return a row; the rest do not.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        for pragma in OPEN_PRAGMAS {
            conn.execute_batch(pragma)?;
        }
        conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// Move the whole write-ahead log into the database file and empty it
    /// (`PRAGMA wal_checkpoint(TRUNCATE)`). A server about to exit calls
    /// this so the database is one self-contained file; `false` means a
    /// reader held the log and some frames stay in it (still durable).
    pub fn checkpoint_truncate(&self) -> Result<bool, CoreError> {
        self.with_connection(|conn| {
            let busy: i64 =
                conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
            Ok(busy == 0)
        })
    }

    /// Runs `PRAGMA wal_checkpoint(PASSIVE)` on close (spec §7.5).
    fn checkpoint(&self) -> Result<(), CoreError> {
        self.with_connection(|conn| {
            conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
            Ok(())
        })
    }
}

pub(crate) fn database_blocking<R>(operation: impl FnOnce() -> R) -> R {
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(operation)
    } else {
        operation()
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        if let Err(err) = self.checkpoint() {
            tracing::warn!("failed to checkpoint WAL on close: {err}");
        }
    }
}

/// opencode v2 migrates a shared `opencode.db` in place: it drops and
/// reshapes tables alforria reads and empties the event log. Opening such a
/// database would fail later in confusing ways, or write rows v2 then
/// mishandles, so stop here with an explanation instead.
fn refuse_foreign_schema(conn: &Connection, path: &Path) -> Result<(), CoreError> {
    let migrated: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_v2')",
        [],
        |row| row.get(0),
    )?;
    if !migrated {
        return Ok(());
    }
    Err(CoreError::Storage(format!(
        "{} was migrated by opencode v2 and can no longer be read by alforria. \
         alforria keeps its data in its own `alforria` directory: create it (or run \
         `alforria migrate --fresh`) to start with a new database, leaving this one to opencode.",
        path.display()
    )))
}

/// The default database path inside the data directory
/// (`database.ts:53`).
pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("opencode.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::storage::test_support::TempDir;

    #[test]
    fn installs_turn_index_when_opening_an_existing_database() {
        let dir = TempDir::new("turn-index-upgrade");
        let path = dir.path().join("db.sqlite");
        let mut connection = Connection::open(&path).unwrap();
        migration::apply(&mut connection).unwrap();
        connection
            .execute_batch(
                r#"INSERT INTO project (id, worktree, time_created, time_updated, sandboxes)
                   VALUES ('project', '/repo', 0, 0, '[]');
                   INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated)
                   VALUES ('session', 'project', 'test', '/repo', 'Test', '1', 0, 0);
                   INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES
                       ('bad', 'session', 1, 1, '{broken'),
                       ('reply', 'session', 2, 2, '{"role":"assistant","parentID":"user"}');"#,
            )
            .unwrap();
        // This represents a database from before the local query index.
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
                    [TURN_INDEX],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        drop(connection);
        for _ in 0..2 {
            Storage::open(&path).unwrap().with_connection(|conn| {
                assert_eq!(
                    conn.query_row(
                        "SELECT COUNT(*) FROM pragma_index_list('message') WHERE name = ?1",
                        [TURN_INDEX],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                    1
                );
                // Additive indexes do not alter the upstream journal.
                assert_eq!(
                    conn.query_row("SELECT COUNT(*) FROM migration", [], |row| row
                        .get::<_, usize>(0))
                        .unwrap(),
                    migration::MIGRATION_IDS.len()
                );
                assert_eq!(
                    conn.query_row("SELECT data FROM message WHERE id = 'bad'", [], |row| {
                        row.get::<_, String>(0)
                    })
                    .unwrap(),
                    "{broken",
                    "opening the database must leave existing rows intact"
                );
            });
        }
    }

    #[test]
    fn a_database_migrated_by_opencode_v2_is_refused_with_a_way_out() {
        let dir = TempDir::new("foreign-schema");
        let path = dir.path().join("opencode.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE session_v2 (id TEXT PRIMARY KEY)")
            .unwrap();
        let error = Storage::open(&path).unwrap_err().to_string();
        assert!(error.contains("migrated by opencode v2"), "{error}");
        assert!(error.contains("alforria migrate --fresh"), "{error}");
        // Nothing was added to the foreign database.
        let tables: i64 = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
    }

    #[test]
    fn contended_sqlite_does_not_stall_a_tokio_worker() {
        use std::sync::{mpsc, Arc};
        use std::time::Duration;
        let dir = TempDir::new("busy-runtime");
        let path = dir.path().join("db.sqlite");
        let storage = Arc::new(Storage::open(&path).unwrap());
        storage
            .with_connection(|conn| conn.execute("CREATE TABLE probe (id INTEGER)", []))
            .unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (tick_tx, tick_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let connection = Connection::open(path).unwrap();
            connection.execute_batch("BEGIN IMMEDIATE").unwrap();
            ready_tx.send(()).unwrap();
            let responsive = tick_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            connection.execute_batch("COMMIT").unwrap();
            responsive
        });
        ready_rx.recv().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let write = tokio::spawn(async move {
                storage
                    .with_connection(|conn| conn.execute("INSERT INTO probe VALUES (1)", []))
                    .unwrap();
            });
            tokio::time::sleep(Duration::from_millis(30)).await;
            tick_tx.send(()).unwrap();
            write.await.unwrap();
        });
        assert!(
            holder.join().unwrap(),
            "the timer must run while SQLite waits for the foreign writer"
        );
    }

    #[test]
    fn open_applies_pragmas() {
        let dir = TempDir::new("pragmas");
        let storage = Storage::open(dir.path().join("db.sqlite")).unwrap();
        storage.with_connection(|conn| {
            let journal_mode: String = conn
                .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                .unwrap();
            assert_eq!(journal_mode, "wal");
            let synchronous: i64 = conn
                .query_row("PRAGMA synchronous", [], |row| row.get(0))
                .unwrap();
            assert_eq!(synchronous, 1 /* NORMAL */);
            let busy_timeout: i64 = conn
                .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
                .unwrap();
            assert_eq!(busy_timeout, 5000);
            let cache_size: i64 = conn
                .query_row("PRAGMA cache_size", [], |row| row.get(0))
                .unwrap();
            assert_eq!(cache_size, -64000);
            let foreign_keys: i64 = conn
                .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
                .unwrap();
            assert_eq!(foreign_keys, 1);
        });
    }

    #[test]
    fn open_migrates_the_database() {
        let dir = TempDir::new("migrates");
        let storage = Storage::open(dir.path().join("db.sqlite")).unwrap();
        let count = storage
            .with_connection(|conn| {
                conn.query_row("SELECT COUNT(*) FROM migration", [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .unwrap();
        assert_eq!(count, migration::MIGRATION_IDS.len() as i64);
    }

    #[test]
    fn in_memory_storage() {
        let storage = Storage::open_in_memory().unwrap();
        assert!(storage.list_projects().unwrap().is_empty());
    }

    /// A foreign connection holding the write lock must not fail our
    /// write transactions: `BEGIN IMMEDIATE` acquires the write lock
    /// upfront, so the `busy_timeout` window is honored instead of a
    /// deferred read→write upgrade failing immediately with "database
    /// is locked" (the WAL snapshot-upgrade trap).
    #[test]
    fn write_transactions_wait_for_a_foreign_writer() {
        let dir = TempDir::new("foreign-writer");
        let path = dir.path().join("db.sqlite");
        let storage = Storage::open(&path).unwrap();
        storage.with_connection(|conn| {
            conn.execute("CREATE TABLE busy_probe (id INTEGER)", [])
                .unwrap();
        });

        let holder = Connection::open(&path).unwrap();
        holder.execute("BEGIN IMMEDIATE", []).unwrap();
        holder
            .execute("INSERT INTO busy_probe (id) VALUES (1)", [])
            .unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            holder.execute("COMMIT", []).unwrap();
        });

        storage
            .with_connection_mut(|conn| -> Result<(), rusqlite::Error> {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .unwrap();
                tx.execute("INSERT INTO busy_probe (id) VALUES (2)", [])
                    .unwrap();
                tx.commit()
            })
            .unwrap();
        releaser.join().unwrap();

        storage.with_connection(|conn| {
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM busy_probe", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 2);
        });
    }
}
