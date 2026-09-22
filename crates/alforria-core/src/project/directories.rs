//! `project_directory` table ops — port of
//! `packages/core/src/project/directories.ts`.

use alforria_schema::project::ProjectDirectory;

use crate::storage::Storage;
use crate::CoreError;

/// `ProjectDirectories.create` (`directories.ts:65-82`).
///
/// `behavior: "ignore"` (the default) inserts or does nothing on conflict;
/// `"replace"` updates the strategy — but only when it differs, or when
/// the stored row has one and the update removes it.
pub fn create(storage: &Storage, input: &ProjectDirectoryCreate) -> Result<bool, CoreError> {
    storage.with_connection(|conn| {
        if input.behavior == DirectoryBehavior::Replace {
            let updated = conn.execute(
                "INSERT INTO project_directory (project_id, directory, strategy, time_created)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (project_id, directory) DO UPDATE SET strategy = ?3
                 WHERE (?3 IS NOT NULL AND (strategy IS NULL OR strategy != ?3))
                    OR (?3 IS NULL AND strategy IS NOT NULL)",
                rusqlite::params![
                    input.project_id,
                    input.directory,
                    input.strategy,
                    time_now() as i64,
                ],
            )?;
            return Ok(updated == 1);
        }
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO project_directory (project_id, directory, strategy, time_created)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                input.project_id,
                input.directory,
                input.strategy,
                time_now() as i64,
            ],
        )?;
        Ok(inserted == 1)
    })
}

/// `ProjectDirectories.remove` (`directories.ts:84-98`).
pub fn remove(storage: &Storage, project_id: &str, directory: &str) -> Result<bool, CoreError> {
    storage.with_connection(|conn| {
        Ok(conn.execute(
            "DELETE FROM project_directory WHERE project_id = ?1 AND directory = ?2",
            rusqlite::params![project_id, directory],
        )? == 1)
    })
}

/// `ProjectDirectories.list` (`directories.ts:100-109`) — newest first,
/// then directory ascending.
pub fn list(storage: &Storage, project_id: &str) -> Result<Vec<ProjectDirectory>, CoreError> {
    storage.with_connection(|conn| {
        let mut stmt = conn.prepare(
            "SELECT directory, strategy FROM project_directory
             WHERE project_id = ?1
             ORDER BY time_created DESC, directory ASC",
        )?;
        let mut rows = stmt.query(rusqlite::params![project_id])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(ProjectDirectory {
                directory: row.get(0)?,
                strategy: row.get(1)?,
            });
        }
        Ok(out)
    })
}

/// `ProjectDirectories.contains` (`directories.ts:111-128`).
pub fn contains(storage: &Storage, project_id: &str, directory: &str) -> Result<bool, CoreError> {
    storage.with_connection(|conn| {
        let found = conn.query_row(
            "SELECT directory FROM project_directory
             WHERE project_id = ?1 AND directory = ?2",
            rusqlite::params![project_id, directory],
            |_| Ok(()),
        );
        Ok(matches!(found, Ok(())))
    })
}

/// `ProjectDirectories.get` (`directories.ts:130-146`).
pub fn get(
    storage: &Storage,
    project_id: &str,
    directory: &str,
) -> Result<Option<ProjectDirectory>, CoreError> {
    storage.with_connection(|conn| {
        let mut stmt = conn.prepare(
            "SELECT directory, strategy FROM project_directory
             WHERE project_id = ?1 AND directory = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![project_id, directory])?;
        match rows.next()? {
            Some(row) => Ok(Some(ProjectDirectory {
                directory: row.get(0)?,
                strategy: row.get(1)?,
            })),
            None => Ok(None),
        }
    })
}

/// `CreateInput` (`directories.ts:17-22`).
pub struct ProjectDirectoryCreate<'a> {
    pub project_id: &'a str,
    pub directory: &'a str,
    pub strategy: Option<&'a str>,
    pub behavior: DirectoryBehavior,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryBehavior {
    Ignore,
    Replace,
}

fn time_now() -> u64 {
    // `$default(() => Date.now())` — millisecond wall time.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_support::TempDir;

    fn storage() -> Storage {
        let dir = TempDir::new("project-directories");
        let storage = Storage::open(dir.path().join("db.sqlite")).unwrap();
        // project_directory rows reference project(id)
        storage
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated)
                     VALUES ('p1', '/repo', '[]', 1, 1)",
                    [],
                )
            })
            .unwrap();
        storage
    }

    #[test]
    fn create_list_contains_get_remove() {
        let storage = storage();
        let created = create(
            &storage,
            &ProjectDirectoryCreate {
                project_id: "p1",
                directory: "/repo",
                strategy: Some("git_worktree"),
                behavior: DirectoryBehavior::Ignore,
            },
        )
        .unwrap();
        assert!(created);

        assert_eq!(
            list(&storage, "p1").unwrap(),
            vec![ProjectDirectory {
                directory: "/repo".to_string(),
                strategy: Some("git_worktree".to_string()),
            }]
        );
        assert!(contains(&storage, "p1", "/repo").unwrap());
        assert_eq!(
            get(&storage, "p1", "/repo").unwrap().unwrap().strategy,
            Some("git_worktree".to_string())
        );
        assert!(get(&storage, "p1", "/other").unwrap().is_none());
        assert!(remove(&storage, "p1", "/repo").unwrap());
        assert!(!remove(&storage, "p1", "/repo").unwrap());
    }

    #[test]
    fn ignore_behavior_does_not_overwrite() {
        let storage = storage();
        create(
            &storage,
            &ProjectDirectoryCreate {
                project_id: "p1",
                directory: "/repo",
                strategy: Some("a"),
                behavior: DirectoryBehavior::Ignore,
            },
        )
        .unwrap();
        let second = create(
            &storage,
            &ProjectDirectoryCreate {
                project_id: "p1",
                directory: "/repo",
                strategy: Some("b"),
                behavior: DirectoryBehavior::Ignore,
            },
        )
        .unwrap();
        assert!(!second, "onConflictDoNothing");
        assert_eq!(
            get(&storage, "p1", "/repo").unwrap().unwrap().strategy,
            Some("a".into())
        );
    }

    #[test]
    fn replace_behavior_updates_only_differing_strategies() {
        let storage = storage();
        create(
            &storage,
            &ProjectDirectoryCreate {
                project_id: "p1",
                directory: "/repo",
                strategy: Some("a"),
                behavior: DirectoryBehavior::Ignore,
            },
        )
        .unwrap();
        // same strategy — no-op (setWhere `ne(strategy, input.strategy)`)
        create(
            &storage,
            &ProjectDirectoryCreate {
                project_id: "p1",
                directory: "/repo",
                strategy: Some("a"),
                behavior: DirectoryBehavior::Replace,
            },
        )
        .unwrap();
        // different strategy — updates
        create(
            &storage,
            &ProjectDirectoryCreate {
                project_id: "p1",
                directory: "/repo",
                strategy: Some("b"),
                behavior: DirectoryBehavior::Replace,
            },
        )
        .unwrap();
        assert_eq!(
            get(&storage, "p1", "/repo").unwrap().unwrap().strategy,
            Some("b".into())
        );
        // clearing the strategy is allowed (setWhere `isNotNull(strategy)`)
        create(
            &storage,
            &ProjectDirectoryCreate {
                project_id: "p1",
                directory: "/repo",
                strategy: None,
                behavior: DirectoryBehavior::Replace,
            },
        )
        .unwrap();
        assert_eq!(
            get(&storage, "p1", "/repo").unwrap().unwrap().strategy,
            None
        );
    }

    #[test]
    fn list_orders_by_time_created_desc_then_directory() {
        let storage = storage();
        // Explicit timestamps: insert order 1, 2, 3 for z, m, a.
        storage
            .with_connection(|conn| {
                for (directory, time) in [("/repo/z", 1), ("/repo/m", 2), ("/repo/a", 3)] {
                    conn.execute(
                        "INSERT INTO project_directory (project_id, directory, time_created)
                         VALUES ('p1', ?1, ?2)",
                        rusqlite::params![directory, time],
                    )?;
                }
                // Tie on time_created → directory ascending breaks it.
                conn.execute(
                    "INSERT INTO project_directory (project_id, directory, time_created)
                     VALUES ('p1', '/repo/x', 3)",
                    [],
                )
            })
            .unwrap();
        let order: Vec<String> = list(&storage, "p1")
            .unwrap()
            .into_iter()
            .map(|d| d.directory)
            .collect();
        assert_eq!(order, vec!["/repo/a", "/repo/x", "/repo/m", "/repo/z"]);
    }
}
