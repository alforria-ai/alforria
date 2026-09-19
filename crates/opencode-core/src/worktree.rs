//! The worktree service — port of `packages/opencode/src/worktree/index.ts`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::git::{GitRunner, SubprocessGit};

/// `Worktree.Info` (`worktree/index.ts:23-28`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Info {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub directory: String,
}

/// `Worktree.CreateInput` (`worktree/index.ts:30-35`).
#[derive(Debug, Clone, Default)]
pub struct CreateInput {
    pub name: Option<String>,
    pub start_command: Option<String>,
}

/// One `worktree.ready` / `worktree.failed` GlobalBus frame (`boot`).
pub struct Frame {
    pub directory: PathBuf,
    pub project_id: String,
    pub workspace_id: Option<String>,
    pub event_type: &'static str,
    pub properties: serde_json::Value,
}

/// Instance/environment access — TS reaches `Project.Service`,
/// `InstanceStore` and the GlobalBus through Effect layers; Rust injects
/// them behind this seam.
pub trait Deps: Send + Sync {
    /// `Global.Path.data`.
    fn data_dir(&self) -> &Path;
    /// `project.addSandbox` (`setup`) — errors swallowed.
    fn add_sandbox(&self, project_id: &str, directory: &Path);
    /// `store.disposeDirectory` (`remove`).
    fn dispose_directory(&self, directory: &Path);
    /// `store.load` (`boot`).
    fn load_instance(&self, directory: &Path) -> Result<(), String>;
    /// `project.commands.start` (`runStartScripts`).
    fn start_command(&self, project_id: &str) -> Option<String>;
    /// `GlobalBus.emit("event", …)` (`boot`).
    fn emit(&self, frame: &Frame);
}

/// The error set (`worktree/index.ts:48-81`) — every variant maps to the
/// `WorktreeApiError { name, data: { message } }` wire shape via its tag.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    pub tag: &'static str,
    pub message: String,
}

impl Error {
    fn new(tag: &'static str, message: impl Into<String>) -> Error {
        Error {
            tag,
            message: message.into(),
        }
    }
}

const NOT_GIT: &str = "Worktrees are only supported for git projects";

/// `InstanceState.context` — the bits the worktree service reads.
#[derive(Debug, Clone)]
pub struct Context {
    pub project_id: String,
    /// `ctx.project.worktree`.
    pub project_worktree: PathBuf,
    /// `ctx.worktree`.
    pub worktree: PathBuf,
    pub workspace_id: Option<String>,
    /// `ctx.project.vcs === "git"`.
    pub is_git: bool,
}

/// `slugify` (`worktree/index.ts:91-98`).
fn slugify(input: &str) -> String {
    let lowered = input.trim().to_lowercase();
    let mut out = String::new();
    for char in lowered.chars() {
        if char.is_ascii_lowercase() || char.is_ascii_digit() {
            out.push(char);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// `canonical` (`worktree/index.ts:295-300`) — `path.resolve`, then realpath
/// when it exists, then normalize (unix casing).
fn canonical(input: &Path) -> PathBuf {
    let abs = if input.is_absolute() {
        input.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(input)
    };
    let abs = crate::git::lexical_normalize(&abs);
    match std::fs::canonicalize(&abs) {
        Ok(real) => real,
        Err(_) => abs,
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `parseWorktreeList` (`worktree/index.ts:302-319`).
#[derive(Debug, Default)]
struct ListEntry {
    path: Option<String>,
    branch: Option<String>,
}

fn parse_worktree_list(text: &str) -> Vec<ListEntry> {
    let mut out: Vec<ListEntry> = Vec::new();
    for line in text.split('\n').map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            out.push(ListEntry {
                path: Some(path.trim().to_string()),
                branch: None,
            });
            continue;
        }
        if let Some(branch) = line.strip_prefix("branch ") {
            if let Some(current) = out.last_mut() {
                current.branch = Some(branch.trim().to_string());
            }
        }
    }
    out
}

/// `locateWorktree` (`worktree/index.ts:321-331`).
fn locate_worktree<'a>(entries: &'a [ListEntry], directory: &Path) -> Option<&'a ListEntry> {
    entries.iter().find(|item| match &item.path {
        Some(path) => canonical(Path::new(path)) == directory,
        None => false,
    })
}

/// `failedRemoves` (`worktree/index.ts:100-113`).
fn failed_removes(chunks: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in chunks {
        for line in chunk.split('\n').map(str::trim) {
            let Some(rest) = line.strip_prefix("warning:").map(str::trim_start) else {
                continue;
            };
            let Some(value) = rest.strip_prefix("failed to remove").map(str::trim_start) else {
                continue;
            };
            let Some((value, _)) = value.split_once(':') else {
                continue;
            };
            let value = value
                .trim()
                .trim_start_matches(['\'', '"'])
                .trim_end_matches(['\'', '"'])
                .to_string();
            if !value.is_empty() {
                out.push(value);
            }
        }
    }
    out
}

const MAX_NAME_ATTEMPTS: usize = 26;

/// The worktree service (`worktree/index.ts:119-127`).
#[derive(Clone)]
pub struct Worktree {
    git: Arc<dyn GitRunner>,
}

impl Default for Worktree {
    fn default() -> Self {
        Worktree::new(Arc::new(SubprocessGit))
    }
}

impl Worktree {
    pub fn new(git: Arc<dyn GitRunner>) -> Worktree {
        Worktree { git }
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> crate::git::GitResult {
        self.git.run(Some(cwd), args)
    }

    fn list_porcelain(&self, ctx: &Context, tag: &'static str) -> Result<Vec<ListEntry>, Error> {
        let result = self.git(&ctx.worktree, &["worktree", "list", "--porcelain"]);
        if result.exit_code != 0 {
            return Err(Error::new(
                tag,
                failure_message(&result.stderr, &result.text, "Failed to read git worktrees"),
            ));
        }
        Ok(parse_worktree_list(&result.text))
    }

    /// `makeWorktreeInfo` + `candidate` (`worktree/index.ts:174-212`).
    pub fn make_worktree_info(
        &self,
        deps: &dyn Deps,
        ctx: &Context,
        name: Option<&str>,
        detached: bool,
    ) -> Result<Info, Error> {
        if !ctx.is_git {
            return Err(Error::new("WorktreeNotGitError", NOT_GIT));
        }
        let root = deps.data_dir().join("worktree").join(&ctx.project_id);
        let _ = std::fs::create_dir_all(&root);
        let name_input = name.filter(|name| !name.is_empty());
        for attempt in 0..MAX_NAME_ATTEMPTS {
            let name = match name_input {
                Some(name) if attempt == 0 => slugify(name),
                Some(name) => {
                    format!(
                        "{}-{}",
                        slugify(name),
                        crate::session::agents::slug_create()
                    )
                }
                None => crate::session::agents::slug_create(),
            };
            let branch = (!detached).then(|| format!("opencode/{name}"));
            let directory = root.join(&name);
            if directory.exists() {
                continue;
            }
            if let Some(branch) = &branch {
                let reference = format!("refs/heads/{branch}");
                let check = self.git.run(
                    Some(&ctx.worktree),
                    &["show-ref", "--verify", "--quiet", &reference],
                );
                if check.exit_code == 0 {
                    continue;
                }
            }
            return Ok(Info {
                name,
                branch,
                directory: directory.to_string_lossy().into_owned(),
            });
        }
        Err(Error::new(
            "WorktreeNameGenerationFailedError",
            "Failed to generate a unique worktree name",
        ))
    }

    /// `setup` (`worktree/index.ts:214-229`) + `boot` (`:231-279`) — the TS
    /// `boot` continuation is forked; the Rust port runs it inline.
    pub fn create_from_info(
        &self,
        deps: &dyn Deps,
        ctx: &Context,
        info: &Info,
        start_command: Option<&str>,
    ) -> Result<(), Error> {
        let created = match info.branch.as_deref() {
            Some(branch) => self.git(
                &ctx.worktree,
                &[
                    "worktree",
                    "add",
                    "--no-checkout",
                    "-b",
                    branch,
                    &info.directory,
                ],
            ),
            None => self.git(
                &ctx.worktree,
                &[
                    "worktree",
                    "add",
                    "--no-checkout",
                    "--detach",
                    &info.directory,
                    "HEAD",
                ],
            ),
        };
        if created.exit_code != 0 {
            let message = failure_message(
                &created.stderr,
                &created.text,
                "Failed to create git worktree",
            );
            return Err(Error::new("WorktreeCreateFailedError", message));
        }
        deps.add_sandbox(&ctx.project_id, Path::new(&info.directory));
        self.boot(deps, ctx, info, start_command);
        Ok(())
    }

    /// `boot` (`worktree/index.ts:231-279`).
    fn boot(&self, deps: &dyn Deps, ctx: &Context, info: &Info, start_command: Option<&str>) {
        let directory = PathBuf::from(&info.directory);
        let populated = self.git(&directory, &["reset", "--hard"]);
        if populated.exit_code != 0 {
            let message = failure_message(
                &populated.stderr,
                &populated.text,
                "Failed to populate worktree",
            );
            tracing::error!(directory = %directory.display(), message, "worktree checkout failed");
            deps.emit(&Frame {
                directory: directory.clone(),
                project_id: ctx.project_id.clone(),
                workspace_id: ctx.workspace_id.clone(),
                event_type: "worktree.failed",
                properties: serde_json::json!({ "message": message }),
            });
            return;
        }

        if let Err(message) = deps.load_instance(&directory) {
            tracing::error!(directory = %directory.display(), message, "worktree bootstrap failed");
            deps.emit(&Frame {
                directory: directory.clone(),
                project_id: ctx.project_id.clone(),
                workspace_id: ctx.workspace_id.clone(),
                event_type: "worktree.failed",
                properties: serde_json::json!({ "message": message }),
            });
            return;
        }

        deps.emit(&Frame {
            directory: directory.clone(),
            project_id: ctx.project_id.clone(),
            workspace_id: ctx.workspace_id.clone(),
            event_type: "worktree.ready",
            properties: serde_json::json!({
                "name": info.name,
                "branch": info.branch,
            }),
        });
        run_start_scripts(deps, ctx, &directory, start_command.unwrap_or(""));
    }

    /// `create` (`worktree/index.ts:289-293`).
    pub fn create(
        &self,
        deps: &dyn Deps,
        ctx: &Context,
        input: Option<&CreateInput>,
    ) -> Result<Info, Error> {
        let input = input.cloned().unwrap_or_default();
        let info = self.make_worktree_info(deps, ctx, input.name.as_deref(), false)?;
        self.create_from_info(deps, ctx, &info, input.start_command.as_deref())?;
        Ok(info)
    }

    /// `list` (`worktree/index.ts:333-359`).
    pub fn list(&self, ctx: &Context) -> Result<Vec<Info>, Error> {
        if !ctx.is_git {
            return Ok(Vec::new());
        }
        let entries = self.list_porcelain(ctx, "WorktreeListFailedError")?;
        let primary = canonical(&ctx.project_worktree);
        let primary_name = file_name(&primary).to_lowercase();
        let mut out = Vec::new();
        for entry in entries {
            let Some(path) = &entry.path else {
                continue;
            };
            let directory = canonical(Path::new(path));
            if directory == primary {
                continue;
            }
            let name = file_name(&directory).to_lowercase();
            out.push(Info {
                name: if name == primary_name {
                    file_name(directory.parent().unwrap_or(&directory))
                } else {
                    name
                },
                directory: directory.to_string_lossy().into_owned(),
                branch: entry
                    .branch
                    .as_deref()
                    .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b).to_string()),
            });
        }
        Ok(out)
    }

    /// `stopFsmonitor` (`worktree/index.ts:361-366`).
    fn stop_fsmonitor(&self, target: &Path) {
        if target.exists() {
            self.git(target, &["fsmonitor--daemon", "stop"]);
        }
    }

    /// `cleanDirectory` (`worktree/index.ts:368-385`) — `rm -rf` with
    /// retries (5 on unix).
    fn clean_directory(&self, target: &Path) -> Result<(), Error> {
        for attempt in 0..5 {
            let result = std::fs::remove_dir_all(target).or_else(|_| std::fs::remove_file(target));
            match result {
                Ok(()) => return Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(_) if attempt == 4 => {
                    return Err(Error::new(
                        "WorktreeRemoveFailedError",
                        "Failed to remove git worktree directory",
                    ));
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        }
        Ok(())
    }

    /// `remove` (`worktree/index.ts:388-449`).
    pub fn remove(&self, deps: &dyn Deps, ctx: &Context, input: &str) -> Result<bool, Error> {
        if !ctx.is_git {
            return Err(Error::new("WorktreeNotGitError", NOT_GIT));
        }
        let directory = canonical(Path::new(input));
        if directory != canonical(&ctx.worktree) {
            // The loaded path casing is preserved for the store cache
            // (`directory` is normalized on Windows).
            deps.dispose_directory(Path::new(input));
        }
        let entries = self.list_porcelain(ctx, "WorktreeRemoveFailedError")?;
        let entry = locate_worktree(&entries, &directory);
        let Some(entry) = entry.filter(|entry| entry.path.is_some()) else {
            if directory.exists() {
                self.stop_fsmonitor(&directory);
                self.clean_directory(&directory)?;
            }
            return Ok(true);
        };
        let entry_path = PathBuf::from(entry.path.clone().unwrap_or_default());
        deps.dispose_directory(&entry_path);
        self.stop_fsmonitor(&entry_path);
        let removed = self.git(
            &ctx.worktree,
            &[
                "worktree",
                "remove",
                "--force",
                &entry_path.to_string_lossy(),
            ],
        );
        if removed.exit_code != 0 {
            let next = self.list_porcelain(ctx, "WorktreeRemoveFailedError")?;
            if locate_worktree(&next, &directory).is_some_and(|item| item.path.is_some()) {
                return Err(Error::new(
                    "WorktreeRemoveFailedError",
                    failure_message(
                        &removed.stderr,
                        &removed.text,
                        "Failed to remove git worktree",
                    ),
                ));
            }
        }
        self.clean_directory(&entry_path)?;
        if let Some(branch) = entry
            .branch
            .as_deref()
            .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b))
            .filter(|b| !b.is_empty())
        {
            let deleted = self.git(&ctx.worktree, &["branch", "-D", branch]);
            if deleted.exit_code != 0 {
                return Err(Error::new(
                    "WorktreeRemoveFailedError",
                    failure_message(
                        &deleted.stderr,
                        &deleted.text,
                        "Failed to delete worktree branch",
                    ),
                ));
            }
        }
        Ok(true)
    }

    /// `prune` (`worktree/index.ts:499-512`).
    fn prune(&self, root: &Path, entries: &[String]) {
        let base = canonical(root);
        let prefix = base.join("");
        for entry in entries {
            let target = canonical(&root.join(entry));
            if target == base || !target.starts_with(&prefix) {
                continue;
            }
            let _ = std::fs::remove_dir_all(&target);
        }
    }

    /// `sweep` (`worktree/index.ts:514-523`).
    fn sweep(&self, root: &Path) -> crate::git::GitResult {
        let first = self.git(root, &["clean", "-ffdx"]);
        if first.exit_code == 0 {
            return first;
        }
        let text = first.text.clone();
        let stderr = first.stderr.clone();
        let entries = failed_removes(&[&stderr, &text]);
        if entries.is_empty() {
            return first;
        }
        self.prune(root, &entries);
        self.git(root, &["clean", "-ffdx"])
    }

    /// `reset` (`worktree/index.ts:525-611`).
    pub fn reset(&self, deps: &dyn Deps, ctx: &Context, input: &str) -> Result<bool, Error> {
        if !ctx.is_git {
            return Err(Error::new("WorktreeNotGitError", NOT_GIT));
        }
        let directory = canonical(Path::new(input));
        let primary = canonical(&ctx.worktree);
        if directory == primary {
            return Err(Error::new(
                "WorktreeResetFailedError",
                "Cannot reset the primary workspace",
            ));
        }
        let entries = self.list_porcelain(ctx, "WorktreeResetFailedError")?;
        let Some(entry) = locate_worktree(&entries, &directory) else {
            return Err(Error::new("WorktreeResetFailedError", "Worktree not found"));
        };
        if entry.path.is_none() {
            return Err(Error::new("WorktreeResetFailedError", "Worktree not found"));
        }
        let worktree_path = PathBuf::from(entry.path.clone().unwrap_or_default());

        let base = crate::vcs::GitCli.default_branch(&ctx.worktree);
        let Some(base) = base else {
            return Err(Error::new(
                "WorktreeResetFailedError",
                "Default branch not found",
            ));
        };

        if base.ref_ != base.name {
            if let Some(sep) = base.ref_.find('/') {
                if sep > 0 {
                    let remote = &base.ref_[..sep];
                    let branch = &base.ref_[sep + 1..];
                    let fetched = self.git(&ctx.worktree, &["fetch", remote, branch]);
                    if fetched.exit_code != 0 {
                        return Err(Error::new(
                            "WorktreeResetFailedError",
                            failure_message(
                                &fetched.stderr,
                                &fetched.text,
                                &format!("Failed to fetch {}", base.ref_),
                            ),
                        ));
                    }
                }
            }
        }

        let reset = self.git(&worktree_path, &["reset", "--hard", &base.ref_]);
        if reset.exit_code != 0 {
            return Err(Error::new(
                "WorktreeResetFailedError",
                failure_message(
                    &reset.stderr,
                    &reset.text,
                    "Failed to reset worktree to target",
                ),
            ));
        }

        let clean_result = self.sweep(&worktree_path);
        if clean_result.exit_code != 0 {
            return Err(Error::new(
                "WorktreeResetFailedError",
                failure_message(
                    &clean_result.stderr,
                    &clean_result.text,
                    "Failed to clean worktree",
                ),
            ));
        }

        let submodule_commands: [(&[&str], &str); 3] = [
            (
                &["submodule", "update", "--init", "--recursive", "--force"],
                "Failed to update submodules",
            ),
            (
                &[
                    "submodule",
                    "foreach",
                    "--recursive",
                    "git",
                    "reset",
                    "--hard",
                ],
                "Failed to reset submodules",
            ),
            (
                &[
                    "submodule",
                    "foreach",
                    "--recursive",
                    "git",
                    "clean",
                    "-fdx",
                ],
                "Failed to clean submodules",
            ),
        ];
        for (args, fallback) in submodule_commands {
            let result = self.git(&worktree_path, args);
            if result.exit_code != 0 {
                return Err(Error::new(
                    "WorktreeResetFailedError",
                    failure_message(&result.stderr, &result.text, fallback),
                ));
            }
        }

        let status = self.git(
            &worktree_path,
            &["-c", "core.fsmonitor=false", "status", "--porcelain=v1"],
        );
        if status.exit_code != 0 {
            return Err(Error::new(
                "WorktreeResetFailedError",
                failure_message(&status.stderr, &status.text, "Failed to read git status"),
            ));
        }
        if !status.text.trim().is_empty() {
            return Err(Error::new(
                "WorktreeResetFailedError",
                format!("Worktree reset left local changes:\n{}", status.text.trim()),
            ));
        }

        run_start_scripts(deps, ctx, &worktree_path, "");
        Ok(true)
    }
}

/// `stderr || text || fallback` (`worktree/index.ts:222-224` etc.).
fn failure_message(stderr: &str, text: &str, fallback: &str) -> String {
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    let text = text.trim();
    if !text.is_empty() {
        return text.to_string();
    }
    fallback.to_string()
}

/// `runStartScripts` (`worktree/index.ts:481-497`).
fn run_start_scripts(deps: &dyn Deps, ctx: &Context, directory: &Path, extra: &str) -> bool {
    let startup = deps.start_command(&ctx.project_id).unwrap_or_default();
    if !run_start_script(directory, &startup, "project") {
        return false;
    }
    run_start_script(directory, extra, "worktree")
}

/// `runStartScript` (`worktree/index.ts:472-479`) over `runStartCommand`
/// (`:461-470`) — `bash -lc <cmd>`.
fn run_start_script(directory: &Path, cmd: &str, kind: &str) -> bool {
    let text = cmd.trim();
    if text.is_empty() {
        return true;
    }
    let code = match Command::new("bash")
        .args(["-lc", text])
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 1,
    };
    if code == 0 {
        return true;
    }
    tracing::error!(kind, directory = %directory.display(), code, "worktree start command failed");
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_support::TempDir;
    use std::sync::Mutex;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed in {}", dir.display());
    }

    fn repo(tag: &str) -> (TempDir, PathBuf) {
        let temp = TempDir::new(tag);
        let dir = temp.path().to_path_buf();
        git(&dir, &["init", "--quiet", "-b", "main"]);
        git(&dir, &["config", "user.email", "test@opencode.test"]);
        git(&dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "--quiet", "-m", "root"]);
        (temp, dir)
    }

    /// A recording [Deps] double.
    #[derive(Default)]
    struct Deps {
        data_dir: PathBuf,
        fail_load: bool,
        start_command: Option<String>,
        sandboxes: Mutex<Vec<String>>,
        disposed: Mutex<Vec<String>>,
        loaded: Mutex<Vec<String>>,
        events: Mutex<Vec<Frame>>,
    }

    impl super::Deps for Deps {
        fn data_dir(&self) -> &Path {
            &self.data_dir
        }
        fn add_sandbox(&self, project_id: &str, directory: &Path) {
            self.sandboxes
                .lock()
                .unwrap()
                .push(format!("{project_id}:{}", directory.display()));
        }
        fn dispose_directory(&self, directory: &Path) {
            self.disposed
                .lock()
                .unwrap()
                .push(directory.to_string_lossy().into_owned());
        }
        fn load_instance(&self, directory: &Path) -> Result<(), String> {
            self.loaded
                .lock()
                .unwrap()
                .push(directory.to_string_lossy().into_owned());
            if self.fail_load {
                Err("boom".to_string())
            } else {
                Ok(())
            }
        }
        fn start_command(&self, _: &str) -> Option<String> {
            self.start_command.clone()
        }
        fn emit(&self, frame: &Frame) {
            self.events.lock().unwrap().push(Frame {
                directory: frame.directory.clone(),
                project_id: frame.project_id.clone(),
                workspace_id: frame.workspace_id.clone(),
                event_type: frame.event_type,
                properties: frame.properties.clone(),
            });
        }
    }

    fn deps(data_dir: &Path) -> Deps {
        Deps {
            data_dir: data_dir.to_path_buf(),
            ..Deps::default()
        }
    }

    fn ctx(worktree: &Path) -> Context {
        Context {
            project_id: "proj_test".to_string(),
            project_worktree: worktree.to_path_buf(),
            worktree: worktree.to_path_buf(),
            workspace_id: None,
            is_git: true,
        }
    }

    #[test]
    fn create_adds_worktree_lists_it_and_boots() {
        let (temp, dir) = repo("worktree-create");
        let deps = deps(temp.path());
        let worktree = Worktree::default();
        let info = worktree
            .create(
                &deps,
                &ctx(&dir),
                Some(&CreateInput {
                    name: Some("My Feature".to_string()),
                    start_command: None,
                }),
            )
            .unwrap();

        assert_eq!(info.name, "my-feature");
        assert_eq!(info.branch.as_deref(), Some("opencode/my-feature"));
        let directory = Path::new(&info.directory);
        assert_eq!(
            directory,
            temp.path()
                .join("worktree")
                .join("proj_test")
                .join("my-feature")
        );
        assert!(directory.join("tracked.txt").exists());

        assert_eq!(
            deps.sandboxes.lock().unwrap().as_slice(),
            [format!("proj_test:{}", directory.display())]
        );
        assert_eq!(
            deps.loaded.lock().unwrap().as_slice(),
            [directory.to_string_lossy().into_owned()]
        );
        let events = deps.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "worktree.ready");
        assert_eq!(events[0].directory, directory);

        let listed = worktree.list(&ctx(&dir)).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "my-feature");
        assert_eq!(listed[0].branch.as_deref(), Some("opencode/my-feature"));
    }

    #[test]
    fn create_with_used_name_generates_a_suffix() {
        let (temp, dir) = repo("worktree-suffix");
        let deps = deps(temp.path());
        let worktree = Worktree::default();
        let first = worktree
            .create(
                &deps,
                &ctx(&dir),
                Some(&CreateInput {
                    name: Some("same".to_string()),
                    start_command: None,
                }),
            )
            .unwrap();
        let second = worktree
            .create(
                &deps,
                &ctx(&dir),
                Some(&CreateInput {
                    name: Some("same".to_string()),
                    start_command: None,
                }),
            )
            .unwrap();
        assert_eq!(first.name, "same");
        assert_ne!(second.name, "same");
        assert!(second.name.starts_with("same-"));
        assert_ne!(first.directory, second.directory);
    }

    #[test]
    fn non_git_project_is_rejected() {
        let temp = TempDir::new("worktree-not-git");
        let plain = Context {
            is_git: false,
            ..ctx(temp.path())
        };
        let deps = deps(temp.path());
        let worktree = Worktree::default();

        let err = worktree.create(&deps, &plain, None).unwrap_err();
        assert_eq!(err.tag, "WorktreeNotGitError");
        assert_eq!(worktree.list(&plain).unwrap(), Vec::<Info>::new());
        let err = worktree
            .remove(&deps, &plain, temp.path().to_string_lossy().as_ref())
            .unwrap_err();
        assert_eq!(err.tag, "WorktreeNotGitError");
        let err = worktree
            .reset(&deps, &plain, temp.path().to_string_lossy().as_ref())
            .unwrap_err();
        assert_eq!(err.tag, "WorktreeNotGitError");
    }

    #[test]
    fn remove_deletes_worktree_and_branch() {
        let (temp, dir) = repo("worktree-remove");
        let deps = deps(temp.path());
        let worktree = Worktree::default();
        let info = worktree
            .create(
                &deps,
                &ctx(&dir),
                Some(&CreateInput {
                    name: Some("gone".to_string()),
                    start_command: None,
                }),
            )
            .unwrap();

        let removed = worktree.remove(&deps, &ctx(&dir), &info.directory).unwrap();
        assert!(removed);
        assert!(worktree.list(&ctx(&dir)).unwrap().is_empty());
        assert!(!Path::new(&info.directory).exists());
        let check = Command::new("git")
            .args(["branch", "--list", "opencode/gone"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&check.stdout).trim(),
            "",
            "worktree branch is deleted"
        );
        assert!(!deps.disposed.lock().unwrap().is_empty());
    }

    #[test]
    fn remove_of_unregistered_directory_cleans_it() {
        let (temp, dir) = repo("worktree-remove-plain");
        let deps = deps(temp.path());
        let worktree = Worktree::default();
        let stray = temp.path().join("stray");
        std::fs::create_dir_all(&stray).unwrap();
        let removed = worktree
            .remove(&deps, &ctx(&dir), stray.to_string_lossy().as_ref())
            .unwrap();
        assert!(removed);
        assert!(!stray.exists());
    }

    #[test]
    fn reset_restores_worktree_and_rejects_primary() {
        let (temp, dir) = repo("worktree-reset");
        let deps = deps(temp.path());
        let worktree = Worktree::default();
        let info = worktree
            .create(
                &deps,
                &ctx(&dir),
                Some(&CreateInput {
                    name: Some("reset-me".to_string()),
                    start_command: None,
                }),
            )
            .unwrap();
        let directory = Path::new(&info.directory);
        std::fs::write(directory.join("tracked.txt"), "dirty\n").unwrap();
        std::fs::write(directory.join("untracked.txt"), "dirt\n").unwrap();

        let reset = worktree.reset(&deps, &ctx(&dir), &info.directory).unwrap();
        assert!(reset);
        assert_eq!(
            std::fs::read_to_string(directory.join("tracked.txt")).unwrap(),
            "one\n"
        );
        assert!(!directory.join("untracked.txt").exists());

        let err = worktree
            .reset(&deps, &ctx(&dir), dir.to_string_lossy().as_ref())
            .unwrap_err();
        assert_eq!(err.tag, "WorktreeResetFailedError");
        assert_eq!(err.message, "Cannot reset the primary workspace");

        let err = worktree
            .reset(
                &deps,
                &ctx(&dir),
                temp.path().join("nowhere").to_string_lossy().as_ref(),
            )
            .unwrap_err();
        assert_eq!(err.message, "Worktree not found");
    }

    #[test]
    fn boot_failure_emits_worktree_failed() {
        let (temp, dir) = repo("worktree-boot-fail");
        let mut deps = deps(temp.path());
        deps.fail_load = true;
        let worktree = Worktree::default();
        let info = worktree
            .make_worktree_info(&deps, &ctx(&dir), Some("failing"), false)
            .unwrap();
        worktree
            .create_from_info(&deps, &ctx(&dir), &info, None)
            .unwrap();
        let events = deps.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "worktree.failed");
        assert_eq!(events[0].properties["message"], "boom");
    }

    #[test]
    fn slugify_matrix() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("  Spaces   Everywhere  "), "spaces-everywhere");
        assert_eq!(slugify("a/b/c"), "a-b-c");
        assert_eq!(slugify("---"), "");
        assert_eq!(slugify("Ümlaut"), "mlaut");
    }
}
