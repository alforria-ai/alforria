//! File watching for `watch` triggers, on the `notify` crate.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use notify::{EventKind, RecursiveMode, Watcher};

/// What a watch never reports: `.git`, and alforria's own directories, which
/// change every time a turn runs and would make a trigger fire itself.
#[derive(Debug, Clone, Default)]
pub struct WatchIgnore {
    roots: Vec<PathBuf>,
}

impl WatchIgnore {
    pub fn new(roots: Vec<PathBuf>) -> WatchIgnore {
        WatchIgnore {
            roots: roots
                .into_iter()
                .filter(|root| !root.as_os_str().is_empty())
                .map(|root| root.canonicalize().unwrap_or(root))
                .collect(),
        }
    }

    pub fn ignores(&self, path: &Path) -> bool {
        if path
            .components()
            .any(|part| matches!(part, Component::Normal(name) if name == ".git"))
        {
            return true;
        }
        self.roots.iter().any(|root| path.starts_with(root))
    }
}

/// Keeps a path watched for as long as it lives.
pub struct WatchHandle {
    _watcher: notify::RecommendedWatcher,
}

impl std::fmt::Debug for WatchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WatchHandle")
    }
}

/// Count the entries a recursive watch of `root` would cover, stopping just
/// past `limit`.
fn count_entries(root: &Path, ignore: &WatchIgnore, limit: usize) -> usize {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !ignore.ignores(entry.path()))
        .filter_map(Result::ok)
        .take(limit.saturating_add(1))
        .count()
}

/// Watch `path` (a file, or a directory and everything under it) and call
/// `on_change` with the paths that changed. Refuses a tree with more than
/// `max_entries` entries: a watch on a home directory or a `node_modules`
/// would cost more than it is worth.
pub fn watch(
    path: &Path,
    within: Option<&Path>,
    ignore: Arc<WatchIgnore>,
    max_entries: usize,
    on_change: impl Fn(Vec<PathBuf>) + Send + 'static,
) -> Result<WatchHandle, String> {
    let root = path
        .canonicalize()
        .map_err(|_| format!("{} does not exist, so it cannot be watched", path.display()))?;
    // Checked here, on the path actually handed to the watcher, and not only
    // when the trigger was saved: the path may have become a symlink since.
    if let Some(within) = within {
        if !crate::tool::external_directory::contains_real(within, &root) {
            return Err(format!(
                "{} leads outside {} and cannot be watched",
                path.display(),
                within.display()
            ));
        }
    }
    if ignore.ignores(&root) {
        return Err(format!(
            "{} is one of alforria's own directories (or inside .git) and cannot be watched",
            path.display()
        ));
    }
    let entries = count_entries(&root, &ignore, max_entries);
    if entries > max_entries {
        return Err(format!(
            "{} holds more than {max_entries} entries; watch a smaller directory",
            path.display()
        ));
    }
    let filter = ignore.clone();
    let inside = root.clone();
    let handler = move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            return;
        };
        if matches!(event.kind, EventKind::Access(_) | EventKind::Other) {
            return;
        }
        let paths: Vec<PathBuf> = event
            .paths
            .into_iter()
            .filter(|path| path.starts_with(&inside) && !filter.ignores(path))
            .collect();
        if !paths.is_empty() {
            on_change(paths);
        }
    };
    // Symlinks inside the tree are not followed: a link must not turn a
    // watch of the project into a watch of wherever it points.
    let mut watcher = notify::RecommendedWatcher::new(
        handler,
        notify::Config::default().with_follow_symlinks(false),
    )
    .map_err(|error| format!("could not start a file watcher: {error}"))?;
    let mode = if root.is_dir() {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    watcher
        .watch(&root, mode)
        .map_err(|error| format!("could not watch {}: {error}", path.display()))?;
    Ok(WatchHandle { _watcher: watcher })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_and_own_directories_are_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let ignore = WatchIgnore::new(vec![data.clone(), PathBuf::new()]);
        let data = data.canonicalize().unwrap();
        assert!(ignore.ignores(&data.join("opencode.db-wal")));
        assert!(ignore.ignores(Path::new("/repo/.git/index")));
        assert!(!ignore.ignores(Path::new("/repo/.github/workflows/ci.yml")));
        assert!(!ignore.ignores(Path::new("/repo/inbox/note.md")));
    }

    #[test]
    fn a_tree_over_the_cap_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..8 {
            std::fs::write(temp.path().join(format!("f{index}")), "x").unwrap();
        }
        // `.git` does not count toward the cap.
        std::fs::create_dir_all(temp.path().join(".git/objects")).unwrap();
        for index in 0..20 {
            std::fs::write(temp.path().join(format!(".git/objects/{index}")), "x").unwrap();
        }
        let ignore = Arc::new(WatchIgnore::default());
        let error = watch(temp.path(), None, ignore.clone(), 5, |_| {}).unwrap_err();
        assert!(error.contains("more than 5 entries"), "{error}");
        assert!(watch(temp.path(), None, ignore.clone(), 9, |_| {}).is_ok());
        let missing = watch(&temp.path().join("nope"), None, ignore, 9, |_| {}).unwrap_err();
        assert!(missing.contains("does not exist"), "{missing}");
    }

    #[test]
    fn own_directories_cannot_be_watched() {
        let temp = tempfile::tempdir().unwrap();
        let ignore = Arc::new(WatchIgnore::new(vec![temp.path().to_path_buf()]));
        let error = watch(temp.path(), None, ignore, 100, |_| {}).unwrap_err();
        assert!(error.contains("cannot be watched"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_project_is_not_watched() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, project.join("link")).unwrap();
        let ignore = Arc::new(WatchIgnore::default());
        let error = watch(
            &project.join("link"),
            Some(&project),
            ignore.clone(),
            100,
            |_| {},
        )
        .unwrap_err();
        assert!(error.contains("leads outside"), "{error}");
        // The project itself is watched, without walking through the link.
        for index in 0..20 {
            std::fs::write(outside.join(format!("f{index}")), "x").unwrap();
        }
        assert!(watch(&project, Some(&project), ignore, 5, |_| {}).is_ok());
    }
}
