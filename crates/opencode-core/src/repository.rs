//! Repository reference parsing + the clone cache — port of
//! `packages/core/src/repository.ts` and `packages/core/src/repository-cache.ts`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::git::{GitRunner, Repository};

/// A parsed remote reference (`repository.ts:15-17`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteReference {
    pub host: String,
    pub path: String,
    pub segments: Vec<String>,
    pub owner: Option<String>,
    pub repo: String,
    pub remote: String,
    pub label: String,
    pub protocol: Option<String>,
}

/// A parsed `file:` reference (`repository.ts:19-23`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReference {
    pub host: String,
    pub path: String,
    pub segments: Vec<String>,
    pub repo: String,
    pub remote: String,
    pub label: String,
    pub protocol: String,
}

/// `Reference` (`repository.ts:24`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    Remote(RemoteReference),
    File(FileReference),
}

/// `parseRemote` failures (`repository.ts:26-55`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("RepositoryInvalidReferenceError: {repository:?}, {message:?}")]
    Invalid { repository: String, message: String },
    #[error("RepositoryUnsupportedLocalRepositoryError: {repository:?}, {message:?}")]
    UnsupportedLocal { repository: String, message: String },
}

impl ParseError {
    pub fn message(&self) -> &str {
        match self {
            ParseError::Invalid { message, .. } => message,
            ParseError::UnsupportedLocal { message, .. } => message,
        }
    }
}

const INVALID_REFERENCE: &str =
    "Repository must be a git URL, host/path reference, or GitHub owner/repo shorthand";
const UNSUPPORTED_LOCAL: &str = "Local file repositories are not supported";

/// `parse` (`repository.ts:57-86`).
pub fn parse(input: &str) -> Option<Reference> {
    let cleaned = normalize_input(input);
    if cleaned.is_empty() {
        return None;
    }

    static GITHUB_PREFIXED: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^github:([^/\s]+)/([^/\s]+)$").unwrap());
    if let Some(caps) = GITHUB_PREFIXED.captures(&cleaned) {
        return build_remote(
            "github.com",
            &[caps[1].to_string(), caps[2].to_string()],
            None,
            None,
        )
        .map(Reference::Remote);
    }

    if !cleaned.contains("://") {
        static SCP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"^(?:[^@/\s]+@)?([^:/\s]+):(.+)$").unwrap()
        });
        if let Some(caps) = SCP.captures(&cleaned) {
            return build_remote(&caps[1], &parts(&caps[2]), Some(&cleaned), None)
                .map(Reference::Remote);
        }

        let direct = parts(&cleaned);
        if direct.len() >= 2 && host_like(&direct[0]) {
            let host = direct[0].clone();
            return build_remote(&host, &direct[1..], None, None).map(Reference::Remote);
        }
        if direct.len() == 2 {
            return build_remote("github.com", &direct, None, None).map(Reference::Remote);
        }
    }

    match parse_url(&cleaned) {
        Some((protocol, host, pathname)) => {
            if protocol == "file:" {
                return build_file(&cleaned, &pathname).map(Reference::File);
            }
            let segments = parts(&pathname);
            let remote = if host == "github.com" {
                github_remote(&segments.join("/"))
            } else {
                cleaned.clone()
            };
            build_remote(&host, &segments, Some(&remote), Some(protocol.as_str()))
                .map(Reference::Remote)
        }
        None => None,
    }
}

/// `parseRemote` (`repository.ts:88-103`).
pub fn parse_remote(input: &str) -> Result<RemoteReference, ParseError> {
    match parse(input) {
        Some(Reference::Remote(reference)) => Ok(reference),
        Some(Reference::File(_)) => Err(ParseError::UnsupportedLocal {
            repository: input.to_string(),
            message: UNSUPPORTED_LOCAL.to_string(),
        }),
        None => Err(ParseError::Invalid {
            repository: input.to_string(),
            message: INVALID_REFERENCE.to_string(),
        }),
    }
}

/// `validateBranch` (`repository.ts:105-111`).
pub fn validate_branch(branch: &str) -> Result<(), String> {
    static VALID: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^[A-Za-z0-9/_.-]+$").unwrap());
    if VALID.is_match(branch) && !branch.starts_with('-') && !branch.contains("..") {
        return Ok(());
    }
    Err(
        "Branch must contain only alphanumeric characters, /, _, ., and -, and cannot start with - or contain .."
            .to_string(),
    )
}

/// `isFile` / `isRemote` (`repository.ts:113-119`).
pub fn is_file(reference: &Reference) -> bool {
    matches!(reference, Reference::File(_))
}

pub fn is_remote(reference: &Reference) -> bool {
    !is_file(reference)
}

/// `cachePath` (`repository.ts:126-128`).
pub fn cache_path(root: &Path, reference: &Reference, branch: Option<&str>) -> PathBuf {
    let segments: Vec<String> = match reference {
        Reference::Remote(reference) => {
            let mut all = vec![reference.host.clone()];
            all.extend(reference.segments.iter().cloned());
            all
        }
        Reference::File(reference) => {
            let mut all = vec![reference.host.clone()];
            all.extend(reference.segments.iter().cloned());
            all
        }
    };
    // Hosts may carry a port (`host.split(":")` in TS); each part lands in
    // its own path segment.
    let mut parts: Vec<String> = Vec::new();
    for segment in segments {
        parts.extend(segment.split(':').map(str::to_string));
    }
    let mut path = root.to_path_buf();
    for part in parts {
        path.push(part);
    }
    match branch {
        Some(branch) => PathBuf::from(format!(
            "{}@{}",
            path.display(),
            encode_uri_component(branch)
        )),
        None => path,
    }
}

/// `cacheIdentity` (`repository.ts:131-133`).
pub fn cache_identity(reference: &Reference) -> String {
    let (host, path) = match reference {
        Reference::Remote(reference) => (reference.host.clone(), reference.path.clone()),
        Reference::File(reference) => (reference.host.clone(), reference.path.clone()),
    };
    format!("{host}/{path}")
}

/// `same` (`repository.ts:135-137`).
pub fn same(left: &Reference, right: &Reference) -> bool {
    cache_identity(left) == cache_identity(right)
}

fn normalize_input(input: &str) -> String {
    let input = input.trim();
    let input = input.strip_prefix("git+").unwrap_or(input);
    let input = match input.split_once('#') {
        Some((before, _)) => before,
        None => input,
    };
    input.trim_end_matches('/').to_string()
}

fn trim_git_suffix(input: &str) -> String {
    input.strip_suffix(".git").unwrap_or(input).to_string()
}

fn parts(input: &str) -> Vec<String> {
    input
        .split('/')
        .map(|item| trim_git_suffix(item.trim()))
        .filter(|item| !item.is_empty())
        .collect()
}

fn safe_host(input: &str) -> bool {
    !input.is_empty()
        && !input.starts_with('-')
        && !input
            .chars()
            .any(|c| c.is_whitespace() || c == '/' || c == '\\')
}

fn safe_segment(input: &str) -> bool {
    input != "."
        && input != ".."
        && !input.contains(':')
        && !input
            .chars()
            .any(|c| c.is_whitespace() || c == '/' || c == '\\')
}

fn host_like(input: &str) -> bool {
    input.contains('.') || input.contains(':') || input == "localhost"
}

fn with_slash(input: &str) -> String {
    if input.ends_with('/') {
        input.to_string()
    } else {
        format!("{input}/")
    }
}

fn github_remote(pathname: &str) -> String {
    match std::env::var("OPENCODE_REPO_CLONE_GITHUB_BASE_URL") {
        Ok(base) if !base.is_empty() => {
            let url = format!("{}{}.git", with_slash(&base), pathname);
            url
        }
        _ => format!("https://github.com/{pathname}.git"),
    }
}

fn build_remote(
    host: &str,
    segments: &[String],
    remote: Option<&str>,
    protocol: Option<&str>,
) -> Option<RemoteReference> {
    let segments: Vec<String> = segments
        .iter()
        .map(|item| trim_git_suffix(item))
        .filter(|item| !item.is_empty())
        .collect();
    if !safe_host(host) || segments.is_empty() || segments.iter().any(|s| !safe_segment(s)) {
        return None;
    }
    let repository_path = segments.join("/");
    let host = host.to_lowercase();
    Some(RemoteReference {
        remote: remote
            .map(str::to_string)
            .unwrap_or_else(|| match host.as_str() {
                "github.com" => github_remote(&repository_path),
                _ => format!("https://{host}/{repository_path}.git"),
            }),
        host: host.clone(),
        owner: if segments.len() == 2 {
            Some(segments[0].clone())
        } else {
            None
        },
        repo: segments[segments.len() - 1].clone(),
        label: if host == "github.com" && segments.len() == 2 {
            repository_path.clone()
        } else {
            format!("{host}/{repository_path}")
        },
        path: repository_path,
        segments,
        protocol: protocol.map(str::to_string),
    })
}

fn build_file(cleaned: &str, pathname: &str) -> Option<FileReference> {
    // `path.normalize(fileURLToPath(url))` — the `file:` URL pathname,
    // percent-decoded.
    let decoded = percent_decode(pathname);
    let file_path = PathBuf::from(decoded);
    let normalized = crate::git::lexical_normalize(&file_path);
    let file_path = normalized.to_string_lossy().into_owned();
    let segments: Vec<String> = file_path
        .split(['/', '\\'])
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.to_string())
        .collect();
    if segments.is_empty() {
        return None;
    }
    Some(FileReference {
        repo: trim_git_suffix(&segments[segments.len() - 1]),
        host: "file".to_string(),
        remote: cleaned.to_string(),
        segments: segments
            .iter()
            .map(|segment| segment.strip_suffix(':').unwrap_or(segment).to_string())
            .collect(),
        path: file_path.clone(),
        label: file_path,
        protocol: "file:".to_string(),
    })
}

/// Minimal `new URL(value)` for the shapes `parse` feeds it: returns
/// `(protocol, host, pathname)` where `host` keeps its port (the JS `host`
/// property).
fn parse_url(value: &str) -> Option<(String, String, String)> {
    let (protocol, rest) = value.split_once("://")?;
    if protocol.is_empty()
        || rest.is_empty()
        || !protocol
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        || !protocol
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    {
        return None;
    }
    let (authority, path) = match rest.split_once('/') {
        // `new URL` keeps the empty authority of `file:` URLs.
        Some((authority, path)) if !authority.is_empty() => {
            (authority.to_string(), format!("/{path}"))
        }
        Some(_) => (String::new(), format!("/{rest}")),
        // `new URL("https://host").pathname` is `/`.
        None => (rest.to_string(), "/".to_string()),
    };
    let host = authority
        .split('@')
        .next_back()
        .unwrap_or(&authority)
        .to_string();
    if host.is_empty() && protocol != "file" {
        return None;
    }
    Some((format!("{protocol}:"), host, path))
}

/// `encodeURIComponent` — everything but
/// `A-Z a-z 0-9 - _ . ! ~ * ' ( )`.
pub(crate) fn encode_uri_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(value) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// RepositoryCache (`repository-cache.ts`)
// ---------------------------------------------------------------------------

/// `EnsureInput` (`repository-cache.ts:28-32`).
pub struct EnsureInput<'a> {
    pub reference: &'a RemoteReference,
    pub refresh: bool,
    pub branch: Option<&'a str>,
}

/// `Result` (`repository-cache.ts:18-26`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheResult {
    pub repository: String,
    pub host: String,
    pub remote: String,
    pub local_path: PathBuf,
    pub status: &'static str,
    pub head: Option<String>,
    pub branch: Option<String>,
}

/// The tagged error set (`repository-cache.ts:34-86`) — every variant maps
/// to a `message` for the reference materialization warning path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("RepositoryCache{tag}: {message}")]
pub struct CacheError {
    pub tag: &'static str,
    pub message: String,
}

impl CacheError {
    fn new(tag: &'static str, message: impl Into<String>) -> CacheError {
        CacheError {
            tag,
            message: message.into(),
        }
    }
}

/// `RepositoryCache.Service` — tracking checkouts under the repos dir
/// (`repository-cache.ts:98-241`). TS serializes via `EffectFlock`
/// (cross-process); the Rust port keys a process-wide lock per cache path.
pub struct RepositoryCache {
    git: Arc<dyn GitRunner>,
    repos_dir: PathBuf,
}

impl RepositoryCache {
    pub fn new(git: Arc<dyn GitRunner>, repos_dir: PathBuf) -> RepositoryCache {
        RepositoryCache { git, repos_dir }
    }

    fn git(&self, cwd: Option<&Path>, args: &[&str]) -> crate::git::GitResult {
        self.git.run(cwd, args)
    }

    /// `ensure` (`repository-cache.ts:141-238`).
    pub fn ensure(&self, input: EnsureInput<'_>) -> Result<CacheResult, CacheError> {
        if let Some(branch) = input.branch {
            validate_branch(branch)
                .map_err(|message| CacheError::new("InvalidBranchError", message))?;
        }

        let repository = input.reference.label.clone();
        let local_path = cache_path(
            &self.repos_dir,
            &Reference::Remote(input.reference.clone()),
            input.branch,
        );
        let clone_target = parse(&input.reference.remote)
            .unwrap_or_else(|| Reference::Remote(input.reference.clone()));

        let _guard = keyed_lock(&local_path);
        if let Some(parent) = local_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| CacheError::new("CacheOperationError", format!("{err:?}")))?;
        }

        let existing = self.git.discover(&local_path);
        let origin = existing
            .as_ref()
            .and_then(|repo| self.git.remote_get(repo, "origin"));
        let origin_reference = origin.as_deref().and_then(parse);
        // Discovery walks upward, so an enclosing repository with a
        // matching origin could masquerade as the cache entry; reuse
        // requires the checkout to live exactly at the cache path.
        let reuse = existing
            .as_ref()
            .is_some_and(|repo| repo.worktree == resolve_local(&local_path))
            && origin_reference
                .as_ref()
                .is_some_and(|reference| same(reference, &clone_target));
        if !reuse && local_path.exists() {
            std::fs::remove_dir_all(&local_path)
                .or_else(|_| std::fs::remove_file(&local_path))
                .map_err(|err| CacheError::new("CacheOperationError", format!("{err:?}")))?;
        }

        let status: &'static str = if !reuse {
            "cloned"
        } else if input.refresh {
            "refreshed"
        } else {
            "cached"
        };

        if status == "cloned" {
            self.clone(&input, &local_path)?;
        }

        if status == "refreshed" {
            self.refresh(&input, existing.as_ref())?;
        }

        let checkout = self.git.discover(&local_path);
        Ok(CacheResult {
            repository,
            host: input.reference.host.clone(),
            remote: input.reference.remote.clone(),
            head: checkout.as_ref().and_then(|repo| self.history_head(repo)),
            branch: checkout.as_ref().and_then(|repo| self.history_branch(repo)),
            local_path,
            status,
        })
    }

    /// `git.repo.clone` (`git.ts:261-289`).
    fn clone(&self, input: &EnsureInput<'_>, local_path: &Path) -> Result<(), CacheError> {
        let mut args: Vec<String> = vec!["clone".into(), "--depth".into(), "100".into()];
        if let Some(branch) = input.branch {
            args.push("--branch".into());
            args.push(branch.to_string());
        }
        args.push("--".into());
        args.push(input.reference.remote.clone());
        args.push(local_path.to_string_lossy().into_owned());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let cwd = local_path.parent();
        let result = self.git(cwd, &args);
        if result.exit_code != 0 {
            let message = result
                .stderr
                .trim()
                .to_string()
                .is_empty()
                .then(|| result.text.trim().to_string())
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "Git clone failed".to_string());
            return Err(CacheError::new("CloneFailedError", message));
        }
        Ok(())
    }

    /// The `refreshed` branch (`repository-cache.ts:186-217`).
    fn refresh(
        &self,
        input: &EnsureInput<'_>,
        existing: Option<&Repository>,
    ) -> Result<(), CacheError> {
        let existing = existing.ok_or_else(|| {
            CacheError::new("FetchFailedError", "Repository is unavailable".to_string())
        })?;
        let result = self.git(Some(&existing.worktree), &["fetch", "--all", "--prune"]);
        if result.exit_code != 0 {
            return Err(CacheError::new(
                "FetchFailedError",
                result.stderr.trim().to_string(),
            ));
        }
        if let Some(branch) = input.branch {
            let spec = format!("refs/heads/{branch}:refs/remotes/origin/{branch}");
            let result = self.git(
                Some(&existing.worktree),
                &["fetch", "origin", &format!("+{spec}")],
            );
            if result.exit_code != 0 {
                return Err(CacheError::new(
                    "FetchFailedError",
                    result.stderr.trim().to_string(),
                ));
            }
        }
        let branch = match input.branch {
            Some(branch) => Some(branch.to_string()),
            None => self.default_remote_branch(existing),
        };
        if let Some(branch) = branch.clone() {
            let result = self.git(
                Some(&existing.worktree),
                &["checkout", "-B", &branch, &format!("origin/{branch}")],
            );
            if result.exit_code != 0 {
                return Err(CacheError::new(
                    "CheckoutFailedError",
                    result.stderr.trim().to_string(),
                ));
            }
        }
        let target = branch
            .or_else(|| self.history_branch(existing))
            .map(|branch| format!("origin/{branch}"))
            .unwrap_or_else(|| "HEAD".to_string());
        let result = self.git(Some(&existing.worktree), &["reset", "--hard", &target]);
        if result.exit_code != 0 {
            return Err(CacheError::new(
                "ResetFailedError",
                result.stderr.trim().to_string(),
            ));
        }
        Ok(())
    }

    /// `history.defaultRemoteBranch` (`git.ts:236-244`).
    fn default_remote_branch(&self, repository: &Repository) -> Option<String> {
        let result = self.git.run(
            Some(&repository.worktree),
            &["symbolic-ref", "refs/remotes/origin/HEAD"],
        );
        if result.exit_code != 0 {
            return None;
        }
        let value = result.text.trim();
        value
            .strip_prefix("refs/remotes/origin/")
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    /// `history.branch` (`git.ts:227-233`).
    fn history_branch(&self, repository: &Repository) -> Option<String> {
        let result = self.git.run(
            Some(&repository.worktree),
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
        );
        if result.exit_code != 0 {
            return None;
        }
        let value = result.text.trim();
        (!value.is_empty()).then(|| value.to_string())
    }

    /// `history.head` (`git.ts:221-225`).
    fn history_head(&self, repository: &Repository) -> Option<String> {
        let result = self
            .git
            .run(Some(&repository.worktree), &["rev-parse", "HEAD"]);
        if result.exit_code != 0 {
            return None;
        }
        let value = result.text.trim();
        (!value.is_empty()).then(|| value.to_string())
    }
}

/// `FSUtil.resolve` — `path.resolve` then realpath when the path exists.
fn resolve_local(path: &Path) -> PathBuf {
    let resolved = crate::git::lexical_normalize(path);
    match std::fs::canonicalize(&resolved) {
        Ok(real) => real,
        Err(_) => resolved,
    }
}

/// Per-cache-path lock (`EffectFlock.withLock`) — process-wide keyed mutex.
fn keyed_lock(path: &Path) -> std::sync::MutexGuard<'static, ()> {
    static LOCKS: std::sync::LazyLock<Mutex<std::collections::HashMap<PathBuf, Arc<Mutex<()>>>>> =
        std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
    let mut locks = LOCKS.lock().unwrap_or_else(|p| p.into_inner());
    let lock = locks
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    drop(locks);
    // Cache paths live for the process, so leaking the handle is fine.
    let leaked: &'static mut Arc<Mutex<()>> = Box::leak(Box::new(lock));
    leaked.lock().unwrap_or_else(|p| p.into_inner())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_support::TempDir;
    use std::process::Command;

    #[test]
    fn parse_github_shorthand() {
        let reference = parse_remote("github:sst/opencode").unwrap();
        assert_eq!(reference.host, "github.com");
        assert_eq!(reference.path, "sst/opencode");
        assert_eq!(reference.segments, vec!["sst", "opencode"]);
        assert_eq!(reference.owner.as_deref(), Some("sst"));
        assert_eq!(reference.repo, "opencode");
        assert_eq!(reference.remote, "https://github.com/sst/opencode.git");
        assert_eq!(reference.label, "sst/opencode");
        assert_eq!(reference.protocol, None);
    }

    #[test]
    fn parse_owner_repo_shorthand_defaults_to_github() {
        let reference = parse_remote("sst/opencode").unwrap();
        assert_eq!(reference.host, "github.com");
        assert_eq!(reference.remote, "https://github.com/sst/opencode.git");
    }

    #[test]
    fn parse_https_url() {
        let reference = parse_remote("https://git.example.com/team/repo.git").unwrap();
        assert_eq!(reference.host, "git.example.com");
        assert_eq!(reference.segments, vec!["team", "repo"]);
        assert_eq!(
            reference.remote, "https://git.example.com/team/repo.git",
            "non-github hosts keep the cleaned input as the remote"
        );
        assert_eq!(reference.label, "git.example.com/team/repo");
        assert_eq!(reference.protocol.as_deref(), Some("https:"));
    }

    #[test]
    fn parse_github_url_uses_github_remote() {
        let reference = parse_remote("https://github.com/sst/opencode").unwrap();
        assert_eq!(reference.remote, "https://github.com/sst/opencode.git");
    }

    #[test]
    fn parse_scp_like() {
        let input = "git@gitlab.com:group/project.git";
        let reference = parse_remote(input).unwrap();
        assert_eq!(reference.host, "gitlab.com");
        assert_eq!(reference.segments, vec!["group", "project"]);
        assert_eq!(reference.remote, input);
    }

    #[test]
    fn parse_host_path() {
        let reference = parse_remote("git.sr.ht/~user/repo").unwrap();
        assert_eq!(reference.host, "git.sr.ht");
        assert_eq!(reference.segments, vec!["~user", "repo"]);
    }

    #[test]
    fn parse_file_url() {
        let reference = parse("file:///tmp/repo").unwrap();
        assert_eq!(
            reference,
            Reference::File(FileReference {
                host: "file".to_string(),
                path: "/tmp/repo".to_string(),
                segments: vec!["tmp".to_string(), "repo".to_string()],
                repo: "repo".to_string(),
                remote: "file:///tmp/repo".to_string(),
                label: "/tmp/repo".to_string(),
                protocol: "file:".to_string(),
            })
        );
        assert!(is_file(&reference));
        assert!(!is_remote(&reference));
        assert!(parse_remote("file:///tmp/repo").is_err());
    }

    #[test]
    fn parse_rejects_unsafe_segments() {
        assert!(parse("github:../etc/passwd").is_none());
        assert!(parse("github:sst/opencode/../../x").is_none());
        assert!(parse("github.com/a/b:c").is_none(), "colon in segment");
    }

    #[test]
    fn parse_normalizes_input() {
        let reference = parse_remote("git+https://github.com/sst/opencode.git#main").unwrap();
        assert_eq!(reference.host, "github.com");
        assert_eq!(reference.segments, vec!["sst", "opencode"]);
    }

    #[test]
    fn parse_invalid() {
        assert!(parse("").is_none());
        assert!(parse("   ").is_none());
        assert!(parse(":::").is_none());
        let err = parse_remote("not a repo").unwrap_err();
        assert!(matches!(err, ParseError::Invalid { .. }));
        assert_eq!(err.message(), INVALID_REFERENCE);
    }

    #[test]
    fn validate_branch_matrix() {
        assert!(validate_branch("main").is_ok());
        assert!(validate_branch("release/2026-09").is_ok());
        assert!(validate_branch("a.b_c-d").is_ok());
        assert!(validate_branch("-start").is_err());
        assert!(validate_branch("a..b").is_err());
        assert!(validate_branch("has space").is_err());
    }

    #[test]
    fn cache_paths() {
        let reference = parse_remote("github:sst/opencode").unwrap();
        assert_eq!(
            cache_path(
                Path::new("/repos"),
                &Reference::Remote(reference.clone()),
                None
            ),
            PathBuf::from("/repos/github.com/sst/opencode")
        );
        assert_eq!(
            cache_path(
                Path::new("/repos"),
                &Reference::Remote(reference.clone()),
                Some("feat/x")
            ),
            PathBuf::from("/repos/github.com/sst/opencode@feat%2Fx"),
        );
        assert_eq!(
            cache_identity(&Reference::Remote(reference)),
            "github.com/sst/opencode"
        );
    }

    #[test]
    fn same_compares_identity() {
        let left = parse_remote("github:sst/opencode").unwrap();
        let right = parse_remote("https://github.com/sst/opencode.git").unwrap();
        assert!(same(&Reference::Remote(left), &Reference::Remote(right)));
    }

    // -------------------------------------------------------------------
    // RepositoryCache against real git
    // -------------------------------------------------------------------

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn make_repo(dir: &Path) {
        git(dir, &["init", "--quiet", "-b", "main"]);
        std::fs::write(dir.join("file.txt"), "one\n").unwrap();
        git(dir, &["add", "file.txt"]);
        git(
            dir,
            &[
                "-c",
                "user.email=t@e.st",
                "-c",
                "user.name=T",
                "commit",
                "--quiet",
                "-m",
                "one",
            ],
        );
    }

    #[test]
    fn cache_clones_and_refreshes() {
        let source = TempDir::new("cache-source");
        make_repo(source.path());

        let cache_root = TempDir::new("cache-root");
        let cache = RepositoryCache::new(
            Arc::new(crate::git::SubprocessGit),
            cache_root.path().to_path_buf(),
        );
        // A direct remote pointing at the local filesystem — parse() never
        // produces it, but ensure() only shells out to `git clone <remote>`.
        let reference = RemoteReference {
            host: "example.com".into(),
            path: "a/b".into(),
            segments: vec!["a".into(), "b".into()],
            owner: Some("a".into()),
            repo: "b".into(),
            remote: source.path().to_string_lossy().into_owned(),
            label: "example.com/a/b".into(),
            protocol: None,
        };

        let result = cache
            .ensure(EnsureInput {
                reference: &reference,
                refresh: false,
                branch: None,
            })
            .unwrap();
        assert_eq!(result.status, "cloned");
        assert!(result.local_path.ends_with("example.com/a/b"));
        assert!(result.head.is_some());
        assert_eq!(result.branch.as_deref(), Some("main"));
        assert!(result.local_path.join("file.txt").exists());

        let result = cache
            .ensure(EnsureInput {
                reference: &reference,
                refresh: true,
                branch: None,
            })
            .unwrap();
        assert_eq!(result.status, "refreshed");

        let result = cache
            .ensure(EnsureInput {
                reference: &reference,
                refresh: false,
                branch: None,
            })
            .unwrap();
        assert_eq!(result.status, "cached");
    }

    #[test]
    fn cache_reclones_when_origin_changes() {
        let source = TempDir::new("cache-source2");
        make_repo(source.path());

        let cache_root = TempDir::new("cache-root2");
        let cache = RepositoryCache::new(
            Arc::new(crate::git::SubprocessGit),
            cache_root.path().to_path_buf(),
        );
        let reference = RemoteReference {
            host: "example.com".into(),
            path: "a/b".into(),
            segments: vec!["a".into(), "b".into()],
            owner: Some("a".into()),
            repo: "b".into(),
            remote: source.path().to_string_lossy().into_owned(),
            label: "example.com/a/b".into(),
            protocol: None,
        };
        cache
            .ensure(EnsureInput {
                reference: &reference,
                refresh: false,
                branch: None,
            })
            .unwrap();

        // A different origin at the same cache key forces a re-clone.
        let origin = cache_root.path().join("example.com").join("a").join("b");
        git(
            &origin,
            &[
                "-c",
                "safe.directory=*",
                "remote",
                "set-url",
                "origin",
                "https://example.com/moved/elsewhere",
            ],
        );
        let result = cache
            .ensure(EnsureInput {
                reference: &reference,
                refresh: false,
                branch: None,
            })
            .unwrap();
        assert_eq!(result.status, "cloned");
    }

    #[test]
    fn cache_rejects_invalid_branch() {
        let cache_root = TempDir::new("cache-root3");
        let cache = RepositoryCache::new(
            Arc::new(crate::git::SubprocessGit),
            cache_root.path().to_path_buf(),
        );
        let reference = RemoteReference {
            host: "example.com".into(),
            path: "a/b".into(),
            segments: vec!["a".into(), "b".into()],
            owner: Some("a".into()),
            repo: "b".into(),
            remote: "/does/not/exist".into(),
            label: "example.com/a/b".into(),
            protocol: None,
        };
        let error = cache
            .ensure(EnsureInput {
                reference: &reference,
                refresh: false,
                branch: Some("has space"),
            })
            .unwrap_err();
        assert_eq!(error.tag, "InvalidBranchError");
    }
}
