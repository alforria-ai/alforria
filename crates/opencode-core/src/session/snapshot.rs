//! Snapshot seam — port of `snapshot/index.ts` reduced to the trait the
//! session engine consumes (spec M5.4). The git-backed implementation is
//! M7 scope; M5 ships [`InMemorySnapshot`] (a test double over file
//! contents, diffed with the M4 `tool::diff` helpers) and a disabled
//! production default.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use opencode_schema::file_diff::FileDiffStatus;

use crate::tool::def::BoxFuture;
use crate::tool::diff::{diff_line_counts, diff_lines};
use crate::CoreError;

/// `Snapshot.Patch` — `patch()` result (snapshot/index.ts:14-18). `files`
/// are absolute worktree paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPatch {
    pub hash: String,
    pub files: Vec<String>,
}

/// A patch reference handed to [`Snapshot::revert`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchPart {
    pub hash: String,
    pub files: Vec<String>,
}

/// `Snapshot.FileDiff` — the schema crate owns the wire shape.
pub type FileDiff = opencode_schema::file_diff::SnapshotFileDiff;

/// The snapshot service seam (`Snapshot.Interface`, snapshot/index.ts:36-45).
/// `track()` returns `None` when snapshots are disabled (the TS
/// `string | undefined` channel); every other method fails on unknown ids.
pub trait Snapshot: Send + Sync {
    /// Capture the current worktree; returns the snapshot id.
    fn track(&self) -> BoxFuture<'static, Option<String>>;
    /// Files changed since the snapshot.
    fn patch(&self, id: &str) -> BoxFuture<'static, Result<SnapshotPatch, CoreError>>;
    /// Check the snapshot out over the worktree.
    fn restore(&self, id: &str) -> BoxFuture<'static, Result<(), CoreError>>;
    /// Roll the listed files back to their snapshot contents.
    fn revert(&self, patches: Vec<PatchPart>) -> BoxFuture<'static, Result<(), CoreError>>;
    /// Diff text of the worktree vs the snapshot.
    fn diff(&self, id: &str) -> BoxFuture<'static, Result<String, CoreError>>;
    /// Per-file diffs between two snapshots.
    fn diff_full(
        &self,
        from: &str,
        to: &str,
    ) -> BoxFuture<'static, Result<Vec<FileDiff>, CoreError>>;
}

/// `config.snapshot == false` / non-git projects: snapshots are disabled.
pub struct DisabledSnapshot;

impl Snapshot for DisabledSnapshot {
    fn track(&self) -> BoxFuture<'static, Option<String>> {
        Box::pin(async { None })
    }

    fn patch(&self, _id: &str) -> BoxFuture<'static, Result<SnapshotPatch, CoreError>> {
        Box::pin(async { Err(CoreError::Storage("snapshots are disabled".to_string())) })
    }

    fn restore(&self, _id: &str) -> BoxFuture<'static, Result<(), CoreError>> {
        Box::pin(async { Ok(()) })
    }

    fn revert(&self, _patches: Vec<PatchPart>) -> BoxFuture<'static, Result<(), CoreError>> {
        Box::pin(async { Ok(()) })
    }

    fn diff(&self, _id: &str) -> BoxFuture<'static, Result<String, CoreError>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn diff_full(
        &self,
        _from: &str,
        _to: &str,
    ) -> BoxFuture<'static, Result<Vec<FileDiff>, CoreError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// The in-memory double: snapshots are file-content maps of a directory
/// tree. `track()` hashes the tree, `patch()` compares against disk.
pub struct InMemorySnapshot {
    worktree: PathBuf,
    state: Mutex<BTreeMap<String, BTreeMap<String, String>>>,
    counter: AtomicU64,
}

impl InMemorySnapshot {
    pub fn new(worktree: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(InMemorySnapshot {
            worktree: worktree.into(),
            state: Mutex::new(BTreeMap::new()),
            counter: AtomicU64::new(0),
        })
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, BTreeMap<String, String>>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn current(&self) -> BTreeMap<String, String> {
        let mut files = BTreeMap::new();
        for entry in WalkDir::new(&self.worktree)
            .into_iter()
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(&self.worktree) else {
                continue;
            };
            let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
            files.insert(rel.to_string_lossy().into_owned(), content);
        }
        files
    }
}

impl Snapshot for InMemorySnapshot {
    fn track(&self) -> BoxFuture<'static, Option<String>> {
        // All `self`-borrowing work happens before the `'static` future.
        let files = self.current();
        let mut hasher = Sha256::new();
        for (path, content) in &files {
            hasher.update(path.as_bytes());
            hasher.update(content.as_bytes());
        }
        let hash = hex::encode(hasher.finalize());
        let id = format!("{hash}-{}", self.counter.fetch_add(1, Ordering::Relaxed));
        self.lock_state().insert(id.clone(), files);
        Box::pin(async { Some(id) })
    }

    fn patch(&self, id: &str) -> BoxFuture<'static, Result<SnapshotPatch, CoreError>> {
        let (id, patch) = self.compute_patch(id);
        Box::pin(async move { patch.map(|files| SnapshotPatch { hash: id, files }) })
    }

    fn restore(&self, id: &str) -> BoxFuture<'static, Result<(), CoreError>> {
        let result = self.restore_sync(id);
        Box::pin(async move { result })
    }

    fn revert(&self, patches: Vec<PatchPart>) -> BoxFuture<'static, Result<(), CoreError>> {
        let result = self.revert_sync(patches);
        Box::pin(async move { result })
    }

    fn diff(&self, id: &str) -> BoxFuture<'static, Result<String, CoreError>> {
        let result = self.diff_sync(id);
        Box::pin(async move { result })
    }

    fn diff_full(
        &self,
        from: &str,
        to: &str,
    ) -> BoxFuture<'static, Result<Vec<FileDiff>, CoreError>> {
        let result = self.diff_full_sync(from, to);
        Box::pin(async move { result })
    }
}

impl InMemorySnapshot {
    fn compute_patch(&self, id: &str) -> (String, Result<Vec<String>, CoreError>) {
        let current = self.current();
        let state = self.lock_state();
        let Some(snapshot) = state.get(id) else {
            return (
                id.to_string(),
                Err(CoreError::Storage(format!("snapshot {id} not found"))),
            );
        };
        let mut files = Vec::new();
        for (path, content) in snapshot {
            if current
                .get(path)
                .map(|found| found != content)
                .unwrap_or(true)
            {
                files.push(path.clone());
            }
        }
        for path in current.keys() {
            if !snapshot.contains_key(path) {
                files.push(path.clone());
            }
        }
        files.sort();
        (id.to_string(), Ok(files))
    }

    fn restore_sync(&self, id: &str) -> Result<(), CoreError> {
        let snapshot = {
            let state = self.lock_state();
            let Some(snapshot) = state.get(id) else {
                return Err(CoreError::Storage(format!("snapshot {id} not found")));
            };
            snapshot.clone()
        };
        for (path, content) in &snapshot {
            let target = self.worktree.join(path);
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(target, content)
                .map_err(|error| CoreError::Storage(format!("snapshot restore failed: {error}")))?;
        }
        Ok(())
    }

    fn revert_sync(&self, patches: Vec<PatchPart>) -> Result<(), CoreError> {
        for patch in patches {
            let snapshot = {
                let state = self.lock_state();
                let Some(snapshot) = state.get(&patch.hash) else {
                    return Err(CoreError::Storage(format!(
                        "snapshot {} not found",
                        patch.hash
                    )));
                };
                snapshot.clone()
            };
            for file in patch.files {
                let Ok(rel) = Path::new(&file).strip_prefix(&self.worktree) else {
                    continue;
                };
                let rel = rel.to_string_lossy().into_owned();
                let target = self.worktree.join(&rel);
                match snapshot.get(&rel) {
                    Some(content) => std::fs::write(&target, content).map_err(|error| {
                        CoreError::Storage(format!("snapshot revert failed: {error}"))
                    })?,
                    None => {
                        let _ = std::fs::remove_file(&target);
                    }
                }
            }
        }
        Ok(())
    }

    fn diff_sync(&self, id: &str) -> Result<String, CoreError> {
        let current = self.current();
        let state = self.lock_state();
        let Some(snapshot) = state.get(id) else {
            return Err(CoreError::Storage(format!("snapshot {id} not found")));
        };
        let mut paths: Vec<&String> = snapshot.keys().collect();
        for path in current.keys() {
            if !snapshot.contains_key(path) {
                paths.push(path);
            }
        }
        paths.sort();
        let mut out = String::new();
        for path in paths {
            let before = snapshot.get(path).map(String::as_str).unwrap_or("");
            let after = current.get(path).map(String::as_str).unwrap_or("");
            if before == after {
                continue;
            }
            for change in diff_lines(before, after) {
                for line in &change.lines {
                    let tag = if change.added {
                        "+"
                    } else if change.removed {
                        "-"
                    } else {
                        " "
                    };
                    out.push_str(&format!("{tag}{line}"));
                }
            }
        }
        Ok(out.trim().to_string())
    }

    fn diff_full_sync(&self, from: &str, to: &str) -> Result<Vec<FileDiff>, CoreError> {
        let state = self.lock_state();
        let Some(before) = state.get(from) else {
            return Err(CoreError::Storage(format!("snapshot {from} not found")));
        };
        let Some(after) = state.get(to) else {
            return Err(CoreError::Storage(format!("snapshot {to} not found")));
        };
        let mut paths: Vec<&String> = before.keys().collect();
        for path in after.keys() {
            if !before.contains_key(path) {
                paths.push(path);
            }
        }
        paths.sort();
        let mut result = Vec::new();
        for path in paths {
            let old = before.get(path).map(String::as_str).unwrap_or("");
            let new = after.get(path).map(String::as_str).unwrap_or("");
            if old == new {
                continue;
            }
            let (additions, deletions) = diff_line_counts(old, new);
            let status = if before.contains_key(path) && after.contains_key(path) {
                FileDiffStatus::Modified
            } else if before.contains_key(path) {
                FileDiffStatus::Deleted
            } else {
                FileDiffStatus::Added
            };
            result.push(FileDiff {
                file: Some(path.clone()),
                patch: Some(crate::tool::diff::create_two_files_patch(path, old, new)),
                additions: additions as f64,
                deletions: deletions as f64,
                status: Some(status),
            });
        }
        Ok(result)
    }
}
