//! v2 misc families (M6.7) — health, location, agent, model, provider,
//! command, skill and fs — port of `packages/server/src/handlers/{health,
//! location,agent,model,provider,command,skill,fs}.ts`.
//!
//! The catalog projection ports `core/src/plugin/models-dev.ts` (the
//! catalog transform + the integration registration that gates provider
//! availability). Under M6 no provider plugin runs, so `apiKey` is never
//! set: a provider is available when it declares no env vars, or when one
//! of its env vars is present in the process environment
//! (`connections.length`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as ExtractPath, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::Value;

use crate::error::{ApiError, ServerError};
use crate::middleware::auth::query_param;
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;

use super::util::{defect, envelope};

pub fn register(
    router: Router<Arc<ServerContext>>,
    method: &str,
    path: &'static str,
) -> (Router<Arc<ServerContext>>, bool) {
    let router = match (method, path) {
        ("GET", "/api/health") => router.route(path, get(health)),
        ("GET", "/api/location") => router.route(path, get(location)),
        ("GET", "/api/agent") => router.route(path, get(agent_list)),
        ("GET", "/api/model") => router.route(path, get(model_list)),
        ("GET", "/api/provider") => router.route(path, get(provider_list)),
        ("GET", "/api/provider/{providerID}") => router.route(path, get(provider_get)),
        ("GET", "/api/command") => router.route(path, get(command_list)),
        ("GET", "/api/skill") => router.route(path, get(skill_list)),
        ("GET", "/api/fs/read/{*path}") => router.route(path, get(fs_read)),
        ("GET", "/api/fs/list") => router.route(path, get(fs_list)),
        ("GET", "/api/fs/find") => router.route(path, get(fs_find)),
        _ => return (router, false),
    };
    (router, true)
}

fn json_ok(value: impl serde::Serialize) -> Response {
    let body = serde_json::to_string(&value).expect("serialization cannot fail");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("static response parts are valid")
}

// ---------------------------------------------------------------------------
// health / location
// ---------------------------------------------------------------------------

/// `health.get` (`handlers/health.ts:5-7`).
async fn health() -> Response {
    json_ok(serde_json::json!({ "healthy": true }))
}

/// `location.get` (`handlers/location.ts:4-17`) — `Location.Info` without a
/// data wrapper.
async fn location(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    Ok(json_ok(super::util::location_info(&location)?))
}

// ---------------------------------------------------------------------------
// agent (`handlers/agent.ts`)
// ---------------------------------------------------------------------------

/// `agent.list` — the V1 registry mapped onto the V2 wire shape.
async fn agent_list(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let agents = location
        .services
        .agents
        .list()
        .into_iter()
        .map(|agent| opencode_schema::agent::AgentInfo {
            id: agent.name.clone(),
            model: agent
                .model
                .as_ref()
                .map(|model| opencode_schema::model::ModelRef {
                    id: model.model_id.clone(),
                    provider_id: model.provider_id.clone(),
                    variant: agent.variant.clone(),
                }),
            request: opencode_schema::provider::ProviderRequest {
                headers: Default::default(),
                body: Default::default(),
            },
            system: agent.prompt.clone(),
            description: agent.description.clone(),
            mode: match agent.mode {
                opencode_core::AgentMode::Subagent => opencode_schema::agent::AgentMode::Subagent,
                opencode_core::AgentMode::Primary => opencode_schema::agent::AgentMode::Primary,
                opencode_core::AgentMode::All => opencode_schema::agent::AgentMode::All,
            },
            hidden: agent.hidden.unwrap_or(false),
            color: agent.color.clone(),
            steps: agent.steps.map(|steps| steps as u64),
            permissions: super::util::relabel_ruleset(&agent.permission),
        })
        .collect::<Vec<_>>();
    envelope(&location, agents)
}

// ---------------------------------------------------------------------------
// model / provider (catalog projection — `core/src/plugin/models-dev.ts`)
// ---------------------------------------------------------------------------

/// `provider.api` (models-dev.ts:164-170): npm packages select the `aisdk`
/// transport.
fn provider_api(item: &opencode_core::catalog::Provider) -> opencode_schema::provider::ProviderApi {
    match (&item.npm, &item.api) {
        (Some(npm), _) => opencode_schema::provider::ProviderApi::Aisdk {
            package: npm.clone(),
            url: item.api.clone(),
            settings: None,
        },
        _ => opencode_schema::provider::ProviderApi::Native {
            url: item.api.clone(),
            settings: Default::default(),
        },
    }
}

/// `available` (`core/src/catalog.ts:53-58`): `!disabled && (apiKey ||
/// connections || (integrationID unset && no integration))`. Under M6
/// apiKey is never set and integrations exist for providers with env vars;
/// a connection is an env var present in the process environment.
fn provider_available(item: &opencode_core::catalog::Provider) -> bool {
    if item.env.is_empty() {
        return true;
    }
    item.env
        .iter()
        .any(|name| !name.is_empty() && std::env::var_os(name).is_some())
}

fn catalog(ctx: &ServerContext) -> Result<opencode_core::catalog::Providers, ServerError> {
    (ctx.catalog)().map_err(defect)
}

fn provider_info(
    item: &opencode_core::catalog::Provider,
) -> opencode_schema::provider::ProviderInfo {
    opencode_schema::provider::ProviderInfo {
        id: item.id.clone(),
        integration_id: None,
        name: item.name.clone(),
        disabled: None,
        api: provider_api(item),
        request: opencode_schema::provider::ProviderRequest {
            headers: Default::default(),
            body: Default::default(),
        },
    }
}

/// `released` (models-dev.ts:12-15) — `Date.parse`, 0 when unparsable.
fn released(date: &str) -> f64 {
    parse_date_millis(date).unwrap_or(0.0)
}

/// `Date.parse(ISO date)` — the models.dev release dates are
/// `YYYY-MM-DD`; the time is midnight UTC.
fn parse_date_millis(date: &str) -> Option<f64> {
    let date = if date.ends_with('Z') {
        date.to_string()
    } else {
        format!("{date}T00:00:00.000Z")
    };
    // Chronological math without a time crate: parse `YYYY-MM-DDTHH:MM:SS`.
    let bytes = date.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let year = date.get(0..4)?.parse::<i64>().ok()?;
    let month = date.get(5..7)?.parse::<i64>().ok()?;
    let day = date.get(8..10)?.parse::<i64>().ok()?;
    let days = days_from_civil(year, month, day)? - days_from_civil(1970, 1, 1)?;
    Some((days * 86_400_000) as f64)
}

/// Days since the Unix epoch (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

/// `cost` (models-dev.ts:17-42) — the base tier plus the explicit tiers and
/// the context-over-200k tier.
fn model_cost(
    input: &Option<opencode_core::catalog::Cost>,
) -> Vec<opencode_schema::model::ModelCost> {
    use opencode_schema::model::{ModelCost, ModelCostCache, ModelCostTier, ModelCostTierType};
    let Some(input) = input else {
        return Vec::new();
    };
    let mut result = vec![ModelCost {
        tier: None,
        input: input.input,
        output: input.output,
        cache: ModelCostCache {
            read: input.cache_read.unwrap_or(0.0),
            write: input.cache_write.unwrap_or(0.0),
        },
    }];
    for tier in input.tiers.iter().flatten() {
        result.push(ModelCost {
            tier: Some(ModelCostTier {
                type_: ModelCostTierType::Context,
                size: tier.tier.size as i64,
            }),
            input: tier.input,
            output: tier.output,
            cache: ModelCostCache {
                read: tier.cache_read.unwrap_or(0.0),
                write: tier.cache_write.unwrap_or(0.0),
            },
        });
    }
    if let Some(over) = &input.context_over_200k {
        result.push(ModelCost {
            tier: Some(ModelCostTier {
                type_: ModelCostTierType::Context,
                size: 200_000,
            }),
            input: over.input,
            output: over.output,
            cache: ModelCostCache {
                read: over.cache_read.unwrap_or(0.0),
                write: over.cache_write.unwrap_or(0.0),
            },
        });
    }
    result
}

/// `mergeCost` (models-dev.ts:44-69) for the experimental modes.
fn merge_cost(
    base: Vec<opencode_schema::model::ModelCost>,
    override_: &Option<opencode_core::catalog::Cost>,
) -> Vec<opencode_schema::model::ModelCost> {
    let Some(over) = override_ else {
        return base;
    };
    let next = model_cost(&Some(over.clone()));
    let mut result = Vec::new();
    // The default (no-tier) entry merges left-to-right; keyed tiers merge
    // by `type:size`.
    let mut base_iter = base.into_iter();
    let base_default = base_iter
        .next()
        .unwrap_or(opencode_schema::model::ModelCost {
            tier: None,
            input: 0.0,
            output: 0.0,
            cache: opencode_schema::model::ModelCostCache {
                read: 0.0,
                write: 0.0,
            },
        });
    let mut next_iter = next.into_iter();
    let next_default = next_iter.next();
    let merged_default = match next_default {
        Some(next) => opencode_schema::model::ModelCost {
            tier: next.tier.or(base_default.tier),
            input: next.input,
            output: next.output,
            cache: opencode_schema::model::ModelCostCache {
                read: next.cache.read,
                write: next.cache.write,
            },
        },
        None => base_default,
    };
    result.push(merged_default);
    let tier_key = |item: &opencode_schema::model::ModelCost| {
        format!(
            "{}:{}",
            item.tier.as_ref().map(|_| "context").unwrap_or("base"),
            item.tier.as_ref().map_or(0, |t| t.size)
        )
    };
    let mut tiers: Vec<opencode_schema::model::ModelCost> = base_iter.collect();
    for item in next_iter {
        if let Some(existing) = tiers.iter_mut().find(|t| tier_key(t) == tier_key(&item)) {
            existing.input = item.input;
            existing.output = item.output;
            existing.cache = item.cache;
            if item.tier.is_some() {
                existing.tier = item.tier.clone();
            }
        } else {
            tiers.push(item);
        }
    }
    result.extend(tiers);
    result
}

/// `applyModel` + `projectModel` (models-dev.ts:79-115, catalog.ts:88-108).
fn project_model(
    item: &opencode_core::catalog::Provider,
    model: &opencode_core::catalog::Model,
    name: String,
    cost: Vec<opencode_schema::model::ModelCost>,
) -> opencode_schema::model::ModelInfo {
    use opencode_schema::model::{
        ModelApi, ModelCapabilities, ModelInfo, ModelLimit, ModelRequest, ModelStatus, ModelTime,
    };
    let provider_api = provider_api(item);
    let api = match &model.provider {
        Some(provider) if provider.npm.is_some() => ModelApi::Aisdk {
            id: model.id.clone(),
            package: provider.npm.clone().unwrap_or_default(),
            url: provider.api.clone(),
            settings: None,
        },
        Some(provider) => ModelApi::Native {
            id: model.id.clone(),
            url: provider.api.clone(),
            settings: Default::default(),
        },
        None => ModelApi::Native {
            id: model.id.clone(),
            url: None,
            settings: Default::default(),
        },
    };
    // `projectModel` — a native api without its own url inherits the
    // provider's (models-dev never emits model-level apis, so this is the
    // common branch).
    let api = match (&api, &provider_api) {
        (
            ModelApi::Native { id, url, settings },
            opencode_schema::provider::ProviderApi::Native { .. },
        ) if url.is_none() && settings.is_empty() => {
            let opencode_schema::provider::ProviderApi::Native { url, .. } = &provider_api else {
                unreachable!()
            };
            ModelApi::Native {
                id: id.clone(),
                url: url.clone(),
                settings: settings.clone(),
            }
        }
        _ => api,
    };
    ModelInfo {
        id: model.id.clone(),
        provider_id: item.id.clone(),
        family: model.family.clone(),
        name,
        api,
        capabilities: ModelCapabilities {
            tools: model.tool_call,
            input: model
                .modalities
                .as_ref()
                .map(|m| m.input.iter().map(format_modality).collect())
                .unwrap_or_default(),
            output: model
                .modalities
                .as_ref()
                .map(|m| m.output.iter().map(format_modality).collect())
                .unwrap_or_default(),
        },
        request: ModelRequest {
            headers: Default::default(),
            body: Default::default(),
            variant: None,
        },
        variants: Vec::new(),
        time: ModelTime {
            released: released(&model.release_date),
        },
        cost,
        status: match model.status {
            Some(opencode_core::catalog::CatalogModelStatus::Alpha) => ModelStatus::Alpha,
            Some(opencode_core::catalog::CatalogModelStatus::Beta) => ModelStatus::Beta,
            Some(opencode_core::catalog::CatalogModelStatus::Deprecated) => ModelStatus::Deprecated,
            None => ModelStatus::Active,
        },
        enabled: true,
        limit: ModelLimit {
            context: model.limit.context as i64,
            input: model.limit.input.map(|value| value as i64),
            output: model.limit.output as i64,
        },
    }
}

fn format_modality(modality: &opencode_core::catalog::Modality) -> String {
    match modality {
        opencode_core::catalog::Modality::Text => "text".to_string(),
        opencode_core::catalog::Modality::Audio => "audio".to_string(),
        opencode_core::catalog::Modality::Image => "image".to_string(),
        opencode_core::catalog::Modality::Video => "video".to_string(),
        opencode_core::catalog::Modality::Pdf => "pdf".to_string(),
    }
}

/// All catalog models: every models.dev model (plus experimental mode
/// variants), sorted by release time descending (`catalog.ts:108-118`).
fn catalog_models(
    providers: &opencode_core::catalog::Providers,
) -> Vec<opencode_schema::model::ModelInfo> {
    let mut models = Vec::new();
    for item in providers.values() {
        for model in item.models.values() {
            let base_cost = model_cost(&model.cost);
            models.push(project_model(item, model, model.name.clone(), base_cost));
            for (mode, options) in model
                .experimental
                .as_ref()
                .and_then(|e| e.modes.as_ref())
                .into_iter()
                .flatten()
            {
                let name = format!("{} {}", model.name, mode[..1].to_uppercase() + &mode[1..]);
                models.push(project_model(
                    item,
                    model,
                    name,
                    merge_cost(model_cost(&model.cost), &options.cost),
                ));
            }
        }
    }
    models.sort_by(|a, b| {
        b.time
            .released
            .partial_cmp(&a.time.released)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    models
}

/// `model.list` (`handlers/model.ts:4-11`) — `catalog.model.available()`.
async fn model_list(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let providers = catalog(&ctx)?;
    let available: std::collections::HashSet<String> = providers
        .values()
        .filter(|item| provider_available(item))
        .map(|item| item.id.clone())
        .collect();
    let models: Vec<_> = catalog_models(&providers)
        .into_iter()
        .filter(|model| available.contains(&model.provider_id) && model.enabled)
        .collect();
    envelope(&location, models)
}

/// `provider.list` (`handlers/provider.ts:6-13`).
async fn provider_list(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let providers = catalog(&ctx)?;
    let providers: Vec<_> = providers
        .values()
        .filter(|item| provider_available(item))
        .map(provider_info)
        .collect();
    envelope(&location, providers)
}

/// `provider.get` (`handlers/provider.ts:15-27`).
async fn provider_get(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    ExtractPath(provider_id): ExtractPath<String>,
) -> Result<Response, ServerError> {
    let providers = catalog(&ctx)?;
    let Some(item) = providers.get(&provider_id) else {
        return Err(ApiError::ProviderNotFound {
            provider_id: provider_id.clone(),
            message: format!("Provider not found: {provider_id}"),
        }
        .into());
    };
    envelope(&location, provider_info(item))
}

// ---------------------------------------------------------------------------
// command (`handlers/command.ts` + `core/src/config/plugin/command.ts`)
// ---------------------------------------------------------------------------

const PROMPT_INITIALIZE: &str = include_str!("command_initialize.txt");
const PROMPT_REVIEW: &str = include_str!("command_review.txt");

/// `command.list` — the built-in init/review commands plus the config
/// `commands` record (merged with the markdown-discovered ones by the M3
/// loader).
async fn command_list(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let directory = location.directory.display().to_string();
    let mut commands = vec![
        opencode_schema::command::CommandInfo {
            name: "init".to_string(),
            template: PROMPT_INITIALIZE.replace("${path}", &directory),
            description: Some("guided AGENTS.md setup".to_string()),
            agent: None,
            model: None,
            subtask: None,
        },
        opencode_schema::command::CommandInfo {
            name: "review".to_string(),
            template: PROMPT_REVIEW.replace("${path}", &directory),
            description: Some(
                "review changes [commit|branch|pr], defaults to uncommitted".to_string(),
            ),
            agent: None,
            model: None,
            subtask: Some(true),
        },
    ];
    let params = opencode_core::LoadParams::new(location.directory.clone())
        .paths(opencode_core::GlobalPaths::from_env());
    let (config, _) = match opencode_core::ConfigLoader::new().load(&params) {
        Ok(loaded) => loaded,
        // Config load failures degrade to the built-ins only (the v1
        // command surface treats a broken config as empty).
        Err(_) => return envelope(&location, commands),
    };
    for (name, command) in config.command.iter().flatten() {
        let model = command.model.as_deref().map(|model| {
            let (provider_id, rest) = model.split_once('/').unwrap_or(("", model));
            opencode_schema::model::ModelRef {
                id: rest.to_string(),
                provider_id: provider_id.to_string(),
                variant: command
                    .variant
                    .clone()
                    .filter(|_| rest.is_empty() || !rest.is_empty()),
            }
        });
        commands.push(opencode_schema::command::CommandInfo {
            name: name.clone(),
            template: command.template.clone(),
            description: command.description.clone(),
            agent: command.agent.clone(),
            model,
            subtask: command.subtask,
        });
    }
    envelope(&location, commands)
}

// ---------------------------------------------------------------------------
// skill (`handlers/skill.ts` + `core/src/config/plugin/skill.ts`)
// ---------------------------------------------------------------------------

/// `skill.list` — every config directory's `skill`/`skills` directories plus
/// the `skills` config entries (`core/src/config/plugin/skill.ts`), deduped
/// by name (later sources win).
async fn skill_list(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let params = opencode_core::LoadParams::new(location.directory.clone())
        .paths(opencode_core::GlobalPaths::from_env());
    let (config, directories) = match opencode_core::ConfigLoader::new().load(&params) {
        Ok(loaded) => loaded,
        Err(_) => return envelope(&location, Vec::<opencode_schema::skill::SkillInfo>::new()),
    };
    let mut sources: Vec<PathBuf> = Vec::new();
    for directory in &directories {
        sources.push(directory.join("skill"));
        sources.push(directory.join("skills"));
    }
    if let Some(entries) = config.skills.as_ref() {
        let items = entries
            .paths
            .iter()
            .flatten()
            .chain(entries.urls.iter().flatten());
        for item in items {
            // URL sources are remote — the M6 adapter reads local
            // directories only (recorded divergence).
            if item.starts_with("http://") || item.starts_with("https://") {
                continue;
            }
            let path = if let Some(rest) = item.strip_prefix("~/") {
                if let Ok(home) = std::env::var("HOME") {
                    PathBuf::from(home).join(rest)
                } else {
                    continue;
                }
            } else {
                location.directory.join(item)
            };
            sources.push(path);
        }
    }

    let mut skills: std::collections::HashMap<String, opencode_schema::skill::SkillInfo> =
        Default::default();
    for source in sources {
        for skill in load_skill_directory(&source) {
            skills.insert(skill.name.clone(), skill);
        }
    }
    let skills: Vec<_> = skills.into_values().collect();
    envelope(&location, skills)
}

/// `load` (`core/src/skill.ts:92-146`) — `{*.md,**/SKILL.md}` with
/// frontmatter `{name?, description?, slash?}`.
fn load_skill_directory(directory: &Path) -> Vec<opencode_schema::skill::SkillInfo> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_skill_files(directory, &mut files, 0);
    files.sort();
    let mut result = Vec::new();
    for file in files {
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Some((frontmatter, body)) = parse_markdown(&content) else {
            continue;
        };
        let name = match frontmatter.get("name").and_then(Value::as_str) {
            Some(name) => Some(name.to_string()),
            None => {
                // `dirname(filepath) === directory ? basename : undefined`
                // — nested SKILL.md files need a frontmatter name.
                if file.parent() == Some(directory) {
                    file.file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                } else {
                    None
                }
            }
        };
        let Some(name) = name else {
            continue;
        };
        result.push(opencode_schema::skill::SkillInfo {
            name,
            description: frontmatter
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            slash: frontmatter.get("slash").and_then(Value::as_bool),
            location: file.display().to_string(),
            content: body.trim().to_string(),
        });
    }
    result
}

fn collect_skill_files(dir: &Path, files: &mut Vec<PathBuf>, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            collect_skill_files(&path, files, depth + 1);
        } else if depth == 0 {
            // `{*.md,**/SKILL.md}` — direct children must be `*.md`
            // (including `SKILL.md` itself).
            if path.extension().is_some_and(|ext| ext == "md") {
                files.push(path);
            }
        } else if path.file_name().is_some_and(|name| name == "SKILL.md") {
            files.push(path);
        }
    }
}

/// A minimal frontmatter split — `---`-delimited YAML with `key: value`
/// lines (the skill frontmatter fields are flat scalars).
fn parse_markdown(content: &str) -> Option<(serde_json::Map<String, Value>, String)> {
    let content = content.strip_prefix("---\n")?;
    let (frontmatter, body) = content.split_once("\n---")?;
    let mut data = serde_json::Map::new();
    for line in frontmatter.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if value == "true" {
            data.insert(key.to_string(), Value::Bool(true));
        } else if value == "false" {
            data.insert(key.to_string(), Value::Bool(false));
        } else {
            data.insert(key.to_string(), Value::String(value.to_string()));
        }
    }
    Some((data, body.trim_start_matches('\n').to_string()))
}

// ---------------------------------------------------------------------------
// fs (`handlers/fs.ts`, `core/src/filesystem.ts`)
// ---------------------------------------------------------------------------

/// `FSUtil.contains` — a lexical child check.
fn contains(root: &Path, target: &Path) -> bool {
    target.starts_with(root)
}

fn resolve_in_location(
    location: &LocationContext,
    input: Option<&str>,
) -> Result<PathBuf, ServerError> {
    let base = &location.directory;
    let absolute = match input {
        Some(input) if !input.is_empty() => {
            let candidate = PathBuf::from(input);
            if candidate.is_absolute() {
                candidate
            } else {
                base.join(candidate)
            }
        }
        _ => base.clone(),
    };
    if !contains(base, &absolute) {
        return Err(defect("Path escapes the location"));
    }
    let real = match std::fs::canonicalize(&absolute) {
        Ok(real) => real,
        Err(err) => return Err(defect(format!("realpath {absolute:?}: {err}"))),
    };
    let root = std::fs::canonicalize(base).unwrap_or_else(|_| base.clone());
    if !contains(&root, &real) {
        return Err(defect("Path escapes the location"));
    }
    Ok(absolute)
}

/// `fs.read` (`handlers/fs.ts:7-16`) — raw bytes with the mime type as
/// content type (no location envelope).
async fn fs_read(
    axum::Extension(location): axum::Extension<LocationContext>,
    ExtractPath(path): ExtractPath<String>,
) -> Result<Response, ServerError> {
    let target = resolve_in_location(&location, Some(&path))?;
    let real = std::fs::canonicalize(&target).map_err(|err| defect(format!("{err}")))?;
    if !real.is_file() {
        return Err(defect("Path is not a file"));
    }
    let bytes = std::fs::read(&real).map_err(|err| defect(format!("{err}")))?;
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            crate::routes::v1::global_control::mime_type(&real),
        )
        .body(Body::from(bytes))
        .map_err(defect)
}

/// `fs.list` (`core/src/filesystem.ts:104-120`) — direct children,
/// directories first, each directory suffixed with `/`.
async fn fs_list(
    axum::Extension(location): axum::Extension<LocationContext>,
    uri: axum::http::Uri,
) -> Result<Response, ServerError> {
    let input = query_param(uri.query(), "path").filter(|value| !value.is_empty());
    let target = resolve_in_location(&location, input.as_deref())?;
    let real = std::fs::canonicalize(&target).map_err(|err| defect(format!("{err}")))?;
    if !real.is_dir() {
        return Err(defect("Path is not a directory"));
    }
    let mut entries: Vec<(String, bool)> = Vec::new();
    for entry in std::fs::read_dir(&target).map_err(|err| defect(format!("{err}")))? {
        let entry = entry.map_err(|err| defect(format!("{err}")))?;
        let file_type = entry.file_type().map_err(|err| defect(format!("{err}")))?;
        if file_type.is_file() {
            entries.push((entry.file_name().to_string_lossy().into_owned(), false));
        } else if file_type.is_dir() {
            entries.push((entry.file_name().to_string_lossy().into_owned(), true));
        }
    }
    entries.sort_by(|a, b| match (a.1, b.1) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.0.cmp(&b.0),
    });
    // `path.relative(location.directory, entry)` + `path.sep` for
    // directories — relative to the location root, not the listed
    // directory (`core/src/filesystem.ts:100-112`).
    let base = target
        .strip_prefix(&location.directory)
        .map(|relative| {
            let joined = relative.display().to_string();
            if joined.is_empty() {
                String::new()
            } else {
                format!("{joined}/")
            }
        })
        .unwrap_or_default();
    let data: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|(name, is_dir)| {
            let suffix = if is_dir { "/" } else { "" };
            serde_json::json!({
                "path": format!("{base}{name}{suffix}"),
                "type": if is_dir { "directory" } else { "file" },
            })
        })
        .collect();
    envelope(&location, data)
}

/// `fs.find` — the ripgrep-layer fuzzy fallback over a directory walk
/// (`core/src/filesystem/search.ts:20-66`); the fff native index is M7.
async fn fs_find(
    axum::Extension(location): axum::Extension<LocationContext>,
    uri: axum::http::Uri,
) -> Result<Response, ServerError> {
    let Some(query) = query_param(uri.query(), "query") else {
        return Err(super::util::query_error("Expected a string, got undefined"));
    };
    let type_raw = query_param(uri.query(), "type");
    let type_filter = match type_raw.as_deref() {
        Some(t @ ("file" | "directory")) => Some(t),
        Some(other) => {
            return Err(super::util::query_error(format!(
                "Expected \"file\" or \"directory\", got {other:?}"
            )))
        }
        None => None,
    };
    let limit = match query_param(uri.query(), "limit") {
        Some(raw) => {
            let value = raw
                .parse::<i64>()
                .map_err(|_| super::util::query_error(format!("Expected a number, got {raw:?}")))?;
            if value < 1 {
                return Err(super::util::query_error(format!(
                    "Expected a positive number, got {raw:?}"
                )));
            }
            value as usize
        }
        None => 50,
    };

    let mut files: Vec<String> = Vec::new();
    let mut directories: Vec<String> = Vec::new();
    collect_paths(
        &location.directory,
        &location.directory,
        &mut files,
        &mut directories,
    );
    let items: Vec<String> = match type_filter {
        Some("file") => files,
        Some("directory") => directories,
        _ => {
            let mut both = files;
            both.extend(directories);
            both
        }
    };
    let query_chars: Vec<char> = query.to_lowercase().chars().collect();
    let matches: Vec<serde_json::Value> = items
        .into_iter()
        .filter(|path| {
            let lowered: Vec<char> = path.to_lowercase().chars().collect();
            let mut i = 0;
            for c in lowered {
                if i < query_chars.len() && query_chars[i] == c {
                    i += 1;
                }
            }
            i == query_chars.len()
        })
        .take(limit)
        .map(|path| {
            let is_dir = path.ends_with('/');
            serde_json::json!({
                "path": path,
                "type": if is_dir { "directory" } else { "file" },
            })
        })
        .collect();
    envelope(&location, matches)
}

fn collect_paths(base: &Path, dir: &Path, files: &mut Vec<String>, directories: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let relative = path.strip_prefix(base).ok();
        let Some(relative) = relative else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            directories.push(format!("{relative}/"));
            collect_paths(base, &entry.path(), files, directories);
        } else {
            files.push(relative);
        }
    }
}
