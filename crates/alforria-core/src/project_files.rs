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

/// Config files of the formatters and language servers the server starts
/// (`lsp/server.rs`, upstream's `format/formatter.ts`): a file with one of
/// these names is run as code by such a tool (`eslint.config.js`,
/// `build.rs` through `Cargo.toml`, `flake.nix`), names plugins or commands
/// it loads (`biome.json`, `tsconfig.json`, `.clangd`,
/// `compile_commands.json`), or decides which binary is started
/// (`package.json`). An agent edits them as part of its work, so the write
/// guard leaves them alone; a file *uploaded* by a credential below `admin`
/// is a write nobody asked the model for, and is refused these names
/// ([`is_tool_config_path`]).
pub const TOOL_CONFIG_FILES: &[&str] = &[
    // JavaScript / TypeScript.
    "package.json",
    "tsconfig.json",
    "jsconfig.json",
    "deno.json",
    "deno.jsonc",
    "biome.json",
    "biome.jsonc",
    ".oxlintrc.json",
    ".eslintrc",
    ".eslintrc.js",
    ".eslintrc.cjs",
    ".eslintrc.json",
    ".eslintrc.yaml",
    ".eslintrc.yml",
    "eslint.config.js",
    "eslint.config.mjs",
    "eslint.config.cjs",
    "eslint.config.ts",
    "eslint.config.mts",
    "eslint.config.cts",
    ".prettierrc",
    ".prettierrc.json",
    ".prettierrc.yaml",
    ".prettierrc.yml",
    ".prettierrc.toml",
    ".prettierrc.js",
    ".prettierrc.cjs",
    ".prettierrc.mjs",
    "prettier.config.js",
    "prettier.config.cjs",
    "prettier.config.mjs",
    "prettier.config.ts",
    "svelte.config.js",
    "astro.config.mjs",
    // Rust.
    "Cargo.toml",
    "build.rs",
    "rustfmt.toml",
    ".rustfmt.toml",
    "rust-toolchain",
    "rust-toolchain.toml",
    // Python.
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "ruff.toml",
    ".ruff.toml",
    "ty.toml",
    "pyrightconfig.json",
    "Pipfile",
    // Go, Ruby, Elixir, Zig, PHP, Dart, OCaml, Gleam, Typst, Julia, Haskell.
    "go.mod",
    "go.work",
    "Gemfile",
    ".rubocop.yml",
    "mix.exs",
    ".formatter.exs",
    "build.zig",
    "build.zig.zon",
    "composer.json",
    "pubspec.yaml",
    "analysis_options.yaml",
    "dune-project",
    "dune-workspace",
    ".merlin",
    ".ocamlformat",
    "gleam.toml",
    "typst.toml",
    "Project.toml",
    "stack.yaml",
    "cabal.project",
    "hie.yaml",
    // C / C++.
    ".clangd",
    ".clang-format",
    "compile_commands.json",
    "compile_flags.txt",
    // JVM, .NET.
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "global.json",
    // Lua, Nix, LaTeX, editor-wide.
    ".luarc.json",
    ".luarc.jsonc",
    "stylua.toml",
    ".stylua.toml",
    "selene.toml",
    "flake.nix",
    ".latexmkrc",
    "latexmkrc",
    ".editorconfig",
];

/// Directories a formatter or language server takes its executable or its
/// settings from: `node_modules/.bin/<server>` is started in preference to
/// the one on `PATH`, `.cargo/config.toml` names a compiler wrapper,
/// `.venv` holds the interpreter.
pub const TOOL_CONFIG_DIRS: &[&str] = &["node_modules", ".cargo", ".venv", "venv", ".vscode"];

/// Whether a path inside the project names a formatter's or language
/// server's config ([`TOOL_CONFIG_FILES`], [`TOOL_CONFIG_DIRS`]) in any
/// component, without regard to case.
pub fn is_tool_config_path(relative: &Path) -> bool {
    relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        TOOL_CONFIG_FILES
            .iter()
            .chain(TOOL_CONFIG_DIRS)
            .any(|tool| tool.eq_ignore_ascii_case(&name))
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

    #[test]
    fn tool_config_paths_match_at_any_depth_and_in_any_case() {
        for path in [
            "package.json",
            "uploads/PACKAGE.JSON",
            "uploads/eslint.config.js",
            "uploads/node_modules/.bin/biome",
            "a/.cargo/config.toml",
            "Cargo.toml",
            "x/build.rs",
            ".clangd",
        ] {
            assert!(is_tool_config_path(Path::new(path)), "{path}");
        }
        for path in ["uploads/report.pdf", "src/main.rs", "package.json.bak"] {
            assert!(!is_tool_config_path(Path::new(path)), "{path}");
        }
        // An agent's own edits are not refused these names.
        assert!(!is_loader_path(Path::new("package.json")));
    }
}
