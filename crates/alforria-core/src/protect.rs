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

/// See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Protected {
    /// `(what it is, resolved path)`.
    locations: Vec<(String, PathBuf)>,
}

impl Protected {
    fn add(&mut self, what: &str, path: &Path) {
        if path.as_os_str().is_empty() {
            return;
        }
        let resolved = resolve(path);
        if !self.locations.iter().any(|(_, known)| *known == resolved) {
            self.locations.push((what.to_string(), resolved));
        }
    }

    /// The host's locations: what the server reads for every project.
    /// `user_config` is false when `server.trust.host` is `"managed"` — the
    /// user's config files then carry no host key (they are still loader
    /// locations for an instance, [`Protected::for_instance`]).
    pub fn host(paths: &crate::GlobalPaths, managed_dir: &Path, user_config: bool) -> Protected {
        let mut out = Protected::default();
        out.add("the data directory", &paths.data);
        out.add("the state directory", &paths.state);
        out.add("the cache directory", &paths.cache);
        out.add("the managed config directory", managed_dir);
        if user_config {
            out.add("the global config directory", &paths.config);
            out.add(
                "~/.opencode",
                &paths.home.join(crate::project_files::CONFIG_DIR),
            );
        }
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
        {
            out.add("the directory of the alforria binary", &dir);
        }
        out
    }

    /// Everything the loaders consult for one instance: the host's
    /// locations, every config directory the config loader searches, and
    /// the paths the loaded config names.
    pub fn for_instance(
        paths: &crate::GlobalPaths,
        managed_dir: &Path,
        search_dirs: &[PathBuf],
        flags: &crate::config::ConfigFlags,
        config: &crate::config::schema::Config,
        directory: &Path,
    ) -> Protected {
        let mut out = Protected::host(paths, managed_dir, true);
        for dir in [
            crate::project_files::CONFIG_DIR,
            crate::project_files::CLAUDE_DIR,
            crate::project_files::AGENTS_DIR,
        ] {
            out.add("a home loader directory", &paths.home.join(dir));
        }
        for dir in search_dirs {
            out.add("a config directory", dir);
        }
        for (what, value) in [
            ("OPENCODE_CONFIG", &flags.config),
            ("OPENCODE_CONFIG_DIR", &flags.config_dir),
        ] {
            if let Some(path) = value.as_deref().filter(|path| !path.is_empty()) {
                out.add(what, Path::new(path));
            }
        }
        let local = |entry: &str| -> Option<PathBuf> {
            if entry.starts_with("http://") || entry.starts_with("https://") {
                return None;
            }
            // A glob protects the directory its fixed prefix names.
            let fixed = match entry.find(['*', '?', '[', '{']) {
                Some(at) => entry[..at].rsplit_once('/')?.0.to_string(),
                None => entry.to_string(),
            };
            let path = match fixed.strip_prefix("~/") {
                Some(rest) => paths.home.join(rest),
                None => PathBuf::from(&fixed),
            };
            Some(if path.is_absolute() {
                path
            } else {
                directory.join(path)
            })
        };
        let named = [
            (
                "skills.paths",
                config.skills.as_ref().and_then(|s| s.paths.clone()),
            ),
            (
                "workflows.paths",
                config.workflows.as_ref().and_then(|w| w.paths.clone()),
            ),
            ("instructions", config.instructions.clone()),
        ];
        for (what, entries) in named {
            for entry in entries.into_iter().flatten() {
                if let Some(path) = local(&entry) {
                    out.add(what, &path);
                }
            }
        }
        out
    }

    pub fn locations(&self) -> &[(String, PathBuf)] {
        &self.locations
    }

    /// Why a write to `target` is refused, if it is: the path, on its
    /// spelling or where it really leads, has a loader name in it or lies
    /// in a protected location.
    pub fn guards(&self, target: &Path) -> Option<String> {
        let resolved = resolve(target);
        let lexical = crate::git::lexical_normalize(&if target.is_absolute() {
            target.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(target)
        });
        let relative = |path: &Path| -> PathBuf {
            path.components()
                .filter(|part| matches!(part, std::path::Component::Normal(_)))
                .collect()
        };
        if is_loader_path(&relative(&resolved)) || is_loader_path(&relative(&lexical)) {
            return Some(format!(
                "{} is a file the agent's configuration is loaded from (config, \
                 instructions, agents, commands, skills, workflows, git hooks)",
                target.display()
            ));
        }
        self.locations
            .iter()
            .find(|(_, location)| within(&resolved, location))
            .map(|(what, location)| {
                format!("{} is in {what} ({})", target.display(), location.display())
            })
    }

    /// The protected location `directory` overlaps — one contains the
    /// other, or they are the same — if any: `(what, location)`.
    pub fn overlap(&self, directory: &Path) -> Option<(String, PathBuf)> {
        let directory = resolve(directory);
        self.locations
            .iter()
            .find(|(_, location)| within(location, &directory) || within(&directory, location))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(home: &Path) -> crate::GlobalPaths {
        crate::GlobalPaths::resolve_with(home.to_path_buf(), |_| None)
    }

    #[test]
    fn a_write_is_judged_on_names_anywhere_and_on_locations() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("home");
        let project = root.join("outer/project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::create_dir_all(home.join("skills-elsewhere")).unwrap();
        let p = paths(&home);
        let config: crate::config::schema::Config = serde_json::from_value(serde_json::json!({
            "skills": { "paths": ["~/skills-elsewhere", "vendor/skills"] },
            "instructions": ["docs/rules/*.md", "https://x.example/rules.md", "/abs/rules.md"],
        }))
        .unwrap();
        let protected = Protected::for_instance(
            &p,
            &root.join("managed"),
            &[p.config.clone(), project.join(".opencode")],
            &crate::config::ConfigFlags::default(),
            &config,
            &project,
        );
        for target in [
            // The project's own loader files…
            project.join("opencode.json"),
            project.join(".opencode/agent/x.md"),
            project.join("src/AGENTS.md"),
            // …an ancestor's, which the walk up reads…
            root.join("outer/AGENTS.md"),
            root.join("CLAUDE.md"),
            root.join("outer/.opencode/command/c.md"),
            // …the home's and the global ones…
            home.join(".claude/skills/s/SKILL.md"),
            home.join(".agents/skills/s/SKILL.md"),
            p.config.join("agent/mine.md"),
            p.config.join("themes/x.json"),
            p.data.join("auth.json"),
            p.state.join("anything"),
            root.join("managed/opencode.json"),
            // …and what the config names.
            home.join("skills-elsewhere/s/SKILL.md"),
            project.join("vendor/skills/s/SKILL.md"),
            project.join("docs/rules/security.md"),
            PathBuf::from("/abs/rules.md"),
        ] {
            assert!(protected.guards(&target).is_some(), "{}", target.display());
        }
        for target in [
            project.join("src/main.rs"),
            project.join("docs/readme.md"),
            root.join("outer/notes.txt"),
            home.join("documents/x.txt"),
        ] {
            assert_eq!(protected.guards(&target), None, "{}", target.display());
        }
        #[cfg(unix)]
        {
            // Reached through a link to a location, and to a loader directory.
            std::fs::create_dir_all(&p.data).unwrap();
            std::os::unix::fs::symlink(&p.data, project.join("d")).unwrap();
            assert!(protected.guards(&project.join("d/auth.json")).is_some());
            std::fs::create_dir_all(project.join(".opencode")).unwrap();
            std::os::unix::fs::symlink(project.join(".opencode"), project.join("cfg")).unwrap();
            assert!(protected.guards(&project.join("cfg/agent/x.md")).is_some());
        }
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
