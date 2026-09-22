//! Ripgrep service — port of `packages/core/src/ripgrep.ts` (spec M4.2).
//!
//! TS shells out to a bundled `rg` binary; the Rust port implements the same
//! search/listing semantics with the `ignore` walker (which is also what
//! ripgrep uses), so gitignore handling, glob overrides (`--glob`,
//! incl. `!`-negations), hidden filtering and symlink skipping match
//! character-for-character. Never shell out to `rg`.
//!
//! This module also hosts the TS `path` helpers (`resolve`, `relative`,
//! `dirname`, `basename`) and the UTF-16 truncation helpers shared by the
//! M4.2 tools.

use std::path::{Component, Path, PathBuf};

use ignore::overrides::OverrideBuilder;
use ignore::DirEntry;
use ignore::WalkBuilder;

/// One grep hit — the slice of TS `Match` the tools render
/// (`path` cwd-relative, `text` includes the line terminator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    pub path: String,
    pub line: u64,
    pub text: String,
}

/// `Ripgrep.Service` (ripgrep.ts:79-83).
pub trait Ripgrep: Send + Sync {
    /// Line matches; respects .gitignore via the `ignore` crate; `include`
    /// is a glob filter (`--glob`); collects at most `limit` matches. Paths
    /// are cwd-relative. Invalid regex patterns error (TS
    /// `InvalidPatternError`).
    fn grep(
        &self,
        cwd: &Path,
        pattern: &str,
        include: Option<&str>,
        limit: usize,
    ) -> Result<Vec<GrepMatch>, String>;
    /// Glob listing, stops after `limit` files (an extra entry is consumed
    /// so callers can distinguish "exactly limit" from "at least limit" the
    /// way `Stream.take(limit + 1)` does in TS); paths are cwd-relative.
    fn glob(&self, cwd: &Path, pattern: &str, limit: usize) -> Vec<PathBuf>;
    /// Walkdir honoring an inverse-glob pattern (e.g. `"!**/SKILL.md"`),
    /// hidden + !follow, `limit` results.
    fn find(
        &self,
        cwd: &Path,
        pattern: &str,
        hidden: bool,
        follow: bool,
        limit: usize,
    ) -> Vec<PathBuf>;
}

/// Default ripgrep adapter over the local filesystem.
#[derive(Debug, Clone, Default)]
pub struct RipgrepService;

/// `--glob=!**/.git/**` is passed on every invocation (ripgrep.ts:164, 198,
/// 227).
const GIT_GLOB: &str = "!**/.git/**";

/// TS caps the match text at 2 000 UTF-16 units (ripgrep.ts:268-270).
const MATCH_TEXT_MAX: usize = 2_000;

impl Ripgrep for RipgrepService {
    fn grep(
        &self,
        cwd: &Path,
        pattern: &str,
        include: Option<&str>,
        limit: usize,
    ) -> Result<Vec<GrepMatch>, String> {
        let regex = regex::Regex::new(pattern).map_err(|e| e.to_string())?;
        let mut globs: Vec<&str> = Vec::new();
        if let Some(include) = include {
            globs.push(include);
        }
        globs.push(GIT_GLOB);
        // rg --json --hidden: hidden files are searched.
        let mut out: Vec<GrepMatch> = Vec::new();
        'walk: for entry in walk(cwd, &globs, false, false) {
            let relative = match relative_to(cwd, entry.path()) {
                Some(relative) => relative,
                None => continue,
            };
            let bytes = match std::fs::read(entry.path()) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            for (number, text, content) in lines_with_terminators(bytes.as_slice()) {
                if regex.is_match(content.as_str()) {
                    let text = match truncate_utf16(&text, MATCH_TEXT_MAX) {
                        Some(truncated) => format!("{truncated}..."),
                        None => text,
                    };
                    out.push(GrepMatch {
                        path: relative.clone(),
                        line: number,
                        text,
                    });
                    if out.len() > limit {
                        break 'walk;
                    }
                }
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    fn glob(&self, cwd: &Path, pattern: &str, limit: usize) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in walk(cwd, &[pattern, GIT_GLOB], true, false) {
            out.push(match relative_to(cwd, entry.path()) {
                Some(relative) => PathBuf::from(relative),
                None => continue,
            });
            if out.len() > limit {
                break;
            }
        }
        out.truncate(limit);
        out
    }

    fn find(
        &self,
        cwd: &Path,
        pattern: &str,
        hidden: bool,
        follow: bool,
        limit: usize,
    ) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let globs: Vec<&str> = if pattern == "*" {
            vec![GIT_GLOB]
        } else {
            vec![pattern, GIT_GLOB]
        };
        // TS `hidden` = rg's `--hidden` flag (include hidden files).
        for entry in walk(cwd, &globs, !hidden, follow) {
            out.push(match relative_to(cwd, entry.path()) {
                Some(relative) => PathBuf::from(relative),
                None => continue,
            });
            if out.len() > limit {
                break;
            }
        }
        out.truncate(limit);
        out
    }
}

/// Walk `cwd` yielding files only: `.gitignore` respected, globs applied as
/// `--glob` overrides, `skip_hidden = true` skips hidden files (the rg
/// default) and symlinks are never returned unless `follow` resolves them
/// (rg skips symlinks during traversal).
fn walk(
    cwd: &Path,
    globs: &[&str],
    skip_hidden: bool,
    follow: bool,
) -> Box<dyn Iterator<Item = DirEntry>> {
    let mut builder = WalkBuilder::new(cwd);
    builder.hidden(skip_hidden);
    builder.follow_links(follow);
    if !globs.is_empty() {
        let mut overrides = OverrideBuilder::new(cwd);
        for glob in globs {
            // rg fails on a malformed glob; the walk is simply empty then.
            if overrides.add(glob).is_err() {
                return Box::new(std::iter::empty());
            }
        }
        if let Ok(overrides) = overrides.build() {
            builder.overrides(overrides);
        }
    }
    Box::new(
        builder
            .build()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_type()
                    .is_some_and(|ty| !ty.is_dir() && !ty.is_symlink())
            }),
    )
}

/// Cwd-relative path with `/` separators (TS strips the leading `./` rg
/// emits).
fn relative_to(cwd: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(cwd)
        .ok()
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
}

/// Split raw bytes into `(1-based line number, text with terminator, text
/// without terminator)`. Terminators are `\n` and `\r\n` (rg line
/// semantics); the text kept for rendering includes the terminator.
fn lines_with_terminators(bytes: &[u8]) -> impl Iterator<Item = (u64, String, String)> + '_ {
    let rest = bytes;
    let mut number = 0u64;
    let mut start = 0usize;
    std::iter::from_fn(move || {
        if start >= rest.len() {
            return None;
        }
        number += 1;
        let newline = rest[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|pos| start + pos);
        let end = newline.unwrap_or(rest.len());
        let end_with_terminator = match newline {
            Some(_) => end + 1,
            None => rest.len(),
        };
        let content_end = if rest[start..end].last() == Some(&b'\r') {
            end - 1
        } else {
            end
        };
        let decode = |slice: &[u8]| -> String { String::from_utf8_lossy(slice).to_string() };
        let text = decode(&rest[start..end_with_terminator]);
        let content = decode(&rest[start..content_end]);
        start = end + 1;
        Some((number, text, content))
    })
}

/// UTF-16 length of `text` (JS `String.length`).
pub(crate) fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// TS `text.slice(0, max)` — truncate to `max` UTF-16 code units, dropping a
/// trailing char that would split a surrogate pair (JS strips the lone
/// trailing high surrogate). Returns `None` when the text already fits.
pub(crate) fn truncate_utf16(text: &str, max: usize) -> Option<String> {
    let mut units = 0usize;
    for (index, ch) in text.char_indices() {
        units += ch.len_utf16();
        if units > max {
            return Some(text[..index].to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// TS `path` helpers shared by the M4.2 tools.
// ---------------------------------------------------------------------------

/// `path.resolve(base, p)` — lexical normalization; absolute `p` wins.
pub(crate) fn ts_resolve(base: &Path, p: &str) -> PathBuf {
    let joined = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        base.join(p)
    };
    normalize(&joined)
}

/// Lexically normalize a path (resolve `.` and `..` without touching the
/// filesystem, like `path.resolve`).
fn normalize(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::ParentDir) | None => out.push(component),
                _ => {} // "/.." resolves to "/"
            },
            _ => out.push(component),
        }
    }
    let mut normalized = PathBuf::new();
    for component in out {
        normalized.push(component.as_os_str());
    }
    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }
    normalized
}

/// `path.relative(from, to)` (unix semantics).
pub(crate) fn ts_relative(from: &Path, to: &Path) -> String {
    let from: Vec<Component> = from
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    let to: Vec<Component> = to
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    let mut common = 0;
    while common < from.len() && common < to.len() && from[common] == to[common] {
        common += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in 0..from.len() - common {
        parts.push("..".to_string());
    }
    for component in &to[common..] {
        parts.push(component.as_os_str().to_string_lossy().to_string());
    }
    parts.join("/")
}

/// `path.dirname` — `"."` for bare filenames.
pub(crate) fn ts_dirname(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// `path.basename`.
pub(crate) fn ts_basename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared fixtures for the M4.2 tool tests: a recording `Ask`/`Metadata`
    //! sink, a stub `Agents` and a `ToolCtxRef` builder.

    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use crate::tool::def::{
        AgentInfo, AgentMode, Agents, Ask, BoxFuture, Extra, InstanceContext, MetadataSink,
        ToolCtxRef,
    };
    use crate::tool::error::ToolError;
    use crate::tool::permission::Ruleset;

    /// `ctx.ask` + `ctx.metadata` — records every request.
    #[derive(Default)]
    pub struct RecordingAsk {
        pub requests: Mutex<Vec<crate::tool::def::AskRequest>>,
        pub metadata_calls: Mutex<Vec<crate::tool::def::MetadataInput>>,
    }

    impl RecordingAsk {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn requests(&self) -> Vec<crate::tool::def::AskRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Ask for RecordingAsk {
        fn ask<'a>(
            &'a self,
            request: crate::tool::def::AskRequest,
        ) -> BoxFuture<'a, Result<(), ToolError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request);
                Ok(())
            })
        }
    }

    impl MetadataSink for RecordingAsk {
        fn metadata<'a>(
            &'a self,
            input: crate::tool::def::MetadataInput,
        ) -> BoxFuture<'a, Result<(), ToolError>> {
            Box::pin(async move {
                self.metadata_calls.lock().unwrap().push(input);
                Ok(())
            })
        }
    }

    /// Minimal `Agents` stub returning a fixed agent.
    pub struct FixedAgents;

    impl Agents for FixedAgents {
        fn get<'a>(&'a self, _agent: &'a str) -> BoxFuture<'a, Result<AgentInfo, ToolError>> {
            Box::pin(async move {
                Ok(AgentInfo {
                    name: "build".to_string(),
                    description: None,
                    mode: AgentMode::Primary,
                    permission: Ruleset::new(),
                })
            })
        }

        fn list<'a>(&'a self) -> BoxFuture<'a, Vec<AgentInfo>> {
            Box::pin(async move { Vec::new() })
        }
    }

    /// Per-invocation context bound to an instance + ask sink.
    pub fn ctx<'a>(
        ask: &'a RecordingAsk,
        instance: &'a InstanceContext,
        extra: &'a Extra,
    ) -> ToolCtxRef<'a> {
        ToolCtxRef {
            session_id: "ses_1",
            message_id: "msg_1",
            agent: "build",
            call_id: Some("cal_1"),
            abort: tokio_util::sync::CancellationToken::new(),
            messages: &[],
            extra,
            instance,
            ask,
            metadata: ask,
        }
    }

    pub fn instance(directory: &Path) -> InstanceContext {
        InstanceContext {
            directory: directory.to_path_buf(),
            worktree: directory.to_path_buf(),
        }
    }

    pub fn fixed_agents() -> Arc<FixedAgents> {
        Arc::new(FixedAgents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, contents: &str) {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn setup_git_repo(dir: &Path) {
        fs::create_dir(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/config"), "[core]\n").unwrap();
        write(dir, ".gitignore", "ignored.txt\n");
        write(dir, "ignored.txt", "ignored match\n");
        write(dir, "kept.txt", "kept match\n");
        write(dir, "src/lib.txt", "lib match\n");
        write(dir, "src/hidden.txt", "hidden match\n");
    }

    #[test]
    fn glob_whitelist_overrides_gitignore() {
        let temp = crate::storage::test_support::TempDir::new("ripgrep-glob");
        setup_git_repo(temp.path());

        // A positive --glob overrides gitignore AND the hidden filter (rg
        // semantics), so every non-.git txt file is listed.
        let mut files = RipgrepService.glob(temp.path(), "**/*.txt", 100);
        files.sort();
        assert_eq!(
            files,
            vec![
                PathBuf::from("ignored.txt"),
                PathBuf::from("kept.txt"),
                PathBuf::from("src/hidden.txt"),
                PathBuf::from("src/lib.txt"),
            ]
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[test]
    fn glob_overrides_gitignore_but_not_hidden_filter() {
        let temp = crate::storage::test_support::TempDir::new("ripgrep-glob-hidden");
        write(temp.path(), "visible.txt", "x");
        write(temp.path(), ".secret.txt", "x");
        write(temp.path(), "other.md", "x");

        let mut files = RipgrepService.glob(temp.path(), "*.txt", 100);
        // rg --files -g '*.txt' lists .secret.txt too (overrides beat the
        // hidden filter), but not other.md. (Sort first: glob returns
        // readdir order, which is filesystem-dependent.)
        files.sort();
        assert_eq!(
            files,
            vec![PathBuf::from(".secret.txt"), PathBuf::from("visible.txt")]
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[test]
    fn glob_never_lists_dot_git() {
        let temp = crate::storage::test_support::TempDir::new("ripgrep-gitdir");
        setup_git_repo(temp.path());

        let files = RipgrepService.glob(temp.path(), "**/*", 100);
        assert!(
            files.iter().all(|p| !p.to_string_lossy().contains(".git/")),
            "{files:?}"
        );
        assert!(files.contains(&PathBuf::from("kept.txt")));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[test]
    fn glob_limit_boundaries() {
        let temp = crate::storage::test_support::TempDir::new("ripgrep-limit");
        for i in 0..101 {
            write(temp.path(), &format!("f{i}.txt"), "x");
        }
        // 101 matches -> 100 returned.
        let files = RipgrepService.glob(temp.path(), "*.txt", 100);
        assert_eq!(files.len(), 100);
        std::fs::remove_dir_all(temp.path()).ok();

        let temp = crate::storage::test_support::TempDir::new("ripgrep-limit2");
        for i in 0..100 {
            write(temp.path(), &format!("f{i}.txt"), "x");
        }
        // exactly 100 matches -> still 100 (indistinguishable caller-side).
        let files = RipgrepService.glob(temp.path(), "*.txt", 100);
        assert_eq!(files.len(), 100);
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[test]
    fn find_supports_negation_glob() {
        let temp = crate::storage::test_support::TempDir::new("ripgrep-find");
        write(temp.path(), "SKILL.md", "x");
        write(temp.path(), "a/SKILL.md", "x");
        write(temp.path(), "a/other.txt", "x");

        let files = RipgrepService.find(temp.path(), "!**/SKILL.md", true, false, 10);
        assert_eq!(files, vec![PathBuf::from("a/other.txt")]);
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[test]
    fn find_star_lists_everything() {
        let temp = crate::storage::test_support::TempDir::new("ripgrep-find-star");
        write(temp.path(), "visible.txt", "x");
        write(temp.path(), ".hidden.txt", "x");
        write(temp.path(), "sub/deep.txt", "x");

        let mut files = RipgrepService.find(temp.path(), "*", true, false, 10);
        files.sort();
        assert_eq!(
            files,
            vec![
                PathBuf::from(".hidden.txt"),
                PathBuf::from("sub/deep.txt"),
                PathBuf::from("visible.txt"),
            ]
        );

        // Without `hidden`, dotfiles are skipped (rg default).
        let files = RipgrepService.find(temp.path(), "*", false, false, 10);
        assert!(!files.contains(&PathBuf::from(".hidden.txt")));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[test]
    fn grep_matches_with_terminators_and_gitignore() {
        let temp = crate::storage::test_support::TempDir::new("ripgrep-grep");
        setup_git_repo(temp.path());
        write(temp.path(), "crlf.txt", "windows match here\r\nsecond\r\n");

        let mut matches = RipgrepService
            .grep(temp.path(), "match", None, 100)
            .unwrap();
        matches.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            matches,
            vec![
                // src/hidden.txt is hidden but --hidden searches it; ignored.txt
                // is gitignored and excluded.
                GrepMatch {
                    path: "crlf.txt".to_string(),
                    line: 1,
                    text: "windows match here\r\n".to_string(),
                },
                GrepMatch {
                    path: "kept.txt".to_string(),
                    line: 1,
                    text: "kept match\n".to_string(),
                },
                GrepMatch {
                    path: "src/hidden.txt".to_string(),
                    line: 1,
                    text: "hidden match\n".to_string(),
                },
                GrepMatch {
                    path: "src/lib.txt".to_string(),
                    line: 1,
                    text: "lib match\n".to_string(),
                },
            ]
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[test]
    fn grep_include_filter_and_hidden() {
        let temp = crate::storage::test_support::TempDir::new("ripgrep-include");
        write(temp.path(), "a.js", "match\n");
        write(temp.path(), "b.ts", "match\n");
        write(temp.path(), ".hidden.js", "match\n");

        let mut matches = RipgrepService
            .grep(temp.path(), "match", Some("*.js"), 100)
            .unwrap();
        // (sort first: matches come in readdir order, which is
        // filesystem-dependent)
        matches.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            matches,
            vec![
                GrepMatch {
                    path: ".hidden.js".to_string(),
                    line: 1,
                    text: "match\n".to_string(),
                },
                GrepMatch {
                    path: "a.js".to_string(),
                    line: 1,
                    text: "match\n".to_string(),
                },
            ]
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[test]
    fn grep_skips_dot_git_and_invalid_regex() {
        let temp = crate::storage::test_support::TempDir::new("ripgrep-bad");
        setup_git_repo(temp.path());

        let matches = RipgrepService.grep(temp.path(), "core", None, 100).unwrap();
        // .git/config contains "core" but is excluded.
        assert!(matches.is_empty());

        let err = RipgrepService.grep(temp.path(), "(", None, 100);
        assert!(err.is_err());
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[test]
    fn grep_text_cap_and_limit() {
        let temp = crate::storage::test_support::TempDir::new("ripgrep-cap");
        let long = "x".repeat(2_100);
        write(temp.path(), "long.txt", &format!("{long} match\nmatch\n"));
        for i in 0..5 {
            write(temp.path(), &format!("m{i}.txt"), "match\n");
        }

        let matches = RipgrepService
            .grep(temp.path(), "match", None, 100)
            .unwrap();
        let first = matches
            .iter()
            .find(|m| m.path == "long.txt")
            .expect("long.txt matched");
        // 2000 UTF-16 units of the line + "..." — the " match" tail is cut.
        assert!(
            first.text.starts_with(&"x".repeat(2_000)),
            "{}",
            first.text.len()
        );
        assert!(first.text.ends_with("..."));

        let limited = RipgrepService.grep(temp.path(), "match", None, 3).unwrap();
        assert_eq!(limited.len(), 3);
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[test]
    fn grep_final_line_without_newline() {
        let temp = crate::storage::test_support::TempDir::new("ripgrep-eof");
        write(temp.path(), "a.txt", "no trailing newline match");
        let matches = RipgrepService
            .grep(temp.path(), "match", None, 100)
            .unwrap();
        assert_eq!(
            matches,
            vec![GrepMatch {
                path: "a.txt".to_string(),
                line: 1,
                text: "no trailing newline match".to_string(),
            }]
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[test]
    fn utf16_truncation() {
        assert_eq!(truncate_utf16("hello", 10), None);
        assert_eq!(truncate_utf16("hello", 5), None);
        assert_eq!(truncate_utf16("hello", 3), Some("hel".to_string()));
        // Astral chars count two UTF-16 units each.
        let astral = "\u{1F600}".repeat(4); // 4 chars, 8 UTF-16 units
        assert_eq!(truncate_utf16(&astral, 8), None);
        assert_eq!(truncate_utf16(&astral, 7), Some("\u{1F600}".repeat(3)));
        assert_eq!(utf16_len(&astral), 8);
    }

    #[test]
    fn ts_path_helpers() {
        assert_eq!(
            ts_resolve(Path::new("/base"), "a/../b"),
            PathBuf::from("/base/b")
        );
        assert_eq!(
            ts_resolve(Path::new("/base"), "/abs/./x"),
            PathBuf::from("/abs/x")
        );
        assert_eq!(ts_relative(Path::new("/a/b"), Path::new("/a/b/c")), "c");
        assert_eq!(ts_relative(Path::new("/a/b/c"), Path::new("/a/b")), "..");
        assert_eq!(ts_relative(Path::new("/a/b"), Path::new("/a/b")), "");
        assert_eq!(ts_relative(Path::new("/a"), Path::new("/a/b/c")), "b/c");
        assert_eq!(ts_dirname(Path::new("file.txt")), PathBuf::from("."));
        assert_eq!(ts_basename(Path::new("/a/b.txt")), "b.txt");
    }
}
