//! Per-directory snapshots of the server state (`acp/directory.ts` +
//! `loadDirectorySnapshot`, service.ts:743-798).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::acp::server::ServerClient;

const PRIORITY: [&str; 4] = ["gpt-5", "claude-sonnet-4", "big-pickle", "gemini-3-pro"];

/// `Provider.sort` (provider.ts:2047-2056): priority index desc,
/// `latest` first, id desc; stable.
fn sort_key(id: &str) -> (i64, i64, std::cmp::Reverse<String>) {
    let priority = PRIORITY
        .iter()
        .position(|filter| id.contains(filter))
        .map(|index| index as i64)
        .unwrap_or(-1);
    let latest = if id.contains("latest") { 0 } else { 1 };
    (priority, latest, std::cmp::Reverse(id.to_string()))
}

/// `parseModel` (provider.ts:2056-2060).
pub fn parse_model(model: &str) -> Value {
    let (provider_id, model_id) = match model.split_once('/') {
        Some((provider_id, model_id)) => (provider_id, model_id),
        None => (model, ""),
    };
    json!({ "providerID": provider_id, "modelID": model_id })
}

/// `Directory.Snapshot` (directory.ts:33-42).
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub directory: String,
    /// provider id -> provider info (the `providers` record).
    pub providers: Value,
    pub model_options: Vec<Value>,
    /// `providerID/modelID` -> variants record.
    pub variants_by_model: HashMap<String, Value>,
    pub available_modes: Vec<Value>,
    pub default_mode_id: String,
    pub available_commands: Vec<Value>,
    pub default_model: Option<Value>,
}

impl Snapshot {
    /// `Directory.variants` (directory.ts:58-60).
    pub fn variants(&self, model: &Value) -> Option<&Value> {
        let key = format!(
            "{}/{}",
            model["providerID"].as_str().unwrap_or_default(),
            model["modelID"].as_str().unwrap_or_default()
        );
        self.variants_by_model.get(&key)
    }

    /// `selectDefaultModel` (service.ts:819-824).
    pub fn select_default_model(&self) -> Value {
        if let Some(default_model) = &self.default_model {
            return default_model.clone();
        }
        if let Some(model) = self.model_options.first() {
            return json!({
                "providerID": model["providerID"],
                "modelID": model["modelID"],
            });
        }
        json!({ "providerID": "unknown", "modelID": "unknown" })
    }

    /// `selectVariant` (service.ts:910-915).
    pub fn select_variant(&self, model: &Value) -> Option<String> {
        let variants = self.variants(model)?;
        let keys = variants
            .as_object()
            .map(|variants| variants.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        if let Some(default) = keys.iter().find(|key| key.as_str() == "default") {
            return Some(default.clone());
        }
        keys.first().cloned()
    }

    /// `hasMode` (service.ts:1150-1152).
    pub fn has_mode(&self, mode_id: &str) -> bool {
        self.available_modes
            .iter()
            .any(|mode| mode["id"] == json!(mode_id))
    }

    /// `hasModel` (service.ts:1146-1148).
    pub fn has_model(&self, model: &Value) -> bool {
        self.providers
            .get(model["providerID"].as_str().unwrap_or_default())
            .and_then(|provider| {
                provider
                    .get("models")
                    .and_then(|models| models.get(model["modelID"].as_str().unwrap_or_default()))
            })
            .is_some()
    }
}

/// `Directory.build` (directory.ts:62-105).
pub fn build(
    directory: &str,
    providers: Value,
    modes: Vec<Value>,
    default_mode_id: &str,
    commands: Vec<Value>,
    default_model: Option<Value>,
) -> Snapshot {
    // modelOptions — every model, sorted by `Provider.sort`.
    let mut options: Vec<(i64, i64, std::cmp::Reverse<String>, Value)> = Vec::new();
    let mut variants_by_model = HashMap::new();
    let empty = serde_json::Map::new();
    let provider_map = providers.as_object().unwrap_or(&empty);
    for (provider_id, provider) in provider_map {
        let Some(models) = provider.get("models").and_then(Value::as_object) else {
            continue;
        };
        for (model_id, model) in models {
            if let Some(variants) = model.get("variants") {
                variants_by_model.insert(format!("{provider_id}/{model_id}"), variants.clone());
            }
            let (priority, latest, reverse_id) = sort_key(model_id);
            options.push((
                priority,
                latest,
                reverse_id,
                json!({
                    "providerID": provider_id,
                    "providerName": provider.get("name").cloned().unwrap_or(Value::Null),
                    "modelID": model_id,
                    "modelName": model.get("name").cloned().unwrap_or(Value::Null),
                }),
            ));
        }
    }
    // stable ordering matching lodash sortBy with desc on the first two
    // priority desc, `latest` first, id desc (provider.ts:2046-2056)
    options.sort_by_key(|(priority, latest, reverse_id, _)| {
        (std::cmp::Reverse(*priority), *latest, reverse_id.clone())
    });

    let default_mode_id = if modes
        .iter()
        .any(|mode| mode["id"] == json!(default_mode_id))
    {
        default_mode_id.to_string()
    } else {
        modes
            .first()
            .and_then(|mode| mode["id"].as_str().map(str::to_string))
            .unwrap_or_else(|| default_mode_id.to_string())
    };

    Snapshot {
        directory: directory.to_string(),
        providers,
        model_options: options.into_iter().map(|(_, _, _, value)| value).collect(),
        variants_by_model,
        available_modes: modes,
        default_mode_id,
        available_commands: commands,
        default_model,
    }
}

/// `defaultModelFromConfig` (service.ts:800-817): configured model, then
/// opencode provider's best, then best overall, then configured-if-any.
fn default_model_from_config(configured_model: Option<&str>, providers: &Value) -> Option<Value> {
    let configured = configured_model.map(parse_model);
    if let Some(configured) = &configured {
        if providers
            .get(configured["providerID"].as_str().unwrap_or_default())
            .and_then(|provider| provider.get("models"))
            .and_then(|models| models.get(configured["modelID"].as_str().unwrap_or_default()))
            .is_some()
        {
            return Some(configured.clone());
        }
    }

    let mut best: Option<(i64, i64, std::cmp::Reverse<String>, Value)> = None;
    let mut opencode_best: Option<(i64, i64, std::cmp::Reverse<String>, Value)> = None;
    for (provider_id, provider) in providers.as_object().into_iter().flatten() {
        let Some(models) = provider.get("models").and_then(Value::as_object) else {
            continue;
        };
        for (model_id, _model) in models {
            let key = sort_key(model_id);
            let value = json!({ "providerID": provider_id, "modelID": model_id });
            if best
                .as_ref()
                .map(|(p, l, r, _)| (*p, *l, r) > (key.0, key.1, &key.2))
                .unwrap_or(true)
            {
                best = Some((key.0, key.1, key.2.clone(), value.clone()));
            }
            if provider_id == "opencode"
                && opencode_best
                    .as_ref()
                    .map(|(p, l, r, _)| (*p, *l, r) > (key.0, key.1, &key.2))
                    .unwrap_or(true)
            {
                opencode_best = Some((key.0, key.1, key.2.clone(), value.clone()));
            }
        }
    }
    if let Some((_, _, _, model)) = opencode_best {
        return Some(model);
    }
    if let Some((_, _, _, model)) = best {
        return Some(model);
    }
    configured.clone()
}

/// The cached per-directory loader (`directory.ts:144-201`).
#[derive(Clone)]
pub struct DirectoryService {
    server: ServerClient,
    snapshots: Arc<Mutex<HashMap<String, Arc<Snapshot>>>>,
}

impl DirectoryService {
    pub fn new(server: ServerClient) -> DirectoryService {
        DirectoryService {
            server,
            snapshots: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// `get` — cached per directory.
    pub async fn get(&self, directory: &str) -> Result<Arc<Snapshot>, String> {
        if let Some(snapshot) = self.snapshots.lock().unwrap().get(directory) {
            return Ok(snapshot.clone());
        }
        let snapshot = Arc::new(self.load(directory).await?);
        self.snapshots
            .lock()
            .unwrap()
            .insert(directory.to_string(), snapshot.clone());
        Ok(snapshot)
    }

    pub fn evict(&self, directory: &str) {
        self.snapshots.lock().unwrap().remove(directory);
    }

    /// `loadDirectorySnapshot` (service.ts:743-798) — five parallel
    /// fetches; the config fetch failure is tolerated.
    async fn load(&self, directory: &str) -> Result<Snapshot, String> {
        let server = self.server.clone();
        let directory_owned = directory.to_string();
        let directory_owned_2 = directory.to_string();
        let directory_owned_3 = directory.to_string();
        let directory_owned_4 = directory.to_string();
        let directory_owned_5 = directory.to_string();

        let (providers, agents, commands, skills, config) = tokio::join!(
            server.config_providers(&directory_owned),
            server.app_agents(&directory_owned_2),
            server.command_list(&directory_owned_3),
            server.app_skills(&directory_owned_4),
            server.config_get(&directory_owned_5),
        );

        let providers = providers?;
        let agents = agents?;
        let commands = commands?;
        let skills = skills?;
        let config = config.unwrap_or(Value::Null);

        let modes: Vec<Value> = agents
            .iter()
            .filter(|agent| agent["mode"] != json!("subagent") && agent["hidden"] != json!(true))
            .map(|agent| {
                let mut mode = json!({
                    "id": agent["name"],
                    "name": agent["name"],
                });
                if let Some(description) = agent.get("description") {
                    mode["description"] = description.clone();
                }
                mode
            })
            .collect();
        let default_mode_id = agents
            .iter()
            .find(|agent| agent["mode"] == json!("primary") && agent["hidden"] != json!(true))
            .and_then(|agent| agent["name"].as_str().map(str::to_string))
            .unwrap_or_else(|| "build".to_string());

        let mut merged_commands: Vec<Value> = commands;
        for skill in &skills {
            let name = skill["name"].as_str().unwrap_or_default();
            if merged_commands
                .iter()
                .any(|command| command["name"].as_str() == Some(name))
            {
                continue;
            }
            merged_commands.push(json!({
                "name": skill["name"],
                "description": skill["description"],
                "source": "skill",
                "template": skill["content"],
                "hints": [],
            }));
        }
        merged_commands.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or_default()
                .cmp(b["name"].as_str().unwrap_or_default())
        });

        // `Object.fromEntries(providers.map(p => [p.id, p]))`
        // (service.ts:762-765) — the route ships a list.
        let providers_value = match providers.get("providers").and_then(Value::as_array) {
            Some(list) => Value::Object(serde_json::Map::from_iter(list.iter().filter_map(
                |provider| {
                    let id = provider.get("id").and_then(Value::as_str)?;
                    Some((id.to_string(), provider.clone()))
                },
            ))),
            None => json!({}),
        };
        let default_model = default_model_from_config(
            config.get("model").and_then(Value::as_str),
            &providers_value,
        );

        Ok(build(
            directory,
            providers_value,
            modes,
            &default_mode_id,
            merged_commands,
            default_model,
        ))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const PROVIDERS: &str = r#"{
        "anthropic": {
            "id": "anthropic",
            "name": "Anthropic",
            "models": {
                "claude-3": { "id": "claude-3", "name": "Claude 3", "variants": { "high": {} } },
                "claude-sonnet-4": { "id": "claude-sonnet-4", "name": "Sonnet" }
            }
        },
        "opencode": {
            "id": "opencode",
            "name": "OpenCode",
            "models": { "roc": { "id": "roc", "name": "Roc" } }
        }
    }"#;

    #[test]
    fn build_sorts_models_by_priority() {
        let providers: Value = serde_json::from_str(PROVIDERS).unwrap();
        let snapshot = build(
            "/repo",
            providers,
            vec![json!({ "id": "build", "name": "build" })],
            "build",
            vec![],
            None,
        );
        let ids: Vec<&str> = snapshot
            .model_options
            .iter()
            .map(|model| model["modelID"].as_str().unwrap())
            .collect();
        assert_eq!(ids.first(), Some(&"claude-sonnet-4"));
        assert_eq!(snapshot.default_mode_id, "build");
        assert!(snapshot
            .variants(&json!({
                "providerID": "anthropic",
                "modelID": "claude-3"
            }))
            .is_some());
    }

    #[test]
    fn default_model_prefers_configured_then_opencode() {
        let providers: Value = serde_json::from_str(PROVIDERS).unwrap();
        // no configured model: opencode provider's best
        let default_model = default_model_from_config(None, &providers).unwrap();
        assert_eq!(
            default_model,
            json!({ "providerID": "opencode", "modelID": "roc" })
        );
        // configured model wins when valid
        let default_model =
            default_model_from_config(Some("anthropic/claude-3"), &providers).unwrap();
        assert_eq!(
            default_model,
            json!({ "providerID": "anthropic", "modelID": "claude-3" })
        );
    }

    #[test]
    fn default_mode_falls_back_to_first_mode() {
        let snapshot = build(
            "/repo",
            json!({}),
            vec![json!({ "id": "a", "name": "a" })],
            "missing",
            vec![],
            None,
        );
        assert_eq!(snapshot.default_mode_id, "a");
    }
}
