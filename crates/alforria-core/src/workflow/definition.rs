//! Workflow discovery and `meta` parsing.
//!
//! Directories are scanned low-to-high precedence, so a project workflow
//! overrides a user workflow with the same name. `.claude/workflows` is
//! read-only compatibility — the runtime still executes QuickJS, so a script
//! written for Claude Code works as-is when it only uses the orchestration
//! globals.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use alforria_schema::workflow::{WorkflowDefinition, WorkflowSource};
use serde::Deserialize;
use walkdir::WalkDir;

use super::WorkflowSettings;

const MAX_SCRIPT_BYTES: u64 = 1024 * 1024;

const BUILTIN_WORKFLOWS: &[(&str, &str)] =
    &[("deep-research", include_str!("builtin/deep-research.js"))];

#[derive(Debug, Clone, Default, Deserialize)]
struct Meta {
    name: Option<String>,
    description: Option<String>,
    phases: Option<Vec<String>>,
}

/// A workflow's `meta` block.
#[derive(Debug, Clone, Default)]
pub struct WorkflowMeta {
    pub name: Option<String>,
    pub description: Option<String>,
    pub phases: Vec<String>,
}

/// Parse the `export const meta = { ... }` object without running the body.
pub fn parse_meta(script: &str) -> WorkflowMeta {
    let Some(literal) = meta_literal(script) else {
        return WorkflowMeta::default();
    };
    let Ok(runtime) = rquickjs::Runtime::new() else {
        return WorkflowMeta::default();
    };
    let started = std::time::Instant::now();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        started.elapsed() > std::time::Duration::from_millis(100)
    })));
    let Ok(context) = rquickjs::Context::full(&runtime) else {
        return WorkflowMeta::default();
    };
    let json: String = match context.with(|ctx| ctx.eval(format!("JSON.stringify(({literal}))"))) {
        Ok(json) => json,
        Err(_) => return WorkflowMeta::default(),
    };
    let Ok(meta) = serde_json::from_str::<Meta>(&json) else {
        return WorkflowMeta::default();
    };
    WorkflowMeta {
        name: meta.name.filter(|name| valid_name(name)),
        description: meta.description,
        phases: meta.phases.unwrap_or_default(),
    }
}

/// `export const meta = {...}` — find the first balanced object literal.
fn meta_literal(script: &str) -> Option<&str> {
    let (start, end) = meta_range(script)?;
    Some(&script[start..end])
}

fn meta_range(script: &str) -> Option<(usize, usize)> {
    let start = script
        .find("export const meta")
        .or_else(|| script.find("const meta"))?;
    let eq = start + script[start..].find('=')?;
    let open = eq + script[eq..].find('{')?;
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (offset, ch) in script[open..].char_indices() {
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' | '`' => quote = Some(ch),
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = open + offset + ch.len_utf8();
                    return Some((open, end));
                }
            }
            _ => {}
        }
    }
    None
}

/// Replace (or add) the `meta` block so a saved script's name always matches
/// the file the UI asked for. Other meta fields are rebuilt from the parsed
/// name/description/phases.
pub fn rewrite_meta(script: &str, name: &str, description: Option<&str>) -> String {
    let meta = parse_meta(script);
    let name = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".to_string());
    let description =
        serde_json::to_string(description.unwrap_or("")).unwrap_or_else(|_| "\"\"".to_string());
    let phases = serde_json::to_string(&meta.phases).unwrap_or_else(|_| "[]".to_string());
    let block = format!("{{ name: {name}, description: {description}, phases: {phases} }}");
    match meta_range(script) {
        Some((start, end)) => format!("{}{}{}", &script[..start], block, &script[end..]),
        None => format!("export const meta = {block}\n\n{script}"),
    }
}

/// A safe workflow name: no separators, no traversal, printable.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 80
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        && !name.starts_with('.')
}

/// Every workflow visible from this instance, sorted by name.
pub fn discover(settings: &WorkflowSettings) -> Vec<WorkflowDefinition> {
    if !settings.enabled {
        return Vec::new();
    }
    let mut found: HashMap<String, WorkflowDefinition> = HashMap::new();
    for (name, script) in BUILTIN_WORKFLOWS {
        let meta = parse_meta(script);
        found.insert(
            (*name).to_string(),
            WorkflowDefinition {
                name: (*name).to_string(),
                description: meta.description,
                path: "builtin".to_string(),
                source: WorkflowSource::Builtin,
                phases: meta.phases,
                script: (*script).to_string(),
            },
        );
    }

    let mut seen = HashSet::new();
    for (dir, source) in scan_dirs(settings) {
        scan_dir(&dir, source, &mut found, &mut seen);
    }
    let mut list: Vec<WorkflowDefinition> = found.into_values().collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    list
}

fn scan_dirs(settings: &WorkflowSettings) -> Vec<(PathBuf, WorkflowSource)> {
    let mut dirs: Vec<(PathBuf, WorkflowSource)> = Vec::new();
    let mut push = |dir: PathBuf, source: WorkflowSource| {
        if !dirs.iter().any(|(existing, _)| existing == &dir) {
            dirs.push((dir, source));
        }
    };

    if settings.claude_compat {
        push(
            settings.home.join(".claude").join("workflows"),
            WorkflowSource::External,
        );
    }
    push(settings.config.join("workflows"), WorkflowSource::Global);
    for dir in &settings.config_dirs {
        let source = if dir.starts_with(&settings.home) {
            WorkflowSource::Global
        } else {
            WorkflowSource::Project
        };
        push(dir.join("workflows"), source);
        push(dir.join("workflow"), source);
    }

    let mut ancestors: Vec<PathBuf> = Vec::new();
    let mut current = Some(settings.directory.as_path());
    while let Some(dir) = current {
        ancestors.push(dir.to_path_buf());
        if dir == settings.worktree {
            break;
        }
        current = dir.parent();
    }
    ancestors.reverse();
    for dir in &ancestors {
        if settings.claude_compat {
            push(
                dir.join(".claude").join("workflows"),
                WorkflowSource::External,
            );
        }
        push(
            dir.join(".opencode").join("workflows"),
            WorkflowSource::Project,
        );
        push(
            dir.join(".opencode").join("workflow"),
            WorkflowSource::Project,
        );
    }

    for path in &settings.paths {
        let expanded = expand_path(path, &settings.directory, &settings.home);
        let source = if expanded.starts_with(&settings.directory)
            || expanded.starts_with(&settings.worktree)
        {
            WorkflowSource::Project
        } else {
            WorkflowSource::Global
        };
        push(expanded, source);
    }
    dirs
}

fn expand_path(path: &str, base: &Path, home: &Path) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn scan_dir(
    dir: &Path,
    source: WorkflowSource,
    found: &mut HashMap<String, WorkflowDefinition>,
    seen: &mut HashSet<PathBuf>,
) {
    if !dir.is_dir() {
        return;
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(dir)
        .follow_links(false)
        .max_depth(6)
        .into_iter()
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| !(name.starts_with('.') && name != "." && name != ".."))
                .unwrap_or(true)
        })
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
        if !matches!(ext, "js" | "mjs") {
            continue;
        }
        if entry.metadata().map(|meta| meta.len()).unwrap_or(0) > MAX_SCRIPT_BYTES {
            continue;
        }
        files.push(path.to_path_buf());
    }
    files.sort();
    for path in files {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Ok(script) = std::fs::read_to_string(&path) else {
            continue;
        };
        let meta = parse_meta(&script);
        let name = meta
            .name
            .clone()
            .or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_string)
            })
            .filter(|name| valid_name(name));
        let Some(name) = name else {
            continue;
        };
        found.insert(
            name.clone(),
            WorkflowDefinition {
                name,
                description: meta.description,
                path: path.to_string_lossy().to_string(),
                source,
                phases: meta.phases,
                script,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_meta_object() {
        let meta = parse_meta(
            r#"
export const meta = {
  name: 'audit-routes',
  description: "Audit {routes}",
  phases: ['Discover', 'Audit'],
}
const x = 1
"#,
        );
        assert_eq!(meta.name.as_deref(), Some("audit-routes"));
        assert_eq!(meta.description.as_deref(), Some("Audit {routes}"));
        assert_eq!(meta.phases, vec!["Discover", "Audit"]);
    }

    #[test]
    fn rewrite_meta_keeps_the_file_and_meta_name_in_sync() {
        let script = "export const meta = { name: 'old', description: 'old' }\nreturn 1\n";
        let script = rewrite_meta(script, "new-name", Some("new description"));
        let meta = parse_meta(&script);
        assert_eq!(meta.name.as_deref(), Some("new-name"));
        assert_eq!(meta.description.as_deref(), Some("new description"));
        assert!(script.contains("return 1"));
    }

    #[test]
    fn rejects_unsafe_names() {
        assert!(valid_name("audit-routes"));
        assert!(!valid_name("../escape"));
        assert!(!valid_name("has space"));
        assert!(!valid_name(".hidden"));
    }

    #[test]
    fn project_workflow_overrides_builtin() {
        let temp = crate::storage::test_support::TempDir::new("workflow-discovery");
        let dir = temp.path().join(".opencode/workflows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("deep-research.js"),
            "export const meta = { name: 'deep-research', description: 'mine' }\nreturn 1\n",
        )
        .unwrap();
        let settings = WorkflowSettings {
            directory: temp.path().to_path_buf(),
            worktree: temp.path().to_path_buf(),
            config: temp.path().join("config"),
            home: temp.path().join("home"),
            config_dirs: vec![],
            paths: vec![],
            state: temp.path().join("state"),
            enabled: true,
            max_concurrent: 2,
            max_agents: 10,
            claude_compat: false,
        };
        let list = discover(&settings);
        let workflow = list.iter().find(|w| w.name == "deep-research").unwrap();
        assert_eq!(workflow.description.as_deref(), Some("mine"));
        assert_eq!(workflow.source, WorkflowSource::Project);
        std::fs::remove_dir_all(temp.path()).ok();
    }
}
