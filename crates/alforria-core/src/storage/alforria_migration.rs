//! alforria's own migrations — the one mechanism for every table upstream
//! does not have.
//!
//! [`crate::storage::migration`] keeps the upstream journal byte-compatible
//! and [`crate::storage::schema::SCHEMA_SQL`] is compared against upstream,
//! so neither can carry a table upstream does not have. Tables that only
//! alforria uses live here instead: SQL files under `alforria_migrations/`,
//! applied in the order of [`MIGRATIONS`] after the upstream schema (which
//! they may read and whose rows they may update, never its shape) and
//! recorded in their own journal table, `alforria_migration`.
//!
//! To add one: drop a new file in `alforria_migrations/`, named
//! `<number>_<area>_<what>.sql` with the next free number, and append it to
//! [`MIGRATIONS`]. Never edit a file that has shipped, never rename an id
//! and never reorder the list: the journal knows a migration by its id, and
//! a database that has already run it must find the same id again.
//!
//! The first five ids come from two branches that each grew a migration set
//! of their own before they were merged (`0001`/`0002` and the
//! `trigger_`/`memory_` ones). Both journaled into this same table with
//! these same ids, so a database that ran either set — or both, in either
//! order — finds its work recorded and only gets what it is missing. The
//! ids are kept as they were written for that reason; new ones follow the
//! numbered scheme from `0006`.

use rusqlite::Connection;

use crate::CoreError;

/// `(id, sql)`, in the order they are applied. Append only.
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_session_restriction",
        include_str!("alforria_migrations/0001_session_restriction.sql"),
    ),
    (
        "0002_outbox_create",
        include_str!("alforria_migrations/0002_outbox_create.sql"),
    ),
    (
        "trigger_0001_init",
        include_str!("alforria_migrations/trigger_0001_init.sql"),
    ),
    (
        "memory_0001_init",
        include_str!("alforria_migrations/memory_0001_init.sql"),
    ),
    (
        "memory_0002_provenance",
        include_str!("alforria_migrations/memory_0002_provenance.sql"),
    ),
    (
        "0006_session_mark",
        include_str!("alforria_migrations/0006_session_mark.sql"),
    ),
    (
        "0007_session_ceiling",
        include_str!("alforria_migrations/0007_session_ceiling.sql"),
    ),
];

/// The journal table.
pub const JOURNAL: &str = "alforria_migration";

/// Every table these migrations own, the journal included, and the shadow
/// tables SQLite keeps for the full-text indexes. The upstream schema
/// comparison leaves them out; a migration that adds a table adds it here
/// (a test checks).
pub const OWN_TABLES: &[&str] = &[
    JOURNAL,
    "session_restriction",
    "session_mark",
    "session_ceiling",
    "outbox",
    "trigger",
    "trigger_state",
    "trigger_run",
    "memory",
    "memory_fts",
    "memory_fts_data",
    "memory_fts_idx",
    "memory_fts_content",
    "memory_fts_docsize",
    "memory_fts_config",
    "transcript_fts",
    "transcript_fts_data",
    "transcript_fts_idx",
    "transcript_fts_content",
    "transcript_fts_docsize",
    "transcript_fts_config",
    "transcript_index",
];

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Apply every migration not yet in the journal. The upstream schema must
/// already be there ([`crate::storage::migration::apply`]).
pub fn apply(conn: &mut Connection) -> Result<(), CoreError> {
    apply_set(conn, MIGRATIONS)
}

/// [`apply`] over an explicit list (tests stop short of a migration to
/// build the database it has to bring over).
pub(crate) fn apply_set(
    conn: &mut Connection,
    migrations: &[(&str, &str)],
) -> Result<(), CoreError> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS alforria_migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)",
        [],
    )?;
    for (id, sql) in migrations {
        // Check and apply under one write lock, each migration in its own
        // transaction: two processes opening the same database at once (a
        // TUI beside `alforria web`) must not both decide the migration is
        // theirs to run, and one that fails leaves nothing behind and no
        // journal row.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let applied: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM alforria_migration WHERE id = ?1)",
            [id],
            |row| row.get(0),
        )?;
        if !applied {
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO alforria_migration (id, time_completed) VALUES (?1, ?2)",
                rusqlite::params![id, now_ms()],
            )?;
        }
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema::SCHEMA_SQL;

    /// A database at the upstream schema and nothing else.
    fn upstream() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::storage::migration::apply(&mut conn).unwrap();
        conn
    }

    fn column(conn: &Connection, sql: &str) -> Vec<String> {
        conn.prepare(sql)
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn journal(conn: &Connection) -> Vec<String> {
        column(conn, "SELECT id FROM alforria_migration ORDER BY rowid")
    }

    /// Everything in `sqlite_master` that is ours, by name and SQL: what
    /// two databases must agree on to count as migrated alike.
    fn own_objects(conn: &Connection) -> Vec<String> {
        let mut rows: Vec<String> = conn
            .prepare("SELECT tbl_name, name, COALESCE(sql, '') FROM sqlite_master")
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .filter(|(table, _, _)| OWN_TABLES.contains(&table.as_str()))
            .map(|(table, name, sql)| format!("{table}|{name}|{sql}"))
            .collect();
        rows.sort();
        rows
    }

    /// The upstream part of `sqlite_master`, in creation order.
    fn upstream_objects(conn: &Connection) -> Vec<String> {
        conn.prepare(
            "SELECT tbl_name, sql FROM sqlite_master
             WHERE sql IS NOT NULL AND name != 'migration' ORDER BY rowid",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .filter(|(table, _)| !OWN_TABLES.contains(&table.as_str()))
        .map(|(_, sql)| sql)
        .collect()
    }

    fn all_ids() -> Vec<String> {
        MIGRATIONS.iter().map(|(id, _)| id.to_string()).collect()
    }

    fn subset(ids: &[&str]) -> Vec<(&'static str, &'static str)> {
        ids.iter()
            .map(|id| {
                *MIGRATIONS
                    .iter()
                    .find(|(known, _)| known == id)
                    .unwrap_or_else(|| panic!("no migration {id}"))
            })
            .collect()
    }

    /// (a) An empty database, as `Storage::open` builds one: the upstream
    /// schema, then every migration, in order, each exactly once.
    #[test]
    fn an_empty_database_gets_every_migration_once() {
        let storage = crate::storage::Storage::open_in_memory().unwrap();
        storage.with_connection(|conn| {
            assert_eq!(journal(conn), all_ids());
            assert_eq!(upstream_objects(conn), SCHEMA_SQL.to_vec());
            conn.execute(
                "INSERT INTO session_restriction (session_id, time_created) VALUES ('ses_1', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO memory (id, project_id, kind, content, time_created, time_updated)
                 VALUES ('mem_1', 'p', 'note', 'x', 1, 1)",
                [],
            )
            .unwrap();
            let trust: String = conn
                .query_row("SELECT trust FROM memory WHERE id = 'mem_1'", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(trust, "unverified", "a row nobody classified");
        });
    }

    /// (b) A database at the upstream schema only (an install from before
    /// alforria had tables of its own): the migrations apply on top, twice
    /// over without effect, and the upstream schema is left byte-identical.
    #[test]
    fn a_database_at_the_upstream_schema_is_brought_up_and_left_intact() {
        let mut conn = upstream();
        let before = upstream_objects(&conn);
        assert_eq!(before, SCHEMA_SQL.to_vec());
        apply(&mut conn).unwrap();
        let once = (journal(&conn), own_objects(&conn));
        apply(&mut conn).unwrap();
        assert_eq!((journal(&conn), own_objects(&conn)), once);
        assert_eq!(once.0, all_ids());
        assert_eq!(upstream_objects(&conn), before);
    }

    /// Before they were merged, two branches each shipped half of this
    /// list under these ids, into this journal. A database that ran one
    /// half, the other, or both in the other order ends up with the same
    /// objects as a fresh one, and nothing is applied twice.
    #[test]
    fn a_database_that_ran_either_branch_s_set_is_completed_not_redone() {
        let fresh = {
            let mut conn = upstream();
            apply(&mut conn).unwrap();
            own_objects(&conn)
        };
        let unattended = ["0001_session_restriction", "0002_outbox_create"];
        let assistant = [
            "trigger_0001_init",
            "memory_0001_init",
            "memory_0002_provenance",
        ];
        for (name, first, second) in [
            ("server-unattended only", &unattended[..], &[][..]),
            ("triggers-memory only", &assistant[..], &[][..]),
            ("both, the other way round", &assistant[..], &unattended[..]),
        ] {
            let mut conn = upstream();
            apply_set(&mut conn, &subset(first)).unwrap();
            apply_set(&mut conn, &subset(second)).unwrap();
            // Rows the earlier build wrote must survive untouched.
            if first.contains(&"0001_session_restriction") {
                conn.execute(
                    "INSERT INTO session_restriction (session_id, time_created) VALUES ('ses_kept', 7)",
                    [],
                )
                .unwrap();
            } else {
                conn.execute(
                    "INSERT INTO trigger_run (id, directory, trigger_id, data, time_started)
                     VALUES ('trr_kept', '/p', 't', '{}', 7)",
                    [],
                )
                .unwrap();
            }
            let stamped: Vec<(String, i64)> = conn
                .prepare("SELECT id, time_completed FROM alforria_migration")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            apply(&mut conn).unwrap();
            apply(&mut conn).unwrap();
            assert_eq!(own_objects(&conn), fresh, "{name}");
            let mut ids = journal(&conn);
            ids.sort();
            let mut expected = all_ids();
            expected.sort();
            assert_eq!(ids, expected, "{name}");
            for (id, time) in stamped {
                let now: i64 = conn
                    .query_row(
                        "SELECT time_completed FROM alforria_migration WHERE id = ?1",
                        [&id],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(now, time, "{name}: {id} was journaled again");
            }
            let kept: i64 = conn
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM session_restriction WHERE session_id = 'ses_kept')
                          + (SELECT COUNT(*) FROM trigger_run WHERE id = 'trr_kept')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(kept, 1, "{name}");
        }
    }

    #[test]
    fn own_tables_lists_exactly_what_the_migrations_create() {
        let mut conn = upstream();
        let upstream_tables = column(&conn, "SELECT name FROM sqlite_master WHERE type = 'table'");
        apply(&mut conn).unwrap();
        let mut tables: Vec<String> =
            column(&conn, "SELECT name FROM sqlite_master WHERE type = 'table'")
                .into_iter()
                .filter(|name| !upstream_tables.contains(name))
                .collect();
        tables.sort();
        let mut own: Vec<String> = OWN_TABLES.iter().map(|name| name.to_string()).collect();
        own.sort();
        assert_eq!(tables, own);
        // And nothing of ours shares a name with something of upstream's.
        for name in OWN_TABLES {
            assert!(
                !upstream_tables.iter().any(|table| table == name),
                "{name} is an upstream table"
            );
        }
    }

    #[test]
    fn a_failing_migration_leaves_nothing_and_is_not_journaled() {
        let mut conn = Connection::open_in_memory().unwrap();
        let set = [("bad_0001", "CREATE TABLE c (id TEXT PRIMARY KEY); NOT SQL;")];
        assert!(apply_set(&mut conn, &set).is_err());
        assert_eq!(
            column(&conn, "SELECT name FROM sqlite_master WHERE type = 'table'"),
            [JOURNAL]
        );
        assert!(journal(&conn).is_empty());
    }

    /// Several processes share one database file; opening it from many at
    /// once must not make any of them fail.
    #[test]
    fn concurrent_opens_do_not_race() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        {
            let mut conn = Connection::open(&path).unwrap();
            crate::storage::migration::apply(&mut conn).unwrap();
        }
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut conn = Connection::open(&path).unwrap();
                    conn.busy_timeout(std::time::Duration::from_secs(10))
                        .unwrap();
                    apply(&mut conn)
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        let conn = Connection::open(&path).unwrap();
        assert_eq!(journal(&conn), all_ids());
    }

    /// The marks that lived in client-writable session metadata move to
    /// `session_mark`, with every descendant; anything that is not plainly
    /// "no" counts, and the metadata key goes.
    #[test]
    fn metadata_marks_move_to_the_servers_table_with_their_descendants() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::storage::migration::apply(&mut conn).unwrap();
        let before: Vec<_> = MIGRATIONS
            .iter()
            .copied()
            .take_while(|(id, _)| *id != "0006_session_mark")
            .collect();
        apply_set(&mut conn, &before).unwrap();
        conn.execute_batch(
            "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated) VALUES ('proj', '/p', '[]', 1, 1);",
        )
        .unwrap();
        let insert = |id: &str, parent: Option<&str>, metadata: Option<&str>| {
            conn.execute(
                "INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, metadata, time_created, time_updated)
                 VALUES (?1, 'proj', ?2, ?1, '/p', 't', '1', ?3, 1, 1)",
                rusqlite::params![id, parent, metadata],
            )
            .unwrap();
        };
        insert(
            "ses_true",
            None,
            Some(r#"{"untrustedInput":true,"note":1}"#),
        );
        insert("ses_child", Some("ses_true"), None);
        insert("ses_grand", Some("ses_child"), Some(r#"{"k":"v"}"#));
        // What a client could store: Rust read these as unmarked, SQL as
        // marked or as an error. They count.
        insert("ses_one", None, Some(r#"{"untrustedInput":1}"#));
        insert("ses_yes", None, Some(r#"{"untrustedInput":"yes"}"#));
        insert("ses_obj", None, Some(r#"{"untrustedInput":{"x":1}}"#));
        // Plainly no.
        insert("ses_false", None, Some(r#"{"untrustedInput":false}"#));
        insert("ses_zero", None, Some(r#"{"untrustedInput":0}"#));
        insert("ses_null", None, Some(r#"{"untrustedInput":null}"#));
        insert("ses_plain", None, None);
        insert("ses_bad", None, Some("not json"));
        apply(&mut conn).unwrap();
        let marked = |id: &str| -> (bool, bool) {
            use crate::session::store::SessionMark;
            conn.query_row(
                &format!(
                    "SELECT {}, {}",
                    SessionMark::OutsideContent.sql("?1"),
                    SessionMark::OutsideStart.sql("?1")
                ),
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        };
        for id in [
            "ses_true",
            "ses_child",
            "ses_grand",
            "ses_one",
            "ses_yes",
            "ses_obj",
        ] {
            assert_eq!(marked(id), (true, true), "{id}");
        }
        for id in ["ses_false", "ses_zero", "ses_null", "ses_plain", "ses_bad"] {
            assert_eq!(marked(id), (false, false), "{id}");
        }
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session WHERE json_valid(metadata) AND json_type(metadata, '$.untrustedInput') IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "the metadata key is gone");
        let kept: String = conn
            .query_row(
                "SELECT metadata FROM session WHERE id = 'ses_true'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept, r#"{"note":1}"#);
    }

    /// The order is the list's, and the list only grows at its end: the
    /// first five ids are frozen as the two branches wrote them, and every
    /// later one is numbered above them.
    #[test]
    fn ids_are_unique_and_the_shipped_prefix_is_frozen() {
        let ids: Vec<&str> = MIGRATIONS.iter().map(|(id, _)| *id).collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "duplicate migration id");
        assert_eq!(
            ids[..5],
            [
                "0001_session_restriction",
                "0002_outbox_create",
                "trigger_0001_init",
                "memory_0001_init",
                "memory_0002_provenance",
            ]
        );
        let mut last = 5u32;
        for id in &ids[5..] {
            let number: u32 = id
                .split('_')
                .next()
                .and_then(|number| number.parse().ok())
                .unwrap_or_else(|| panic!("{id}: later migrations are <number>_<area>_<what>"));
            assert!(number > last, "{id} is out of order");
            last = number;
        }
    }
}
