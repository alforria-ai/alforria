//! The per-instance vcs service — port of `packages/opencode/src/git/index.ts`
//! (the git CLI wrapper) and `packages/opencode/src/project/vcs.ts`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Serialize;

/// `Git.Item` (`git/index.ts:38-42`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub file: String,
    pub code: String,
    pub status: Kind,
}

/// `Git.Kind` (`git/index.ts:31`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Added,
    Deleted,
    Modified,
}

/// `Git.Base` (`git/index.ts:33-36`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub name: String,
    pub ref_: String,
}

/// `Git.Stat` (`git/index.ts:44-48`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub file: String,
    pub additions: i64,
    pub deletions: i64,
}

/// `Git.Patch` (`git/index.ts:50-53`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub text: String,
    pub truncated: bool,
}

/// `Vcs.Info` (`project/vcs.ts:240-244`) — optional fields are omitted from
/// the JSON when absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct VcsInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

/// `Vcs.FileStatus` (`project/vcs.ts:258-263`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VcsFileStatus {
    pub file: String,
    pub additions: i64,
    pub deletions: i64,
    pub status: Kind,
}

/// `Vcs.FileDiff` (`project/vcs.ts:246-255`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VcsFileDiff {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    pub additions: i64,
    pub deletions: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Kind>,
}

/// `Vcs.ApplyResult` (`project/vcs.ts:271-274`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VcsApplyResult {
    pub applied: bool,
}

/// `Vcs.PatchApplyError` (`project/vcs.ts:276-279`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("VcsPatchApplyError: {message}, reason: {reason}")]
pub struct PatchApplyError {
    pub message: String,
    pub reason: &'static str,
}

const PATCH_CONTEXT_LINES: usize = 2_147_483_647;
const MAX_PATCH_BYTES: usize = 10_000_000;
const MAX_TOTAL_PATCH_BYTES: usize = 10_000_000;

const RAW_CONTEXT_LINE_CAP: usize = 10_000_000;

/// `kind` (`git/index.ts:93-99`).
fn kind(code: &str) -> Kind {
    if code == "??" {
        return Kind::Added;
    }
    if code.contains('U') {
        return Kind::Modified;
    }
    if code.contains('A') && !code.contains('D') {
        return Kind::Added;
    }
    if code.contains('D') && !code.contains('A') {
        return Kind::Deleted;
    }
    Kind::Modified
}

fn nuls(text: &str) -> Vec<&str> {
    text.split('\0').filter(|item| !item.is_empty()).collect()
}

/// The git CLI surface (`git/index.ts:75-91`). All git subprocesses run
/// with the `cfg` flags (`:6-18`); failures are data (`:22-29`).
#[derive(Debug, Clone, Copy, Default)]
pub struct GitCli;

/// One subprocess result (`git/index.ts:60-66`).
#[derive(Debug, Clone)]
pub struct RunResult {
    pub exit_code: i32,
    pub text: String,
    pub stderr: String,
    pub truncated: bool,
}

/// The `cfg` flags on every git subprocess (`git/index.ts:6-18`).
const CFG: &[&str] = &[
    "--no-optional-locks",
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.longpaths=true",
    "-c",
    "core.symlinks=true",
    "-c",
    "core.quotepath=false",
];

fn out(text: &str) -> String {
    text.trim().to_string()
}

impl GitCli {
    /// `Git.run` (`git/index.ts:110-132`).
    pub fn run(&self, cwd: &Path, args: &[&str], max_output_bytes: Option<usize>) -> RunResult {
        let mut command = Command::new("git");
        command
            .args(CFG)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match command.output() {
            Ok(output) => {
                let mut stdout = output.stdout;
                let mut truncated = false;
                if let Some(max) = max_output_bytes {
                    if stdout.len() > max {
                        stdout.truncate(max);
                        truncated = true;
                    }
                }
                RunResult {
                    exit_code: output.status.code().unwrap_or(1),
                    text: String::from_utf8_lossy(&stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    truncated,
                }
            }
            Err(err) => RunResult {
                exit_code: 1,
                text: String::new(),
                stderr: err.to_string(),
                truncated: false,
            },
        }
    }

    /// `git.applyPatch` (`git/index.ts:322-324`) — needs stdin.
    fn apply_patch(&self, cwd: &Path, patch: &str) -> i32 {
        let mut command = Command::new("git");
        command
            .args(CFG)
            .args(["apply", "-"])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = match command.spawn() {
            Ok(child) => child,
            Err(_) => return 1,
        };
        let mut child = child;
        if let Some(stdin) = child.stdin.as_mut() {
            if stdin.write_all(patch.as_bytes()).is_err() {
                // The process may have exited before reading stdin.
            }
        }
        match child.wait_with_output() {
            Ok(output) => output.status.code().unwrap_or(1),
            Err(_) => 1,
        }
    }

    /// `Git.branch` (`git/index.ts:164-169`).
    pub fn branch(&self, cwd: &Path) -> Option<String> {
        let result = self.run(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"], None);
        if result.exit_code != 0 {
            return None;
        }
        let text = out(&result.text);
        (!text.is_empty()).then_some(text)
    }

    /// `Git.defaultBranch` (`git/index.ts:177-193`).
    pub fn default_branch(&self, cwd: &Path) -> Option<Base> {
        let remote = self.primary(cwd);
        if let Some(remote) = remote.as_deref() {
            let result = self.run(
                cwd,
                &["symbolic-ref", &format!("refs/remotes/{remote}/HEAD")],
                None,
            );
            if result.exit_code == 0 {
                let ref_ = out(&result.text)
                    .strip_prefix("refs/remotes/")
                    .map(str::to_string)
                    .unwrap_or_else(|| out(&result.text));
                let name = ref_
                    .strip_prefix(&format!("{remote}/"))
                    .filter(|name| !name.is_empty())
                    .map(str::to_string);
                if let Some(name) = name {
                    return Some(Base { name, ref_ });
                }
            }
        }

        let refs = self.refs(cwd);
        if let Some(next) = self.configured(cwd, &refs) {
            return Some(next);
        }
        if refs.iter().any(|item| item == "main") {
            return Some(Base {
                name: "main".to_string(),
                ref_: "main".to_string(),
            });
        }
        if refs.iter().any(|item| item == "master") {
            return Some(Base {
                name: "master".to_string(),
                ref_: "master".to_string(),
            });
        }
        None
    }

    /// `refs` (`git/index.ts:145-147`).
    fn refs(&self, cwd: &Path) -> Vec<String> {
        let result = self.run(
            cwd,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
            None,
        );
        if result.exit_code != 0 {
            return Vec::new();
        }
        result
            .text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// `configured` (`git/index.ts:149-154`).
    fn configured(&self, cwd: &Path, list: &[String]) -> Option<Base> {
        let result = self.run(cwd, &["config", "init.defaultBranch"], None);
        let name = out(&result.text);
        if name.is_empty() || !list.iter().any(|item| item == &name) {
            return None;
        }
        Some(Base {
            name: name.clone(),
            ref_: name,
        })
    }

    /// `primary` (`git/index.ts:156-162`).
    fn primary(&self, cwd: &Path) -> Option<String> {
        let result = self.run(cwd, &["remote"], None);
        if result.exit_code != 0 {
            return None;
        }
        let list: Vec<String> = result
            .text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        if list.iter().any(|item| item == "origin") {
            return Some("origin".to_string());
        }
        if list.len() == 1 {
            return list.first().cloned();
        }
        if list.iter().any(|item| item == "upstream") {
            return Some("upstream".to_string());
        }
        list.first().cloned()
    }

    /// `Git.hasHead` (`git/index.ts:195-198`).
    pub fn has_head(&self, cwd: &Path) -> bool {
        self.run(cwd, &["rev-parse", "--verify", "HEAD"], None)
            .exit_code
            == 0
    }

    /// `Git.mergeBase` (`git/index.ts:200-205`).
    pub fn merge_base(&self, cwd: &Path, base: &str) -> Option<String> {
        let result = self.run(cwd, &["merge-base", base, "HEAD"], None);
        if result.exit_code != 0 {
            return None;
        }
        let text = out(&result.text);
        (!text.is_empty()).then_some(text)
    }

    /// `Git.status` (`git/index.ts:215-226`).
    pub fn status(&self, cwd: &Path) -> Vec<Item> {
        let result = self.run(
            cwd,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--no-renames",
                "-z",
                "--",
                ".",
            ],
            None,
        );
        nuls(&result.text)
            .into_iter()
            .filter_map(|item| {
                let file = &item[3.min(item.len())..];
                if file.is_empty() {
                    return None;
                }
                if item.len() < 2 {
                    return None;
                }
                let code = &item[..2];
                Some(Item {
                    file: file.to_string(),
                    status: kind(code),
                    code: code.to_string(),
                })
            })
            .collect()
    }

    /// `Git.diff` (`git/index.ts:228-238`).
    pub fn diff(&self, cwd: &Path, ref_: &str) -> Vec<Item> {
        let result = self.run(
            cwd,
            &[
                "diff",
                "--no-ext-diff",
                "--no-renames",
                "--name-status",
                "-z",
                ref_,
                "--",
                ".",
            ],
            None,
        );
        let list = nuls(&result.text);
        let mut out = Vec::new();
        let mut idx = 0;
        while idx + 1 < list.len() || (idx + 1 == list.len() && !list.is_empty()) {
            let code = list[idx];
            let Some(file) = list.get(idx + 1) else {
                break;
            };
            if code.is_empty() || file.is_empty() {
                break;
            }
            out.push(Item {
                file: file.to_string(),
                code: code.to_string(),
                status: kind(code),
            });
            idx += 2;
        }
        out
    }

    /// `Git.stats` (`git/index.ts:240-261`).
    pub fn stats(&self, cwd: &Path, ref_: &str) -> Vec<Stat> {
        let result = self.run(
            cwd,
            &[
                "diff",
                "--no-ext-diff",
                "--no-renames",
                "--numstat",
                "-z",
                ref_,
                "--",
                ".",
            ],
            None,
        );
        nuls(&result.text)
            .into_iter()
            .filter_map(|item| {
                let a = item.find('\t')?;
                let b = item[a + 1..].find('\t').map(|found| found + a + 1)?;
                let file = &item[b + 1..];
                if file.is_empty() {
                    return None;
                }
                let adds = &item[..a];
                let dels = &item[a + 1..b];
                let additions = if adds == "-" {
                    0
                } else {
                    adds.parse::<i64>().unwrap_or(0)
                };
                let deletions = if dels == "-" {
                    0
                } else {
                    dels.parse::<i64>().unwrap_or(0)
                };
                Some(Stat {
                    file: file.to_string(),
                    additions,
                    deletions,
                })
            })
            .collect()
    }

    /// `Git.patch` (`git/index.ts:263-269`).
    pub fn patch(
        &self,
        cwd: &Path,
        ref_: &str,
        file: &str,
        context: Option<usize>,
        max_output_bytes: Option<usize>,
    ) -> Patch {
        let unified = format!("--unified={}", context.unwrap_or(3));
        let result = self.run(
            cwd,
            &[
                "diff",
                "--patch",
                "--no-ext-diff",
                "--no-renames",
                &unified,
                ref_,
                "--",
                file,
            ],
            max_output_bytes,
        );
        Patch {
            text: if result.truncated {
                String::new()
            } else {
                result.text
            },
            truncated: result.truncated,
        }
    }

    /// `Git.patchAll` (`git/index.ts:271-277`).
    pub fn patch_all(
        &self,
        cwd: &Path,
        ref_: &str,
        context: Option<usize>,
        max_output_bytes: Option<usize>,
    ) -> Patch {
        let unified = format!("--unified={}", context.unwrap_or(3));
        let result = self.run(
            cwd,
            &[
                "diff",
                "--patch",
                "--no-ext-diff",
                "--no-renames",
                &unified,
                ref_,
                "--",
                ".",
            ],
            max_output_bytes,
        );
        Patch {
            text: result.text,
            truncated: result.truncated,
        }
    }

    /// `Git.patchUntracked` (`git/index.ts:279-299`).
    pub fn patch_untracked(
        &self,
        cwd: &Path,
        file: &str,
        context: Option<usize>,
        max_output_bytes: Option<usize>,
    ) -> Patch {
        let unified = format!("--unified={}", context.unwrap_or(3));
        let result = self.run(
            cwd,
            &[
                "diff",
                "--no-index",
                "--patch",
                "--no-ext-diff",
                "--no-renames",
                &unified,
                "--",
                "/dev/null",
                file,
            ],
            max_output_bytes,
        );
        Patch {
            text: if result.truncated {
                String::new()
            } else {
                result.text
            },
            truncated: result.truncated,
        }
    }

    /// `Git.statUntracked` (`git/index.ts:301-320`).
    pub fn stat_untracked(&self, cwd: &Path, file: &str) -> Option<Stat> {
        let result = self.run(
            cwd,
            &["diff", "--no-index", "--numstat", "--", "/dev/null", file],
            Some(4096),
        );
        if result.truncated {
            return None;
        }
        let parts: Vec<&str> = result.text.split('\t').collect();
        if parts.len() < 2 {
            return None;
        }
        let additions = if parts[0] == "-" {
            0
        } else {
            parts[0].parse::<i64>().unwrap_or(0)
        };
        let deletions = if parts[1] == "-" {
            0
        } else {
            parts[1].parse::<i64>().unwrap_or(0)
        };
        Some(Stat {
            file: file.to_string(),
            additions,
            deletions,
        })
    }
}

/// `emptyPatch` (`project/vcs.ts:18`).
fn empty_patch(file: &str) -> String {
    crate::tool::diff::format_patch(file, "", "", 0)
}

fn nums(list: &[Stat]) -> std::collections::HashMap<&str, (i64, i64)> {
    list.iter()
        .map(|item| (item.file.as_str(), (item.additions, item.deletions)))
        .collect()
}

/// `splitGitPatch` (`project/vcs.ts:88-95`).
fn split_git_patch(patch: &Patch) -> Vec<String> {
    let mut starts: Vec<usize> = Vec::new();
    let bytes = patch.text.as_bytes();
    let mut from = 0;
    while let Some(found) = patch.text[from..].find("diff --git ") {
        let at = from + found;
        if at == 0 || bytes[at - 1] == b'\n' {
            starts.push(at);
        }
        from = at + 1;
    }
    let mut chunks: Vec<String> = starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(patch.text.len());
            patch.text[*start..end].to_string()
        })
        .collect();
    if patch.truncated && !chunks.is_empty() {
        chunks.pop();
    }
    chunks
}

#[derive(Default)]
struct Batch {
    patches: std::collections::HashMap<String, String>,
    capped: bool,
}

/// `fileFromDiffPath` + `parsePathToken` + `parseQuotedPath`
/// (`project/vcs.ts:33-76`).
fn parse_quoted_path(value: &str) -> Option<(String, usize)> {
    // Byte-indexed (the returned offset is used for byte slicing; JS string
    // indices are UTF-16 units, but slicing is on ASCII quotes/backslashes
    // so byte offsets match the TS remainder semantics exactly).
    let chars: Vec<(usize, char)> = value.char_indices().collect();
    let mut out = String::new();
    let mut idx = 1;
    while idx < chars.len() {
        let (offset, char) = chars[idx];
        if char == '"' {
            return Some((out, offset + char.len_utf8()));
        }
        if char != '\\' {
            out.push(char);
            idx += 1;
            continue;
        }
        idx += 1;
        match chars.get(idx) {
            Some((_, 't')) => out.push('\t'),
            Some((_, 'n')) => out.push('\n'),
            Some((_, 'r')) => out.push('\r'),
            Some((_, '"')) => out.push('"'),
            Some((_, '\\')) => out.push('\\'),
            Some((_, next)) => out.push(*next),
            None => {}
        }
        idx += 1;
    }
    None
}

fn parse_path_token(value: &str) -> String {
    if !value.starts_with('"') {
        return value.split('\t').next().unwrap_or(value).to_string();
    }
    parse_quoted_path(value)
        .map(|(value, _)| value)
        .unwrap_or_else(|| value.to_string())
}

fn file_from_diff_path(value: &str) -> Option<String> {
    if value.is_empty() || value == "/dev/null" {
        return None;
    }
    let file = parse_path_token(value);
    match file.strip_prefix("a/").or_else(|| file.strip_prefix("b/")) {
        Some(stripped) => Some(stripped.to_string()),
        None => Some(file),
    }
}

fn file_from_git_header(header: &str) -> Option<String> {
    if header.starts_with('"') {
        let first = parse_quoted_path(header);
        let second = first
            .as_ref()
            .map(|(_, end)| &header[*end..])
            .map(str::trim_start)
            .filter(|rest| !rest.is_empty())?;
        if !second.starts_with('"') {
            return file_from_diff_path(second);
        }
        return file_from_diff_path(&parse_quoted_path(second)?.0);
    }
    let separator = header.find(" b/")?;
    file_from_diff_path(&header[separator + 1..])
}

fn file_from_patch_chunk(chunk: &str) -> Option<String> {
    static NEXT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?m)^\+\+\+ (.+)$").unwrap());
    static BEFORE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?m)^--- (.+)$").unwrap());
    static GIT: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?m)^diff --git (.+)$").unwrap());
    let file = NEXT
        .captures(chunk)
        .and_then(|m| file_from_diff_path(&m[1]))
        .or_else(|| {
            BEFORE
                .captures(chunk)
                .and_then(|m| file_from_diff_path(&m[1]))
        });
    if let Some(file) = file {
        return Some(file);
    }
    GIT.captures(chunk)
        .and_then(|m| file_from_git_header(&m[1]))
}

fn batch_patches(
    git: &GitCli,
    cwd: &Path,
    ref_: &str,
    list: &[Item],
    context: Option<usize>,
) -> Batch {
    if list.is_empty() {
        return Batch::default();
    }
    let result = git.patch_all(
        cwd,
        ref_,
        Some(context.unwrap_or(PATCH_CONTEXT_LINES)),
        Some(MAX_TOTAL_PATCH_BYTES),
    );
    let mut patches: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (index, chunk) in split_git_patch(&result).into_iter().enumerate() {
        let file =
            file_from_patch_chunk(&chunk).or_else(|| list.get(index).map(|item| item.file.clone()));
        let Some(file) = file else { continue };
        let entry = patches.entry(file).or_default();
        *entry += &chunk;
    }
    Batch {
        patches,
        capped: result.truncated,
    }
}

fn native_patch(
    git: &GitCli,
    cwd: &Path,
    ref_: Option<&str>,
    item: &Item,
    context: Option<usize>,
) -> String {
    let result = match (item.code == "??", ref_) {
        (true, _) | (false, None) => git.patch_untracked(
            cwd,
            &item.file,
            Some(context.unwrap_or(PATCH_CONTEXT_LINES)),
            Some(MAX_PATCH_BYTES),
        ),
        (false, Some(ref_)) => git.patch(
            cwd,
            ref_,
            &item.file,
            Some(context.unwrap_or(PATCH_CONTEXT_LINES)),
            Some(MAX_PATCH_BYTES),
        ),
    };
    if !result.truncated && !result.text.is_empty() {
        return result.text;
    }
    empty_patch(&item.file)
}

fn patch_for_item(
    git: &GitCli,
    cwd: &Path,
    ref_: Option<&str>,
    item: &Item,
    batch: &Batch,
    capped: bool,
    context: Option<usize>,
) -> String {
    if capped {
        return empty_patch(&item.file);
    }
    if let Some(batched) = batch.patches.get(&item.file) {
        return batched.clone();
    }
    if item.code != "??" && batch.capped {
        return empty_patch(&item.file);
    }
    native_patch(git, cwd, ref_, item, context)
}

fn files(
    git: &GitCli,
    cwd: &Path,
    ref_: Option<&str>,
    list: &[Item],
    map: &std::collections::HashMap<&str, (i64, i64)>,
    batch: &Batch,
    context: Option<usize>,
) -> Vec<VcsFileDiff> {
    let mut next: Vec<VcsFileDiff> = Vec::new();
    let mut total: usize = 0;
    let mut capped = false;

    let mut sorted: Vec<&Item> = list.iter().collect();
    sorted.sort_by(|a, b| a.file.cmp(&b.file));
    for item in sorted {
        let stat = map.get(item.file.as_str()).copied().or_else(|| {
            (item.status == Kind::Added)
                .then(|| git.stat_untracked(cwd, &item.file))
                .flatten()
                .map(|stat| (stat.additions, stat.deletions))
        });
        let patch = patch_for_item(git, cwd, ref_, item, batch, capped, context);
        let result = if capped {
            (patch, true)
        } else if total + patch.len() <= MAX_TOTAL_PATCH_BYTES {
            (patch.clone(), false)
        } else {
            (empty_patch(&item.file), true)
        };
        let (patch, result_capped) = result;
        capped = capped || result_capped;
        if !capped {
            total += patch.len();
            capped = total >= MAX_TOTAL_PATCH_BYTES;
        }
        next.push(VcsFileDiff {
            file: item.file.clone(),
            patch: Some(patch),
            additions: stat.map(|(additions, _)| additions).unwrap_or(0),
            deletions: stat.map(|(_, deletions)| deletions).unwrap_or(0),
            status: Some(item.status),
        });
    }
    next
}

fn diff_against_ref(
    git: &GitCli,
    cwd: &Path,
    ref_: &str,
    context: Option<usize>,
) -> Vec<VcsFileDiff> {
    let list = git.diff(cwd, ref_);
    let stats = git.stats(cwd, ref_);
    let extra = git.status(cwd);
    let merged: Vec<Item> = list
        .iter()
        .cloned()
        .chain(extra.into_iter().filter(|item| item.code == "??"))
        .collect();
    files(
        git,
        cwd,
        Some(ref_),
        &merged,
        &nums(&stats),
        &batch_patches(git, cwd, ref_, &list, context),
        context,
    )
}

fn track(git: &GitCli, cwd: &Path, ref_: Option<&str>, context: Option<usize>) -> Vec<VcsFileDiff> {
    match ref_ {
        None => files(
            git,
            cwd,
            None,
            &git.status(cwd),
            &std::collections::HashMap::new(),
            &Batch::default(),
            context,
        ),
        Some(ref_) => diff_against_ref(git, cwd, ref_, context),
    }
}

/// The instance vcs service — `project/vcs.ts:281-289`. Non-git directories
/// behave like projects without vcs (`ctx.project.vcs !== "git"`).
pub struct Vcs {
    pub git: GitCli,
}

impl Default for Vcs {
    fn default() -> Self {
        Vcs { git: GitCli }
    }
}

impl Vcs {
    fn is_git(&self, directory: &Path) -> bool {
        crate::git::GitRunner::discover(&crate::git::SubprocessGit, directory).is_some()
    }

    /// `info` — the `{branch, default_branch}` handler shape
    /// (`handlers/instance.ts:34-38`).
    pub fn info(&self, directory: &Path) -> VcsInfo {
        if !self.is_git(directory) {
            return VcsInfo::default();
        }
        VcsInfo {
            branch: self.git.branch(directory),
            default_branch: self.git.default_branch(directory).map(|base| base.name),
        }
    }

    /// `status` (`project/vcs.ts:348-372`).
    pub fn status(&self, directory: &Path) -> Vec<VcsFileStatus> {
        if !self.is_git(directory) {
            return Vec::new();
        }
        let ref_ = self.git.has_head(directory).then_some("HEAD");
        let list = self.git.status(directory);
        let stats = match ref_ {
            Some(ref_) => self.git.stats(directory, ref_),
            None => Vec::new(),
        };
        let map = nums(&stats);
        let mut sorted: Vec<&Item> = list.iter().collect();
        sorted.sort_by(|a, b| a.file.cmp(&b.file));
        sorted
            .into_iter()
            .map(|item| {
                let stat = map.get(item.file.as_str()).copied().or_else(|| {
                    (item.status == Kind::Added)
                        .then(|| self.git.stat_untracked(directory, &item.file))
                        .flatten()
                        .map(|stat| (stat.additions, stat.deletions))
                });
                VcsFileStatus {
                    file: item.file.clone(),
                    additions: stat.map(|(additions, _)| additions).unwrap_or(0),
                    deletions: stat.map(|(_, deletions)| deletions).unwrap_or(0),
                    status: item.status,
                }
            })
            .collect()
    }

    /// `diff` (`project/vcs.ts:373-386`).
    pub fn diff(&self, directory: &Path, mode: &str, context: Option<i64>) -> Vec<VcsFileDiff> {
        if !self.is_git(directory) {
            return Vec::new();
        }
        let context = context.map(|value| value.clamp(0, i32::MAX as i64) as usize);
        if mode == "git" {
            let ref_ = self.git.has_head(directory).then_some("HEAD");
            return track(&self.git, directory, ref_, context);
        }
        // mode === "branch"
        let root = self.git.default_branch(directory);
        let Some(root) = root else {
            return Vec::new();
        };
        let current = self.git.branch(directory);
        if current.as_deref() == Some(root.name.as_str()) {
            return Vec::new();
        }
        let Some(ref_) = self.git.merge_base(directory, &root.ref_) else {
            return Vec::new();
        };
        diff_against_ref(&self.git, directory, &ref_, context)
    }

    /// `diffRaw` (`project/vcs.ts:387-399`).
    pub fn diff_raw(&self, directory: &Path) -> String {
        if !self.is_git(directory) {
            return String::new();
        }
        let tracked = if self.git.has_head(directory) {
            self.git
                .patch_all(directory, "HEAD", None, Some(RAW_CONTEXT_LINE_CAP))
                .text
        } else {
            String::new()
        };
        let mut parts = vec![tracked];
        for item in self.git.status(directory) {
            if item.code == "??" {
                parts.push(
                    self.git
                        .patch_untracked(directory, &item.file, None, None)
                        .text,
                );
            }
        }
        parts
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `apply` (`project/vcs.ts:400-416`).
    pub fn apply(&self, directory: &Path, patch: &str) -> Result<VcsApplyResult, PatchApplyError> {
        if !self.is_git(directory) {
            return Err(PatchApplyError {
                message: "Patch can't be applied because the project is not git-based".to_string(),
                reason: "non-git",
            });
        }
        let applied = self.git.apply_patch(directory, patch);
        if applied != 0 {
            return Err(PatchApplyError {
                message: "Patch can't be applied".to_string(),
                reason: "not-clean",
            });
        }
        Ok(VcsApplyResult { applied: true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_support::TempDir;
    use std::path::PathBuf;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git subprocess");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// A repo with a `main` branch and one committed file.
    fn repo(tag: &str) -> (TempDir, PathBuf) {
        let temp = TempDir::new(tag);
        let dir = temp.path().to_path_buf();
        git(&dir, &["init", "--quiet", "-b", "main"]);
        git(&dir, &["config", "user.email", "test@opencode.test"]);
        git(&dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        std::fs::write(dir.join("deleting.txt"), "delete me\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "--quiet", "-m", "root"]);
        (temp, dir)
    }

    fn vcs() -> Vcs {
        Vcs::default()
    }

    #[test]
    fn git_branch_before_and_after_commit() {
        let temp = TempDir::new("vcs-branch-unborn");
        git(temp.path(), &["init", "--quiet", "-b", "main"]);
        let cli = GitCli;
        assert!(!cli.has_head(temp.path()));
        // An unborn HEAD still resolves its symbolic ref.
        assert_eq!(cli.branch(temp.path()), Some("main".to_string()));
        git(temp.path(), &["config", "user.email", "t@e.st"]);
        git(temp.path(), &["config", "user.name", "T"]);
        std::fs::write(temp.path().join("a.txt"), "a\n").unwrap();
        git(temp.path(), &["add", "a.txt"]);
        git(temp.path(), &["commit", "--quiet", "-m", "a"]);
        assert!(cli.has_head(temp.path()));
        assert_eq!(cli.branch(temp.path()), Some("main".to_string()));
    }

    #[test]
    fn git_default_branch_prefers_main_and_master() {
        let (temp, dir) = repo("vcs-default-branch");
        let cli = GitCli;
        assert_eq!(
            cli.default_branch(&dir),
            Some(Base {
                name: "main".to_string(),
                ref_: "main".to_string(),
            })
        );

        git(&dir, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(dir.join("tracked.txt"), "two\n").unwrap();
        git(&dir, &["commit", "--quiet", "-am", "two"]);
        let base = cli.default_branch(&dir).unwrap();
        assert_eq!(base.name, "main");

        // With no main/master and no configuration there is no default.
        git(&dir, &["branch", "--quiet", "-d", "main"]);
        assert_eq!(cli.default_branch(&dir), None);
        let _ = temp;
    }

    #[test]
    fn git_default_branch_follows_remote_symbolic_ref() {
        let (temp, dir) = repo("vcs-default-remote");
        git(&dir, &["config", "user.email", "t@e.st"]);
        git(&dir, &["config", "user.name", "T"]);

        // Clone locally and point origin's HEAD at a non-main branch.
        let clone = temp.path().join("clone");
        git(
            temp.path(),
            &[
                "clone",
                "--quiet",
                dir.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        git(&clone, &["checkout", "--quiet", "-b", "develop"]);
        git(&clone, &["push", "--quiet", "origin", "develop"]);
        git(&clone, &["remote", "set-head", "origin", "develop"]);

        let cli = GitCli;
        let base = cli.default_branch(&clone).unwrap();
        // set-head --auto picks the pushed branch (develop).
        assert_eq!(base.name, "develop");
        assert_eq!(base.ref_, "origin/develop");
    }

    #[test]
    fn git_status_reports_untracked_modified_and_deleted() {
        let (temp, dir) = repo("vcs-status");
        std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        std::fs::remove_file(dir.join("deleting.txt")).unwrap();

        let cli = GitCli;
        let mut items = cli.status(&dir);
        items.sort_by(|a, b| a.file.cmp(&b.file));
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].file, "deleting.txt");
        assert_eq!(items[0].code, " D");
        assert_eq!(items[0].status, Kind::Deleted);
        assert_eq!(items[1].file, "new.txt");
        assert_eq!(items[1].code, "??");
        assert_eq!(items[1].status, Kind::Added);
        assert_eq!(items[2].file, "tracked.txt");
        assert_eq!(items[2].code, " M");
        assert_eq!(items[2].status, Kind::Modified);
        let _ = temp;
    }

    #[test]
    fn git_diff_and_stats_against_head() {
        let (temp, dir) = repo("vcs-diff");
        std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();
        let cli = GitCli;
        let mut items = cli.diff(&dir, "HEAD");
        items.sort_by(|a, b| a.file.cmp(&b.file));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].file, "tracked.txt");
        assert_eq!(items[0].status, Kind::Modified);

        let mut stats = cli.stats(&dir, "HEAD");
        stats.sort_by(|a, b| a.file.cmp(&b.file));
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].file, "tracked.txt");
        assert_eq!(stats[0].additions, 1);
        assert_eq!(stats[0].deletions, 1);
        let _ = temp;
    }

    #[test]
    fn git_patch_surfaces() {
        let (_temp, dir) = repo("vcs-patch");
        std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();
        std::fs::write(dir.join("untracked.txt"), "brand new\n").unwrap();
        let cli = GitCli;

        let patch = cli.patch(&dir, "HEAD", "tracked.txt", None, None);
        assert!(!patch.truncated);
        assert!(patch.text.contains("--- a/tracked.txt"));

        let all = cli.patch_all(&dir, "HEAD", None, None);
        assert!(!all.truncated);
        assert!(all.text.contains("diff --git a/tracked.txt b/tracked.txt"));

        let untracked = cli.patch_untracked(&dir, "untracked.txt", None, None);
        assert!(!untracked.truncated);
        assert!(untracked.text.contains("--- /dev/null"));

        let stat = cli.stat_untracked(&dir, "untracked.txt").unwrap();
        assert_eq!(stat.file, "untracked.txt");
        assert_eq!(stat.additions, 1);
        assert_eq!(stat.deletions, 0);
    }

    #[test]
    fn vcs_info_status_and_diff() {
        let (temp, dir) = repo("vcs-service");
        let vcs = vcs();
        assert_eq!(
            vcs.info(&dir),
            VcsInfo {
                branch: Some("main".to_string()),
                default_branch: Some("main".to_string()),
            }
        );
        assert_eq!(
            vcs.info(&temp.path().join("nope")),
            VcsInfo {
                branch: None,
                default_branch: None,
            }
        );

        std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();
        std::fs::write(dir.join("created.txt"), "created\n").unwrap();

        let mut status = vcs.status(&dir);
        status.sort_by(|a, b| a.file.cmp(&b.file));
        assert_eq!(status.len(), 2);
        assert_eq!(
            status[0],
            VcsFileStatus {
                file: "created.txt".to_string(),
                additions: 1,
                deletions: 0,
                status: Kind::Added,
            }
        );
        assert_eq!(
            status[1],
            VcsFileStatus {
                file: "tracked.txt".to_string(),
                additions: 1,
                deletions: 1,
                status: Kind::Modified,
            }
        );

        // mode "git": tracked + untracked diffs against HEAD.
        let mut diffs = vcs.diff(&dir, "git", None);
        diffs.sort_by(|a, b| a.file.cmp(&b.file));
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].file, "created.txt");
        assert_eq!(diffs[0].status, Some(Kind::Added));
        assert!(diffs[0].patch.as_deref().unwrap().contains("created.txt"));
        assert_eq!(diffs[1].file, "tracked.txt");
        assert_eq!(diffs[1].status, Some(Kind::Modified));

        // mode "branch": on the default branch there is nothing to diff.
        assert!(vcs.diff(&dir, "branch", None).is_empty());

        // diffRaw includes both tracked and untracked patches.
        let raw = vcs.diff_raw(&dir);
        assert!(raw.contains("tracked.txt"));
        assert!(raw.contains("created.txt"));
    }

    #[test]
    fn vcs_diff_branch_mode_follows_merge_base() {
        let (_temp, dir) = repo("vcs-branch-diff");
        git(&dir, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(dir.join("tracked.txt"), "changed on feature\n").unwrap();
        git(&dir, &["commit", "--quiet", "-am", "feature work"]);
        std::fs::write(dir.join("tracked.txt"), "dirty on feature\n").unwrap();

        let vcs = vcs();
        let mut diffs = vcs.diff(&dir, "branch", None);
        diffs.sort_by(|a, b| a.file.cmp(&b.file));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].file, "tracked.txt");
        assert_eq!(diffs[0].status, Some(Kind::Modified));
    }

    #[test]
    fn vcs_apply_round_trip_and_errors() {
        let (_temp, dir) = repo("vcs-apply");
        let vcs = vcs();

        let patch = "diff --git a/tracked.txt b/tracked.txt\n\
                     --- a/tracked.txt\n\
                     +++ b/tracked.txt\n\
                     @@ -1 +1 @@\n\
                     -one\n\
                     +applied\n";
        vcs.apply(&dir, patch).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("tracked.txt")).unwrap(),
            "applied\n"
        );

        // Re-applying the same patch conflicts (not clean).
        let err = vcs.apply(&dir, patch).unwrap_err();
        assert_eq!(err.reason, "not-clean");

        let plain = TempDir::new("vcs-apply-plain");
        let err = vcs.apply(plain.path(), patch).unwrap_err();
        assert_eq!(err.reason, "non-git");
    }
}
