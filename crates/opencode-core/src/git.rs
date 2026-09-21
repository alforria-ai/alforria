//! Git subprocess plumbing — port of `packages/core/src/git.ts` (the parts
//! M7 needs: `repo.discover`, `remote.get`, `history.rootCommits`, plus the
//! raw runner `init_git` and the snapshot seam build on).
//!
//! All git access is subprocess `git` (no libgit2, matching the TS
//! reference). Failures are data, never panics: a git spawn error maps to
//! `code: 1` with empty output (`git.ts:954-956`).

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::CoreError;

/// `Git.Repository` (`git.ts:14-18`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub worktree: PathBuf,
    pub git_directory: PathBuf,
    pub common_directory: PathBuf,
}

/// One subprocess result — exit code + captured output (`git.ts:948-952`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitResult {
    pub exit_code: i32,
    pub text: String,
    pub stderr: String,
}

impl GitResult {
    fn failure() -> GitResult {
        GitResult {
            exit_code: 1,
            text: String::new(),
            stderr: String::new(),
        }
    }
}

/// Subprocess git seam (`Git.Service`) — the subset M7 needs. Tests inject
/// fakes; production uses [`SubprocessGit`].
pub trait GitRunner: Send + Sync {
    /// Run `git <args>` with `cwd` (or the instance directory when absent)
    /// and capture the result (`run`/`execute`, `git.ts:184-240`).
    fn run(&self, cwd: Option<&Path>, args: &[&str]) -> GitResult;

    /// `repo.discover` (`git.ts:184-201`): walk up for `.git`, then
    /// `rev-parse --show-toplevel` / `--git-dir` / `--git-common-dir`.
    /// `None` when no repository encloses the directory.
    fn discover(&self, dir: &Path) -> Option<Repository> {
        let dotgit = fs_up(dir, ".git")?;
        let cwd = dotgit.parent()?.to_path_buf();
        let top_level = self.run(Some(&cwd), &["rev-parse", "--show-toplevel"]);
        let git_dir = self.run(Some(&cwd), &["rev-parse", "--git-dir"]);
        let common_dir = self.run(Some(&cwd), &["rev-parse", "--git-common-dir"]);
        if git_dir.exit_code != 0 || common_dir.exit_code != 0 {
            return None;
        }
        let worktree = if top_level.exit_code == 0 {
            resolve_path(&cwd, &top_level.text)
        } else {
            cwd.clone()
        };
        Some(Repository {
            worktree,
            git_directory: resolve_path(&cwd, &git_dir.text),
            common_directory: resolve_path(&cwd, &common_dir.text),
        })
    }

    /// `remote.get` (`git.ts:205-209`).
    fn remote_get(&self, repository: &Repository, name: &str) -> Option<String> {
        let result = self.run(Some(&repository.worktree), &["remote", "get-url", name]);
        if result.exit_code != 0 {
            return None;
        }
        let trimmed = result.text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    /// `history.rootCommits` (`git.ts:211-219`) — `rev-list
    /// --max-parents=0 HEAD`, trimmed, **sorted**.
    fn root_commits(&self, repository: &Repository) -> Vec<String> {
        let result = self.run(
            Some(&repository.worktree),
            &["rev-list", "--max-parents=0", "HEAD"],
        );
        if result.exit_code != 0 {
            return Vec::new();
        }
        let mut roots: Vec<String> = result
            .text
            .split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        roots.sort();
        roots
    }
}

/// `FSUtil.up({targets: [name], start})` — nearest ancestor of `start`
/// (inclusive) holding a `name` entry.
fn fs_up(start: &Path, name: &str) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

/// `resolvePath` (`git.ts:981-987`): trim trailing line breaks, normalize
/// absolute paths, resolve relative ones against `cwd`.
fn resolve_path(cwd: &Path, value: &str) -> PathBuf {
    let trimmed = value.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return cwd.to_path_buf();
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return lexical_normalize(path);
    }
    lexical_normalize(&cwd.join(path))
}

/// `path.resolve`-style lexical normalization (no filesystem access).
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut root = PathBuf::new();
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            c @ (Component::Prefix(_) | Component::RootDir) => {
                root.push(c.as_os_str());
                parts.clear();
            }
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            Component::Normal(part) => parts.push(part.to_os_string()),
        }
    }
    for part in parts {
        root.push(part);
    }
    root
}

/// Production subprocess runner (`AppProcess` + cross-spawn in TS).
#[derive(Debug, Clone, Copy, Default)]
pub struct SubprocessGit;

impl GitRunner for SubprocessGit {
    fn run(&self, cwd: Option<&Path>, args: &[&str]) -> GitResult {
        let mut command = Command::new("git");
        // The TS global config (git/index.ts:6-13): no lock contention,
        // raw paths, and quotepath=false so non-ASCII filenames decode.
        command
            .arg("--no-optional-locks")
            .args(["-c", "core.autocrlf=false"])
            .args(["-c", "core.fsmonitor=false"])
            .args(["-c", "core.longpaths=true"])
            .args(["-c", "core.symlinks=true"])
            .args(["-c", "core.quotepath=false"])
            .args(args)
            .stdin(std::process::Stdio::null());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        match command.output() {
            Ok(output) => GitResult {
                exit_code: output.status.code().unwrap_or(1),
                text: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            },
            Err(_) => GitResult::failure(),
        }
    }
}

/// `which("git")` (`util/which.ts`) — a `git` executable on `PATH` (or an
/// absolute-path override).
pub fn which_git() -> bool {
    let path = std::env::var_os("PATH");
    let path = match path {
        Some(path) => path,
        None => return false,
    };
    for dir in std::env::split_paths(&path) {
        if dir.join("git").is_file() {
            return true;
        }
    }
    false
}

/// `Error("Git is not installed")` etc. — plain message errors from
/// `initGit` (`project/project.ts:366-375`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct GitError(pub String);

impl From<GitError> for CoreError {
    fn from(err: GitError) -> Self {
        CoreError::Storage(err.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_support::TempDir;

    fn init_repo(dir: &Path) {
        let git = SubprocessGit;
        let result = git.run(Some(dir), &["init", "--quiet"]);
        assert_eq!(result.exit_code, 0, "git init: {}", result.stderr);
    }

    #[test]
    fn discover_walks_up_to_the_worktree() {
        let dir = TempDir::new("git-discover");
        init_repo(dir.path());
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();

        let repo = SubprocessGit.discover(&nested).expect("repo discovered");
        assert_eq!(repo.worktree, dir.path().canonicalize().unwrap());
        assert!(repo.git_directory.ends_with(".git"));
        assert_eq!(repo.git_directory, repo.common_directory);

        let outside = TempDir::new("git-norepo");
        assert!(SubprocessGit.discover(outside.path()).is_none());
    }

    #[test]
    fn remote_get_and_root_commits() {
        let dir = TempDir::new("git-remote");
        init_repo(dir.path());
        let repo = SubprocessGit.discover(dir.path()).unwrap();

        assert_eq!(SubprocessGit.remote_get(&repo, "origin"), None);

        std::fs::write(dir.path().join("file.txt"), "content\n").unwrap();
        let git = SubprocessGit;
        assert_eq!(git.run(Some(dir.path()), &["add", "file.txt"]).exit_code, 0);
        git.run(
            Some(dir.path()),
            &[
                "-c",
                "user.email=t@e.st",
                "-c",
                "user.name=T",
                "commit",
                "--quiet",
                "-m",
                "init",
            ],
        );
        let roots = git.root_commits(&repo);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].len(), 40, "sha1 commit id");
    }

    #[test]
    fn resolve_path_matrix() {
        assert_eq!(
            resolve_path(Path::new("/a"), "/b/c\n"),
            PathBuf::from("/b/c")
        );
        assert_eq!(resolve_path(Path::new("/a"), ""), PathBuf::from("/a"));
        assert_eq!(
            resolve_path(Path::new("/a"), "b/../c"),
            PathBuf::from("/a/c")
        );
    }
}
