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
//! a database that has already run it must find the same id again. A test
//! pins the hash of every body (`shipped_migration_bodies_are_pinned`); to
//! change what a shipped migration did, add a new one that repairs it, as
//! `0008_session_mark_backfill` does for `memory_0002_provenance`, whose
//! body was once edited in place. The bodies an id used to have are kept
//! under `alforria_migrations/test_data/` and only tests read them.
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
    (
        "0008_session_mark_backfill",
        include_str!("alforria_migrations/0008_session_mark_backfill.sql"),
    ),
    (
        "0009_memory_directory",
        include_str!("alforria_migrations/0009_memory_directory.sql"),
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

    const BACKFILL: &str = "0008_session_mark_backfill";

    /// The body `memory_0002_provenance` had at commit 8068a0d, before it
    /// was edited in place under the same id. Test data: a database that
    /// ran it must end where one that ran the current body does.
    const MEMORY_0002_AT_8068A0D: &str =
        include_str!("alforria_migrations/test_data/memory_0002_provenance.historical_8068a0d.sql");

    /// Every migration before `id`: the database a build without it left.
    fn until(id: &str) -> Vec<(&'static str, &'static str)> {
        assert!(MIGRATIONS.iter().any(|(known, _)| *known == id), "{id}");
        MIGRATIONS
            .iter()
            .copied()
            .take_while(|(known, _)| *known != id)
            .collect()
    }

    fn project(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated) VALUES ('proj', '/p', '[]', 1, 1);",
        )
        .unwrap();
    }

    fn session(conn: &Connection, id: &str, parent: Option<&str>, metadata: Option<&str>) {
        conn.execute(
            "INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, metadata, time_created, time_updated)
             VALUES (?1, 'proj', ?2, ?1, '/p', 't', '1', ?3, 1, 1)",
            rusqlite::params![id, parent, metadata],
        )
        .unwrap();
    }

    /// `(outside_content, outside_start)`, by the one definition.
    fn marks(conn: &Connection, id: &str) -> (bool, bool) {
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
    }

    /// A session's `outside-input` ceiling, as stored.
    fn outside_ceiling_of(conn: &Connection, id: &str) -> Option<String> {
        use rusqlite::OptionalExtension;
        conn.query_row(
            "SELECT readings FROM session_ceiling WHERE session_id = ?1 AND name = ?2",
            [id, crate::trigger::OUTSIDE_CEILING],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
    }

    /// The read-only ceiling of a trigger with no block of its own, as the
    /// live code stores it.
    fn read_only_ceiling() -> String {
        serde_json::to_string(&vec![crate::trigger::outside_ceiling(&Vec::new())]).unwrap()
    }

    /// `(trust, provenance.untrusted)` of a memory record.
    fn trust(conn: &Connection, id: &str) -> (String, Option<bool>) {
        conn.query_row(
            "SELECT trust, CASE WHEN json_valid(provenance) THEN json_extract(provenance, '$.untrusted') END
             FROM memory WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    }

    fn verified() -> (String, Option<bool>) {
        ("verified".to_string(), Some(false))
    }

    fn demoted() -> (String, Option<bool>) {
        ("unverified".to_string(), Some(true))
    }

    /// A shipped migration is never edited: a database that already ran it
    /// keeps the old result for ever, under the same journal id. That
    /// happened once (`memory_0002_provenance`, repaired by 0008). The
    /// hashes are of the bodies with `\n` line endings.
    #[test]
    fn shipped_migration_bodies_are_pinned() {
        use sha2::{Digest, Sha256};
        const PINNED: &[(&str, &str)] = &[
            (
                "0001_session_restriction",
                "2e81edc344effae1b7af87042a69d46ec41180868dba0b2df0564a07045ce89d",
            ),
            (
                "0002_outbox_create",
                "4de716388aded6cae627de60e8540a84ec64e74237a26de39a81e968e8f56198",
            ),
            (
                "trigger_0001_init",
                "f33c6b4ae267f8d0b82196ee3b059f2bdb8087bf581e406da5cdc367d2d5f26b",
            ),
            (
                "memory_0001_init",
                "4f46423c619f5cdec3cec2761b545c91269b2c2c35ec5e3ec343b3f4914debb5",
            ),
            (
                "memory_0002_provenance",
                "b3e7e8f5e17d76c854b2c5ba5ce1fa6290cdb6b62e76c44435c702cf19def23f",
            ),
            (
                "0006_session_mark",
                "4e68e2abceac34b8ba942e1c25121b9d5e926848c117177944046514802dc3e5",
            ),
            (
                "0007_session_ceiling",
                "bf29a6e49e5db952c1278d34b24be67b67d8d7825f1707d83a752195773d277a",
            ),
            (
                "0008_session_mark_backfill",
                "08507915238e88562725d53d034ba76d2913013e7754944e91db15b53cb246b7",
            ),
            (
                "0009_memory_directory",
                "f5847853d1bbd28fc38d587880aa0854edd1bd299097137a77a364b5b2ab80e9",
            ),
        ];
        for (id, sql) in MIGRATIONS {
            let hash = format!("{:x}", Sha256::digest(sql.replace("\r\n", "\n")));
            match PINNED.iter().find(|(pinned, _)| pinned == id) {
                Some((_, pinned)) => assert_eq!(
                    &hash, pinned,
                    "the body of migration {id} changed. A migration that has shipped is never \
                     edited: databases that already ran it would keep the old result under the \
                     same id. Restore the file and add a NEW migration (the next free number, \
                     appended to MIGRATIONS) that makes the change, with its hash here."
                ),
                None => panic!(
                    "migration {id} has no pinned hash: add (\"{id}\", \"{hash}\") to PINNED \
                     once its body is final. It must not change after that."
                ),
            }
        }
        assert_eq!(
            PINNED.len(),
            MIGRATIONS.len(),
            "a pinned migration is gone from MIGRATIONS: shipped migrations are never removed"
        );
    }

    /// M7. Records of the `global` project get the directory of the
    /// session their origin names; one that names no session, a session
    /// that is gone, or has no readable origin gets none — and then no
    /// directory is shown it, only the operator's list is. A project's
    /// records are left alone. The upgraded database has the objects of a
    /// fresh one.
    #[test]
    fn global_records_are_attributed_through_their_session_and_the_rest_to_no_directory() {
        const DIRECTORY: &str = "0009_memory_directory";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let fresh = {
            let mut conn = upstream();
            apply(&mut conn).unwrap();
            own_objects(&conn)
        };
        {
            let mut conn = Connection::open(&path).unwrap();
            crate::storage::migration::apply(&mut conn).unwrap();
            apply_set(&mut conn, &until(DIRECTORY)).unwrap();
            conn.execute_batch(
                "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated) VALUES
                    ('proj', '/repo', '[]', 1, 1), ('global', '/', '[]', 1, 1);",
            )
            .unwrap();
            for (id, project, directory) in [
                ("ses_a", "global", "/srv/a"),
                ("ses_b", "global", "/srv/b"),
                ("ses_repo", "proj", "/repo"),
            ] {
                conn.execute(
                    "INSERT INTO session (id, project_id, slug, directory, title, version, time_created, time_updated)
                     VALUES (?1, ?2, ?1, ?3, 't', '1', 1, 1)",
                    [id, project, directory],
                )
                .unwrap();
            }
            for (id, project, provenance) in [
                (
                    "mem_tool_a",
                    "global",
                    Some(r#"{"source":"tool","sessionID":"ses_a","untrusted":false}"#),
                ),
                (
                    "mem_flush_b",
                    "global",
                    Some(r#"{"source":"flush","sessionID":"ses_b","untrusted":false}"#),
                ),
                // `scope: "global"`, written from a repository.
                (
                    "mem_shared",
                    "global",
                    Some(r#"{"source":"tool","sessionID":"ses_repo","untrusted":false}"#),
                ),
                (
                    "mem_api",
                    "global",
                    Some(r#"{"source":"api","untrusted":false}"#),
                ),
                (
                    "mem_client",
                    "global",
                    Some(r#"{"source":"client","untrusted":false,"restricted":true}"#),
                ),
                (
                    "mem_gone",
                    "global",
                    Some(r#"{"source":"tool","sessionID":"ses_deleted","untrusted":true}"#),
                ),
                (
                    "mem_number",
                    "global",
                    Some(r#"{"source":"tool","sessionID":7,"untrusted":true}"#),
                ),
                ("mem_junk", "global", Some("not json")),
                ("mem_none", "global", None),
                (
                    "mem_proj",
                    "proj",
                    Some(r#"{"source":"tool","sessionID":"ses_repo","untrusted":false}"#),
                ),
            ] {
                conn.execute(
                    "INSERT INTO memory (id, project_id, kind, content, trust, provenance, time_created, time_updated)
                     VALUES (?1, ?2, 'note', ?1, 'verified', ?3, 1, 1)",
                    rusqlite::params![id, project, provenance],
                )
                .unwrap();
                conn.execute("INSERT INTO memory_fts (content, id) VALUES (?1, ?1)", [id])
                    .unwrap();
            }
        }
        // This build opens it, twice.
        let storage = std::sync::Arc::new(crate::storage::Storage::open(&path).unwrap());
        storage.with_connection_mut(apply).unwrap();
        let directories: Vec<String> = storage.with_connection(|conn| {
            assert_eq!(own_objects(conn), fresh);
            assert_eq!(journal(conn), all_ids());
            column(
                conn,
                "SELECT id || '=' || COALESCE(directory, '-') FROM memory ORDER BY id",
            )
        });
        assert_eq!(
            directories,
            [
                "mem_api=-",
                "mem_client=-",
                "mem_flush_b=/srv/b",
                "mem_gone=-",
                "mem_junk=-",
                "mem_none=-",
                "mem_number=-",
                "mem_proj=-",
                "mem_shared=/repo",
                "mem_tool_a=/srv/a",
            ]
        );

        // What each caller is shown afterwards.
        use crate::memory::test_support::{service, settings_in};
        use crate::memory::ListQuery;
        let ids = |records: Vec<crate::memory::MemoryRecord>| -> Vec<String> {
            let mut ids: Vec<String> = records.into_iter().map(|record| record.id).collect();
            ids.sort();
            ids
        };
        let from = |project: &str, directory: &str| {
            service(
                storage.clone(),
                settings_in(project, std::path::Path::new(directory)),
            )
        };
        let unattributed = [
            "mem_api",
            "mem_client",
            "mem_gone",
            "mem_junk",
            "mem_none",
            "mem_number",
        ];
        for (project, directory, own) in [
            // The directory a record was written in still has it.
            ("global", "/srv/a", vec!["mem_tool_a"]),
            ("global", "/srv/b", vec!["mem_flush_b"]),
            ("global", "/srv/new", vec![]),
            // A project's records are the project's, from any directory.
            ("proj", "/repo", vec!["mem_proj"]),
            ("proj", "/repo/sub", vec!["mem_proj"]),
        ] {
            let memory = from(project, directory);
            assert_eq!(
                ids(memory.list(&ListQuery::default()).unwrap()),
                own,
                "{directory}"
            );
            for id in unattributed {
                assert_eq!(memory.get(id).unwrap(), None, "{directory}: {id}");
                assert!(
                    memory.search(id, None, true, 10).unwrap().is_empty(),
                    "{directory}: {id}"
                );
                for restricted in [false, true] {
                    assert!(
                        !memory
                            .injection_for(restricted)
                            .unwrap_or_default()
                            .contains(id),
                        "{directory}: {id}"
                    );
                }
            }
            assert_eq!(
                ids(memory.list_unattributed(None, 0).unwrap()),
                unattributed,
                "{directory}"
            );
        }
        // `scope: "global"` reads every `global` record, as it did.
        let mut everywhere = settings_in("global", std::path::Path::new("/repo"));
        everywhere.global = true;
        assert_eq!(
            service(storage.clone(), everywhere)
                .list(&ListQuery::default())
                .unwrap()
                .len(),
            9
        );
    }

    /// M4. `memory_0002_provenance` had two bodies under one id. A database
    /// that ran either ends with the schema of a fresh one, the descendants
    /// of a webhook run marked, and the record one of them wrote at depth 3
    /// unverified. (The first body looked at a session and its direct
    /// parent only, and promoted that record.)
    #[test]
    fn a_database_that_ran_either_body_of_memory_0002_ends_like_a_fresh_one() {
        let current = subset(&["memory_0002_provenance"])[0].1;
        assert_ne!(MEMORY_0002_AT_8068A0D, current);
        let fresh = {
            let mut conn = upstream();
            apply(&mut conn).unwrap();
            (own_objects(&conn), upstream_objects(&conn))
        };
        for (name, body) in [
            ("the body at 8068a0d", MEMORY_0002_AT_8068A0D),
            ("the current body", current),
        ] {
            // The assistant branch's set, as the build of that time had it.
            let mut conn = upstream();
            apply_set(
                &mut conn,
                &subset(&["trigger_0001_init", "memory_0001_init"]),
            )
            .unwrap();
            project(&conn);
            session(&conn, "ses_ok", None, None);
            session(&conn, "ses_hook", None, Some(r#"{"untrustedInput":true}"#));
            session(&conn, "ses_child", Some("ses_hook"), None);
            session(&conn, "ses_grand", Some("ses_child"), None);
            for (id, session_id) in [("mem_grand", "ses_grand"), ("mem_ok", "ses_ok")] {
                conn.execute(
                    "INSERT INTO memory (id, project_id, kind, content, metadata, time_created, time_updated)
                     VALUES (?1, 'proj', 'note', 'planted by the webhook body', json_object('sessionID', ?2), 1, 1)",
                    [id, session_id],
                )
                .unwrap();
            }
            apply_set(&mut conn, &[("memory_0002_provenance", body)]).unwrap();
            // This build opens it.
            apply(&mut conn).unwrap();
            apply(&mut conn).unwrap();
            assert_eq!(own_objects(&conn), fresh.0, "{name}");
            assert_eq!(upstream_objects(&conn), fresh.1, "{name}");
            let mut ids = journal(&conn);
            ids.sort();
            let mut expected = all_ids();
            expected.sort();
            assert_eq!(ids, expected, "{name}");
            assert_eq!(trust(&conn, "mem_grand"), demoted(), "{name}");
            assert_eq!(trust(&conn, "mem_ok"), verified(), "{name}");
            for id in ["ses_hook", "ses_child", "ses_grand"] {
                assert_eq!(marks(&conn, id), (true, true), "{name}: {id}");
                assert_eq!(
                    outside_ceiling_of(&conn, id),
                    Some(read_only_ceiling()),
                    "{name}: {id}"
                );
            }
            assert_eq!(marks(&conn, "ses_ok"), (false, false), "{name}");
            assert_eq!(outside_ceiling_of(&conn, "ses_ok"), None, "{name}");
        }
    }

    /// M5, step by step. A session with `metadata.triggerID` gets both
    /// marks and the read-only ceiling unless its trigger is stored, parses
    /// and is plainly started by the clock; its descendants follow it.
    #[test]
    fn a_trigger_s_session_is_marked_unless_its_trigger_is_plainly_started_by_the_clock() {
        let mut conn = upstream();
        apply_set(&mut conn, &until(BACKFILL)).unwrap();
        project(&conn);
        let clock = [
            (
                "cron",
                r#"{"on":{"cron":"0 9 * * *","timezone":"Europe/Paris"},"prompt":"p"}"#,
            ),
            (
                "interval",
                r#"{"on":{"interval":"1h"},"prompt":"p","session":"reuse","inheritedPermission":[]}"#,
            ),
            ("at", r#"{"on":{"at":"2027-01-01T00:00:00Z"},"prompt":"p"}"#),
            (
                "cron_explicit",
                r#"{"on":{"cron":"* * * * *","webhook":false,"watch":null},"prompt":"p"}"#,
            ),
        ];
        let outside = [
            ("webhook", r#"{"on":{"webhook":true},"prompt":"p"}"#),
            ("watch", r#"{"on":{"watch":{"path":"inbox"}},"prompt":"p"}"#),
            // Not something a build would store; nothing here is taken on
            // trust.
            (
                "cron_and_webhook",
                r#"{"on":{"cron":"* * * * *","webhook":true},"prompt":"p"}"#,
            ),
            (
                "cron_and_watch",
                r#"{"on":{"cron":"* * * * *","watch":{"path":"x"}},"prompt":"p"}"#,
            ),
            (
                "webhook_as_number",
                r#"{"on":{"cron":"* * * * *","webhook":1},"prompt":"p"}"#,
            ),
            (
                "unknown_kind",
                r#"{"on":{"cron":"* * * * *","mail":true},"prompt":"p"}"#,
            ),
            ("no_kind", r#"{"on":{},"prompt":"p"}"#),
            ("no_on", r#"{"prompt":"p"}"#),
            ("on_is_text", r#"{"on":"cron","prompt":"p"}"#),
            ("cron_is_not_text", r#"{"on":{"cron":true},"prompt":"p"}"#),
            ("not_json", "not json"),
            ("cut_short", r#"{"on":{"cron":"* * * * *""#),
        ];
        let store = |directory: &str, id: &str, spec: &str| {
            conn.execute(
                "INSERT INTO \"trigger\" (directory, id, project_id, spec, time_created, time_updated)
                 VALUES (?1, ?2, 'proj', ?3, 1, 1)",
                [directory, id, spec],
            )
            .unwrap();
        };
        for (id, spec) in clock.iter().chain(&outside) {
            store("/p", id, spec);
            session(
                &conn,
                &format!("ses_{id}"),
                None,
                Some(&serde_json::json!({ "triggerID": id, "note": 1 }).to_string()),
            );
        }
        // A clock trigger of that name, but of another directory; and one
        // whose id only matches a number loosely.
        store("/elsewhere", "elsewhere", clock[0].1);
        session(
            &conn,
            "ses_elsewhere",
            None,
            Some(r#"{"triggerID":"elsewhere"}"#),
        );
        store("/p", "7", clock[0].1);
        session(&conn, "ses_number", None, Some(r#"{"triggerID":7}"#));
        // No row: deleted since, or declared in the config only.
        session(&conn, "ses_gone", None, Some(r#"{"triggerID":"gone"}"#));
        session(&conn, "ses_null", None, Some(r#"{"triggerID":null}"#));
        // Below a marked one, and below one that is not.
        session(&conn, "ses_webhook_child", Some("ses_webhook"), None);
        session(
            &conn,
            "ses_webhook_grand",
            Some("ses_webhook_child"),
            Some(r#"{"k":"v"}"#),
        );
        session(&conn, "ses_gone_child", Some("ses_gone"), None);
        session(&conn, "ses_cron_child", Some("ses_cron"), None);
        // No trigger started these.
        session(&conn, "ses_plain", None, None);
        session(&conn, "ses_other", None, Some(r#"{"note":"triggerID"}"#));
        session(&conn, "ses_bad", None, Some("not json"));

        apply(&mut conn).unwrap();

        let mut unmarked: Vec<String> = clock.iter().map(|(id, _)| format!("ses_{id}")).collect();
        unmarked.extend(["ses_cron_child", "ses_plain", "ses_other", "ses_bad"].map(String::from));
        for id in &unmarked {
            assert_eq!(marks(&conn, id), (false, false), "{id}");
            assert_eq!(outside_ceiling_of(&conn, id), None, "{id}");
        }
        let mut marked: Vec<String> = outside.iter().map(|(id, _)| format!("ses_{id}")).collect();
        marked.extend(
            [
                "ses_elsewhere",
                "ses_number",
                "ses_gone",
                "ses_null",
                "ses_webhook_child",
                "ses_webhook_grand",
                "ses_gone_child",
            ]
            .map(String::from),
        );
        for id in &marked {
            assert_eq!(marks(&conn, id), (true, true), "{id}");
            assert_eq!(
                outside_ceiling_of(&conn, id),
                Some(read_only_ceiling()),
                "{id}"
            );
        }
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM session", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            sessions as usize,
            marked.len() + unmarked.len(),
            "every case is checked"
        );
        // The metadata is the session's still.
        let kept: String = conn
            .query_row(
                "SELECT metadata FROM session WHERE id = 'ses_webhook'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept, r#"{"note":1,"triggerID":"webhook"}"#);
    }

    /// The backfill on a database with nothing in it writes nothing, and a
    /// second run over one it has repaired changes nothing: marks and
    /// ceilings that exist are kept as they are, records are demoted once.
    #[test]
    fn the_backfill_is_a_no_op_on_an_empty_database_and_the_second_time() {
        let body = subset(&[BACKFILL])[0].1;
        // The marks it reads and writes are the ones the lookups read.
        for mark in [
            crate::session::store::SessionMark::OutsideStart,
            crate::session::store::SessionMark::OutsideContent,
        ] {
            assert!(body.contains(&format!("'{}'", mark.as_str())), "{mark:?}");
        }
        assert!(body.contains(
            &crate::session::store::SessionMark::OutsideContent
                .sql("json_extract(memory.provenance, '$.sessionID')")
        ));
        assert!(body.contains(&format!("'{}'", read_only_ceiling())));
        assert!(body.contains(&format!("'{}'", crate::trigger::OUTSIDE_CEILING)));

        let mut conn = upstream();
        apply(&mut conn).unwrap();
        let rows = |conn: &Connection| -> Vec<String> {
            let mut out = column(
                conn,
                "SELECT 'mark|' || session_id || '|' || mark || '|' || time_created FROM session_mark",
            );
            out.extend(column(
                conn,
                "SELECT 'ceiling|' || session_id || '|' || name || '|' || readings || '|' || time_updated FROM session_ceiling",
            ));
            out.extend(column(
                conn,
                "SELECT 'memory|' || id || '|' || trust || '|' || COALESCE(provenance, '') || '|' || time_updated FROM memory",
            ));
            out.extend(column(
                conn,
                "SELECT 'session|' || id || '|' || COALESCE(metadata, '') FROM session",
            ));
            out.sort();
            out
        };
        assert!(rows(&conn).is_empty(), "nothing to repair, nothing written");

        project(&conn);
        // As 0006 left a session it took the mark from the metadata of:
        // marked, no ceiling. Its child predates inheritance.
        session(&conn, "ses_carried", None, None);
        session(&conn, "ses_carried_child", Some("ses_carried"), None);
        // As this build makes a webhook run's session: marked, with the
        // ceiling its trigger's block gives it.
        session(&conn, "ses_run", None, Some(r#"{"triggerID":"hook"}"#));
        // Provenance only: it read a web page. No ceiling, and its child
        // (made before it did) is not its business.
        session(&conn, "ses_read", None, None);
        session(&conn, "ses_read_child", Some("ses_read"), None);
        session(&conn, "ses_clean", None, None);
        let own = r#"[[{"permission":"*","pattern":"*","action":"deny"},{"permission":"bash","pattern":"git status","action":"allow"}]]"#;
        conn.execute_batch(&format!(
            "INSERT INTO session_mark (session_id, mark, time_created) VALUES
                ('ses_carried', 'outside_start', 5), ('ses_carried', 'outside_content', 5),
                ('ses_run', 'outside_start', 5), ('ses_run', 'outside_content', 5),
                ('ses_read', 'outside_content', 5);
             INSERT INTO session_ceiling (session_id, name, readings, time_updated) VALUES
                ('ses_run', 'outside-input', '{own}', 5),
                ('ses_run', 'trigger-creator', '[[]]', 5);"
        ))
        .unwrap();
        for (id, trust, provenance) in [
            (
                "mem_child",
                "verified",
                Some(r#"{"source":"tool","sessionID":"ses_carried_child","untrusted":false}"#),
            ),
            (
                "mem_read",
                "verified",
                Some(r#"{"source":"tool","sessionID":"ses_read","untrusted":false,"history":[]}"#),
            ),
            (
                "mem_gone",
                "verified",
                Some(r#"{"source":"tool","sessionID":"ses_deleted","untrusted":false}"#),
            ),
            (
                "mem_no_session",
                "verified",
                Some(r#"{"source":"tool","untrusted":false}"#),
            ),
            (
                "mem_flush_gone",
                "unverified",
                Some(r#"{"source":"flush","sessionID":"ses_deleted","untrusted":false}"#),
            ),
            (
                "mem_clean",
                "verified",
                Some(r#"{"source":"tool","sessionID":"ses_clean","untrusted":false}"#),
            ),
            (
                "mem_read_child",
                "verified",
                Some(r#"{"source":"tool","sessionID":"ses_read_child","untrusted":false}"#),
            ),
            (
                "mem_api",
                "verified",
                Some(r#"{"source":"api","untrusted":false}"#),
            ),
            (
                "mem_flush",
                "unverified",
                Some(r#"{"source":"flush","sessionID":"ses_clean","untrusted":false}"#),
            ),
            (
                "mem_already",
                "unverified",
                Some(r#"{"source":"tool","sessionID":"ses_run","untrusted":true}"#),
            ),
            ("mem_junk", "verified", Some("not json")),
            ("mem_none", "unverified", None),
        ] {
            conn.execute(
                "INSERT INTO memory (id, project_id, kind, content, trust, provenance, time_created, time_updated)
                 VALUES (?1, 'proj', 'note', ?1, ?2, ?3, 1, 1)",
                rusqlite::params![id, trust, provenance],
            )
            .unwrap();
        }
        let before = rows(&conn);

        conn.execute_batch(body).unwrap();
        let once = rows(&conn);
        assert_ne!(once, before);
        conn.execute_batch(body).unwrap();
        assert_eq!(rows(&conn), once, "the second run changed something");

        assert_eq!(marks(&conn, "ses_carried_child"), (true, true));
        for id in ["ses_carried", "ses_carried_child"] {
            assert_eq!(
                outside_ceiling_of(&conn, id),
                Some(read_only_ceiling()),
                "{id}"
            );
        }
        // What was there is as it was: the run's own ceiling, the time of
        // its marks, the provenance mark that is no ceiling.
        assert_eq!(outside_ceiling_of(&conn, "ses_run").as_deref(), Some(own));
        for kept in [
            "mark|ses_run|outside_start|5",
            "mark|ses_carried|outside_content|5",
            "ceiling|ses_run|trigger-creator|[[]]|5",
        ] {
            assert!(once.iter().any(|row| row == kept), "{kept}");
        }
        assert_eq!(marks(&conn, "ses_read"), (true, false));
        assert_eq!(marks(&conn, "ses_read_child"), (false, false));
        assert_eq!(marks(&conn, "ses_clean"), (false, false));
        for id in ["ses_read", "ses_read_child", "ses_clean"] {
            assert_eq!(outside_ceiling_of(&conn, id), None, "{id}");
        }
        for id in [
            "mem_child",
            "mem_read",
            "mem_gone",
            "mem_no_session",
            "mem_flush_gone",
        ] {
            assert_eq!(trust(&conn, id), demoted(), "{id}");
        }
        for id in ["mem_clean", "mem_read_child", "mem_api"] {
            assert_eq!(trust(&conn, id), verified(), "{id}");
        }
        // The rest of a demoted origin is kept.
        assert!(once.iter().any(|row| row
            == r#"memory|mem_read|unverified|{"source":"tool","sessionID":"ses_read","untrusted":true,"history":[]}|1"#));
        // Not touched: nothing to learn, or nothing to read.
        for row in &before {
            if [
                "mem_flush|",
                "mem_already|",
                "mem_junk|",
                "mem_none|",
                "mem_api|",
                "mem_clean|",
            ]
            .iter()
            .any(|id| row.starts_with(&format!("memory|{id}")))
            {
                assert!(once.contains(row), "{row}");
            }
        }
    }
}
