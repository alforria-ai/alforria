//! Project identity — port of `packages/core/src/project.ts`
//! (`ProjectV2.resolve` / `.commit`) plus the registry
//! ([`registry::ProjectRegistry`]) and the [`directories`] store.

pub mod directories;
pub mod registry;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use sha1::{Digest, Sha1};

use crate::git::{GitRunner, Repository};

/// `Project.ID.global` — the non-git sentinel (`project-id.ts`).
pub const GLOBAL_ID: &str = "global";

/// `ProjectV2.Vcs` (`project/schema.ts:10-15`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vcs {
    pub store: PathBuf,
}

/// `ProjectV2.Resolved` (`project.ts:30-35`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub previous: Option<String>,
    pub id: String,
    pub directory: PathBuf,
    pub vcs: Option<Vcs>,
}

/// `Hash.fast` — sha1 hex (`util/hash.ts:4-6`).
pub fn hash_fast(input: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// `Project.resolve` (`project.ts:110-122`).
pub fn resolve(git: &dyn GitRunner, input: &Path) -> Resolved {
    let Some(repo) = git.discover(input) else {
        return Resolved {
            previous: None,
            id: GLOBAL_ID.to_string(),
            directory: fs_root(input),
            vcs: None,
        };
    };
    let previous = cached_id(&repo.common_directory);
    let id = remote_id(git, &repo)
        .or_else(|| previous.clone())
        .or_else(|| root_id(git, &repo));
    Resolved {
        previous,
        id: id.unwrap_or_else(|| GLOBAL_ID.to_string()),
        directory: repo.worktree.clone(),
        vcs: Some(Vcs {
            store: repo.common_directory.clone(),
        }),
    }
}

/// `Project.commit` (`project.ts:124-126`) — write the id back to
/// `<store>/opencode`, ignoring all errors.
pub fn commit(store: &Path, id: &str) {
    let _ = std::fs::write(store.join("opencode"), id);
}

/// The cached id: `<common-dir>/opencode` content, trimmed; empty → `None`
/// (`project.ts:65-71`).
fn cached_id(common_directory: &Path) -> Option<String> {
    let value = std::fs::read_to_string(common_directory.join("opencode")).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The remote-derived id — sha1 of `git-remote:<normalized>` (`:73-79`).
fn remote_id(git: &dyn GitRunner, repo: &Repository) -> Option<String> {
    let origin = git.remote_get(repo, "origin")?;
    let normalized = normalize_url(&origin)?;
    Some(hash_fast(&format!("git-remote:{normalized}")))
}

enum UrlParse {
    /// `new URL(value)` succeeded → `(hostname, pathname)`.
    Ok { host: String, name: String },
    /// `new URL(value)` succeeded but the protocol is `file:`.
    Reject,
    /// `new URL(value)` threw.
    Invalid,
}

/// Remote-URL normalization (`project.ts:81-103`).
fn normalize_url(input: &str) -> Option<String> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    match parse_url(value) {
        UrlParse::Ok { host, name } => parts(&host, &name),
        UrlParse::Reject => None,
        UrlParse::Invalid => {
            // scp-style: `^([^@/:]+@)?([^/:]+):(.+)$` (`project.ts:90`).
            static SCP: OnceLock<Regex> = OnceLock::new();
            let scp = SCP.get_or_init(|| Regex::new(r"^([^@/:]+@)?([^/:]+):(.+)$").unwrap());
            let caps = scp.captures(value)?;
            parts(&caps[2], &caps[3])
        }
    }
}

/// A minimal `new URL(value)`: `<scheme>://[<user>@]<host>[:port]<path>`.
fn parse_url(value: &str) -> UrlParse {
    let Some((scheme, rest)) = value.split_once("://") else {
        return UrlParse::Invalid;
    };
    if scheme.is_empty()
        || rest.is_empty()
        || !scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    {
        return UrlParse::Invalid;
    }
    if scheme.eq_ignore_ascii_case("file") {
        return UrlParse::Reject;
    }
    let (authority, name) = match rest.split_once('/') {
        Some((authority, path)) => (authority.to_string(), format!("/{path}")),
        None => (rest.to_string(), String::new()),
    };
    let host = authority
        .split('@')
        .next_back()
        .unwrap_or(&authority)
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string();
    UrlParse::Ok { host, name }
}

/// `parts` (`project.ts:96-103`): strip leading `/`, trailing `.git`,
/// trailing `/`; host lowercased, pathname case preserved.
fn parts(host: &str, name: &str) -> Option<String> {
    let pathname = name.trim_start_matches('/');
    let pathname = match pathname
        .strip_suffix(".git/")
        .or_else(|| pathname.strip_suffix(".git"))
    {
        Some(stripped) => stripped,
        None => pathname,
    };
    let pathname = pathname.trim_end_matches('/');
    if host.is_empty() || pathname.is_empty() {
        return None;
    }
    Some(format!("{}/{}", host.to_lowercase(), pathname))
}

/// `path.parse(input).root` — the filesystem root of the input.
fn fs_root(input: &Path) -> PathBuf {
    let mut root = PathBuf::new();
    for component in input.components() {
        match component {
            std::path::Component::Prefix(prefix) => root.push(prefix.as_os_str()),
            std::path::Component::RootDir => root.push(std::path::Component::RootDir.as_os_str()),
            _ => break,
        }
    }
    if root.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        root
    }
}

/// The first sorted root commit, if any (`project.ts:105-108`).
fn root_id(git: &dyn GitRunner, repo: &Repository) -> Option<String> {
    git.root_commits(repo).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_table() {
        // https URLs — host lowercased, pathname case preserved
        assert_eq!(
            normalize_url("https://github.com/sst/opencode").unwrap(),
            "github.com/sst/opencode"
        );
        assert_eq!(
            normalize_url("https://GitHub.com/SST/Opencode").unwrap(),
            "github.com/SST/Opencode"
        );
        // trailing `.git` and slashes
        assert_eq!(
            normalize_url("https://github.com/sst/opencode.git").unwrap(),
            "github.com/sst/opencode"
        );
        assert_eq!(
            normalize_url("https://github.com/sst/opencode.git/").unwrap(),
            "github.com/sst/opencode"
        );
        assert_eq!(
            normalize_url("https://github.com/sst/opencode/").unwrap(),
            "github.com/sst/opencode"
        );
        // ports and credentials do not leak into the host
        assert_eq!(
            normalize_url("https://user:pass@GitHub.com:8443/sst/opencode").unwrap(),
            "github.com/sst/opencode"
        );
        // scp-style git@ URLs
        assert_eq!(
            normalize_url("git@github.com:sst/opencode.git").unwrap(),
            "github.com/sst/opencode"
        );
        // scp-style with an explicit user
        assert_eq!(
            normalize_url("me@github.com:sst/opencode").unwrap(),
            "github.com/sst/opencode"
        );
        // file: protocol never produces an id (project.ts:87)
        assert_eq!(normalize_url("file:///tmp/repo"), None);
        // empty host / pathname
        assert_eq!(normalize_url("https:///repo"), None);
        assert_eq!(normalize_url("https://github.com"), None);
        // paths without a host segment
        assert_eq!(normalize_url("/tmp/repo"), None);
        assert_eq!(normalize_url("   "), None);
    }

    #[test]
    fn project_id_is_deterministic() {
        let id = hash_fast("git-remote:github.com/sst/opencode");
        assert_eq!(id.len(), 40);
        assert_eq!(
            hash_fast("git-remote:github.com/sst/opencode"),
            hash_fast("git-remote:github.com/sst/opencode")
        );
    }

    struct FakeGit {
        repo: Option<Repository>,
        remote: Option<String>,
        roots: Vec<String>,
    }

    impl crate::git::GitRunner for FakeGit {
        fn run(&self, _cwd: Option<&Path>, _args: &[&str]) -> crate::git::GitResult {
            crate::git::GitResult {
                exit_code: 1,
                text: String::new(),
                stderr: String::new(),
            }
        }

        fn discover(&self, _dir: &Path) -> Option<Repository> {
            self.repo.clone()
        }

        fn remote_get(&self, _repository: &Repository, _name: &str) -> Option<String> {
            self.remote.clone()
        }

        fn root_commits(&self, _repository: &Repository) -> Vec<String> {
            self.roots.clone()
        }
    }

    fn fake_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "opencode-project-test-{}-{}",
            std::process::id(),
            ulid::Ulid::new()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_root_commit_without_remote_or_cache() {
        let dir = fake_dir();
        let git = FakeGit {
            repo: Some(Repository {
                worktree: dir.clone(),
                git_directory: dir.join(".git"),
                common_directory: dir.join(".git"),
            }),
            remote: None,
            roots: vec!["cafebabe".to_string()],
        };
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/opencode"), "").unwrap();

        let resolved = resolve(&git, &dir);
        assert_eq!(resolved.previous, None, "empty cache is not a previous id");
        assert_eq!(resolved.id, "cafebabe");
        assert_eq!(resolved.directory, dir);
        assert_eq!(
            resolved.vcs,
            Some(Vcs {
                store: dir.join(".git")
            })
        );
    }

    #[test]
    fn resolve_prefers_remote_over_cache_and_root() {
        let dir = fake_dir();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/opencode"), "previous-id").unwrap();
        let git = FakeGit {
            repo: Some(Repository {
                worktree: dir.clone(),
                git_directory: dir.join(".git"),
                common_directory: dir.join(".git"),
            }),
            remote: Some("https://github.com/sst/opencode.git".to_string()),
            roots: vec!["cafebabe".to_string()],
        };
        let resolved = resolve(&git, &dir);
        assert_eq!(resolved.previous.as_deref(), Some("previous-id"));
        assert_eq!(resolved.id, hash_fast("git-remote:github.com/sst/opencode"));
    }

    #[test]
    fn resolve_prefers_cache_over_root() {
        let dir = fake_dir();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/opencode"), "cached-id").unwrap();
        let git = FakeGit {
            repo: Some(Repository {
                worktree: dir.clone(),
                git_directory: dir.join(".git"),
                common_directory: dir.join(".git"),
            }),
            remote: None,
            roots: vec!["cafebabe".to_string()],
        };
        assert_eq!(resolve(&git, &dir).id, "cached-id");
    }

    #[test]
    fn resolve_non_git_directory_is_global_with_fs_root() {
        let git = FakeGit {
            repo: None,
            remote: None,
            roots: Vec::new(),
        };
        let resolved = resolve(&git, Path::new("/some/dir"));
        assert_eq!(resolved.id, GLOBAL_ID);
        assert_eq!(resolved.directory, PathBuf::from("/"));
        assert_eq!(resolved.vcs, None);
        assert_eq!(resolved.previous, None);
    }

    #[test]
    fn commit_writes_the_id_file() {
        let dir = fake_dir();
        commit(&dir, "abc123");
        assert_eq!(
            std::fs::read(dir.join("opencode")).unwrap(),
            b"abc123".to_vec(),
            "no trailing newline"
        );
    }
}
