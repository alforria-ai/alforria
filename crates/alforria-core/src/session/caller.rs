//! Who a prompt comes from (alforria extension).
//!
//! Upstream has one kind of caller: the operator, who may name any agent,
//! attach any file and run any command. A server that also takes scoped
//! bearer tokens has a second kind — someone who may talk to the agent but
//! must not execute anything except through the model under the operator's
//! permission policy, nor change that policy.
//!
//! The distinction travels with the request ([`PromptInput::caller`],
//! `CommandInput::caller`) *and stays with the session*: a session a
//! restricted caller created is restricted for life, with its children and
//! forks, so the turns the engine starts by itself — goal rounds, subagent
//! runs, resumes — are bound as well. It is enforced here in core, at the
//! points where the effective values are resolved and used: the agent that
//! will actually run, the parts that will actually be stored, the file that
//! is actually about to be read. A check cannot look at one spelling of a
//! value while the engine uses another, and a turn source that never heard
//! of callers is bound by the session it runs in.
//!
//! [`PromptInput::caller`]: crate::session::prompt_input::PromptInput

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::session::agents::AgentInfo;
use crate::tool::def::AgentMode;

/// Where a prompt or command comes from. There is deliberately no default:
/// every place that builds a prompt says which of these it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// A request from the operator: the TUI, the CLI, Basic auth, an
    /// `admin` token. Upstream behaviour — unless the session itself is
    /// restricted, which bounds the operator's requests to it too.
    Operator,
    /// A request from a credential below `admin` (`chat`, `connector`).
    Restricted,
    /// A turn the engine starts by itself: a `/goal` round, a subagent the
    /// task tool runs, a workflow agent, a resume, a trigger. It has no
    /// caller of its own; it is bound by whoever the session belongs to.
    Session,
}

/// What applies to one turn: the caller's kind combined with the
/// session's own restriction.
///
/// A session created by a restricted caller is restricted for its whole
/// life ([`SessionStore::restrict`]), and so are its children and forks.
/// That is what binds the turns nobody is making a request for: the
/// restriction is read from the session, at the one place every turn
/// passes through, so a turn source that forgets about callers altogether
/// is still bound.
///
/// [`SessionStore::restrict`]: crate::session::store::SessionStore::restrict
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// The turn is bound by the restrictions of a caller below `admin`.
    pub restricted: bool,
    /// The turn comes from a request (as opposed to the engine itself):
    /// its agent, parts and ids are of somebody's choosing.
    pub from_request: bool,
}

impl Caller {
    /// Whether this caller may act on a session at all: prompt it, run a
    /// command or a goal in it.
    ///
    /// A caller below `admin` acts only on restricted sessions — its own
    /// kind. Anything it starts in a session can schedule turns the engine
    /// runs later with the *session's* rights (goal rounds, continuations,
    /// subagent results); in an operator's session those would be the
    /// operator's rights. Rather than track who caused each scheduled turn,
    /// the rule is that it cannot cause any there. (A fork is how it gets a
    /// session of its own from an operator's: the fork is restricted.)
    pub fn may_act_on(self, session_restricted: bool) -> Result<(), String> {
        if self == Caller::Restricted && !session_restricted {
            return Err(
                "this session belongs to the operator: this token can read it or fork it, \
                 but not act in it"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// The bounds of a turn from this caller on a session that is, or is
    /// not, itself restricted.
    pub fn bounds(self, session_restricted: bool) -> Bounds {
        Bounds {
            restricted: session_restricted || self == Caller::Restricted,
            from_request: self != Caller::Session,
        }
    }
}

/// Where an open of `path`, as it is spelled, would land: the one resolver
/// ([`crate::protect::resolve_path`]) — links read where they stand, `..`
/// applied to the resolved path and never folded out of the spelling. An
/// error (a `..` after a component that does not exist, a link loop, an
/// unreadable directory) is for the caller to treat as "outside".
pub fn resolve_for_access(path: &Path) -> std::io::Result<PathBuf> {
    crate::protect::resolve_path(path)
}

/// The project a restricted caller is confined to: the instance directory,
/// and its git worktree when it has one (a non-git project's worktree is
/// `/`, which is not a boundary — `containsPath`,
/// `instance-context.ts:18-24`).
pub fn project_roots(directory: &Path, worktree: &Path) -> Vec<PathBuf> {
    // The trusted side: a root that cannot be resolved keeps its spelling
    // (`protect::resolve`), which nothing resolved will be found inside.
    let mut roots = vec![crate::protect::resolve(directory)];
    if worktree != Path::new("/") && !worktree.as_os_str().is_empty() {
        let worktree = crate::protect::resolve(worktree);
        if !roots.contains(&worktree) {
            roots.push(worktree);
        }
    }
    roots
}

impl Bounds {
    pub fn is_restricted(self) -> bool {
        self.restricted
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
    /// caller it must be inside the project where the kernel would land
    /// the path as spelled — `..` and every symlink on the way followed in
    /// the order they stand; a path that cannot be resolved is outside.
    pub fn check_read(self, path: &Path, directory: &Path, worktree: &Path) -> Result<(), String> {
        if !self.is_restricted() {
            return Ok(());
        }
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            directory.join(path)
        };
        let inside = resolve_for_access(&path).is_ok_and(|resolved| {
            project_roots(directory, worktree)
                .iter()
                .any(|root| crate::protect::within(&resolved, root))
        });
        if inside {
            Ok(())
        } else {
            Err("this token may only attach files inside the project".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESTRICTED: Bounds = Bounds {
        restricted: true,
        from_request: true,
    };

    #[test]
    fn the_session_restriction_binds_every_kind_of_caller() {
        for caller in [Caller::Operator, Caller::Restricted, Caller::Session] {
            assert!(
                caller.bounds(true).restricted,
                "{caller:?} on a restricted session"
            );
        }
        assert!(Caller::Restricted.bounds(false).restricted);
        assert!(!Caller::Operator.bounds(false).restricted);
        assert!(!Caller::Session.bounds(false).restricted);
        assert!(Caller::Operator.bounds(false).from_request);
        assert!(!Caller::Session.bounds(true).from_request);
    }

    #[test]
    fn a_restricted_caller_acts_only_on_restricted_sessions() {
        assert!(Caller::Restricted.may_act_on(true).is_ok());
        assert!(Caller::Restricted.may_act_on(false).is_err());
        for caller in [Caller::Operator, Caller::Session] {
            assert!(caller.may_act_on(true).is_ok());
            assert!(caller.may_act_on(false).is_ok());
        }
    }

    #[test]
    fn the_operator_is_never_confined() {
        let bounds = Caller::Operator.bounds(false);
        assert!(bounds
            .check_read(Path::new("/etc/passwd"), Path::new("/repo"), Path::new("/"))
            .is_ok());
        let mut tools = BTreeMap::new();
        tools.insert("bash".to_string(), true);
        assert!(bounds.check_tools(Some(&tools)).is_ok());
    }

    #[test]
    fn a_restricted_caller_switches_tools_off_only() {
        let mut tools = BTreeMap::new();
        tools.insert("webfetch".to_string(), false);
        assert!(RESTRICTED.check_tools(Some(&tools)).is_ok());
        tools.insert("bash".to_string(), true);
        assert!(RESTRICTED.check_tools(Some(&tools)).is_err());
        assert!(RESTRICTED.check_tools(None).is_ok());
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
        let check = |path: &str| RESTRICTED.check_read(Path::new(path), &project, Path::new("/"));
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
            // `..` is applied where the link led, not to the spelling:
            // `linkdir/../outside/secret` is the secret, though folding the
            // spelling first reads it as `project/outside/secret`.
            std::fs::create_dir_all(project.join("outside")).unwrap();
            std::fs::write(project.join("outside/secret"), "decoy").unwrap();
            assert!(check("linkdir/../outside/secret").is_err());
            // What cannot be resolved is outside.
            assert!(check("nowhere/../src/a.rs").is_err());
        }
        // A git worktree above the directory is part of the project.
        assert!(RESTRICTED
            .check_read(&root.join("outside/secret"), &project, &root)
            .is_ok());
    }
}
