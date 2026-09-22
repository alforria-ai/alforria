//! The rusqlite connection: open + PRAGMAs + busy handling (port of
//! `packages/core/src/database/database.ts`).

use std::path::{Path, PathBuf};
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

/// A `rusqlite` connection behind a `Mutex`.
///
/// All storage APIs are synchronous; async callers on the tokio runtime
/// should go through `tokio::task::spawn_blocking` (the handle is
/// `block_in_place`-friendly — it never holds the lock across an await
/// point because it never awaits). There is no async SQLite driver: the
/// bundled SQLite is synchronous and that is fine at this scale.
#[derive(Debug)]
pub struct Storage {
    conn: Mutex<Connection>,
}

impl Storage {
    /// Open `<path>`, apply the PRAGMAs and run the migrations
    /// (`database.ts` wires `DatabaseMigration.apply` right after the
    /// PRAGMAs, so opening an incompatible database fails here).
    pub fn open(path: impl AsRef<Path>) -> Result<Storage, CoreError> {
        let mut conn = Connection::open(path)?;
        Self::configure(&conn)?;
        migration::apply(&mut conn)?;
        Ok(Storage {
            conn: Mutex::new(conn),
        })
    }

    /// Open an in-memory database (no `WAL` journal — SQLite keeps the
    /// `memory` journal mode there).
    pub fn open_in_memory() -> Result<Storage, CoreError> {
        let mut conn = Connection::open_in_memory()?;
        Self::configure(&conn)?;
        migration::apply(&mut conn)?;
        Ok(Storage {
            conn: Mutex::new(conn),
        })
    }

    /// Open `<data_dir>/opencode.db` (`database.ts:53`).
    pub fn open_default(data_dir: &Path) -> Result<Storage, CoreError> {
        std::fs::create_dir_all(data_dir).map_err(|err| CoreError::Storage(err.to_string()))?;
        Storage::open(db_path(data_dir))
    }

    /// Run a closure with the connection.
    pub fn with_connection<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        let conn = self.lock();
        f(&conn)
    }

    /// Run a closure with mutable access to the connection (needed for
    /// transactions).
    pub fn with_connection_mut<R>(&self, f: impl FnOnce(&mut Connection) -> R) -> R {
        let mut conn = self.lock();
        f(&mut conn)
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

    /// Runs `PRAGMA wal_checkpoint(PASSIVE)` on close (spec §7.5).
    fn checkpoint(&self) -> Result<(), CoreError> {
        self.with_connection(|conn| {
            conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
            Ok(())
        })
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        if let Err(err) = self.checkpoint() {
            tracing::warn!("failed to checkpoint WAL on close: {err}");
        }
    }
}

/// The default database path inside the opencode data directory
/// (`database.ts:53`).
pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("opencode.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::storage::test_support::TempDir;

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
