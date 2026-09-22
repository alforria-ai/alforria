//! System prompt — port of `session/system.ts`.
//!
//! * `provider` — the per-provider prompt dispatch table (system.ts:28-51).
//! * `environment` — the `<env>` block (system.ts:69-105).
//! * `skills` — the skill system-prompt block (system.ts:107-119).
//! * `mcp` — the `<mcp_instructions>` block (system.ts:121-137).

use crate::session::agents::AgentInfo;
use crate::tool::permission::{disabled, merge};
use crate::tool::skill::SkillInfo;

use alforria_schema::permission_v1::PermissionV1Ruleset;

use std::path::Path;

const PROMPT_ANTHROPIC: &str = include_str!("prompt/anthropic.txt");
const PROMPT_DEFAULT: &str = include_str!("prompt/default.txt");
const PROMPT_BEAST: &str = include_str!("prompt/beast.txt");
const PROMPT_GEMINI: &str = include_str!("prompt/gemini.txt");
const PROMPT_GPT: &str = include_str!("prompt/gpt.txt");
const PROMPT_ASTRA: &str = include_str!("prompt/gpt-astra.txt");
const PROMPT_KIMI: &str = include_str!("prompt/kimi.txt");
const PROMPT_META: &str = include_str!("prompt/meta.txt");
const PROMPT_CODEX: &str = include_str!("prompt/codex.txt");
const PROMPT_TRINITY: &str = include_str!("prompt/trinity.txt");

/// `provider(model)` (system.ts:28-51) — the provider prompt for a model,
/// dispatched on `model.api.id` (and `model.providerID` for kimi).
pub fn provider(api_id: &str, provider_id: &str) -> Vec<String> {
    if api_id.contains("muse") {
        let name = if api_id.contains("muse-glimmer") {
            "Muse Glimmer"
        } else {
            "Muse Spark"
        };
        return vec![PROMPT_META.replace("{{MODEL_NAME}}", name)];
    }
    if api_id.contains("gpt-4") || api_id.contains("o1") || api_id.contains("o3") {
        return vec![PROMPT_BEAST.to_string()];
    }
    if api_id.contains("gpt") {
        if api_id.contains("gpt-6") {
            return vec![PROMPT_ASTRA.to_string()];
        }
        if api_id.contains("codex") {
            return vec![PROMPT_CODEX.to_string()];
        }
        return vec![PROMPT_GPT.to_string()];
    }
    if api_id.contains("gemini-") {
        return vec![PROMPT_GEMINI.to_string()];
    }
    if api_id.contains("claude") {
        return vec![PROMPT_ANTHROPIC.to_string()];
    }
    if api_id.to_lowercase().contains("trinity") {
        return vec![PROMPT_TRINITY.to_string()];
    }
    if api_id.to_lowercase().contains("kimi")
        || ["kimi-for-coding", "moonshotai", "moonshotai-cn"].contains(&provider_id)
    {
        return vec![PROMPT_KIMI.to_string()];
    }
    vec![PROMPT_DEFAULT.to_string()]
}

/// The model identity lines of `environment` (system.ts:74-104).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentModel {
    pub provider_id: String,
    pub id: String,
    pub api_id: String,
}

/// `InstanceState.context` + `instance.project.vcs === "git"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentContext {
    pub directory: String,
    pub worktree: String,
    pub vcs: bool,
}

/// `Reference.Service.list()` entries — only those with a description are
/// rendered (system.ts:71-73).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentReference {
    pub name: String,
    pub path: String,
    pub description: Option<String>,
}

/// `process.platform` for the current platform ("linux", "darwin", "win32").
pub fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// `new Date(now).toDateString()` — e.g. `"Thu Sep 17 2026"` (local time).
pub fn to_date_string(now_ms: u64) -> String {
    use chrono::TimeZone;
    let local = chrono::Local
        .timestamp_millis_opt(now_ms as i64)
        .single()
        .or_else(|| chrono::Local.timestamp_millis_opt(0).single())
        .expect("a valid timestamp");
    local.format("%a %b %-d %Y").to_string()
}

/// `environment(model)` (system.ts:69-105).
pub fn environment(
    model: &EnvironmentModel,
    ctx: &EnvironmentContext,
    references: Vec<EnvironmentReference>,
    now_ms: u64,
) -> Vec<String> {
    let mut out = vec![[
        format!(
            "You are powered by the model named {}. The exact model ID is {}/{}",
            model.api_id, model.provider_id, model.api_id
        ),
        "Here is some useful information about the environment you are running in:".to_string(),
        "<env>".to_string(),
        format!("  Working directory: {}", ctx.directory),
        format!("  Workspace root folder: {}", ctx.worktree),
        format!(
            "  Is directory a git repo: {}",
            if ctx.vcs { "yes" } else { "no" }
        ),
        format!("  Platform: {}", platform()),
        format!("  Today's date: {}", to_date_string(now_ms)),
        "</env>".to_string(),
    ]
    .join("\n")];
    let mut references: Vec<EnvironmentReference> = references
        .into_iter()
        .filter(|reference| reference.description.is_some())
        .collect();
    references.sort_by(|a, b| a.name.cmp(&b.name));
    if !references.is_empty() {
        let mut lines = vec![
            "Project references provide additional directories that can be accessed when relevant."
                .to_string(),
            "<available_references>".to_string(),
        ];
        for reference in &references {
            lines.push("  <reference>".to_string());
            lines.push(format!("    <name>{}</name>", reference.name));
            lines.push(format!("    <path>{}</path>", reference.path));
            if let Some(description) = &reference.description {
                lines.push(format!("    <description>{description}</description>"));
            }
            lines.push("  </reference>".to_string());
        }
        lines.push("</available_references>".to_string());
        out.push(lines.join("\n"));
    }
    out
}

/// `Skill.fmt` verbose block (skill/index.ts:321-345).
pub fn skill_fmt_verbose(list: &[SkillInfo]) -> String {
    let described: Vec<&SkillInfo> = list
        .iter()
        .filter(|skill| skill.description.is_some())
        .collect();
    if described.is_empty() {
        return "No skills are currently available.".to_string();
    }
    let mut sorted = described;
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut lines = vec!["<available_skills>".to_string()];
    for skill in sorted {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", skill.name));
        lines.push(format!(
            "    <description>{}</description>",
            skill.description.clone().unwrap_or_default()
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_html(&skill.location)
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

/// `escapeHtml` (util/html.ts).
fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// `skills(agent)` (system.ts:107-119): `None` when the `skill` permission
/// is disabled for the agent.
pub fn skills(agent: &AgentInfo, available: &[SkillInfo]) -> Option<String> {
    if disabled(&["skill"], &agent.permission).contains("skill") {
        return None;
    }
    Some(
        [
            "Skills provide specialized instructions and workflows for specific tasks.".to_string(),
            "Use the skill tool to load a skill when a task matches its description.".to_string(),
            skill_fmt_verbose(available),
        ]
        .join("\n"),
    )
}

/// One `mcp.instructions()` entry (M5 uses an empty MCP seam).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpInstruction {
    pub name: String,
    pub tools: Vec<String>,
    pub instructions: String,
}

/// `mcp(agent, permission)` (system.ts:121-137).
pub fn mcp(
    agent: &AgentInfo,
    permission: Option<&PermissionV1Ruleset>,
    items: &[McpInstruction],
) -> Option<String> {
    let empty = PermissionV1Ruleset::new();
    let ruleset = merge(&[&agent.permission, permission.unwrap_or(&empty)]);
    let instructions: Vec<&McpInstruction> = items
        .iter()
        .filter(|item| {
            if item.tools.is_empty() {
                return true;
            }
            let tools: Vec<&str> = item.tools.iter().map(|t| t.as_str()).collect();
            disabled(&tools, &ruleset).len() < item.tools.len()
        })
        .collect();
    if instructions.is_empty() {
        return None;
    }
    let mut lines = vec!["<mcp_instructions>".to_string()];
    for item in instructions {
        lines.push(format!("  <server name=\"{}\">", item.name));
        for line in item.instructions.split('\n') {
            lines.push(format!("    {line}"));
        }
        lines.push("  </server>".to_string());
    }
    lines.push("</mcp_instructions>".to_string());
    Some(lines.join("\n"))
}

/// The `Skill` discovery inputs are injected by the caller; this alias keeps
/// the module map (`system.rs` owns the SystemPrompt port) honest.
pub use crate::tool::skill::SkillInfo as SystemSkillInfo;

#[allow(dead_code)]
fn _unused_path_witness(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use alforria_schema::permission_v1::{PermissionV1Action, PermissionV1Rule};

    fn agent_with(permission: PermissionV1Ruleset) -> AgentInfo {
        AgentInfo {
            name: "build".to_string(),
            description: None,
            mode: crate::tool::def::AgentMode::Primary,
            native: None,
            hidden: None,
            top_p: None,
            temperature: None,
            color: None,
            permission,
            model: None,
            variant: None,
            prompt: None,
            options: Default::default(),
            steps: None,
        }
    }

    #[test]
    fn provider_dispatch_table() {
        assert_eq!(
            provider("claude-sonnet-4-5", "anthropic"),
            vec![PROMPT_ANTHROPIC.to_string()]
        );
        assert_eq!(provider("gpt-4o", "openai"), vec![PROMPT_BEAST.to_string()]);
        assert_eq!(
            provider("o3-mini", "openai"),
            vec![PROMPT_BEAST.to_string()]
        );
        assert_eq!(provider("gpt-6", "openai"), vec![PROMPT_ASTRA.to_string()]);
        assert_eq!(
            provider("gpt-5-codex", "openai"),
            vec![PROMPT_CODEX.to_string()]
        );
        assert_eq!(provider("gpt-5", "openai"), vec![PROMPT_GPT.to_string()]);
        assert_eq!(
            provider("gemini-2.5-pro", "models"),
            vec![PROMPT_GEMINI.to_string()]
        );
        assert_eq!(
            provider("claude-opus-4", "anthropic"),
            vec![PROMPT_ANTHROPIC.to_string()]
        );
        assert_eq!(
            provider("Trinity", "anthropic"),
            vec![PROMPT_TRINITY.to_string()]
        );
        assert_eq!(
            provider("kimi-k2", "moonshot"),
            vec![PROMPT_KIMI.to_string()]
        );
        assert_eq!(provider("whatever", "x"), vec![PROMPT_DEFAULT.to_string()]);
    }

    #[test]
    fn muse_gets_model_name_substitution() {
        let glimmer = provider("muse-glimmer", "meta");
        assert!(glimmer[0].contains("Muse Glimmer"));
        assert!(!glimmer[0].contains("{{MODEL_NAME}}"));
        let spark = provider("muse-spark", "meta");
        assert!(spark[0].contains("Muse Spark"));
        // Both occurrences are replaced.
        assert_eq!(spark[0].matches("Muse Spark").count(), 2);
    }

    #[test]
    fn kimi_provider_ids() {
        assert_eq!(
            provider("some-model", "moonshotai"),
            vec![PROMPT_KIMI.to_string()]
        );
        assert_eq!(
            provider("some-model", "kimi-for-coding"),
            vec![PROMPT_KIMI.to_string()]
        );
        assert_eq!(
            provider("some-model", "moonshotai-cn"),
            vec![PROMPT_KIMI.to_string()]
        );
    }

    #[test]
    fn environment_env_block() {
        let model = EnvironmentModel {
            provider_id: "anthropic".to_string(),
            id: "claude-sonnet-4-5".to_string(),
            api_id: "claude-sonnet-4-5".to_string(),
        };
        let ctx = EnvironmentContext {
            directory: "/repo".to_string(),
            worktree: "/repo".to_string(),
            vcs: true,
        };
        let out = environment(&model, &ctx, Vec::new(), 1_000_000_000_000);
        assert_eq!(out.len(), 1);
        let expected = [
            "You are powered by the model named claude-sonnet-4-5. The exact model ID is anthropic/claude-sonnet-4-5",
            "Here is some useful information about the environment you are running in:",
            "<env>",
            "  Working directory: /repo",
            "  Workspace root folder: /repo",
            "  Is directory a git repo: yes",
            "  Platform: linux",
            &format!("  Today's date: {}", to_date_string(1_000_000_000_000)),
            "</env>",
        ]
        .join("\n");
        assert_eq!(out[0], expected);
    }

    #[test]
    fn environment_references_block() {
        let model = EnvironmentModel {
            provider_id: "x".to_string(),
            id: "y".to_string(),
            api_id: "y".to_string(),
        };
        let ctx = EnvironmentContext {
            directory: "/repo".to_string(),
            worktree: "/repo".to_string(),
            vcs: false,
        };
        let out = environment(
            &model,
            &ctx,
            vec![
                EnvironmentReference {
                    name: "zeta".to_string(),
                    path: "/a".to_string(),
                    description: Some("z docs".to_string()),
                },
                EnvironmentReference {
                    name: "alpha".to_string(),
                    path: "/b".to_string(),
                    description: Some("a docs".to_string()),
                },
                EnvironmentReference {
                    name: "hidden".to_string(),
                    path: "/c".to_string(),
                    description: None,
                },
            ],
            0,
        );
        assert_eq!(out.len(), 2);
        assert!(out[1].contains("Project references provide additional"));
        // Sorted by name; description-less entries are filtered out.
        let alpha = out[1].find("<name>alpha</name>").unwrap();
        let zeta = out[1].find("<name>zeta</name>").unwrap();
        assert!(alpha < zeta);
        assert!(!out[1].contains("hidden"));
    }

    #[test]
    fn skills_block_and_permission_gate() {
        let available = vec![SkillInfo {
            name: "deploy".to_string(),
            description: Some("deploys things".to_string()),
            location: "/skills/deploy</SKILL.md".to_string(),
            content: String::new(),
        }];
        let agent = agent_with(Vec::new());
        let text = skills(&agent, &available).unwrap();
        assert_eq!(
            text,
            [
                "Skills provide specialized instructions and workflows for specific tasks.",
                "Use the skill tool to load a skill when a task matches its description.",
                "<available_skills>\n  <skill>\n    <name>deploy</name>\n    <description>deploys things</description>\n    <location>/skills/deploy&lt;/SKILL.md</location>\n  </skill>\n</available_skills>",
            ]
                .join("\n")
        );

        // A bare `*` deny on skill disables the block.
        let denied = agent_with(vec![PermissionV1Rule {
            permission: "skill".to_string(),
            pattern: "*".to_string(),
            action: PermissionV1Action::Deny,
        }]);
        assert!(skills(&denied, &available).is_none());
    }

    #[test]
    fn skills_without_descriptions() {
        let agent = agent_with(Vec::new());
        let text = skills(
            &agent,
            &[SkillInfo {
                name: "x".to_string(),
                description: None,
                location: String::new(),
                content: String::new(),
            }],
        )
        .unwrap();
        assert!(text.ends_with("No skills are currently available."));
    }

    #[test]
    fn mcp_block() {
        let agent = agent_with(Vec::new());
        let items = vec![McpInstruction {
            name: "server".to_string(),
            tools: vec![],
            instructions: "line one\nline two".to_string(),
        }];
        let text = mcp(&agent, None, &items).unwrap();
        assert_eq!(
            text,
            "<mcp_instructions>\n  <server name=\"server\">\n    line one\n    line two\n  </server>\n</mcp_instructions>"
        );
        assert!(mcp(&agent, None, &[]).is_none());
    }

    #[test]
    fn mcp_fully_disabled_servers_are_hidden() {
        let agent = agent_with(Vec::new());
        let items = vec![McpInstruction {
            name: "server".to_string(),
            tools: vec!["bash".to_string()],
            instructions: "x".to_string(),
        }];
        // The session ruleset denies bash entirely -> hidden.
        let ruleset = vec![PermissionV1Rule {
            permission: "bash".to_string(),
            pattern: "*".to_string(),
            action: PermissionV1Action::Deny,
        }];
        assert!(mcp(&agent, Some(&ruleset), &items).is_none());
        // A permissive session keeps the block.
        let allow = vec![PermissionV1Rule {
            permission: "bash".to_string(),
            pattern: "*".to_string(),
            action: PermissionV1Action::Allow,
        }];
        assert!(mcp(&agent, Some(&allow), &items).is_some());
    }
}
