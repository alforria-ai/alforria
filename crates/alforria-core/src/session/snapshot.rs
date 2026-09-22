//! Snapshot seam — port of `snapshot/index.ts`. [`GitSnapshot`] is the
//! production git-backed implementation (M7.3); [`InMemorySnapshot`] is the
//! test double over file contents, diffed with the M4 `tool::diff` helpers,
//! and [`DisabledSnapshot`] the no-op default for instances without a
//! location.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use alforria_schema::file_diff::FileDiffStatus;

use crate::git::GitResult;
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
pub type FileDiff = alforria_schema::file_diff::SnapshotFileDiff;

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

    fn patch(&self, id: &str) -> BoxFuture<'static, Result<SnapshotPatch, CoreError>> {
        // Soft-fail like the git snapshot (snapshot/index.ts:349-361).
        let hash = id.to_string();
        Box::pin(async move {
            Ok(SnapshotPatch {
                hash,
                files: Vec::new(),
            })
        })
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

// ---------------------------------------------------------------------------
// Production git snapshot (M7.3) — port of `snapshot/index.ts`
// ---------------------------------------------------------------------------

/// `prune` (`snapshot/index.ts:23`).
const PRUNE: &str = "7.days";
/// `limit` — 2 MiB (`snapshot/index.ts:24`).
const LIMIT: u64 = 2 * 1024 * 1024;

/// The `-c` flag sets (`snapshot/index.ts:25-27`).
const CORE: [&str; 4] = ["-c", "core.longpaths=true", "-c", "core.symlinks=true"];
const CFG: [&str; 6] = [
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.longpaths=true",
    "-c",
    "core.symlinks=true",
];
const QUOTE: [&str; 8] = [
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.longpaths=true",
    "-c",
    "core.symlinks=true",
    "-c",
    "core.quotepath=false",
];

/// One subprocess result — stdout stays raw for the `cat-file --batch`
/// parser (byte offsets, not lossy UTF-8).
pub struct SnapshotOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// TS `git()` (`snapshot/index.ts:81-100`) — the subprocess seam with
/// stdin support (`check-ignore`/`add`/`rm` read pathspecs from stdin).
/// Spawn failures are data: `code: 1` with the error as stderr (`:93-99`).
pub trait SnapshotRunner: Send + Sync {
    fn run(
        &self,
        cwd: Option<&Path>,
        env: &[(&str, String)],
        args: &[&str],
        stdin: Option<&str>,
    ) -> SnapshotOutput;
}

/// Production subprocess runner — `AppProcess` + cross-spawn in TS.
pub struct SubprocessSnapshotRunner;

impl SnapshotRunner for SubprocessSnapshotRunner {
    fn run(
        &self,
        cwd: Option<&Path>,
        env: &[(&str, String)],
        args: &[&str],
        stdin: Option<&str>,
    ) -> SnapshotOutput {
        let mut command = Command::new("git");
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped());
        for (key, value) in env {
            command.env(key, value);
        }
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        let Ok(mut child) = command.spawn() else {
            return SnapshotOutput {
                code: 1,
                stdout: Vec::new(),
                stderr: "git failed to spawn".to_string(),
            };
        };
        if let (Some(mut handle), Some(data)) = (child.stdin.take(), stdin) {
            let _ = handle.write_all(data.as_bytes());
        }
        match child.wait_with_output() {
            Ok(output) => SnapshotOutput {
                code: output.status.code().unwrap_or(1),
                stdout: output.stdout,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            },
            Err(err) => SnapshotOutput {
                code: 1,
                stdout: Vec::new(),
                stderr: err.to_string(),
            },
        }
    }
}

/// The per-instance inputs — the TS `state` from `InstanceState`
/// (`snapshot/index.ts:66-73`).
pub struct GitSnapshotInput {
    pub directory: PathBuf,
    pub worktree: PathBuf,
    pub project_id: String,
    /// `project.vcs == "git"`.
    pub vcs_is_git: bool,
    /// `config.snapshot != false`.
    pub snapshot_enabled: bool,
    /// `Global.Path.data` — the snapshot root parent.
    pub data: PathBuf,
}

/// The `locks` map keyed by gitdir (`snapshot/index.ts:55-64`) —
/// instances sharing a gitdir serialize through the same lock.
static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock_for(gitdir: &Path) -> Arc<Mutex<()>> {
    let mut locks = LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks
        .entry(gitdir.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// The git-backed [`Snapshot`] — a line-for-line port of
/// `snapshot/index.ts` (only the error channels differ: soft git failures
/// are logged, never thrown).
#[derive(Clone)]
pub struct GitSnapshot {
    runner: std::sync::Arc<dyn SnapshotRunner>,
    directory: PathBuf,
    worktree: PathBuf,
    gitdir: PathBuf,
    vcs_is_git: bool,
    snapshot_enabled: bool,
    lock: Arc<Mutex<()>>,
    cleanup: Arc<(Mutex<bool>, Condvar)>,
}

impl Drop for GitSnapshot {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.cleanup;
        if let Ok(mut stopped) = lock.lock() {
            *stopped = true;
        }
        cvar.notify_all();
    }
}

impl GitSnapshot {
    pub fn new(input: GitSnapshotInput) -> Arc<GitSnapshot> {
        Self::with_cleanup(input, Duration::from_secs(60), Duration::from_secs(60 * 60))
    }

    /// Same as [`GitSnapshot::new`] with injectable loop delays for tests.
    pub fn with_cleanup(
        input: GitSnapshotInput,
        first: Duration,
        interval: Duration,
    ) -> Arc<GitSnapshot> {
        let gitdir = input
            .data
            .join("snapshot")
            .join(&input.project_id)
            .join(crate::project::hash_fast(&input.worktree.to_string_lossy()));
        let cleanup = Arc::new((Mutex::new(false), Condvar::new()));
        let lock = lock_for(&gitdir);
        let snapshot = Arc::new(GitSnapshot {
            runner: std::sync::Arc::new(SubprocessSnapshotRunner),
            directory: input.directory,
            worktree: input.worktree,
            gitdir,
            vcs_is_git: input.vcs_is_git,
            snapshot_enabled: input.snapshot_enabled,
            lock,
            cleanup: cleanup.clone(),
        });
        spawn_cleanup_loop(snapshot.clone(), first, interval);
        snapshot
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn git(&self, args: &[String], cwd: Option<&Path>, stdin: Option<&str>) -> GitResult {
        self.git_env(args, cwd, &[], stdin)
    }

    fn git_env(
        &self,
        args: &[String],
        cwd: Option<&Path>,
        env: &[(&str, String)],
        stdin: Option<&str>,
    ) -> GitResult {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.runner.run(cwd, env, &args, stdin);
        GitResult {
            exit_code: output.code,
            text: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: output.stderr,
        }
    }

    /// `args` (`snapshot/index.ts:75`): `--git-dir <gitdir> --work-tree
    /// <worktree>` prefixed after the `-c` flags.
    fn args(&self, flags: &[&str], cmd: &[&str]) -> Vec<String> {
        let mut out: Vec<String> = flags.iter().map(|flag| flag.to_string()).collect();
        out.extend([
            "--git-dir".to_string(),
            self.gitdir.to_string_lossy().into_owned(),
            "--work-tree".to_string(),
            self.worktree.to_string_lossy().into_owned(),
        ]);
        out.extend(cmd.iter().map(|item| item.to_string()));
        out
    }

    /// `enabled` (`snapshot/index.ts:167-170`).
    fn enabled(&self) -> bool {
        self.vcs_is_git && self.snapshot_enabled
    }

    /// `ignore` (`snapshot/index.ts:102-130`).
    fn ignore(&self, files: &[String]) -> HashSet<String> {
        if files.is_empty() {
            return HashSet::new();
        }
        // check-ignore treats a leading colon as pathspec magic but accepts
        // and echoes a protective ./ prefix.
        let check_paths: Vec<String> = files
            .iter()
            .map(|file| {
                if file.starts_with(':') {
                    format!("./{file}")
                } else {
                    file.clone()
                }
            })
            .collect();
        let mut args: Vec<String> = QUOTE.iter().map(|flag| flag.to_string()).collect();
        args.extend([
            "--git-dir".to_string(),
            self.worktree.join(".git").to_string_lossy().into_owned(),
            "--work-tree".to_string(),
            self.worktree.to_string_lossy().into_owned(),
            "check-ignore".to_string(),
            "--no-index".to_string(),
            "--stdin".to_string(),
            "-z".to_string(),
        ]);
        let stdin = encode_nul_paths(&check_paths);
        let check = self.git(&args, Some(&self.worktree), Some(&stdin));
        if check.exit_code != 0 && check.exit_code != 1 {
            return HashSet::new();
        }
        check
            .text
            .split('\0')
            .filter(|item| !item.is_empty())
            .map(|item| match item.strip_prefix("./:") {
                Some(rest) => rest.to_string(),
                None => item.to_string(),
            })
            .collect()
    }

    /// `drop` (`snapshot/index.ts:132-144`).
    fn drop_files(&self, files: &[String]) {
        if files.is_empty() {
            return;
        }
        let args = self.args(
            &CFG,
            &[
                "rm",
                "--cached",
                "-f",
                "--ignore-unmatch",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
        );
        let stdin = encode_pathspecs(files);
        self.git(&args, Some(&self.worktree), Some(&stdin));
    }

    /// `stage` (`snapshot/index.ts:146-160`).
    fn stage(&self, files: &[String]) {
        if files.is_empty() {
            return;
        }
        let args = self.args(
            &CFG,
            &[
                "add",
                "--all",
                "--sparse",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
        );
        let stdin = encode_pathspecs(files);
        let result = self.git(&args, Some(&self.worktree), Some(&stdin));
        if result.exit_code == 0 {
            return;
        }
        tracing::warn!(
            "failed to add snapshot files: exitCode={} stderr={}",
            result.exit_code,
            result.stderr
        );
    }

    /// `excludes` (`snapshot/index.ts:172-180`).
    fn excludes(&self) -> Option<PathBuf> {
        let result = self.git(
            &[
                "rev-parse".to_string(),
                "--path-format=absolute".to_string(),
                "--git-path".to_string(),
                "info/exclude".to_string(),
            ],
            Some(&self.worktree),
            None,
        );
        let file = result.text.trim();
        if file.is_empty() {
            return None;
        }
        let file = PathBuf::from(file);
        if !file.exists() {
            return None;
        }
        Some(file)
    }

    /// `sync` (`snapshot/index.ts:182-193`).
    fn sync(&self, list: &[String]) {
        let file = self.excludes();
        let target = self.gitdir.join("info").join("exclude");
        let mut parts: Vec<String> = Vec::new();
        if let Some(file) = file {
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            parts.push(text.trim_end().to_string());
        }
        for item in list {
            parts.push(format!("/{}", item.replace('\\', "/")));
        }
        let text = parts
            .iter()
            .filter(|part| !part.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::create_dir_all(self.gitdir.join("info"));
        let _ = std::fs::write(
            &target,
            if text.is_empty() {
                String::new()
            } else {
                format!("{text}\n")
            },
        );
    }

    /// `seed` (`snapshot/index.ts:198-233`).
    fn seed(&self) {
        if !self.vcs_is_git {
            return;
        }
        let common = self.git(
            &[
                "rev-parse".to_string(),
                "--path-format=absolute".to_string(),
                "--git-common-dir".to_string(),
            ],
            Some(&self.worktree),
            None,
        );
        if common.exit_code != 0 {
            return;
        }
        let source = common.text.trim();
        if source.is_empty() || !Path::new(source).exists() {
            return;
        }
        // Share the source object database (and the source's own
        // alternates, skipping any that no longer exist) so seeded blobs
        // resolve.
        let source_objects = Path::new(source).join("objects");
        let mut candidates = vec![source_objects.clone()];
        let chained = std::fs::read_to_string(source_objects.join("info").join("alternates"))
            .unwrap_or_default();
        for line in chained.split('\n') {
            let line = line.trim();
            if !line.is_empty() {
                candidates.push(PathBuf::from(line));
            }
        }
        let mut alternates: Vec<PathBuf> = Vec::new();
        for candidate in candidates {
            if candidate.exists() {
                alternates.push(candidate);
            }
        }
        if alternates.is_empty() {
            return;
        }
        let _ = std::fs::create_dir_all(self.gitdir.join("objects").join("info"));
        let text = alternates
            .iter()
            .map(|path| path.to_string_lossy())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(
            self.gitdir.join("objects").join("info").join("alternates"),
            format!("{text}\n"),
        );
        // Seed the index from the source repo so already-hashed entries are
        // reused. Best-effort: a missing/incompatible index just falls back
        // to a full add.
        let source_index = Path::new(source).join("index");
        if source_index.exists() {
            let _ = std::fs::copy(source_index, self.gitdir.join("index"));
        }
    }

    /// `add` (`snapshot/index.ts:235-298`).
    fn add(&self) {
        self.sync(&[]);
        let diff = self.git(
            &self.args(&QUOTE, &["diff-files", "--name-only", "-z", "--", "."]),
            Some(&self.directory),
            None,
        );
        let other = self.git(
            &self.args(
                &QUOTE,
                &[
                    "ls-files",
                    "--full-name",
                    "--others",
                    "--exclude-standard",
                    "-z",
                    "--",
                    ".",
                ],
            ),
            Some(&self.directory),
            None,
        );
        if diff.exit_code != 0 || other.exit_code != 0 {
            tracing::warn!("failed to list snapshot files");
            return;
        }
        let tracked = split_nul(&diff.text);
        let untracked = split_nul(&other.text);
        let mut all: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for item in tracked.iter().chain(untracked.iter()) {
            if seen.insert(item.clone()) {
                all.push(item.clone());
            }
        }
        if all.is_empty() {
            return;
        }
        // Resolve source-repo ignore rules against the exact candidate set.
        // --no-index keeps this pattern-based even when a path is already
        // tracked.
        let ignored = self.ignore(&all);
        if !ignored.is_empty() {
            let ignored_files: Vec<String> = ignored.iter().cloned().collect();
            tracing::info!(
                "removing gitignored files from snapshot: count={}",
                ignored_files.len()
            );
            self.drop_files(&ignored_files);
        }
        let allow: Vec<String> = all
            .iter()
            .filter(|item| !ignored.contains(*item))
            .cloned()
            .collect();
        if allow.is_empty() {
            return;
        }
        let mut large = HashSet::new();
        for item in &allow {
            if let Ok(meta) = std::fs::metadata(self.worktree.join(item)) {
                if meta.is_file() && meta.len() > LIMIT {
                    large.insert(item.clone());
                }
            }
        }
        let block: Vec<String> = untracked
            .iter()
            .filter(|item| large.contains(*item))
            .cloned()
            .collect();
        let block_set: HashSet<String> = block.iter().cloned().collect();
        self.sync(&block);
        // Stage only the allowed candidate paths so snapshot updates stay
        // scoped.
        let stage: Vec<String> = allow
            .iter()
            .filter(|item| !block_set.contains(*item))
            .cloned()
            .collect();
        self.stage(&stage);
    }

    /// `cleanup` (`snapshot/index.ts:300-316`).
    fn cleanup_impl(&self) {
        let _guard = self.lock();
        if !self.enabled() {
            return;
        }
        if !self.gitdir.exists() {
            return;
        }
        let result = self.git(
            &self.args(&[], &["gc", &format!("--prune={PRUNE}")]),
            Some(&self.directory),
            None,
        );
        if result.exit_code != 0 {
            tracing::warn!(
                "cleanup failed: exitCode={} stderr={}",
                result.exit_code,
                result.stderr
            );
            return;
        }
        tracing::info!("cleanup: prune={PRUNE}");
    }

    /// `track` (`snapshot/index.ts:318-347`).
    fn track_impl(&self) -> Option<String> {
        let _guard = self.lock();
        if !self.enabled() {
            return None;
        }
        let existed = self.gitdir.exists();
        let _ = std::fs::create_dir_all(&self.gitdir);
        if !existed {
            let env = [
                ("GIT_DIR", self.gitdir.to_string_lossy().into_owned()),
                (
                    "GIT_WORK_TREE",
                    self.worktree.to_string_lossy().into_owned(),
                ),
            ];
            self.git_env(&["init".to_string()], None, &env, None);
            let gitdir = self.gitdir.to_string_lossy().into_owned();
            for (key, value) in [
                ("core.autocrlf", "false"),
                ("core.longpaths", "true"),
                ("core.symlinks", "true"),
                ("core.fsmonitor", "false"),
                // Tuning for very large worktrees so the first add stays
                // bounded.
                ("feature.manyFiles", "true"),
                ("index.version", "4"),
                ("index.threads", "true"),
                ("core.untrackedCache", "true"),
            ] {
                self.git(
                    &[
                        "--git-dir".to_string(),
                        gitdir.clone(),
                        "config".to_string(),
                        key.to_string(),
                        value.to_string(),
                    ],
                    None,
                    None,
                );
            }
            self.seed();
            tracing::info!("initialized");
        }
        self.add();
        let result = self.git(
            &self.args(&[], &["write-tree"]),
            Some(&self.directory),
            None,
        );
        let hash = result.text.trim().to_string();
        Some(hash)
    }

    /// `patch` (`snapshot/index.ts:349-380`).
    fn patch_impl(&self, hash: &str) -> SnapshotPatch {
        let _guard = self.lock();
        self.add();
        let result = self.git(
            &self.args(
                &QUOTE,
                &[
                    "diff",
                    "--cached",
                    "--no-ext-diff",
                    "--name-only",
                    hash,
                    "--",
                    ".",
                ],
            ),
            Some(&self.directory),
            None,
        );
        if result.exit_code != 0 {
            tracing::warn!(
                "failed to get diff: hash={hash} exitCode={}",
                result.exit_code
            );
            return SnapshotPatch {
                hash: hash.to_string(),
                files: Vec::new(),
            };
        }
        let files: Vec<String> = result
            .text
            .trim()
            .split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect();
        // Hide ignored-file removals from the user-facing patch output.
        let ignored = self.ignore(&files);
        SnapshotPatch {
            hash: hash.to_string(),
            files: files
                .iter()
                .filter(|file| !ignored.contains(*file))
                .map(|file| {
                    self.worktree
                        .join(file)
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect(),
        }
    }

    /// `restore` (`snapshot/index.ts:382-406`).
    fn restore_impl(&self, snapshot: &str) {
        let _guard = self.lock();
        let result = self.git(
            &self.args(&CORE, &["read-tree", snapshot]),
            Some(&self.worktree),
            None,
        );
        if result.exit_code == 0 {
            let checkout = self.git(
                &self.args(&CORE, &["checkout-index", "-a", "-f"]),
                Some(&self.worktree),
                None,
            );
            if checkout.exit_code == 0 {
                return;
            }
            tracing::error!(
                "failed to restore snapshot: snapshot={snapshot} exitCode={} stderr={}",
                checkout.exit_code,
                checkout.stderr
            );
            return;
        }
        tracing::error!(
            "failed to restore snapshot: snapshot={snapshot} exitCode={} stderr={}",
            result.exit_code,
            result.stderr
        );
    }

    /// `revert` (`snapshot/index.ts:408-524`).
    fn revert_impl(&self, patches: Vec<PatchPart>) {
        let _guard = self.lock();
        struct Op {
            hash: String,
            file: String,
            rel: String,
        }
        let mut ops: Vec<Op> = Vec::new();
        let mut seen = HashSet::new();
        for item in patches {
            for file in &item.files {
                if !seen.insert(file.clone()) {
                    continue;
                }
                ops.push(Op {
                    hash: item.hash.clone(),
                    file: file.clone(),
                    rel: relative(&self.worktree, file),
                });
            }
        }

        let single = |op: &Op| {
            tracing::info!("reverting: file={} hash={}", op.file, op.hash);
            let result = self.git(
                &self.args(&CORE, &["checkout", &op.hash, "--", &op.file]),
                Some(&self.worktree),
                None,
            );
            if result.exit_code == 0 {
                return;
            }
            let tree = self.git(
                &self.args(&CORE, &["ls-tree", &op.hash, "--", &op.rel]),
                Some(&self.worktree),
                None,
            );
            if tree.exit_code == 0 && !tree.text.trim().is_empty() {
                tracing::info!(
                    "file existed in snapshot but checkout failed, keeping: file={}",
                    op.file
                );
                return;
            }
            tracing::info!("file did not exist in snapshot, deleting: file={}", op.file);
            let _ = std::fs::remove_file(&op.file);
        };

        let mut i = 0usize;
        while i < ops.len() {
            let first = &ops[i];
            let mut run = vec![first];
            let mut j = i + 1;
            // Only batch adjacent files when their paths cannot affect
            // each other.
            while j < ops.len() && run.len() < 100 {
                let next = &ops[j];
                if next.hash != first.hash {
                    break;
                }
                if run.iter().any(|item| clash(&item.rel, &next.rel)) {
                    break;
                }
                run.push(next);
                j += 1;
            }
            if run.len() == 1 {
                single(first);
                i = j;
                continue;
            }
            let mut cmd: Vec<&str> = vec!["ls-tree", "--name-only", &first.hash, "--"];
            cmd.extend(run.iter().map(|op| op.rel.as_str()));
            let tree = self.git(&self.args(&CORE, &cmd), Some(&self.worktree), None);
            if tree.exit_code != 0 {
                tracing::info!("batched ls-tree failed, falling back to single-file revert");
                for op in &run {
                    single(op);
                }
                i = j;
                continue;
            }
            let have: HashSet<&str> = tree
                .text
                .trim()
                .split('\n')
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect();
            let list: Vec<&Op> = run
                .iter()
                .filter(|op| have.contains(op.rel.as_str()))
                .copied()
                .collect();
            if !list.is_empty() {
                tracing::info!("reverting: hash={} files={}", first.hash, list.len());
                let mut cmd: Vec<&str> = vec!["checkout", &first.hash, "--"];
                cmd.extend(list.iter().map(|op| op.file.as_str()));
                let result = self.git(&self.args(&CORE, &cmd), Some(&self.worktree), None);
                if result.exit_code != 0 {
                    tracing::info!("batched checkout failed, falling back to single-file revert");
                    for op in &run {
                        single(op);
                    }
                    i = j;
                    continue;
                }
            }
            for op in &run {
                if !have.contains(op.rel.as_str()) {
                    tracing::info!("file did not exist in snapshot, deleting: file={}", op.file);
                    let _ = std::fs::remove_file(&op.file);
                }
            }
            i = j;
        }
    }

    /// `diff` (`snapshot/index.ts:526-544`).
    fn diff_impl(&self, hash: &str) -> String {
        let _guard = self.lock();
        self.add();
        let result = self.git(
            &self.args(
                &QUOTE,
                &["diff", "--cached", "--no-ext-diff", hash, "--", "."],
            ),
            Some(&self.worktree),
            None,
        );
        if result.exit_code != 0 {
            tracing::warn!(
                "failed to get diff: hash={hash} exitCode={} stderr={}",
                result.exit_code,
                result.stderr
            );
            return String::new();
        }
        result.text.trim().to_string()
    }

    /// `diffFull` (`snapshot/index.ts:546-759`).
    fn diff_full_impl(&self, from: &str, to: &str) -> Vec<FileDiff> {
        let _guard = self.lock();
        let mut statuses: HashMap<String, FileDiffStatus> = HashMap::new();
        let status_result = self.git(
            &self.args(
                &QUOTE,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--name-status",
                    "--no-renames",
                    from,
                    to,
                    "--",
                    ".",
                ],
            ),
            Some(&self.directory),
            None,
        );
        for line in status_result.text.trim().split('\n') {
            if line.is_empty() {
                continue;
            }
            let Some((code, file)) = line.split_once('\t') else {
                continue;
            };
            if code.is_empty() || file.is_empty() {
                continue;
            }
            let status = if code.starts_with('A') {
                FileDiffStatus::Added
            } else if code.starts_with('D') {
                FileDiffStatus::Deleted
            } else {
                FileDiffStatus::Modified
            };
            statuses.insert(file.to_string(), status);
        }

        let numstat = self.git(
            &self.args(
                &QUOTE,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-renames",
                    "--numstat",
                    from,
                    to,
                    "--",
                    ".",
                ],
            ),
            Some(&self.directory),
            None,
        );
        let mut rows: Vec<Row> = Vec::new();
        for line in numstat.text.trim().split('\n') {
            if line.is_empty() {
                continue;
            }
            let Some((adds, rest)) = line.split_once('\t') else {
                continue;
            };
            let Some((dels, file)) = rest.split_once('\t') else {
                continue;
            };
            if file.is_empty() {
                continue;
            }
            let binary = adds == "-" && dels == "-";
            let parse = |token: &str| {
                token
                    .parse::<f64>()
                    .ok()
                    .filter(|value| value.is_finite())
                    .unwrap_or_default()
            };
            let additions = if binary { 0.0 } else { parse(adds) };
            let deletions = if binary { 0.0 } else { parse(dels) };
            rows.push(Row {
                file: file.to_string(),
                status: statuses
                    .get(file)
                    .copied()
                    .unwrap_or(FileDiffStatus::Modified),
                binary,
                additions,
                deletions,
            });
        }

        // Hide ignored-file removals from the user-facing diff output.
        let ignored = self.ignore(&rows.iter().map(|row| row.file.clone()).collect::<Vec<_>>());
        if !ignored.is_empty() {
            rows.retain(|row| !ignored.contains(&row.file));
        }

        let step = 100;
        let mut result: Vec<FileDiff> = Vec::new();
        for run in rows.chunks(step) {
            let text = self.load(run, from, to);
            for row in run {
                let (before, after) = if row.binary {
                    (String::new(), String::new())
                } else if let Some(text) = text.as_ref() {
                    let hit = text.get(&row.file).cloned().unwrap_or_default();
                    (hit.before, hit.after)
                } else {
                    self.show(row, from, to)
                };
                result.push(FileDiff {
                    file: Some(row.file.clone()),
                    patch: Some(if row.binary {
                        String::new()
                    } else {
                        crate::tool::diff::format_patch(
                            &row.file,
                            &before,
                            &after,
                            crate::tool::diff::MAX_SAFE_INTEGER,
                        )
                    }),
                    additions: row.additions,
                    deletions: row.deletions,
                    status: Some(row.status),
                });
            }
        }
        result
    }

    /// `show` (`snapshot/index.ts:563-586`) — the per-file `git show`
    /// fallback.
    fn show(&self, row: &Row, from: &str, to: &str) -> (String, String) {
        if row.binary {
            return (String::new(), String::new());
        }
        if row.status == FileDiffStatus::Added {
            let after = self.git(
                &self.args(&CFG, &["show", &format!("{to}:{}", row.file)]),
                None,
                None,
            );
            return (String::new(), after.text);
        }
        if row.status == FileDiffStatus::Deleted {
            let before = self.git(
                &self.args(&CFG, &["show", &format!("{from}:{}", row.file)]),
                None,
                None,
            );
            return (before.text, String::new());
        }
        let before = self.git(
            &self.args(&CFG, &["show", &format!("{from}:{}", row.file)]),
            None,
            None,
        );
        let after = self.git(
            &self.args(&CFG, &["show", &format!("{to}:{}", row.file)]),
            None,
            None,
        );
        (before.text, after.text)
    }

    /// `load` (`snapshot/index.ts:588-682`) — the `cat-file --batch`
    /// bulk fetch, falling back to `None` (per-file show) on any failure.
    fn load(&self, rows: &[Row], from: &str, to: &str) -> Option<HashMap<String, BeforeAfter>> {
        let mut refs: Vec<(String, bool, String)> = Vec::new();
        for row in rows {
            if row.binary {
                continue;
            }
            match row.status {
                FileDiffStatus::Added => {
                    refs.push((row.file.clone(), false, format!("{to}:{}", row.file)));
                }
                FileDiffStatus::Deleted => {
                    refs.push((row.file.clone(), true, format!("{from}:{}", row.file)));
                }
                FileDiffStatus::Modified => {
                    refs.push((row.file.clone(), true, format!("{from}:{}", row.file)));
                    refs.push((row.file.clone(), false, format!("{to}:{}", row.file)));
                }
            }
        }
        if refs.is_empty() {
            return Some(HashMap::new());
        }
        let stdin = refs
            .iter()
            .map(|(_, _, r#ref)| r#ref.clone())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let args = self.args(&CFG, &["cat-file", "--batch"]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let batch = self
            .runner
            .run(Some(&self.directory), &[], &args, Some(&stdin));
        if batch.code != 0 {
            tracing::info!(
                "git cat-file --batch failed during snapshot diff, falling back to per-file git show: stderr={} refs={}",
                batch.stderr,
                refs.len()
            );
            return None;
        }
        parse_cat_file(&refs, &batch.stdout)
    }
}

impl GitSnapshot {
    /// `git gc --prune=7.days` — the cleanup Interface method
    /// (`snapshot/index.ts:300-316`), driven hourly by the loop.
    pub fn cleanup(&self) {
        self.cleanup_impl();
    }
}

impl Snapshot for GitSnapshot {
    // Multi-second `git` subprocesses run under Effect fibers in TS; here
    // they are sync fns moved onto the blocking pool so a slow `git add`
    // never stalls a tokio worker.

    fn track(&self) -> BoxFuture<'static, Option<String>> {
        let snapshot = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || snapshot.track_impl())
                .await
                .unwrap_or_default()
        })
    }

    fn patch(&self, id: &str) -> BoxFuture<'static, Result<SnapshotPatch, CoreError>> {
        let snapshot = self.clone();
        let id = id.to_string();
        Box::pin(async move {
            let hash = id.clone();
            Ok(
                tokio::task::spawn_blocking(move || snapshot.patch_impl(&id))
                    .await
                    .unwrap_or_else(|_| SnapshotPatch {
                        hash,
                        files: Vec::new(),
                    }),
            )
        })
    }

    fn restore(&self, id: &str) -> BoxFuture<'static, Result<(), CoreError>> {
        let snapshot = self.clone();
        let id = id.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || snapshot.restore_impl(&id))
                .await
                .map_err(|_| CoreError::Storage("snapshot task failed".to_string()))
        })
    }

    fn revert(&self, patches: Vec<PatchPart>) -> BoxFuture<'static, Result<(), CoreError>> {
        let snapshot = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || snapshot.revert_impl(patches))
                .await
                .map_err(|_| CoreError::Storage("snapshot task failed".to_string()))
        })
    }

    fn diff(&self, id: &str) -> BoxFuture<'static, Result<String, CoreError>> {
        let snapshot = self.clone();
        let id = id.to_string();
        Box::pin(async move {
            Ok(tokio::task::spawn_blocking(move || snapshot.diff_impl(&id))
                .await
                .unwrap_or_default())
        })
    }

    fn diff_full(
        &self,
        from: &str,
        to: &str,
    ) -> BoxFuture<'static, Result<Vec<FileDiff>, CoreError>> {
        let snapshot = self.clone();
        let from = from.to_string();
        let to = to.to_string();
        Box::pin(async move {
            Ok(
                tokio::task::spawn_blocking(move || snapshot.diff_full_impl(&from, &to))
                    .await
                    .unwrap_or_default(),
            )
        })
    }
}

/// A `diffFull` row (`snapshot/index.ts:549-555`).
struct Row {
    file: String,
    status: FileDiffStatus,
    binary: bool,
    additions: f64,
    deletions: f64,
}

/// One side of a `diffFull` content pair.
#[derive(Clone, Default)]
struct BeforeAfter {
    before: String,
    after: String,
}

/// The `cat-file --batch` response parser (`snapshot/index.ts:620-676`):
/// `<sha> blob <size>` headers, ` missing` markers, and truncated /
/// trailing output all fall back to `None` (per-file `git show`).
fn parse_cat_file(
    refs: &[(String, bool, String)],
    out: &[u8],
) -> Option<HashMap<String, BeforeAfter>> {
    let mut map: HashMap<String, BeforeAfter> = HashMap::new();
    let mut i = 0usize;
    for (file, before, _r) in refs {
        let end = out[i..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|found| found + i)?;
        let head = String::from_utf8_lossy(&out[i..end]).into_owned();
        i = end + 1;
        let hit = map.entry(file.clone()).or_default();
        if head.ends_with(" missing") {
            continue;
        }
        let size = blob_size(&head)?;
        if i + size >= out.len() || out[i + size] != b'\n' {
            return None;
        }
        let text = String::from_utf8_lossy(&out[i..i + size]).into_owned();
        if *before {
            hit.before = text;
        } else {
            hit.after = text;
        }
        i += size + 1;
    }
    if i != out.len() {
        return None;
    }
    Some(map)
}

/// The `^[0-9a-f]+ blob (\d+)$` header match.
fn blob_size(head: &str) -> Option<usize> {
    let (hex, size) = head.split_once(" blob ")?;
    if hex.is_empty() || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    if !size.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    size.parse::<usize>().ok()
}

fn encode_nul_paths(files: &[String]) -> String {
    let mut out = files.join("\0");
    out.push('\0');
    out
}

fn encode_pathspecs(files: &[String]) -> String {
    let pathspecs = files
        .iter()
        .map(|file| format!(":(top,literal){file}"))
        .collect::<Vec<_>>();
    encode_nul_paths(&pathspecs)
}

fn split_nul(text: &str) -> Vec<String> {
    text.split('\0')
        .filter(|item| !item.is_empty())
        .map(String::from)
        .collect()
}

fn relative(worktree: &Path, file: &str) -> String {
    let path = Path::new(file);
    let rel = match path.strip_prefix(worktree) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => path.to_path_buf(),
    };
    rel.to_string_lossy().replace('\\', "/")
}

/// `clash` (`snapshot/index.ts:445`).
fn clash(a: &str, b: &str) -> bool {
    a == b
        || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
}

/// The hourly `git gc --prune=7.days` loop (`snapshot/index.ts:761-766`):
/// one run after a 1-minute delay, then every hour.
fn spawn_cleanup_loop(snapshot: Arc<GitSnapshot>, first: Duration, interval: Duration) {
    std::thread::spawn(move || {
        let snapshot = Arc::downgrade(&snapshot);
        let mut delay = first;
        loop {
            let Some(snap) = snapshot.upgrade() else {
                return;
            };
            // Clone the shared stop signal and drop the strong snapshot
            // reference before waiting: the async `Snapshot` wrappers
            // clone the struct into `'static` futures (`track`, `patch`,
            // ...), and each clone's `Drop` re-arms the stop flag, so the
            // flag alone cannot distinguish a stale clone drop from the
            // final drop — the weak reference can.
            let cleanup = snap.cleanup.clone();
            drop(snap);
            let (lock, cvar) = &*cleanup;
            let Ok(mut stopped) = lock.lock() else {
                return;
            };
            loop {
                if *stopped {
                    *stopped = false;
                    if snapshot.upgrade().is_none() {
                        return;
                    }
                }
                let (next, timed_out) = match cvar.wait_timeout(stopped, delay) {
                    Ok(result) => (result.0, result.1.timed_out()),
                    Err(poisoned) => {
                        let (guard, result) = poisoned.into_inner();
                        (guard, result.timed_out())
                    }
                };
                stopped = next;
                if timed_out {
                    break;
                }
            }
            drop(stopped);
            if let Some(snap) = snapshot.upgrade() {
                snap.cleanup();
            }
            delay = interval;
        }
    });
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
        // Soft-fail to an empty patch when the snapshot is unknown —
        // the git snapshot logs a warning and returns `{hash, files: []}`
        // (snapshot/index.ts:349-361).
        let (id, patch) = self.compute_patch(id);
        Box::pin(async move {
            let files = patch.unwrap_or_default();
            Ok(SnapshotPatch { hash: id, files })
        })
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
        // TS `patch()` hands out absolute worktree paths
        // (`path.join(state.worktree, x)`).
        let files = files
            .into_iter()
            .map(|path| self.worktree.join(&path).to_string_lossy().into_owned())
            .collect();
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

// ---------------------------------------------------------------------------
// Tests — real-git integration against tempdir repositories (spec M7 §6.1)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod git_tests {
    use super::*;
    use crate::git::{GitRunner, SubprocessGit};
    use crate::storage::test_support::TempDir;
    use alforria_schema::file_diff::FileDiffStatus;

    fn init_repo(dir: &Path) {
        let git = SubprocessGit;
        assert_eq!(
            git.run(Some(dir), &["init", "--quiet"]).exit_code,
            0,
            "git init"
        );
        git.run(Some(dir), &["config", "user.email", "t@e.st"]);
        git.run(Some(dir), &["config", "user.name", "T"]);
    }

    fn commit_all(dir: &Path) {
        let git = SubprocessGit;
        git.run(Some(dir), &["add", "-A"]);
        git.run(
            Some(dir),
            &["commit", "--quiet", "--allow-empty", "-m", "x"],
        );
    }

    fn snapshot(dir: &Path) -> (Arc<GitSnapshot>, TempDir) {
        // `Global.Path.data` lives outside the worktree — the snapshot
        // store must never observe itself.
        let data = TempDir::new("snap-data");
        let snap = GitSnapshot::with_cleanup(
            GitSnapshotInput {
                directory: dir.to_path_buf(),
                worktree: dir.to_path_buf(),
                project_id: "test".to_string(),
                vcs_is_git: true,
                snapshot_enabled: true,
                data: data.path().to_path_buf(),
            },
            Duration::from_secs(3600),
            Duration::from_secs(3600),
        );
        (snap, data)
    }

    fn gitdir(data: &Path, worktree: &Path) -> PathBuf {
        data.join("snapshot/test")
            .join(crate::project::hash_fast(&worktree.to_string_lossy()))
    }

    fn tree_files(gitdir: &Path, tree: &str) -> Vec<String> {
        let git = SubprocessGit;
        let result = git.run(Some(gitdir), &["ls-tree", "-r", "--name-only", tree]);
        assert_eq!(result.exit_code, 0, "ls-tree: {}", result.stderr);
        result
            .text
            .trim()
            .split('\n')
            .filter(|line| !line.is_empty())
            .map(String::from)
            .collect()
    }

    #[tokio::test]
    async fn track_patch_revert_round_trip() {
        let dir = TempDir::new("snap-round");
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        commit_all(dir.path());

        let (snap, _data) = snapshot(dir.path());
        let base = snap.track().await.expect("snapshot hash");
        assert!(base.len() >= 40, "write-tree hash: {base}");

        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "new\n").unwrap();
        std::fs::remove_file(dir.path().join("b.txt")).unwrap();

        let patch = snap.patch(&base).await.unwrap();
        let mut files = patch.files.clone();
        files.sort();
        assert_eq!(
            files,
            vec![
                dir.path().join("a.txt").to_string_lossy().into_owned(),
                dir.path().join("b.txt").to_string_lossy().into_owned(),
                dir.path().join("new.txt").to_string_lossy().into_owned(),
            ]
        );

        snap.revert(vec![PatchPart {
            hash: base.clone(),
            files: patch.files.clone(),
        }])
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "two\n"
        );
        assert!(
            !dir.path().join("new.txt").exists(),
            "file absent from the snapshot is deleted"
        );
    }

    #[tokio::test]
    async fn restore_checks_out_snapshot() {
        let dir = TempDir::new("snap-restore");
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "before\n").unwrap();
        commit_all(dir.path());

        let (snap, _data) = snapshot(dir.path());
        let base = snap.track().await.unwrap();

        std::fs::write(dir.path().join("a.txt"), "after\n").unwrap();
        snap.restore(&base).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "before\n"
        );
    }

    #[tokio::test]
    async fn ignored_files_are_excluded_from_patches() {
        let dir = TempDir::new("snap-ignore");
        init_repo(dir.path());
        std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        commit_all(dir.path());

        let (snap, _data) = snapshot(dir.path());
        let base = snap.track().await.unwrap();

        std::fs::write(dir.path().join("app.log"), "noise\n").unwrap();
        std::fs::write(dir.path().join("app.txt"), "kept\n").unwrap();
        let patch = snap.patch(&base).await.unwrap();
        assert_eq!(
            patch.files,
            vec![dir.path().join("app.txt").to_string_lossy().into_owned()],
            "gitignored file is filtered from patch output"
        );
    }

    #[tokio::test]
    async fn large_untracked_files_are_blocked() {
        let dir = TempDir::new("snap-large");
        init_repo(dir.path());
        std::fs::write(dir.path().join("small.txt"), "tiny\n").unwrap();
        commit_all(dir.path());

        let (snap, data) = snapshot(dir.path());
        snap.track().await.unwrap();

        std::fs::write(dir.path().join("big.bin"), vec![b'a'; 2 * 1024 * 1024 + 1]).unwrap();
        let next = snap.track().await.unwrap();
        let files = tree_files(&gitdir(data.path(), dir.path()), &next);
        assert!(!files.contains(&"big.bin".to_string()), "large file staged");
        assert!(files.contains(&"small.txt".to_string()));

        let exclude =
            std::fs::read_to_string(gitdir(data.path(), dir.path()).join("info/exclude")).unwrap();
        assert!(
            exclude.contains("/big.bin"),
            "blocked via exclude: {exclude}"
        );
    }

    #[tokio::test]
    async fn seed_shares_the_source_object_database() {
        let dir = TempDir::new("snap-seed");
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        commit_all(dir.path());

        let (snap, data) = snapshot(dir.path());
        snap.track().await.unwrap();

        let gitdir = gitdir(data.path(), dir.path());
        let alternates = std::fs::read_to_string(gitdir.join("objects/info/alternates")).unwrap();
        assert!(
            alternates.contains(".git/objects"),
            "alternates points at the source object database: {alternates}"
        );
        assert!(gitdir.join("index").exists(), "source index copied");
    }

    #[tokio::test]
    async fn disabled_snapshot_returns_none() {
        let dir = TempDir::new("snap-disabled");
        init_repo(dir.path());
        commit_all(dir.path());
        let data = TempDir::new("snap-data");

        let disabled = GitSnapshot::with_cleanup(
            GitSnapshotInput {
                directory: dir.path().to_path_buf(),
                worktree: dir.path().to_path_buf(),
                project_id: "test".to_string(),
                vcs_is_git: false,
                snapshot_enabled: true,
                data: data.path().to_path_buf(),
            },
            Duration::from_secs(3600),
            Duration::from_secs(3600),
        );
        assert_eq!(futures::executor::block_on(disabled.track()), None);

        let off = GitSnapshot::with_cleanup(
            GitSnapshotInput {
                directory: dir.path().to_path_buf(),
                worktree: dir.path().to_path_buf(),
                project_id: "test".to_string(),
                vcs_is_git: true,
                snapshot_enabled: false,
                data: data.path().to_path_buf(),
            },
            Duration::from_secs(3600),
            Duration::from_secs(3600),
        );
        assert_eq!(futures::executor::block_on(off.track()), None);
    }

    #[tokio::test]
    async fn diff_returns_unified_diff_text() {
        let dir = TempDir::new("snap-diff");
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        commit_all(dir.path());

        let (snap, _data) = snapshot(dir.path());
        let base = snap.track().await.unwrap();

        std::fs::write(dir.path().join("a.txt"), "one\nTWO\n").unwrap();
        let diff = snap.diff(&base).await.unwrap();
        assert!(diff.starts_with("diff --git a/a.txt b/a.txt\n"));
        assert!(diff.ends_with("@@ -1,2 +1,2 @@\n one\n-two\n+TWO"));
    }

    #[tokio::test]
    async fn diff_full_between_two_snapshots() {
        let dir = TempDir::new("snap-diff-full");
        init_repo(dir.path());
        std::fs::write(dir.path().join("keep.txt"), "keep\n").unwrap();
        std::fs::write(dir.path().join("del.txt"), "delete me\n").unwrap();
        std::fs::write(dir.path().join("mod.txt"), "before\n").unwrap();
        commit_all(dir.path());

        let (snap, _data) = snapshot(dir.path());
        let from = snap.track().await.unwrap();

        std::fs::remove_file(dir.path().join("del.txt")).unwrap();
        std::fs::write(dir.path().join("mod.txt"), "after\n").unwrap();
        std::fs::write(dir.path().join("add.txt"), "fresh\n").unwrap();
        std::fs::write(dir.path().join("bin.dat"), b"bi\x00nary\n").unwrap();
        let to = snap.track().await.unwrap();

        let mut diffs = snap.diff_full(&from, &to).await.unwrap();
        diffs.sort_by(|a, b| a.file.cmp(&b.file));

        let add = diffs
            .iter()
            .find(|d| d.file.as_deref() == Some("add.txt"))
            .unwrap();
        assert_eq!(add.status, Some(FileDiffStatus::Added));
        assert_eq!(add.additions, 1.0);
        assert_eq!(add.deletions, 0.0);
        assert_eq!(
            add.patch.as_deref().unwrap().trim_end(),
            "Index: add.txt\n===================================================================\n--- add.txt\t\n+++ add.txt\t\n@@ -0,0 +1,1 @@\n+fresh"
        );

        let del = diffs
            .iter()
            .find(|d| d.file.as_deref() == Some("del.txt"))
            .unwrap();
        assert_eq!(del.status, Some(FileDiffStatus::Deleted));
        assert_eq!(del.additions, 0.0);
        assert_eq!(del.deletions, 1.0);

        let modified = diffs
            .iter()
            .find(|d| d.file.as_deref() == Some("mod.txt"))
            .unwrap();
        assert_eq!(modified.status, Some(FileDiffStatus::Modified));
        assert_eq!(modified.additions, 1.0);
        assert_eq!(modified.deletions, 1.0);

        let bin = diffs
            .iter()
            .find(|d| d.file.as_deref() == Some("bin.dat"))
            .unwrap();
        assert_eq!(bin.additions, 0.0);
        assert_eq!(bin.deletions, 0.0);
        assert_eq!(bin.patch.as_deref(), Some(""), "binary patch is empty");

        assert!(
            !diffs.iter().any(|d| d.file.as_deref() == Some("keep.txt")),
            "unchanged files are absent"
        );
    }

    #[tokio::test]
    async fn revert_single_file_uses_the_single_op_path() {
        let dir = TempDir::new("snap-revert-single");
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        commit_all(dir.path());

        let (snap, _data) = snapshot(dir.path());
        let base = snap.track().await.unwrap();

        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        std::fs::write(dir.path().join("spawned.txt"), "created\n").unwrap();
        snap.revert(vec![PatchPart {
            hash: base,
            files: vec![dir.path().join("a.txt").to_string_lossy().into_owned()],
        }])
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\n"
        );
    }

    #[tokio::test]
    async fn cleanup_packs_the_snapshot_store() {
        let dir = TempDir::new("snap-cleanup");
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        commit_all(dir.path());

        let (snap, data) = snapshot(dir.path());
        // A file the source repo does not have forces a fresh blob into the
        // snapshot store — `git gc` has something to pack.
        std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        snap.track().await.unwrap();
        snap.cleanup();

        let gitdir = gitdir(data.path(), dir.path());
        let packs: Vec<_> = std::fs::read_dir(gitdir.join("objects/pack"))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(!packs.is_empty(), "git gc created packs");
    }

    #[tokio::test]
    async fn cleanup_loop_fires_after_the_first_delay() {
        let dir = TempDir::new("snap-loop");
        init_repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        commit_all(dir.path());
        let data = TempDir::new("snap-data");

        // The interval must stay short enough to retry: the first gc can
        // fire before `track()` has created the snapshot repo, and
        // `cleanup` then early-returns — a hour-long production
        // interval would never retry inside the test budget. One
        // second retries promptly without colliding gc runs into
        // `git gc` lock/gc.log contention.
        let snap = GitSnapshot::with_cleanup(
            GitSnapshotInput {
                directory: dir.path().to_path_buf(),
                worktree: dir.path().to_path_buf(),
                project_id: "test".to_string(),
                vcs_is_git: true,
                snapshot_enabled: true,
                data: data.path().to_path_buf(),
            },
            Duration::from_millis(10),
            Duration::from_secs(1),
        );
        std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        snap.track().await.unwrap();
        let gitdir = gitdir(data.path(), dir.path());
        let mut packed = false;
        // Generous budget — `git gc` under a fully parallel test load can
        // far exceed the usual milliseconds.
        for _ in 0..600 {
            if gitdir.join("objects/pack").exists()
                && std::fs::read_dir(gitdir.join("objects/pack"))
                    .unwrap()
                    .filter_map(Result::ok)
                    .next()
                    .is_some()
            {
                packed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(packed, "the cleanup loop ran gc");
    }

    #[test]
    fn cat_file_parser_matrix() {
        let refs = vec![
            ("a.txt".to_string(), true, "h1:a.txt".to_string()),
            ("b.txt".to_string(), false, "h2:b.txt".to_string()),
        ];
        let head = "0123456789abcdef0123456789abcdef01234567";

        // Valid: header + content, then a missing marker.
        let valid = format!("{} blob 6\nhello \nh2:b.txt missing\n", head,);
        let parsed = parse_cat_file(&refs, valid.as_bytes()).unwrap();
        assert_eq!(parsed.get("a.txt").unwrap().before, "hello ");
        assert_eq!(parsed.get("b.txt").unwrap().after, "");

        // Truncated header (no trailing newline at all).
        assert!(parse_cat_file(&refs, b"0123456789abcdef0123456789abcdef01234567 blob").is_none());

        // Unexpected header.
        assert!(parse_cat_file(&refs, b"not a blob header\n").is_none());

        // Truncated content (declared size runs past the output).
        let truncated_content = format!("{head} blob 6\nabc\n");
        assert!(parse_cat_file(&refs, truncated_content.as_bytes()).is_none());

        // Trailing data after all refs consumed.
        let trailing = format!("{head} blob 1\na\nextra\n");
        assert!(parse_cat_file(&refs, trailing.as_bytes()).is_none());
    }

    #[tokio::test]
    async fn clash_matrix() {
        assert!(clash("a/b", "a/b"));
        assert!(clash("a/b/c", "a/b"));
        assert!(clash("a/b", "a/b/c"));
        assert!(!clash("a/bc", "a/b"));
        assert!(!clash("a", "b"));
    }
}
