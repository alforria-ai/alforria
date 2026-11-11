//! alforria's own migrations.
//!
//! [`crate::storage::migration`] keeps the upstream journal byte-compatible
//! and [`crate::storage::schema::SCHEMA_SQL`] is compared against upstream,
//! so neither can carry a table upstream does not have. Tables that only
//! alforria uses live here instead: numbered SQL files under
//! `alforria_migrations/`, applied in order after the upstream schema and
//! recorded in their own journal table, `alforria_migration`.
//!
//! To add one: drop a new file in `alforria_migrations/`, named
//! `<number>_<area>_<what>.sql`, and add it to [`MIGRATIONS`]. Never edit
//! a file that has shipped.

use rusqlite::Connection;

use crate::CoreError;

/// `(id, sql)`, in the order they are applied.
pub const MIGRATIONS: &[(&str, &str)] = &[(
    "0001_session_restriction",
    include_str!("alforria_migrations/0001_session_restriction.sql"),
)];

/// Every table these migrations own, the journal included. The upstream
/// schema comparison leaves them out; a migration that adds a table adds
/// it here (a test checks).
pub const OWN_TABLES: &[&str] = &["alforria_migration", "session_restriction"];

/// Apply every migration not yet in the journal.
pub fn apply(conn: &mut Connection) -> Result<(), CoreError> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS alforria_migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)",
        [],
    )?;
    for (id, sql) in MIGRATIONS {
        let applied: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM alforria_migration WHERE id = ?1)",
            [id],
            |row| row.get(0),
        )?;
        if applied {
            continue;
        }
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO alforria_migration (id, time_completed) VALUES (?1, ?2)",
            rusqlite::params![
                id,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis() as i64)
                    .unwrap_or(0)
            ],
        )?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_once_and_are_journalled() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn).unwrap();
        apply(&mut conn).unwrap();
        let journal: Vec<String> = conn
            .prepare("SELECT id FROM alforria_migration ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let expected: Vec<String> = MIGRATIONS.iter().map(|(id, _)| id.to_string()).collect();
        assert_eq!(journal, expected);
        conn.execute(
            "INSERT INTO session_restriction (session_id, time_created) VALUES ('ses_1', 1)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn own_tables_lists_exactly_what_the_migrations_create() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn).unwrap();
        let mut tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        tables.sort();
        let mut own: Vec<String> = OWN_TABLES.iter().map(|name| name.to_string()).collect();
        own.sort();
        assert_eq!(tables, own);
    }

    #[test]
    fn ids_are_unique_and_ordered() {
        let ids: Vec<&str> = MIGRATIONS.iter().map(|(id, _)| *id).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(ids, sorted);
    }
}
