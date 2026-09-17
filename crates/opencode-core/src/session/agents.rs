//! Agent registry — port of `agent/agent.ts` (agent.ts:35-340).
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use rand::seq::SliceRandom;

use opencode_schema::permission_v1::PermissionV1Action;
use opencode_schema::permission_v1::PermissionV1Rule;
use opencode_schema::permission_v1::PermissionV1Ruleset;

use crate::config::schema::{AgentMode as ConfigAgentMode, Config};
use crate::tool::def::AgentMode;
use crate::tool::permission::{from_config, merge};

/// `Agent.Info.model` (agent.ts:45-50).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentModel {
    pub provider_id: String,
    pub model_id: String,
}

/// `Agent.Info` (agent.ts:35-56).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentInfo {
    pub name: String,
    pub description: Option<String>,
    pub mode: AgentMode,
    pub native: Option<bool>,
    pub hidden: Option<bool>,
    pub top_p: Option<f64>,
    pub temperature: Option<f64>,
    pub color: Option<String>,
    pub permission: PermissionV1Ruleset,
    pub model: Option<AgentModel>,
    pub variant: Option<String>,
    pub prompt: Option<String>,
    pub options: BTreeMap<String, serde_json::Value>,
    pub steps: Option<f64>,
}

impl AgentInfo {
    /// `hidden !== true`
    pub fn is_visible(&self) -> bool {
        self.hidden != Some(true)
    }
}

/// `Provider.parseModel` (provider/provider.ts:2058-2064): split at the
/// first `/`.
pub fn parse_model(model: &str) -> AgentModel {
    let (provider_id, rest) = match model.split_once('/') {
        Some((provider_id, rest)) => (provider_id, rest),
        None => (model, ""),
    };
    AgentModel {
        provider_id: provider_id.to_string(),
        model_id: rest.to_string(),
    }
}

/// An empty [`Config`] — every field is optional with a default, so `{}`
/// deserializes (used for `AgentRegistryInput::default`).
fn empty_config() -> Config {
    serde_json::from_value(serde_json::json!({})).expect("all config fields are optional")
}

/// The registry construction inputs (agent.ts:98-136).
#[derive(Debug, Clone)]
pub struct AgentRegistryInput {
    /// The effective config (`cfg.permission`, `cfg.agent`, `cfg.default_agent`).
    pub config: Config,
    /// `skill.dirs()`.
    pub skill_dirs: Vec<PathBuf>,
    /// `reference.list()` paths (only read when the config declares
    /// `references`/`reference`).
    pub reference_dirs: Vec<PathBuf>,
    /// Instance `ctx.worktree` (plan-agent path relativization).
    pub worktree: PathBuf,
    /// `Global.Path.data` (plans dir, truncate glob).
    pub data_dir: PathBuf,
    /// `Global.Path.tmp`.
    pub tmp_dir: PathBuf,
    /// The user home directory (`~/` + `$HOME` expansion in `fromConfig`).
    pub home: PathBuf,
}

impl Default for AgentRegistryInput {
    fn default() -> Self {
        Self {
            config: empty_config(),
            skill_dirs: Vec::new(),
            reference_dirs: Vec::new(),
            worktree: PathBuf::new(),
            data_dir: PathBuf::new(),
            tmp_dir: PathBuf::new(),
            home: PathBuf::new(),
        }
    }
}

/// `defaultInfo()` failures (agent.ts:328-340).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DefaultAgentError {
    #[error("default agent \"{0}\" not found")]
    NotFound(String),
    #[error("default agent \"{0}\" is a subagent")]
    Subagent(String),
    #[error("default agent \"{0}\" is hidden")]
    Hidden(String),
    #[error("no primary visible agent found")]
    NoPrimaryVisible,
}

/// The agent registry — `Agent.Service` state (agent.ts:88-340). `generate()`
/// is not ported (M8; the spec pins it as `unsupported` in M5).
#[derive(Debug, Clone)]
pub struct AgentRegistry {
    /// Insertion-ordered (TS object key order): build, plan, general,
    /// explore, compaction, title, summary, then config agents.
    agents: IndexMap<String, AgentInfo>,
    /// `cfg.default_agent` (resolved once at construction).
    default_agent: Option<String>,
}

const PROMPT_COMPACTION: &str = include_str!("../agent/prompt/compaction.txt");
const PROMPT_EXPLORE: &str = include_str!("../agent/prompt/explore.txt");
const PROMPT_SUMMARY: &str = include_str!("../agent/prompt/summary.txt");
const PROMPT_TITLE: &str = include_str!("../agent/prompt/title.txt");

/// `Truncate.GLOB` (tool/truncate.ts:17): `<data>/tool-output/*`.
pub fn truncation_glob(data_dir: &Path) -> String {
    join_glob(&data_dir.join("tool-output"))
}

fn join_glob(dir: &Path) -> String {
    format!("{}/*", dir.display())
}

/// Node `path.relative(from, to)` — lexical, `/`-separated.
fn path_relative(from: &Path, to: &Path) -> String {
    let skip = |c: &std::path::Component<'_>| !matches!(c, std::path::Component::CurDir);
    let from: Vec<_> = from.components().filter(skip).collect();
    let to: Vec<_> = to.components().filter(skip).collect();
    let mut common = 0;
    while common < from.len() && common < to.len() && from[common] == to[common] {
        common += 1;
    }
    let mut result = PathBuf::new();
    for _ in common..from.len() {
        result.push("..");
    }
    for part in &to[common..] {
        result.push(part.as_os_str());
    }
    result.display().to_string()
}

fn rule(permission: &str, pattern: &str, action: PermissionV1Action) -> PermissionV1Rule {
    PermissionV1Rule {
        permission: permission.to_string(),
        pattern: pattern.to_string(),
        action,
    }
}

/// Build a ruleset from an ordered permission→(pattern, action) table —
/// `Permission.fromConfig` on a literal, preserving declaration order.
fn rules(table: &[(&str, Vec<(&str, PermissionV1Action)>)]) -> PermissionV1Ruleset {
    let mut out = PermissionV1Ruleset::new();
    for (permission, patterns) in table {
        for (pattern, action) in patterns {
            out.push(rule(permission, pattern, *action));
        }
    }
    out
}

/// `external_directory: { "*": "ask", ...whitelisted: allow }`
/// (agent.ts:114-117, 122-125).
fn external_allow_rules(whitelisted: &[String]) -> PermissionV1Ruleset {
    let mut out = vec![rule("external_directory", "*", PermissionV1Action::Ask)];
    for dir in whitelisted {
        out.push(rule("external_directory", dir, PermissionV1Action::Allow));
    }
    out
}

fn agent_mode(mode: ConfigAgentMode) -> AgentMode {
    match mode {
        ConfigAgentMode::Subagent => AgentMode::Subagent,
        ConfigAgentMode::Primary => AgentMode::Primary,
        ConfigAgentMode::All => AgentMode::All,
    }
}

/// `Slug.create()` (packages/core/src/util/slug.ts).
pub fn slug_create() -> String {
    const ADJECTIVES: &[&str] = &[
        "brave", "calm", "clever", "cosmic", "crisp", "curious", "eager", "gentle", "glowing",
        "happy", "hidden", "jolly", "kind", "lucky", "mighty", "misty", "neon", "nimble",
        "playful", "proud", "quick", "quiet", "shiny", "silent", "stellar", "sunny", "swift",
        "tidy", "witty",
    ];
    const NOUNS: &[&str] = &[
        "cabin", "cactus", "canyon", "circuit", "comet", "eagle", "engine", "falcon", "forest",
        "garden", "harbor", "island", "knight", "lagoon", "meadow", "moon", "mountain", "nebula",
        "orchid", "otter", "panda", "pixel", "planet", "river", "rocket", "sailor", "squid",
        "star", "tiger", "wizard", "wolf",
    ];
    let adjective = ADJECTIVES.choose(&mut rand::thread_rng()).unwrap();
    let noun = NOUNS.choose(&mut rand::thread_rng()).unwrap();
    format!("{adjective}-{noun}")
}

impl AgentRegistry {
    /// Construct the registry (agent.ts:88-311).
    pub fn new(input: &AgentRegistryInput) -> AgentRegistry {
        let cfg = &input.config;
        let glob = truncation_glob(&input.data_dir);
        let mut whitelisted_dirs = vec![glob.clone(), join_glob(&input.tmp_dir)];
        for dir in input.skill_dirs.iter().chain(input.reference_dirs.iter()) {
            whitelisted_dirs.push(join_glob(dir));
        }

        // agent.ts:119-136 — the defaults table (declaration order binding).
        let mut defaults = rules(&[
            ("*", vec![("*", PermissionV1Action::Allow)]),
            ("doom_loop", vec![("*", PermissionV1Action::Ask)]),
        ]);
        defaults.extend(external_allow_rules(&whitelisted_dirs));
        defaults.extend(rules(&[
            ("question", vec![("*", PermissionV1Action::Deny)]),
            ("plan_enter", vec![("*", PermissionV1Action::Deny)]),
            ("plan_exit", vec![("*", PermissionV1Action::Deny)]),
            (
                "read",
                vec![
                    ("*", PermissionV1Action::Allow),
                    ("*.env", PermissionV1Action::Ask),
                    ("*.env.*", PermissionV1Action::Ask),
                    ("*.env.example", PermissionV1Action::Allow),
                ],
            ),
        ]));

        let user = match &cfg.permission {
            Some(permission) => from_config(permission, &input.home),
            None => Vec::new(),
        };
        let readonly_external = external_allow_rules(&whitelisted_dirs);
        let deny_all = rules(&[("*", vec![("*", PermissionV1Action::Deny)])]);
        let plans_dir = input.data_dir.join("plans");

        let mut agents: IndexMap<String, AgentInfo> = IndexMap::new();

        // build (agent.ts:141-155)
        agents.insert(
            "build".into(),
            AgentInfo {
                name: "build".to_string(),
                description: Some(
                    "The default agent. Executes tools based on configured permissions.".into(),
                ),
                mode: AgentMode::Primary,
                native: Some(true),
                hidden: None,
                top_p: None,
                temperature: None,
                color: None,
                permission: merge(&[
                    &defaults,
                    &rules(&[
                        ("question", vec![("*", PermissionV1Action::Allow)]),
                        ("plan_enter", vec![("*", PermissionV1Action::Allow)]),
                    ]),
                    &user,
                ]),
                model: None,
                variant: None,
                prompt: None,
                options: BTreeMap::new(),
                steps: None,
            },
        );
        // plan (agent.ts:156-181)
        agents.insert(
            "plan".into(),
            AgentInfo {
                name: "plan".to_string(),
                description: Some("Plan mode. Disallows all edit tools.".into()),
                mode: AgentMode::Primary,
                native: Some(true),
                hidden: None,
                top_p: None,
                temperature: None,
                color: None,
                permission: merge(&[
                    &defaults,
                    &rules(&[
                        ("question", vec![("*", PermissionV1Action::Allow)]),
                        ("plan_exit", vec![("*", PermissionV1Action::Allow)]),
                        ("task", vec![("general", PermissionV1Action::Deny)]),
                        (
                            "external_directory",
                            vec![(join_glob(&plans_dir).as_str(), PermissionV1Action::Allow)],
                        ),
                        (
                            "edit",
                            vec![
                                ("*", PermissionV1Action::Deny),
                                (
                                    Path::new(".opencode")
                                        .join("plans")
                                        .join("*.md")
                                        .display()
                                        .to_string()
                                        .as_str(),
                                    PermissionV1Action::Allow,
                                ),
                                (
                                    path_relative(&input.worktree, &plans_dir.join("*.md"))
                                        .as_str(),
                                    PermissionV1Action::Allow,
                                ),
                            ],
                        ),
                    ]),
                    &user,
                ]),
                model: None,
                variant: None,
                prompt: None,
                options: BTreeMap::new(),
                steps: None,
            },
        );
        // general (agent.ts:182-195)
        agents.insert(
            "general".into(),
            AgentInfo {
                name: "general".to_string(),
                description: Some(
                    "General-purpose agent for researching complex questions and executing multi-step tasks. Use this agent to execute multiple units of work in parallel.".into(),
                ),
                mode: AgentMode::Subagent,
                native: Some(true),
                hidden: None,
                top_p: None,
                temperature: None,
                color: None,
                permission: merge(&[
                    &defaults,
                    &rules(&[("todowrite", vec![("*", PermissionV1Action::Deny)])]),
                    &user,
                ]),
                model: None,
                variant: None,
                prompt: None,
                options: BTreeMap::new(),
                steps: None,
            },
        );
        // explore (agent.ts:196-218)
        agents.insert(
            "explore".into(),
            AgentInfo {
                name: "explore".to_string(),
                description: Some(
                    "Fast agent specialized for exploring codebases. Use this when you need to quickly find files by patterns (eg. \"src/components/**/*.tsx\"), search code for keywords (eg. \"API endpoints\"), or answer questions about the codebase (eg. \"how do API endpoints work?\"). When calling this agent, specify the desired thoroughness level: \"quick\" for basic searches, \"medium\" for moderate exploration, or \"very thorough\" for comprehensive analysis across multiple locations and naming conventions.".into(),
                ),
                mode: AgentMode::Subagent,
                native: Some(true),
                hidden: None,
                top_p: None,
                temperature: None,
                color: None,
                permission: merge(&[
                    &defaults,
                    &rules(&[
                        ("*", vec![("*", PermissionV1Action::Deny)]),
                        ("grep", vec![("*", PermissionV1Action::Allow)]),
                        ("glob", vec![("*", PermissionV1Action::Allow)]),
                        ("list", vec![("*", PermissionV1Action::Allow)]),
                        ("bash", vec![("*", PermissionV1Action::Allow)]),
                        ("webfetch", vec![("*", PermissionV1Action::Allow)]),
                        ("websearch", vec![("*", PermissionV1Action::Allow)]),
                        ("read", vec![("*", PermissionV1Action::Allow)]),
                    ]),
                    &rules(&[(
                        "external_directory",
                        readonly_external
                            .iter()
                            .map(|r| (r.pattern.as_str(), r.action))
                            .collect(),
                    )]),
                    &user,
                ]),
                model: None,
                variant: None,
                prompt: Some(PROMPT_EXPLORE.to_string()),
                options: BTreeMap::new(),
                steps: None,
            },
        );
        // compaction (agent.ts:219-233)
        agents.insert(
            "compaction".into(),
            AgentInfo {
                name: "compaction".to_string(),
                description: None,
                mode: AgentMode::Primary,
                native: Some(true),
                hidden: Some(true),
                top_p: None,
                temperature: None,
                color: None,
                permission: merge(&[&defaults, &deny_all, &user]),
                model: None,
                variant: None,
                prompt: Some(PROMPT_COMPACTION.to_string()),
                options: BTreeMap::new(),
                steps: None,
            },
        );
        // title (agent.ts:234-249)
        agents.insert(
            "title".into(),
            AgentInfo {
                name: "title".to_string(),
                description: None,
                mode: AgentMode::Primary,
                native: Some(true),
                hidden: Some(true),
                top_p: None,
                temperature: Some(0.5),
                color: None,
                permission: merge(&[&defaults, &deny_all, &user]),
                model: None,
                variant: None,
                prompt: Some(PROMPT_TITLE.to_string()),
                options: BTreeMap::new(),
                steps: None,
            },
        );
        // summary (agent.ts:250-264)
        agents.insert(
            "summary".into(),
            AgentInfo {
                name: "summary".to_string(),
                description: None,
                mode: AgentMode::Primary,
                native: Some(true),
                hidden: Some(true),
                top_p: None,
                temperature: None,
                color: None,
                permission: merge(&[&defaults, &deny_all, &user]),
                model: None,
                variant: None,
                prompt: Some(PROMPT_SUMMARY.to_string()),
                options: BTreeMap::new(),
                steps: None,
            },
        );

        // Config agents (agent.ts:267-294).
        if let Some(config_agents) = &cfg.agent {
            for (key, value) in config_agents {
                if value.disable == Some(true) {
                    agents.shift_remove(key);
                    continue;
                }
                let item = agents.entry(key.clone()).or_insert_with(|| AgentInfo {
                    name: key.clone(),
                    mode: AgentMode::All,
                    permission: merge(&[&defaults, &user]),
                    options: BTreeMap::new(),
                    native: Some(false),
                    model: None,
                    variant: None,
                    prompt: None,
                    description: None,
                    temperature: None,
                    top_p: None,
                    hidden: None,
                    color: None,
                    steps: None,
                });
                if let Some(model) = &value.model {
                    item.model = Some(parse_model(model));
                }
                if let Some(variant) = &value.variant {
                    item.variant = Some(variant.clone());
                }
                if let Some(prompt) = &value.prompt {
                    item.prompt = Some(prompt.clone());
                }
                if let Some(description) = &value.description {
                    item.description = Some(description.clone());
                }
                if let Some(temperature) = value.temperature {
                    item.temperature = Some(temperature);
                }
                if let Some(top_p) = value.top_p {
                    item.top_p = Some(top_p);
                }
                if let Some(mode) = value.mode {
                    item.mode = agent_mode(mode);
                }
                if let Some(color) = &value.color {
                    item.color = Some(color.0.clone());
                }
                if let Some(hidden) = value.hidden {
                    item.hidden = Some(hidden);
                }
                if let Some(name) = value.rest.get("name").and_then(serde_json::Value::as_str) {
                    item.name = name.to_string();
                }
                if let Some(steps) = value.steps {
                    item.steps = Some(steps.0 as f64);
                }
                let mut options = serde_json::to_value(&item.options).unwrap_or_default();
                crate::merge::merge_deep(
                    &mut options,
                    &serde_json::to_value(&value.options).unwrap_or_default(),
                );
                item.options = serde_json::from_value(options).unwrap_or_default();
                item.permission = merge(&[
                    &item.permission,
                    &from_config(&value.permission, &input.home),
                ]);
            }
        }

        // Ensure Truncate.GLOB is allowed unless explicitly configured
        // (agent.ts:297-310).
        let names: Vec<String> = agents.keys().cloned().collect();
        for name in names {
            let explicit = agents
                .get(&name)
                .expect("agent exists")
                .permission
                .iter()
                .any(|r| {
                    r.permission == "external_directory"
                        && r.action == PermissionV1Action::Deny
                        && r.pattern == glob
                });
            if explicit {
                continue;
            }
            let item = agents.get_mut(&name).expect("agent exists");
            item.permission = merge(&[
                &item.permission,
                &vec![rule("external_directory", &glob, PermissionV1Action::Allow)],
            ]);
        }

        AgentRegistry {
            agents,
            default_agent: cfg.default_agent.clone(),
        }
    }

    /// `get` (agent.ts:312-314).
    pub fn get(&self, agent: &str) -> Option<&AgentInfo> {
        self.agents.get(agent)
    }

    /// `list` (agent.ts:316-326): default-agent first, then name asc.
    pub fn list(&self) -> Vec<AgentInfo> {
        let default_agent = &self.default_agent;
        let mut agents: Vec<AgentInfo> = self.agents.values().cloned().collect();
        agents.sort_by(|a, b| {
            let a_default = match default_agent {
                Some(name) => &a.name == name,
                None => a.name == "build",
            };
            let b_default = match default_agent {
                Some(name) => &b.name == name,
                None => b.name == "build",
            };
            b_default.cmp(&a_default).then_with(|| a.name.cmp(&b.name))
        });
        agents
    }

    /// `defaultInfo` (agent.ts:328-340).
    pub fn default_info(&self) -> Result<AgentInfo, DefaultAgentError> {
        if let Some(default_agent) = &self.default_agent {
            let agent = self
                .agents
                .get(default_agent)
                .ok_or_else(|| DefaultAgentError::NotFound(default_agent.clone()))?;
            if agent.mode == AgentMode::Subagent {
                return Err(DefaultAgentError::Subagent(default_agent.clone()));
            }
            if agent.hidden == Some(true) {
                return Err(DefaultAgentError::Hidden(default_agent.clone()));
            }
            return Ok(agent.clone());
        }
        self.agents
            .values()
            .find(|agent| agent.mode != AgentMode::Subagent && agent.is_visible())
            .cloned()
            .ok_or(DefaultAgentError::NoPrimaryVisible)
    }

    /// `defaultAgent` (agent.ts:342-344).
    pub fn default_agent(&self) -> Result<String, DefaultAgentError> {
        Ok(self.default_info()?.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::permission::evaluate;

    fn test_input(config: &Config) -> AgentRegistryInput {
        AgentRegistryInput {
            config: config.clone(),
            skill_dirs: vec![],
            reference_dirs: vec![],
            worktree: PathBuf::from("/repo"),
            data_dir: PathBuf::from("/data"),
            tmp_dir: PathBuf::from("/tmp"),
            home: PathBuf::from("/home/user"),
        }
    }

    fn empty_config() -> Config {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }

    fn user_config(permission: serde_json::Value) -> Config {
        serde_json::from_value(serde_json::json!({ "permission": permission })).unwrap()
    }

    #[test]
    fn seven_native_agents() {
        let registry = AgentRegistry::new(&test_input(&empty_config()));
        for name in [
            "build",
            "plan",
            "general",
            "explore",
            "compaction",
            "title",
            "summary",
        ] {
            let agent = registry
                .get(name)
                .unwrap_or_else(|| panic!("{name} missing"));
            assert!(agent.native == Some(true), "{name} must be native");
        }
        assert_eq!(registry.get("nope"), None);
    }

    #[test]
    fn build_agent_permissions() {
        let registry = AgentRegistry::new(&test_input(&empty_config()));
        let build = registry.get("build").unwrap();
        assert_eq!(build.mode, AgentMode::Primary);
        assert_eq!(
            evaluate("bash", "anything", &[&build.permission]).action,
            PermissionV1Action::Allow
        );
        assert_eq!(
            evaluate("doom_loop", "*", &[&build.permission]).action,
            PermissionV1Action::Ask
        );
        assert_eq!(
            evaluate("question", "*", &[&build.permission]).action,
            PermissionV1Action::Allow
        );
        assert_eq!(
            evaluate("plan_enter", "*", &[&build.permission]).action,
            PermissionV1Action::Allow
        );
        assert_eq!(
            evaluate("plan_exit", "*", &[&build.permission]).action,
            PermissionV1Action::Deny
        );
        assert_eq!(
            evaluate("read", ".env", &[&build.permission]).action,
            PermissionV1Action::Ask
        );
        assert_eq!(
            evaluate("read", "a.b.env", &[&build.permission]).action,
            PermissionV1Action::Ask
        );
        assert_eq!(
            evaluate("read", "a.env.example", &[&build.permission]).action,
            PermissionV1Action::Allow
        );
    }

    #[test]
    fn external_directory_whitelist() {
        let registry = AgentRegistry::new(&test_input(&empty_config()));
        let build = registry.get("build").unwrap();
        // unknown dirs ask
        assert_eq!(
            evaluate(
                "external_directory",
                "/some/random/dir",
                &[&build.permission]
            )
            .action,
            PermissionV1Action::Ask
        );
        // tmp + truncate glob allow
        for dir in ["/tmp/x/y", "/data/tool-output/spill"] {
            assert_eq!(
                evaluate("external_directory", dir, &[&build.permission]).action,
                PermissionV1Action::Allow,
                "{dir}"
            );
        }
    }

    #[test]
    fn skill_and_reference_dirs_are_whitelisted() {
        let registry = AgentRegistry::new(&AgentRegistryInput {
            config: empty_config(),
            skill_dirs: vec![PathBuf::from("/skills/my-skill")],
            reference_dirs: vec![PathBuf::from("/refs/main")],
            worktree: PathBuf::from("/repo"),
            data_dir: PathBuf::from("/data"),
            tmp_dir: PathBuf::from("/tmp"),
            home: PathBuf::from("/home/user"),
        });
        let build = registry.get("build").unwrap();
        assert_eq!(
            evaluate(
                "external_directory",
                "/skills/my-skill/x",
                &[&build.permission]
            )
            .action,
            PermissionV1Action::Allow
        );
        assert_eq!(
            evaluate("external_directory", "/refs/main/x", &[&build.permission]).action,
            PermissionV1Action::Allow
        );
        assert_eq!(
            evaluate(
                "external_directory",
                "/skills/other/x",
                &[&build.permission]
            )
            .action,
            PermissionV1Action::Ask
        );
    }

    #[test]
    fn plan_agent_denies_edits() {
        let registry = AgentRegistry::new(&test_input(&empty_config()));
        let plan = registry.get("plan").unwrap();
        assert_eq!(
            evaluate("edit", "src/lib.ts", &[&plan.permission]).action,
            PermissionV1Action::Deny
        );
        assert_eq!(
            evaluate("edit", ".opencode/plans/plan.md", &[&plan.permission]).action,
            PermissionV1Action::Allow
        );
        assert_eq!(
            evaluate("task", "general", &[&plan.permission]).action,
            PermissionV1Action::Deny
        );
        assert_eq!(
            evaluate("plan_exit", "*", &[&plan.permission]).action,
            PermissionV1Action::Allow
        );
        // <data>/plans/* allow
        assert_eq!(
            evaluate("external_directory", "/data/plans/x", &[&plan.permission]).action,
            PermissionV1Action::Allow
        );
        // relative path from worktree to <data>/plans/*.md allow
        assert_eq!(
            evaluate("edit", "../data/plans/a.md", &[&plan.permission]).action,
            PermissionV1Action::Allow
        );
    }

    #[test]
    fn general_agent_denies_todowrite() {
        let registry = AgentRegistry::new(&test_input(&empty_config()));
        let general = registry.get("general").unwrap();
        assert_eq!(general.mode, AgentMode::Subagent);
        assert_eq!(
            evaluate("todowrite", "*", &[&general.permission]).action,
            PermissionV1Action::Deny
        );
    }

    #[test]
    fn explore_agent_readonly() {
        let registry = AgentRegistry::new(&test_input(&empty_config()));
        let explore = registry.get("explore").unwrap();
        assert_eq!(
            evaluate("*", "anything", &[&explore.permission]).action,
            PermissionV1Action::Deny
        );
        for tool in [
            "grep",
            "glob",
            "list",
            "bash",
            "webfetch",
            "websearch",
            "read",
        ] {
            assert_eq!(
                evaluate(tool, "*", &[&explore.permission]).action,
                PermissionV1Action::Allow,
                "{tool}"
            );
        }
        // readonly external_directory: ask for unknown, allow whitelisted
        assert_eq!(
            evaluate("external_directory", "/elsewhere", &[&explore.permission]).action,
            PermissionV1Action::Ask
        );
        assert_eq!(
            evaluate("external_directory", "/tmp/x", &[&explore.permission]).action,
            PermissionV1Action::Allow
        );
        assert_eq!(explore.mode, AgentMode::Subagent);
        assert!(explore.prompt.as_deref().unwrap().starts_with("You are"));
    }

    #[test]
    fn hidden_agents() {
        let registry = AgentRegistry::new(&test_input(&empty_config()));
        for name in ["compaction", "title", "summary"] {
            let agent = registry.get(name).unwrap();
            assert_eq!(agent.hidden, Some(true), "{name}");
            assert_eq!(agent.mode, AgentMode::Primary);
        }
        assert_eq!(registry.get("title").unwrap().temperature, Some(0.5));
    }

    #[test]
    fn hidden_agents_deny_everything() {
        let registry = AgentRegistry::new(&test_input(&empty_config()));
        for name in ["compaction", "title", "summary"] {
            let agent = registry.get(name).unwrap();
            assert_eq!(
                evaluate("*", "anything", &[&agent.permission]).action,
                PermissionV1Action::Deny
            );
        }
    }

    #[test]
    fn user_permission_overrides_defaults() {
        // `user` merges last, so its rules win over the defaults.
        let registry = AgentRegistry::new(&test_input(&user_config(
            serde_json::json!({ "bash": "deny" }),
        )));
        let build = registry.get("build").unwrap();
        assert_eq!(
            evaluate("bash", "ls", &[&build.permission]).action,
            PermissionV1Action::Deny
        );
    }

    #[test]
    fn config_agent_disable() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "agent": { "build": { "disable": true } },
        }))
        .unwrap();
        let registry = AgentRegistry::new(&test_input(&config));
        assert_eq!(registry.get("build"), None);
    }

    #[test]
    fn config_agent_unknown_creates_new() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "agent": {
                "reviewer": {
                    "description": "Reviews code",
                    "prompt": "You review code.",
                    "model": "anthropic/claude-sonnet-4-5",
                },
            },
        }))
        .unwrap();
        let registry = AgentRegistry::new(&test_input(&config));
        let reviewer = registry.get("reviewer").unwrap();
        assert_eq!(reviewer.mode, AgentMode::All);
        assert_eq!(reviewer.native, Some(false));
        assert_eq!(
            reviewer.model,
            Some(AgentModel {
                provider_id: "anthropic".to_string(),
                model_id: "claude-sonnet-4-5".to_string(),
            })
        );
        assert_eq!(reviewer.description.as_deref(), Some("Reviews code"));
        // defaults + user merged in
        assert_eq!(
            evaluate("read", "src/lib.ts", &[&reviewer.permission]).action,
            PermissionV1Action::Allow
        );
    }

    #[test]
    fn config_agent_overrides_native() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "agent": {
                "build": { "description": "Custom build", "steps": 10 },
                "title": { "hidden": false },
            },
        }))
        .unwrap();
        let registry = AgentRegistry::new(&test_input(&config));
        let build = registry.get("build").unwrap();
        assert_eq!(build.description.as_deref(), Some("Custom build"));
        assert_eq!(build.mode, AgentMode::Primary);
        assert_eq!(build.steps, Some(10.0));
        // ?? semantics: title keeps its temperature, clears hidden.
        let title = registry.get("title").unwrap();
        assert_eq!(title.temperature, Some(0.5));
        assert_eq!(title.hidden, Some(false));
    }

    #[test]
    fn config_agent_options_merge_deep() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "agent": {
                "build": {
                    "options": {
                        "reasoning": { "effort": "high", "extra": true },
                    },
                },
            },
        }))
        .unwrap();
        let registry = AgentRegistry::new(&test_input(&config));
        let build = registry.get("build").unwrap();
        assert_eq!(
            build.options.get("reasoning"),
            Some(&serde_json::json!({ "effort": "high", "extra": true }))
        );
    }

    #[test]
    fn config_agent_name_override() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "agent": { "reviewer": { "name": "Reviewer" } },
        }))
        .unwrap();
        let registry = AgentRegistry::new(&test_input(&config));
        assert_eq!(registry.get("reviewer").unwrap().name, "Reviewer");
    }

    #[test]
    fn glob_allow_append_and_explicit_deny() {
        let glob = truncation_glob(Path::new("/data"));
        // Default: GLOB allowed.
        let registry = AgentRegistry::new(&test_input(&empty_config()));
        let build = registry.get("build").unwrap();
        assert_eq!(
            evaluate("external_directory", &glob, &[&build.permission]).action,
            PermissionV1Action::Allow
        );

        // Explicit deny rule for GLOB suppresses the append.
        let config: Config = serde_json::from_value(serde_json::json!({
            "agent": {
                "build": {
                    "permission": { "external_directory": { "/data/tool-output/*": "deny" } },
                },
            },
        }))
        .unwrap();
        let registry = AgentRegistry::new(&test_input(&config));
        let build = registry.get("build").unwrap();
        assert_eq!(
            evaluate("external_directory", &glob, &[&build.permission]).action,
            PermissionV1Action::Deny
        );
    }

    #[test]
    fn list_orders_default_agent_first_then_name() {
        let registry = AgentRegistry::new(&test_input(&empty_config()));
        let listed = registry.list();
        let names: Vec<&str> = listed.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names.first(), Some(&"build"));
        // All 7 native agents present, name-ascending after build.
        assert_eq!(
            names[1..],
            [
                "compaction",
                "explore",
                "general",
                "plan",
                "summary",
                "title"
            ],
        );
    }

    #[test]
    fn list_honors_configured_default_agent() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "default_agent": "plan",
        }))
        .unwrap();
        let registry = AgentRegistry::new(&test_input(&config));
        assert_eq!(registry.list().first().unwrap().name, "plan");
        assert_eq!(registry.default_agent().unwrap(), "plan");
    }

    #[test]
    fn default_info_errors() {
        let registry = AgentRegistry::new(&test_input(&empty_config()));
        assert_eq!(registry.default_agent().unwrap(), "build");

        let config: Config = serde_json::from_value(serde_json::json!({
            "default_agent": "nope",
        }))
        .unwrap();
        let registry = AgentRegistry::new(&test_input(&config));
        assert_eq!(
            registry.default_agent().unwrap_err(),
            DefaultAgentError::NotFound("nope".to_string())
        );

        let config: Config = serde_json::from_value(serde_json::json!({
            "default_agent": "explore",
        }))
        .unwrap();
        let registry = AgentRegistry::new(&test_input(&config));
        assert_eq!(
            registry.default_agent().unwrap_err(),
            DefaultAgentError::Subagent("explore".to_string())
        );

        let config: Config = serde_json::from_value(serde_json::json!({
            "default_agent": "title",
        }))
        .unwrap();
        let registry = AgentRegistry::new(&test_input(&config));
        assert_eq!(
            registry.default_agent().unwrap_err(),
            DefaultAgentError::Hidden("title".to_string())
        );

        // Every visible primary agent disabled -> no primary visible agent.
        let config: Config = serde_json::from_value(serde_json::json!({
            "agent": {
                "build": { "disable": true },
                "plan": { "disable": true },
            },
        }))
        .unwrap();
        let registry = AgentRegistry::new(&test_input(&config));
        assert_eq!(
            registry.default_agent().unwrap_err(),
            DefaultAgentError::NoPrimaryVisible
        );
    }

    #[test]
    fn default_info_falls_back_to_first_visible() {
        // build disabled -> plan is the first visible primary agent.
        let config: Config = serde_json::from_value(serde_json::json!({
            "agent": { "build": { "disable": true } },
        }))
        .unwrap();
        let registry = AgentRegistry::new(&test_input(&config));
        assert_eq!(registry.default_agent().unwrap(), "plan");
    }

    #[test]
    fn parse_model_splits_at_first_slash() {
        let model = parse_model("anthropic/claude/sonnet");
        assert_eq!(model.provider_id, "anthropic");
        assert_eq!(model.model_id, "claude/sonnet");
    }

    #[test]
    fn path_relative_walks() {
        assert_eq!(
            path_relative(Path::new("/a/b"), Path::new("/a/c")),
            "../c".to_string()
        );
        assert_eq!(
            path_relative(Path::new("/a/b"), Path::new("/a/b/c/d")),
            "c/d".to_string()
        );
        assert_eq!(path_relative(Path::new("/a/b"), Path::new("/a/b")), "");
    }

    #[test]
    fn slug_uses_the_ts_vocabulary() {
        for _ in 0..20 {
            let slug = slug_create();
            assert!(
                slug.split('-').count() == 2,
                "expected adjective-noun, got {slug}"
            );
        }
    }
}
