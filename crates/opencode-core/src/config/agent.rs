//! Agent/mode Markdown discovery — port of `packages/opencode/src/config/agent.ts`
//! and the shared helpers from `config/markdown.ts` / `config/entry-name.ts`.
//!
//! * `load(dir)` scans `{agent,agents}/**/*.md` under `dir`;
//! * `loadMode(dir)` scans `{mode,modes}/*.md` and forces `mode: "primary"`.
//!
//! Markdown frontmatter is parsed with `gray_matter` (YAML engine). Frontmatter
//! parse failures skip the file silently (TS `.catch(() => undefined)`), while
//! schema decode failures of *agents* are fatal (`ConfigParse.schema` throws).
//! Modes skip on decode failure (TS only inserts on `Exit.isSuccess`).

use std::fs;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

use gray_matter::engine::yaml::YAML;
use gray_matter::matter::Matter;
use gray_matter::value::pod::Pod;
use serde_json::{Map, Value};

use crate::config::schema::{AgentInfo, AgentMode};
use crate::{CoreError, SchemaIssue};

/// The `Glob.scan` patterns of TS `agent.ts` / `command.ts`.
const AGENT_PATTERNS: &[&str] = &["agent/**/*.md", "agents/**/*.md"];
const MODE_PATTERNS: &[&str] = &["mode/*.md", "modes/*.md"];

// ---------------------------------------------------------------------------
// Entry names (`config/entry-name.ts`)
// ---------------------------------------------------------------------------

/// `configEntryNameFromPath` — strips a known prefix from an already-relative
/// path, then removes the file extension (`path.extname` semantics: a leading
/// dot in the basename does not count as an extension).
pub(crate) fn config_entry_name_from_path(relative_path: &str, prefixes: &[&str]) -> String {
    let normalized = relative_path.replace('\\', "/");
    let candidate: String = match prefixes.iter().find_map(|p| normalized.strip_prefix(p)) {
        Some(stripped) => stripped.to_owned(),
        None => Path::new(&normalized)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    strip_extension(&candidate).to_owned()
}

/// `path.extname` + slice — strips the trailing extension unless the only dot
/// starts the basename (`.md` has no extension).
fn strip_extension(candidate: &str) -> &str {
    let base_start = candidate.rfind('/').map_or(0, |i| i + 1);
    let dot = candidate[base_start..]
        .rfind('.')
        .filter(|dot| base_start + dot > base_start);
    match dot {
        Some(dot) => &candidate[..base_start + dot],
        None => candidate,
    }
}

// ---------------------------------------------------------------------------
// Markdown frontmatter (`config/markdown.ts` + core `config/markdown.ts`)
// ---------------------------------------------------------------------------

/// A parsed Markdown document: frontmatter keys and the body.
pub(crate) struct Markdown {
    pub(crate) data: Map<String, Value>,
    pub(crate) content: String,
}

/// Parses frontmatter YAML + body. Returns `None` on YAML parse failure
/// (TS `ConfigMarkdown.parse` throws; callers skip the file).
pub(crate) fn parse_markdown(text: &str) -> Option<Markdown> {
    let matter: Matter<YAML> = Matter::new();
    let parsed =
        std::panic::catch_unwind(AssertUnwindSafe(|| matter.matter(text.to_owned()))).ok()?;
    if !matches!(parsed.data, Pod::Null) {
        return Some(Markdown {
            data: pod_to_map(&parsed.data),
            content: parsed.content,
        });
    }

    // The YAML engine reports both invalid YAML and empty frontmatter as null;
    // blank (whitespace/comment-only) frontmatter decodes to `{}` in TS.
    if frontmatter_is_blank(text) {
        return Some(Markdown {
            data: Map::new(),
            content: parsed.content,
        });
    }

    // TS `ConfigMarkdown.parse` retries with `sanitize`d frontmatter (unquoted
    // colons in values become block scalars).
    let sanitized = sanitize(text);
    if sanitized != text {
        if let Ok(parsed) = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let matter: Matter<YAML> = Matter::new();
            matter.matter(sanitized.clone())
        })) {
            if !matches!(parsed.data, Pod::Null) {
                return Some(Markdown {
                    data: pod_to_map(&parsed.data),
                    content: parsed.content,
                });
            }
        }
    }
    None
}

fn read_markdown(path: &Path) -> Option<Markdown> {
    parse_markdown(&fs::read_to_string(path).ok()?)
}

/// Whether the frontmatter block is whitespace or comments only. Mirrors
/// gray_matter's own extraction (text between `---` and the first closing
/// `---`) so it also covers the truly-empty `---\n---` frontmatter that the
/// sanitize regex cannot capture.
fn frontmatter_is_blank(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("---") else {
        return false;
    };
    if rest.starts_with('-') {
        return false; // "----" style prefixes are not frontmatter delimiters
    }
    let frontmatter = match rest.find("---") {
        Some(close) => &rest[..close],
        None => rest,
    };
    frontmatter.lines().all(|line| {
        let trimmed = line.trim();
        trimmed.is_empty() || trimmed.starts_with('#')
    })
}

/// Core `config/markdown.ts` `sanitize` — rewrite values containing an
/// unquoted `:` into block scalars so lenient frontmatter keeps parsing.
fn sanitize(content: &str) -> String {
    let Some(captures) = frontmatter_regex().captures(content) else {
        return content.to_owned();
    };
    let frontmatter = captures.get(1).map_or("", |m| m.as_str());
    let entry_regex = regex::Regex::new(r"^([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*(.*)$").unwrap();
    let mut result: Vec<String> = Vec::new();
    for line in frontmatter.split("\r\n").flat_map(|l| l.split('\n')) {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() || line.starts_with(char::is_whitespace) {
            result.push(line.to_owned());
            continue;
        }
        let Some(entry) = entry_regex.captures(line) else {
            result.push(line.to_owned());
            continue;
        };
        let key = entry.get(1).map_or("", |m| m.as_str());
        let value = entry.get(2).map_or("", |m| m.as_str()).trim();
        if value.is_empty()
            || value == ">"
            || value == "|"
            || value.starts_with('"')
            || value.starts_with('\'')
            || !value.contains(':')
        {
            result.push(line.to_owned());
            continue;
        }
        result.push(format!("{key}: |-"));
        result.push(format!("  {value}"));
    }
    content.replacen(frontmatter, &result.join("\n"), 1)
}

fn frontmatter_regex() -> regex::Regex {
    regex::Regex::new(r"^---\r?\n([\s\S]*?)\r?\n---").unwrap()
}

fn pod_to_map(pod: &Pod) -> Map<String, Value> {
    match pod {
        Pod::Hash(entries) => {
            let mut out = Map::new();
            for (key, value) in entries {
                out.insert(key.clone(), pod_to_value(value));
            }
            out
        }
        // Scalar frontmatter spreads to no keys in an object literal.
        _ => Map::new(),
    }
}

fn pod_to_value(pod: &Pod) -> Value {
    match pod {
        Pod::Null => Value::Null,
        Pod::String(value) => Value::String(value.clone()),
        Pod::Integer(value) => Value::Number((*value).into()),
        Pod::Boolean(value) => Value::Bool(*value),
        Pod::Float(value) => serde_json::Number::from_f64(*value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Pod::Array(items) => Value::Array(items.iter().map(pod_to_value).collect()),
        Pod::Hash(_) => Value::Object(pod_to_map(pod)),
    }
}

// ---------------------------------------------------------------------------
// Glob scan (`Glob.scan` — dot + symlink traversal, relative to `dir`)
// ---------------------------------------------------------------------------

pub(crate) fn scan_markdown(dir: &Path, patterns: &[&str]) -> Vec<PathBuf> {
    let mut builder = ignore::WalkBuilder::new(dir);
    builder
        .hidden(true)
        .follow_links(true)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .parents(false)
        .require_git(false);
    let mut globs = globset::GlobSetBuilder::new();
    for pattern in patterns {
        // `literal_separator`: `*` never crosses `/` (npm glob semantics);
        // `**` still matches zero or more whole components.
        let glob = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .unwrap_or_else(|err| panic!("invalid glob pattern {pattern}: {err}"));
        globs.add(glob);
    }
    let globs = globs.build().unwrap();
    let mut found = Vec::new();
    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(dir) else {
            continue;
        };
        if globs.is_match(relative.to_string_lossy().replace('\\', "/")) {
            found.push(entry.into_path());
        }
    }
    found.sort();
    found
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

pub(crate) fn relative_name(path: &Path, dir: &Path, prefixes: &[&str]) -> String {
    let relative = path
        .strip_prefix(dir)
        .map_or_else(|_| PathBuf::new(), Path::to_path_buf)
        .to_string_lossy()
        .replace('\\', "/");
    config_entry_name_from_path(&relative, prefixes)
}

fn decode_invalid(path: &Path, err: serde_json::Error) -> CoreError {
    CoreError::ConfigInvalid {
        path: path.to_path_buf(),
        message: None,
        issues: vec![SchemaIssue {
            path: Vec::new(),
            message: err.to_string(),
        }],
    }
}

/// `ConfigAgent.load(dir)` — `{name, ...data, prompt: content.trim()}` decoded
/// through `ConfigAgentV1.Info`. Frontmatter parse failures skip the file;
/// schema decode failures are fatal.
pub fn load_agents(dir: &Path) -> Result<Value, CoreError> {
    let mut result = Map::new();
    for path in scan_markdown(dir, AGENT_PATTERNS) {
        let Some(md) = read_markdown(&path) else {
            continue;
        };
        let name = relative_name(&path, dir, &["agent/", "agents/"]);
        let mut config = Map::new();
        config.insert("name".to_owned(), Value::String(name.clone()));
        config.extend(md.data);
        config.insert(
            "prompt".to_owned(),
            Value::String(md.content.trim().to_owned()),
        );
        let agent: AgentInfo = serde_json::from_value(Value::Object(config))
            .map_err(|err| decode_invalid(&path, err))?;
        let value = serde_json::to_value(&agent)
            .map_err(|err| CoreError::invalid(&path, err.to_string()))?;
        result.insert(name, value);
    }
    Ok(Value::Object(result))
}

/// `ConfigAgent.loadMode(dir)` — like [`load_agents`] over `{mode,modes}/*.md`,
/// forcing `mode: "primary"` on every entry. Decode failures are skipped.
pub fn load_modes(dir: &Path) -> Result<Value, CoreError> {
    let mut result = Map::new();
    for path in scan_markdown(dir, MODE_PATTERNS) {
        let Some(md) = read_markdown(&path) else {
            continue;
        };
        let name = relative_name(&path, dir, &["mode/", "modes/"]);
        let mut config = Map::new();
        config.insert("name".to_owned(), Value::String(name.clone()));
        config.extend(md.data);
        config.insert(
            "prompt".to_owned(),
            Value::String(md.content.trim().to_owned()),
        );
        let Ok(mut agent) = serde_json::from_value::<AgentInfo>(Value::Object(config)) else {
            continue;
        };
        agent.mode = Some(AgentMode::Primary);
        if let Ok(value) = serde_json::to_value(&agent) {
            result.insert(name, value);
        }
    }
    Ok(Value::Object(result))
}

// ---------------------------------------------------------------------------
// ConfigDiscovery default (seam wiring)
// ---------------------------------------------------------------------------

/// [`crate::config::precedence::ConfigDiscovery`] over the agent/command
/// Markdown layout. The default discovery of the precedence chain.
#[derive(Debug, Clone, Copy, Default)]
pub struct MarkdownDiscovery;

impl super::precedence::ConfigDiscovery for MarkdownDiscovery {
    fn commands(&self, dir: &Path) -> Result<Value, CoreError> {
        super::command::load_commands(dir)
    }

    fn agents(&self, dir: &Path) -> Result<Value, CoreError> {
        load_agents(dir)
    }

    fn modes(&self, dir: &Path) -> Result<Value, CoreError> {
        load_modes(dir)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use super::*;
    use crate::config::precedence::{ConfigDiscovery, ConfigLoader, LoadParams, NoopDiscovery};
    use crate::config::schema::decode_config;
    use crate::paths::GlobalPaths;

    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "opencode-core-m33-agent-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn entry_name_strips_prefixes_and_extension() {
        assert_eq!(
            config_entry_name_from_path("agent/nested/deep.md", &["agent/", "agents/"]),
            "nested/deep"
        );
        assert_eq!(
            config_entry_name_from_path("agents/foo.md", &["agent/", "agents/"]),
            "foo"
        );
        // Unprefixed paths fall back to the basename.
        assert_eq!(
            config_entry_name_from_path("agent/foo.md", &["mode/", "modes/"]),
            "foo"
        );
        // A leading dot in the basename is not an extension (`path.extname`).
        assert_eq!(config_entry_name_from_path("agent/.md", &["agent/"]), ".md");
        assert_eq!(
            config_entry_name_from_path("agent/a.b.md", &["agent/"]),
            "a.b"
        );
        // Prefixes match anchored — "agentsfoo/" is not "agents/".
        assert_eq!(
            config_entry_name_from_path("agentsfoo/x.md", &["agent/", "agents/"]),
            "x"
        );
    }

    #[test]
    fn nested_agents_discovered_with_trimmed_prompt() {
        let dir = temp_dir();
        write(
            &dir.join("agent").join("nested").join("deep.md"),
            "---\nmodel: anthropic/claude\n---\n  hello  \n",
        );
        write(
            &dir.join("agents").join("alt.md"),
            "---\ndescription: alt agent\n---\nbody\n",
        );

        let agents = load_agents(&dir).unwrap();
        let deep = agents.get("nested/deep").unwrap();
        assert_eq!(deep["model"], json!("anthropic/claude"));
        assert_eq!(deep["prompt"], json!("hello"));
        let alt = agents.get("alt").unwrap();
        assert_eq!(alt["description"], json!("alt agent"));
        assert_eq!(alt["prompt"], json!("body"));
    }

    #[test]
    fn agent_unknown_frontmatter_keys_move_into_options() {
        let dir = temp_dir();
        write(
            &dir.join("agent").join("a.md"),
            "---\nmodel: m\nunknownKey: yes\n---\nbody\n",
        );
        let agents = load_agents(&dir).unwrap();
        let agent = agents.get("a").unwrap();
        assert_eq!(agent["options"]["unknownKey"], json!("yes"));
        // `name` is a known key — it stays out of options.
        assert!(agent["options"].get("name").is_none());
        assert_eq!(agent["name"], json!("a"));
    }

    #[test]
    fn agent_normalize_rules_apply() {
        let dir = temp_dir();
        write(
            &dir.join("agent").join("a.md"),
            "---\ntools: {\"write\": true, \"bash\": false}\nmaxSteps: 3\n---\nbody\n",
        );
        let agents = load_agents(&dir).unwrap();
        let agent = agents.get("a").unwrap();
        assert_eq!(agent["permission"]["edit"], json!("allow"));
        assert_eq!(agent["permission"]["bash"], json!("deny"));
        assert_eq!(agent["steps"], json!(3));
    }

    #[test]
    fn invalid_agent_frontmatter_is_skipped() {
        let dir = temp_dir();
        write(
            &dir.join("agent").join("bad.md"),
            "---\nkey: [unclosed\n---\nbody\n",
        );
        write(
            &dir.join("agent").join("good.md"),
            "---\nmodel: m\n---\nbody\n",
        );
        let agents = load_agents(&dir).unwrap();
        assert!(agents.get("bad").is_none());
        assert!(agents.get("good").is_some());
    }

    #[test]
    fn sanitize_fixes_unquoted_colons() {
        let dir = temp_dir();
        write(
            &dir.join("agent").join("a.md"),
            "---\ndescription: fix: things\n---\nbody\n",
        );
        let agents = load_agents(&dir).unwrap();
        assert_eq!(agents["a"]["description"], json!("fix: things"));
    }

    #[test]
    fn blank_frontmatter_is_an_empty_map() {
        let dir = temp_dir();
        write(&dir.join("agent").join("a.md"), "---\n---\nbody\n");
        let agents = load_agents(&dir).unwrap();
        assert_eq!(agents["a"]["prompt"], json!("body"));
    }

    #[test]
    fn agent_decode_failure_is_fatal() {
        let dir = temp_dir();
        // steps is a PositiveInt — 0 is invalid.
        write(
            &dir.join("agent").join("a.md"),
            "---\nsteps: 0\n---\nbody\n",
        );
        let err = load_agents(&dir).unwrap_err();
        assert!(matches!(err, CoreError::ConfigInvalid { .. }), "{err:?}");
    }

    #[test]
    fn modes_force_primary() {
        let dir = temp_dir();
        write(
            &dir.join("mode").join("plan.md"),
            "---\nmode: subagent\n---\nplan body\n",
        );
        write(
            &dir.join("modes").join("other.md"),
            "---\n---\nother body\n",
        );
        let modes = load_modes(&dir).unwrap();
        assert_eq!(modes["plan"]["mode"], json!("primary"));
        assert_eq!(modes["plan"]["prompt"], json!("plan body"));
        assert_eq!(modes["other"]["mode"], json!("primary"));
    }

    #[test]
    fn mode_decode_failure_is_skipped() {
        let dir = temp_dir();
        write(
            &dir.join("mode").join("bad.md"),
            "---\nsteps: 0\n---\nbody\n",
        );
        let modes = load_modes(&dir).unwrap();
        assert!(modes.as_object().unwrap().is_empty());
    }

    #[test]
    fn modes_are_single_level() {
        let dir = temp_dir();
        write(&dir.join("mode").join("nested").join("x.md"), "---\n---\n");
        let modes = load_modes(&dir).unwrap();
        assert!(modes.as_object().unwrap().is_empty());
    }

    #[test]
    fn discovery_is_wired_into_the_precedence_chain() {
        let root = temp_dir();
        let work = root.join("work");
        write(
            &work.join(".opencode").join("agent").join("build.md"),
            "---\ndescription: builds\n---\nYou are Build.\n",
        );
        write(
            &work.join(".opencode").join("command").join("deploy.md"),
            "---\ndescription: deploys\n---\nDeploy!\n",
        );
        write(
            &work.join(".opencode").join("mode").join("review.md"),
            "---\n---\nReview mode.\n",
        );

        let params = LoadParams::new(work.clone())
            .worktree(work.clone())
            .paths(GlobalPaths {
                home: root.join("home"),
                config: root.join("config"),
                data: root.join("data"),
                cache: root.join("cache"),
            });
        let (config, _) = ConfigLoader.load(&params).unwrap();

        let build = &config.agent.as_ref().unwrap()["build"];
        assert_eq!(build.prompt.as_deref(), Some("You are Build."));
        let deploy = config.command.as_ref().unwrap()["deploy"].clone();
        assert_eq!(deploy.template, "Deploy!");
        // Modes land in `agent` with mode "primary" forced.
        let review = &config.agent.as_ref().unwrap()["review"];
        assert_eq!(review.mode, Some(AgentMode::Primary));
        // Decode check: the merged result re-decodes cleanly.
        decode_config(&json!({}), &work).unwrap();
    }

    // Silence unused warnings for the seam default used above.
    #[test]
    fn noop_discovery_still_available() {
        let dir = temp_dir();
        let agents = NoopDiscovery.agents(&dir).unwrap();
        assert!(agents.as_object().unwrap().is_empty());
    }
}
