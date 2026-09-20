//! cli/cmd/db.ts port — the `db` command: raw SQL queries and the
//! interactive `sqlite3` shell.

use std::path::PathBuf;

use clap::ArgMatches;
use rusqlite::types::ValueRef;
use serde_json::Value;

use crate::error::{CliError, TypedError};
use crate::ui::Ui;

/// The default database path (`Database.path()`, database.ts:53).
pub fn database_path(paths: &opencode_core::GlobalPaths) -> PathBuf {
    opencode_core::storage::db_path(&paths.data)
}

/// One cell → JSON. Blobs render with the JS `Buffer` JSON shape.
fn cell_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Number(i.into()),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        ValueRef::Text(text) => Value::String(String::from_utf8_lossy(text).into_owned()),
        ValueRef::Blob(blob) => serde_json::json!({
            "type": "Buffer",
            "data": blob.to_vec(),
        }),
    }
}

/// Execute raw SQL, returning rows as ordered JSON objects.
pub fn query_rows(
    storage: &opencode_core::Storage,
    query: &str,
) -> Result<Vec<serde_json::Map<String, Value>>, String> {
    storage.with_connection(|conn| {
        let mut stmt = conn.prepare(query).map_err(|err| err.to_string())?;
        let column_names: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        let mut rows = stmt.query([]).map_err(|err| err.to_string())?;
        let mut out = Vec::new();
        loop {
            let row = match rows.next() {
                Ok(Some(row)) => row,
                Ok(None) => break,
                Err(err) => return Err(err.to_string()),
            };
            let mut item = serde_json::Map::new();
            for (index, column) in column_names.iter().enumerate() {
                let value = match row.get_ref(index) {
                    Ok(value) => value,
                    Err(err) => return Err(err.to_string()),
                };
                item.insert(column.clone(), cell_json(value));
            }
            out.push(item);
        }
        Ok(out)
    })
}

/// The `--format tsv` renderer (db.ts:31-34): header row + tab rows;
/// nothing when the result set is empty.
pub fn format_tsv(rows: &[serde_json::Map<String, Value>]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let keys: Vec<String> = rows[0].keys().cloned().collect();
    let mut lines = vec![keys.join("\t")];
    for row in rows {
        lines.push(
            keys.iter()
                .map(|key| cell_to_string(&row[key]))
                .collect::<Vec<_>>()
                .join("\t"),
        );
    }
    lines.join("\n")
}

/// JS `String(value)` over a SQLite cell.
fn cell_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `db [query]` (db.ts:8-43) — query execution or the `sqlite3` shell.
pub fn run(matches: &ArgMatches, ui: &mut Ui) -> Result<(), TypedError> {
    let paths = opencode_core::GlobalPaths::from_env();
    if matches.subcommand_name().is_some() {
        let path = database_path(&paths);
        ui.write_stdout(&format!("{}\n", path.display()));
        return Ok(());
    }
    let query = matches
        .get_one::<String>("query")
        .map(String::as_str)
        .unwrap_or_default();
    let format = matches
        .get_one::<String>("format")
        .map(String::as_str)
        .unwrap_or("tsv");
    if query.is_empty() {
        return shell(&paths);
    }
    let storage = opencode_core::Storage::open_default(&paths.data)
        .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
    let rows = query_rows(&storage, query)
        .map_err(|err| TypedError::Cli(CliError::new(format!("SQL error: {err}"))))?;
    if format == "json" {
        let value = serde_json::to_value(&rows).unwrap_or(Value::Array(Vec::new()));
        ui.write_stdout(&format!(
            "{}\n",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        ));
    } else {
        let tsv = format_tsv(&rows);
        if !tsv.is_empty() {
            ui.write_stdout(&format!("{tsv}\n"));
        }
    }
    Ok(())
}

/// `spawn("sqlite3", [Database.path()], { stdio: "inherit" })` (db.ts:38-41).
fn shell(paths: &opencode_core::GlobalPaths) -> Result<(), TypedError> {
    let path = database_path(paths);
    let status = std::process::Command::new("sqlite3")
        .arg(&path)
        .status()
        .map_err(|err| TypedError::Cli(CliError::new(format!("Failed to run sqlite3: {err}"))))?;
    if !status.success() {
        return Err(TypedError::Cli(CliError::new("sqlite3 shell failed")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> opencode_core::Storage {
        let dir = tempfile::tempdir().unwrap();
        opencode_core::Storage::open(dir.path().join("db.sqlite")).unwrap()
    }

    #[test]
    fn database_path_is_data_opencode_db() {
        let paths = opencode_core::GlobalPaths::resolve(std::path::PathBuf::from("/home"));
        let path = database_path(&paths);
        assert!(path.ends_with("opencode.db"), "{}", path.display());
    }

    #[test]
    fn query_json_output_shape() {
        let storage = storage();
        storage
            .with_connection(|conn| conn.execute("CREATE TABLE t (id INTEGER, name TEXT)", []))
            .unwrap();
        storage
            .with_connection(|conn| {
                conn.execute("INSERT INTO t (id, name) VALUES (1, 'a'), (2, NULL)", [])
            })
            .unwrap();
        let rows = query_rows(&storage, "SELECT id, name FROM t ORDER BY id").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["id"], Value::Number(1.into()));
        assert_eq!(rows[0]["name"], Value::String("a".to_string()));
        assert_eq!(rows[1]["name"], Value::Null);
        let json = serde_json::to_string_pretty(&rows).unwrap();
        assert!(json.contains("\"id\": 1"), "{json}");
    }

    #[test]
    fn insert_via_query_rows_persists() {
        let storage = opencode_core::Storage::open_in_memory().unwrap();
        storage.with_connection(|conn| {
            let _ = conn.execute(
                "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated) VALUES ('p1', '/w', '[]', 1, 1)",
                [],
            );
        });
        assert!(!query_rows(&storage, "SELECT id FROM project")
            .unwrap()
            .is_empty());
        query_rows(&storage, "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated) VALUES ('p2', '/w', '[]', 1, 1)")
            .unwrap();
        assert_eq!(
            query_rows(&storage, "SELECT id FROM project")
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn tsv_renders_header_and_rows() {
        let rows: Vec<serde_json::Map<String, Value>> = vec![
            serde_json::Map::from_iter([
                ("id".to_string(), Value::Number(1.into())),
                ("name".to_string(), Value::String("a b".to_string())),
            ]),
            serde_json::Map::from_iter([
                ("id".to_string(), Value::Number(2.into())),
                ("name".to_string(), Value::Null),
            ]),
        ];
        assert_eq!(format_tsv(&rows), "id\tname\n1\ta b\n2\tnull");
    }

    #[test]
    fn tsv_empty_result_prints_nothing() {
        assert_eq!(format_tsv(&[]), "");
    }

    #[test]
    fn invalid_sql_is_an_error() {
        let storage = storage();
        assert!(query_rows(&storage, "SELECT FROM").is_err());
    }
}
