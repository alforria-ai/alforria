//! `skill` tool — port of `tool/skill.ts` + `skill/index.ts` (spec M4.6).
//!
//! [`Skill`] is the `Skill.Service` seam. The M4 default ([`SkillService`])
//! provides what the tool needs and no more (spec STOP S2): discovery of
//! `SKILL.md` files from the `.opencode` config directories
//! (`{skill,skills}/**/SKILL.md`) and the `skills.paths` config entries
//! (`**/SKILL.md`), frontmatter-parsed for `name`/`description`.
//! Skill *activation* / prompt injection, external (`~/.claude`, …) skill
//! dirs and `skills.urls` discovery are M7 scope.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::agent::{parse_markdown, scan_markdown};
use crate::tool::def::{define, Agents, AskRequest, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::ripgrep::{ts_dirname, ts_resolve, Ripgrep};
use crate::tool::truncate::Truncate;

/// `Skill.Info` (skill/index.ts:37-43).
#[derive(Debug, Clone, PartialEq)]
pub struct SkillInfo {
    pub name: String,
    pub description: Option<String>,
    /// Absolute path of the `SKILL.md` file.
    pub location: String,
    pub content: String,
}

/// `Skill.NotFoundError.message` (skill/index.ts:73-80): the tool dies with
/// this message when `require` fails.
pub fn not_found_message(name: &str, available: &[String]) -> String {
    let available = if available.is_empty() {
        "none".to_string()
    } else {
        available.join(", ")
    };
    format!("Skill \"{name}\" not found. Available skills: {available}")
}

/// `Skill.Service` seam (skill/index.ts:97-103) — the slice the tool uses.
pub trait Skill: Send + Sync {
    fn require<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<SkillInfo, ToolError>>;
}

#[derive(Debug, Deserialize)]
pub struct SkillParameters {
    pub name: String,
}

/// Hand-authored JSON Schema (byte-matches the Effect-generated schema; see
/// `tests/golden/toolschema/skill.json`).
pub fn parameters() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "The name of the skill from available_skills"
            }
        },
        "required": [
            "name"
        ]
    })
}

/// Build the `skill` tool.
pub fn skill_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    skill: Arc<dyn Skill>,
    ripgrep: Arc<dyn Ripgrep>,
) -> ToolDef {
    define(
        "skill",
        include_str!("txt/skill.txt"),
        parameters(),
        None,
        truncate,
        agents,
        move |params: SkillParameters, ctx: ToolCtxRef<'_>| {
            run(params, ctx, Arc::clone(&skill), Arc::clone(&ripgrep))
        },
    )
}

fn run(
    params: SkillParameters,
    ctx: ToolCtxRef<'_>,
    skill: Arc<dyn Skill>,
    ripgrep: Arc<dyn Ripgrep>,
) -> BoxFuture<'_, Result<ExecuteResult, ToolError>> {
    Box::pin(async move {
        let info = skill.require(&params.name).await?;

        ctx.ask
            .ask(AskRequest {
                permission: "skill".to_string(),
                patterns: vec![params.name.clone()],
                always: vec![params.name.clone()],
                metadata: json!({}),
            })
            .await?;

        let dir = ts_dirname(Path::new(&info.location));
        let files = ripgrep.find(&dir, "!**/SKILL.md", true, false, 10);
        let file_tags = files
            .iter()
            .map(|file| {
                format!(
                    "<file>{}</file>",
                    ts_resolve(&dir, &file.to_string_lossy()).display()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let output = [
            format!("<skill_content name=\"{}\">", info.name),
            format!("# Skill: {}", info.name),
            String::new(),
            info.content.trim().to_string(),
            String::new(),
            format!("Base directory for this skill: {}", dir.display()),
            "Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.".to_string(),
            "Note: file list is sampled.".to_string(),
            String::new(),
            "<skill_files>".to_string(),
            file_tags,
            "</skill_files>".to_string(),
            "</skill_content>".to_string(),
        ]
        .join("\n");

        Ok(ExecuteResult {
            title: format!("Loaded skill: {}", info.name),
            metadata: json!({
                "name": info.name,
                "dir": dir.display().to_string(),
            }),
            output,
            attachments: None,
        })
    })
}

// ---------------------------------------------------------------------------
// Filesystem discovery (the M4 slice of `Skill.Service`)
// ---------------------------------------------------------------------------

/// `OPENCODE_SKILL_PATTERN` / `SKILL_PATTERN` (skill/index.ts:23-25) — the
/// scan patterns for the two discovery sources M4 supports.
const OPENCODE_SKILL_PATTERNS: &[&str] = &["{skill,skills}/**/SKILL.md"];
const SKILL_PATTERNS: &[&str] = &["**/SKILL.md"];
/// `EXTERNAL_SKILL_PATTERN` (skill/index.ts:23) — the external
/// (`~/.claude`, `~/.agents`) skills live under `skills/**/SKILL.md`.
const EXTERNAL_SKILL_PATTERNS: &[&str] = &["skills/**/SKILL.md"];

/// Inputs to [`SkillService::discover`] — the two M4 discovery sources of
/// `discoverSkills` (skill/index.ts:173-233).
#[derive(Debug, Clone, Default)]
pub struct SkillDiscovery {
    /// Config directories scanned with `{skill,skills}/**/SKILL.md`
    /// (TS `config.directories()` — e.g. `<worktree>/.opencode` and the
    /// global config directory).
    pub directories: Vec<PathBuf>,
    /// `skills.paths` config entries (TS `cfg.skills?.paths`).
    pub paths: Vec<String>,
    /// The instance directory — relative `skills.paths` entries resolve
    /// against it.
    pub directory: PathBuf,
    /// The user's home directory (`~/` expansion).
    pub home: PathBuf,
    /// The worktree root — the up-tree walk for `.claude`/`.agents`
    /// stops here (`fsys.up({ stop: worktree })`).
    pub worktree: PathBuf,
}

/// The filesystem-backed `Skill.Service` default: skills discovered eagerly
/// from disk (discovery in TS is lazy per-instance; eager is all the tool
/// needs in M4).
#[derive(Debug, Clone, Default)]
pub struct SkillService {
    skills: HashMap<String, SkillInfo>,
}

impl SkillService {
    pub fn discover(input: &SkillDiscovery) -> SkillService {
        let mut skills = HashMap::new();
        // External skills (skill/index.ts:184-204): `~/.claude` +
        // `~/.agents` (unless `OPENCODE_DISABLE_EXTERNAL_SKILLS`), plus
        // the same dirs up-tree from the directory to the worktree.
        if !std::env::var("OPENCODE_DISABLE_EXTERNAL_SKILLS")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
        {
            let mut external = vec![".agents".to_string()];
            let claude_disabled = [
                "OPENCODE_DISABLE_CLAUDE_CODE",
                "OPENCODE_DISABLE_CLAUDE_CODE_SKILLS",
            ]
            .iter()
            .any(|key| {
                std::env::var(key)
                    .map(|v| !v.is_empty() && v != "0")
                    .unwrap_or(false)
            });
            if !claude_disabled {
                external.push(".claude".to_string());
            }
            for dir in &external {
                let root = input.home.join(dir);
                if root.is_dir() {
                    for path in scan_markdown(&root, EXTERNAL_SKILL_PATTERNS) {
                        add_skill(&mut skills, &path);
                    }
                }
            }
            // `fsys.up({ targets, start: directory, stop: worktree })` —
            // walk directory → worktree (inclusive) looking for the
            // same dirs.
            let mut current = input.directory.clone();
            loop {
                for dir in &external {
                    let root = current.join(dir);
                    if root.is_dir() {
                        for path in scan_markdown(&root, EXTERNAL_SKILL_PATTERNS) {
                            add_skill(&mut skills, &path);
                        }
                    }
                }
                if current == input.worktree {
                    break;
                }
                match current.parent() {
                    Some(parent) => current = parent.to_path_buf(),
                    None => break,
                }
            }
        }
        for dir in &input.directories {
            for path in scan_markdown(dir, OPENCODE_SKILL_PATTERNS) {
                add_skill(&mut skills, &path);
            }
        }
        for item in &input.paths {
            let expanded = match item.strip_prefix("~/") {
                Some(rest) => input.home.join(rest),
                None => PathBuf::from(item),
            };
            let dir = if expanded.is_absolute() {
                expanded
            } else {
                input.directory.join(expanded)
            };
            if !dir.is_dir() {
                continue;
            }
            for path in scan_markdown(&dir, SKILL_PATTERNS) {
                add_skill(&mut skills, &path);
            }
        }
        SkillService { skills }
    }

    /// `Skill.get` (skill/index.ts:289-292).
    pub fn get(&self, name: &str) -> Option<&SkillInfo> {
        self.skills.get(name)
    }

    /// `Skill.all` (skill/index.ts:301-303), unordered.
    pub fn all(&self) -> Vec<&SkillInfo> {
        self.skills.values().collect()
    }
}

impl Skill for SkillService {
    fn require<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<SkillInfo, ToolError>> {
        Box::pin(async move {
            if let Some(info) = self.skills.get(name) {
                return Ok(info.clone());
            }
            let mut available: Vec<String> =
                self.skills.keys().map(|name| name.to_string()).collect();
            available.sort();
            Err(ToolError::Failed(not_found_message(name, &available)))
        })
    }
}

/// `add` (skill/index.ts:105-140): parse frontmatter, skip on parse failure
/// or non-skill frontmatter, later duplicates win.
fn add_skill(skills: &mut HashMap<String, SkillInfo>, path: &Path) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return,
    };
    let Some(md) = parse_markdown(&text) else {
        return;
    };
    let Some(name) = md.data.get("name").and_then(Value::as_str) else {
        return;
    };
    let description = match md.data.get("description") {
        None => None,
        Some(Value::String(description)) => Some(description.clone()),
        Some(_) => return,
    };
    skills.insert(
        name.to_string(),
        SkillInfo {
            name: name.to_string(),
            description,
            location: path.display().to_string(),
            content: md.content,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_support::TempDir;
    use crate::tool::def::Extra;
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::ripgrep::RipgrepService;
    use crate::tool::truncate::TruncateService;
    use serde_json::json;

    fn tool(service: Arc<dyn Skill>, dir: &Path) -> ToolDef {
        skill_tool(
            Arc::new(TruncateService::default_limits(dir.join("tool-output"))),
            fixed_agents(),
            service,
            Arc::new(RipgrepService),
        )
    }

    fn service_for(dir: &Path) -> SkillService {
        SkillService::discover(&SkillDiscovery {
            directories: vec![dir.join(".opencode")],
            paths: Vec::new(),
            directory: dir.to_path_buf(),
            worktree: dir.to_path_buf(),
            home: dir.join("home"),
        })
    }

    #[test]
    fn external_skill_dirs_are_scanned() {
        // `~/.claude/skills` + `~/.agents/skills` + up-tree `.claude`
        // (skill/index.ts:184-204).
        let dir = TempDir::new("skills");
        let home = dir.path().join("home");
        write(
            &home
                .join(".claude")
                .join("skills")
                .join("libertai-search")
                .join("SKILL.md"),
            "---
name: libertai-search
description: Search the web
---
body",
        );
        write(
            &home
                .join(".agents")
                .join("skills")
                .join("other")
                .join("SKILL.md"),
            "---
name: other-skill
description: Other
---
body",
        );
        let service = SkillService::discover(&SkillDiscovery {
            directories: Vec::new(),
            paths: Vec::new(),
            directory: dir.path().to_path_buf(),
            worktree: dir.path().to_path_buf(),
            home: home.clone(),
        });
        assert!(service.get("libertai-search").is_some(), "claude skills");
        assert!(service.get("other-skill").is_some(), "agents skills");
    }

    #[test]
    fn external_skills_up_tree() {
        let dir = TempDir::new("skills-up");
        let home = dir.path().join("home");
        write(
            &dir.path()
                .join(".claude")
                .join("skills")
                .join("proj")
                .join("SKILL.md"),
            "---
name: proj-skill
description: Proj
---
body",
        );
        let service = SkillService::discover(&SkillDiscovery {
            directories: Vec::new(),
            paths: Vec::new(),
            directory: dir.path().join("sub").join("sub"),
            worktree: dir.path().to_path_buf(),
            home,
        });
        assert!(service.get("proj-skill").is_some(), "up-tree .claude");
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn golden_parameters_schema() {
        let golden = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/toolschema/skill.json"
        ))
        .unwrap();
        let golden: Value = serde_json::from_str(&golden).unwrap();
        assert_eq!(parameters(), golden);
    }

    #[test]
    fn discovery_from_config_dirs_and_skill_paths() {
        let temp = TempDir::new("skill-discover");
        let dir = temp.path();
        write(
            &dir.join(".opencode")
                .join("skill")
                .join("deploy")
                .join("SKILL.md"),
            "---\nname: deploy\ndescription: deploys things\n---\nDeploy steps.\n",
        );
        // The `skills/` alias is scanned too, including nested entries.
        write(
            &dir.join(".opencode")
                .join("skills")
                .join("review")
                .join("SKILL.md"),
            "---\nname: review\ndescription: reviews\n---\nReview steps.\n",
        );
        // Neither a `SKILL.md` without a name…
        write(
            &dir.join(".opencode")
                .join("skill")
                .join("anon")
                .join("SKILL.md"),
            "no frontmatter\n",
        );
        // …nor one with a non-string description is a skill.
        write(
            &dir.join(".opencode")
                .join("skill")
                .join("weird")
                .join("SKILL.md"),
            "---\nname: weird\ndescription: [oops\n---\n",
        );
        // A later duplicate name wins (TS logs and overwrites).
        write(
            &dir.join(".opencode")
                .join("skill")
                .join("deploy")
                .join("nested")
                .join("SKILL.md"),
            "---\nname: deploy\ndescription: wins\n---\n",
        );
        // skills.paths entries (absolute + relative + missing).
        write(
            &dir.join("extra-skills").join("lint").join("SKILL.md"),
            "---\nname: lint\n---\nLint.\n",
        );
        write(
            &dir.join("home").join("home-skill").join("SKILL.md"),
            "---\nname: from-home\n---\nHome.\n",
        );

        let service = SkillService::discover(&SkillDiscovery {
            directories: vec![dir.join(".opencode")],
            paths: vec![
                dir.join("extra-skills").to_string_lossy().to_string(),
                "extra-skills".to_string(),
                "~/home-skill".to_string(),
                "missing-dir".to_string(),
            ],
            directory: dir.to_path_buf(),
            worktree: dir.to_path_buf(),
            home: dir.join("home"),
        });

        assert_eq!(
            service.get("deploy").unwrap().description.as_deref(),
            Some("wins")
        );
        assert_eq!(
            service.get("review").unwrap().description.as_deref(),
            Some("reviews")
        );
        assert!(service.get("anon").is_none());
        assert!(service.get("weird").is_none());
        assert!(service.get("lint").is_some());
        assert!(service.get("from-home").is_some());
        assert_eq!(service.all().len(), 4);

        // Content is untrimmed; location is absolute.
        let deploy = service.get("deploy").unwrap();
        assert!(deploy.content.trim().is_empty());
        assert!(deploy.location.ends_with("SKILL.md"));
        assert!(Path::new(&deploy.location).is_absolute());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn require_not_found_message_is_byte_exact() {
        let temp = TempDir::new("skill-notfound");
        write(
            &temp
                .path()
                .join(".opencode")
                .join("skill")
                .join("zeta")
                .join("SKILL.md"),
            "---\nname: zeta\n---\n",
        );
        write(
            &temp
                .path()
                .join(".opencode")
                .join("skill")
                .join("alpha")
                .join("SKILL.md"),
            "---\nname: alpha\n---\n",
        );
        let service = service_for(temp.path());
        let err = service.require("nope").await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "Skill \"nope\" not found. Available skills: alpha, zeta"
        );

        let empty = SkillService::discover(&SkillDiscovery {
            directories: vec![temp.path().join(".opencode").join("missing")],
            paths: Vec::new(),
            directory: temp.path().to_path_buf(),
            worktree: temp.path().to_path_buf(),
            home: temp.path().join("home"),
        });
        let err = empty.require("nope").await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "Skill \"nope\" not found. Available skills: none"
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn output_block_is_byte_exact() {
        let temp = TempDir::new("skill-output");
        let dir = temp.path();
        let location = dir
            .join(".opencode")
            .join("skill")
            .join("my-skill")
            .join("SKILL.md");
        write(
            &location,
            "---\nname: my-skill\ndescription: does things\n---\n\n  Body here.  \n",
        );
        write(
            &dir.join(".opencode")
                .join("skill")
                .join("my-skill")
                .join("reference.txt"),
            "reference",
        );

        let def = tool(Arc::new(service_for(dir)), dir);
        let ask = RecordingAsk::new();
        let inst = instance(dir);
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);

        let result = (def.execute)(json!({ "name": "my-skill" }), ctx)
            .await
            .unwrap();

        assert_eq!(result.title, "Loaded skill: my-skill");
        assert_eq!(
            result.output,
            format!(
                "<skill_content name=\"my-skill\">\n# Skill: my-skill\n\nBody here.\n\nBase directory for this skill: {base}\nRelative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.\nNote: file list is sampled.\n\n<skill_files>\n<file>{file}</file>\n</skill_files>\n</skill_content>",
                base = dir.join(".opencode").join("skill").join("my-skill").display(),
                file = dir.join(".opencode").join("skill").join("my-skill").join("reference.txt").display(),
            )
        );
        assert_eq!(
            result.metadata,
            json!({
                "name": "my-skill",
                "dir": dir.join(".opencode").join("skill").join("my-skill").display().to_string(),
                "truncated": false,
            })
        );

        // ask shape (skill.ts:27-32)
        let requests = ask.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].permission, "skill");
        assert_eq!(requests[0].patterns, vec!["my-skill".to_string()]);
        assert_eq!(requests[0].always, vec!["my-skill".to_string()]);
        assert_eq!(requests[0].metadata, json!({}));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn multiple_files_and_empty_file_list() {
        let temp = TempDir::new("skill-files");
        let dir = temp.path();
        write(
            &dir.join(".opencode")
                .join("skill")
                .join("rich")
                .join("SKILL.md"),
            "---\nname: rich\n---\nBody.\n",
        );
        for extra_file in ["reference/notes.md", "scripts/run.sh", "top.txt"] {
            write(
                &dir.join(".opencode")
                    .join("skill")
                    .join("rich")
                    .join(extra_file),
                "x",
            );
        }
        let def = tool(Arc::new(service_for(dir)), dir);
        let ask = RecordingAsk::new();
        let inst = instance(dir);
        let extra = Extra::default();
        let first = ctx(&ask, &inst, &extra);

        let result = (def.execute)(json!({ "name": "rich" }), first)
            .await
            .unwrap();
        let base = dir.join(".opencode").join("skill").join("rich");
        for extra_file in ["reference/notes.md", "scripts/run.sh", "top.txt"] {
            assert!(
                result
                    .output
                    .contains(&format!("<file>{}</file>", base.join(extra_file).display())),
                "{}",
                result.output
            );
        }
        assert!(!result.output.contains("SKILL.md"));

        // A skill without extra files renders an empty file list.
        write(
            &dir.join(".opencode")
                .join("skill")
                .join("bare")
                .join("SKILL.md"),
            "---\nname: bare\n---\nBody.\n",
        );
        let def = tool(Arc::new(service_for(dir)), dir);
        let result = (def.execute)(json!({ "name": "bare" }), ctx(&ask, &inst, &extra))
            .await
            .unwrap();
        assert!(
            result.output.contains("<skill_files>\n\n</skill_files>"),
            "{}",
            result.output
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn unknown_skill_dies_without_asking() {
        let temp = TempDir::new("skill-unknown");
        let dir = temp.path();
        write(
            &dir.join(".opencode")
                .join("skill")
                .join("known")
                .join("SKILL.md"),
            "---\nname: known\n---\nBody.\n",
        );
        let def = tool(Arc::new(service_for(dir)), dir);
        let ask = RecordingAsk::new();
        let inst = instance(dir);
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);

        let err = (def.execute)(json!({ "name": "unknown" }), ctx)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Skill \"unknown\" not found. Available skills: known"
        );
        // `require` runs before the ask (skill.ts:24-32).
        assert!(ask.requests().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }
}
