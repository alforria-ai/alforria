//! Tool registry — port of `tool/registry.ts` (spec M4.1).
//!
//! The registry owns the builtin/custom tool sets, the per-model visibility
//! matrix and the dynamic descriptions (task subagent list, code-mode
//! catalog). Plugin/custom tools (`{tool,tools}/*.{js,ts}` dynamic imports)
//! are not ported (JS imports); the registry keeps the `custom` slot for
//! later milestones.

use std::sync::Arc;

use alforria_schema::permission_v1::PermissionV1Action;

use crate::tool::def::{AgentInfo, AgentMode, Agents, BoxFuture, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::permission::{evaluate, Ruleset};

/// Runtime flags the registry consumes (runtime-flags.ts; `client` defaults
/// to `"cli"`).
#[derive(Debug, Clone)]
pub struct RuntimeFlags {
    pub client: String,
    pub enable_question_tool: bool,
    pub enable_exa: bool,
    pub enable_parallel: bool,
    pub experimental_code_mode: bool,
    pub experimental_lsp_tool: bool,
    pub experimental_plan_mode: bool,
}

impl Default for RuntimeFlags {
    fn default() -> Self {
        RuntimeFlags {
            client: "cli".to_string(),
            enable_question_tool: false,
            enable_exa: false,
            enable_parallel: false,
            experimental_code_mode: false,
            experimental_lsp_tool: false,
            experimental_plan_mode: false,
        }
    }
}

/// `webSearchEnabled` (registry.ts:58-65).
pub fn web_search_enabled(provider_id: &str, flags: &RuntimeFlags) -> bool {
    provider_id == "opencode"
        || provider_id == "opencode-go"
        || flags.enable_exa
        || flags.enable_parallel
}

/// Input to [`ToolRegistry::tools`] (registry.ts:81-87).
#[derive(Debug, Clone)]
pub struct ToolModel<'a> {
    pub provider_id: &'a str,
    pub model_id: &'a str,
    pub agent: AgentInfo,
    pub permission: Option<&'a Ruleset>,
}

/// `describeCodeMode` seam (M4.8): produces the MCP tool catalog
/// description for the `execute` tool, or `None`/empty when it must stay
/// hidden.
pub type CodeModeDescriber =
    Arc<dyn for<'a> Fn(&ToolModel<'a>) -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// Tool registry state (registry.ts:70-75): builtin tools first, custom
/// tools after, plus the `task`/`read` handles the session needs.
#[derive(Clone)]
pub struct ToolRegistry {
    builtin: Vec<ToolDef>,
    custom: Vec<ToolDef>,
    task: ToolDef,
    read: ToolDef,
    flags: RuntimeFlags,
    agents: Arc<dyn Agents>,
    code_mode: Option<CodeModeDescriber>,
}

fn find_tool<'a>(builtin: &'a [ToolDef], custom: &'a [ToolDef], id: &str) -> Option<&'a ToolDef> {
    builtin.iter().chain(custom.iter()).find(|t| t.id == id)
}

impl ToolRegistry {
    /// Assemble the registry. The builtin list must contain `task` and `read`
    /// (registry.ts requires both to init).
    pub fn new(
        builtin: Vec<ToolDef>,
        custom: Vec<ToolDef>,
        flags: RuntimeFlags,
        agents: Arc<dyn Agents>,
    ) -> Result<Self, ToolError> {
        let task = find_tool(&builtin, &custom, "task")
            .cloned()
            .ok_or_else(|| ToolError::Failed("registry requires a `task` tool".to_string()))?;
        let read = find_tool(&builtin, &custom, "read")
            .cloned()
            .ok_or_else(|| ToolError::Failed("registry requires a `read` tool".to_string()))?;
        Ok(ToolRegistry {
            builtin,
            custom,
            task,
            read,
            flags,
            agents,
            code_mode: None,
        })
    }

    /// The `execute` tool description seam (M4.8). `None` (the default)
    /// keeps `execute` hidden, exactly like a TS build without the dynamic
    /// code-mode import.
    pub fn with_code_mode(mut self, code_mode: Option<CodeModeDescriber>) -> Self {
        self.code_mode = code_mode;
        self
    }

    /// Tool ids: builtin then custom (registry.ts:261-263).
    pub fn ids(&self) -> Vec<String> {
        self.all().iter().map(|t| t.id.to_string()).collect()
    }

    /// All registered tools: builtin then custom (registry.ts:256-259).
    pub fn all(&self) -> Vec<&ToolDef> {
        self.builtin.iter().chain(self.custom.iter()).collect()
    }

    /// The `task` and `read` defs (registry.ts:342-345).
    pub fn named(&self) -> (&ToolDef, &ToolDef) {
        (&self.task, &self.read)
    }

    /// `describeTask` (registry.ts:265-278): subagents the requesting agent
    /// may delegate to, sorted by name.
    async fn describe_task(&self, agent: &AgentInfo) -> String {
        let items: Vec<AgentInfo> = self
            .agents
            .list()
            .await
            .into_iter()
            .filter(|item| item.mode != AgentMode::Primary)
            .collect();
        let mut filtered: Vec<AgentInfo> = items
            .into_iter()
            .filter(|item| {
                evaluate("task", &item.name, &[&agent.permission]).action
                    != PermissionV1Action::Deny
            })
            .collect();
        filtered.sort_by(|a, b| a.name.cmp(&b.name));
        let list = filtered
            .iter()
            .map(|item| {
                format!(
                    "- {}: {}",
                    item.name,
                    item.description
                        .as_deref()
                        .unwrap_or("This subagent should only be called manually by the user.")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("Available agent types and the tools they have access to:\n{list}")
    }

    /// `tools(model)` (registry.ts:291-340): visibility filter + dynamic
    /// descriptions. The `plugin.trigger("tool.definition", …)` hook is an
    /// M4 no-op.
    pub async fn tools(&self, model: ToolModel<'_>) -> Vec<ToolDef> {
        let model_id = model.model_id;
        let use_patch =
            model_id.contains("gpt-") && !model_id.contains("oss") && !model_id.contains("gpt-4");

        let filtered: Vec<&ToolDef> = self
            .all()
            .into_iter()
            .filter(|tool| {
                if tool.id == "websearch" {
                    return web_search_enabled(model.provider_id, &self.flags);
                }
                if tool.id == "apply_patch" {
                    return use_patch;
                }
                if tool.id == "edit" || tool.id == "write" {
                    return !use_patch;
                }
                true
            })
            .collect();

        let code_mode_description = if filtered.iter().any(|t| t.id == "execute") {
            self.describe_code_mode(&model).await
        } else {
            None
        };
        let code_mode_description = code_mode_description.filter(|d| !d.is_empty());

        let task_description = self.describe_task(&model.agent).await;

        let mut out = Vec::new();
        for tool in filtered {
            let mut description_parts = vec![tool.description.to_string()];
            if tool.id == "task" {
                description_parts.push(task_description.clone());
            }
            if tool.id == "execute" {
                match &code_mode_description {
                    Some(description) => description_parts.push(description.clone()),
                    // `execute` is only visible with a non-empty description.
                    None => continue,
                }
            }
            let description = description_parts
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            out.push(ToolDef {
                id: tool.id,
                description: description.into(),
                parameters: tool.parameters.clone(),
                format_validation_error: tool.format_validation_error.clone(),
                execute: Arc::clone(&tool.execute),
            });
        }
        out
    }

    async fn describe_code_mode(&self, model: &ToolModel<'_>) -> Option<String> {
        let describer = self.code_mode.as_ref()?;
        describer(model).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::def::BoxFuture;
    use crate::tool::error::ToolError;

    struct FixedAgents {
        agents: Vec<AgentInfo>,
        missing: bool,
    }

    impl Agents for FixedAgents {
        fn get<'a>(&'a self, _agent: &'a str) -> BoxFuture<'a, Result<AgentInfo, ToolError>> {
            Box::pin(async move {
                if self.missing {
                    return Err(ToolError::Failed("Unknown agent".to_string()));
                }
                Ok(AgentInfo {
                    name: "build".to_string(),
                    description: None,
                    mode: AgentMode::Primary,
                    permission: Ruleset::new(),
                })
            })
        }

        fn list<'a>(&'a self) -> BoxFuture<'a, Vec<AgentInfo>> {
            Box::pin(async move { self.agents.clone() })
        }
    }

    fn agent_info(name: &str) -> AgentInfo {
        AgentInfo {
            name: name.to_string(),
            description: None,
            mode: AgentMode::Primary,
            permission: Ruleset::new(),
        }
    }

    fn subagent(name: &str) -> AgentInfo {
        AgentInfo {
            name: name.to_string(),
            description: None,
            mode: AgentMode::Subagent,
            permission: Ruleset::new(),
        }
    }

    fn tools_of(ids: &[&'static str]) -> Vec<ToolDef> {
        ids.iter()
            .map(|id| ToolDef {
                id,
                description: format!("{id} description").into(),
                parameters: serde_json::json!({}),
                format_validation_error: None,
                execute: Arc::new(|_args, _ctx| {
                    panic!("unused in tests");
                }),
            })
            .collect()
    }

    /// The canonical builtin order (registry.ts:231-249) with flag-conditional
    /// slots expanded — executable documentation of the M4 target state.
    fn builtin_ids(flags: &RuntimeFlags) -> Vec<&'static str> {
        let mut ids = vec!["invalid"];
        if ["app", "cli", "desktop"].contains(&flags.client.as_str()) || flags.enable_question_tool
        {
            ids.push("question");
        }
        ids.extend([
            "bash",
            "read",
            "glob",
            "grep",
            "edit",
            "write",
            "task",
            "webfetch",
            "todowrite",
            "websearch",
            "skill",
            "apply_patch",
        ]);
        if flags.experimental_code_mode {
            ids.push("execute");
        }
        if flags.experimental_lsp_tool {
            ids.push("lsp");
        }
        if flags.experimental_plan_mode && flags.client == "cli" {
            ids.push("plan_exit");
        }
        ids
    }

    fn registry(
        builtin: Vec<ToolDef>,
        flags: RuntimeFlags,
        agents: Arc<dyn Agents>,
    ) -> ToolRegistry {
        ToolRegistry::new(builtin, Vec::new(), flags, agents).expect("valid registry")
    }

    #[test]
    fn builtin_order_table() {
        for flags in [
            RuntimeFlags::default(),
            RuntimeFlags {
                client: "app".to_string(),
                enable_question_tool: false,
                enable_exa: false,
                enable_parallel: false,
                experimental_code_mode: true,
                experimental_lsp_tool: true,
                experimental_plan_mode: true,
            },
            RuntimeFlags {
                client: "web".to_string(),
                enable_question_tool: true,
                enable_exa: false,
                enable_parallel: false,
                experimental_code_mode: false,
                experimental_lsp_tool: true,
                experimental_plan_mode: true,
            },
        ] {
            let ids = builtin_ids(&flags);
            let reg = registry(
                tools_of(&ids),
                flags.clone(),
                Arc::new(FixedAgents {
                    agents: vec![agent_info("build")],
                    missing: false,
                }),
            );
            assert_eq!(reg.ids(), ids);
        }
    }

    #[test]
    fn question_slot_follows_flags() {
        let ids = builtin_ids(&RuntimeFlags {
            client: "web".to_string(),
            enable_question_tool: false,
            enable_exa: false,
            enable_parallel: false,
            experimental_code_mode: false,
            experimental_lsp_tool: false,
            experimental_plan_mode: false,
        });
        assert!(!ids.contains(&"question"));

        let ids = builtin_ids(&RuntimeFlags {
            client: "web".to_string(),
            enable_question_tool: true,
            enable_exa: false,
            enable_parallel: false,
            experimental_code_mode: false,
            experimental_lsp_tool: false,
            experimental_plan_mode: false,
        });
        assert!(ids.contains(&"question"));
    }

    #[test]
    fn plan_exit_requires_cli_and_flag() {
        let flags = RuntimeFlags {
            client: "web".to_string(),
            experimental_plan_mode: true,
            ..Default::default()
        };
        assert!(!builtin_ids(&flags).contains(&"plan_exit"));

        let flags = RuntimeFlags {
            client: "cli".to_string(),
            experimental_plan_mode: true,
            ..Default::default()
        };
        assert!(builtin_ids(&flags).contains(&"plan_exit"));
    }

    #[test]
    fn custom_tools_come_after_builtin() {
        let flags = RuntimeFlags::default();
        let reg = registry(
            tools_of(&["invalid", "bash", "read", "task"]),
            flags.clone(),
            Arc::new(FixedAgents {
                agents: vec![agent_info("build")],
                missing: false,
            }),
        );
        assert_eq!(reg.ids(), vec!["invalid", "bash", "read", "task"]);

        let reg = ToolRegistry::new(
            tools_of(&["invalid", "bash", "read", "task"]),
            tools_of(&["my_plugin"]),
            flags,
            Arc::new(FixedAgents {
                agents: vec![agent_info("build")],
                missing: false,
            }),
        )
        .expect("valid registry");
        assert_eq!(
            reg.ids(),
            vec!["invalid", "bash", "read", "task", "my_plugin"]
        );
        let ids = reg.ids();
        assert_eq!(ids.first().map(String::as_str), Some("invalid"));
    }

    #[test]
    fn named_returns_task_and_read() {
        let reg = registry(
            tools_of(&["invalid", "bash", "read", "task"]),
            RuntimeFlags::default(),
            Arc::new(FixedAgents {
                agents: vec![agent_info("build")],
                missing: false,
            }),
        );
        let (task, read) = reg.named();
        assert_eq!(task.id, "task");
        assert_eq!(read.id, "read");
    }

    #[tokio::test]
    async fn tools_websearch_provider_matrix() {
        let base = ["invalid", "bash", "read", "task", "websearch"];
        let make = || tools_of(&base);
        let model = |provider: &'static str| ToolModel {
            provider_id: provider,
            model_id: "claude-4",
            agent: agent_info("build"),
            permission: None,
        };

        for (provider, expected) in [
            ("opencode", true),
            ("opencode-go", true),
            ("anthropic", false),
        ] {
            let reg = registry(
                make(),
                RuntimeFlags::default(),
                Arc::new(FixedAgents {
                    agents: vec![agent_info("build")],
                    missing: false,
                }),
            );
            let tools = reg.tools(model(provider)).await;
            let visible = tools.iter().any(|t| t.id == "websearch");
            assert_eq!(visible, expected, "provider {provider}");
        }

        for flag in ["exa", "parallel"] {
            let flags = if flag == "exa" {
                RuntimeFlags {
                    enable_exa: true,
                    ..Default::default()
                }
            } else {
                RuntimeFlags {
                    enable_parallel: true,
                    ..Default::default()
                }
            };
            let reg = registry(
                make(),
                flags,
                Arc::new(FixedAgents {
                    agents: vec![agent_info("build")],
                    missing: false,
                }),
            );
            let tools = reg.tools(model("anthropic")).await;
            assert!(tools.iter().any(|t| t.id == "websearch"), "{flag} flag");
        }
    }

    #[tokio::test]
    async fn tools_patch_model_matrix() {
        let base = ["invalid", "read", "edit", "write", "task", "apply_patch"];
        let make = || tools_of(&base);
        for (model_id, patch_visible) in [
            ("gpt-5o", true),
            ("gpt-4o", false),       // gpt-4 excluded
            ("gpt-oss-120b", false), // oss excluded
            ("claude-sonnet-4", false),
        ] {
            let reg = registry(
                make(),
                RuntimeFlags::default(),
                Arc::new(FixedAgents {
                    agents: vec![agent_info("build")],
                    missing: false,
                }),
            );
            let tools = reg
                .tools(ToolModel {
                    provider_id: "anthropic",
                    model_id,
                    agent: agent_info("build"),
                    permission: None,
                })
                .await;
            assert_eq!(
                tools.iter().any(|t| t.id == "apply_patch"),
                patch_visible,
                "model {model_id}"
            );
            assert_eq!(
                tools.iter().any(|t| t.id == "edit"),
                !patch_visible,
                "model {model_id}"
            );
            assert_eq!(
                tools.iter().any(|t| t.id == "write"),
                !patch_visible,
                "model {model_id}"
            );
        }
    }

    #[tokio::test]
    async fn tools_execute_requires_description() {
        let base = ["invalid", "read", "task", "execute"];
        let make = || tools_of(&base);

        // No code-mode describer -> execute hidden.
        let reg = registry(
            make(),
            RuntimeFlags::default(),
            Arc::new(FixedAgents {
                agents: vec![agent_info("build")],
                missing: false,
            }),
        );
        let tools = reg
            .tools(ToolModel {
                provider_id: "anthropic",
                model_id: "claude-4",
                agent: agent_info("build"),
                permission: None,
            })
            .await;
        assert!(!tools.iter().any(|t| t.id == "execute"));

        // Non-empty description -> visible with appended description.
        let mut reg = registry(
            make(),
            RuntimeFlags::default(),
            Arc::new(FixedAgents {
                agents: vec![agent_info("build")],
                missing: false,
            }),
        );
        reg = reg.with_code_mode(Some(Arc::new(|_model| {
            Box::pin(async move { Some("MCP catalog".to_string()) })
        })));
        let tools = reg
            .tools(ToolModel {
                provider_id: "anthropic",
                model_id: "claude-4",
                agent: agent_info("build"),
                permission: None,
            })
            .await;
        let execute = tools.iter().find(|t| t.id == "execute").unwrap();
        assert_eq!(execute.description, "execute description\nMCP catalog");

        // Empty description -> hidden again.
        let mut reg = registry(
            make(),
            RuntimeFlags::default(),
            Arc::new(FixedAgents {
                agents: vec![agent_info("build")],
                missing: false,
            }),
        );
        reg = reg.with_code_mode(Some(Arc::new(|_model| {
            Box::pin(async move { Some(String::new()) })
        })));
        let tools = reg
            .tools(ToolModel {
                provider_id: "anthropic",
                model_id: "claude-4",
                agent: agent_info("build"),
                permission: None,
            })
            .await;
        assert!(!tools.iter().any(|t| t.id == "execute"));
    }

    #[tokio::test]
    async fn tools_augments_task_description() {
        let agents = vec![
            agent_info("build"),
            {
                let mut a = subagent("zeta");
                a.description = Some("Zeta agent".to_string());
                a
            },
            subagent("alpha"),
            subagent("hidden"),
        ];
        let reg = registry(
            tools_of(&["invalid", "read", "task"]),
            RuntimeFlags::default(),
            Arc::new(FixedAgents {
                agents,
                missing: false,
            }),
        );
        // TS evaluates `task` delegation against the *requesting* agent's
        // ruleset: deny task:"hidden" there to hide the hidden subagent.
        let mut build = agent_info("build");
        build.permission = vec![alforria_schema::permission_v1::PermissionV1Rule {
            permission: "task".to_string(),
            pattern: "hidden".to_string(),
            action: PermissionV1Action::Deny,
        }];
        let tools = reg
            .tools(ToolModel {
                provider_id: "anthropic",
                model_id: "claude-4",
                agent: build,
                permission: None,
            })
            .await;
        let task = tools.iter().find(|t| t.id == "task").unwrap();
        assert_eq!(
            task.description,
            "task description\nAvailable agent types and the tools they have access to:\n- alpha: This subagent should only be called manually by the user.\n- zeta: Zeta agent"
        );
    }

    #[test]
    fn registry_requires_task_and_read() {
        let agents: Arc<dyn Agents> = Arc::new(FixedAgents {
            agents: vec![agent_info("build")],
            missing: false,
        });
        let err = ToolRegistry::new(
            tools_of(&["invalid", "read"]),
            Vec::new(),
            RuntimeFlags::default(),
            agents.clone(),
        );
        assert!(err.is_err());
        let err = ToolRegistry::new(
            tools_of(&["invalid", "task"]),
            Vec::new(),
            RuntimeFlags::default(),
            agents,
        );
        assert!(err.is_err());
    }
}
