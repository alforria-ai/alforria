//! Who a prompt comes from (alforria extension).
//!
//! Upstream has one kind of caller: the operator, who may name any agent,
//! attach any file and run any command. A server that also takes scoped
//! bearer tokens has a second kind — someone who may talk to the agent but
//! must not execute anything except through the model under the operator's
//! permission policy, nor change that policy.
//!
//! The distinction travels *with the request* ([`PromptInput::caller`],
//! `CommandInput::caller`) and is enforced here in core, at the points
//! where the effective values are resolved and used: the agent that will
//! actually run, the parts that will actually be stored, the file that is
//! actually about to be read. A route only says who is calling; it cannot
//! forget a check, and a check cannot look at one spelling of a value while
//! the engine uses another.
//!
//! [`PromptInput::caller`]: crate::session::prompt_input::PromptInput

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::session::agents::AgentInfo;
use crate::tool::def::AgentMode;

/// The kind of caller a prompt or command comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Caller {
    /// The operator: the TUI, the CLI, Basic auth, an `admin` token, the
    /// engine's own synthetic prompts. Upstream behaviour, unrestricted.
    #[default]
    Operator,
    /// A credential below `admin` (`chat`, `connector`).
    Restricted,
}

/// Resolve `path` the way a symlink-following open would reach it: the
/// deepest existing ancestor is canonicalized and the rest (which does not
/// exist, so cannot be a link) is appended after lexical normalization.
pub fn resolve_for_access(path: &Path) -> PathBuf {
    let normalized = crate::git::lexical_normalize(path);
    let mut existing = normalized.as_path();
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            let mut resolved = crate::filesystem::strip_verbatim(real);
            for part in tail.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name);
                existing = parent;
            }
            _ => return normalized,
        }
    }
}

/// The project a restricted caller is confined to: the instance directory,
/// and its git worktree when it has one (a non-git project's worktree is
/// `/`, which is not a boundary — `containsPath`,
/// `instance-context.ts:18-24`).
pub fn project_roots(directory: &Path, worktree: &Path) -> Vec<PathBuf> {
    let mut roots = vec![resolve_for_access(directory)];
    if worktree != Path::new("/") && !worktree.as_os_str().is_empty() {
        let worktree = resolve_for_access(worktree);
        if !roots.contains(&worktree) {
            roots.push(worktree);
        }
    }
    roots
}

impl Caller {
    pub fn is_restricted(self) -> bool {
        self == Caller::Restricted
    }

    /// The agent a turn is about to run as, whatever named it: the request,
    /// the session, a command's configuration or the default. A restricted
    /// caller gets the agents a client's picker offers — never a hidden one
    /// (title, summary, compaction and the like) and never a subagent-only
    /// one.
    pub fn check_agent(self, agent: &AgentInfo) -> Result<(), String> {
        if self.is_restricted() && (!agent.is_visible() || agent.mode == AgentMode::Subagent) {
            return Err(format!(
                "this token may not run the {} agent: it is hidden or a subagent",
                agent.name
            ));
        }
        Ok(())
    }

    /// The per-prompt `tools` map becomes session permission rules (`true`
    /// → allow). A restricted caller may switch tools off, never on.
    pub fn check_tools(self, tools: Option<&BTreeMap<String, bool>>) -> Result<(), String> {
        if !self.is_restricted() {
            return Ok(());
        }
        match tools.and_then(|tools| tools.iter().find(|(_, enabled)| **enabled)) {
            Some((tool, _)) => Err(format!(
                "this token may not enable tools ({tool}): the tools map can only switch tools off"
            )),
            None => Ok(()),
        }
    }

    /// A file about to be read on the caller's behalf. For a restricted
    /// caller it must be inside the project once `..` and every existing
    /// symlink on the way are followed.
    pub fn check_read(self, path: &Path, directory: &Path, worktree: &Path) -> Result<(), String> {
        if !self.is_restricted() {
            return Ok(());
        }
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            directory.join(path)
        };
        let resolved = resolve_for_access(&path);
        if project_roots(directory, worktree)
            .iter()
            .any(|root| resolved.starts_with(root))
        {
            Ok(())
        } else {
            Err("this token may only attach files inside the project".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_operator_is_never_confined() {
        let caller = Caller::default();
        assert_eq!(caller, Caller::Operator);
        assert!(caller
            .check_read(Path::new("/etc/passwd"), Path::new("/repo"), Path::new("/"))
            .is_ok());
        let mut tools = BTreeMap::new();
        tools.insert("bash".to_string(), true);
        assert!(caller.check_tools(Some(&tools)).is_ok());
    }

    #[test]
    fn a_restricted_caller_switches_tools_off_only() {
        let mut tools = BTreeMap::new();
        tools.insert("webfetch".to_string(), false);
        assert!(Caller::Restricted.check_tools(Some(&tools)).is_ok());
        tools.insert("bash".to_string(), true);
        assert!(Caller::Restricted.check_tools(Some(&tools)).is_err());
        assert!(Caller::Restricted.check_tools(None).is_ok());
    }

    #[test]
    fn reads_are_judged_after_links_and_dots_are_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let project = root.join("project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::fs::write(root.join("outside/secret"), "s").unwrap();
        std::fs::write(project.join("src/a.rs"), "a").unwrap();
        let check =
            |path: &str| Caller::Restricted.check_read(Path::new(path), &project, Path::new("/"));
        assert!(check("src/a.rs").is_ok());
        assert!(check(&project.join("src/a.rs").display().to_string()).is_ok());
        assert!(check("src/../src/a.rs").is_ok());
        assert!(check("not-there-yet.txt").is_ok());
        assert!(check("../outside/secret").is_err());
        assert!(check("src/../../outside/secret").is_err());
        assert!(check(&root.join("outside/secret").display().to_string()).is_err());
        assert!(check("/etc/passwd").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("outside/secret"), project.join("link")).unwrap();
            std::os::unix::fs::symlink(root.join("outside"), project.join("linkdir")).unwrap();
            assert!(check("link").is_err());
            assert!(check("linkdir/secret").is_err());
            assert!(check("linkdir/new/file").is_err());
        }
        // A git worktree above the directory is part of the project.
        assert!(Caller::Restricted
            .check_read(&root.join("outside/secret"), &project, &root)
            .is_ok());
    }
}
