//! The places a write on behalf of a restricted or outside-input session
//! must not touch, and that a server with bearer authentication must not
//! serve as a project (alforria extension).
//!
//! Two things are protected:
//!
//! * **loader names** — `opencode.json`, `.opencode/`, `AGENTS.md`,
//!   `.claude/`, `.git/` and the rest of [`crate::project_files`] — as a
//!   component *anywhere* in the resolved path. The loaders walk up from the
//!   project (to `/` for a directory that is not a repository) and into the
//!   home directory, so a loader file in an ancestor or under `~` is read
//!   just like one in the project;
//! * **locations** — the directories and files the loaders and the server
//!   itself read that carry no telltale name: the global config directory,
//!   the managed directory, the data, state and cache directories, the
//!   binary's own directory, and whatever the instance's config names
//!   (`OPENCODE_CONFIG*`, `skills.paths`, `workflows.paths`, local
//!   `instructions`).
//!
//! The set is assembled from the loaders' own answers
//! ([`crate::config::ConfigLoader::search_dirs`], the config they produced)
//! and the shared name constants, so a new search location is protected
//! where it is searched. One function, [`Protected::guards`], says whether
//! a write may go ahead; every mutating file operation of the edit tools
//! goes through it with the destination it is about to modify
//! ([`crate::tool::def::InstanceContext::write_file`], `remove_file`, and
//! `about_to_modify` for a check ahead of a batch) — enforcement does not
//! depend on what a tool put in a permission ask.
//!
//! This is defence in depth, not the control: a session that may run `bash`
//! writes any file the server's user can. What keeps a project from
//! carrying policy on an unattended server is the host's permission ceiling
//! and restricted project config (`docs/unattended-server.md`).

//!
//! # The invariant, and the three ways it fails
//!
//! *A guarded session's tools never modify a file that a loader or the
//! server reads, unless the host's own policy grants that very path — and
//! never a host location at all.* It has failed in three ways, each of
//! which has a property test below:
//!
//! 1. **the wrong path is judged** — the guard's idea of where a write
//!    lands differs from the kernel's. There is one resolver,
//!    [`resolve_path`], and it resolves as the kernel does: component by
//!    component, links read where they stand, `..` applied to the resolved
//!    path and never to the spelling, an error instead of a guess. It is
//!    tested against the operating system. On top of it a guarded session
//!    is refused a final component that is a symlink and a path through a
//!    dangling one ([`landing`]). And the write is made *on the landing
//!    that was judged*, not on the name again: [`WriteGuard::admit`]
//!    returns it, and [`write_at`] / [`remove_at`] / [`open_parent_of`]
//!    open its parent from the root one component at a time with
//!    `O_NOFOLLOW` (creating what is missing) and create, replace or
//!    unlink the last component through that handle, not followed either
//!    (`beneath::NoFollowDir`). There is no second resolution of the name
//!    for a rename or a link swapped in between to redirect: such a write
//!    fails. `write`, `edit`, `apply_patch` (through
//!    `InstanceContext::write_file` / `remove_file`), the upload route and
//!    a guarded session's revert write this way.
//! 2. **the protected set is incomplete** — a place a loader reads is not
//!    in it. The set is built from the loaders' own answers, a loader name
//!    is refused anywhere, and for the config's `instructions` — globs,
//!    matched in every directory up to the worktree — the guard holds no
//!    matching logic at all: it asks the instruction loader's own search
//!    (`session::instruction::EntrySearch`) whether it would read the path.
//! 3. **the grant is too broad** — `edit_loader` lifts more than it names.
//!    The guard is always on; a grant is looked up per write, for the
//!    resolved path, in the host's reading of the policy only, and lifts
//!    project-level loader files alone. Host locations — the data, state,
//!    cache, global and managed config directories, the binary's
//!    directory — are lifted by nothing.
//!
//! # What the guard does not cover
//!
//! * **Other writers.** `bash`, a formatter, an LSP or MCP server, a
//!   workflow script: anything that is not one of the guarded operations
//!   above writes what the server's user can.
//! * **What a guarded revert leaves alone.** `session::revert` judges every
//!   file a revert or unrevert means to write ([`Protected::judge`]),
//!   refuses the whole revert when one is protected, and then writes
//!   exactly the judged files at their judged landings through the handle
//!   ([`restore_at`], [`remove_at`]) — the check and the write are one
//!   list, and the check-to-write window is closed as for a tool. The price
//!   is that such a revert is narrower than upstream's whole-tree restore:
//!   a file the snapshot service does not list (git-ignored ones) and a
//!   path the snapshot holds as a symlink or a submodule are not restored.
//! * **A directory renamed afterwards.** The guard judges where a write
//!   lands when it is made. A process that can rename inside the project —
//!   which here means the session's own shell — can let the write land in
//!   `plain/` and then rename `plain` to `.opencode`; no race is needed and
//!   no check at the write can see it. Handle-based writing closes the
//!   check-to-write window; it does not make names permanent.
//! * **Platforms without `openat` (Windows).** There is no directory
//!   handle to write through, so the write is made by name — with the
//!   judged landing resolved again immediately before each step (creating
//!   the directories, creating the file, the rename, the removal) and the
//!   step refused when it no longer resolves to itself
//!   (`beneath::NoFollowDir`'s non-Unix half, the one place this is
//!   decided for every guarded write and the upload route). A link swapped
//!   in between the judgement and that comparison is refused; one swapped
//!   in between the comparison and the system call that follows it is not
//!   seen. The window is an instant, not closed.
//! * **Unguarded sessions.** The operator's own sessions have no guard and
//!   write by name exactly as upstream does.
//!
//! # The resolver is shared
//!
//! [`resolve_path`] is also what every other "is this path inside that
//! place" decision asks: a restricted caller's attachments
//! (`session::caller`), the server's project boundary, upload area, file
//! tickets and directory allowlist, a stored trigger's watch path
//! ([`contains`], [`within`]). A function that folds `..` out of a spelling
//! before reading links (`git::lexical_normalize`, upstream's
//! `FSUtil.resolve`, `tool::ripgrep::ts_resolve`) names things the way
//! upstream does and may decide nothing about confinement on its own; where
//! one survives next to a check, a comment at the site says why.
use std::path::{Path, PathBuf};

use crate::project_files::is_loader_path;

/// Where a path leads, resolved the way the kernel resolves it — **the one
/// resolver** of this crate's guard, confinement and loader-match code.
///
/// The path is walked component by component from the root. A component
/// that exists and is a symlink is read and the walk continues from its
/// target (at most [`MAX_LINKS`] of them, the kernel's `ELOOP`). A `..` is
/// applied to the *resolved* path so far, never to the spelling:
/// `link/../x` with `link -> /elsewhere/dir` is `/elsewhere/x`, not `x`.
/// Once a component does not exist nothing further can be a link: the rest
/// is appended as spelled — and a `..` there is an error, since what it
/// would be relative to cannot be known. Any I/O error other than "not
/// found" is an error too. Callers that decide something treat an error as
/// the protected answer.
pub fn resolve_path(path: &Path) -> std::io::Result<PathBuf> {
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::path::Component;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let invalid = |message: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, message);
    let mut out = PathBuf::new();
    let mut pending: VecDeque<OsString> = VecDeque::new();
    for part in absolute.components() {
        match part {
            Component::Prefix(_) | Component::RootDir => out.push(part.as_os_str()),
            other => pending.push_back(other.as_os_str().to_os_string()),
        }
    }
    let root = out.clone();
    let (mut links, mut missing) = (0usize, false);
    while let Some(part) = pending.pop_front() {
        if part == "." {
            continue;
        }
        if part == ".." {
            if missing {
                return Err(invalid(
                    "a `..` after a component that does not exist cannot be resolved",
                ));
            }
            // At the root `..` stays at the root, as for the kernel.
            if out != root {
                out.pop();
            }
            continue;
        }
        let next = out.join(&part);
        if missing {
            out = next;
            continue;
        }
        match std::fs::symlink_metadata(&next) {
            Ok(meta) if meta.file_type().is_symlink() => {
                links += 1;
                if links > MAX_LINKS {
                    return Err(invalid("too many levels of symbolic links"));
                }
                let target = std::fs::read_link(&next)?;
                // An absolute target starts over from the root; a relative
                // one continues from the directory holding the link.
                let mut front: Vec<OsString> = Vec::new();
                for piece in target.components() {
                    match piece {
                        Component::Prefix(_) | Component::RootDir => {
                            out = PathBuf::from(piece.as_os_str());
                            if !target.has_root() {
                                continue;
                            }
                            out = root.clone();
                        }
                        other => front.push(other.as_os_str().to_os_string()),
                    }
                }
                for piece in front.into_iter().rev() {
                    pending.push_front(piece);
                }
            }
            Ok(meta) => {
                if !meta.is_dir() && !pending.is_empty() {
                    return Err(invalid("a component of the path is not a directory"));
                }
                out = next;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing = true;
                out = next;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(out)
}

/// The kernel's limit on symlinks followed in one resolution.
pub const MAX_LINKS: usize = 40;

/// [`resolve_path`] for building a protected set, where an answer is
/// needed for every location: one that cannot be resolved is kept as it is
/// spelled, which still protects that spelling. (A decision about a write
/// never comes through here: [`landing`] refuses on an error.)
pub fn resolve(path: &Path) -> PathBuf {
    resolve_path(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        }
    })
}

/// Component-wise comparison key: case-folded where the filesystem is
/// case-insensitive.
fn key(path: &Path) -> PathBuf {
    if cfg!(any(windows, target_os = "macos")) {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path.to_path_buf()
    }
}

/// Whether `inner` is `outer` or below it (component-wise). Both are
/// expected resolved ([`resolve_path`]): this compares, it does not look.
pub fn within(inner: &Path, outer: &Path) -> bool {
    key(inner).starts_with(key(outer))
}

/// Whether `child`, as the kernel would resolve that spelling, is `parent`
/// or below it. `parent` is the trusted side (a project directory) and is
/// resolved with [`resolve`]; a `child` that cannot be resolved is not
/// inside anything.
pub fn contains(parent: &Path, child: &Path) -> bool {
    resolve_path(child).is_ok_and(|child| within(&child, &resolve(parent)))
}

/// Whether a grant can lift the protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A project-level loader file or location: an explicit `edit_loader`
    /// grant for the path, in the host's policy, lifts it.
    Loader,
    /// A host location, or a target that cannot be judged: nothing lifts it.
    Host,
}

/// Why a write is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub class: Class,
    pub reason: String,
    /// Where the write would land ([`landing`]), when that could be told.
    pub landing: Option<PathBuf>,
}

/// Where a write to `target` lands, or why it cannot be said:
/// [`resolve_path`] — the kernel's own reading, `..` and links included —
/// with two shapes refused rather than followed: a final component that is
/// a symlink, and a path that goes through a dangling one (a guarded
/// session has no business writing through either, and the write, made on
/// the landing with no link followed, would not create the latter's
/// target). An error from the resolver is a refusal.
pub fn landing(target: &Path) -> Result<PathBuf, String> {
    let landing = resolve_path(target)
        .map_err(|error| format!("{} cannot be resolved ({error})", target.display()))?;
    let absolute = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(target)
    };
    // An error here is "nothing by that name yet", which is not a link.
    if std::fs::symlink_metadata(&absolute).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(format!("{} is a symbolic link", target.display()));
    }
    // A link to nowhere on the way: the spelled path has a component that
    // exists (as a link) below which nothing resolves. The resolver has
    // followed it faithfully; a guarded session does not get to use it.
    let mut prefix = PathBuf::new();
    for part in absolute.components() {
        prefix.push(part.as_os_str());
        let is_link =
            std::fs::symlink_metadata(&prefix).is_ok_and(|meta| meta.file_type().is_symlink());
        if is_link && std::fs::metadata(&prefix).is_err() {
            return Err(format!(
                "{} goes through a dangling symbolic link",
                target.display()
            ));
        }
    }
    Ok(landing)
}

/// See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Protected {
    /// Host locations — `(what it is, resolved path)`: never liftable.
    host: Vec<(String, PathBuf)>,
    /// Loader locations of the instance: liftable by a grant.
    loader: Vec<(String, PathBuf)>,
    /// The config's `instructions` entries, as the instruction loader
    /// itself searches for them (`session::instruction::EntrySearch`). The
    /// guard holds no glob or directory-walk logic of its own: it asks the
    /// loader's description whether it would read the path.
    instructions: Vec<crate::session::instruction::EntrySearch>,
}

impl Protected {
    fn add(list: &mut Vec<(String, PathBuf)>, what: &str, path: &Path) {
        if path.as_os_str().is_empty() {
            return;
        }
        let resolved = resolve(path);
        if !list.iter().any(|(_, known)| *known == resolved) {
            list.push((what.to_string(), resolved));
        }
    }

    /// The host's locations: what the server reads for every project.
    /// `user_config` is false when `server.trust.host` is `"managed"` — the
    /// user's config files then carry no host key (they are still loader
    /// locations for an instance, [`Protected::for_instance`]).
    pub fn host(paths: &crate::GlobalPaths, managed_dir: &Path, user_config: bool) -> Protected {
        let mut out = Protected::default();
        let host = &mut out.host;
        Self::add(host, "the data directory", &paths.data);
        Self::add(host, "the state directory", &paths.state);
        Self::add(host, "the cache directory", &paths.cache);
        Self::add(host, "the managed config directory", managed_dir);
        if user_config {
            Self::add(host, "the global config directory", &paths.config);
            Self::add(
                host,
                "~/.opencode",
                &paths.home.join(crate::project_files::CONFIG_DIR),
            );
        }
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
        {
            Self::add(host, "the directory of the alforria binary", &dir);
        }
        out
    }

    /// Everything the loaders consult for one instance: the host's
    /// locations, every config directory the config loader searches, and
    /// the paths and globs the loaded config names. No entry is dropped.
    pub fn for_instance(
        paths: &crate::GlobalPaths,
        managed_dir: &Path,
        search_dirs: &[PathBuf],
        flags: &crate::config::ConfigFlags,
        config: &crate::config::schema::Config,
        directory: &Path,
        worktree: &Path,
    ) -> Protected {
        let mut out = Protected::host(paths, managed_dir, true);
        for dir in [
            crate::project_files::CLAUDE_DIR,
            crate::project_files::AGENTS_DIR,
        ] {
            Self::add(
                &mut out.loader,
                "a home loader directory",
                &paths.home.join(dir),
            );
        }
        for dir in search_dirs {
            // The host's own are already there, and stay unliftable.
            if out.host.iter().all(|(_, known)| *known != resolve(dir)) {
                Self::add(&mut out.loader, "a config directory", dir);
            }
        }
        for (what, value) in [
            ("OPENCODE_CONFIG", &flags.config),
            ("OPENCODE_CONFIG_DIR", &flags.config_dir),
        ] {
            if let Some(path) = value.as_deref().filter(|path| !path.is_empty()) {
                Self::add(&mut out.host, what, Path::new(path));
            }
        }
        // Directories the config names: protected whole.
        let named = [
            (
                "skills.paths",
                config.skills.as_ref().and_then(|s| s.paths.clone()),
            ),
            (
                "workflows.paths",
                config.workflows.as_ref().and_then(|w| w.paths.clone()),
            ),
        ];
        for (what, entries) in named {
            for entry in entries.into_iter().flatten() {
                if entry.starts_with("http://") || entry.starts_with("https://") {
                    continue;
                }
                let path = match entry.strip_prefix("~/") {
                    Some(rest) => paths.home.join(rest),
                    None if entry == "~" => paths.home.clone(),
                    None => directory.join(&entry),
                };
                Self::add(&mut out.loader, what, &path);
            }
        }
        // Instruction entries: the loader's own search, not a copy of it.
        let instruction_paths = crate::session::instruction::InstructionPaths {
            config: paths.config.clone(),
            home: paths.home.clone(),
            directory: directory.to_path_buf(),
            worktree: worktree.to_path_buf(),
        };
        let instruction_flags = crate::session::instruction::InstructionFlags {
            disable_claude_code_prompt: false,
            disable_project_config: flags.disable_project_config,
        };
        out.instructions = config
            .instructions
            .iter()
            .flatten()
            .map(|raw| {
                crate::session::instruction::EntrySearch::new(
                    raw,
                    &instruction_paths,
                    instruction_flags,
                )
            })
            .collect();
        out
    }

    /// [`Protected::for_instance`] for an instance whose config was loaded
    /// with `params`: the one constructor every entry point that boots an
    /// instance uses — the server's instance factory and the CLI's one-shot
    /// instance (`alforria run`) alike — so the set is the same wherever a
    /// guarded session runs. The managed directory, the search directories
    /// and the flags are the config loader's own answers for those
    /// parameters.
    pub fn for_loaded(
        params: &crate::LoadParams,
        config: &crate::config::schema::Config,
        worktree: &Path,
    ) -> Protected {
        let loader = crate::ConfigLoader::new();
        Protected::for_instance(
            &params.paths,
            &loader.managed_dir(params),
            &loader.search_dirs(params),
            &params.flags,
            config,
            &params.directory,
            worktree,
        )
    }

    /// Every protected location, host and loader.
    pub fn locations(&self) -> Vec<(String, PathBuf)> {
        self.host.iter().chain(&self.loader).cloned().collect()
    }

    /// Why a write to `target` is refused, if it is.
    pub fn guards(&self, target: &Path) -> Option<Refusal> {
        self.judge(target).err()
    }

    /// Where a write to `target` lands ([`landing`]) when it may go ahead;
    /// why not otherwise. The landing returned is the one that was judged:
    /// a caller that writes there through [`write_at`] writes what was
    /// judged and nothing else.
    pub fn judge(&self, target: &Path) -> Result<PathBuf, Refusal> {
        let landing = match landing(target) {
            Ok(landing) => landing,
            Err(reason) => {
                return Err(Refusal {
                    class: Class::Host,
                    reason,
                    landing: None,
                })
            }
        };
        let refusal = |class, reason: String| {
            Err(Refusal {
                class,
                reason,
                landing: Some(landing.clone()),
            })
        };
        let inside = |list: &[(String, PathBuf)]| {
            list.iter()
                .find(|(_, location)| within(&landing, location))
                .map(|(what, location)| {
                    format!("{} is in {what} ({})", target.display(), location.display())
                })
        };
        if let Some(reason) = inside(&self.host) {
            return refusal(Class::Host, reason);
        }
        let names: PathBuf = landing
            .components()
            .filter(|part| matches!(part, std::path::Component::Normal(_)))
            .collect();
        if is_loader_path(&names) {
            return refusal(
                Class::Loader,
                format!(
                    "{} is a file the agent's configuration is loaded from (config, \
                     instructions, agents, commands, skills, workflows, git hooks)",
                    target.display()
                ),
            );
        }
        if let Some(reason) = inside(&self.loader) {
            return refusal(Class::Loader, reason);
        }
        // Asked of the loader's own description of each entry, for the
        // landing and for the path as the tool spelled it.
        if self
            .instructions
            .iter()
            .any(|entry| entry.would_read(&landing) || entry.would_read(target))
        {
            return refusal(
                Class::Loader,
                format!(
                    "{} is a file the config's `instructions` load",
                    target.display()
                ),
            );
        }
        Ok(landing)
    }

    /// The protected location `directory` overlaps — one contains the
    /// other, or they are the same — if any: `(what, location)`.
    pub fn overlap(&self, directory: &Path) -> Option<(String, PathBuf)> {
        let directory = resolve(directory);
        self.host
            .iter()
            .chain(&self.loader)
            .find(|(_, location)| within(location, &directory) || within(&directory, location))
            .cloned()
    }
}

/// The write guard of a guarded session: the protected set, and the policy
/// a grant is looked up in.
#[derive(Debug, Clone)]
pub struct WriteGuard {
    pub protected: std::sync::Arc<Protected>,
    /// The session's policy. A grant counts only when *every* reading has
    /// it ([`Policy::explicitly_allows`]) — so one a project wrote, which
    /// the host's reading lacks, lifts nothing.
    ///
    /// [`Policy::explicitly_allows`]: crate::tool::permission::Policy::explicitly_allows
    pub policy: crate::tool::permission::Policy,
    /// The project a relative grant pattern is relative to.
    pub worktree: PathBuf,
}

impl WriteGuard {
    /// Whether the write to `target` may go ahead; `Err` is why not.
    pub fn check(&self, target: &Path) -> Result<(), String> {
        self.admit(target).map(|_| ())
    }

    /// [`WriteGuard::check`], answering with where the write lands: the
    /// resolved path that was judged (or that a grant was found for). The
    /// write is then made there through the directory handle
    /// ([`write_at`], [`remove_at`]), so what is written is what was
    /// judged.
    pub fn admit(&self, target: &Path) -> Result<PathBuf, String> {
        let refusal = match self.protected.judge(target) {
            Ok(landing) => return Ok(landing),
            Err(refusal) => refusal,
        };
        let grant = crate::project_files::EDIT_LOADER_PERMISSION;
        let lifted = refusal.class == Class::Loader
            && refusal.landing.as_ref().is_some_and(|landing| {
                let absolute = landing.to_string_lossy();
                // (Not under the worktree: there is no relative spelling to
                // look a grant up by — the stricter answer.)
                let relative = landing
                    .strip_prefix(resolve(&self.worktree))
                    .map(|path| path.to_string_lossy().into_owned());
                self.policy.explicitly_allows(grant, &absolute)
                    || relative.is_ok_and(|path| self.policy.explicitly_allows(grant, &path))
            });
        if let (true, Some(landing)) = (lifted, &refusal.landing) {
            return Ok(landing.clone());
        }
        Err(match refusal.class {
            Class::Loader => format!(
                "{}. This session is restricted or has read outside content, so it may not \
                 write there; the operator can allow that path with the `{grant}` permission \
                 in the host config.",
                refusal.reason
            ),
            Class::Host => format!(
                "{}. This session is restricted or has read outside content, so it may not \
                 write there.",
                refusal.reason
            ),
        })
    }
}

/// Write `contents` to `landing` — a path a guard judged
/// ([`WriteGuard::admit`], [`Protected::judge`]): absolute, resolved, no
/// link in it. Its directories are created and the file is created or
/// truncated through the parent's handle, no component followed
/// (`beneath::NoFollowDir`). Anything swapped in after the judgement — a
/// link for a directory, a link for the file — makes this fail instead of
/// landing the write where the link points.
pub fn write_at(landing: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(any(test, feature = "fail-points"))]
    fail_point::fire(landing);
    crate::beneath::write_no_follow(landing, contents)
}

/// [`write_at`] for a file restored from a snapshot: its executable bit is
/// set or cleared too, on the handle that was written.
pub fn restore_at(landing: &Path, contents: &[u8], executable: bool) -> std::io::Result<()> {
    #[cfg(any(test, feature = "fail-points"))]
    fail_point::fire(landing);
    crate::beneath::write_no_follow_mode(landing, contents, Some(executable))
}

/// Remove the file at a judged `landing`, through the parent's handle; see
/// [`write_at`].
pub fn remove_at(landing: &Path) -> std::io::Result<()> {
    #[cfg(any(test, feature = "fail-points"))]
    fail_point::fire(landing);
    crate::beneath::remove_no_follow(landing)
}

/// The parent directory of a judged `landing`, opened (and created) with no
/// component followed, for a caller that writes in several steps — the
/// upload route's temporary file and rename. See [`write_at`].
pub fn open_parent_of(landing: &Path) -> std::io::Result<crate::beneath::NoFollowDir> {
    #[cfg(any(test, feature = "fail-points"))]
    fail_point::fire(landing);
    let parent = landing.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "the path has no parent")
    })?;
    crate::beneath::NoFollowDir::open(parent, true)
}

/// Test instrumentation: run something between a guard's judgement of a
/// path and the write that follows it, which is where a concurrent rename
/// or link swap would have to happen. Compiled for tests only.
#[cfg(any(test, feature = "fail-points"))]
pub mod fail_point {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    type Hook = Arc<dyn Fn(&Path) + Send + Sync>;
    static HOOKS: Mutex<Vec<(u64, PathBuf, Hook)>> = Mutex::new(Vec::new());
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Disarms its hook when dropped.
    pub struct Armed(u64);

    impl Drop for Armed {
        fn drop(&mut self) {
            HOOKS
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|(id, _, _)| *id != self.0);
        }
    }

    /// Run `hook` with the judged landing before every guarded write that
    /// lands under `under` (a test's own directory, so tests sharing a
    /// process do not see each other's).
    pub fn between_check_and_write(
        under: &Path,
        hook: impl Fn(&Path) + Send + Sync + 'static,
    ) -> Armed {
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        HOOKS.lock().unwrap_or_else(|p| p.into_inner()).push((
            id,
            under.to_path_buf(),
            Arc::new(hook),
        ));
        Armed(id)
    }

    pub(super) fn fire(landing: &Path) {
        let hooks: Vec<Hook> = HOOKS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, under, _)| landing.starts_with(under))
            .map(|(_, _, hook)| hook.clone())
            .collect();
        for hook in hooks {
            hook(landing);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::permission::Policy;
    use alforria_schema::permission_v1::{PermissionV1Action, PermissionV1Rule};

    fn paths(home: &Path) -> crate::GlobalPaths {
        crate::GlobalPaths::resolve_with(home.to_path_buf(), |_| None)
    }

    struct Tree {
        _dir: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
        project: PathBuf,
        protected: Protected,
    }

    fn tree(config: serde_json::Value) -> Tree {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("home");
        let project = root.join("outer/project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::create_dir_all(project.join("docs/rules")).unwrap();
        std::fs::create_dir_all(home.join("skills-elsewhere")).unwrap();
        let p = paths(&home);
        std::fs::create_dir_all(&p.data).unwrap();
        std::fs::create_dir_all(&p.config).unwrap();
        let config: crate::config::schema::Config = serde_json::from_value(config).unwrap();
        let protected = Protected::for_instance(
            &p,
            &root.join("managed"),
            &[p.config.clone(), project.join(".opencode")],
            &crate::config::ConfigFlags::default(),
            &config,
            &project,
            &root.join("outer"),
        );
        Tree {
            _dir: dir,
            root,
            home,
            project,
            protected,
        }
    }

    fn default_config() -> serde_json::Value {
        serde_json::json!({
            "skills": { "paths": ["~/skills-elsewhere", "vendor/skills"] },
            "instructions": [
                "docs/rules/*.md", "*.rules", "RULES*.txt", "~/notes/*.md",
                "https://x.example/rules.md", "/abs/rules.md", "plain/file.md",
            ],
        })
    }

    /// Failure 2, the protected set: every place a loader reads is in it,
    /// by name anywhere or by location, and no configured entry is dropped.
    #[test]
    fn every_loader_location_is_protected() {
        let t = tree(default_config());
        let p = paths(&t.home);
        let (project, root, home) = (&t.project, &t.root, &t.home);
        let refused = |target: PathBuf| t.protected.guards(&target).map(|refusal| refusal.class);
        for target in [
            // The project's own loader files…
            project.join("opencode.json"),
            project.join(".opencode/agent/x.md"),
            project.join("src/AGENTS.md"),
            // …an ancestor's, which the walk up reads…
            root.join("outer/AGENTS.md"),
            root.join("CLAUDE.md"),
            root.join("outer/.opencode/command/c.md"),
            // …the home's…
            home.join(".claude/skills/s/SKILL.md"),
            home.join(".agents/skills/s/SKILL.md"),
            // …and every configured entry: paths, globs with and without a
            // directory part, `~`-relative ones, a bare `~`.
            home.join("skills-elsewhere/s/SKILL.md"),
            project.join("vendor/skills/s/SKILL.md"),
            project.join("docs/rules/security.md"),
            project.join("team.rules"),
            project.join("RULES-2027.txt"),
            home.join("notes/todo.md"),
            PathBuf::from("/abs/rules.md"),
            project.join("plain/file.md"),
        ] {
            assert_eq!(
                refused(target.clone()),
                Some(Class::Loader),
                "{}",
                target.display()
            );
        }
        for target in [
            p.config.join("agent/mine.md"),
            p.config.join("opencode.json"),
            p.data.join("auth.json"),
            p.state.join("anything"),
            p.cache.join("x/y"),
            root.join("managed/opencode.json"),
            home.join(".opencode/agent/x.md"),
        ] {
            assert_eq!(
                refused(target.clone()),
                Some(Class::Host),
                "{}",
                target.display()
            );
        }
        for target in [
            project.join("src/main.rs"),
            project.join("docs/readme.md"),
            root.join("outer/notes.txt"),
        ] {
            assert_eq!(refused(target.clone()), None, "{}", target.display());
        }
    }

    /// The guard and the loader are one question: for a tree of files and
    /// entries using `**`, braces, character classes, patterns that match in
    /// an ancestor up to the worktree, and `~/` ones, every file the real
    /// instruction loader reads is refused by the guard (guard ⊇ loader) —
    /// and files it does not read, in the same directories, are not swept
    /// up with them.
    #[test]
    fn every_file_the_instruction_loader_reads_is_refused() {
        use crate::session::instruction::{
            Instruction, InstructionFlags, InstructionPaths, NoRemoteInstructions,
        };
        let entries = [
            "docs/**/*.md",
            "rules/{security,style}.txt",
            "notes/[a-c]*.rst",
            "*.guide",
            "conf/*/policy.md",
            "~/global-rules/**/*.md",
            "~/single.md",
            "~/fresh/*.md",
            "plain/file.md",
        ];
        let t = tree(serde_json::json!({ "instructions": entries }));
        let (project, outer, home) = (&t.project, t.root.join("outer"), &t.home);
        let files = [
            // In the instance directory…
            project.join("docs/a.md"),
            project.join("docs/deep/er/b.md"),
            project.join("rules/security.txt"),
            project.join("rules/style.txt"),
            project.join("rules/other.txt"),
            project.join("notes/apple.rst"),
            project.join("notes/zebra.rst"),
            project.join("top.guide"),
            project.join("conf/x/policy.md"),
            project.join("plain/file.md"),
            project.join("src/main.rs"),
            project.join("docs/a.txt"),
            // …in its parent, up to the worktree (the walk up)…
            outer.join("docs/parent.md"),
            outer.join("rules/style.txt"),
            outer.join("parent.guide"),
            outer.join("notes/beta.rst"),
            outer.join("readme.txt"),
            // …above the worktree (not searched)…
            t.root.join("docs/above.md"),
            // …and in the home.
            home.join("global-rules/a/b/c.md"),
            home.join("single.md"),
            home.join("other.md"),
        ];
        for file in &files {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "x").unwrap();
        }
        let loader = Instruction::new(
            InstructionFlags::default(),
            InstructionPaths {
                config: paths(home).config,
                home: home.clone(),
                directory: project.clone(),
                worktree: outer.clone(),
            },
            entries.iter().map(|entry| entry.to_string()).collect(),
            std::sync::Arc::new(NoRemoteInstructions),
        );
        let read = loader.system_paths();
        assert!(
            read.len() >= 13,
            "the loader found {}: {read:?}",
            read.len()
        );
        for file in &read {
            assert!(
                t.protected.guards(file).is_some(),
                "the loader reads {} and the guard lets it be written",
                file.display()
            );
        }
        // Not read, not refused.
        for file in &files {
            if !read.contains(file) {
                assert_eq!(t.protected.guards(file), None, "{}", file.display());
            }
        }
        // A search directory that does not exist when the guard is asked:
        // the write that would create it and a matching file is refused.
        assert!(!project.join("conf/brand-new").exists());
        assert!(t
            .protected
            .guards(&project.join("conf/brand-new/policy.md"))
            .is_some());
        // (An absolute or `~/` entry is matched by its last component in
        // its own directory only — the loader's reading, `**` in the
        // directory part included — so that is all the guard claims.)
        assert!(!home.join("fresh").exists());
        assert!(t.protected.guards(&home.join("fresh/x.md")).is_some());
        assert_eq!(t.protected.guards(&home.join("fresh/x.txt")), None);
        // A search directory behind a symlink: the landing is the real
        // path, and it is refused there too.
        #[cfg(unix)]
        {
            std::fs::create_dir_all(t.root.join("real-rules")).unwrap();
            std::fs::remove_dir_all(project.join("rules")).unwrap();
            std::os::unix::fs::symlink(t.root.join("real-rules"), project.join("rules")).unwrap();
            assert!(t
                .protected
                .guards(&project.join("rules/security.txt"))
                .is_some());
            assert!(t
                .protected
                .guards(&t.root.join("real-rules/style.txt"))
                .is_some());
            assert_eq!(
                t.protected.guards(&t.root.join("real-rules/other.txt")),
                None
            );
        }
        // Files that do not exist yet but would be read once written.
        assert!(t
            .protected
            .guards(&project.join("docs/new/later.md"))
            .is_some());
        assert!(t
            .protected
            .guards(&outer.join("rules/security.txt"))
            .is_some());
    }

    /// Failure 1, the wrong path judged. Over generated spellings of paths
    /// in a tree with symlinks — to protected and to ordinary places,
    /// dangling ones, `..` in every position — whenever the guard lets a
    /// write through, the write is made for real and the file it produced
    /// is looked up: it is never in a protected place.
    #[cfg(unix)]
    #[test]
    fn a_write_the_guard_allows_never_lands_somewhere_protected() {
        use std::os::unix::fs::symlink;
        let t = tree(default_config());
        let p = paths(&t.home);
        let project = &t.project;
        std::fs::create_dir_all(project.join(".opencode/agent")).unwrap();
        std::fs::write(project.join("AGENTS.md"), "operator's").unwrap();
        std::fs::create_dir_all(t.root.join("elsewhere/dir")).unwrap();
        symlink(&p.data, project.join("to-data")).unwrap();
        symlink(project.join(".opencode"), project.join("to-cfg")).unwrap();
        symlink(project.join("AGENTS.md"), project.join("to-agents")).unwrap();
        symlink(t.root.join("elsewhere/dir"), project.join("to-elsewhere")).unwrap();
        symlink(p.config.join("not-yet"), project.join("dangling")).unwrap();
        symlink(project.join("src"), project.join("to-src")).unwrap();
        // `to-elsewhere/..` is `elsewhere`, where a loader name waits.
        std::fs::create_dir_all(t.root.join("elsewhere/.opencode")).unwrap();

        let parts = [
            "src",
            "docs",
            "to-data",
            "to-cfg",
            "to-agents",
            "to-elsewhere",
            "dangling",
            "to-src",
            "..",
            ".",
            ".opencode",
            "agent",
            "x.md",
            "new.txt",
            "AGENTS.md",
            "deep",
            "auth.json",
        ];
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut next = move |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        let (mut allowed, mut refused) = (0, 0);
        for _ in 0..3000 {
            let depth = 1 + next(4);
            let mut target = project.clone();
            for _ in 0..depth {
                target.push(parts[next(parts.len())]);
            }
            if t.protected.guards(&target).is_some() {
                refused += 1;
                continue;
            }
            // Allowed: write as the tools do, and see where it went.
            if let Some(parent) = target.parent() {
                if std::fs::create_dir_all(parent).is_err() {
                    continue;
                }
            }
            if std::fs::write(&target, "probe").is_err() {
                continue;
            }
            allowed += 1;
            let landed = std::fs::canonicalize(&target).unwrap();
            let names: PathBuf = landed
                .components()
                .filter(|part| matches!(part, std::path::Component::Normal(_)))
                .collect();
            assert!(
                !is_loader_path(&names),
                "{} was allowed and landed on {}",
                target.display(),
                landed.display()
            );
            for (what, location) in t.protected.locations() {
                assert!(
                    !landed.starts_with(&location),
                    "{} was allowed and landed in {what}: {}",
                    target.display(),
                    landed.display()
                );
            }
            // Where the guard said it would land is where it landed.
            assert_eq!(
                landing(&target).ok(),
                Some(landed.clone()),
                "{}",
                target.display()
            );
            let _ = std::fs::remove_file(&landed);
        }
        assert!(
            allowed > 50 && refused > 500,
            "allowed {allowed}, refused {refused}"
        );
        // The shapes by name.
        for (target, why) in [
            ("to-agents", "symbolic link"),
            ("dangling", "symbolic link"),
            ("dangling/x.md", "dangling"),
        ] {
            let refusal = t.protected.guards(&project.join(target)).expect(target);
            assert!(refusal.reason.contains(why), "{target}: {}", refusal.reason);
            assert_eq!(refusal.class, Class::Host, "{target}: not liftable");
        }
        // `..` is applied to where the path has got to, not to its
        // spelling: `to-elsewhere/..` is `elsewhere`, whose `.opencode` is
        // a loader directory; and a `..` that stays inside is just a path.
        let through = t
            .protected
            .guards(&project.join("to-elsewhere/../.opencode/x.md"))
            .expect("refused");
        assert_eq!(
            through.landing,
            Some(t.root.join("elsewhere/.opencode/x.md"))
        );
        assert!(t
            .protected
            .guards(&project.join("src/../AGENTS.md"))
            .is_some());
        assert_eq!(
            t.protected.guards(&project.join("src/../docs/new.txt")),
            None
        );
        // A `..` after something that does not exist cannot be resolved.
        assert!(t
            .protected
            .guards(&project.join("no-such-dir/../x.txt"))
            .is_some_and(|refusal| refusal.class == Class::Host));
        // A link to an ordinary directory is followed, like the kernel does.
        assert_eq!(t.protected.guards(&project.join("to-src/new.rs")), None);
        assert!(t
            .protected
            .guards(&project.join("to-cfg/agent/x.md"))
            .is_some());
        assert!(t
            .protected
            .guards(&project.join("to-data/auth.json"))
            .is_some());
    }

    /// The resolver against the operating system. Over a tree with
    /// symlinks — to directories, to files, chained, relative with `..`,
    /// absolute, dangling, looping — and generated spellings with `.`,
    /// `..`, repeated slashes and links in every position: when the target
    /// exists, `resolve_path` is `fs::canonicalize`; when its tail does
    /// not, the tail is created through the spelling and the file is found
    /// exactly where `resolve_path` said it would land.
    #[cfg(unix)]
    #[test]
    fn the_resolver_agrees_with_the_kernel() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for sub in ["a/b/c", "a/d", "e/f", "g"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        std::fs::write(root.join("a/b/file.txt"), "x").unwrap();
        std::fs::write(root.join("e/other.txt"), "x").unwrap();
        symlink(root.join("e/f"), root.join("a/abs-link")).unwrap();
        symlink("../e", root.join("a/rel-link")).unwrap();
        symlink("b/c", root.join("a/down-link")).unwrap();
        symlink("rel-link", root.join("a/chain")).unwrap();
        symlink("b/file.txt", root.join("a/file-link")).unwrap();
        symlink(root.join("nowhere/at/all"), root.join("a/dangling")).unwrap();
        symlink("missing-sibling", root.join("e/dangling-rel")).unwrap();
        symlink("loop-b", root.join("g/loop-a")).unwrap();
        symlink("loop-a", root.join("g/loop-b")).unwrap();
        symlink("..", root.join("a/b/up")).unwrap();

        let parts = [
            "a",
            "b",
            "c",
            "d",
            "e",
            "f",
            "g",
            "abs-link",
            "rel-link",
            "down-link",
            "chain",
            "file-link",
            "dangling",
            "dangling-rel",
            "loop-a",
            "up",
            "file.txt",
            "other.txt",
            "..",
            "..",
            ".",
            "",
            "new",
            "fresh.txt",
        ];
        let mut seed = 0x2545f4914f6cdd1du64;
        let mut next = move |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        let (mut existing, mut created, mut refused) = (0, 0, 0);
        for _ in 0..6000 {
            let mut spelling = root.to_string_lossy().into_owned();
            for _ in 0..(1 + next(6)) {
                spelling.push('/');
                spelling.push_str(parts[next(parts.len())]);
            }
            let path = Path::new(&spelling);
            match std::fs::canonicalize(path) {
                Ok(real) => {
                    existing += 1;
                    assert_eq!(resolve_path(path).ok(), Some(real), "{spelling}");
                }
                Err(_) => {
                    let Ok(resolved) = resolve_path(path) else {
                        // Refused: a loop, a file used as a directory, a
                        // `..` after something that does not exist.
                        refused += 1;
                        continue;
                    };
                    // Stay inside the tree (a `..` chain can leave it).
                    if !resolved.starts_with(&root) || resolved.exists() {
                        continue;
                    }
                    // Create the tail through the spelling, as a tool does.
                    let made = path
                        .parent()
                        .is_some_and(|parent| std::fs::create_dir_all(parent).is_ok())
                        && std::fs::File::create(path).is_ok();
                    if !made {
                        continue;
                    }
                    created += 1;
                    assert!(
                        resolved.is_file(),
                        "{spelling} was created, but not at {}",
                        resolved.display()
                    );
                    assert_eq!(
                        std::fs::canonicalize(path).unwrap(),
                        std::fs::canonicalize(&resolved).unwrap(),
                        "{spelling}"
                    );
                    // Leave the tree as it was for the next spelling.
                    std::fs::remove_file(&resolved).unwrap();
                    let mut up = resolved.parent().map(Path::to_path_buf);
                    while let Some(parent) = up {
                        if std::fs::remove_dir(&parent).is_err() {
                            break;
                        }
                        up = parent.parent().map(Path::to_path_buf);
                    }
                    for sub in ["a/b/c", "a/d", "e/f", "g"] {
                        std::fs::create_dir_all(root.join(sub)).unwrap();
                    }
                }
            }
        }
        assert!(
            existing > 500 && created > 100 && refused > 100,
            "existing {existing}, created {created}, refused {refused}"
        );
        // By name: the differential this resolver exists for.
        assert_eq!(
            resolve_path(&root.join("a/abs-link/../x")).unwrap(),
            root.join("e/x")
        );
        assert_eq!(
            resolve_path(&root.join("a/dangling/x")).unwrap(),
            root.join("nowhere/at/all/x")
        );
        assert!(resolve_path(&root.join("a/nope/../x")).is_err());
        assert!(resolve_path(&root.join("g/loop-a/x")).is_err());
    }

    /// Failure 3, the grant: looked up per write and for that path; only an
    /// explicit `edit_loader` rule that every reading holds counts; and it
    /// lifts loader files, never a host location or an unjudgeable target.
    #[test]
    fn a_grant_lifts_the_path_it_names_and_no_host_location() {
        let t = tree(default_config());
        let p = paths(&t.home);
        let rule = |permission: &str, pattern: &str, action| PermissionV1Rule {
            permission: permission.to_string(),
            pattern: pattern.to_string(),
            action,
        };
        use PermissionV1Action::Allow;
        let guard = |policy: Policy| WriteGuard {
            protected: std::sync::Arc::new(t.protected.clone()),
            policy,
            worktree: t.project.clone(),
        };
        let agents = t.project.join("docs/AGENTS.md");
        let config = t.project.join("opencode.json");
        let host_file = p.data.join("auth.json");
        let global = p.config.join("opencode.json");

        // No grant: both loader files refused. `* allow` is not a grant.
        let none = guard(Policy::plain(vec![
            rule("*", "*", Allow),
            rule("edit", "*", Allow),
        ]));
        assert!(none.check(&agents).is_err());
        assert!(none.check(&config).is_err());
        assert!(none.check(&t.project.join("src/main.rs")).is_ok());

        // A grant for one path lifts that path — relative to the project or
        // absolute — and nothing else.
        let one = guard(Policy::plain(vec![
            rule("*", "*", Allow),
            rule("edit_loader", "docs/AGENTS.md", Allow),
        ]));
        assert!(one.check(&agents).is_ok());
        assert!(one.check(&config).is_err());
        let absolute = guard(Policy::plain(vec![rule(
            "edit_loader",
            &config.to_string_lossy(),
            Allow,
        )]));
        assert!(absolute.check(&config).is_ok());
        assert!(absolute.check(&agents).is_err());

        // The widest grant lifts every loader file and still no host
        // location, and no target that cannot be judged.
        let all = guard(Policy::plain(vec![rule("edit_loader", "*", Allow)]));
        assert!(all.check(&agents).is_ok());
        assert!(all.check(&config).is_ok());
        for target in [host_file, global, t.project.join("gone/../opencode.json")] {
            let error = all.check(&target).unwrap_err();
            assert!(!error.contains("edit_loader"), "{error}");
        }

        // A grant only the project wrote — absent from the host's reading —
        // lifts nothing.
        let projects = guard(
            Policy::plain(vec![rule("*", "*", Allow), rule("edit_loader", "*", Allow)])
                .with_ceiling("host", vec![rule("*", "*", Allow)]),
        );
        assert!(projects.check(&agents).is_err());
    }

    #[test]
    fn overlap_is_both_ways_and_resolves_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("home");
        let p = paths(&home);
        // Nothing exists yet: the locations are compared all the same.
        let host = Protected::host(&p, &root.join("managed"), true);
        let overlaps = |directory: &Path| host.overlap(directory).map(|(what, _)| what);
        // Contains a protected location.
        assert!(overlaps(&home).is_some());
        assert!(overlaps(&root).is_some());
        // Is one.
        assert_eq!(overlaps(&p.data).as_deref(), Some("the data directory"));
        // Is inside one.
        assert!(overlaps(&p.config.join("work")).is_some());
        assert!(overlaps(&p.data.join("worktree/x")).is_some());
        assert!(overlaps(&p.state).is_some());
        assert!(overlaps(&p.cache.join("deep/er")).is_some());
        assert!(overlaps(&root.join("managed")).is_some());
        // Beside them all.
        assert_eq!(overlaps(&root.join("projects/app")), None);
        // A sibling with a location's name as its prefix is not inside it.
        assert_eq!(overlaps(&root.join("managed-other")), None);

        // With the user's config no host config, it is not protected here.
        let managed = Protected::host(&p, &root.join("managed"), false);
        assert_eq!(managed.overlap(&p.config.join("work")), None);
        assert!(managed.overlap(&p.data).is_some());

        #[cfg(unix)]
        {
            // Through a symlink on the directory's side…
            std::fs::create_dir_all(&p.config).unwrap();
            std::os::unix::fs::symlink(&p.config, root.join("alias")).unwrap();
            assert!(overlaps(&root.join("alias")).is_some());
            assert!(overlaps(&root.join("alias/work")).is_some());
            // …and on the location's: a home that is itself a link.
            std::os::unix::fs::symlink(&home, root.join("home-link")).unwrap();
            let linked = Protected::host(&paths(&root.join("home-link")), &root.join("m"), true);
            assert!(linked.overlap(&p.config.join("work")).is_some());
            assert!(linked.overlap(&home).is_some());
        }
    }
}
