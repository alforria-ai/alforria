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
//! calls it with the destination it is about to modify
//! ([`crate::tool::def::InstanceContext::about_to_modify`]) — enforcement
//! does not depend on what a tool put in a permission ask.
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
//!    lands differs from the kernel's. The guard does not try to be clever
//!    about it: a target with a `..` in it, a final component that is a
//!    symlink, and a dangling symlink on the way are *refused* outright in
//!    a guarded session ([`landing`]). What is left resolves the same way
//!    for the guard and the kernel — links in existing directories followed
//!    by `canonicalize`, the rest created as spelled. (This is the
//!    conservative version. Judging and writing through one directory
//!    handle — `openat` the parent, create the last component with
//!    `O_NOFOLLOW` — would also close the window in which a link is swapped
//!    between the check and the write; it is not done here, and a session
//!    that can swap links has `bash` and needs no such trick.)
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
use std::path::{Path, PathBuf};

use crate::project_files::is_loader_path;

/// Where `path` really is: made absolute and normalised, the deepest
/// existing ancestor canonicalised (every symlink on the way followed) and
/// the part that does not exist yet appended. Both sides of every
/// comparison in this module go through here, so a symlinked home, a
/// location that has not been created yet and `..` are all compared like
/// with like.
pub fn resolve(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    crate::tool::external_directory::real_path(&absolute)
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

/// Whether `inner` is `outer` or below it (component-wise).
fn within(inner: &Path, outer: &Path) -> bool {
    key(inner).starts_with(key(outer))
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

/// Where a write to `target` lands — the one resolver for that question —
/// or why it cannot be said. `target` is taken as spelled, never normalised:
/// a `..` component, a final component that is a symlink, and a dangling
/// symlink where the path stops existing are refused; for anything else the
/// deepest existing directory resolves as the kernel resolves it and the
/// remainder is created as spelled.
pub fn landing(target: &Path) -> Result<PathBuf, String> {
    let absolute = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(target)
    };
    if absolute
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(format!(
            "{} has a `..` in it: where it leads cannot be judged safely",
            target.display()
        ));
    }
    if std::fs::symlink_metadata(&absolute).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(format!("{} is a symbolic link", target.display()));
    }
    // The deepest ancestor that exists, and what is yet to be created.
    let mut existing = absolute.as_path();
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    let real = loop {
        match std::fs::canonicalize(existing) {
            Ok(real) => break real,
            Err(_) => {
                // Not there — or a link to nowhere, which a write through
                // it would follow.
                if std::fs::symlink_metadata(existing).is_ok() {
                    return Err(format!(
                        "{} goes through a dangling symbolic link",
                        target.display()
                    ));
                }
                match (existing.parent(), existing.file_name()) {
                    (Some(parent), Some(name)) => {
                        rest.push(name);
                        existing = parent;
                    }
                    _ => return Err(format!("{} cannot be resolved", target.display())),
                }
            }
        }
    };
    Ok(rest.iter().rev().fold(real, |path, part| path.join(part)))
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

    /// Every protected location, host and loader.
    pub fn locations(&self) -> Vec<(String, PathBuf)> {
        self.host.iter().chain(&self.loader).cloned().collect()
    }

    /// Why a write to `target` is refused, if it is.
    pub fn guards(&self, target: &Path) -> Option<Refusal> {
        let landing = match landing(target) {
            Ok(landing) => landing,
            Err(reason) => {
                return Some(Refusal {
                    class: Class::Host,
                    reason,
                    landing: None,
                })
            }
        };
        let refusal = |class, reason: String| {
            Some(Refusal {
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
        None
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
        let Some(refusal) = self.protected.guards(target) else {
            return Ok(());
        };
        let grant = crate::project_files::EDIT_LOADER_PERMISSION;
        let lifted = refusal.class == Class::Loader
            && refusal.landing.as_ref().is_some_and(|landing| {
                let absolute = landing.to_string_lossy();
                let relative = landing
                    .strip_prefix(resolve(&self.worktree))
                    .map(|path| path.to_string_lossy().into_owned());
                self.policy.explicitly_allows(grant, &absolute)
                    || relative.is_ok_and(|path| self.policy.explicitly_allows(grant, &path))
            });
        if lifted {
            return Ok(());
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
            ("to-elsewhere/../.opencode/x.md", ".."),
            ("src/../AGENTS.md", ".."),
            ("to-agents", "symbolic link"),
            ("dangling", "symbolic link"),
            ("dangling/x.md", "dangling"),
        ] {
            let refusal = t.protected.guards(&project.join(target)).expect(target);
            assert!(refusal.reason.contains(why), "{target}: {}", refusal.reason);
            assert_eq!(refusal.class, Class::Host, "{target}: not liftable");
        }
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
        for target in [host_file, global, t.project.join("src/../opencode.json")] {
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
