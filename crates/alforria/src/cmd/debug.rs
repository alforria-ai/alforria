//! cli/cmd/debug/* port — the 13 `debug` subcommands.
//!
//! `config`, `lsp`, `rg`, `file`, `scrap`, `skill`, `snapshot`, `agent`,
//! `startup`, `v2`, `info`, `paths`, `wait` (debug/index.ts).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::ArgMatches;
use serde_json::{json, Value};

use crate::error::{CliError, TypedError};
use crate::instance::Instance;
use crate::ui::Ui;

use alforria_core::tool::lsp::LspServer;
use alforria_core::tool::ripgrep::Ripgrep as _;

use super::agent::agent_json;

/// `Effect.sleep(Duration.days(1))` (debug/index.ts:41-47).
pub fn wait_duration() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}

fn start_marker() -> &'static Instant {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now)
}

/// Record process start (called from `main`).
pub fn mark_startup() {
    start_marker();
}

/// `performance.now()` — ms since process start.
pub fn startup_ms() -> f64 {
    start_marker().elapsed().as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------------------
// info / paths / wait / startup (debug/index.ts)
// ---------------------------------------------------------------------------

fn os_type() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        "windows" => "Windows_NT",
        other => other,
    }
}

fn os_release() -> String {
    #[cfg(target_os = "linux")]
    {
        unsafe {
            let mut uts: libc::utsname = std::mem::zeroed();
            if libc::uname(&mut uts) == 0 {
                let bytes: Vec<u8> = uts
                    .release
                    .iter()
                    .take_while(|byte| **byte != 0)
                    .map(|byte| u8::try_from(*byte).unwrap_or_default())
                    .collect();
                if let Ok(release) = String::from_utf8(bytes) {
                    return release;
                }
            }
        }
    }
    "unknown".to_string()
}

fn os_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => other,
    }
}

/// `debug info` (debug/index.ts:49-77).
pub fn info(ui: &mut Ui, config: &alforria_core::config::schema::Config, pure: bool) {
    ui.write_stdout(&format!(
        "alforria version: {}\n",
        env!("CARGO_PKG_VERSION")
    ));
    ui.write_stdout(&format!(
        "os: {} {} {}\n",
        os_type(),
        os_release(),
        os_arch()
    ));
    let term_program = std::env::var("TERM_PROGRAM").ok().filter(|v| !v.is_empty());
    let terminal = [
        term_program.map(|program| match std::env::var("TERM_PROGRAM_VERSION") {
            Ok(version) if !version.is_empty() => format!("{program} {version}"),
            _ => program,
        }),
        std::env::var("TERM").ok().filter(|v| !v.is_empty()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" / ");
    ui.write_stdout(&format!(
        "terminal: {}\n",
        if terminal.is_empty() {
            "unknown".to_string()
        } else {
            terminal
        }
    ));
    ui.write_stdout("plugins:\n");
    if pure {
        ui.write_stdout("external plugins disabled (--pure)\n");
        return;
    }
    let Some(plugins) = config.plugin.as_ref() else {
        ui.write_stdout("none\n");
        return;
    };
    if plugins.is_empty() {
        ui.write_stdout("none\n");
        return;
    }
    for plugin in plugins {
        let specifier = match plugin {
            alforria_core::config::schema::PluginSpec::Name(spec) => spec.clone(),
            alforria_core::config::schema::PluginSpec::Pair(spec, _) => spec.clone(),
        };
        ui.write_stdout(&format!("- {specifier}\n"));
    }
}

/// `debug paths` (debug/index.ts:79-86) — `Global.Path` entries, key padded
/// to 10 columns.
pub fn paths(ui: &mut Ui, global: &alforria_core::GlobalPaths) {
    let entries: [(&str, PathBuf); 9] = [
        ("home", global.home.clone()),
        ("data", global.data.clone()),
        ("bin", global.cache.join("bin")),
        ("log", global.data.join("log")),
        ("repos", global.data.join("repos")),
        ("cache", global.cache.clone()),
        ("config", global.config.clone()),
        ("state", global.state.clone()),
        ("tmp", std::env::temp_dir().join("alforria")),
    ];
    for (key, value) in entries {
        ui.write_stdout(&format!("{:<10} {}\n", key, value.display()));
    }
}

// ---------------------------------------------------------------------------
// rg / file / skill (ripgrep.ts, file.ts, skill.ts)
// ---------------------------------------------------------------------------

/// `debug rg files` (ripgrep.ts:15-45).
pub fn rg_files(
    ui: &mut Ui,
    directory: &Path,
    glob: Option<&str>,
    limit: Option<usize>,
) -> Result<(), TypedError> {
    let ripgrep = alforria_core::tool::ripgrep::RipgrepService;
    let files = ripgrep.glob(directory, glob.unwrap_or("**/*"), limit.unwrap_or(10_000));
    let paths = files
        .iter()
        .map(|file| file.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    ui.write_stdout(&format!("{paths}\n"));
    Ok(())
}

/// `debug rg search` (ripgrep.ts:47-79) — the core `GrepMatch` slice of the
/// wire `Match` shape.
pub fn rg_search(
    ui: &mut Ui,
    directory: &Path,
    pattern: &str,
    include: Option<&str>,
    limit: Option<usize>,
) -> Result<(), TypedError> {
    let ripgrep = alforria_core::tool::ripgrep::RipgrepService;
    let results = ripgrep
        .grep(directory, pattern, include, limit.unwrap_or(10_000))
        .map_err(CliError::new)?;
    let matches: Vec<Value> = results
        .iter()
        .map(|m| {
            json!({
                "entry": {"path": m.path, "type": "file"},
                "line": m.line,
                "text": m.text,
            })
        })
        .collect();
    ui.write_stdout(&format!(
        "{}\n",
        serde_json::to_string_pretty(&matches).unwrap_or_default()
    ));
    Ok(())
}

/// `mime-types` `lookup(p) || "application/octet-stream"`
/// (fs-util.ts:224-226) — the common subset.
pub fn mime_type(path: &Path) -> String {
    let mime = path
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .and_then(|ext| match ext.as_str() {
            "js" | "mjs" | "jsx" => Some("text/javascript"),
            "json" => Some("application/json"),
            "html" | "htm" => Some("text/html"),
            "css" => Some("text/css"),
            "md" | "markdown" => Some("text/markdown"),
            "txt" => Some("text/plain"),
            "xml" => Some("text/xml"),
            "csv" => Some("text/csv"),
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "svg" => Some("image/svg+xml"),
            "pdf" => Some("application/pdf"),
            "zip" => Some("application/zip"),
            "mp3" => Some("audio/mpeg"),
            "mp4" => Some("video/mp4"),
            _ => None,
        });
    mime.map(str::to_string)
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

/// `debug file search <query>` (file.ts:16-29).
pub fn file_search(ui: &mut Ui, directory: &Path, vcs: bool, query: &str) {
    let state = alforria_core::filesystem::FindState::build(directory, vcs);
    let results = alforria_core::filesystem::find(&state, query, None, None);
    ui.write_stdout(&format!("{}\n", results.join("\n")));
}

/// `debug file read <path>` (file.ts:31-50) — base64 content JSON.
pub fn file_read(ui: &mut Ui, directory: &Path, path: &str) -> Result<(), TypedError> {
    let target = directory.join(path);
    let bytes = std::fs::read(&target)
        .map_err(|err| TypedError::Cli(CliError::new(format!("Failed to read {path}: {err}"))))?;
    let encoded = crate::client::base64_encode(&bytes);
    ui.write_stdout(&format!(
        "{}\n",
        serde_json::to_string_pretty(&json!({
            "content": encoded,
            "encoding": "base64",
            "mime": mime_type(&target),
        }))
        .unwrap_or_default()
    ));
    Ok(())
}

/// `debug file list <path>` (file.ts:52-65) — directories first, both
/// halves path-sorted, directories `/`-suffixed.
pub fn file_list(ui: &mut Ui, directory: &Path, path: &str) -> Result<(), TypedError> {
    let target = directory.join(path);
    let entries = std::fs::read_dir(&target)
        .map_err(|err| TypedError::Cli(CliError::new(format!("Failed to list {path}: {err}"))))?;
    let mut out: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir();
        if !is_dir && !file_type.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        out.push((
            if is_dir { format!("{name}/") } else { name },
            if is_dir { "directory" } else { "file" }.to_string(),
        ));
    }
    out.sort_by(|a, b| match (a.1 == "directory", b.1 == "directory") {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.0.cmp(&b.0),
    });
    let entries: Vec<Value> = out
        .into_iter()
        .map(|(path, type_)| json!({"path": path, "type": type_}))
        .collect();
    ui.write_stdout(&format!(
        "{}\n",
        serde_json::to_string_pretty(&entries).unwrap_or_default()
    ));
    Ok(())
}

/// `debug skill` (skill.ts) — `Skill.Service.all()` as JSON.
pub fn skill(ui: &mut Ui, instance: &Instance) {
    let skills = alforria_core::tool::skill::SkillService::discover(
        &alforria_core::tool::skill::SkillDiscovery {
            directories: instance.config_dirs.clone(),
            paths: instance
                .config
                .skills
                .as_ref()
                .and_then(|skills| skills.paths.clone())
                .unwrap_or_default(),
            directory: instance.directory.clone(),
            worktree: instance.worktree.clone(),
            home: instance.paths.home.clone(),
        },
    );
    let mut all: Vec<_> = skills.all().into_iter().cloned().collect();
    all.sort_by(|a, b| a.name.cmp(&b.name));
    let values: Vec<Value> = all
        .iter()
        .map(|skill| {
            json!({
                "name": skill.name,
                "description": skill.description,
                "location": skill.location,
                "content": skill.content,
            })
        })
        .collect();
    ui.write_stdout(&format!(
        "{}\n",
        serde_json::to_string_pretty(&values).unwrap_or_default()
    ));
}

// ---------------------------------------------------------------------------
// debug agent (agent.ts + agent.handler.ts) — tool registry wiring
// ---------------------------------------------------------------------------

/// `Agent.Service` access for the tool registry — the registry knows the
/// full `Agent.Info` records; the tool system needs the reduced slice.
#[derive(Clone)]
struct RegistryAgents(alforria_core::AgentRegistry);

impl alforria_core::tool::def::Agents for RegistryAgents {
    fn get<'a>(
        &'a self,
        agent: &'a str,
    ) -> alforria_core::tool::def::BoxFuture<
        'a,
        Result<alforria_core::tool::def::AgentInfo, alforria_core::tool::error::ToolError>,
    > {
        let info = self.0.get(agent).cloned();
        Box::pin(async move {
            info.map(|info| alforria_core::tool::def::AgentInfo {
                name: info.name,
                description: info.description,
                mode: info.mode,
                permission: info.permission,
            })
            .ok_or_else(|| {
                alforria_core::tool::error::ToolError::Failed(format!("Unknown agent {agent}"))
            })
        })
    }

    fn list<'a>(
        &'a self,
    ) -> alforria_core::tool::def::BoxFuture<'a, Vec<alforria_core::tool::def::AgentInfo>> {
        let list = self
            .0
            .list()
            .into_iter()
            .map(|info| alforria_core::tool::def::AgentInfo {
                name: info.name,
                description: info.description,
                mode: info.mode,
                permission: info.permission,
            })
            .collect();
        Box::pin(async move { list })
    }
}

struct ReqwestMcpHttpClient;

impl alforria_core::tool::mcp_websearch::McpHttpClient for ReqwestMcpHttpClient {
    fn post<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(String, String)>,
        body: &'a str,
    ) -> alforria_core::tool::def::BoxFuture<
        'a,
        Result<
            alforria_core::tool::mcp_websearch::McpHttpResponse,
            alforria_core::tool::error::ToolError,
        >,
    > {
        Box::pin(async move {
            let client = reqwest::Client::new();
            let mut request = client.post(url).body(body.to_string());
            for (key, value) in headers {
                request = request.header(key, value);
            }
            let response = request
                .send()
                .await
                .map_err(|err| alforria_core::tool::error::ToolError::Failed(err.to_string()))?;
            let text = response
                .text()
                .await
                .map_err(|err| alforria_core::tool::error::ToolError::Failed(err.to_string()))?;
            Ok(alforria_core::tool::mcp_websearch::McpHttpResponse { body: text })
        })
    }
}

fn bool_env(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.is_empty() && value != "0")
}

fn experimental_env(name: &str) -> bool {
    let Ok(value) = std::env::var(name) else {
        return false;
    };
    let Some(rest) = value.strip_prefix("experimental:") else {
        return false;
    };
    rest.parse::<f64>().is_ok_and(|n| n > 0.0)
}

fn client_env() -> String {
    std::env::var("OPENCODE_CLIENT").unwrap_or_else(|_| "cli".to_string())
}

/// `backgroundSubagentsEnabled` — `Bool("background_subagents")` with the
/// `experimental` prefix accepted too.
fn background_subagents_enabled() -> bool {
    bool_env("OPENCODE_ENABLE_BACKGROUND_SUBAGENTS")
        || bool_env("OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS")
}

/// `registry.ts:240-249` — the builtin tool set, CLI build (no LSP
/// integration, no MCP servers, no question removal for `cli`).
fn tool_registry(
    instance: &Instance,
) -> Result<alforria_core::tool::registry::ToolRegistry, TypedError> {
    let truncate = Arc::new(
        alforria_core::tool::truncate::TruncateService::default_limits(
            instance.paths.data.join("tool-output"),
        ),
    );
    let agents: Arc<dyn alforria_core::tool::def::Agents> =
        Arc::new(RegistryAgents(instance.services.agents.clone()));
    let context = instance
        .services
        .instance_context(&instance.directory, None)
        .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
    let ops = alforria_core::session::task_ops::ProductionTaskOps::new(
        instance.services.sessions.clone(),
        instance.services.messages.clone(),
        instance.services.agents.clone(),
        context.clone(),
    );
    let task = alforria_core::tool::task::task_tool(
        truncate.clone(),
        agents.clone(),
        ops,
        instance
            .config
            .subagent_depth
            .map(|depth| depth.max(1) as usize)
            .unwrap_or(1),
        instance
            .config
            .experimental
            .as_ref()
            .and_then(|experimental| experimental.primary_tools.clone())
            .unwrap_or_default(),
        if background_subagents_enabled() {
            alforria_core::tool::task::BackgroundMode::Enabled
        } else {
            alforria_core::tool::task::BackgroundMode::Disabled
        },
        if background_subagents_enabled() {
            Some(
                alforria_core::session::background::BackgroundJobService::new(
                    instance.services.clock().clone(),
                ),
            )
        } else {
            None
        },
    );
    let ripgrep: Arc<dyn alforria_core::tool::ripgrep::Ripgrep> =
        Arc::new(alforria_core::tool::ripgrep::RipgrepService);
    let mut builtin = vec![
        alforria_core::tool::invalid::invalid_tool(truncate.clone(), agents.clone()),
        alforria_core::tool::question::question_tool(
            truncate.clone(),
            agents.clone(),
            instance.services.question.clone(),
        ),
        futures::executor::block_on(
            alforria_core::tool::shell::ShellTool::new(
                instance
                    .config
                    .shell
                    .clone()
                    .unwrap_or_else(|| "sh".to_string()),
                2 * 60 * 1000,
                truncate.clone(),
                Arc::new(alforria_core::tool::shell::TokioSpawner),
            )
            .def(agents.clone()),
        ),
        alforria_core::tool::read::read_tool(truncate.clone(), agents.clone(), None),
        alforria_core::tool::glob::glob_tool(truncate.clone(), agents.clone(), ripgrep.clone()),
        alforria_core::tool::grep::grep_tool(truncate.clone(), agents.clone(), ripgrep.clone()),
        alforria_core::tool::edit::edit_tool(truncate.clone(), agents.clone(), None, None, None),
        alforria_core::tool::write::write_tool(truncate.clone(), agents.clone(), None, None, None),
        task,
        alforria_core::tool::webfetch::webfetch_tool(
            truncate.clone(),
            agents.clone(),
            alforria_core::tool::webfetch::reqwest_client(),
        ),
        alforria_core::tool::todo::todo_tool(
            truncate.clone(),
            agents.clone(),
            Arc::new(
                alforria_core::tool::todo::TodoService::new(instance.services.storage.clone())
                    .with_events(instance.services.events.clone()),
            ),
        ),
        alforria_core::tool::websearch::websearch_tool(
            truncate.clone(),
            agents.clone(),
            Arc::new(ReqwestMcpHttpClient),
            alforria_core::tool::websearch::WebSearchFlags {
                exa: bool_env("OPENCODE_ENABLE_EXA")
                    || experimental_env("OPENCODE_EXPERIMENTAL_EXA"),
                parallel: bool_env("OPENCODE_ENABLE_PARALLEL")
                    || experimental_env("OPENCODE_EXPERIMENTAL_PARALLEL"),
            },
            Arc::new(alforria_core::tool::websearch::SystemYear),
            alforria_core::tool::websearch::WebSearchEnv::default(),
        ),
        alforria_core::tool::skill::skill_tool(
            truncate.clone(),
            agents.clone(),
            Arc::new(alforria_core::tool::skill::SkillService::discover(
                &alforria_core::tool::skill::SkillDiscovery {
                    directories: instance.config_dirs.clone(),
                    paths: instance
                        .config
                        .skills
                        .as_ref()
                        .and_then(|skills| skills.paths.clone())
                        .unwrap_or_default(),
                    directory: instance.directory.clone(),
                    worktree: instance.worktree.clone(),
                    home: instance.paths.home.clone(),
                },
            )),
            ripgrep.clone(),
        ),
        alforria_core::tool::apply_patch::apply_patch_tool(
            truncate.clone(),
            agents.clone(),
            None,
            None,
            None,
        ),
    ];
    let question_enabled = matches!(client_env().as_str(), "app" | "cli" | "desktop")
        || bool_env("OPENCODE_ENABLE_QUESTION_TOOL");
    if !question_enabled {
        builtin.remove(1);
    }
    alforria_core::tool::registry::ToolRegistry::new(
        builtin,
        Vec::new(),
        alforria_core::tool::registry::RuntimeFlags {
            client: client_env(),
            enable_question_tool: bool_env("OPENCODE_ENABLE_QUESTION_TOOL"),
            enable_exa: bool_env("OPENCODE_ENABLE_EXA"),
            enable_parallel: bool_env("OPENCODE_ENABLE_PARALLEL"),
            experimental_code_mode: false,
            experimental_lsp_tool: false,
            experimental_plan_mode: false,
        },
        agents,
    )
    .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))
}

/// `debug agent <name>` (agent.handler.ts:18-65) — the config detail JSON
/// with the resolved `tools` map. Tool execution (`--tool`) is TODO(C6):
/// it needs the session-engine tool context.
pub fn debug_agent(ui: &mut Ui, instance: &Instance, name: &str) -> Result<(), TypedError> {
    let Some(agent) = instance.services.agents.get(name) else {
        ui.write_stderr(&format!(
            "Agent {name} not found, run 'alforria agent list' to get an agent list\n"
        ));
        return Err(TypedError::Cli(CliError::with_exit_code("", 1)));
    };
    let registry = tool_registry(instance)?;
    let model = agent
        .model
        .clone()
        .or_else(|| {
            instance
                .config
                .model
                .as_deref()
                .map(alforria_core::session::agents::parse_model)
        })
        .unwrap_or(alforria_core::session::agents::AgentModel {
            provider_id: String::new(),
            model_id: String::new(),
        });
    let tools =
        futures::executor::block_on(registry.tools(alforria_core::tool::registry::ToolModel {
            provider_id: &model.provider_id,
            model_id: &model.model_id,
            agent: alforria_core::tool::def::AgentInfo {
                name: agent.name.clone(),
                description: agent.description.clone(),
                mode: agent.mode,
                permission: agent.permission.clone(),
            },
            permission: None,
        }));
    let disabled = alforria_core::tool::permission::disabled(
        &tools.iter().map(|tool| tool.id).collect::<Vec<&str>>(),
        &agent.permission,
    );
    let mut resolved = serde_json::Map::new();
    for tool in &tools {
        resolved.insert(
            tool.id.to_string(),
            Value::Bool(!disabled.contains(tool.id)),
        );
    }
    let mut out = agent_json(agent);
    out.as_object_mut()
        .expect("agent json is an object")
        .insert("tools".to_string(), Value::Object(resolved));
    ui.write_stdout(&format!(
        "{}\n",
        serde_json::to_string_pretty(&out).unwrap_or_default()
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

fn require_instance_directory() -> Result<PathBuf, TypedError> {
    std::env::current_dir().map_err(|err| TypedError::Unknown {
        raw: err.to_string(),
    })
}

fn lsp_service(instance: &Instance) -> Arc<alforria_core::lsp::LspService> {
    alforria_core::lsp::LspService::new(alforria_core::lsp::LspInput {
        lsp: instance.config.lsp.clone(),
        directory: instance.directory.clone(),
        worktree: instance.worktree.clone(),
        paths: instance.paths.clone(),
        events: Some(instance.services.events.clone()),
        flags: alforria_core::lsp::server::Flags::from_env(),
    })
}

fn vcs_is_git(instance: &Instance) -> bool {
    matches!(
        instance.location.project.vcs,
        Some(alforria_schema::project::ProjectVcs::Git)
    )
}

fn snapshot_service(instance: &Instance) -> Arc<dyn alforria_core::session::snapshot::Snapshot> {
    if !vcs_is_git(instance) || instance.config.snapshot == Some(false) {
        return Arc::new(alforria_core::session::snapshot::DisabledSnapshot);
    }
    alforria_core::session::snapshot::GitSnapshot::new(
        alforria_core::session::snapshot::GitSnapshotInput {
            directory: instance.directory.clone(),
            worktree: instance.worktree.clone(),
            project_id: instance.location.project.id.clone(),
            vcs_is_git: true,
            snapshot_enabled: true,
            data: instance.paths.data.clone(),
        },
    ) as Arc<dyn alforria_core::session::snapshot::Snapshot>
}

pub fn run(matches: &ArgMatches, ui: &mut Ui) -> Result<(), TypedError> {
    let Some(sub) = matches.subcommand() else {
        return Ok(());
    };
    match sub.0 {
        "wait" => {
            let runtime = super::runtime()?;
            runtime.block_on(async {
                tokio::time::sleep(wait_duration()).await;
            });
            Ok(())
        }
        "startup" => {
            ui.write_stdout(&format!("{}\n", startup_ms()));
            Ok(())
        }
        "paths" => {
            paths(ui, &alforria_core::GlobalPaths::from_env());
            Ok(())
        }
        "info" => {
            let instance = crate::instance::boot(None)?;
            info(ui, &instance.config, bool_env("OPENCODE_PURE"));
            Ok(())
        }
        "config" => {
            let instance = crate::instance::boot(None)?;
            let config = serde_json::to_string_pretty(&instance.config).unwrap_or_default();
            ui.write_stdout(&format!("{config}\n"));
            Ok(())
        }
        "scrap" => {
            let instance = crate::instance::boot(None)?;
            let registry = alforria_core::project::registry::ProjectRegistry::new(
                instance.services.storage.clone(),
                Arc::new(alforria_core::git::SubprocessGit),
                Arc::new(alforria_core::catalog::SystemClock),
                Arc::new(|_| {}),
            );
            let list = registry
                .list()
                .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
            ui.write_stdout(&format!(
                "{}\n",
                serde_json::to_string_pretty(&list).unwrap_or_default()
            ));
            Ok(())
        }
        "skill" => {
            let instance = crate::instance::boot(None)?;
            skill(ui, &instance);
            Ok(())
        }
        "v2" => {
            let instance = crate::instance::boot(None)?;
            v2(ui, &instance)
        }
        "agent" => {
            let agent_matches = sub.1;
            let name = agent_matches.get_one::<String>("name").expect("name");
            if agent_matches.get_one::<String>("tool").is_some() {
                return Err(TypedError::Cli(CliError::new(
                    "--tool execution is not supported yet",
                )));
            }
            let instance = crate::instance::boot(None)?;
            debug_agent(ui, &instance, name)
        }
        "snapshot" => {
            let snapshot_matches = sub.1;
            let snapshot_matches = snapshot_matches.subcommand().expect("snapshot subcommand");
            let instance = crate::instance::boot(None)?;
            let snapshot = snapshot_service(&instance);
            let runtime = super::runtime()?;
            match snapshot_matches.0 {
                "track" => {
                    let hash = runtime.block_on(async { snapshot.track().await });
                    ui.write_stdout(&format!("{}\n", hash.unwrap_or_else(|| "null".to_string())));
                    Ok(())
                }
                "patch" => {
                    let hash = snapshot_matches.1.get_one::<String>("hash").expect("hash");
                    let patch = runtime
                        .block_on(async { snapshot.patch(hash).await })
                        .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
                    let files = patch.files.clone();
                    ui.write_stdout(&format!(
                        "{}\n",
                        serde_json::to_string_pretty(&json!({
                            "hash": patch.hash,
                            "files": files,
                        }))
                        .unwrap_or_default()
                    ));
                    Ok(())
                }
                "diff" => {
                    let hash = snapshot_matches.1.get_one::<String>("hash").expect("hash");
                    let diff = runtime
                        .block_on(async { snapshot.diff(hash).await })
                        .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
                    ui.write_stdout(&format!("{diff}\n"));
                    Ok(())
                }
                _ => Ok(()),
            }
        }
        "lsp" => {
            let lsp_matches = sub.1.subcommand().expect("lsp subcommand");
            let instance = crate::instance::boot(None)?;
            let lsp = lsp_service(&instance);
            let runtime = super::runtime()?;
            match lsp_matches.0 {
                "diagnostics" => {
                    let file = lsp_matches.1.get_one::<String>("file").expect("file");
                    let diagnostics = runtime.block_on(async {
                        lsp.touch_file(file, Some(alforria_core::lsp::client::WaitMode::Full))
                            .await;
                        lsp.diagnostics_record().await
                    });
                    ui.write_stdout(&format!(
                        "{}\n",
                        serde_json::to_string_pretty(&diagnostics).unwrap_or_default()
                    ));
                    Ok(())
                }
                "symbols" => {
                    let query = lsp_matches.1.get_one::<String>("query").expect("query");
                    let results = runtime.block_on(async { lsp.workspace_symbol(query).await });
                    ui.write_stdout(&format!(
                        "{}\n",
                        serde_json::to_string_pretty(&results).unwrap_or_default()
                    ));
                    Ok(())
                }
                "document-symbols" => {
                    let uri = lsp_matches.1.get_one::<String>("uri").expect("uri");
                    let results = runtime.block_on(async { lsp.document_symbol(uri).await });
                    ui.write_stdout(&format!(
                        "{}\n",
                        serde_json::to_string_pretty(&results).unwrap_or_default()
                    ));
                    Ok(())
                }
                _ => Ok(()),
            }
        }
        "rg" => {
            let rg_matches = sub.1.subcommand().expect("rg subcommand");
            let directory = require_instance_directory()?;
            match rg_matches.0 {
                "files" => {
                    let files_matches = rg_matches.1;
                    rg_files(
                        ui,
                        &directory,
                        files_matches.get_one::<String>("glob").map(String::as_str),
                        files_matches.get_one::<usize>("limit").copied(),
                    )
                }
                "search" => {
                    let pattern = rg_matches
                        .1
                        .get_one::<String>("pattern")
                        .cloned()
                        .expect("pattern");
                    let glob = rg_matches
                        .1
                        .get_many::<String>("glob")
                        .map(|values| values.cloned().collect::<Vec<_>>())
                        .unwrap_or_default();
                    rg_search(
                        ui,
                        &directory,
                        &pattern,
                        glob.first().map(String::as_str),
                        rg_matches.1.get_one::<usize>("limit").copied(),
                    )
                }
                _ => Ok(()),
            }
        }
        "file" => {
            let file_matches = sub.1.subcommand().expect("file subcommand");
            let directory = require_instance_directory()?;
            match file_matches.0 {
                "search" => {
                    let query = file_matches
                        .1
                        .get_one::<String>("query")
                        .cloned()
                        .expect("query");
                    let vcs = std::fs::metadata(directory.join(".git"))
                        .map(|meta| meta.is_dir())
                        .is_ok();
                    file_search(ui, &directory, vcs, &query);
                    Ok(())
                }
                "read" => {
                    let path = file_matches
                        .1
                        .get_one::<String>("path")
                        .cloned()
                        .expect("path");
                    file_read(ui, &directory, &path)
                }
                "list" => {
                    let path = file_matches
                        .1
                        .get_one::<String>("path")
                        .cloned()
                        .expect("path");
                    file_list(ui, &directory, &path)
                }
                _ => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

/// `debug v2` (v2.ts) — the v2 catalog: providers, the default model and
/// the per-provider small-model table (smoke output over models.dev).
fn v2(ui: &mut Ui, instance: &Instance) -> Result<(), TypedError> {
    let catalog = crate::catalog::catalog_service();
    let providers = catalog.get().map_err(crate::error::core_error)?;
    let mut available: Vec<(&String, &alforria_core::catalog::Provider)> =
        providers.iter().collect();
    available.sort_by(|a, b| a.0.cmp(b.0));
    let ids: Vec<String> = available.iter().map(|(id, _)| id.to_string()).collect();
    let mut small = serde_json::Map::new();
    for id in &ids {
        small.insert(
            id.clone(),
            Value::String(
                providers
                    .get(id)
                    .and_then(|provider| {
                        provider
                            .models
                            .keys()
                            .next()
                            .map(|model| format!("{id}/{model}"))
                    })
                    .unwrap_or_default(),
            ),
        );
    }
    ui.write_stdout(&format!(
        "{}\n",
        serde_json::to_string_pretty(&json!({
            "providers": ids,
            "default": instance.config.model,
            "small": Value::Object(small),
        }))
        .unwrap_or_default()
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_is_one_day() {
        assert_eq!(wait_duration(), Duration::from_secs(24 * 60 * 60));
    }

    #[test]
    fn startup_ms_is_positive_and_growing() {
        mark_startup();
        let first = startup_ms();
        assert!(first >= 0.0);
        assert!(startup_ms() + 1.0 >= first, "{first}");
    }

    #[test]
    fn paths_pad_keys_to_ten_columns() {
        let (mut ui, captured) = Ui::capture(false);
        let global = alforria_core::GlobalPaths::resolve(PathBuf::from("/home/user"));
        paths(&mut ui, &global);
        let stdout = captured.stdout();
        assert!(stdout.contains("home       /home/user\n"), "{stdout}");
        assert!(stdout.contains("data      "), "{stdout}");
        assert!(stdout.contains("config    "), "{stdout}");
        assert!(stdout.contains("state     "), "{stdout}");
        assert!(stdout.contains("tmp       "), "{stdout}");
    }

    #[test]
    fn info_lines_without_plugins() {
        let (mut ui, captured) = Ui::capture(false);
        let config = serde_json::from_value(json!({})).unwrap();
        info(&mut ui, &config, false);
        let stdout = captured.stdout();
        assert!(stdout.starts_with("alforria version: "), "{stdout}");
        assert!(stdout.contains("\nos: "), "{stdout}");
        assert!(stdout.contains("terminal: "), "{stdout}");
        assert!(stdout.ends_with("plugins:\nnone\n"), "{stdout}");
    }

    #[test]
    fn info_pure_disables_plugins() {
        let (mut ui, captured) = Ui::capture(false);
        let config = serde_json::from_value(json!({})).unwrap();
        info(&mut ui, &config, true);
        assert!(captured
            .stdout()
            .ends_with("plugins:\nexternal plugins disabled (--pure)\n"));
    }

    #[test]
    fn mime_lookup_table() {
        assert_eq!(mime_type(Path::new("a/b.png")), "image/png");
        assert_eq!(mime_type(Path::new("note.md")), "text/markdown");
        assert_eq!(mime_type(Path::new("x.RS")), "application/octet-stream");
    }

    #[test]
    fn file_read_outputs_base64_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hi").unwrap();
        let (mut ui, captured) = Ui::capture(false);
        file_read(&mut ui, dir.path(), "a.txt").unwrap();
        let stdout = captured.stdout();
        assert!(stdout.contains("\"encoding\": \"base64\""), "{stdout}");
        assert!(stdout.contains("\"content\": \"aGk=\""), "{stdout}");
        assert!(stdout.contains("\"mime\": \"text/plain\""), "{stdout}");
    }

    #[test]
    fn file_list_sorts_directories_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("z.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        let (mut ui, captured) = Ui::capture(false);
        file_list(&mut ui, dir.path(), ".").unwrap();
        let stdout = captured.stdout();
        let entries: Vec<Value> = serde_json::from_str(stdout.trim()).unwrap();
        assert_eq!(entries[0], json!({"path": "sub/", "type": "directory"}));
        assert_eq!(entries[1], json!({"path": "a.txt", "type": "file"}));
        assert_eq!(entries[2], json!({"path": "z.txt", "type": "file"}));
    }

    #[test]
    fn rg_files_and_search() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "nothing\n").unwrap();
        let (mut ui, captured) = Ui::capture(false);
        rg_files(&mut ui, dir.path(), None, None).unwrap();
        let stdout = captured.stdout();
        assert!(stdout.contains("a.txt"), "{stdout}");
        assert!(stdout.contains("b.txt"), "{stdout}");

        let (mut ui, captured) = Ui::capture(false);
        rg_search(&mut ui, dir.path(), "hello", None, None).unwrap();
        let matches: Vec<Value> = serde_json::from_str(captured.stdout().trim()).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0]["entry"],
            json!({"path": "a.txt", "type": "file"})
        );
    }

    #[test]
    fn file_search_finds_by_query() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("apple.txt"), "").unwrap();
        std::fs::write(dir.path().join("banana.txt"), "").unwrap();
        let (mut ui, captured) = Ui::capture(false);
        file_search(&mut ui, dir.path(), false, "apple");
        let stdout = captured.stdout();
        assert!(stdout.contains("apple.txt"), "{stdout}");
        assert!(!stdout.contains("banana"), "{stdout}");
    }

    #[test]
    fn snapshot_disabled_track_prints_null() {
        // DisabledSnapshot: track -> None -> "null" (session store prints
        // the stringified result).
        let snapshot: Arc<dyn alforria_core::session::snapshot::Snapshot> =
            Arc::new(alforria_core::session::snapshot::DisabledSnapshot);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hash = runtime.block_on(async { snapshot.track().await });
        assert_eq!(hash, None);
    }

    #[test]
    fn lsp_diagnostics_without_servers_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let lsp = alforria_core::lsp::LspService::new(alforria_core::lsp::LspInput {
            lsp: None,
            directory: dir.path().to_path_buf(),
            worktree: dir.path().to_path_buf(),
            paths: alforria_core::GlobalPaths::resolve(dir.path().to_path_buf()),
            events: None,
            flags: alforria_core::lsp::server::Flags::from_env(),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let record = runtime.block_on(async {
            lsp.touch_file("a.rs", Some(alforria_core::lsp::client::WaitMode::Full))
                .await;
            lsp.diagnostics_record().await
        });
        assert!(record.is_empty());
        let symbols = runtime.block_on(async { lsp.workspace_symbol("query").await });
        assert!(symbols.is_empty());
        let document = runtime.block_on(async { lsp.document_symbol("file:///a.rs").await });
        assert!(document.is_empty());
    }

    #[test]
    fn bool_env_requires_truthy_value() {
        std::env::remove_var("OPENCODE_X_TEST");
        assert!(!bool_env("OPENCODE_X_TEST"));
        std::env::set_var("OPENCODE_X_TEST", "1");
        assert!(bool_env("OPENCODE_X_TEST"));
        std::env::set_var("OPENCODE_X_TEST", "0");
        assert!(!bool_env("OPENCODE_X_TEST"));
        std::env::remove_var("OPENCODE_X_TEST");
    }
}
