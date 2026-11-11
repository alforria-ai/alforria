//! alforria's own migrations.
//!
//! [`crate::storage::migration`] keeps the upstream journal byte-compatible
//! and carries no bodies, so nothing of alforria's can go there (or into
//! `SCHEMA_SQL`, which is compared against upstream). Tables that only
//! alforria has are created here instead, one file per migration under
//! `migrations/`, named after the feature that owns it, and journaled in
//! `alforria_migration`.

use rusqlite::Connection;

use crate::CoreError;

/// Every alforria migration, in the order it is applied. Append only.
pub const MIGRATIONS: &[(&str, &str)] = &[(
    "trigger_0001_init",
    include_str!("migrations/trigger_0001_init.sql"),
)];

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Apply the migrations the journal does not list yet, each in its own
/// transaction.
pub fn apply(conn: &mut Connection) -> Result<(), CoreError> {
    apply_set(conn, MIGRATIONS)
}

fn apply_set(conn: &mut Connection, migrations: &[(&str, &str)]) -> Result<(), CoreError> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS alforria_migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)",
        [],
    )?;
    for (id, sql) in migrations {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let done: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM alforria_migration WHERE id = ?1)",
            [id],
            |row| row.get(0),
        )?;
        if !done {
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

    fn tables(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        names
    }

    #[test]
    fn applies_each_migration_once() {
        let mut conn = Connection::open_in_memory().unwrap();
        let set = [("a_0001", "CREATE TABLE a (id TEXT PRIMARY KEY)")];
        apply_set(&mut conn, &set).unwrap();
        apply_set(&mut conn, &set).unwrap();
        let more = [set[0], ("b_0001", "CREATE TABLE b (id TEXT PRIMARY KEY)")];
        apply_set(&mut conn, &more).unwrap();
        assert_eq!(tables(&conn), ["a", "alforria_migration", "b"]);
        let journaled: i64 = conn
            .query_row("SELECT COUNT(*) FROM alforria_migration", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(journaled, 2);
    }

    #[test]
    fn a_failing_migration_is_not_journaled() {
        let mut conn = Connection::open_in_memory().unwrap();
        let set = [("bad_0001", "CREATE TABLE c (id TEXT PRIMARY KEY); NOT SQL;")];
        assert!(apply_set(&mut conn, &set).is_err());
        assert_eq!(tables(&conn), ["alforria_migration"]);
    }

    #[test]
    fn the_shipped_migrations_apply_to_an_empty_database() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn).unwrap();
        apply(&mut conn).unwrap();
        assert!(tables(&conn).contains(&"trigger".to_string()));
    }
}
