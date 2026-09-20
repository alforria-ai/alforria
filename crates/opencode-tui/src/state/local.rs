//! `local.tsx` — agent/model/session/mcp local state (M8.2).
//!
//! TS reference: `context/local.tsx` (542 lines). The permission mode
//! context (`context/permission.tsx`) lives in `state/mod.rs`.
//! `agent`, `provider`, `config` stay `serde_json::Value` — the same
//! dynamic typing the TS store keeps (no M1 DTOs exist for them).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::state::kv::{read_json, write_json_atomic};
use crate::state::route::Route;
use crate::state::sync::SyncState;
use crate::state::{Args, State, Toast, ToastVariant};

/// `{ providerID, modelID }` — openapi model reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

impl ModelRef {
    fn key(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }
}

/// `parseModel` (`local.tsx:27-33`).
pub fn parse_model(model: &str) -> ModelRef {
    let (provider_id, model_id) = match model.split_once('/') {
        Some((provider_id, rest)) => (provider_id, rest),
        None => (model, ""),
    };
    ModelRef {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
    }
}

/// `recentModels` (`local.tsx:35-49`): newest first, dedup, max 10.
pub fn recent_models(model: &ModelRef, recent: &[ModelRef]) -> Vec<ModelRef> {
    let mut seen = HashSet::new();
    let mut items: Vec<ModelRef> = Vec::new();
    for item in std::iter::once(model).chain(recent.iter()) {
        if seen.insert(item.key()) {
            items.push(item.clone());
        }
    }
    items.into_iter().take(10).collect()
}

/// `LocalTheme` palette rotation (`local.tsx:83-91`) — theme keys.
pub const AGENT_COLOR_ROTATION: [&str; 7] = [
    "secondary",
    "accent",
    "success",
    "warning",
    "primary",
    "error",
    "info",
];

/// An agent color: `#hex` or a theme key (`local.tsx:119-131`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentColor {
    Hex(String),
    Theme(String),
}

/// `mcp.toggle` result — the driver performs the connect/disconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpAction {
    Connect(String),
    Disconnect(String),
}

#[derive(Debug, Clone, Default)]
pub struct ModelLocal {
    /// Per-agent selected model (`modelStore.model`).
    pub selected: BTreeMap<String, ModelRef>,
    pub recent: Vec<ModelRef>,
    pub favorite: Vec<ModelRef>,
    pub variant: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
struct AgentLocal {
    current: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct SessionLocal {
    pinned: Vec<String>,
}

/// The `Local` context (`local.tsx:51-541`). Persistence
/// (`model.json` / `session.json`, `local.tsx:164/420`) only applies
/// when constructed with a state dir.
#[derive(Debug, Clone, Default)]
pub struct LocalState {
    agent: AgentLocal,
    model: ModelLocal,
    session: SessionLocal,
    state_dir: Option<std::path::PathBuf>,
}

impl LocalState {
    pub fn new(state_dir: Option<&Path>) -> LocalState {
        let mut local = LocalState {
            state_dir: state_dir.map(Path::to_path_buf),
            ..LocalState::default()
        };
        local.load_model();
        local.load_session();
        local
    }

    // -------------------------------------------------------- agent

    /// Non-subagent, non-hidden agents (`local.tsx:78`).
    fn agent_values(sync: &SyncState) -> Vec<&Value> {
        sync.agent
            .iter()
            .filter(|a| agent_mode(a) != "subagent" && !agent_hidden(a))
            .collect()
    }

    fn visible_agent_values(sync: &SyncState) -> Vec<&Value> {
        sync.agent.iter().filter(|a| !agent_hidden(a)).collect()
    }

    /// `agent.current()` (`local.tsx:97`).
    pub fn agent_current<'a>(&self, sync: &'a SyncState) -> Option<&'a Value> {
        let agents = Self::agent_values(sync);
        agents
            .iter()
            .find(|a| agent_name(a) == self.agent.current.as_deref())
            .or_else(|| agents.first())
            .copied()
    }

    /// `agent.set(name)` (`local.tsx:99-107`) — toast on miss.
    pub fn agent_set(&mut self, name: &str, sync: &SyncState) -> Option<Toast> {
        if !Self::agent_values(sync)
            .iter()
            .any(|a| agent_name(a) == Some(name))
        {
            return Some(Toast {
                variant: ToastVariant::Warning,
                message: format!("Agent not found: {name}"),
                duration_ms: 3000,
            });
        }
        self.agent.current = Some(name.to_string());
        None
    }

    /// `agent.move(direction)` (`local.tsx:108-118`) — wrap-around.
    pub fn agent_move(&mut self, direction: i32, sync: &SyncState) {
        let agents = Self::agent_values(sync);
        let Some(current) = self.agent_current(sync) else {
            return;
        };
        let name = agent_name(current).unwrap_or_default();
        let Some(index) = agents.iter().position(|a| agent_name(a) == Some(name)) else {
            return;
        };
        let len = agents.len();
        let next = if direction < 0 {
            if index == 0 {
                len - 1
            } else {
                index - 1
            }
        } else if index + 1 >= len {
            0
        } else {
            index + 1
        };
        if let Some(next) = agents.get(next) {
            self.agent.current = agent_name(next).map(str::to_string);
        }
    }

    /// `agent.color(name)` (`local.tsx:119-131`).
    pub fn agent_color(&self, name: &str, sync: &SyncState) -> AgentColor {
        let agents = Self::visible_agent_values(sync);
        let Some(index) = agents.iter().position(|a| agent_name(a) == Some(name)) else {
            return AgentColor::Theme(AGENT_COLOR_ROTATION[0].to_string());
        };
        let agent = agents[index];
        if let Some(color) = agent.get("color").and_then(Value::as_str) {
            if color.starts_with('#') {
                return AgentColor::Hex(color.to_string());
            }
            return AgentColor::Theme(color.to_string());
        }
        AgentColor::Theme(AGENT_COLOR_ROTATION[index % AGENT_COLOR_ROTATION.len()].to_string())
    }

    // -------------------------------------------------------- model

    fn model_info<'a>(sync: &'a SyncState, model: &ModelRef) -> Option<&'a Value> {
        sync.provider
            .iter()
            .find(|p| p.get("id").and_then(Value::as_str) == Some(model.provider_id.as_str()))
            .and_then(|p| p.get("models")?.as_object()?.get(&model.model_id))
    }

    fn is_model_valid(&self, sync: &SyncState, model: &ModelRef) -> bool {
        Self::model_info(sync, model).is_some()
    }

    /// `fallbackModel` (`local.tsx:197-234`): `--model` arg → config
    /// model → first valid recent → first provider's default/first
    /// model.
    fn fallback_model(&self, sync: &SyncState, args: &Args) -> Option<ModelRef> {
        if let Some(model) = &args.model {
            let parsed = parse_model(model);
            if self.is_model_valid(sync, &parsed) {
                return Some(parsed);
            }
        }
        if let Some(config_model) = sync.config.get("model").and_then(Value::as_str) {
            let parsed = parse_model(config_model);
            if self.is_model_valid(sync, &parsed) {
                return Some(parsed);
            }
        }
        for item in &self.model.recent {
            if self.is_model_valid(sync, item) {
                return Some(item.clone());
            }
        }
        let provider = sync.provider.first()?;
        let provider_id = provider.get("id").and_then(Value::as_str)?;
        let default_model = sync.provider_default.get(provider_id);
        let first_model = provider
            .get("models")
            .and_then(Value::as_object)
            .and_then(|models| models.values().next())
            .and_then(|info| info.get("id"))
            .and_then(Value::as_str);
        let model = default_model.map(String::as_str).or(first_model)?;
        Some(ModelRef {
            provider_id: provider_id.to_string(),
            model_id: model.to_string(),
        })
    }

    /// `currentModel` (`local.tsx:236-245`): per-agent selected →
    /// agent's configured model → fallback, first valid wins.
    pub fn model_current(&self, sync: &SyncState, args: &Args) -> Option<ModelRef> {
        let agent = self.agent_current(sync)?;
        let name = agent_name(agent)?;
        if let Some(selected) = self.model.selected.get(name) {
            if self.is_model_valid(sync, selected) {
                return Some(selected.clone());
            }
        }
        if let Some(model) = agent.get("model") {
            let parsed = ModelRef {
                provider_id: model.get("providerID").and_then(Value::as_str)?.to_string(),
                model_id: model.get("modelID").and_then(Value::as_str)?.to_string(),
            };
            if self.is_model_valid(sync, &parsed) {
                return Some(parsed);
            }
        }
        self.fallback_model(sync, args)
    }

    /// `parsed` memo (`local.tsx:258-274`).
    pub fn model_parsed(&self, sync: &SyncState, args: &Args) -> ParsedModel {
        let Some(current) = self.model_current(sync, args) else {
            return ParsedModel {
                provider: "Connect a provider".to_string(),
                model: "No provider selected".to_string(),
                reasoning: false,
            };
        };
        let info = Self::model_info(sync, &current);
        let provider = sync
            .provider
            .iter()
            .find(|p| p.get("id").and_then(Value::as_str) == Some(current.provider_id.as_str()));
        ParsedModel {
            provider: provider
                .and_then(|p| p.get("name").and_then(Value::as_str))
                .unwrap_or(&current.provider_id)
                .to_string(),
            model: info
                .and_then(|i| i.get("name").and_then(Value::as_str))
                .unwrap_or(&current.model_id)
                .to_string(),
            reasoning: info
                .and_then(|i| i.get("capabilities"))
                .and_then(|c| c.get("reasoning"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }

    /// `cycle(direction)` (`local.tsx:275-289`) — over recent, no save.
    pub fn model_cycle(&mut self, direction: i32, sync: &SyncState, args: &Args) {
        let Some(current) = self.model_current(sync, args) else {
            return;
        };
        let recent = &self.model.recent;
        let Some(index) = recent.iter().position(|x| *x == current) else {
            return;
        };
        let len = recent.len();
        let next = if direction < 0 {
            if index == 0 {
                len - 1
            } else {
                index - 1
            }
        } else if index + 1 >= len {
            0
        } else {
            index + 1
        };
        let Some(value) = recent.get(next) else {
            return;
        };
        let Some(agent) = self.agent_current(sync) else {
            return;
        };
        let Some(name) = agent_name(agent) else {
            return;
        };
        self.model.selected.insert(name.to_string(), value.clone());
    }

    /// `cycleFavorite(direction)` (`local.tsx:290-319`).
    pub fn model_cycle_favorite(
        &mut self,
        direction: i32,
        sync: &SyncState,
        args: &Args,
    ) -> Option<Toast> {
        let favorites: Vec<ModelRef> = self
            .model
            .favorite
            .iter()
            .filter(|item| self.is_model_valid(sync, item))
            .cloned()
            .collect();
        if favorites.is_empty() {
            return Some(Toast {
                variant: ToastVariant::Info,
                message: "Add a favorite model to use this shortcut".to_string(),
                duration_ms: 3000,
            });
        }
        let current = self.model_current(sync, args);
        let index = match current
            .as_ref()
            .and_then(|c| favorites.iter().position(|x| x == c))
        {
            Some(index) => {
                let next = index as i64 + direction as i64;
                if next < 0 {
                    favorites.len() - 1
                } else if next >= favorites.len() as i64 {
                    0
                } else {
                    next as usize
                }
            }
            None => {
                if direction < 0 {
                    favorites.len() - 1
                } else {
                    0
                }
            }
        };
        let next = favorites[index].clone();
        let agent = self.agent_current(sync)?;
        let name = agent_name(agent)?;
        self.model.selected.insert(name.to_string(), next.clone());
        self.model.recent = recent_models(&next, &self.model.recent);
        self.save_model();
        None
    }

    /// `set(model, { recent })` (`local.tsx:320-338`).
    pub fn model_set(&mut self, sync: &SyncState, model: ModelRef, recent: bool) -> Option<Toast> {
        if !self.is_model_valid(sync, &model) {
            return Some(Toast {
                variant: ToastVariant::Warning,
                message: format!(
                    "Model {}/{} is not valid",
                    model.provider_id, model.model_id
                ),
                duration_ms: 3000,
            });
        }
        let agent = self.agent_current(sync)?;
        let name = agent_name(agent)?.to_string();
        self.model.selected.insert(name, model.clone());
        if recent {
            self.model.recent = recent_models(&model, &self.model.recent);
            self.save_model();
        }
        None
    }

    /// `toggleFavorite(model)` (`local.tsx:339-361`).
    pub fn model_toggle_favorite(&mut self, sync: &SyncState, model: &ModelRef) -> Option<Toast> {
        if !self.is_model_valid(sync, model) {
            return Some(Toast {
                variant: ToastVariant::Warning,
                message: format!(
                    "Model {}/{} is not valid",
                    model.provider_id, model.model_id
                ),
                duration_ms: 3000,
            });
        }
        let exists = self
            .model
            .favorite
            .iter()
            .any(|x| x.provider_id == model.provider_id && x.model_id == model.model_id);
        let next = if exists {
            self.model
                .favorite
                .iter()
                .filter(|x| x.provider_id != model.provider_id || x.model_id != model.model_id)
                .cloned()
                .collect()
        } else {
            vec![model.clone()]
                .into_iter()
                .chain(self.model.favorite.iter().cloned())
                .collect()
        };
        self.model.favorite = next;
        self.save_model();
        None
    }

    pub fn model_recent(&self) -> &[ModelRef] {
        &self.model.recent
    }

    pub fn model_favorite(&self) -> &[ModelRef] {
        &self.model.favorite
    }

    fn variant_key(&self, sync: &SyncState, args: &Args) -> Option<(String, Vec<String>)> {
        let current = self.model_current(sync, args)?;
        let variants = Self::model_info(sync, &current)
            .and_then(|info| info.get("variants"))
            .and_then(Value::as_object)
            .map(|variants| variants.keys().cloned().collect())
            .unwrap_or_default();
        Some((current.key(), variants))
    }

    /// `variant.selected()` (`local.tsx:363-368`).
    pub fn variant_selected(&self, sync: &SyncState, args: &Args) -> Option<String> {
        let (key, _) = self.variant_key(sync, args)?;
        self.model.variant.get(&key).cloned()
    }

    /// `variant.current()` (`local.tsx:369-374`).
    pub fn variant_current(&self, sync: &SyncState, args: &Args) -> Option<String> {
        let selected = self.variant_selected(sync, args)?;
        let (_, variants) = self.variant_key(sync, args)?;
        if variants.contains(&selected) {
            Some(selected)
        } else {
            None
        }
    }

    /// `variant.list()` (`local.tsx:375-382`).
    pub fn variant_list(&self, sync: &SyncState, args: &Args) -> Vec<String> {
        self.variant_key(sync, args)
            .map(|(_, variants)| variants)
            .unwrap_or_default()
    }

    /// `variant.set(value)` (`local.tsx:383-389`) — `None` wraps to
    /// `"default"`.
    pub fn variant_set(&mut self, sync: &SyncState, args: &Args, value: Option<&str>) {
        let Some((key, _)) = self.variant_key(sync, args) else {
            return;
        };
        self.model
            .variant
            .insert(key, value.unwrap_or("default").to_string());
        self.save_model();
    }

    /// `variant.cycle()` (`local.tsx:390-404`) — wraps to `default`.
    pub fn variant_cycle(&mut self, sync: &SyncState, args: &Args) {
        let variants = self.variant_list(sync, args);
        if variants.is_empty() {
            return;
        }
        let Some(current) = self.variant_current(sync, args) else {
            self.variant_set(sync, args, Some(&variants[0]));
            return;
        };
        let index = variants.iter().position(|v| *v == current);
        match index {
            Some(index) if index + 1 < variants.len() => {
                self.variant_set(sync, args, Some(&variants[index + 1]))
            }
            _ => self.variant_set(sync, args, None),
        }
    }

    // ------------------------------------------------------- session

    /// `slots` memo (`local.tsx:452-455`): first 9 pinned ids that
    /// still exist as parentless sessions.
    pub fn session_slots(&self, sync: &SyncState) -> Vec<String> {
        let existing: HashSet<&str> = sync
            .session
            .iter()
            .filter(|s| s.parent_id.is_none())
            .map(|s| s.id.as_str())
            .collect();
        self.session
            .pinned
            .iter()
            .filter(|id| existing.contains(&id.as_str()))
            .take(9)
            .cloned()
            .collect()
    }

    pub fn session_pinned(&self) -> &[String] {
        &self.session.pinned
    }

    pub fn session_is_pinned(&self, session_id: &str) -> bool {
        self.session.pinned.iter().any(|id| id == session_id)
    }

    /// `togglePin(sessionID)` (`local.tsx:484-493`).
    pub fn session_toggle_pin(&mut self, session_id: &str) {
        if self.session.pinned.iter().any(|id| id == session_id) {
            self.session.pinned.retain(|id| id != session_id);
        } else {
            self.session.pinned.push(session_id.to_string());
        }
        self.save_session();
    }

    /// `prune(sessionID)` (`local.tsx:457-467`) — wired to
    /// `session.deleted` (`:469-471`).
    pub fn session_prune(&mut self, session_id: &str) {
        self.session.pinned.retain(|id| id != session_id);
        self.save_session();
    }

    /// `quickSwitch(slot)` (`local.tsx:494-499`): 1-based slot; no-op
    /// when already on the session. Returns the route to navigate to.
    pub fn session_quick_switch(
        &self,
        sync: &SyncState,
        route: &Route,
        slot: usize,
    ) -> Option<Route> {
        let target = self.session_slots(sync).get(slot.checked_sub(1)?)?.clone();
        if let Route::Session { session_id, .. } = route {
            if *session_id == target {
                return None;
            }
        }
        Some(Route::Session {
            session_id: target,
            prompt: None,
        })
    }

    // ----------------------------------------------------------- mcp

    /// `mcp.isEnabled(name)` (`local.tsx:506-509`).
    pub fn mcp_is_enabled(&self, sync: &SyncState, name: &str) -> bool {
        sync.mcp
            .get(name)
            .and_then(|s| s.get("status"))
            .and_then(Value::as_str)
            == Some("connected")
    }

    /// `mcp.toggle(name)` (`local.tsx:510-519`).
    pub fn mcp_toggle(&self, sync: &SyncState, name: &str) -> McpAction {
        if self.mcp_is_enabled(sync, name) {
            McpAction::Disconnect(name.to_string())
        } else {
            McpAction::Connect(name.to_string())
        }
    }

    // ----------------------------------------------------- persistence

    fn load_model(&mut self) {
        let Some(dir) = &self.state_dir else {
            return;
        };
        let Ok(value) = read_json(&dir.join("model.json")) else {
            return;
        };
        if let Some(recent) = value.get("recent").and_then(Value::as_array) {
            self.model.recent = recent
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect();
        }
        if let Some(favorite) = value.get("favorite").and_then(Value::as_array) {
            self.model.favorite = favorite
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect();
        }
        if let Some(variant) = value.get("variant") {
            if let Ok(variant) = serde_json::from_value::<BTreeMap<String, String>>(variant.clone())
            {
                self.model.variant = variant;
            }
        }
    }

    fn load_session(&mut self) {
        let Some(dir) = &self.state_dir else {
            return;
        };
        let Ok(value) = read_json(&dir.join("session.json")) else {
            return;
        };
        if let Some(pinned) = value.get("pinned").and_then(Value::as_array) {
            self.session.pinned = pinned
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect();
        }
    }

    fn save_model(&self) {
        let Some(dir) = &self.state_dir else {
            return;
        };
        let value = json!({
            "recent": self.model.recent,
            "favorite": self.model.favorite,
            "variant": self.model.variant,
        });
        if let Err(error) = write_json_atomic(&dir.join("model.json"), &value) {
            eprintln!("Failed to write model state: {error}");
        }
    }

    fn save_session(&self) {
        let Some(dir) = &self.state_dir else {
            return;
        };
        let value = json!({
            "pinned": self.session.pinned,
        });
        if let Err(error) = write_json_atomic(&dir.join("session.json"), &value) {
            eprintln!("Failed to write session state: {error}");
        }
    }
}

/// `parsed` memo result (`local.tsx:258-274`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedModel {
    pub provider: String,
    pub model: String,
    pub reasoning: bool,
}

/// `session.deleted` listener (`local.tsx:469-471`).
pub fn on_bus_event(state: &mut State, event: &opencode_schema::event_manifest::Event) {
    if let opencode_schema::event_manifest::Event::SessionDeleted(data) = event {
        state.local.session_prune(&data.info.id);
    }
}

fn agent_name(agent: &Value) -> Option<&str> {
    agent.get("name").and_then(Value::as_str)
}

fn agent_mode(agent: &Value) -> &str {
    agent.get("mode").and_then(Value::as_str).unwrap_or("")
}

fn agent_hidden(agent: &Value) -> bool {
    agent
        .get("hidden")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencode_schema::session_v1::V1SessionInfo;

    fn sync_with_providers() -> SyncState {
        let mut sync = SyncState::new();
        sync.provider = vec![json!({
            "id": "anthropic",
            "name": "Anthropic",
            "models": {
                "claude-sonnet": {
                    "id": "claude-sonnet",
                    "name": "Claude Sonnet",
                    "capabilities": {"reasoning": true},
                    "variants": {"fast": {}},
                },
                "claude-haiku": {"id": "claude-haiku"},
            },
        })];
        sync.provider_default
            .insert("anthropic".into(), "claude-sonnet".into());
        sync.agent = vec![
            json!({"name": "build", "mode": "primary"}),
            json!({"name": "plan", "mode": "primary", "hidden": false}),
            json!({"name": "secret", "mode": "primary", "hidden": true}),
            json!({"name": "sub", "mode": "subagent"}),
        ];
        sync
    }

    fn haiku() -> ModelRef {
        ModelRef {
            provider_id: "anthropic".into(),
            model_id: "claude-haiku".into(),
        }
    }

    #[test]
    fn parse_model_splits_only_the_first_slash() {
        let parsed = parse_model("anthropic/claude/sonnet");
        assert_eq!(parsed.provider_id, "anthropic");
        assert_eq!(parsed.model_id, "claude/sonnet");
        let bare = parse_model("noprovider");
        assert_eq!(
            (bare.provider_id.as_str(), bare.model_id.as_str()),
            ("noprovider", "")
        );
    }

    #[test]
    fn recent_models_dedups_and_caps_at_10() {
        let first = haiku();
        let recent: Vec<ModelRef> = (0..12)
            .map(|i| ModelRef {
                provider_id: "p".into(),
                model_id: i.to_string(),
            })
            .collect();
        let result = recent_models(&first, &recent);
        assert_eq!(result.len(), 10);
        assert_eq!(result[0], first);
        assert_eq!(result[1], recent[0]);
        assert!(recent_models(&first, std::slice::from_ref(&first)).len() == 1);
    }

    #[test]
    fn agent_list_and_current_rules() {
        let sync = sync_with_providers();
        let mut local = LocalState::new(None);
        assert_eq!(
            local.agent_current(&sync).and_then(agent_name),
            Some("build")
        );
        let toast = local.agent_set("nope", &sync).expect("toast");
        assert_eq!(toast.message, "Agent not found: nope");
        assert!(local.agent_set("plan", &sync).is_none());
        assert_eq!(
            local.agent_current(&sync).and_then(agent_name),
            Some("plan")
        );
        // plan -> build (wrap)
        local.agent_move(1, &sync);
        assert_eq!(
            local.agent_current(&sync).and_then(agent_name),
            Some("build")
        );
        // build -> plan
        local.agent_move(1, &sync);
        assert_eq!(
            local.agent_current(&sync).and_then(agent_name),
            Some("plan")
        );
        // plan -> back over the top
        local.agent_move(-1, &sync);
        assert_eq!(
            local.agent_current(&sync).and_then(agent_name),
            Some("build")
        );
    }

    #[test]
    fn agent_color_rotation_and_overrides() {
        let mut sync = sync_with_providers();
        let local = LocalState::new(None);
        assert_eq!(
            local.agent_color("build", &sync),
            AgentColor::Theme("secondary".into())
        );
        assert_eq!(
            local.agent_color("plan", &sync),
            AgentColor::Theme("accent".into())
        );
        assert_eq!(
            local.agent_color("missing", &sync),
            AgentColor::Theme("secondary".into())
        );
        sync.agent[0] = json!({"name": "build", "mode": "primary", "color": "#ff0000"});
        assert_eq!(
            local.agent_color("build", &sync),
            AgentColor::Hex("#ff0000".into())
        );
        sync.agent[0] = json!({"name": "build", "mode": "primary", "color": "primary"});
        assert_eq!(
            local.agent_color("build", &sync),
            AgentColor::Theme("primary".into())
        );
    }

    #[test]
    fn model_fallback_order() {
        let sync = sync_with_providers();
        let local = LocalState::new(None);
        let args = Args::default();

        // No args, no config, no recent → provider default model.
        assert_eq!(
            local.model_current(&sync, &args).unwrap().model_id,
            "claude-sonnet"
        );

        // Invalid --model falls through to the provider default.
        let args = Args {
            model: Some("openai/gpt".into()),
            ..Args::default()
        };
        assert_eq!(
            local.model_current(&sync, &args).unwrap().model_id,
            "claude-sonnet"
        );

        // Valid --model wins over everything.
        let args = Args {
            model: Some("anthropic/claude-haiku".into()),
            ..Args::default()
        };
        assert_eq!(
            local.model_current(&sync, &args).unwrap().model_id,
            "claude-haiku"
        );

        // config.model wins over the provider default; recent comes
        // after config in the fallback order.
        let args = Args::default();
        let mut with_config = sync.clone();
        with_config.config = json!({"model": "anthropic/claude-haiku"});
        let mut with_recent = local.clone();
        with_recent.model.recent = vec![haiku()];
        assert_eq!(
            with_recent
                .model_current(&with_config, &args)
                .unwrap()
                .model_id,
            "claude-haiku",
            "config model wins over recent"
        );
        let mut config_sonnet = with_config.clone();
        config_sonnet.config = json!({"model": "anthropic/claude-sonnet"});
        assert_eq!(
            with_recent
                .model_current(&config_sonnet, &args)
                .unwrap()
                .model_id,
            "claude-sonnet"
        );
    }

    #[test]
    fn model_parsed_labels() {
        let sync = sync_with_providers();
        let local = LocalState::new(None);
        let parsed = local.model_parsed(&sync, &Args::default());
        assert_eq!(parsed.provider, "Anthropic");
        assert_eq!(parsed.model, "Claude Sonnet");
        assert!(parsed.reasoning);
        let empty = SyncState::new();
        let parsed = local.model_parsed(&empty, &Args::default());
        assert_eq!(parsed.provider, "Connect a provider");
        assert_eq!(parsed.model, "No provider selected");
    }

    #[test]
    fn model_set_invalid_warns_and_valid_sets() {
        let sync = sync_with_providers();
        let args = Args::default();
        let mut local = LocalState::new(None);
        let toast = local
            .model_set(
                &sync,
                ModelRef {
                    provider_id: "x".into(),
                    model_id: "y".into(),
                },
                false,
            )
            .expect("warns");
        assert_eq!(toast.message, "Model x/y is not valid");
        assert!(local.model_set(&sync, haiku(), true).is_none());
        assert_eq!(local.model_recent().len(), 1);
        assert_eq!(
            local.model_current(&sync, &args).unwrap().model_id,
            "claude-haiku"
        );
    }

    #[test]
    fn favorites_toggle_and_cycle() {
        let sync = sync_with_providers();
        let args = Args::default();
        let mut local = LocalState::new(None);
        let toast = local
            .model_cycle_favorite(1, &sync, &args)
            .expect("empty favorites toast");
        assert_eq!(toast.message, "Add a favorite model to use this shortcut");
        local.model_toggle_favorite(&sync, &haiku());
        assert_eq!(local.model_favorite().len(), 1);
        local.model_toggle_favorite(&sync, &haiku());
        assert!(local.model_favorite().is_empty());
        local.model_toggle_favorite(&sync, &haiku());
        assert!(local.model_cycle_favorite(1, &sync, &args).is_none());
        assert_eq!(
            local.model_current(&sync, &args).unwrap(),
            haiku(),
            "cycleFavorite selects the favorite"
        );
        assert_eq!(local.model_recent().len(), 1);
    }

    #[test]
    fn variant_cycle_wraps_to_default() {
        let sync = sync_with_providers();
        let args = Args::default();
        let mut local = LocalState::new(None);
        local.model_set(
            &sync,
            ModelRef {
                provider_id: "anthropic".into(),
                model_id: "claude-sonnet".into(),
            },
            false,
        );
        assert_eq!(local.variant_list(&sync, &args), vec!["fast".to_string()]);
        local.variant_cycle(&sync, &args);
        assert_eq!(
            local.variant_selected(&sync, &args),
            Some("fast".to_string())
        );
        local.variant_cycle(&sync, &args);
        assert_eq!(
            local.variant_selected(&sync, &args),
            Some("default".to_string()),
            "cycle wraps to default"
        );
        assert!(local.variant_current(&sync, &args).is_none());
    }

    #[test]
    fn model_json_persists_with_ts_wire_shape() {
        let dir = tempfile::tempdir().unwrap();
        let mut local = LocalState::new(Some(dir.path()));
        local.model_toggle_favorite(&sync_with_providers(), &haiku());
        let sync = sync_with_providers();
        let args = Args::default();
        local.model_set(&sync, haiku(), true);
        local.variant_set(&sync, &args, Some("fast"));
        let raw = std::fs::read_to_string(dir.path().join("model.json")).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap(),
            json!({
                "recent": [{"providerID": "anthropic", "modelID": "claude-haiku"}],
                "favorite": [{"providerID": "anthropic", "modelID": "claude-haiku"}],
                "variant": {"anthropic/claude-haiku": "fast"},
            })
        );
        let reloaded = LocalState::new(Some(dir.path()));
        assert_eq!(reloaded.model_recent().len(), 1);
        assert_eq!(reloaded.model_favorite().len(), 1);
        assert_eq!(
            reloaded.variant_selected(&sync, &args),
            Some("fast".to_string())
        );
    }

    #[test]
    fn session_json_persists_with_ts_wire_shape() {
        let dir = tempfile::tempdir().unwrap();
        let mut local = LocalState::new(Some(dir.path()));
        local.session_toggle_pin("ses_1");
        let raw = std::fs::read_to_string(dir.path().join("session.json")).unwrap();
        assert_eq!(raw, r#"{"pinned":["ses_1"]}"#);
        assert!(LocalState::new(Some(dir.path())).session_is_pinned("ses_1"));
    }

    #[test]
    fn session_pins_prune_and_quick_switch() {
        let mut sync = sync_with_providers();
        sync.session = vec![
            V1SessionInfo {
                id: "ses_a".into(),
                parent_id: None,
                directory: "/a".into(),
                ..session_fixture()
            },
            V1SessionInfo {
                id: "ses_child".into(),
                parent_id: Some("ses_a".into()),
                ..session_fixture()
            },
            V1SessionInfo {
                id: "ses_b".into(),
                parent_id: None,
                ..session_fixture()
            },
        ];
        let mut local = LocalState::new(None);
        local.session_toggle_pin("ses_a");
        local.session_toggle_pin("ses_child");
        local.session_toggle_pin("ses_b");
        // ses_child has a parent — not a quick-switch slot.
        assert_eq!(
            local.session_slots(&sync),
            vec!["ses_a".to_string(), "ses_b".to_string()]
        );
        assert!(local.session_is_pinned("ses_a"));
        let route = local
            .session_quick_switch(&sync, &Route::Home { prompt: None }, 1)
            .expect("navigates");
        assert!(matches!(route, Route::Session { session_id, .. } if session_id == "ses_a"));
        assert!(
            local
                .session_quick_switch(
                    &sync,
                    &Route::Session {
                        session_id: "ses_a".into(),
                        prompt: None,
                    },
                    1
                )
                .is_none(),
            "no-op when already on the session"
        );
        assert!(local
            .session_quick_switch(&sync, &Route::Home { prompt: None }, 9)
            .is_none());
        local.session_prune("ses_a");
        assert!(!local.session_is_pinned("ses_a"));
        assert_eq!(local.session_slots(&sync), vec!["ses_b".to_string()]);
    }

    #[test]
    fn mcp_toggle_action() {
        let mut sync = sync_with_providers();
        sync.mcp.insert("on".into(), json!({"status": "connected"}));
        sync.mcp.insert("off".into(), json!({"status": "error"}));
        let local = LocalState::new(None);
        assert_eq!(
            local.mcp_toggle(&sync, "on"),
            McpAction::Disconnect("on".into())
        );
        assert_eq!(
            local.mcp_toggle(&sync, "off"),
            McpAction::Connect("off".into())
        );
        assert_eq!(
            local.mcp_toggle(&sync, "unknown"),
            McpAction::Connect("unknown".into())
        );
    }

    #[test]
    fn model_cycle_rotates_recent() {
        let sync = sync_with_providers();
        let args = Args::default();
        let mut local = LocalState::new(None);
        local.model.recent = vec![
            ModelRef {
                provider_id: "anthropic".into(),
                model_id: "claude-sonnet".into(),
            },
            haiku(),
        ];
        local.model_cycle(1, &sync, &args);
        assert_eq!(
            local.model_current(&sync, &args).unwrap().model_id,
            "claude-haiku"
        );
        local.model_cycle(1, &sync, &args);
        assert_eq!(
            local.model_current(&sync, &args).unwrap().model_id,
            "claude-sonnet"
        );
        local.model_cycle(-1, &sync, &args);
        assert_eq!(
            local.model_current(&sync, &args).unwrap().model_id,
            "claude-haiku"
        );
    }

    fn session_fixture() -> V1SessionInfo {
        V1SessionInfo {
            id: "ses_x".into(),
            slug: "x".into(),
            project_id: "prj".into(),
            workspace_id: None,
            directory: "/x".into(),
            path: None,
            parent_id: None,
            summary: None,
            cost: None,
            tokens: None,
            share: None,
            title: "X".into(),
            agent: None,
            model: None,
            version: "1".into(),
            metadata: None,
            time: opencode_schema::session_v1::V1SessionTime {
                created: 1,
                updated: 1,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert: None,
        }
    }
}
