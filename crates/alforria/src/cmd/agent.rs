//! cli/cmd/agent.ts port — the `agent` command family: `create` (the
//! fully-flagged non-interactive path) and `list`.

use std::path::Path;

use alforria_core::session::agents::{parse_model, AgentModel, AgentRegistry};
use clap::ArgMatches;
use serde_json::{json, Value};

use crate::error::{CliError, TypedError};
use crate::ui::Ui;

// agent.ts:19-31 — permission keys (not raw tool names), also the deny-map
// source of `agent create`.
pub const AVAILABLE_PERMISSIONS: [&str; 11] = [
    "bash",
    "read",
    "edit",
    "glob",
    "grep",
    "webfetch",
    "task",
    "todowrite",
    "websearch",
    "lsp",
    "skill",
];

/// `agent.ts:35-56` `Agent.Info.mode` rendered for the CLI output.
pub fn agent_mode(mode: alforria_core::tool::def::AgentMode) -> &'static str {
    use alforria_core::tool::def::AgentMode;
    match mode {
        AgentMode::All => "all",
        AgentMode::Primary => "primary",
        AgentMode::Subagent => "subagent",
    }
}

/// CLI sort (agent.ts:240-245): native-first, then name ascending.
pub fn sort_agents(
    agents: Vec<alforria_core::session::agents::AgentInfo>,
) -> Vec<alforria_core::session::agents::AgentInfo> {
    let mut agents = agents;
    agents.sort_by(|a, b| {
        let (a_native, b_native) = (a.native.unwrap_or(false), b.native.unwrap_or(false));
        b_native.cmp(&a_native).then_with(|| a.name.cmp(&b.name))
    });
    agents
}

/// `JSON.stringify(ruleset, null, 2)` — the ruleset array, pretty-printed.
pub fn permission_json(permission: &alforria_schema::permission_v1::PermissionV1Ruleset) -> Value {
    serde_json::to_value(permission).unwrap_or(Value::Array(Vec::new()))
}

/// `agent list` (agent.ts:234-252): `{name} ({mode})` plus the
/// 2-space-indented permissions JSON, one agent per blank-line block.
pub fn list(ui: &mut Ui, registry: &AgentRegistry) {
    for agent in sort_agents(registry.list()) {
        ui.write_stdout(&format!("{} ({})\n", agent.name, agent_mode(agent.mode)));
        let permission =
            serde_json::to_string_pretty(&agent.permission).unwrap_or_else(|_| "[]".to_string());
        let indented = permission
            .lines()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        ui.write_stdout(&format!("{indented}\n"));
    }
}

// ---------------------------------------------------------------------------
// create (agent.ts:33-232) — fully non-interactive path only
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CreateArgs {
    pub path: Option<String>,
    pub description: Option<String>,
    pub mode: Option<String>,
    pub permissions: Option<String>,
    pub model: Option<String>,
}

impl CreateArgs {
    pub fn from_matches(matches: &ArgMatches) -> Self {
        CreateArgs {
            path: matches.get_one::<String>("path").cloned(),
            description: matches.get_one::<String>("description").cloned(),
            mode: matches.get_one::<String>("mode").cloned(),
            permissions: matches.get_one::<String>("permissions").cloned(),
            model: matches.get_one::<String>("model").cloned(),
        }
    }
}

/// The `Agent.generate` result (agent.ts:58-62).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedAgent {
    pub identifier: String,
    pub when_to_use: String,
    pub system_prompt: String,
}

/// Seam for the LLM generation behind `agent create` (agent.ts:348-435).
pub trait AgentGenerator {
    fn generate(
        &self,
        description: &str,
        model: Option<&AgentModel>,
    ) -> Result<GeneratedAgent, String>;
}

/// Production seam: the ported provider runtime does not expose
/// `generateObject` yet, so generation reports a failure the same way a
/// model-less TS instance does (agent.ts:132-135).
pub struct NoLlmGenerator;

impl AgentGenerator for NoLlmGenerator {
    fn generate(
        &self,
        _description: &str,
        _model: Option<&AgentModel>,
    ) -> Result<GeneratedAgent, String> {
        Err("no language model available".to_string())
    }
}

/// js-yaml scalar rules for a frontmatter value (gray-matter stringify):
/// plain when safe, single-quoted otherwise.
fn yaml_scalar(value: &str) -> String {
    let plain = !value.is_empty()
        && !value.starts_with(' ')
        && !value.starts_with('\t')
        && !value.ends_with(' ')
        && !value.ends_with('\t')
        && !value.contains('\n')
        && !value.contains(": ")
        && !value.contains(" #")
        && !value.starts_with([
            '#', '&', '*', '!', '%', '@', '`', '\'', '"', '{', '[', '-', '?', ':',
        ]);
    if plain {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "''"))
    }
}

/// `matter.stringify(systemPrompt, frontmatter)` — the gray-matter
/// frontmatter writer: `---\n{yaml}---\n{content}`. `denied` preserves
/// declaration order (`AVAILABLE_PERMISSIONS` order in TS).
pub fn agent_markdown(
    when_to_use: &str,
    mode: &str,
    denied: &[(String, String)],
    system_prompt: &str,
) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("description: {}\n", yaml_scalar(when_to_use)));
    out.push_str(&format!("mode: {}\n", mode));
    if !denied.is_empty() {
        out.push_str("permission:\n");
        for (key, action) in denied {
            out.push_str(&format!("  {}: {}\n", key, yaml_scalar(action)));
        }
    }
    out.push_str("---\n");
    out.push_str(system_prompt);
    out
}

/// `agent create` (agent.ts:61-230) — the fully-flagged non-interactive
/// path: all of `--path/--description/--mode/--permissions` given
/// (agent.ts:77). Interactive `@clack/prompts` flows are out of scope.
pub fn create(
    ui: &mut Ui,
    args: &CreateArgs,
    generator: &dyn AgentGenerator,
) -> Result<(), TypedError> {
    let usage = "--description, --mode, --permissions and --path are required";
    if args.path.as_deref().is_none_or(str::is_empty)
        || args.description.as_deref().is_none_or(str::is_empty)
        || args.mode.as_deref().is_none_or(str::is_empty)
        || args.permissions.is_none()
    {
        return Err(TypedError::Cli(CliError::new(format!(
            "Missing required flags: {usage}"
        ))));
    }
    let permissions = args.permissions.as_deref().expect("checked");
    let description = args.description.as_deref().expect("checked");
    let mode = args.mode.as_deref().expect("checked");

    let model = args.model.as_deref().map(parse_model);
    let generated = generator
        .generate(description, model.as_ref())
        .map_err(|err| {
            TypedError::Cli(CliError::with_exit_code(
                format!("LLM failed to generate agent: {err}"),
                1,
            ))
        })?;

    let selected: Vec<String> = if permissions.is_empty() {
        AVAILABLE_PERMISSIONS
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        permissions
            .split(',')
            .map(|tool| tool.trim().to_string())
            .collect()
    };

    // Deny anything not explicitly selected (agent.ts:186-192).
    let denied: Vec<(String, String)> = AVAILABLE_PERMISSIONS
        .iter()
        .filter(|permission| !selected.contains(&permission.to_string()))
        .map(|permission| (permission.to_string(), "deny".to_string()))
        .collect();

    let content = agent_markdown(
        &generated.when_to_use,
        mode,
        &denied,
        &generated.system_prompt,
    );
    let target = Path::new(args.path.as_deref().expect("checked")).join("agents");
    let file = target.join(format!("{}.md", generated.identifier));
    if file.exists() {
        return Err(TypedError::Cli(CliError::with_exit_code(
            format!("Agent file already exists: {}", file.display()),
            1,
        )));
    }
    std::fs::create_dir_all(&target).map_err(|err| {
        TypedError::Cli(CliError::new(format!(
            "Failed to create {}: {err}",
            target.display()
        )))
    })?;
    std::fs::write(&file, content).map_err(|err| {
        TypedError::Cli(CliError::new(format!(
            "Failed to write {}: {err}",
            file.display()
        )))
    })?;
    ui.write_stdout(&format!("{}\n", file.display()));
    Ok(())
}

pub fn run(matches: &ArgMatches, ui: &mut Ui) -> Result<(), TypedError> {
    match matches.subcommand_name() {
        Some("list") => {
            let instance = crate::instance::boot(None)?;
            list(ui, &instance.services.agents);
            Ok(())
        }
        Some("create") => {
            let args =
                CreateArgs::from_matches(matches.subcommand_matches("create").expect("create"));
            create(ui, &args, &NoLlmGenerator)
        }
        _ => unreachable!("agent requires a subcommand"),
    }
}

/// Used by `debug agent` for the tools map (agent.handler.ts:60-64).
pub fn agent_json(agent: &alforria_core::session::agents::AgentInfo) -> Value {
    let mut out = json!({
        "name": agent.name,
        "mode": agent_mode(agent.mode),
        "options": agent.options,
    });
    let object = out.as_object_mut().expect("object");
    if let Some(description) = &agent.description {
        object.insert("description".to_string(), json!(description));
    }
    if let Some(native) = agent.native {
        object.insert("native".to_string(), json!(native));
    }
    if let Some(hidden) = agent.hidden {
        object.insert("hidden".to_string(), json!(hidden));
    }
    if let Some(top_p) = agent.top_p {
        object.insert("topP".to_string(), json!(top_p));
    }
    if let Some(temperature) = agent.temperature {
        object.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(color) = &agent.color {
        object.insert("color".to_string(), json!(color));
    }
    if let Some(model) = &agent.model {
        object.insert(
            "model".to_string(),
            json!({"modelID": model.model_id, "providerID": model.provider_id}),
        );
    }
    if let Some(variant) = &agent.variant {
        object.insert("variant".to_string(), json!(variant));
    }
    if let Some(prompt) = &agent.prompt {
        object.insert("prompt".to_string(), json!(prompt));
    }
    if let Some(steps) = agent.steps {
        object.insert("steps".to_string(), json!(steps));
    }
    object.insert("permission".to_string(), permission_json(&agent.permission));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use alforria_core::session::agents::{AgentInfo, AgentRegistryInput};
    use alforria_core::tool::def::AgentMode;
    use alforria_schema::permission_v1::PermissionV1Ruleset;

    fn agent(name: &str, mode: AgentMode, native: Option<bool>) -> AgentInfo {
        AgentInfo {
            name: name.to_string(),
            description: None,
            mode,
            native,
            hidden: None,
            top_p: None,
            temperature: None,
            color: None,
            permission: PermissionV1Ruleset::new(),
            model: None,
            variant: None,
            prompt: None,
            options: BTreeMap::new(),
            steps: None,
        }
    }

    #[test]
    fn list_sorts_native_first_then_name() {
        let mut agents = vec![
            agent("build", AgentMode::Primary, None),
            agent("custom", AgentMode::All, Some(false)),
            agent("explore", AgentMode::Subagent, Some(true)),
        ];
        let sorted = sort_agents(std::mem::take(&mut agents));
        let names: Vec<&str> = sorted.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["explore", "build", "custom"]);
    }

    #[test]
    fn list_prints_name_mode_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let registry_input = AgentRegistryInput {
            worktree: dir.path().to_path_buf(),
            data_dir: dir.path().to_path_buf(),
            tmp_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            ..AgentRegistryInput::default()
        };
        let registry = AgentRegistry::new(&registry_input);
        let (mut ui, captured) = Ui::capture(false);
        super::list(&mut ui, &registry);
        let stdout = captured.stdout();
        assert!(
            stdout.starts_with("build (primary)\n  [\n    {\n"),
            "{stdout}"
        );
        assert!(stdout.contains("explore (subagent)\n  ["), "{stdout}");
    }

    struct FixedGenerator {
        calls: AtomicUsize,
    }

    impl AgentGenerator for FixedGenerator {
        fn generate(
            &self,
            _description: &str,
            _model: Option<&AgentModel>,
        ) -> Result<GeneratedAgent, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(GeneratedAgent {
                identifier: "reviewer".to_string(),
                when_to_use: "Reviews code".to_string(),
                system_prompt: "You review code.".to_string(),
            })
        }
    }

    fn create_args(path: &str) -> CreateArgs {
        CreateArgs {
            path: Some(path.to_string()),
            description: Some("code reviewer".to_string()),
            mode: Some("subagent".to_string()),
            permissions: Some("read,edit".to_string()),
            model: None,
        }
    }

    #[test]
    fn create_writes_frontmatter_file_and_prints_path() {
        let dir = tempfile::tempdir().unwrap();
        let generator = FixedGenerator {
            calls: AtomicUsize::new(0),
        };
        let (mut ui, captured) = Ui::capture(false);
        create(
            &mut ui,
            &create_args(dir.path().to_str().unwrap()),
            &generator,
        )
        .unwrap();
        let file = dir.path().join("agents").join("reviewer.md");
        let content = std::fs::read_to_string(&file).unwrap();
        let expected = format!(
            "---\ndescription: {}\nmode: {}\npermission:\n{}\n---\n{}",
            "Reviews code",
            "subagent",
            AVAILABLE_PERMISSIONS
                .iter()
                .filter(|p| **p != "read" && **p != "edit")
                .map(|p| format!("  {p}: deny"))
                .collect::<Vec<_>>()
                .join("\n"),
            "You review code."
        );
        assert_eq!(content, expected);
        assert_eq!(captured.stdout(), format!("{}\n", file.display()));
        assert_eq!(generator.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn create_empty_permissions_allows_everything() {
        let dir = tempfile::tempdir().unwrap();
        let mut args = create_args(dir.path().to_str().unwrap());
        args.permissions = Some(String::new());
        let (mut ui, _captured) = Ui::capture(false);
        create(
            &mut ui,
            &args,
            &FixedGenerator {
                calls: AtomicUsize::new(0),
            },
        )
        .unwrap();
        let content =
            std::fs::read_to_string(dir.path().join("agents").join("reviewer.md")).unwrap();
        assert!(!content.contains("deny"), "{content}");
        assert!(content.contains("mode: subagent\n---"), "{content}");
    }

    #[test]
    fn create_existing_file_exits_one() {
        let dir = tempfile::tempdir().unwrap();
        let (mut ui, _captured) = Ui::capture(false);
        create(
            &mut ui,
            &create_args(dir.path().to_str().unwrap()),
            &FixedGenerator {
                calls: AtomicUsize::new(0),
            },
        )
        .unwrap();
        let (mut ui, _captured) = Ui::capture(false);
        let err = create(
            &mut ui,
            &create_args(dir.path().to_str().unwrap()),
            &FixedGenerator {
                calls: AtomicUsize::new(0),
            },
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        let message = crate::error::format_error(&err).unwrap();
        assert!(message.contains("Agent file already exists"), "{message}");
    }

    #[test]
    fn create_requires_all_flags() {
        let (mut ui, _captured) = Ui::capture(false);
        let err = create(
            &mut ui,
            &CreateArgs {
                description: Some("d".to_string()),
                ..CreateArgs::default()
            },
            &FixedGenerator {
                calls: AtomicUsize::new(0),
            },
        )
        .unwrap_err();
        assert!(
            crate::error::format_error(&err)
                .unwrap()
                .contains("Missing required flags"),
            "{}",
            crate::error::format_error(&err).unwrap()
        );
    }

    #[test]
    fn create_generation_failure_exits_one() {
        struct Failing;
        impl AgentGenerator for Failing {
            fn generate(
                &self,
                _description: &str,
                _model: Option<&AgentModel>,
            ) -> Result<GeneratedAgent, String> {
                Err("boom".to_string())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (mut ui, _captured) = Ui::capture(false);
        let err = create(
            &mut ui,
            &create_args(dir.path().to_str().unwrap()),
            &Failing,
        )
        .unwrap_err();
        assert_eq!(
            crate::error::format_error(&err).unwrap(),
            "LLM failed to generate agent: boom"
        );
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn yaml_scalar_quotes_special_values() {
        assert_eq!(yaml_scalar("plan"), "plan");
        assert_eq!(yaml_scalar("a: b"), "'a: b'");
        assert_eq!(yaml_scalar("plan: it's"), "'plan: it''s'");
    }
}
