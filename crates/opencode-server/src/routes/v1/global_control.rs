//! v1 global, control, instance, file, experimental + tui route families
//! (M6.6) — port of `httpapi/handlers/{global,control,instance,file,
//! experimental,tui}.ts` over the M3 config loader, the M5 services and the
//! M6.6 state seams.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path as PathParam, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{json, Value};

use opencode_core::event::definition::Definition;
use opencode_core::tool::ripgrep::Ripgrep as _;
use opencode_core::{PublishOptions, INSTALLATION_VERSION};
use opencode_schema::session_v1::V1SessionInfo;

use crate::error::ServerError;
use crate::middleware::auth::query_param;
use crate::middleware::location::LocationContext;
use crate::routes::v1::util::*;
use crate::state::ServerContext;

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// `Global.Path.state` (`global.ts:14`) — XDG state home + `opencode`.
fn global_state_path() -> PathBuf {
    let xdg = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty());
    match xdg {
        Some(v) => PathBuf::from(v).join("opencode"),
        None => {
            let home = opencode_core::GlobalPaths::from_env().home;
            home.join(".local").join("state").join("opencode")
        }
    }
}

/// `FSUtil.contains` — whether `path` stays inside `directory`.
fn path_contains(directory: &Path, path: &Path) -> bool {
    let dir = crate::state::resolve_directory(directory);
    let path = crate::state::resolve_directory(path);
    path.starts_with(dir)
}

/// Semver validation for `GlobalUpgradeInput` (`groups/global.ts:24-28`) —
/// `semver.valid` semantics: optional `=`/`v` prefix, `X.Y.Z` core, optional
/// prerelease and build.
fn is_semver(value: &str) -> bool {
    let rest = value.strip_prefix('=').unwrap_or(value);
    let rest = rest.strip_prefix('v').unwrap_or(rest);
    let core = rest.split(['-', '+']).next().unwrap_or_default();
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()))
    {
        return false;
    }
    let tail = &rest[core.len()..];
    tail.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '.'))
}

fn publish(
    location: &LocationContext,
    definition: &Definition,
    data: Value,
) -> Result<(), ServerError> {
    location
        .services
        .events
        .publish(definition, data, PublishOptions::default())
        .map(|_| ())
        .map_err(defect)
}

// ---------------------------------------------------------------------------
// global (`handlers/global.ts`)
// ---------------------------------------------------------------------------

/// `globalConfigFile` (config.ts:140-146).
fn global_config_file() -> PathBuf {
    let paths = opencode_core::GlobalPaths::from_env();
    let candidates = [
        paths.config.join("opencode.jsonc"),
        paths.config.join("opencode.json"),
        paths.config.join("config.json"),
    ];
    candidates
        .into_iter()
        .find(|file| file.exists())
        .unwrap_or_else(|| paths.config.join("opencode.jsonc"))
}

/// `health` (`handlers/global.ts:60-62`).
pub async fn global_health() -> Result<Response, ServerError> {
    Ok(json_ok(json!({
        "healthy": true,
        "version": INSTALLATION_VERSION,
    })))
}

/// `configGet` (`handlers/global.ts:64-66`).
pub async fn global_config_get() -> Result<Response, ServerError> {
    let file = global_config_file();
    let value = match std::fs::read_to_string(&file) {
        Ok(text) if !text.trim().is_empty() => opencode_core::parse_jsonc(&text, &file)?,
        _ => json!({}),
    };
    opencode_core::config::schema::decode_config(&value, &file)?;
    Ok(json_ok(value))
}

/// `configUpdate` (`handlers/global.ts:68-71`) — merge into the global config
/// file; a change disposes every instance and emits `global.disposed`.
pub async fn global_config_update(
    State(ctx): State<Arc<ServerContext>>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|err| payload_error(format!("Invalid JSON payload: {err}")))?;
    if !payload.is_object() {
        return Err(payload_error("Expected an object"));
    }
    let file = global_config_file();
    let before = std::fs::read_to_string(&file).unwrap_or_else(|_| "{}".to_string());
    let mut merged = if std::fs::exists(&file).unwrap_or(false) {
        opencode_core::parse_jsonc(&before, &file)?
    } else {
        json!({})
    };
    opencode_core::merge_deep(&mut merged, &payload);
    opencode_core::config::schema::decode_config(&merged, &file)?;
    // `writableGlobal` — a cleared `shell` drops the key (config.ts:148-157).
    if merged["shell"] == json!("") {
        if let Some(obj) = merged.as_object_mut() {
            obj.remove("shell");
        }
    }
    let serialized = serde_json::to_string_pretty(&merged).expect("serialization");
    let changed = std::fs::exists(&file).unwrap_or(false) && serialized != before;
    std::fs::create_dir_all(file.parent().unwrap_or(Path::new(".")))
        .map_err(|err| defect(format!("failed to create config dir: {err}")))?;
    std::fs::write(&file, &serialized).map_err(defect)?;
    if changed {
        ctx.instances.dispose_all();
        ctx.global_bus.emit(crate::sse::GlobalEvent::injected(
            Some("global".to_string()),
            None,
            None,
            GLOBAL_DISPOSED_TYPE,
            json!({}),
        ));
    }
    Ok(json_ok(merged))
}

/// `dispose` (`handlers/global.ts:73-75`).
pub async fn global_dispose(
    State(ctx): State<Arc<ServerContext>>,
) -> Result<Response, ServerError> {
    ctx.instances.dispose_all();
    ctx.global_bus.emit(crate::sse::GlobalEvent::injected(
        Some("global".to_string()),
        None,
        None,
        GLOBAL_DISPOSED_TYPE,
        json!({}),
    ));
    Ok(json_ok(true))
}

const GLOBAL_DISPOSED_TYPE: &str = "global.disposed";

#[derive(Deserialize)]
struct UpgradePayload {
    target: String,
}

/// `upgrade` (`handlers/global.ts:77-99`) — the `Installation` seam; the
/// unknown installation method answers 400.
pub async fn global_upgrade(
    State(ctx): State<Arc<ServerContext>>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: UpgradePayload = parse_payload(&body)?;
    if !is_semver(&payload.target) {
        return Err(payload_error("Expected a semantic version"));
    }
    if ctx.installation.method() == "unknown" {
        return Ok(Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"success": false, "error": "Unknown installation method"}).to_string(),
            ))
            .expect("static response parts are valid"));
    }
    let target = payload.target;
    match ctx.installation.upgrade(&target) {
        Ok(()) => Ok(json_ok(json!({"success": true, "version": target}))),
        Err(message) => Ok(Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"success": false, "error": message}).to_string(),
            ))
            .expect("static response parts are valid")),
    }
}

// ---------------------------------------------------------------------------
// control (`handlers/control.ts`)
// ---------------------------------------------------------------------------

/// `Auth.Info` validation (`auth/index.ts:11-35`) — a `type`-tagged union:
/// `oauth` needs `refresh`/`access`/`expires`, `api` needs `key`,
/// `wellknown` needs `key`/`token`.
fn auth_info_valid(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    match obj.get("type").and_then(Value::as_str) {
        Some("oauth") => {
            obj.get("refresh").is_some_and(Value::is_string)
                && obj.get("access").is_some_and(Value::is_string)
                && obj.get("expires").is_some_and(Value::is_number)
        }
        Some("api") => obj.get("key").is_some_and(Value::is_string),
        Some("wellknown") => {
            obj.get("key").is_some_and(Value::is_string)
                && obj.get("token").is_some_and(Value::is_string)
        }
        _ => false,
    }
}

/// `authSet` (`handlers/control.ts:7-13`).
pub async fn auth_set(
    State(ctx): State<Arc<ServerContext>>,
    PathParam(provider_id): PathParam<String>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|err| payload_error(format!("Invalid JSON payload: {err}")))?;
    if !auth_info_valid(&payload) {
        return Err(payload_error("Expected Auth.Info"));
    }
    ctx.auth_store.set(&provider_id, payload)?;
    Ok(json_ok(true))
}

/// `authRemove` (`handlers/control.ts:15-20`).
pub async fn auth_remove(
    State(ctx): State<Arc<ServerContext>>,
    PathParam(provider_id): PathParam<String>,
) -> Result<Response, ServerError> {
    ctx.auth_store.remove(&provider_id)?;
    Ok(json_ok(true))
}

#[derive(Deserialize)]
struct LogPayload {
    service: String,
    level: String,
    message: String,
    #[serde(default)]
    extra: Option<std::collections::BTreeMap<String, Value>>,
}

/// `log` (`handlers/control.ts:22-31`).
pub async fn log(body: Bytes) -> Result<Response, ServerError> {
    let payload: LogPayload = parse_payload(&body)?;
    match payload.level.as_str() {
        "debug" => {
            tracing::debug!(service = %payload.service, ?payload.extra, "{}", payload.message)
        }
        "info" => tracing::info!(service = %payload.service, ?payload.extra, "{}", payload.message),
        "warn" => tracing::warn!(service = %payload.service, ?payload.extra, "{}", payload.message),
        "error" => {
            tracing::error!(service = %payload.service, ?payload.extra, "{}", payload.message)
        }
        _ => return Err(payload_error("Expected a literal")),
    }
    Ok(json_ok(true))
}

// ---------------------------------------------------------------------------
// instance (`handlers/instance.ts`)
// ---------------------------------------------------------------------------

/// `dispose` (`handlers/instance.ts:19-22`).
pub async fn instance_dispose(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let response = json_ok(true);
    ctx.instances.dispose_directory(&location.directory);
    Ok(response)
}

/// `getPath` (`handlers/instance.ts:24-32`).
pub async fn path_info(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let paths = opencode_core::GlobalPaths::from_env();
    Ok(json_ok(json!({
        "home": paths.home,
        "state": global_state_path(),
        "config": paths.config,
        "worktree": location.directory,
        "directory": location.directory,
    })))
}

/// `getVcs` (`handlers/instance.ts:34-38`).
pub async fn vcs_info(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let info = ctx.vcs.info(&location.directory)?;
    Ok(json_ok(info))
}

/// `getVcsStatus` (`handlers/instance.ts:40-42`).
pub async fn vcs_status(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let rows = ctx.vcs.status(&location.directory)?;
    Ok(json_ok(rows))
}

/// `getVcsDiff` (`handlers/instance.ts:44-48`).
pub async fn vcs_diff(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    uri: Uri,
) -> Result<Response, ServerError> {
    let mode = match query_param(uri.query(), "mode").as_deref() {
        Some(mode @ ("git" | "branch")) => mode.to_string(),
        Some(other) => {
            return Err(query_error(format!(
                "Expected \"git\" or \"branch\", got {other:?}"
            )))
        }
        None => return Err(query_error("Expected a string, got undefined")),
    };
    let context = match query_param(uri.query(), "context") {
        Some(value) => Some(
            value
                .parse::<i64>()
                .map_err(|_| query_error(format!("Expected a number, got {value:?}")))?,
        ),
        None => None,
    };
    let diff = ctx.vcs.diff(&location.directory, &mode, context)?;
    Ok(json_ok(diff))
}

/// `getVcsDiffRaw` (`handlers/instance.ts:50-52`).
pub async fn vcs_diff_raw(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let raw = ctx.vcs.diff_raw(&location.directory)?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/x-diff; charset=utf-8")
        .body(Body::from(raw))
        .map_err(defect)
}

/// `applyVcs` (`handlers/instance.ts:54-65`).
pub async fn vcs_apply(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let patch: Value = serde_json::from_slice(&body)
        .map_err(|err| payload_error(format!("Invalid JSON payload: {err}")))?;
    let result = ctx.vcs.apply(&location.directory, &patch)?;
    Ok(json_ok(result))
}

/// `Command.hints` (command/index.ts:36-43).
fn hints(template: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    let mut numbered: Vec<String> = regex::Regex::new(r"\$\d+")
        .expect("static pattern")
        .find_iter(template)
        .map(|m| m.as_str().to_string())
        .collect();
    numbered.sort();
    numbered.dedup();
    result.append(&mut numbered);
    if template.contains("$ARGUMENTS") {
        result.push("$ARGUMENTS".to_string());
    }
    result
}

/// `getCommand` (`handlers/instance.ts:67-69`).
pub async fn command_list(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let params = opencode_core::LoadParams::new(location.directory.clone())
        .paths(opencode_core::GlobalPaths::from_env());
    let (config, _) = opencode_core::ConfigLoader::new().load(&params)?;
    let mut commands: Vec<Value> = vec![
        json!({
            "name": "init",
            "description": "guided AGENTS.md setup",
            "source": "command",
            "template": {},
            "hints": ["$ARGUMENTS"],
        }),
        json!({
            "name": "review",
            "description": "review changes [commit|branch|pr], defaults to uncommitted",
            "source": "command",
            "template": {},
            "subtask": true,
            "hints": ["$ARGUMENTS"],
        }),
    ];
    for (name, command) in config.command.iter().flatten() {
        commands.push(json!({
            "name": name,
            "agent": command.agent,
            "model": command.model,
            "description": command.description,
            "source": "command",
            "template": command.template,
            "subtask": command.subtask,
            "hints": hints(&command.template),
        }));
    }
    Ok(json_ok(commands))
}

/// `getAgent` (`handlers/instance.ts:71-73`).
pub async fn agent_list(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let agents = location.services.agents.list();
    let list: Vec<Value> = agents
        .iter()
        .map(|agent| {
            let mut value = json!({
                "name": agent.name,
                "description": agent.description,
                "mode": match agent.mode {
                    opencode_core::AgentMode::Primary => "primary",
                    opencode_core::AgentMode::Subagent => "subagent",
                    opencode_core::AgentMode::All => "all",
                },
                "permission": agent.permission,
                "options": agent.options,
            });
            if let Some(native) = agent.native {
                value["native"] = json!(native);
            }
            if let Some(hidden) = agent.hidden {
                value["hidden"] = json!(hidden);
            }
            if let Some(top_p) = agent.top_p {
                value["topP"] = json!(top_p);
            }
            if let Some(temperature) = agent.temperature {
                value["temperature"] = json!(temperature);
            }
            if let Some(color) = &agent.color {
                value["color"] = json!(color);
            }
            if let Some(model) = &agent.model {
                value["model"] = json!({
                    "providerID": model.provider_id,
                    "modelID": model.model_id,
                });
            }
            if let Some(variant) = &agent.variant {
                value["variant"] = json!(variant);
            }
            if let Some(prompt) = &agent.prompt {
                value["prompt"] = json!(prompt);
            }
            if let Some(steps) = agent.steps {
                value["steps"] = json!(steps);
            }
            value
        })
        .collect();
    Ok(json_ok(list))
}

/// `getSkill` (`handlers/instance.ts:75-77`).
pub async fn skill_list(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let skills = ctx.skills.status(&location.directory)?;
    Ok(json_ok(skills))
}

/// `getLsp` (`handlers/instance.ts:79-81`).
pub async fn lsp_status(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let lsp = ctx.lsp.status(&location.directory)?;
    Ok(json_ok(lsp))
}

/// `getFormatter` (`handlers/instance.ts:83-85`).
pub async fn formatter_status(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let status = ctx.formatter.status(&location.directory)?;
    Ok(json_ok(status))
}

// ---------------------------------------------------------------------------
// file (`handlers/file.ts`)
// ---------------------------------------------------------------------------

/// `findText` (`handlers/file.ts:25-39`) — ripgrep candidates, then the
/// submatch/absolute-offset enrichment the legacy `Match` shape needs.
pub async fn find_text(
    axum::Extension(location): axum::Extension<LocationContext>,
    uri: Uri,
) -> Result<Response, ServerError> {
    let pattern = match query_param(uri.query(), "pattern") {
        Some(pattern) => pattern,
        None => return Err(query_error("Expected a string, got undefined")),
    };
    let regex =
        regex::Regex::new(&pattern).map_err(|_| defect(format!("Invalid regex: {pattern}")))?;
    let directory = location.directory.clone();
    let matches = opencode_core::tool::ripgrep::RipgrepService
        .grep(&directory, &pattern, None, 10)
        .map_err(defect)?;
    let mut out = Vec::new();
    for m in matches {
        let path = directory.join(&m.path);
        let (line_start, line_text) = match read_line(&path, m.line) {
            Some(read) => read,
            None => continue,
        };
        let submatches: Vec<Value> = regex
            .find_iter(&line_text)
            .map(|found| {
                json!({
                    "match": { "text": found.as_str() },
                    "start": found.start(),
                    "end": found.end(),
                })
            })
            .collect();
        out.push(json!({
            "path": { "text": m.path },
            "lines": { "text": m.text },
            "line_number": m.line,
            "absolute_offset": line_start,
            "submatches": submatches,
        }));
    }
    Ok(json_ok(out))
}

/// The byte offset of the 1-based `line` plus its text.
fn read_line(path: &Path, line: u64) -> Option<(usize, String)> {
    let bytes = std::fs::read(path).ok()?;
    let mut start = 0usize;
    let mut current = 1u64;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            if current == line {
                return Some((
                    start,
                    String::from_utf8_lossy(&bytes[start..index]).into_owned(),
                ));
            }
            current += 1;
            start = index + 1;
        }
    }
    if current == line && start < bytes.len() {
        return Some((start, String::from_utf8_lossy(&bytes[start..]).into_owned()));
    }
    None
}

/// `findFile` (`handlers/file.ts:41-60`) — fuzzy file/directory name search
/// over the ripgrep-backed find state (`filesystem/search.ts`).
pub async fn find_file(
    axum::Extension(location): axum::Extension<LocationContext>,
    uri: Uri,
) -> Result<Response, ServerError> {
    let query = match query_param(uri.query(), "query") {
        Some(query) => query,
        None => return Err(query_error("Expected a string, got undefined")),
    };
    let limit = match query_param(uri.query(), "limit") {
        Some(raw) => {
            let raw = raw
                .parse::<i64>()
                .map_err(|_| query_error(format!("Expected a number, got {raw:?}")))?;
            if !(1..=200).contains(&raw) {
                return Err(query_error(format!(
                    "Expected a number between 1 and 200, got {raw}"
                )));
            }
            raw as usize
        }
        None => 10,
    };
    let mut type_filter = match query_param(uri.query(), "type").as_deref() {
        Some(t @ ("file" | "directory")) => Some(t.to_string()),
        Some(other) => {
            return Err(query_error(format!(
                "Expected \"file\" or \"directory\", got {other:?}"
            )))
        }
        None => None,
    };
    if type_filter.is_none() && query_param(uri.query(), "dirs").as_deref() == Some("false") {
        type_filter = Some("file".to_string());
    }
    let vcs = location
        .services
        .instance(&location.directory)
        .map_err(|_| defect("instance context"))?
        .project
        .vcs
        .is_some();
    let state = opencode_core::filesystem::FindState::build(&location.directory, vcs);
    let find_type = match type_filter.as_deref() {
        Some("file") => Some(opencode_core::filesystem::FindType::File),
        Some("directory") => Some(opencode_core::filesystem::FindType::Directory),
        _ => None,
    };
    let matches = opencode_core::filesystem::find(&state, &query, find_type, Some(limit));
    Ok(json_ok(matches))
}

/// `findSymbol` (`handlers/file.ts:62-64`).
pub async fn find_symbol() -> Result<Response, ServerError> {
    Ok(json_ok(Vec::<Value>::new()))
}

/// `list` (`handlers/file.ts:66-93`).
pub async fn file_list(
    axum::Extension(location): axum::Extension<LocationContext>,
    uri: Uri,
) -> Result<Response, ServerError> {
    let path = match query_param(uri.query(), "path") {
        Some(path) => path,
        None => return Err(query_error("Expected a string, got undefined")),
    };
    let directory = &location.directory;
    let target = directory.join(&path);
    let entries = std::fs::read_dir(&target).map_err(defect)?;
    let ignore = CombinedIgnore::load(directory);
    let mut out: Vec<Value> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let relative = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}/{name}")
        };
        let absolute = target.join(entry.file_name());
        let ignored = if is_dir {
            ignore.ignores(&format!("{relative}/"))
        } else {
            ignore.ignores(&relative)
        };
        out.push(json!({
            "name": name,
            "path": relative,
            "absolute": absolute,
            "type": if is_dir { "directory" } else { "file" },
            "ignored": ignored,
        }));
    }
    Ok(json_ok(out))
}

/// `ignore` + `.gitignore` from the project directory.
struct CombinedIgnore {
    matcher: ignore::gitignore::Gitignore,
}

impl CombinedIgnore {
    fn load(directory: &Path) -> CombinedIgnore {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(directory);
        for name in [".gitignore", ".ignore"] {
            let file = directory.join(name);
            if let Ok(text) = std::fs::read_to_string(&file) {
                for line in text.lines() {
                    let _ = builder.add_line(Some(file.clone()), line);
                }
            }
        }
        CombinedIgnore {
            matcher: builder
                .build()
                .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty()),
        }
    }

    fn ignores(&self, path: &str) -> bool {
        self.matcher.matched(path, path.ends_with('/')).is_ignore()
    }
}

/// `content` (`handlers/file.ts:95-135`).
pub async fn file_content(
    axum::Extension(location): axum::Extension<LocationContext>,
    uri: Uri,
) -> Result<Response, ServerError> {
    let path = match query_param(uri.query(), "path") {
        Some(path) => path,
        None => return Err(query_error("Expected a string, got undefined")),
    };
    let directory = &location.directory;
    let file = directory.join(&path);
    if !path_contains(directory, &file) {
        return Err(defect("Path escapes the location"));
    }
    if !file.exists() {
        return Ok(json_ok(json!({"type": "text", "content": ""})));
    }
    let bytes = std::fs::read(&file).map_err(defect)?;
    if bytes.contains(&0) {
        use base64::Engine;
        return Ok(json_ok(json!({
            "type": "binary",
            "content": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "encoding": "base64",
            "mimeType": mime_type(&file),
        })));
    }
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    Ok(json_ok(json!({"type": "text", "content": text})))
}

/// A reduced `mime-types` lookup for the binary branch of `fs.read`.
pub(crate) fn mime_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/vnd.microsoft.icon",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "",
    }
    .to_string()
}

/// `status` (`handlers/file.ts:137-139`).
pub async fn file_status() -> Result<Response, ServerError> {
    Ok(json_ok(Vec::<Value>::new()))
}

// ---------------------------------------------------------------------------
// experimental (`handlers/experimental.ts`)
// ---------------------------------------------------------------------------

/// `capabilities` (`handlers/experimental.ts:48-50`).
pub async fn experimental_capabilities() -> Result<Response, ServerError> {
    Ok(json_ok(
        json!({ "backgroundSubagents": crate::engine::background_subagents_enabled() }),
    ))
}

/// `tool` (`handlers/experimental.ts:89-99`).
pub async fn experimental_tool(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    uri: Uri,
) -> Result<Response, ServerError> {
    let provider = match query_param(uri.query(), "provider") {
        Some(provider) => provider,
        None => return Err(query_error("Expected a string, got undefined")),
    };
    let model = match query_param(uri.query(), "model") {
        Some(model) => model,
        None => return Err(query_error("Expected a string, got undefined")),
    };
    let registry = ctx.tools.registry(&location)?;
    let agent = location.services.agents.default_info().map_err(defect)?;
    let tools = registry
        .tools(opencode_core::tool::registry::ToolModel {
            provider_id: &provider,
            model_id: &model,
            agent: opencode_core::tool::def::AgentInfo {
                name: agent.name,
                description: agent.description,
                mode: agent.mode,
                permission: agent.permission,
            },
            permission: None,
        })
        .await;
    let list: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "id": tool.id,
                "description": tool.description,
                "parameters": tool.parameters,
            })
        })
        .collect();
    Ok(json_ok(list))
}

/// `toolIDs` (`handlers/experimental.ts:101-103`).
pub async fn experimental_tool_ids(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let registry = ctx.tools.registry(&location)?;
    Ok(json_ok(registry.ids()))
}

// ---------------------------------------------------------------------------
// worktree (`handlers/experimental.ts:106-130` + `worktree/index.ts`)
// ---------------------------------------------------------------------------

/// `WorktreeApiError` mapping (`handlers/experimental.ts:15-18`).
fn worktree_error(err: opencode_core::worktree::Error) -> ServerError {
    crate::error::ApiError::Worktree {
        tag: err.tag,
        message: err.message,
    }
    .into()
}

/// The `InstanceState.context` bits the worktree service reads.
fn worktree_context(
    location: &LocationContext,
) -> Result<opencode_core::worktree::Context, ServerError> {
    let instance = location
        .services
        .instance(&location.directory)
        .map_err(defect)?;
    Ok(opencode_core::worktree::Context {
        project_id: instance.project.id.clone(),
        project_worktree: PathBuf::from(&instance.project.worktree),
        worktree: instance.worktree.clone(),
        workspace_id: location.workspace_id.clone(),
        is_git: instance.project.vcs == Some(opencode_schema::project::ProjectVcs::Git),
    })
}

/// `worktree` (`handlers/experimental.ts:106-109`) — the project's
/// sandboxes.
pub async fn worktree_list(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let instance = location
        .services
        .instance(&location.directory)
        .map_err(defect)?;
    let sandboxes = ctx
        .projects
        .sandboxes(&instance.project.id)
        .map_err(|err| defect(err.to_string()))?;
    Ok(json_ok(sandboxes))
}

#[derive(serde::Deserialize, Default)]
struct WorktreeCreatePayload {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "startCommand")]
    start_command: Option<String>,
}

/// `worktreeCreate` (`handlers/experimental.ts:111-115`).
pub async fn worktree_create(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let input = if body.is_empty() {
        None
    } else {
        let payload: WorktreeCreatePayload = parse_payload(&body)?;
        Some(opencode_core::worktree::CreateInput {
            name: payload.name,
            start_command: payload.start_command,
        })
    };
    let worktree_context = worktree_context(&location)?;
    let info = ctx
        .worktree
        .create(&*ctx.worktree_deps, &worktree_context, input.as_ref())
        .map_err(worktree_error)?;
    Ok(json_ok(info))
}

#[derive(serde::Deserialize)]
struct WorktreeDirectoryPayload {
    directory: String,
}

/// `worktreeRemove` (`handlers/experimental.ts:117-126`).
pub async fn worktree_remove(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: WorktreeDirectoryPayload = parse_payload(&body)?;
    let worktree_context = worktree_context(&location)?;
    ctx.worktree
        .remove(&*ctx.worktree_deps, &worktree_context, &payload.directory)
        .map_err(worktree_error)?;
    let instance = location
        .services
        .instance(&location.directory)
        .map_err(defect)?;
    ctx.projects
        .remove_sandbox(&instance.project.id, &payload.directory)
        .map_err(|err| defect(err.to_string()))?;
    Ok(json_ok(true))
}

/// `worktreeReset` (`handlers/experimental.ts:128-132`).
pub async fn worktree_reset(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: WorktreeDirectoryPayload = parse_payload(&body)?;
    let worktree_context = worktree_context(&location)?;
    ctx.worktree
        .reset(&*ctx.worktree_deps, &worktree_context, &payload.directory)
        .map_err(worktree_error)?;
    Ok(json_ok(true))
}

// ---------------------------------------------------------------------------
// control-plane (`handlers/control-plane.ts`)

/// `moveSession` (`handlers/control-plane.ts:12-27`).
pub async fn control_plane_move_session(
    State(ctx): State<Arc<ServerContext>>,
    body: Bytes,
) -> Result<Response, ServerError> {
    #[derive(serde::Deserialize)]
    struct Destination {
        directory: String,
    }
    #[derive(serde::Deserialize)]
    struct MovePayload {
        #[serde(rename = "sessionID")]
        session_id: String,
        destination: Destination,
        #[serde(default, rename = "moveChanges")]
        move_changes: bool,
    }
    let payload: MovePayload = parse_payload(&body)?;
    if !payload.session_id.starts_with("ses") {
        return Err(payload_error("Expected a string starting with \"ses\""));
    }
    let move_session = opencode_core::control_plane::MoveSession::new(
        ctx.sessions.clone(),
        Arc::new(opencode_core::SubprocessGit),
        ctx.bus.clone(),
        Arc::new(opencode_core::catalog::SystemClock),
    );
    match move_session.move_session(&opencode_core::control_plane::Input {
        session_id: payload.session_id,
        destination: payload.destination.directory,
        move_changes: payload.move_changes,
    }) {
        Ok(()) => Ok(Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .expect("static response parts are valid")),
        Err(opencode_core::control_plane::MoveSessionError::Known(err)) => {
            Err(crate::error::ApiError::MoveSession {
                message: err.message(),
            }
            .into())
        }
        Err(opencode_core::control_plane::MoveSessionError::Defect(err)) => Err(defect(err)),
    }
}

/// `session` (`handlers/experimental.ts:157-176`).
pub async fn experimental_session(
    axum::Extension(location): axum::Extension<LocationContext>,
    uri: Uri,
) -> Result<Response, ServerError> {
    let limit = query_i64(&uri, "limit")?.unwrap_or(100);
    let directory_param =
        matches!(query_param(uri.query(), "directory").as_deref(), Some(d) if !d.is_empty());
    let input = opencode_core::GlobalListInput {
        directory: if directory_param {
            Some(location.directory.to_string_lossy().into_owned())
        } else {
            None
        },
        roots: query_bool(&uri, "roots")?,
        start: query_i64(&uri, "start")?,
        cursor: query_i64(&uri, "cursor")?,
        search: query_param(uri.query(), "search"),
        limit: Some(limit + 1),
        archived: query_bool(&uri, "archived")?,
    };
    let all = location
        .services
        .sessions
        .list_global(&input)
        .map_err(defect)?;
    let more = all.len() as i64 > limit;
    let list: Vec<&opencode_core::session::GlobalInfo> = if more {
        all.iter().take(limit as usize).collect()
    } else {
        all.iter().collect()
    };
    let body = serde_json::to_string(&list).expect("serialization");
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json");
    if more && !list.is_empty() {
        let next = list[list.len() - 1].info.time.updated;
        builder = builder.header("x-next-cursor", next.to_string());
    }
    Ok(builder
        .body(Body::from(body))
        .expect("static response parts are valid"))
}

/// `sessionBackground` (`handlers/experimental.ts:178-193`): promote the
/// session's running, non-background task jobs; `true` when any promoted.
pub async fn experimental_session_background(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
    PathParam(session_id): PathParam<String>,
) -> Result<Response, ServerError> {
    // `if (!flags.experimentalBackgroundSubagents) return false` — before
    // any service resolution (experimental.ts:180).
    if !crate::engine::background_subagents_enabled() {
        return Ok(json_ok(false));
    }
    let engine = (ctx.engine_factory)(&location)?;
    let promoted = engine.session_background(&session_id).await;
    Ok(json_ok(promoted))
}

/// `resource` (`handlers/experimental.ts:195-197`).
pub async fn experimental_resource(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    let mcp = ctx.mcp.service(&location)?;
    Ok(json_ok(mcp.resources(None).await))
}

// ---------------------------------------------------------------------------
// tui (`handlers/tui.ts`)
// ---------------------------------------------------------------------------

const TUI_PROMPT_APPEND: Definition = Definition::ephemeral("tui.prompt.append");
const TUI_COMMAND_EXECUTE: Definition = Definition::ephemeral("tui.command.execute");
const TUI_TOAST_SHOW: Definition = Definition::ephemeral("tui.toast.show");
const TUI_SESSION_SELECT: Definition = Definition::ephemeral("tui.session.select");

/// `publishCommand` (`handlers/tui.ts:20-21`) — `undefined` commands drop
/// the key entirely.
fn publish_command(
    location: &LocationContext,
    command: Option<&str>,
) -> Result<Response, ServerError> {
    let data = match command {
        Some(command) => json!({ "command": command }),
        None => json!({}),
    };
    publish(location, &TUI_COMMAND_EXECUTE, data)?;
    Ok(json_ok(true))
}

/// `commandAliases` (`handlers/tui.ts:15-30`).
fn command_alias(command: &str) -> Option<&'static str> {
    match command {
        "session_new" => Some("session.new"),
        "session_share" => Some("session.share"),
        "session_interrupt" => Some("session.interrupt"),
        "session_compact" => Some("session.compact"),
        "messages_page_up" => Some("session.page.up"),
        "messages_page_down" => Some("session.page.down"),
        "messages_line_up" => Some("session.line.up"),
        "messages_line_down" => Some("session.line.down"),
        "messages_half_page_up" => Some("session.half.page.up"),
        "messages_half_page_down" => Some("session.half.page.down"),
        "messages_first" => Some("session.first"),
        "messages_last" => Some("session.last"),
        "agent_cycle" => Some("agent.cycle"),
        _ => None,
    }
}

#[derive(Deserialize)]
struct PromptAppendPayload {
    text: String,
}

#[derive(Deserialize)]
struct ToastShowPayload {
    #[serde(default)]
    title: Option<String>,
    message: String,
    variant: String,
    #[serde(default)]
    duration: Option<u64>,
}

#[derive(Deserialize)]
struct SessionSelectPayload {
    #[serde(rename = "sessionID")]
    session_id: String,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum TuiPublishPayload {
    #[serde(rename = "tui.prompt.append")]
    PromptAppend { properties: PromptAppendPayload },
    #[serde(rename = "tui.command.execute")]
    CommandExecute {
        properties: CommandExecuteProperties,
    },
    #[serde(rename = "tui.toast.show")]
    ToastShow { properties: ToastShowPayload },
    #[serde(rename = "tui.session.select")]
    SessionSelect { properties: SessionSelectPayload },
}

#[derive(Deserialize)]
struct CommandExecuteProperties {
    command: String,
}

/// `appendPrompt` (`handlers/tui.ts:20-24`).
pub async fn tui_append_prompt(
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: PromptAppendPayload = parse_payload(&body)?;
    publish(
        &location,
        &TUI_PROMPT_APPEND,
        json!({ "text": payload.text }),
    )?;
    Ok(json_ok(true))
}

/// `openHelp` (`handlers/tui.ts:26-29`).
pub async fn tui_open_help(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    publish_command(&location, Some("help.show"))?;
    Ok(json_ok(true))
}

/// `openSessions` (`handlers/tui.ts:31-34`).
pub async fn tui_open_sessions(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    publish_command(&location, Some("session.list"))?;
    Ok(json_ok(true))
}

/// `openThemes` (`handlers/tui.ts:36-39`).
pub async fn tui_open_themes(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    publish_command(&location, Some("session.list"))?;
    Ok(json_ok(true))
}

/// `openModels` (`handlers/tui.ts:41-44`).
pub async fn tui_open_models(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    publish_command(&location, Some("model.list"))?;
    Ok(json_ok(true))
}

/// `submitPrompt` (`handlers/tui.ts:46-49`).
pub async fn tui_submit_prompt(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    publish_command(&location, Some("prompt.submit"))?;
    Ok(json_ok(true))
}

/// `clearPrompt` (`handlers/tui.ts:51-54`).
pub async fn tui_clear_prompt(
    axum::Extension(location): axum::Extension<LocationContext>,
) -> Result<Response, ServerError> {
    publish_command(&location, Some("prompt.clear"))?;
    Ok(json_ok(true))
}

#[derive(Deserialize)]
struct ExecuteCommandPayload {
    command: String,
}

/// `TuiEvent.ToastShow` data (`tui-event.ts:40-50`) — optional `title`
/// serializes as an absent key, `duration` defaults to 5000.
fn toast_data(payload: ToastShowPayload) -> Value {
    let mut data = serde_json::Map::new();
    if let Some(title) = payload.title {
        data.insert("title".to_string(), json!(title));
    }
    data.insert("message".to_string(), json!(payload.message));
    data.insert("variant".to_string(), json!(payload.variant));
    data.insert(
        "duration".to_string(),
        json!(payload.duration.unwrap_or(5000)),
    );
    Value::Object(data)
}

/// `executeCommand` (`handlers/tui.ts:56-60`).
pub async fn tui_execute_command(
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: ExecuteCommandPayload = parse_payload(&body)?;
    publish_command(&location, command_alias(&payload.command))?;
    Ok(json_ok(true))
}

/// `showToast` (`handlers/tui.ts:62-67`).
pub async fn tui_show_toast(
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: ToastShowPayload = parse_payload(&body)?;
    if !matches!(
        payload.variant.as_str(),
        "info" | "success" | "warning" | "error"
    ) {
        return Err(payload_error("Expected a literal"));
    }
    publish(&location, &TUI_TOAST_SHOW, toast_data(payload))?;
    Ok(json_ok(true))
}

/// `publish` (`handlers/tui.ts:69-81`).
pub async fn tui_publish(
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: TuiPublishPayload = parse_payload(&body)?;
    match payload {
        TuiPublishPayload::PromptAppend { properties } => publish(
            &location,
            &TUI_PROMPT_APPEND,
            json!({ "text": properties.text }),
        )?,
        TuiPublishPayload::CommandExecute { properties } => publish(
            &location,
            &TUI_COMMAND_EXECUTE,
            json!({ "command": properties.command }),
        )?,
        TuiPublishPayload::ToastShow { properties } => {
            publish(&location, &TUI_TOAST_SHOW, toast_data(properties))?
        }
        TuiPublishPayload::SessionSelect { properties } => {
            if !properties.session_id.starts_with("ses") {
                return Err(payload_error("Expected a string starting with \"ses\""));
            }
            publish(
                &location,
                &TUI_SESSION_SELECT,
                json!({ "sessionID": properties.session_id }),
            )?
        }
    }
    Ok(json_ok(true))
}

/// `selectSession` (`handlers/tui.ts:83-91`).
pub async fn tui_select_session(
    axum::Extension(location): axum::Extension<LocationContext>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let payload: SessionSelectPayload = parse_payload(&body)?;
    if !payload.session_id.starts_with("ses") {
        return Ok(bad_request_empty());
    }
    let _session: V1SessionInfo = location
        .services
        .sessions
        .get(&payload.session_id)
        .map_err(session_error)?;
    publish(
        &location,
        &TUI_SESSION_SELECT,
        json!({ "sessionID": payload.session_id }),
    )?;
    Ok(json_ok(true))
}

/// `controlNext` (`handlers/tui.ts:112-114`).
pub async fn tui_control_next(
    State(ctx): State<Arc<ServerContext>>,
) -> Result<Response, ServerError> {
    match ctx.tui.next_request().await {
        Some(request) => Ok(json_ok(request)),
        None => Err(defect("TUI request queue closed")),
    }
}

/// `controlResponse` (`handlers/tui.ts:116-119`).
pub async fn tui_control_response(
    State(ctx): State<Arc<ServerContext>>,
    body: Bytes,
) -> Result<Response, ServerError> {
    let response: Value = serde_json::from_slice(&body)
        .map_err(|err| payload_error(format!("Invalid JSON payload: {err}")))?;
    ctx.tui.push_response(response);
    Ok(json_ok(true))
}

// ---------------------------------------------------------------------------
// route registration
// ---------------------------------------------------------------------------

pub fn register(
    router: axum::Router<Arc<ServerContext>>,
    method: &str,
    path: &'static str,
) -> (axum::Router<Arc<ServerContext>>, bool) {
    use axum::routing::{delete, patch, put};
    let router = match (method, path) {
        // ---- global ----
        ("GET", "/global/health") => router.route(path, get(global_health)),
        ("GET", "/global/config") => router.route(path, get(global_config_get)),
        ("PATCH", "/global/config") => router.route(path, patch(global_config_update)),
        ("POST", "/global/dispose") => router.route(path, post(global_dispose)),
        ("POST", "/global/upgrade") => router.route(path, post(global_upgrade)),
        // ---- control ----
        ("PUT", "/auth/{providerID}") => router.route(path, put(auth_set)),
        ("DELETE", "/auth/{providerID}") => router.route(path, delete(auth_remove)),
        ("POST", "/log") => router.route(path, post(log)),
        // ---- instance ----
        ("POST", "/instance/dispose") => router.route(path, post(instance_dispose)),
        ("GET", "/path") => router.route(path, get(path_info)),
        ("GET", "/vcs") => router.route(path, get(vcs_info)),
        ("GET", "/vcs/status") => router.route(path, get(vcs_status)),
        ("GET", "/vcs/diff") => router.route(path, get(vcs_diff)),
        ("GET", "/vcs/diff/raw") => router.route(path, get(vcs_diff_raw)),
        ("POST", "/vcs/apply") => router.route(path, post(vcs_apply)),
        ("GET", "/command") => router.route(path, get(command_list)),
        ("GET", "/agent") => router.route(path, get(agent_list)),
        ("GET", "/skill") => router.route(path, get(skill_list)),
        ("GET", "/lsp") => router.route(path, get(lsp_status)),
        ("GET", "/formatter") => router.route(path, get(formatter_status)),
        // ---- file ----
        ("GET", "/find") => router.route(path, get(find_text)),
        ("GET", "/find/file") => router.route(path, get(find_file)),
        ("GET", "/find/symbol") => router.route(path, get(find_symbol)),
        ("GET", "/file") => router.route(path, get(file_list)),
        ("GET", "/file/content") => router.route(path, get(file_content)),
        ("GET", "/file/status") => router.route(path, get(file_status)),
        // ---- experimental ----
        ("GET", "/experimental/capabilities") => router.route(path, get(experimental_capabilities)),
        ("GET", "/experimental/tool") => router.route(path, get(experimental_tool)),
        ("GET", "/experimental/tool/ids") => router.route(path, get(experimental_tool_ids)),
        ("GET", "/experimental/worktree") => router.route(path, get(worktree_list)),
        ("POST", "/experimental/worktree") => router.route(path, post(worktree_create)),
        ("DELETE", "/experimental/worktree") => router.route(path, delete(worktree_remove)),
        ("POST", "/experimental/worktree/reset") => router.route(path, post(worktree_reset)),
        ("POST", "/experimental/control-plane/move-session") => {
            router.route(path, post(control_plane_move_session))
        }
        ("GET", "/experimental/session") => router.route(path, get(experimental_session)),
        ("GET", "/experimental/resource") => router.route(path, get(experimental_resource)),
        ("POST", "/experimental/session/{sessionID}/background") => {
            router.route(path, post(experimental_session_background))
        }
        // ---- tui ----
        ("POST", "/tui/append-prompt") => router.route(path, post(tui_append_prompt)),
        ("POST", "/tui/open-help") => router.route(path, post(tui_open_help)),
        ("POST", "/tui/open-sessions") => router.route(path, post(tui_open_sessions)),
        ("POST", "/tui/open-themes") => router.route(path, post(tui_open_themes)),
        ("POST", "/tui/open-models") => router.route(path, post(tui_open_models)),
        ("POST", "/tui/submit-prompt") => router.route(path, post(tui_submit_prompt)),
        ("POST", "/tui/clear-prompt") => router.route(path, post(tui_clear_prompt)),
        ("POST", "/tui/execute-command") => router.route(path, post(tui_execute_command)),
        ("POST", "/tui/show-toast") => router.route(path, post(tui_show_toast)),
        ("POST", "/tui/publish") => router.route(path, post(tui_publish)),
        ("POST", "/tui/select-session") => router.route(path, post(tui_select_session)),
        ("GET", "/tui/control/next") => router.route(path, get(tui_control_next)),
        ("POST", "/tui/control/response") => router.route(path, post(tui_control_response)),
        _ => return (router, false),
    };
    (router, true)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_matrix() {
        assert!(is_semver("1.2.3"));
        assert!(is_semver("v1.2.3"));
        assert!(is_semver("1.2.3-beta.1"));
        assert!(is_semver("1.2.3+build"));
        assert!(!is_semver("1.2"));
        assert!(!is_semver("latest"));
        assert!(!is_semver(""));
    }

    #[test]
    fn read_line_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
        assert_eq!(read_line(&file, 1), Some((0, "alpha".to_string())));
        assert_eq!(read_line(&file, 2), Some((6, "beta".to_string())));
        assert_eq!(read_line(&file, 9), None);
    }
}
