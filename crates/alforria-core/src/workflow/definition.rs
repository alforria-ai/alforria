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
use serde_json::Value;
use walkdir::WalkDir;

use super::WorkflowSettings;

const MAX_SCRIPT_BYTES: u64 = 1024 * 1024;
const META_MEMORY_LIMIT: usize = 16 * 1024 * 1024;

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

/// Evaluate a `meta` object literal in a throwaway context.
fn meta_object(literal: &str) -> Option<serde_json::Map<String, Value>> {
    let runtime = rquickjs::Runtime::new().ok()?;
    runtime.set_memory_limit(META_MEMORY_LIMIT);
    let started = std::time::Instant::now();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        started.elapsed() > std::time::Duration::from_millis(100)
    })));
    let context = rquickjs::Context::full(&runtime).ok()?;
    let json: String = context
        .with(|ctx| ctx.eval(format!("JSON.stringify(({literal}))")))
        .ok()?;
    match serde_json::from_str(&json).ok()? {
        Value::Object(fields) => Some(fields),
        _ => None,
    }
}

/// Parse the `export const meta = { ... }` object without running the body.
pub fn parse_meta(script: &str) -> WorkflowMeta {
    let Some((start, end)) = meta_range(script) else {
        return WorkflowMeta::default();
    };
    let Some(fields) = meta_object(&script[start..end]) else {
        return WorkflowMeta::default();
    };
    let Ok(meta) = serde_json::from_value::<Meta>(Value::Object(fields)) else {
        return WorkflowMeta::default();
    };
    WorkflowMeta {
        name: meta.name.filter(|name| valid_name(name)),
        description: meta.description,
        phases: meta.phases.unwrap_or_default(),
    }
}

/// The object literal of a `const meta = { ... }` declaration: balanced braces,
/// skipping strings and comments. `const metadata` and a `meta` initialized by
/// anything but a literal do not count.
fn meta_range(script: &str) -> Option<(usize, usize)> {
    let open = script
        .match_indices("const meta")
        .find_map(|(index, found)| {
            let rest = &script[index + found.len()..];
            let value = rest.trim_start().strip_prefix('=')?.trim_start();
            value.starts_with('{').then(|| script.len() - value.len())
        })?;
    let bytes = script.as_bytes();
    let mut depth = 0usize;
    let mut index = open;
    while index < bytes.len() {
        match bytes[index] {
            quote @ (b'\'' | b'"' | b'`') => {
                index += 1;
                while *bytes.get(index)? != quote {
                    index += if bytes[index] == b'\\' { 2 } else { 1 };
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += script[index..].find('\n')?;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += script[index..].find("*/")? + 1;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((open, index + 1));
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Set the `meta` name so a saved script always matches the name the UI asked
/// for, leaving the rest of the script and the other fields alone. `None`
/// keeps the script's description and an empty one removes it. A `meta` that
/// is not a plain literal is refused rather than rewritten blind.
pub fn rewrite_meta(script: &str, name: &str, description: Option<&str>) -> Result<String, String> {
    let range = meta_range(script);
    let mut fields = match range {
        Some((start, end)) => meta_object(&script[start..end])
            .ok_or_else(|| "the script's meta must be a plain object literal".to_string())?,
        None => serde_json::Map::new(),
    };
    fields.insert("name".to_string(), Value::String(name.to_string()));
    match description {
        Some("") => {
            fields.remove("description");
        }
        Some(description) => {
            fields.insert(
                "description".to_string(),
                Value::String(description.to_string()),
            );
        }
        None => {}
    }
    let block =
        serde_json::to_string_pretty(&Value::Object(fields)).map_err(|err| err.to_string())?;
    Ok(match range {
        Some((start, end)) => format!("{}{}{}", &script[..start], block, &script[end..]),
        None => format!("export const meta = {block}\n\n{script}"),
    })
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
    // A non-git instance has `/` as its worktree, which holds everything.
    let in_project = |dir: &Path| {
        dir.starts_with(&settings.directory)
            || (settings.worktree.parent().is_some() && dir.starts_with(&settings.worktree))
    };
    let (project_dirs, global_dirs): (Vec<&PathBuf>, Vec<&PathBuf>) =
        settings.config_dirs.iter().partition(|dir| in_project(dir));

    if settings.claude_compat {
        push(
            settings.home.join(".claude").join("workflows"),
            WorkflowSource::External,
        );
    }
    push(settings.config.join("workflows"), WorkflowSource::Global);
    for dir in global_dirs {
        push(dir.join("workflows"), WorkflowSource::Global);
        push(dir.join("workflow"), WorkflowSource::Global);
    }

    if settings.project_config {
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
        for dir in project_dirs {
            push(dir.join("workflows"), WorkflowSource::Project);
            push(dir.join("workflow"), WorkflowSource::Project);
        }
    }

    for path in &settings.paths {
        let expanded = expand_path(path, &settings.directory, &settings.home);
        if in_project(&expanded) {
            if settings.project_config {
                push(expanded, WorkflowSource::Project);
            }
        } else {
            push(expanded, WorkflowSource::Global);
        }
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
        let script = rewrite_meta(script, "new-name", Some("new description")).unwrap();
        let meta = parse_meta(&script);
        assert_eq!(meta.name.as_deref(), Some("new-name"));
        assert_eq!(meta.description.as_deref(), Some("new description"));
        assert!(script.contains("return 1"));
    }

    #[test]
    fn rewrite_meta_leaves_lookalikes_and_comments_alone() {
        let script = "const metadata = { a: 1 }\nreturn metadata\n";
        let saved = rewrite_meta(script, "x", None).unwrap();
        assert!(saved.starts_with("export const meta = "), "{saved}");
        assert!(saved.ends_with(script), "{saved}");
        assert_eq!(parse_meta(&saved).name.as_deref(), Some("x"));

        let script = "export const meta = {\n  // don't lose this } brace\n  /* nor } this */\n  name: 'old',\n}\nreturn 1\n";
        let saved = rewrite_meta(script, "x", None).unwrap();
        assert_eq!(saved.matches("const meta").count(), 1, "{saved}");
        assert!(saved.ends_with("\nreturn 1\n"), "{saved}");
        assert_eq!(parse_meta(&saved).name.as_deref(), Some("x"));

        let script = "const meta = build()\nconst other = { a: 1 }\n";
        let saved = rewrite_meta(script, "x", None).unwrap();
        assert!(saved.ends_with(script), "{saved}");
    }

    #[test]
    fn rewrite_meta_keeps_other_fields_and_the_description_unless_told() {
        let script =
            "export const meta = { name: 'a', description: 'keep', phases: ['P'], tag: 1 }\nreturn 1";
        let saved = rewrite_meta(script, "b", None).unwrap();
        let meta = parse_meta(&saved);
        assert_eq!(meta.description.as_deref(), Some("keep"));
        assert_eq!(meta.phases, vec!["P"]);
        assert!(saved.contains("\"tag\": 1"), "{saved}");

        let cleared = rewrite_meta(script, "b", Some("")).unwrap();
        assert_eq!(parse_meta(&cleared).description, None);
    }

    #[test]
    fn rewrite_meta_refuses_a_meta_it_cannot_read() {
        let script = "export const meta = { name: compute() }\nreturn 1";
        assert!(rewrite_meta(script, "x", None).is_err());
    }

    fn settings_in(root: &Path) -> WorkflowSettings {
        WorkflowSettings {
            directory: root.join("home/repo"),
            worktree: root.join("home/repo"),
            config: root.join("config"),
            home: root.join("home"),
            config_dirs: vec![
                root.join("home/repo/.opencode"),
                root.join("home/.opencode"),
            ],
            paths: vec![],
            state: root.join("state"),
            enabled: true,
            max_concurrent: 2,
            max_agents: 10,
            claude_compat: true,
            project_config: true,
        }
    }

    #[test]
    fn project_dirs_under_home_are_project_and_outrank_the_rest() {
        let root = PathBuf::from("/r");
        let dirs = scan_dirs(&settings_in(&root));
        let at = |dir: &str| {
            dirs.iter()
                .position(|(path, _)| path == &root.join(dir))
                .unwrap()
        };
        let project = root.join("home/repo/.opencode/workflows");
        assert_eq!(
            dirs.iter()
                .find(|(path, _)| path == &project)
                .map(|(_, source)| *source),
            Some(WorkflowSource::Project)
        );
        assert!(at("home/.opencode/workflows") < at("home/repo/.claude/workflows"));
        assert!(at("home/repo/.claude/workflows") < at("home/repo/.opencode/workflows"));
    }

    #[test]
    fn disabling_project_config_skips_project_dirs() {
        let root = PathBuf::from("/r");
        let settings = WorkflowSettings {
            project_config: false,
            paths: vec!["local".to_string()],
            ..settings_in(&root)
        };
        let dirs = scan_dirs(&settings);
        assert!(
            dirs.iter()
                .all(|(path, _)| !path.starts_with(root.join("home/repo"))),
            "{dirs:?}"
        );
        assert!(dirs
            .iter()
            .any(|(path, _)| path == &root.join("config/workflows")));
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
            project_config: true,
        };
        let list = discover(&settings);
        let workflow = list.iter().find(|w| w.name == "deep-research").unwrap();
        assert_eq!(workflow.description.as_deref(), Some("mine"));
        assert_eq!(workflow.source, WorkflowSource::Project);
        std::fs::remove_dir_all(temp.path()).ok();
    }
}
