//! The names the loaders look for inside a project (alforria extension).
//!
//! Config, instructions, agents, commands, skills and workflows are all
//! discovered by file name, in the project directory and up its ancestors.
//! Whoever can create a file with one of these names changes what the agent
//! is told, what it is allowed, or what runs. The names live here, and the
//! loaders take them from here, so that the one place that must refuse to
//! write them on behalf of a caller below `admin` (the upload route) cannot
//! fall behind the loaders: a discovery location added to these constants
//! is protected the moment it is discovered.

use std::path::Path;

/// Project config files (`config/precedence.rs`), most specific last.
pub const PROJECT_CONFIG_FILES: &[&str] = &["opencode.jsonc", "opencode.json"];

/// Config file names read from a config directory (`config/precedence.rs`).
pub const CONFIG_DIR_FILES: &[&str] = &["config.json", "opencode.json", "opencode.jsonc"];

/// The project config directory: config files, agents, commands, modes,
/// skills, workflows, plans, plugins.
pub const CONFIG_DIR: &str = ".opencode";

/// Claude Code's directory, read for skills and workflows.
pub const CLAUDE_DIR: &str = ".claude";

/// The cross-agent directory, read for skills.
pub const AGENTS_DIR: &str = ".agents";

/// Instruction files (`session/instruction.rs`), found in the project and
/// beside files the agent reads.
pub const AGENTS_FILE: &str = "AGENTS.md";
pub const CLAUDE_FILE: &str = "CLAUDE.md";
pub const CONTEXT_FILE: &str = "CONTEXT.md";

/// The repository's own directory: hooks there run as code whenever the
/// server calls git.
pub const GIT_DIR: &str = ".git";

/// alforria's own project directory, reserved.
pub const ALFORRIA_DIR: &str = ".alforria";

/// Every directory name whose contents the loaders (or git) act on.
pub const LOADER_DIRS: &[&str] = &[CONFIG_DIR, CLAUDE_DIR, AGENTS_DIR, GIT_DIR, ALFORRIA_DIR];

/// Every file name the loaders act on, wherever in the project it sits.
pub fn loader_files() -> Vec<&'static str> {
    let mut files: Vec<&'static str> = Vec::new();
    for name in PROJECT_CONFIG_FILES.iter().chain(CONFIG_DIR_FILES).chain(&[
        AGENTS_FILE,
        CLAUDE_FILE,
        CONTEXT_FILE,
    ]) {
        if !files.contains(name) {
            files.push(name);
        }
    }
    files
}

/// Whether a path inside the project names something the loaders act on:
/// any component is a loader directory or a loader file, compared without
/// regard to case (the filesystem may not have any).
pub fn is_loader_path(relative: &Path) -> bool {
    let files = loader_files();
    relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        LOADER_DIRS
            .iter()
            .chain(files.iter())
            .any(|loader| loader.eq_ignore_ascii_case(&name))
    })
}

/// The permission an operator grants to let a restricted or
/// outside-input session edit loader paths after all
/// (`"permission": { "edit_loader": "allow" }` in the host config).
pub const EDIT_LOADER_PERMISSION: &str = "edit_loader";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_paths_match_at_any_depth_and_in_any_case() {
        for path in [
            "opencode.json",
            "OPENCODE.JSONC",
            "config.json",
            "AGENTS.md",
            "agents.md",
            "docs/CLAUDE.md",
            "a/b/CONTEXT.md",
            ".opencode/agent/x.md",
            ".opencode",
            "pkg/.opencode/command/y.md",
            ".claude/skills/s/SKILL.md",
            ".claude/workflows/w.js",
            ".agents/skills/s/SKILL.md",
            ".git/hooks/pre-commit",
            ".GIT/config",
            ".alforria/x",
        ] {
            assert!(is_loader_path(Path::new(path)), "{path}");
        }
        for path in [
            "uploads/report.pdf",
            "src/main.rs",
            "opencode.json.bak",
            "gitignore",
            "my.opencode/x",
        ] {
            assert!(!is_loader_path(Path::new(path)), "{path}");
        }
    }
}
