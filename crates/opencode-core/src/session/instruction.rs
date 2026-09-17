//! Instruction service — port of `session/instruction.ts`.
//!
//! `extract`/`loaded` collect `metadata.loaded` paths from completed
//! non-compacted `read` tool parts; `resolve` walks upward from a file
//! being read and attaches nearby instruction files once per message
//! (per-message "claims" state); `system` assembles the system-prompt
//! instruction blocks.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use opencode_schema::session_v1::{V1Part, V1ToolState};
use serde_json::Value;

use crate::session::message::WithParts;

/// `AGENTS.md`, `CLAUDE.md` (flag-gated), `CONTEXT.md` (deprecated)
/// (instruction.ts:64-68).
fn instruction_files(disable_claude_code_prompt: bool) -> &'static [&'static str] {
    match disable_claude_code_prompt {
        true => &["AGENTS.md", "CONTEXT.md"],
        false => &["AGENTS.md", "CLAUDE.md", "CONTEXT.md"],
    }
}

/// `extract` (instruction.ts:17-32): the `metadata.loaded` paths of
/// completed, non-compacted `read` tool parts.
pub fn extract(messages: &[WithParts]) -> HashSet<String> {
    let mut paths = HashSet::new();
    for msg in messages {
        for part in &msg.parts {
            let V1Part::Tool { tool, state, .. } = part else {
                continue;
            };
            if tool != "read" {
                continue;
            }
            let V1ToolState::Completed { time, metadata, .. } = state else {
                continue;
            };
            if time.compacted.is_some() {
                continue;
            }
            let Some(loaded) = metadata.get("loaded").and_then(Value::as_array) else {
                continue;
            };
            for path in loaded {
                if let Some(path) = path.as_str() {
                    paths.insert(path.to_string());
                }
            }
        }
    }
    paths
}

/// `loaded` export (instruction.ts:227-229).
pub fn loaded(messages: &[WithParts]) -> HashSet<String> {
    extract(messages)
}

/// `path.resolve` — lexical absolute path normalization (no filesystem
/// access, `..` components resolved).
pub fn lexical_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn dirname(path: &Path) -> PathBuf {
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf())
}

/// Node `fs.exists` — any filesystem entry.
fn exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// `FSUtil.findUp` (fs-util.ts:154-166): collect every existing
/// `join(current, target)` from `start` up to (and including) `stop`.
pub fn find_up(target: &str, start: &Path, stop: Option<&Path>) -> Vec<PathBuf> {
    let stop = stop.map(lexical_absolute);
    let mut current = lexical_absolute(start);
    let mut result = Vec::new();
    loop {
        let search = current.join(target);
        if exists(&search) {
            result.push(lexical_absolute(&search));
        }
        if stop.as_ref() == Some(&current) {
            break;
        }
        let parent = dirname(&current);
        if parent == current {
            break;
        }
        current = parent;
    }
    result
}

/// `fs.glob` (fs-util.ts:126-151): files below `dir` matching `pattern`,
/// absolute paths, hidden files included (`dot: true`).
pub fn glob_files(dir: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut builder = ignore::WalkBuilder::new(dir);
    builder
        .hidden(false)
        .follow_links(true)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .require_git(false);
    let Ok(glob) = globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
    else {
        return Vec::new();
    };
    let Ok(globs) = globset::GlobSetBuilder::new().add(glob).build() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in builder.build() {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(dir) else {
            continue;
        };
        if globs.is_match(relative.to_string_lossy().replace('\\', "/")) {
            found.push(lexical_absolute(entry.path()));
        }
    }
    found.sort();
    found
}

/// `FSUtil.globUp` (fs-util.ts:184-201): glob in each directory from
/// `start` up to (and including) `stop`.
fn glob_up(pattern: &str, start: &Path, stop: Option<&Path>) -> Vec<PathBuf> {
    let stop = stop.map(lexical_absolute);
    let mut current = lexical_absolute(start);
    let mut result = Vec::new();
    loop {
        result.extend(glob_files(&current, pattern));
        if stop.as_ref() == Some(&current) {
            break;
        }
        let parent = dirname(&current);
        if parent == current {
            break;
        }
        current = parent;
    }
    result
}

/// Remote instruction fetching — `https://` config instruction entries
/// fetched with a 5 s timeout (instruction.ts:95-103). The default
/// implementation performs no fetch (failures resolve to `""`).
pub trait RemoteInstructions: Send + Sync {
    fn fetch(&self, url: &str) -> String;
}

/// The no-op remote fetcher — remote URLs resolve to `""`.
#[derive(Debug, Clone, Default)]
pub struct NoRemoteInstructions;

impl RemoteInstructions for NoRemoteInstructions {
    fn fetch(&self, _url: &str) -> String {
        String::new()
    }
}

/// Runtime flags the instruction service reads.
#[derive(Debug, Clone, Copy, Default)]
pub struct InstructionFlags {
    /// `flags.disableClaudeCodePrompt` — also gates the `CLAUDE.md`
    /// instruction file.
    pub disable_claude_code_prompt: bool,
    /// `Flag.OPENCODE_DISABLE_PROJECT_CONFIG`.
    pub disable_project_config: bool,
}

#[derive(Debug, Clone, Default)]
pub struct InstructionPaths {
    /// `Global.Path.config` — the global config directory.
    pub config: PathBuf,
    /// `global.home` — the user home directory.
    pub home: PathBuf,
    /// `ctx.directory` — the instance directory.
    pub directory: PathBuf,
    /// `ctx.worktree` — the instance worktree root.
    pub worktree: PathBuf,
}

/// A resolved instruction block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInstruction {
    pub filepath: PathBuf,
    pub content: String,
}

/// `Instruction.Service` (instruction.ts:48-224).
pub struct Instruction {
    flags: InstructionFlags,
    paths: InstructionPaths,
    /// `cfg.instructions`.
    config_instructions: Vec<String>,
    remote: Arc<dyn RemoteInstructions>,
    /// `s.claims` — `Map<MessageID, Set<string>>`.
    claims: Mutex<HashMap<String, HashSet<String>>>,
}

impl Instruction {
    pub fn new(
        flags: InstructionFlags,
        paths: InstructionPaths,
        config_instructions: Vec<String>,
        remote: Arc<dyn RemoteInstructions>,
    ) -> Instruction {
        Instruction {
            flags,
            paths,
            config_instructions,
            remote,
            claims: Mutex::new(HashMap::new()),
        }
    }

    fn instruction_files(&self) -> &'static [&'static str] {
        instruction_files(self.flags.disable_claude_code_prompt)
    }

    fn relative(&self, instruction: &str) -> Vec<PathBuf> {
        if !self.flags.disable_project_config {
            return glob_up(
                instruction,
                &self.paths.directory,
                Some(&self.paths.worktree),
            );
        }
        glob_up(instruction, &self.paths.config, Some(&self.paths.config))
    }

    /// `read` (instruction.ts:91-93): read a file, `""` on failure.
    fn read(&self, filepath: &Path) -> String {
        std::fs::read_to_string(filepath).unwrap_or_default()
    }

    /// `clear` (instruction.ts:105-108).
    pub fn clear(&self, message_id: &str) {
        self.claims
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(message_id);
    }

    /// `systemPaths` (instruction.ts:110-153). Insertion-ordered set.
    pub fn system_paths(&self) -> Vec<PathBuf> {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut paths: Vec<PathBuf> = Vec::new();
        let add = |path: PathBuf, seen: &mut HashSet<PathBuf>, paths: &mut Vec<PathBuf>| {
            if seen.insert(path.clone()) {
                paths.push(path);
            }
        };

        let mut global_files = vec![self.paths.config.join("AGENTS.md")];
        if !self.flags.disable_claude_code_prompt {
            global_files.push(self.paths.home.join(".claude").join("CLAUDE.md"));
        }
        for file in global_files {
            if exists(&file) {
                add(lexical_absolute(&file), &mut seen, &mut paths);
                break;
            }
        }

        // The first project-level match wins so we don't stack
        // AGENTS.md/CLAUDE.md from every ancestor.
        if !self.flags.disable_project_config {
            for file in self.instruction_files() {
                let matches = find_up(file, &self.paths.directory, Some(&self.paths.worktree));
                if !matches.is_empty() {
                    for m in matches {
                        add(m, &mut seen, &mut paths);
                    }
                    break;
                }
            }
        }

        for raw in &self.config_instructions {
            if raw.starts_with("https://") || raw.starts_with("http://") {
                continue;
            }
            let instruction = match raw.strip_prefix("~/") {
                Some(rest) => self.paths.home.join(rest),
                None => PathBuf::from(raw),
            };
            let matches = if instruction.is_absolute() {
                glob_files(&dirname(&instruction), &basename(&instruction))
            } else {
                self.relative(&instruction.to_string_lossy())
            };
            for m in matches {
                add(lexical_absolute(&m), &mut seen, &mut paths);
            }
        }

        paths
    }

    /// `system` (instruction.ts:155-169): `Instructions from: {path}
    // {content}` blocks.
    pub fn system(&self) -> Vec<String> {
        let paths = self.system_paths();
        let urls: Vec<&String> = self
            .config_instructions
            .iter()
            .filter(|item| item.starts_with("https://") || item.starts_with("http://"))
            .collect();
        let mut out = Vec::new();
        for (item, file) in paths.iter().map(|p| (p, self.read(p))) {
            if !file.is_empty() {
                out.push(format!("Instructions from: {}\n{file}", item.display()));
            }
        }
        for url in urls {
            let body = self.remote.fetch(url);
            if !body.is_empty() {
                out.push(format!("Instructions from: {url}\n{body}"));
            }
        }
        out
    }

    /// `find` (instruction.ts:171-177): the first instruction file that
    /// exists in `dir`.
    pub fn find(&self, dir: &Path) -> Option<PathBuf> {
        for file in self.instruction_files() {
            let filepath = lexical_absolute(&dir.join(file));
            if exists(&filepath) {
                return Some(filepath);
            }
        }
        None
    }

    /// `resolve` (instruction.ts:179-221): walk upward from the file being
    /// read and attach nearby instruction files once per message.
    pub fn resolve(
        &self,
        messages: &[WithParts],
        filepath: &Path,
        message_id: &str,
    ) -> Vec<ResolvedInstruction> {
        let sys: HashSet<PathBuf> = self.system_paths().into_iter().collect();
        let already = extract(messages);
        let mut results = Vec::new();
        let root = lexical_absolute(&self.paths.directory);

        let target = lexical_absolute(filepath);
        let mut current = dirname(&target);

        let mut claims = self.claims.lock().unwrap_or_else(|p| p.into_inner());
        while current.starts_with(&root) && current != root {
            let Some(found) = self.find(&current) else {
                current = dirname(&current);
                continue;
            };
            if found == target
                || sys.contains(&found)
                || already.contains(&found.to_string_lossy().into_owned())
            {
                current = dirname(&current);
                continue;
            }
            let set = claims.entry(message_id.to_string()).or_default();
            if set.contains(&found.to_string_lossy().into_owned()) {
                current = dirname(&current);
                continue;
            }
            set.insert(found.to_string_lossy().to_string());
            let content = self.read(&found);
            if !content.is_empty() {
                results.push(ResolvedInstruction {
                    content: format!("Instructions from: {}\n{content}", found.display()),
                    filepath: found,
                });
            }
            current = dirname(&current);
        }
        results
    }
}

/// `path.basename` — the final component.
fn basename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_support::TempDir;
    use opencode_schema::schema::JsonMap;
    use opencode_schema::session_v1::{ToolStateCompletedTime, UserTime, V1UserModel};

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn user_with_read(loaded: Option<Vec<String>>) -> WithParts {
        let metadata: Option<JsonMap> = loaded.map(|paths| {
            serde_json::from_value(serde_json::json!({"loaded": paths, "other": true})).unwrap()
        });
        let time = ToolStateCompletedTime {
            start: 1,
            end: 2,
            compacted: None,
        };
        let state = V1ToolState::Completed {
            input: serde_json::Map::new(),
            output: String::new(),
            title: String::new(),
            metadata: metadata.unwrap_or_default(),
            time,
            attachments: None,
        };
        let part = V1Part::Tool {
            id: "prt_1".to_string(),
            session_id: "ses_1".to_string(),
            message_id: "msg_u".to_string(),
            call_id: "call_1".to_string(),
            tool: "read".to_string(),
            state,
            metadata: None,
        };
        WithParts {
            info: opencode_schema::session_v1::V1Message::User {
                id: "msg_u".to_string(),
                session_id: "ses_1".to_string(),
                time: UserTime { created: 1.0 },
                format: None,
                summary: None,
                agent: "build".to_string(),
                model: V1UserModel {
                    provider_id: "anthropic".to_string(),
                    model_id: "claude".to_string(),
                    variant: None,
                },
                system: None,
                tools: None,
            },
            parts: vec![part],
        }
    }

    #[test]
    fn extract_collects_loaded_paths() {
        let messages = vec![user_with_read(Some(vec![
            "/a/b.md".to_string(),
            "/c/d.md".to_string(),
        ]))];
        let paths = extract(&messages);
        assert!(paths.contains("/a/b.md"));
        assert!(paths.contains("/c/d.md"));
    }

    #[test]
    fn system_paths_global_then_project() {
        let temp = TempDir::new("instruction-sys");
        let dir = temp.path();
        let home = dir.join("home");
        let worktree = dir.join("repo");
        let nested = worktree.join("packages/inner");
        write(&worktree.join("AGENTS.md"), "repo instructions");
        write(&home.join(".config/opencode/AGENTS.md"), "global");

        let instruction = Instruction::new(
            InstructionFlags::default(),
            InstructionPaths {
                config: home.join(".config/opencode"),
                home: home.clone(),
                directory: nested,
                worktree: worktree.clone(),
            },
            Vec::new(),
            Arc::new(NoRemoteInstructions),
        );
        let paths = instruction.system_paths();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], home.join(".config/opencode/AGENTS.md"));
        assert_eq!(paths[1], worktree.join("AGENTS.md"));

        // The remote instruction list is empty without config entries.
        assert_eq!(
            instruction.system(),
            vec![
                format!(
                    "Instructions from: {}\nglobal",
                    home.join(".config/opencode/AGENTS.md").display()
                ),
                format!(
                    "Instructions from: {}\nrepo instructions",
                    worktree.join("AGENTS.md").display()
                ),
            ]
        );
    }

    #[test]
    fn system_paths_first_project_match_wins() {
        let temp = TempDir::new("instruction-first");
        let dir = temp.path();
        let worktree = dir.join("repo");
        write(&worktree.join("CLAUDE.md"), "claude");
        write(&worktree.join("AGENTS.md"), "agents");

        let instruction = Instruction::new(
            InstructionFlags::default(),
            InstructionPaths {
                config: dir.join("nope"),
                home: dir.join("home"),
                directory: worktree.clone(),
                worktree: worktree.clone(),
            },
            Vec::new(),
            Arc::new(NoRemoteInstructions),
        );
        // AGENTS.md is checked before CLAUDE.md.
        let paths = instruction.system_paths();
        assert_eq!(paths, vec![worktree.join("AGENTS.md")]);
    }

    #[test]
    fn find_up_collects_all_matches() {
        let temp = TempDir::new("instruction-findup");
        let dir = temp.path();
        write(&dir.join("a/b/AGENTS.md"), "x");
        write(&dir.join("a/AGENTS.md"), "y");
        let matches = find_up("AGENTS.md", &dir.join("a/b"), Some(dir));
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn resolve_walks_up_and_claims_once() {
        let temp = TempDir::new("instruction-resolve");
        let dir = temp.path();
        let worktree = dir.join("repo");
        let nested = worktree.join("packages/inner");
        write(&nested.join("AGENTS.md"), "inner instructions");
        write(
            &worktree.join("packages/AGENTS.md"),
            "packages instructions",
        );
        let instruction = Instruction::new(
            InstructionFlags::default(),
            InstructionPaths {
                config: dir.join("cfg"),
                home: dir.join("home"),
                directory: worktree.clone(),
                worktree: worktree.clone(),
            },
            Vec::new(),
            Arc::new(NoRemoteInstructions),
        );
        let target = nested.join("src/main.rs");
        write(&target, "fn main() {}");

        let first = instruction.resolve(&[], &target, "msg_1");
        // Walk: nested dir (inner AGENTS.md), then packages (AGENTS.md) —
        // the loop stops at the root (instruction.ts:207-209) so the
        // worktree itself is never inspected.
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].filepath, nested.join("AGENTS.md"));
        assert_eq!(
            first[0].content,
            format!(
                "Instructions from: {}\ninner instructions",
                nested.join("AGENTS.md").display()
            )
        );
        assert_eq!(first[1].filepath, worktree.join("packages/AGENTS.md"));

        // Second resolve on the same message claims nothing new.
        let second = instruction.resolve(&[], &target, "msg_1");
        assert!(second.is_empty());

        // A different message id resolves again.
        let third = instruction.resolve(&[], &target, "msg_2");
        assert_eq!(third.len(), 2);

        // clear() resets the claims of the message.
        instruction.clear("msg_2");
        let fourth = instruction.resolve(&[], &target, "msg_2");
        assert_eq!(fourth.len(), 2);
    }

    #[test]
    fn resolve_stops_at_root_and_skips_system_paths() {
        let temp = TempDir::new("instruction-root");
        let dir = temp.path();
        let worktree = dir.join("repo");
        write(&worktree.join("AGENTS.md"), "root-level");
        let instruction = Instruction::new(
            InstructionFlags::default(),
            InstructionPaths {
                config: dir.join("cfg"),
                home: dir.join("home"),
                directory: worktree.clone(),
                worktree: worktree.clone(),
            },
            Vec::new(),
            Arc::new(NoRemoteInstructions),
        );
        // The walk starts at dirname(target) and stops before the root —
        // the worktree AGENTS.md is a *system* path anyway.
        let resolved = instruction.resolve(&[], &worktree.join("src/main.rs"), "msg_1");
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_skips_already_loaded() {
        let temp = TempDir::new("instruction-loaded");
        let dir = temp.path();
        let worktree = dir.join("repo");
        let nested = worktree.join("packages/inner");
        write(&nested.join("AGENTS.md"), "inner");
        let instruction = Instruction::new(
            InstructionFlags::default(),
            InstructionPaths {
                config: dir.join("cfg"),
                home: dir.join("home"),
                directory: nested.clone(),
                worktree: worktree.clone(),
            },
            Vec::new(),
            Arc::new(NoRemoteInstructions),
        );
        let messages = vec![user_with_read(Some(vec![nested
            .join("AGENTS.md")
            .to_string_lossy()
            .to_string()]))];
        let resolved = instruction.resolve(&messages, &nested.join("src/a.rs"), "msg_1");
        assert!(resolved.is_empty());
    }

    #[test]
    fn config_instructions_glob() {
        let temp = TempDir::new("instruction-config");
        let dir = temp.path();
        let worktree = dir.join("repo");
        write(&dir.join("home/extra/notes.md"), "notes");
        let instruction = Instruction::new(
            InstructionFlags::default(),
            InstructionPaths {
                config: dir.join("cfg"),
                home: dir.join("home"),
                directory: worktree.clone(),
                worktree: worktree.clone(),
            },
            vec![
                "~/extra/*.md".to_string(),
                "https://example.com/instructions.md".to_string(),
            ],
            Arc::new(NoRemoteInstructions),
        );
        let paths = instruction.system_paths();
        assert_eq!(paths, vec![dir.join("home/extra/notes.md")]);
        // https:// entries need remote fetch (the no-op fetcher yields "").
        assert_eq!(instruction.system().len(), 1);
    }

    #[test]
    fn remote_instructions_are_fetched() {
        struct Fixed;
        impl RemoteInstructions for Fixed {
            fn fetch(&self, _url: &str) -> String {
                "remote body".to_string()
            }
        }
        let temp = TempDir::new("instruction-remote");
        let dir = temp.path();
        let instruction = Instruction::new(
            InstructionFlags::default(),
            InstructionPaths {
                config: dir.join("cfg"),
                home: dir.join("home"),
                directory: dir.join("repo"),
                worktree: dir.join("repo"),
            },
            vec!["https://example.com/i.md".to_string()],
            Arc::new(Fixed),
        );
        assert_eq!(
            instruction.system(),
            vec!["Instructions from: https://example.com/i.md\nremote body".to_string()]
        );
    }

    #[test]
    fn project_config_flag_disables_project_lookup() {
        let temp = TempDir::new("instruction-flag");
        let dir = temp.path();
        let worktree = dir.join("repo");
        write(&worktree.join("AGENTS.md"), "x");
        let instruction = Instruction::new(
            InstructionFlags {
                disable_claude_code_prompt: false,
                disable_project_config: true,
            },
            InstructionPaths {
                config: dir.join("cfg"),
                home: dir.join("home"),
                directory: worktree.clone(),
                worktree: worktree.clone(),
            },
            Vec::new(),
            Arc::new(NoRemoteInstructions),
        );
        assert!(instruction.system_paths().is_empty());
    }
}
