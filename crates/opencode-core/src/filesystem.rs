//! Ripgrep-backed fuzzy search — port of `core/src/filesystem/search.ts`
//! (the ripgrep layer; the `#fff` native index is out of scope).

use std::path::Path;

use crate::fuzzysort;
use crate::tool::ripgrep::{Ripgrep, RipgrepService};

/// The background walk state (`filesystem/search.ts:30-38`): every file the
/// ripgrep `--files` walk yields plus the derived directory set.
pub struct FindState {
    pub files: Vec<String>,
    pub directories: Vec<String>,
}

/// `Number.MAX_SAFE_INTEGER` — the walk limit under a vcs
/// (`filesystem/search.ts:36`).
const MAX_SAFE_INTEGER: usize = 9007199254740991;

const WALK_LIMIT: usize = 100_000;

impl FindState {
    /// Run the ripgrep walk building `state.files`/`state.directories`
    /// (`:35-48`). `vcs` is `location.vcs` — the limit is unbounded under
    /// a vcs, else 100 000.
    pub fn build(directory: &Path, vcs: bool) -> FindState {
        let mut files = Vec::new();
        let mut directories: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for entry in RipgrepService.find(
            directory,
            "*",
            false,
            false,
            if vcs { MAX_SAFE_INTEGER } else { WALK_LIMIT },
        ) {
            let path = entry.to_string_lossy().replace('\\', "/");
            files.push(path.clone());
            let parts: Vec<&str> = path.split('/').collect();
            for index in 0..parts.len().saturating_sub(1) {
                let directory_path = format!("{}/", parts[..=index].join("/"));
                if seen.insert(directory_path.clone()) {
                    directories.push(directory_path);
                }
            }
        }
        FindState { files, directories }
    }
}

/// `FileSystem.FindInput.type` — `file`, `directory` or unset (both).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindType {
    File,
    Directory,
}

/// The fuzzysort-backed `find` (`filesystem/search.ts:101-117`) — matching
/// targets with directory entries suffixed `path.sep`. Returns the matched
/// paths in fuzzysort order (best first).
pub fn find(
    state: &FindState,
    query: &str,
    find_type: Option<FindType>,
    limit: Option<usize>,
) -> Vec<String> {
    let items: Vec<String> = match find_type {
        Some(FindType::File) => state.files.clone(),
        Some(FindType::Directory) => state.directories.clone(),
        None => {
            let mut both = state.files.clone();
            both.extend(state.directories.clone());
            both
        }
    };
    fuzzysort::go(
        query,
        &items,
        &fuzzysort::Options {
            limit,
            threshold: None,
        },
    )
    .into_iter()
    .map(|index| items[index].clone())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn find_ranks_and_types() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("src/cmd")).unwrap();
        fs::write(root.path().join("src/cmd/main.rs"), "").unwrap();
        fs::write(root.path().join("README.md"), "").unwrap();

        let state = FindState::build(root.path(), false);
        assert_eq!(
            state.files,
            vec!["README.md".to_string(), "src/cmd/main.rs".to_string()]
        );
        // Intermediate directories, `/`-suffixed, first-seen order.
        assert_eq!(
            state.directories,
            vec!["src/".to_string(), "src/cmd/".to_string()]
        );

        let found = find(&state, "main", Some(FindType::File), None);
        assert_eq!(found, vec!["src/cmd/main.rs".to_string()]);

        let found = find(&state, "cmd", Some(FindType::Directory), None);
        assert_eq!(found, vec!["src/cmd/".to_string()]);

        let found = find(&state, "read", None, None);
        assert_eq!(found, vec!["README.md".to_string()]);
    }

    #[test]
    fn find_respects_gitignore_under_git() {
        let root = tempfile::tempdir().unwrap();
        // ripgrep respects .gitignore only inside a git repository.
        fs::create_dir_all(root.path().join(".git")).unwrap();
        fs::create_dir_all(root.path().join("node_modules/pkg")).unwrap();
        fs::create_dir_all(root.path().join("target")).unwrap();
        fs::write(root.path().join(".gitignore"), "node_modules\ntarget\n").unwrap();
        fs::write(root.path().join("node_modules/pkg/a.js"), "").unwrap();
        fs::write(root.path().join("target/b.rs"), "").unwrap();
        fs::write(root.path().join("kept.txt"), "").unwrap();

        let state = FindState::build(root.path(), true);
        // Hidden files (.gitignore) are skipped, matching rg's default.
        assert_eq!(state.files, vec!["kept.txt".to_string()]);
    }
}
