//! Project copy service — port of `packages/core/src/project/copy.ts`
//! and `packages/core/src/project/copy-strategies.ts` (the built-in
//! `git_worktree` strategy).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::event::definition::Definition;
use crate::event::EventBus;
use crate::git::GitRunner;
use crate::project::directories::{self, DirectoryBehavior, ProjectDirectoryCreate};
use crate::storage::Storage;
use crate::CoreError;

/// `Event.Updated` (`project-directories.ts:4-9`) — non-durable.
static UPDATED: Definition = Definition::ephemeral("project.directories.updated");

/// `ProjectCopy.Copy` (`schema/project-copy.ts:30-33`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Copy {
    pub directory: String,
}

/// `ProjectCopy.ListEntry` (`copy.ts:57-60`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub directory: PathBuf,
    /// `"root"` for the main worktree, `"copy"` for linked ones.
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Root,
    Copy,
}

/// `Git.WorktreeError` (`git.ts:860-878`) — only `message` and
/// `forceRequired` reach the wire (`project-copy.ts` handler).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct GitWorktreeError {
    pub operation: &'static str,
    pub directory: String,
    pub message: String,
    pub force_required: Option<bool>,
}

/// The error set of `ProjectCopy.Interface` (`copy.ts:96-101`). Messages
/// match the v2 handler's `message` mapping (`server/handlers/project-copy.ts`).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CopyError {
    #[error("Project copy source not found: {directory}")]
    SourceDirectoryNotFound { directory: String },
    #[error("Project copy destination already exists: {directory}")]
    DestinationExists { directory: String },
    #[error("Project copy directory unavailable: {directory}")]
    DirectoryUnavailable { directory: String },
    #[error("Invalid project copy directory: {directory}")]
    InvalidDirectory { directory: String },
    #[error("Project copy strategy unavailable: {strategy}")]
    StrategyUnavailable { strategy: String },
    #[error("Project copy strategy already registered: {strategy}")]
    DuplicateStrategy { strategy: String },
    #[error(transparent)]
    Worktree(GitWorktreeError),
    /// Storage-layer failures fall through to their message
    /// (`message(error)` in the handler).
    #[error("{0}")]
    Core(String),
}

impl From<CoreError> for CopyError {
    fn from(err: CoreError) -> Self {
        CopyError::Core(err.to_string())
    }
}

impl CopyError {
    /// `forceRequired` on the 400 wire error — only worktree removes
    /// set it (`server/handlers/project-copy.ts:51-57`).
    pub fn force_required(&self) -> Option<bool> {
        match self {
            CopyError::Worktree(err) => err.force_required,
            _ => None,
        }
    }
}

/// `ProjectCopy.CreateInput` minus `projectID`/`sourceDirectory`
/// (`schema/project-copy.ts:13-21`).
#[derive(Debug, Clone)]
pub struct CreateInput<'a> {
    pub strategy: &'a str,
    pub source_directory: &'a str,
    pub directory: &'a str,
    pub name: Option<&'a str>,
}

/// `ProjectCopy.RemoveInput` minus `projectID` (`schema/project-copy.ts:23-28`).
#[derive(Debug, Clone)]
pub struct RemoveInput<'a> {
    pub directory: &'a str,
    pub force: bool,
}

/// `ProjectCopy.RefreshResult` (`copy.ts:32-35`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct RefreshResult {
    pub updated: Vec<String>,
    pub removed: Vec<String>,
}

/// `Strategy` (`copy.ts:103-113`) — the copy strategy seam. The TS layer
/// only registers `makeGitWorktreeStrategy`; the Rust registry mirrors
/// that by default.
pub trait Strategy: Send + Sync {
    fn id(&self) -> &str;
    fn create(&self, source: &Path, directory: &Path) -> Result<(), CopyError>;
    fn remove(&self, directory: &Path, force: bool) -> Result<(), CopyError>;
    fn list(&self, directory: &Path) -> Result<Vec<ListEntry>, CopyError>;
}

pub struct ProjectCopy {
    storage: Arc<Storage>,
    events: Arc<EventBus>,
    strategies: Mutex<HashMap<String, Arc<dyn Strategy>>>,
}

impl ProjectCopy {
    pub fn new(storage: Arc<Storage>, events: Arc<EventBus>) -> Arc<ProjectCopy> {
        let service = Arc::new(ProjectCopy {
            storage,
            events,
            strategies: Mutex::new(HashMap::new()),
        });
        // Register the default strategy (`copy.ts:143`) — the registration
        // is infallible for the empty registry.
        let _ = service.register(Arc::new(GitWorktreeStrategy {
            git: Arc::new(crate::git::SubprocessGit),
        }));
        service
    }

    /// `register` (`copy.ts:126-130`) + the default-strategy registration
    /// (`copy.ts:143`).
    pub fn register(&self, strategy: Arc<dyn Strategy>) -> Result<(), CopyError> {
        let mut strategies = self.strategies.lock().unwrap_or_else(|p| p.into_inner());
        let id = strategy.id().to_string();
        if strategies.contains_key(&id) {
            return Err(CopyError::DuplicateStrategy { strategy: id });
        }
        strategies.insert(id, strategy);
        Ok(())
    }

    fn strategy(&self, id: &str) -> Result<Arc<dyn Strategy>, CopyError> {
        let strategies = self.strategies.lock().unwrap_or_else(|p| p.into_inner());
        strategies
            .get(id)
            .cloned()
            .ok_or_else(|| CopyError::StrategyUnavailable {
                strategy: id.to_string(),
            })
    }

    /// `canonical` (`copy.ts:120-123`) — absolute form of `input`; errors
    /// when the path is not a directory.
    fn canonical(&self, input: &Path) -> Result<PathBuf, CopyError> {
        let resolved = resolve_path(input);
        if !resolved.is_dir() {
            return Err(CopyError::DirectoryUnavailable {
                directory: input.to_string_lossy().into_owned(),
            });
        }
        Ok(resolved)
    }

    /// `source` (`copy.ts:145-150`) — canonical and owned by the project.
    fn source(&self, source: &str, project_id: &str) -> Result<PathBuf, CopyError> {
        let source_directory = self.canonical(Path::new(source))?;
        if !directories::contains(
            &self.storage,
            project_id,
            &source_directory.to_string_lossy(),
        )? {
            return Err(CopyError::SourceDirectoryNotFound {
                directory: source_directory.to_string_lossy().into_owned(),
            });
        }
        Ok(source_directory)
    }

    /// `ProjectCopy.create` (`copy.ts:152-178`).
    pub fn create(&self, project_id: &str, input: &CreateInput) -> Result<Copy, CopyError> {
        let selected = self.strategy(input.strategy)?;
        let source_directory = self.source(input.source_directory, project_id)?;
        let destination_parent = Path::new(input.directory);
        std::fs::create_dir_all(destination_parent)
            .map_err(|err| CoreError::Storage(err.to_string()))?;
        let name = input
            .name
            .map(str::to_string)
            .unwrap_or_else(crate::session::agents::slug_create);
        let mut suffix = 1;
        let mut copy_directory = destination_parent.join(&name);
        while exists_safe(&copy_directory) {
            suffix += 1;
            if suffix > 10 {
                return Err(CopyError::DestinationExists {
                    directory: copy_directory.to_string_lossy().into_owned(),
                });
            }
            copy_directory = destination_parent.join(format!("{name}-{suffix}"));
        }

        selected.create(&source_directory, &copy_directory)?;
        let copy_directory = self.canonical(&copy_directory)?;
        let created = directories::create(
            &self.storage,
            &ProjectDirectoryCreate {
                project_id,
                directory: &copy_directory.to_string_lossy(),
                strategy: Some(input.strategy),
                behavior: DirectoryBehavior::Replace,
            },
        )?;
        self.changed(project_id, created);
        Ok(Copy {
            directory: copy_directory.to_string_lossy().into_owned(),
        })
    }

    /// `ProjectCopy.remove` (`copy.ts:180-193`).
    pub fn remove(&self, project_id: &str, input: &RemoveInput) -> Result<(), CopyError> {
        let copy_directory = self.canonical(Path::new(input.directory))?;
        let stored =
            directories::get(&self.storage, project_id, &copy_directory.to_string_lossy())?;
        let Some(strategy_id) = stored.and_then(|row| row.strategy) else {
            return Err(CopyError::InvalidDirectory {
                directory: copy_directory.to_string_lossy().into_owned(),
            });
        };
        let strategy = self.strategy(&strategy_id)?;
        strategy.remove(&copy_directory, input.force)?;
        let removed =
            directories::remove(&self.storage, project_id, &copy_directory.to_string_lossy())?;
        self.changed(project_id, removed);
        Ok(())
    }

    /// `ProjectCopy.refresh` (`copy.ts:195-244`).
    pub fn refresh(&self, project_id: &str) -> Result<RefreshResult, CopyError> {
        let stored = directories::list(&self.storage, project_id)?;
        let mut discovered: HashMap<String, Option<String>> = HashMap::new();
        let mut discovered_order: Vec<String> = Vec::new();
        let mut removed = Vec::new();
        for item in &stored {
            let path = Path::new(&item.directory);
            if !path.is_dir() {
                removed.push(item.directory.clone());
                continue;
            }
            if item.strategy.is_some() {
                continue;
            }
            let strategies = self.strategies.lock().unwrap_or_else(|p| p.into_inner());
            for strategy in strategies.values() {
                let Ok(entries) = strategy.list(path) else {
                    // `ProjectCopy.DirectoryUnavailableError` → `[]`.
                    continue;
                };
                for entry in entries {
                    let directory = entry.directory.to_string_lossy().into_owned();
                    let strategy_id =
                        (entry.kind == EntryKind::Copy).then(|| strategy.id().to_string());
                    if discovered.insert(directory.clone(), strategy_id).is_none() {
                        discovered_order.push(directory);
                    }
                }
            }
        }
        let mut updated = Vec::new();
        for directory in discovered_order {
            let strategy_id = discovered.get(&directory).and_then(|s| s.clone());
            if directories::create(
                &self.storage,
                &ProjectDirectoryCreate {
                    project_id,
                    directory: &directory,
                    strategy: strategy_id.as_deref(),
                    behavior: DirectoryBehavior::Replace,
                },
            )? {
                updated.push(directory);
            }
        }
        let mut removed_dirs = Vec::new();
        for directory in &removed {
            if directories::remove(&self.storage, project_id, directory)? {
                removed_dirs.push(directory.clone());
            }
        }
        let result = RefreshResult {
            updated,
            removed: removed_dirs,
        };
        self.changed(
            project_id,
            !result.updated.is_empty() || !result.removed.is_empty(),
        );
        Ok(result)
    }

    /// `changed` (`copy.ts:118-120`).
    fn changed(&self, project_id: &str, update: bool) {
        if update {
            let _ = self.events.publish(
                &UPDATED,
                serde_json::json!({"projectID": project_id}),
                Default::default(),
            );
        }
    }
}

/// `fs.existsSafe` — an `exists` that swallows errors (`copy.ts:166`).
fn exists_safe(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// `fs.resolve` — absolute path normalization without symlink
/// resolution (Node `path.resolve`).
fn resolve_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize(path)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        normalize(&cwd.join(path))
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The built-in `git_worktree` strategy (`copy-strategies.ts:6-36`).
pub struct GitWorktreeStrategy {
    git: Arc<dyn GitRunner>,
}

impl GitWorktreeStrategy {
    pub fn new(git: Arc<dyn GitRunner>) -> GitWorktreeStrategy {
        GitWorktreeStrategy { git }
    }

    fn worktree_error(
        &self,
        result: &crate::git::GitResult,
        operation: &'static str,
        directory: &str,
    ) -> CopyError {
        let message = if !result.stderr.trim().is_empty() {
            result.stderr.trim().to_string()
        } else if !result.text.trim().is_empty() {
            result.text.trim().to_string()
        } else {
            "Git failed".to_string()
        };
        // `/contains modified or untracked files|is dirty/i`
        let force_required = (operation == "remove").then(|| {
            message.contains("contains modified or untracked files")
                || message.to_lowercase().contains("is dirty")
        });
        CopyError::Worktree(GitWorktreeError {
            operation,
            directory: directory.to_string(),
            message,
            force_required,
        })
    }
}

impl Strategy for GitWorktreeStrategy {
    fn id(&self) -> &str {
        "git_worktree"
    }

    fn create(&self, source: &Path, directory: &Path) -> Result<(), CopyError> {
        let Some(repository) = self.git.discover(source) else {
            return Err(CopyError::DirectoryUnavailable {
                directory: source.to_string_lossy().into_owned(),
            });
        };
        let result = self.git.run(
            Some(&repository.worktree),
            &[
                "worktree",
                "add",
                "--detach",
                &directory.to_string_lossy(),
                "HEAD",
            ],
        );
        if result.exit_code != 0 {
            return Err(self.worktree_error(&result, "create", &directory.to_string_lossy()));
        }
        if self.git.discover(directory).is_none() {
            return Err(CopyError::Worktree(GitWorktreeError {
                operation: "create",
                directory: directory.to_string_lossy().into_owned(),
                message: "Created worktree could not be opened".to_string(),
                force_required: None,
            }));
        }
        Ok(())
    }

    fn remove(&self, directory: &Path, force: bool) -> Result<(), CopyError> {
        let Some(repository) = self.git.discover(directory) else {
            return Err(CopyError::DirectoryUnavailable {
                directory: directory.to_string_lossy().into_owned(),
            });
        };
        let directory_arg = directory.to_string_lossy().into_owned();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&directory_arg);
        let result = self.git.run(Some(&repository.common_directory), &args);
        if result.exit_code != 0 {
            return Err(self.worktree_error(&result, "remove", &directory_arg));
        }
        Ok(())
    }

    fn list(&self, directory: &Path) -> Result<Vec<ListEntry>, CopyError> {
        let Some(repository) = self.git.discover(directory) else {
            return Err(CopyError::DirectoryUnavailable {
                directory: directory.to_string_lossy().into_owned(),
            });
        };
        let result = self.git.run(
            Some(&repository.worktree),
            &["worktree", "list", "--porcelain"],
        );
        if result.exit_code != 0 {
            return Err(self.worktree_error(&result, "list", &directory.to_string_lossy()));
        }
        let mut entries = Vec::new();
        for line in result.text.lines() {
            let Some(path) = line.strip_prefix("worktree ") else {
                continue;
            };
            let path = resolve_path(Path::new(path.trim()));
            if !path.is_dir() {
                // `canonical` fails → filtered out (`copy-strategies.ts:28-34`).
                continue;
            }
            entries.push(ListEntry {
                directory: path,
                kind: if entries.is_empty() {
                    EntryKind::Root
                } else {
                    EntryKind::Copy
                },
            });
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventBus;
    use crate::storage::test_support::TempDir;

    fn service(dir: &TempDir) -> Arc<ProjectCopy> {
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let events = Arc::new(EventBus::new_shared(storage.clone(), None));
        let service = ProjectCopy::new(storage, events);
        let _ = dir;
        service
    }

    #[test]
    fn source_must_be_in_project() {
        let dir = TempDir::new("project-copy");
        let service = service(&dir);
        let err = service
            .create(
                "proj",
                &CreateInput {
                    strategy: "git_worktree",
                    source_directory: dir.path().to_string_lossy().into_owned().as_str(),
                    directory: "/tmp",
                    name: None,
                },
            )
            .unwrap_err();
        assert!(
            matches!(err, CopyError::SourceDirectoryNotFound { .. }),
            "{err:?}"
        );
        let err = service
            .create(
                "proj",
                &CreateInput {
                    strategy: "missing",
                    source_directory: dir.path().to_string_lossy().into_owned().as_str(),
                    directory: "/tmp",
                    name: None,
                },
            )
            .unwrap_err();
        assert!(
            matches!(err, CopyError::StrategyUnavailable { .. }),
            "{err:?}"
        );
    }

    fn git(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn have_git() -> bool {
        crate::git::which_git()
    }

    #[test]
    fn create_remove_refresh_round_trip() {
        if !have_git() {
            return;
        }
        let dir = TempDir::new("project-copy");
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let events = Arc::new(EventBus::new_shared(storage.clone(), None));
        let service = ProjectCopy::new(storage, events);

        // A git repository is the copy source.
        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "--initial-branch", "main"]);
        std::fs::write(source.join("README.md"), "hi").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "init"]);

        let source_str = source.to_string_lossy().into_owned();
        service
            .storage
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated)
                     VALUES ('proj', 'proj', '[]', 1, 1)",
                    [],
                )
            })
            .unwrap();
        crate::project::directories::create(
            &service.storage,
            &ProjectDirectoryCreate {
                project_id: "proj",
                directory: &source_str,
                strategy: None,
                behavior: DirectoryBehavior::Ignore,
            },
        )
        .unwrap();

        // create
        let destination = dir.path().join("dest");
        let copy = service
            .create(
                "proj",
                &CreateInput {
                    strategy: "git_worktree",
                    source_directory: &source_str,
                    directory: &destination.to_string_lossy(),
                    name: Some("my-copy"),
                },
            )
            .unwrap();
        assert!(
            copy.directory.contains("my-copy"),
            "unexpected copy directory: {}",
            copy.directory
        );
        assert!(Path::new(&copy.directory).join("README.md").is_file());

        // remove
        service
            .remove(
                "proj",
                &RemoveInput {
                    directory: &copy.directory,
                    force: false,
                },
            )
            .unwrap();
        assert!(!Path::new(&copy.directory).exists());

        // refresh re-discovers an externally-created worktree
        let external = dir.path().join("external");
        git(
            &source,
            &[
                "worktree",
                "add",
                "--detach",
                &external.to_string_lossy(),
                "HEAD",
            ],
        );
        let result = service.refresh("proj").unwrap();
        assert_eq!(
            result.updated,
            vec![external.to_string_lossy().into_owned()]
        );
        let stored = crate::project::directories::list(&service.storage, "proj").unwrap();
        let row = stored
            .iter()
            .find(|row| row.directory == external.to_string_lossy())
            .expect("external worktree stored");
        assert_eq!(row.strategy.as_deref(), Some("git_worktree"));
    }
}
