//! Migration journal: port of `packages/core/src/database/migration.ts`.
//!
//! The TS reference runs 40 (in fact 38, see [`MIGRATION_IDS`]) incremental
//! drizzle migrations whose bodies only matter for upgrading databases older
//! than the pinned commit. The Rust port does not carry the bodies: fresh
//! databases get the full schema (spec §7) and every migration ID seeded into
//! the journal; existing databases are only brought up to date at the journal
//! level (`apply_only`). This keeps the journal byte-compatible so a TS
//! opencode and a Rust opencode agree on what has been applied.

use std::collections::HashSet;

use rusqlite::Connection;

use crate::storage::schema::SCHEMA_SQL;
use crate::CoreError;

/// Every migration ID, in order (`migration.gen.ts` / spec §7.1).
///
/// Note: the spec prose says "all 40" but both the spec's own §7.1 list and
/// the pinned `migration.gen.ts` contain 38 entries — the cited file wins
/// (spec §9 S1).
pub const MIGRATION_IDS: &[&str] = &[
    "20260127222353_familiar_lady_ursula",
    "20260211171708_add_project_commands",
    "20260213144116_wakeful_the_professor",
    "20260225215848_workspace",
    "20260227213759_add_session_workspace_id",
    "20260228203230_blue_harpoon",
    "20260303231226_add_workspace_fields",
    "20260309230000_move_org_to_state",
    "20260312043431_session_message_cursor",
    "20260323234822_events",
    "20260410174513_workspace-name",
    "20260413175956_chief_energizer",
    "20260423070820_add_icon_url_override",
    "20260427172553_slow_nightmare",
    "20260428004200_add_session_path",
    "20260501142318_next_venus",
    "20260504145000_add_sync_owner",
    "20260507164347_add_workspace_time",
    "20260510033149_session_usage",
    "20260511000411_data_migration_state",
    "20260511173437_session-metadata",
    "20260601010001_normalize_storage_paths",
    "20260601202201_amazing_prowler",
    "20260602002951_lowly_union_jack",
    "20260602182828_add_project_directories",
    "20260603001617_session_message_projection_indexes",
    "20260603040000_session_message_projection_order",
    "20260603141458_session_input_inbox",
    "20260603160727_jittery_ezekiel_stane",
    "20260604172448_event_sourced_session_input",
    "20260605003541_add_session_context_snapshot",
    "20260605042240_add_context_epoch_agent",
    "20260611035744_credential",
    "20260611192811_lush_chimera",
    "20260612174303_project_dir_strategy",
    "20260622142730_simplify_session_context_epoch",
    "20260622170816_reset_v2_session_state",
    "20260622202450_simplify_session_input",
];

/// `Date.now()` — unix milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn list_tables(conn: &Connection) -> Result<Vec<String>, CoreError> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let tables = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(tables)
}

fn insert_migration(tx: &Connection, id: &str) -> Result<(), CoreError> {
    tx.execute(
        "INSERT INTO migration (id, time_completed) VALUES (?1, ?2)",
        rusqlite::params![id, now_ms()],
    )?;
    Ok(())
}

fn select_completed(conn: &Connection) -> Result<HashSet<String>, CoreError> {
    let mut stmt = conn.prepare("SELECT id FROM migration")?;
    let completed = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<HashSet<_>, _>>()?;
    Ok(completed)
}

/// Create the migration journal and apply the compiled-in migrations
/// (port of `migration.ts:18-41`).
///
/// A fresh (empty) database gets the full §7 schema plus a journal seeded
/// with every migration ID, all in one transaction — exactly what TS does on
/// a fresh install. A database that already has a `session` table is handed
/// to [`apply_only`]; any other non-empty database is fatal.
///
/// Concurrency: TS guards this with a semaphore; in the Rust port the
/// `Mutex` inside [`crate::storage::connection::Storage`] serializes access
/// for a single process (spec: cross-connection apply is out of scope).
pub fn apply(conn: &mut Connection) -> Result<(), CoreError> {
    let tables = list_tables(conn)?;
    if tables.iter().any(|table| table == "session") {
        return apply_only(conn, MIGRATION_IDS);
    }
    if !tables.is_empty() {
        return Err(CoreError::Storage(
            "Database is not empty and has no session table".into(),
        ));
    }

    let tx = conn.transaction()?;
    for statement in SCHEMA_SQL {
        tx.execute_batch(statement)?;
    }
    tx.execute(
        "CREATE TABLE migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)",
        [],
    )?;
    for id in MIGRATION_IDS {
        insert_migration(&tx, id)?;
    }
    tx.commit()?;
    Ok(())
}

/// Bring the journal up to date without running any migration bodies
/// (port of `migration.ts:43-107`).
///
/// Creates the journal if missing, seeds it from a legacy drizzle
/// `__drizzle_migrations` table when the journal is empty (named-column
/// form, or the timestamp-prefix fallback), then records every unapplied
/// migration ID from `input`.
pub fn apply_only(conn: &Connection, input: &[&str]) -> Result<(), CoreError> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)",
        [],
    )?;

    let mut completed = select_completed(conn)?;
    if completed.is_empty() && has_drizzle_journal(conn)? {
        let named = has_named_drizzle_journal(conn)?;
        if named {
            // Journal with a `name` column: seed straight from it.
            conn.execute(
                "INSERT OR IGNORE INTO migration (id, time_completed)
                 SELECT name, ?1 FROM __drizzle_migrations WHERE name IS NOT NULL",
                rusqlite::params![now_ms()],
            )?;
        } else {
            // Pre-name drizzle journals only have `created_at`; match the
            // timestamp back to the migration whose ID starts with it.
            let mut stmt = conn.prepare(
                "SELECT created_at, strftime('%Y%m%d%H%M%S', created_at / 1000, 'unixepoch') AS prefix
                 FROM __drizzle_migrations WHERE created_at IS NOT NULL",
            )?;
            let entries = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;

            for (created_at, prefix) in entries {
                let migration = input
                    .iter()
                    .find(|id| id.starts_with(&format!("{prefix}_")))
                    .ok_or_else(|| {
                        CoreError::Storage(format!(
                            "Legacy migration timestamp {created_at} does not match any known migration"
                        ))
                    })?;
                conn.execute(
                    "INSERT OR IGNORE INTO migration (id, time_completed) VALUES (?1, ?2)",
                    rusqlite::params![migration, now_ms()],
                )?;
            }
        }
        completed = select_completed(conn)?;
    }

    for id in input {
        if completed.contains(*id) {
            continue;
        }
        // The TS reference runs `migration.up` here; the Rust port does not
        // carry the incremental bodies (see the module docs), so it only
        // records the ID.
        insert_migration(conn, id)?;
    }
    Ok(())
}

fn has_drizzle_journal(conn: &Connection) -> Result<bool, CoreError> {
    let found = conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '__drizzle_migrations'",
        [],
        |_| Ok(()),
    );
    match found {
        Ok(()) => Ok(true),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn has_named_drizzle_journal(conn: &Connection) -> Result<bool, CoreError> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('__drizzle_migrations')")?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names.iter().any(|name| name == "name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::storage::test_support::TempDir;

    fn conn_in(dir: &TempDir) -> Connection {
        Connection::open(dir.path().join("db.sqlite")).unwrap()
    }

    fn journal(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT id FROM migration ORDER BY id")
            .unwrap();
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        ids
    }

    #[test]
    fn fresh_apply_creates_schema_and_seeds_journal() {
        let dir = TempDir::new("fresh");
        let mut conn = conn_in(&dir);
        apply(&mut conn).unwrap();

        // Every migration ID is journaled, and apply is idempotent.
        let ids = journal(&conn);
        assert_eq!(ids.len(), MIGRATION_IDS.len());
        assert!(ids.contains(&"20260127222353_familiar_lady_ursula".to_string()));
        assert!(ids.contains(&"20260622202450_simplify_session_input".to_string()));
        apply(&mut conn).unwrap();
        assert_eq!(journal(&conn).len(), MIGRATION_IDS.len());

        // No ID was lost or duplicated relative to the compiled-in list.
        let mut sorted = MIGRATION_IDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn apply_only_on_existing_session_db() {
        let dir = TempDir::new("existing");
        let mut conn = conn_in(&dir);
        conn.execute_batch(
            "CREATE TABLE session (id text PRIMARY KEY); CREATE TABLE other (id text PRIMARY KEY);",
        )
        .unwrap();
        apply(&mut conn).unwrap();
        assert_eq!(journal(&conn).len(), MIGRATION_IDS.len());
    }

    #[test]
    fn rejects_non_empty_db_without_session_table() {
        let dir = TempDir::new("no-session");
        let mut conn = conn_in(&dir);
        conn.execute_batch("CREATE TABLE junk (id text PRIMARY KEY)")
            .unwrap();
        let err = apply(&mut conn).unwrap_err();
        assert_eq!(
            err.to_string(),
            "StorageError: Database is not empty and has no session table"
        );
    }

    #[test]
    fn seeds_journal_from_named_drizzle_journal() {
        let dir = TempDir::new("drizzle-named");
        let conn = conn_in(&dir);
        conn.execute_batch("CREATE TABLE session (id text PRIMARY KEY)")
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE __drizzle_migrations (id integer PRIMARY KEY, name text, created_at integer);
             INSERT INTO __drizzle_migrations (name, created_at) VALUES ('20260127222353_familiar_lady_ursula', 1);
             INSERT INTO __drizzle_migrations (name, created_at) VALUES ('20260211171708_add_project_commands', 2);
             INSERT INTO __drizzle_migrations (name, created_at) VALUES (NULL, 3);",
        )
        .unwrap();

        apply_only(&conn, MIGRATION_IDS).unwrap();
        let ids = journal(&conn);
        assert!(ids.contains(&"20260127222353_familiar_lady_ursula".to_string()));
        assert!(ids.contains(&"20260211171708_add_project_commands".to_string()));
        // Seeded from the drizzle journal + every unapplied compiled-in ID.
        assert_eq!(ids.len(), MIGRATION_IDS.len());
    }

    #[test]
    fn seeds_journal_from_timestamp_drizzle_journal() {
        let dir = TempDir::new("drizzle-ts");
        let conn = conn_in(&dir);
        conn.execute_batch(
            "CREATE TABLE session (id text PRIMARY KEY);
             CREATE TABLE __drizzle_migrations (id integer PRIMARY KEY, created_at integer);
             INSERT INTO __drizzle_migrations (created_at) VALUES (1769552633000);",
        )
        .unwrap();
        // 1769552633000 == 20260127222353 UTC.
        apply_only(&conn, MIGRATION_IDS).unwrap();
        let ids = journal(&conn);
        assert!(ids.contains(&"20260127222353_familiar_lady_ursula".to_string()));
        assert_eq!(ids.len(), MIGRATION_IDS.len());
    }

    #[test]
    fn rejects_unknown_legacy_timestamp() {
        let dir = TempDir::new("drizzle-unknown");
        let conn = conn_in(&dir);
        conn.execute_batch(
            "CREATE TABLE session (id text PRIMARY KEY);
             CREATE TABLE __drizzle_migrations (id integer PRIMARY KEY, created_at integer);
             INSERT INTO __drizzle_migrations (created_at) VALUES (12345678900000);",
        )
        .unwrap();
        let err = apply_only(&conn, MIGRATION_IDS).unwrap_err();
        assert_eq!(
            err.to_string(),
            "StorageError: Legacy migration timestamp 12345678900000 does not match any known migration"
        );
    }
}
