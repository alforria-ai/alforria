//! `ConfigV2Compat` (config/v2-compat.ts) — lower a V2 configuration
//! record onto the V1 shape so the V1 schema can validate it.
//!
//! `lower` decodes the input against a permissive record, rejects V2
//! `permissions` (fatal `InvalidError`), flags unsupported keys, and
//! normalizes settings/agents/commands/mcp/lsp onto their legacy names.
//! Diagnostics are warnings; the lowered value feeds `ConfigV1.Info`.

use serde_json::{json, Map, Value};

use crate::SchemaIssue;

/// `Diagnostic` (v2-compat.ts:5-9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticKind {
    Invalid,
    Unsupported,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub path: Vec<String>,
    pub message: String,
}

/// `Result` (v2-compat.ts:11-15).
pub struct LowerResult {
    pub value: Value,
    pub diagnostics: Vec<Diagnostic>,
}

/// The fatal `InvalidError` for V2 permissions (v2-compat.ts:107-113).
#[derive(Debug, thiserror::Error)]
#[error("V2 permissions are not supported by OpenCode V1. Use V1 \"permission\" rules or run opencode2.")]
pub struct V2PermissionsError {
    pub issues: Vec<SchemaIssue>,
}

/// `lower` (v2-compat.ts:91-132).
pub fn lower(input: &Value) -> Result<LowerResult, V2PermissionsError> {
    let Some(record) = as_record(input) else {
        return Ok(LowerResult {
            value: input.clone(),
            diagnostics: Vec::new(),
        });
    };

    // `agents`/`agent`/`mode` carrying V2 `permissions` is fatal.
    let mut permissions: Vec<Vec<String>> = Vec::new();
    if record.contains_key("permissions") {
        permissions.push(vec!["permissions".to_string()]);
    }
    for key in ["agents", "agent", "mode"] {
        if let Some(agents) = record.get(key).and_then(as_record) {
            for (name, value) in agents {
                if let Some(agent) = as_record(value) {
                    if agent.contains_key("permissions") {
                        permissions.push(vec![
                            key.to_string(),
                            name.clone(),
                            "permissions".to_string(),
                        ]);
                    }
                }
            }
        }
    }
    if !permissions.is_empty() {
        return Err(V2PermissionsError {
            issues: permissions
                .into_iter()
                .map(|path| SchemaIssue {
                    path,
                    message: "V2 permissions are not supported by OpenCode V1. Use V1 \"permission\" rules or run opencode2."
                        .to_string(),
                })
                .collect(),
        });
    }

    let mut diagnostics = Vec::new();
    let mut result: Map<String, Value> = record.clone();

    for key in ["plugins", "providers", "websearch", "warming"] {
        if record.contains_key(key) {
            unsupported(&[key.to_string()], &mut diagnostics);
        }
    }

    normalize_settings(record, &mut result, &mut diagnostics);
    normalize_model(record, &mut result, diagnostics_warn_only(&mut diagnostics));
    normalize_skills(record, &mut result, &mut diagnostics);
    normalize_compaction(record, &mut result, &mut diagnostics);
    normalize_experimental(record, &mut result, &mut diagnostics);
    normalize_agents(record, &mut result, &mut diagnostics);
    normalize_commands(record, &mut result, &mut diagnostics);
    normalize_mcp(record, &mut result, &mut diagnostics);
    normalize_lsp(record, &mut result, &mut diagnostics);

    Ok(LowerResult {
        value: Value::Object(result),
        diagnostics,
    })
}

fn diagnostics_warn_only(diagnostics: &mut Vec<Diagnostic>) -> &mut Vec<Diagnostic> {
    diagnostics
}

fn as_record(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn unsupported(path: &[String], diagnostics: &mut Vec<Diagnostic>) {
    diagnostics.push(Diagnostic {
        kind: DiagnosticKind::Unsupported,
        path: path.to_vec(),
        message: "Omitted native setting that cannot be represented in V1".to_string(),
    });
}

fn conflict(path: &[String], diagnostics: &mut Vec<Diagnostic>) {
    diagnostics.push(Diagnostic {
        kind: DiagnosticKind::Conflict,
        path: path.to_vec(),
        message: "Retained legacy value over native value".to_string(),
    });
}

/// `decodeValue` — a malformed value yields an `invalid` diagnostic
/// instead of a decode failure (v2-compat.ts:395-403).
fn invalid(path: &[String], diagnostics: &mut Vec<Diagnostic>) {
    diagnostics.push(Diagnostic {
        kind: DiagnosticKind::Invalid,
        path: path.to_vec(),
        message: "Native setting could not be lowered because it is malformed".to_string(),
    });
}

/// `preferLegacy` (v2-compat.ts:405-417).
fn prefer_legacy(
    target: &mut Map<String, Value>,
    key: &str,
    value: Value,
    path: &[String],
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let Some(existing) = target.get(key) {
        if existing != &value {
            conflict(path, diagnostics);
        }
        return;
    }
    target.insert(key.to_string(), value);
}

/// `normalizeSettings` (v2-compat.ts:135-144).
fn normalize_settings(
    input: &Map<String, Value>,
    result: &mut Map<String, Value>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let Some(snapshots) = input.get("snapshots") {
        if let Some(value) = decode_bool(snapshots) {
            prefer_legacy(
                result,
                "snapshot",
                json!(value),
                &["snapshots".to_string()],
                diagnostics,
            );
        } else {
            invalid(&["snapshots".to_string()], diagnostics);
        }
    }
    if let Some(media) = input.get("media") {
        // `ConfigAttachmentV1.Info` is a record — any object lowers.
        if let Some(value) = as_record(media) {
            prefer_legacy(
                result,
                "attachment",
                Value::Object(value.clone()),
                &["media".to_string()],
                diagnostics,
            );
        } else {
            invalid(&["media".to_string()], diagnostics);
        }
    }
}

fn decode_bool(value: &Value) -> Option<bool> {
    value.as_bool()
}

/// `normalizeModel` (v2-compat.ts:146-153).
fn normalize_model(
    input: &Map<String, Value>,
    result: &mut Map<String, Value>,
    _diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(model) = input.get("model") else {
        return;
    };
    let Some(selection) = decode_selection(model) else {
        return;
    };
    let (model, variant) = lower_selection(&selection);
    result.insert("model".to_string(), json!(model));
    if let Some(variant) = variant {
        result.insert("variant".to_string(), json!(variant));
    }
}

/// `Selection` (v2-compat.ts:48-56) — `provider/model[#variant]` or a
/// `{providerID, model, variant?}` record.
fn decode_selection(value: &Value) -> Option<Value> {
    if let Some(text) = value.as_str() {
        // `/^[^\/#]+\/[^#]+(#[^#]+)?$/` (v2-compat.ts:48-52).
        let (provider, variant) = match text.split_once('#') {
            Some((provider, variant)) if !variant.is_empty() => (provider, Some(variant)),
            Some(_) => return None,
            None => (text, None),
        };
        let _ = variant;
        if provider.starts_with('/')
            || provider.ends_with('/')
            || !provider.contains('/')
            || provider.contains('#')
        {
            return None;
        }
        return Some(value.clone());
    }
    let record = value.as_object()?;
    if !record.contains_key("providerID") || !record.contains_key("model") {
        return None;
    }
    if record["providerID"].as_str().is_none_or(str::is_empty)
        || record["model"].as_str().is_none_or(str::is_empty)
    {
        return None;
    }
    if let Some(variant) = record.get("variant") {
        if variant.as_str().is_none_or(str::is_empty) {
            return None;
        }
    }
    Some(value.clone())
}

/// `lowerSelection` (v2-compat.ts:319-329).
fn lower_selection(selection: &Value) -> (String, Option<String>) {
    if let Some(text) = selection.as_str() {
        if let Some(index) = text.find('#') {
            return (
                text[..index].to_string(),
                Some(text[index + 1..].to_string()),
            );
        }
        return (text.to_string(), None);
    }
    let record = selection
        .as_object()
        .expect("validated by decode_selection");
    let model = format!(
        "{}/{}",
        record["providerID"].as_str().unwrap_or_default(),
        record["model"].as_str().unwrap_or_default()
    );
    match record.get("variant").and_then(Value::as_str) {
        Some(variant) => (model, Some(variant.to_string())),
        None => (model, None),
    }
}

/// `normalizeSkills` (v2-compat.ts:155-163).
fn normalize_skills(
    input: &Map<String, Value>,
    result: &mut Map<String, Value>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(skills) = input.get("skills").and_then(Value::as_array) else {
        return;
    };
    let mut paths = Vec::new();
    let mut urls = Vec::new();
    let mut valid = true;
    for skill in skills {
        if let Some(text) = skill.as_str() {
            if text.starts_with("http://") || text.starts_with("https://") {
                urls.push(skill.clone());
            } else {
                paths.push(skill.clone());
            }
        } else {
            valid = false;
        }
    }
    if !valid {
        invalid(&["skills".to_string()], diagnostics);
        return;
    }
    result.insert(
        "skills".to_string(),
        json!({ "paths": paths, "urls": urls }),
    );
}

/// `normalizeCompaction` (v2-compat.ts:165-191).
fn normalize_compaction(
    input: &Map<String, Value>,
    result: &mut Map<String, Value>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(compaction) = input.get("compaction").and_then(Value::as_object) else {
        return;
    };
    let mut value = compaction.clone();
    if let Some(keep) = value.get("keep") {
        if let Some(keep) = keep.as_object() {
            if let Some(tokens) = keep.get("tokens") {
                match decode_non_negative_int(tokens) {
                    Some(tokens) => {
                        prefer_legacy(
                            &mut value,
                            "preserve_recent_tokens",
                            json!(tokens),
                            &[
                                "compaction".to_string(),
                                "keep".to_string(),
                                "tokens".to_string(),
                            ],
                            diagnostics,
                        );
                    }
                    None => invalid(
                        &[
                            "compaction".to_string(),
                            "keep".to_string(),
                            "tokens".to_string(),
                        ],
                        diagnostics,
                    ),
                }
            }
        }
    }
    if let Some(buffer) = value.get("buffer") {
        match decode_non_negative_int(buffer) {
            Some(buffer) => {
                prefer_legacy(
                    &mut value,
                    "reserved",
                    json!(buffer),
                    &["compaction".to_string(), "buffer".to_string()],
                    diagnostics,
                );
            }
            None => invalid(
                &["compaction".to_string(), "buffer".to_string()],
                diagnostics,
            ),
        }
    }
    result.insert("compaction".to_string(), Value::Object(value));
}

fn decode_non_negative_int(value: &Value) -> Option<u64> {
    let number = value.as_f64()?;
    if number < 0.0 || number.fract() != 0.0 {
        return None;
    }
    Some(number as u64)
}

/// `normalizeExperimental` (v2-compat.ts:193-217).
fn normalize_experimental(
    input: &Map<String, Value>,
    result: &mut Map<String, Value>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(experimental) = input.get("experimental").and_then(Value::as_object) else {
        return;
    };
    if experimental.contains_key("portable_shell_scanner") {
        unsupported(
            &[
                "experimental".to_string(),
                "portable_shell_scanner".to_string(),
            ],
            diagnostics,
        );
    }
    let Some(depth) = experimental.get("subagent_depth") else {
        return;
    };
    match decode_non_negative_int(depth) {
        Some(depth) => {
            prefer_legacy(
                result,
                "subagent_depth",
                json!(depth),
                &["experimental".to_string(), "subagent_depth".to_string()],
                diagnostics,
            );
        }
        None => invalid(
            &["experimental".to_string(), "subagent_depth".to_string()],
            diagnostics,
        ),
    }
}

/// `normalizeAgents` (v2-compat.ts:219-236).
fn normalize_agents(
    input: &Map<String, Value>,
    result: &mut Map<String, Value>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(agents) = input.get("agents").and_then(Value::as_object) else {
        return;
    };
    let legacy = result.get("agent").and_then(Value::as_object).cloned();
    let mut merged: Map<String, Value> = legacy.clone().unwrap_or_default();
    for (name, value) in agents {
        let path = vec!["agents".to_string(), name.clone()];
        if let Some(existing) = merged.get(name) {
            if existing != value {
                conflict(&path, diagnostics);
            }
            continue;
        }
        let Some(parsed) = decode_agent(value, &path, diagnostics) else {
            continue;
        };
        let parsed = parsed;
        if parsed
            .get("request")
            .and_then(|request| request.get("headers"))
            .is_some()
        {
            unsupported(
                &[
                    "agents".to_string(),
                    name.clone(),
                    "request".to_string(),
                    "headers".to_string(),
                ],
                diagnostics,
            );
        }
        let _ = parsed;
        let lowered = lower_agent(value);
        merged.insert(name.clone(), lowered);
    }
    if !merged.is_empty() || legacy.is_some() {
        result.insert("agent".to_string(), Value::Object(merged));
    }
}

fn decode_agent(
    value: &Value,
    path: &[String],
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Value> {
    let Some(record) = value.as_object() else {
        invalid(path, diagnostics);
        return None;
    };
    for key in record.keys() {
        if !matches!(
            key.as_str(),
            "model"
                | "request"
                | "system"
                | "description"
                | "mode"
                | "hidden"
                | "color"
                | "steps"
                | "disabled"
        ) {
            let _ = key;
        }
    }
    if let Some(model) = record.get("model") {
        if decode_selection(model).is_none() {
            invalid(
                &[path.to_vec(), vec!["model".to_string()]].concat(),
                diagnostics,
            );
            return None;
        }
    }
    Some(value.clone())
}

/// `lowerAgent` (v2-compat.ts:359-372).
fn lower_agent(input: &Value) -> Value {
    let record = input.as_object().expect("validated by decode_agent");
    let mut result = Map::new();
    for key in ["description", "mode", "hidden", "color", "steps"] {
        if let Some(value) = record.get(key) {
            if !value.is_null() {
                result.insert(key.to_string(), value.clone());
            }
        }
    }
    if let Some(system) = record.get("system") {
        if !system.is_null() {
            result.insert("prompt".to_string(), system.clone());
        }
    }
    if let Some(disabled) = record.get("disabled") {
        if !disabled.is_null() {
            result.insert("disable".to_string(), disabled.clone());
        }
    }
    if let Some(model) = record.get("model") {
        if !model.is_null() {
            let (model, variant) = lower_selection(model);
            result.insert("model".to_string(), json!(model));
            if let Some(variant) = variant {
                result.insert("variant".to_string(), json!(variant));
            }
        }
    }
    if let Some(request) = record.get("request").and_then(Value::as_object) {
        if let Some(body) = request.get("body") {
            if !body.is_null() {
                result.insert("options".to_string(), body.clone());
            }
        }
    }
    Value::Object(result)
}

/// `normalizeCommands` (v2-compat.ts:238-254).
fn normalize_commands(
    input: &Map<String, Value>,
    result: &mut Map<String, Value>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(commands) = input.get("commands").and_then(Value::as_object) else {
        return;
    };
    let legacy = result.get("command").and_then(Value::as_object).cloned();
    let mut merged: Map<String, Value> = legacy.clone().unwrap_or_default();
    for (name, value) in commands {
        let path = vec!["commands".to_string(), name.clone()];
        let Some(parsed) = decode_command(value, &path, diagnostics) else {
            continue;
        };
        prefer_legacy(
            &mut merged,
            name,
            lower_command(&parsed),
            &path,
            diagnostics,
        );
    }
    if !merged.is_empty() || legacy.is_some() {
        result.insert("command".to_string(), Value::Object(merged));
    }
}

/// `Command` (v2-compat.ts:84-88) — `{template, description?, agent?,
/// model?, subtask?}`.
fn decode_command(
    value: &Value,
    path: &[String],
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Value> {
    let Some(record) = value.as_object() else {
        invalid(path, diagnostics);
        return None;
    };
    if record.get("template").and_then(Value::as_str).is_none() {
        invalid(path, diagnostics);
        return None;
    }
    if let Some(model) = record.get("model") {
        if decode_selection(model).is_none() {
            invalid(
                &[path.to_vec(), vec!["model".to_string()]].concat(),
                diagnostics,
            );
            return None;
        }
    }
    Some(value.clone())
}

/// `lowerCommand` (v2-compat.ts:374-376).
fn lower_command(input: &Value) -> Value {
    let record = input.as_object().expect("validated by decode_command");
    let mut result = record.clone();
    if let Some(model) = record.get("model") {
        if !model.is_null() {
            let (model, variant) = lower_selection(model);
            result.remove("model");
            result.insert("model".to_string(), json!(model));
            match variant {
                Some(variant) => {
                    result.insert("variant".to_string(), json!(variant));
                }
                None => {
                    result.remove("variant");
                }
            }
        }
    }
    Value::Object(result)
}

/// `normalizeMcp` (v2-compat.ts:256-317).
fn normalize_mcp(
    input: &Map<String, Value>,
    result: &mut Map<String, Value>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(mcp) = input.get("mcp").and_then(Value::as_object) else {
        return;
    };
    let mut servers: Map<String, Value> = Map::new();
    let nested = mcp.get("servers").and_then(Value::as_object);
    let envelope = nested.is_some_and(|nested| !is_direct_server(nested));
    let global_timeout =
        nested.is_none() && mcp.get("timeout").and_then(Value::as_object).is_some();
    let _ = global_timeout;

    for (name, value) in mcp {
        if name == "servers" && envelope {
            continue;
        }
        if name == "timeout"
            && mcp
                .get("timeout")
                .and_then(Value::as_object)
                .map(|timeout| {
                    timeout.is_empty()
                        || timeout.contains_key("startup")
                        || timeout.contains_key("catalog")
                        || timeout.contains_key("execution")
                })
                .unwrap_or(false)
            && nested.is_none()
        {
            continue;
        }
        let path = vec!["mcp".to_string(), name.clone()];
        let record = value.as_object();
        let oauth = record
            .and_then(|record| record.get("oauth"))
            .and_then(Value::as_object);
        let native = record.is_some_and(|record| {
            record.contains_key("disabled")
                || record.contains_key("codemode")
                || record
                    .get("timeout")
                    .map(|timeout| timeout.is_object())
                    .unwrap_or(false)
                || oauth.is_some_and(|oauth| {
                    [
                        "client_id",
                        "client_secret",
                        "callback_port",
                        "redirect_uri",
                    ]
                    .iter()
                    .any(|key| oauth.contains_key(*key))
                })
        });
        servers.insert(
            name.clone(),
            if native {
                normalize_server(value, &path, diagnostics).unwrap_or_else(|| value.clone())
            } else {
                value.clone()
            },
        );
    }

    if envelope {
        let nested = nested.expect("checked above");
        for (name, value) in nested {
            let path = vec!["mcp".to_string(), "servers".to_string(), name.clone()];
            if let Some(existing) = servers.get(name) {
                if existing != value {
                    conflict(&path, diagnostics);
                }
                continue;
            }
            let record = value.as_object();
            if record.is_some_and(|record| {
                record
                    .get("enabled")
                    .map(Value::is_boolean)
                    .unwrap_or(false)
                    && !record.contains_key("disabled")
            }) {
                servers.insert(name.clone(), value.clone());
                continue;
            }
            if let Some(server) = normalize_server(value, &path, diagnostics) {
                servers.insert(name.clone(), server);
            }
        }
    }
    result.insert("mcp".to_string(), Value::Object(servers));

    // Global `mcp.timeout` lowers into `experimental.mcp_timeout` when it
    // names equal catalog/execution timeouts (v2-compat.ts:305-317).
    let Some(timeout_record) = mcp.get("timeout").and_then(Value::as_object) else {
        return;
    };
    let nested_absent = mcp.get("servers").and_then(Value::as_object).is_none();
    if !nested_absent {
        return;
    }
    if let Some(timeout) = lower_timeout(timeout_record) {
        let mut experimental = result
            .get("experimental")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        prefer_legacy(
            &mut experimental,
            "mcp_timeout",
            json!(timeout),
            &["mcp".to_string(), "timeout".to_string()],
            diagnostics,
        );
        result.insert("experimental".to_string(), Value::Object(experimental));
    } else if !timeout_record.is_empty() {
        unsupported(&["mcp".to_string(), "timeout".to_string()], diagnostics);
    }
}

/// `isDirectServer` (v2-compat.ts:319-325) — object entries named
/// `type`/`enabled` holding non-objects are literal servers.
fn is_direct_server(value: &Map<String, Value>) -> bool {
    ["type", "enabled"].iter().any(|key| {
        value.contains_key(*key) && {
            let entry = &value[*key];
            entry.is_null() || !entry.is_object() || entry.is_array()
        }
    })
}

/// `normalizeServer` (v2-compat.ts:327-343).
fn normalize_server(
    input: &Value,
    path: &[String],
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Value> {
    let server = decode_server(input, path, diagnostics)?;
    if let Some(record) = server.as_object() {
        if record.contains_key("codemode") {
            let mut path = path.to_vec();
            path.push("codemode".to_string());
            unsupported(&path, diagnostics);
        }
        if let Some(timeout) = record.get("timeout").and_then(Value::as_object) {
            if lower_timeout(timeout).is_none() && !timeout.is_empty() {
                let mut path = path.to_vec();
                path.push("timeout".to_string());
                unsupported(&path, diagnostics);
            }
        }
    }
    let mut lowered = lower_server(&server);
    if let Some(raw) = input.as_object() {
        if let Some(enabled) = raw.get("enabled") {
            if let Some(enabled) = enabled.as_bool() {
                if let Some(object) = lowered.as_object_mut() {
                    object.insert("enabled".to_string(), json!(enabled));
                }
            }
        }
    }
    Some(lowered)
}

fn decode_server(
    value: &Value,
    path: &[String],
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Value> {
    let Some(record) = value.as_object() else {
        invalid(path, diagnostics);
        return None;
    };
    let is_local = record.get("type").and_then(Value::as_str) == Some("local");
    let is_remote = record.get("type").and_then(Value::as_str) == Some("remote");
    if is_local {
        if record.get("command").and_then(Value::as_array).is_none() {
            invalid(path, diagnostics);
            return None;
        }
    } else if is_remote {
        if record.get("url").and_then(Value::as_str).is_none() {
            invalid(path, diagnostics);
            return None;
        }
    } else {
        invalid(path, diagnostics);
        return None;
    }
    Some(value.clone())
}

/// `lowerServer` (v2-compat.ts:333-357).
fn lower_server(input: &Value) -> Value {
    let record = input.as_object().expect("validated by decode_server");
    let mut result = record.clone();
    result.remove("disabled");
    result.remove("codemode");
    result.remove("timeout");
    result.remove("type");
    let disabled = record
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    result.insert("enabled".to_string(), json!(!disabled));
    if let Some(timeout) = record.get("timeout").and_then(Value::as_object) {
        if let Some(timeout) = lower_timeout(timeout) {
            result.insert("timeout".to_string(), json!(timeout));
        }
    }
    if record.get("type").and_then(Value::as_str) == Some("remote") {
        if let Some(oauth) = record.get("oauth").and_then(Value::as_object) {
            let mut lowered = Map::new();
            if let Some(value) = oauth.get("client_id") {
                lowered.insert("clientId".to_string(), value.clone());
            }
            if let Some(value) = oauth.get("client_secret") {
                lowered.insert("clientSecret".to_string(), value.clone());
            }
            if let Some(value) = oauth.get("scope") {
                lowered.insert("scope".to_string(), value.clone());
            }
            if let Some(value) = oauth.get("callback_port") {
                lowered.insert("callbackPort".to_string(), value.clone());
            }
            if let Some(value) = oauth.get("redirect_uri") {
                lowered.insert("redirectUri".to_string(), value.clone());
            }
            result.insert("oauth".to_string(), Value::Object(lowered));
        }
    }
    Value::Object(result)
}

/// `lowerTimeout` (v2-compat.ts:331-337) — only an equal catalog/execution
/// pair lowers (no `startup`).
fn lower_timeout(input: &Map<String, Value>) -> Option<u64> {
    if input.contains_key("startup") {
        return None;
    }
    match (
        input.get("catalog").and_then(Value::as_u64),
        input.get("execution").and_then(Value::as_u64),
    ) {
        (Some(catalog), Some(execution)) if catalog == execution => Some(catalog),
        _ => None,
    }
}

/// `normalizeLsp` (v2-compat.ts:345-361).
fn normalize_lsp(
    input: &Map<String, Value>,
    result: &mut Map<String, Value>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(lsp) = input.get("lsp").and_then(Value::as_object) else {
        return;
    };
    let lowered: Map<String, Value> = lsp
        .iter()
        .filter(|(name, value)| {
            if crate::config::schema::lsp_builtin_server_ids().contains(&name.as_str()) {
                return true;
            }
            let Some(entry) = value.as_object() else {
                return true;
            };
            if entry.get("disabled") == Some(&json!(true)) {
                return true;
            }
            if entry.get("extensions").is_some() {
                return true;
            }
            unsupported(&["lsp".to_string(), name.to_string()], diagnostics);
            false
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    result.insert("lsp".to_string(), Value::Object(lowered));
}

/// `normalizeLoadedConfig` (config.ts:54-63) — the legacy keys never reach
/// the schema.
pub fn normalize_loaded_config(data: &Value) -> Value {
    let Some(record) = data.as_object() else {
        return data.clone();
    };
    let has_legacy = record.contains_key("theme")
        || record.contains_key("keybinds")
        || record.contains_key("tui");
    if !has_legacy {
        return data.clone();
    }
    let mut copy = record.clone();
    copy.remove("theme");
    copy.remove("keybinds");
    copy.remove("tui");
    Value::Object(copy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_rejects_v2_permissions() {
        let result = lower(&json!({ "permissions": { "bash": { "deny": ["rm -rf"] } } }));
        assert!(result.is_err());
    }

    #[test]
    fn lower_rejects_agent_v2_permissions() {
        let result = lower(&json!({
            "agents": { "build": { "permissions": {} } }
        }));
        assert!(result.is_err());
    }

    #[test]
    fn lower_maps_snapshots_to_snapshot() {
        let result = lower(&json!({ "snapshots": true })).unwrap();
        assert_eq!(result.value["snapshot"], json!(true));
    }

    #[test]
    fn lower_maps_media_to_attachment() {
        let result = lower(&json!({ "media": { "maxMB": 10 } })).unwrap();
        assert_eq!(result.value["attachment"], json!({ "maxMB": 10 }));
    }

    #[test]
    fn lower_maps_model_selection() {
        let result = lower(&json!({ "model": { "providerID": "anthropic", "model": "claude", "variant": "high" } }))
            .unwrap();
        assert_eq!(result.value["model"], json!("anthropic/claude"));
        assert_eq!(result.value["variant"], json!("high"));
    }

    #[test]
    fn lower_splits_string_selection() {
        let result = lower(&json!({ "model": "anthropic/claude#max" })).unwrap();
        assert_eq!(result.value["model"], json!("anthropic/claude"));
        assert_eq!(result.value["variant"], json!("max"));
    }

    #[test]
    fn lower_rejects_malformed_selection() {
        // A malformed selection stays for the final V1 decoder rather than
        // being sanitized away (normalizeModel returns early).
        let result = lower(&json!({ "model": { "providerID": "", "model": "claude" } })).unwrap();
        assert_eq!(
            result.value.get("model"),
            Some(&json!({ "providerID": "", "model": "claude" }))
        );
    }

    #[test]
    fn lower_splits_skills_into_paths_and_urls() {
        let result = lower(&json!({
            "skills": ["/local/skills", "https://example.com/skill"]
        }))
        .unwrap();
        assert_eq!(result.value["skills"]["paths"], json!(["/local/skills"]));
        assert_eq!(
            result.value["skills"]["urls"],
            json!(["https://example.com/skill"])
        );
    }

    #[test]
    fn lower_flags_conflicting_legacy_values() {
        let result = lower(&json!({
            "experimental": { "subagent_depth": 3 },
            "subagent_depth": 2
        }))
        .unwrap();
        assert_eq!(result.value["subagent_depth"], json!(2));
        assert!(result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == DiagnosticKind::Conflict));
    }

    #[test]
    fn lower_maps_compaction_keep_tokens() {
        let result = lower(&json!({
            "compaction": { "keep": { "tokens": 1000 }, "buffer": 500 }
        }))
        .unwrap();
        assert_eq!(
            result.value["compaction"]["preserve_recent_tokens"],
            json!(1000)
        );
        assert_eq!(result.value["compaction"]["reserved"], json!(500));
    }

    #[test]
    fn lower_maps_experimental_subagent_depth() {
        let result = lower(&json!({ "experimental": { "subagent_depth": 3 } })).unwrap();
        assert_eq!(result.value["subagent_depth"], json!(3));
    }

    #[test]
    fn lower_agents_merge_into_legacy_agent() {
        let result = lower(&json!({
            "agents": { "build": { "system": "be a builder", "mode": "subagent" } }
        }))
        .unwrap();
        assert_eq!(
            result.value["agent"]["build"]["prompt"],
            json!("be a builder")
        );
        assert_eq!(result.value["agent"]["build"]["mode"], json!("subagent"));
    }

    #[test]
    fn lower_agent_conflicts_reported() {
        let result = lower(&json!({
            "agent": { "build": { "description": "legacy" } },
            "agents": { "build": { "description": "native" } }
        }))
        .unwrap();
        assert_eq!(
            result.value["agent"]["build"]["description"],
            json!("legacy")
        );
        assert!(result
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == DiagnosticKind::Conflict));
    }

    #[test]
    fn lower_agent_disabled_and_model() {
        let result = lower(&json!({
            "agents": {
                "review": {
                    "disabled": true,
                    "model": { "providerID": "openai", "model": "gpt" }
                }
            }
        }))
        .unwrap();
        assert_eq!(result.value["agent"]["review"]["disable"], json!(true));
        assert_eq!(
            result.value["agent"]["review"]["model"],
            json!("openai/gpt")
        );
    }

    #[test]
    fn lower_command_merges_and_lowers_model() {
        let result = lower(&json!({
            "commands": { "deploy": { "template": "deploy now", "model": "vercel/x#high" } }
        }))
        .unwrap();
        let command = &result.value["command"]["deploy"];
        assert_eq!(command["template"], json!("deploy now"));
        assert_eq!(command["model"], json!("vercel/x"));
        assert_eq!(command["variant"], json!("high"));
    }

    #[test]
    fn lower_mcp_local_and_remote_servers() {
        let result = lower(&json!({
            "mcp": {
                "typescript": {
                    "type": "local",
                    "command": ["npx", "tserver"],
                    "disabled": false,
                    "timeout": { "catalog": 5000, "execution": 5000 }
                },
                "docs": {
                    "type": "remote",
                    "url": "https://example.com",
                    "oauth": { "client_id": "abc", "client_secret": "secret", "scope": "read" }
                }
            }
        }))
        .unwrap();
        let mcp = &result.value["mcp"];
        assert_eq!(mcp["typescript"]["command"], json!(["npx", "tserver"]));
        assert_eq!(mcp["typescript"]["enabled"], json!(true));
        assert_eq!(mcp["typescript"]["timeout"], json!(5000));
        assert_eq!(mcp["docs"]["oauth"]["clientId"], json!("abc"));
        assert_eq!(mcp["docs"]["oauth"]["clientSecret"], json!("secret"));
    }

    #[test]
    fn lower_mcp_servers_envelope() {
        let result = lower(&json!({
            "mcp": {
                "servers": {
                    "inner": { "type": "local", "command": ["run"] }
                }
            }
        }))
        .unwrap();
        assert_eq!(result.value["mcp"]["inner"]["command"], json!(["run"]));
    }

    #[test]
    fn lower_mcp_global_timeout_into_experimental() {
        let result = lower(&json!({
            "mcp": { "timeout": { "catalog": 30000, "execution": 30000 } }
        }))
        .unwrap();
        assert_eq!(result.value["experimental"]["mcp_timeout"], json!(30000));
    }

    #[test]
    fn lower_lsp_drops_unsupported_custom_servers() {
        let result = lower(&json!({
            "lsp": {
                "rust": { "command": ["rust-analyzer"] },
                "custom": { "command": ["my-lsp"] }
            }
        }))
        .unwrap();
        assert!(result.value["lsp"].get("rust").is_some());
        assert!(result.value["lsp"].get("custom").is_none());
    }

    #[test]
    fn normalize_loaded_strips_legacy_keys() {
        let value = normalize_loaded_config(&json!({
            "theme": "dark",
            "keybinds": {},
            "tui": {},
            "model": "x/y"
        }));
        let record = value.as_object().unwrap();
        assert!(!record.contains_key("theme"));
        assert!(!record.contains_key("keybinds"));
        assert!(!record.contains_key("tui"));
        assert!(record.contains_key("model"));
    }
}
