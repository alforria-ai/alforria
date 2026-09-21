//! Config wire structs — port of `packages/core/src/v1/config/*`.
//!
//! The `opencode.json` contract (`config.ts` identifier `"Config"`). The
//! top-level [`Config`] struct uses **per-field renames** — the wire format
//! mixes camelCase (`logLevel`, `smallModel` is *not* one, but `logLevel`,
//! `default_agent`, `tool_output`, `mdnsDomain`, `maxSteps`, `openTelemetry`,
//! `apiKey`, `baseURL` all coexist) — so no blanket `rename_all` is used on
//! the top-level struct. Nested structs follow their TS counterparts
//! (snake_case with per-field renames where TS uses camelCase).
//!
//! Decode semantics (M3 spec §2.2/§2.3):
//!
//! * unknown fields are ignored (`onExcessProperty: "ignore"`);
//! * every optional field is plain-absent when missing, never `null`;
//! * `NonNegativeInt` → `u64`, `PositiveInt` → [`PositiveInt`] (rejects 0);
//! * [`Config::decode`] collects *all* top-level schema issues before failing
//!   (`errors: "all"`) as `CoreError::ConfigInvalid { path, issues }`;
//! * [`AgentInfo`] and [`PermissionInfo`] port their TS `normalize` transforms
//!   (unknown keys migrate into `options`, `tools` folds into `permission`).

use std::collections::BTreeMap;
use std::path::Path;

use serde::de::Deserializer;
use serde::{Deserialize, Serialize, Serializer};

use crate::CoreError;

pub const SCHEMA_REF: &str = "https://opencode.ai/config.json";

// ---------------------------------------------------------------------------
// Number types
// ---------------------------------------------------------------------------

/// `PositiveInt` — an integer `> 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PositiveInt(pub u64);

impl PositiveInt {
    pub fn get(self) -> u64 {
        self.0
    }
}

impl Serialize for PositiveInt {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for PositiveInt {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = PositiveInt;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a positive integer")
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                if v == 0 {
                    Err(E::custom("must be a positive integer, got 0"))
                } else {
                    Ok(PositiveInt(v))
                }
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                if v <= 0 {
                    Err(E::custom(format_args!(
                        "must be a positive integer, got {v}"
                    )))
                } else {
                    Ok(PositiveInt(v as u64))
                }
            }
        }
        deserializer.deserialize_u64(V)
    }
}

// ---------------------------------------------------------------------------
// Primitives & unions
// ---------------------------------------------------------------------------

/// `LogLevel` — `"DEBUG" | "INFO" | "WARN" | "ERROR"` (verbatim casing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LogLevel {
    #[serde(rename = "DEBUG")]
    Debug,
    #[serde(rename = "INFO")]
    Info,
    #[serde(rename = "WARN")]
    Warn,
    #[serde(rename = "ERROR")]
    Error,
}

/// `share` — `"manual" | "auto" | "disabled"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Share {
    Manual,
    Auto,
    Disabled,
}

/// `autoupdate` — `boolean | "notify"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Autoupdate {
    Boolean(bool),
    Notify(NotifyLiteral),
}

/// The literal `"notify"` (untagged unit variants match `null`, not
/// strings, so this is a validated newtype).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NotifyLiteral;

impl Serialize for NotifyLiteral {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("notify")
    }
}

impl<'de> Deserialize<'de> for NotifyLiteral {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value == "notify" {
            Ok(NotifyLiteral)
        } else {
            Err(serde::de::Error::custom("expected \"notify\""))
        }
    }
}

/// `layout` — `"auto" | "stretch"` (`@deprecated`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    Auto,
    Stretch,
}

/// Agent `color` — `#RRGGBB` or a theme-color name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Color(pub String);

const THEME_COLORS: &[&str] = &[
    "primary",
    "secondary",
    "accent",
    "success",
    "warning",
    "error",
    "info",
];

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let valid = raw.len() == 7
            && raw.starts_with('#')
            && raw[1..].chars().all(|c| c.is_ascii_hexdigit())
            || THEME_COLORS.contains(&raw.as_str());
        if !valid {
            return Err(serde::de::Error::custom(format_args!(
                "invalid color: {raw:?}"
            )));
        }
        Ok(Color(raw))
    }
}

/// `mode` — `"subagent" | "primary" | "all"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Subagent,
    Primary,
    All,
}

// ---------------------------------------------------------------------------
// server.ts
// ---------------------------------------------------------------------------

/// `Server` — `v1/config/server.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<PositiveInt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mdns: Option<bool>,
    #[serde(
        rename = "mdnsDomain",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub mdns_domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// command.ts (wire shape only — discovery lives in `config::command`)
// ---------------------------------------------------------------------------

/// `Info` — `v1/config/command.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandInfo {
    pub template: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtask: Option<bool>,
}

// ---------------------------------------------------------------------------
// skills.ts
// ---------------------------------------------------------------------------

/// `Info` — `v1/config/skills.ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillsInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// config/reference.ts
// ---------------------------------------------------------------------------

/// Git reference — `reference.ts` `Git`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceGit {
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

/// Local-directory reference — `reference.ts` `Local`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceLocal {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

/// `Entry` — `string | Git | Local`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReferenceEntry {
    Repository(String),
    Git(ReferenceGit),
    Local(ReferenceLocal),
}

/// `Info` — `Record<string, Entry>`.
pub type ReferenceInfo = BTreeMap<String, ReferenceEntry>;

// ---------------------------------------------------------------------------
// plugin.ts
// ---------------------------------------------------------------------------

/// `Spec` — `string | [string, Options]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginSpec {
    Name(String),
    Pair(String, BTreeMap<String, serde_json::Value>),
}

// ---------------------------------------------------------------------------
// permission.ts
// ---------------------------------------------------------------------------

/// `Action` — `"ask" | "allow" | "deny"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Ask,
    Allow,
    Deny,
}

/// `Rule` — `Action | Record<string, Action>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PermissionRule {
    Action(PermissionAction),
    Object(BTreeMap<String, PermissionAction>),
}

/// Keys typed as a bare `Action` (vs `Rule`) in `permission.ts`.
const PERMISSION_ACTION_KEYS: &[&str] = &[
    "todowrite",
    "question",
    "webfetch",
    "websearch",
    "doom_loop",
];

/// `Info` — `"ask" | "allow" | "deny" | Record<string, Rule>`.
///
/// A bare action string normalizes to `{"*": action}` (`normalizeInput`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PermissionInfo {
    pub rules: BTreeMap<String, PermissionRule>,
}

impl Serialize for PermissionInfo {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.rules.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PermissionInfo {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = PermissionInfo;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a permission action or record")
            }

            fn visit_str<E: serde::de::Error>(self, action: &str) -> Result<Self::Value, E> {
                let rule = serde_json::from_value::<PermissionRule>(serde_json::Value::String(
                    action.to_owned(),
                ))
                .map_err(serde::de::Error::custom)?;
                let mut rules = BTreeMap::new();
                rules.insert("*".to_owned(), rule);
                Ok(PermissionInfo { rules })
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut rules = BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    let value = map.next_value::<serde_json::Value>()?;
                    let rule: PermissionRule = if PERMISSION_ACTION_KEYS.contains(&key.as_str()) {
                        // Typed as a bare `Action` — a record is invalid here.
                        let action: PermissionAction =
                            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                        PermissionRule::Action(action)
                    } else {
                        match serde_json::from_value::<PermissionAction>(value.clone()) {
                            Ok(action) => PermissionRule::Action(action),
                            Err(_) => {
                                serde_json::from_value(value).map_err(serde::de::Error::custom)?
                            }
                        }
                    };
                    rules.insert(key, rule);
                }
                Ok(PermissionInfo { rules })
            }
        }
        deserializer.deserialize_any(V)
    }
}

// ---------------------------------------------------------------------------
// agent.ts
// ---------------------------------------------------------------------------

/// `Info` — `v1/config/agent.ts` with the `normalize` transform.
///
/// Decoding applies the TS `normalize`: unknown keys migrate into `options`
/// (the passthrough keys also stay at the top level), `tools` folds into
/// `permission` (write/edit/patch → `edit`; the agent's own `permission`
/// entries win), and `steps` falls back to `maxSteps`.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentInfo {
    pub model: Option<String>,
    pub variant: Option<String>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub prompt: Option<String>,
    pub tools: Option<BTreeMap<String, bool>>,
    pub disable: Option<bool>,
    pub description: Option<String>,
    pub mode: Option<AgentMode>,
    pub hidden: Option<bool>,
    pub color: Option<Color>,
    pub steps: Option<PositiveInt>,
    /// `maxSteps` — camelCase on the wire, `@deprecated` in favor of `steps`.
    pub max_steps: Option<PositiveInt>,
    /// Always an object after normalize.
    pub options: BTreeMap<String, serde_json::Value>,
    /// Always an object after normalize.
    pub permission: PermissionInfo,
    /// Keys outside the agent struct (includes `name`), preserved verbatim.
    pub rest: BTreeMap<String, serde_json::Value>,
}

impl AgentInfo {
    const STRUCT_KEYS: &'static [&'static str] = &[
        "model",
        "variant",
        "temperature",
        "top_p",
        "prompt",
        "tools",
        "disable",
        "description",
        "mode",
        "hidden",
        "options",
        "color",
        "steps",
        "maxSteps",
        "permission",
    ];

    /// Keys exempt from the unknown-key → `options` migration (`KNOWN_KEYS`).
    const KNOWN_KEYS: &'static [&'static str] = &[
        "name",
        "model",
        "variant",
        "prompt",
        "description",
        "temperature",
        "top_p",
        "mode",
        "hidden",
        "color",
        "steps",
        "maxSteps",
        "options",
        "permission",
        "disable",
        "tools",
    ];

    fn from_value(value: serde_json::Value) -> Result<Self, String> {
        #[derive(Default, Deserialize)]
        #[serde(rename_all = "snake_case")]
        struct Raw {
            model: Option<String>,
            variant: Option<String>,
            temperature: Option<f64>,
            #[serde(rename = "top_p")]
            top_p: Option<f64>,
            prompt: Option<String>,
            tools: Option<BTreeMap<String, bool>>,
            disable: Option<bool>,
            description: Option<String>,
            mode: Option<AgentMode>,
            hidden: Option<bool>,
            color: Option<Color>,
            steps: Option<PositiveInt>,
            #[serde(rename = "maxSteps")]
            max_steps: Option<PositiveInt>,
            options: Option<BTreeMap<String, serde_json::Value>>,
            permission: Option<PermissionInfo>,
        }

        let raw = serde_json::from_value::<Raw>(value.clone()).map_err(|e| e.to_string())?;

        let mut rest = BTreeMap::new();
        if let Some(entries) = value.as_object() {
            for (key, value) in entries {
                if !Self::STRUCT_KEYS.contains(&key.as_str()) {
                    rest.insert(key.clone(), value.clone());
                }
            }
        }

        // options = { ...options, ...unknown keys } (top-level wins).
        let mut options = raw.options.clone().unwrap_or_default();
        for (key, value) in &rest {
            if !Self::KNOWN_KEYS.contains(&key.as_str()) {
                options.insert(key.clone(), value.clone());
            }
        }

        // tools fold + `Object.assign(permission, agent.permission)`.
        let mut permission = PermissionInfo::default();
        if let Some(tools) = &raw.tools {
            for (tool, enabled) in tools {
                let action = if *enabled { "allow" } else { "deny" };
                if matches!(tool.as_str(), "write" | "edit" | "patch") {
                    permission
                        .rules
                        .insert("edit".to_owned(), PermissionRule::action(action));
                } else {
                    permission
                        .rules
                        .insert(tool.clone(), PermissionRule::action(action));
                }
            }
        }
        if let Some(agent_permission) = &raw.permission {
            for (key, rule) in &agent_permission.rules {
                permission.rules.insert(key.clone(), rule.clone());
            }
        }

        // steps ?? maxSteps
        let steps = raw.steps.or(raw.max_steps);

        Ok(AgentInfo {
            model: raw.model,
            variant: raw.variant,
            temperature: raw.temperature,
            top_p: raw.top_p,
            prompt: raw.prompt,
            tools: raw.tools,
            disable: raw.disable,
            description: raw.description,
            mode: raw.mode,
            hidden: raw.hidden,
            color: raw.color,
            steps,
            max_steps: raw.max_steps,
            options,
            permission,
            rest,
        })
    }
}

impl PermissionRule {
    fn action(name: &str) -> PermissionRule {
        // Panics only on a literal that is part of the wire enum.
        serde_json::from_value::<PermissionAction>(serde_json::Value::String(name.to_owned()))
            .map(PermissionRule::Action)
            .unwrap()
    }
}

impl Serialize for AgentInfo {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde_json::Value;
        let mut map = serde_json::Map::new();
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                map.insert(key.to_owned(), value);
            }
        };
        put("model", self.model.clone().map(Value::String));
        put("variant", self.variant.clone().map(Value::String));
        put(
            "temperature",
            self.temperature.map(|v| serde_json::json!(v)),
        );
        put("top_p", self.top_p.map(|v| serde_json::json!(v)));
        put("prompt", self.prompt.clone().map(Value::String));
        put("tools", self.tools.clone().map(|v| serde_json::json!(v)));
        put("disable", self.disable.map(Value::Bool));
        put("description", self.description.clone().map(Value::String));
        put("mode", self.mode.map(|v| serde_json::json!(v)));
        put("hidden", self.hidden.map(Value::Bool));
        put("color", self.color.clone().map(|c| serde_json::json!(c)));
        put("steps", self.steps.map(|v| serde_json::json!(v.get())));
        if self.max_steps.is_some() {
            put(
                "maxSteps",
                self.max_steps.map(|v| serde_json::json!(v.get())),
            );
        }
        for (key, value) in &self.rest {
            map.insert(key.clone(), value.clone());
        }
        map.insert(
            "options".to_owned(),
            serde_json::to_value(&self.options).map_err(serde::ser::Error::custom)?,
        );
        map.insert(
            "permission".to_owned(),
            serde_json::to_value(&self.permission).map_err(serde::ser::Error::custom)?,
        );
        map.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AgentInfo {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        AgentInfo::from_value(value).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// provider.ts
// ---------------------------------------------------------------------------

/// `provider.ts` `Model.cost.context_over_200k`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelCostOver {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// `provider.ts` `Model.cost`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelCost {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_over_200k: Option<ProviderModelCostOver>,
}

/// `provider.ts` `Model.limit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelLimit {
    pub context: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    pub output: f64,
}

/// `provider.ts` `Model.modalities`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelModalities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Vec<String>>,
}

/// `provider.ts` `Model.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderModelStatus {
    Alpha,
    Beta,
    Deprecated,
    Active,
}

/// `provider.ts` `Model.interleaved` — `bool | string | {field: string}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProviderModelInterleaved {
    Boolean(bool),
    Field(String),
    Struct { field: String },
}

/// `provider.ts` `Model.provider`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModelProvider {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

/// `provider.ts` `Model`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interleaved: Option<ProviderModelInterleaved>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ProviderModelCost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<ProviderModelLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<ProviderModelModalities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ProviderModelStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderModelProvider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variants: Option<BTreeMap<String, BTreeMap<String, serde_json::Value>>>,
}

/// `provider.ts` `Info.options` — known option keys with camelCase wire names
/// plus an open rest map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderOptions {
    #[serde(rename = "apiKey", default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(rename = "baseURL", default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(
        rename = "enterpriseUrl",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub enterprise_url: Option<String>,
    #[serde(
        rename = "setCacheKey",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub set_cache_key: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<TimeoutSetting>,
    #[serde(
        rename = "headerTimeout",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub header_timeout: Option<TimeoutSetting>,
    #[serde(
        rename = "chunkTimeout",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub chunk_timeout: Option<TimeoutSetting>,
    /// `Record<string, Any>` rest (via flatten).
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// `timeout` etc. — `PositiveInt | false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TimeoutSetting {
    Millis(PositiveInt),
    Disabled(bool),
}

/// `provider.ts` `Info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whitelist: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blacklist: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<ProviderOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<BTreeMap<String, ProviderModel>>,
}

// ---------------------------------------------------------------------------
// mcp.ts
// ---------------------------------------------------------------------------

/// `mcp.ts` `OAuth`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpOAuth {
    #[serde(rename = "clientId", default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(
        rename = "clientSecret",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(
        rename = "callbackPort",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub callback_port: Option<Port>,
    #[serde(
        rename = "redirectUri",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub redirect_uri: Option<String>,
}

/// `callbackPort` — an int between 1 and 65535.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct Port(pub u16);

impl<'de> Deserialize<'de> for Port {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Port;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an integer between 1 and 65535")
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                if v == 0 || v > 65535 {
                    Err(E::custom("must be between 1 and 65535"))
                } else {
                    Ok(Port(v as u16))
                }
            }
        }
        deserializer.deserialize_u64(V)
    }
}

/// `oauth` — `OAuth | false`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpOAuthSetting {
    OAuth(McpOAuth),
    Disabled(bool),
}

/// `mcp.ts` `Local`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpLocal {
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<PositiveInt>,
}

/// `mcp.ts` `Remote`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpRemote {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<PositiveInt>,
}

/// `mcp.ts` `Info` — discriminated on `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpInfo {
    Local(McpLocal),
    Remote(McpRemote),
}

/// Top-level `mcp` entry — `McpInfo | { enabled: boolean }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpEntry {
    Server(McpInfo),
    EnabledOnly { enabled: bool },
}

// ---------------------------------------------------------------------------
// formatter.ts
// ---------------------------------------------------------------------------

/// `formatter.ts` `Entry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormatterEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
}

/// `formatter.ts` `Info` — `boolean | Record<string, Entry>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FormatterInfo {
    Boolean(bool),
    Entries(BTreeMap<String, FormatterEntry>),
}

// ---------------------------------------------------------------------------
// lsp.ts
// ---------------------------------------------------------------------------

/// `lsp.ts` `Entry` — `{disabled: true}` or a server config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LspEntry {
    Disabled {
        disabled: TrueLiteral,
    },
    Server {
        command: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extensions: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disabled: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<BTreeMap<String, String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initialization: Option<BTreeMap<String, serde_json::Value>>,
    },
}

/// A boolean that must be `true` (`Schema.Literal(true)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TrueLiteral;

impl Serialize for TrueLiteral {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(true)
    }
}

impl<'de> Deserialize<'de> for TrueLiteral {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = bool::deserialize(deserializer)?;
        if value {
            Ok(TrueLiteral)
        } else {
            Err(serde::de::Error::custom("expected literal `true`"))
        }
    }
}

/// The builtin LSP server ids from `lsp.ts` (order preserved).
const LSP_BUILTIN_SERVER_IDS: &[&str] = &[
    "deno",
    "typescript",
    "vue",
    "eslint",
    "oxlint",
    "biome",
    "gopls",
    "ruby-lsp",
    "ty",
    "pyright",
    "elixir-ls",
    "zls",
    "csharp",
    "razor",
    "fsharp",
    "sourcekit-lsp",
    "rust",
    "clangd",
    "svelte",
    "astro",
    "jdtls",
    "kotlin-ls",
    "yaml-ls",
    "lua-ls",
    "php intelephense",
    "prisma",
    "dart",
    "ocaml-lsp",
    "bash",
    "terraform",
    "texlab",
    "dockerfile",
    "gleam",
    "clojure-lsp",
    "nixd",
    "tinymist",
    "haskell-language-server",
    "julials",
];

/// The builtin LSP server ids (`v2-compat.ts` reads `builtinServerIds`).
pub fn lsp_builtin_server_ids() -> &'static [&'static str] {
    LSP_BUILTIN_SERVER_IDS
}

/// `lsp.ts` `Info` — `boolean | Record<string, Entry>` with the
/// custom-servers-require-`extensions` check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LspInfo {
    Boolean(bool),
    Entries(BTreeMap<String, LspEntry>),
}

impl LspInfo {
    /// `requiresExtensionsForCustomServers`: custom (non-builtin) servers
    /// must declare `extensions` unless disabled.
    pub fn validate(&self) -> Result<(), String> {
        let entries = match self {
            LspInfo::Boolean(_) => return Ok(()),
            LspInfo::Entries(entries) => entries,
        };
        for (id, config) in entries {
            let needs_extensions = match config {
                LspEntry::Disabled { .. } => continue,
                LspEntry::Server {
                    disabled,
                    extensions,
                    ..
                } => !matches!(disabled, Some(true)) && extensions.is_none(),
            };
            if needs_extensions && !LSP_BUILTIN_SERVER_IDS.contains(&id.as_str()) {
                return Err("For custom LSP servers, 'extensions' array is required.".to_owned());
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// attachment.ts
// ---------------------------------------------------------------------------

/// `attachment.ts` `Image`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttachmentImage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_resize: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_width: Option<PositiveInt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<PositiveInt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_base64_bytes: Option<PositiveInt>,
}

/// `attachment.ts` `Info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttachmentInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<AttachmentImage>,
}

// ---------------------------------------------------------------------------
// misc top-level shapes
// ---------------------------------------------------------------------------

/// `watcher` — `{ ignore?: string[] }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatcherInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
}

/// `enterprise` — `{ url?: string }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnterpriseInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// `tool_output`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutputInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<PositiveInt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<PositiveInt>,
}

/// `compaction`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_turns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserve_recent_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved: Option<u64>,
}

/// `core/policy.ts` `Effect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyEffect {
    Allow,
    Deny,
}

/// `ConfigExperimental.Policy` — `Policy.Info` + `action: "provider.use"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub action: PolicyAction,
    pub effect: PolicyEffect,
    pub resource: String,
}

/// `Catalog.PolicyActions` — the literal `"provider.use"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PolicyAction {
    #[serde(rename = "provider.use")]
    ProviderUse,
}

/// `experimental` — note `openTelemetry` is camelCase among snake_case keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentalInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_paste_summary: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_tool: Option<bool>,
    #[serde(
        rename = "openTelemetry",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub open_telemetry: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_loop_on_deny: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_timeout: Option<PositiveInt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policies: Option<Vec<Policy>>,
}

// ---------------------------------------------------------------------------
// The Config struct
// ---------------------------------------------------------------------------

/// `ConfigV1.Info` — the `opencode.json` wire shape.
///
/// Wire casing is mixed (`logLevel`, `default_agent`, `tool_output`,
/// `mdnsDomain`, `maxSteps`, `openTelemetry`, …), so every field carries its
/// own rename; there is no blanket rename on this struct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(rename = "logLevel", default, skip_serializing_if = "Option::is_none")]
    pub log_level: Option<LogLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<BTreeMap<String, CommandInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<SkillsInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<ReferenceInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<ReferenceInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watcher: Option<WatcherInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<Vec<PluginSpec>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<Share>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoshare: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoupdate: Option<Autoupdate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_providers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_providers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_depth: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<BTreeMap<String, AgentInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<BTreeMap<String, AgentInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<BTreeMap<String, ProviderInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<BTreeMap<String, McpEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formatter: Option<FormatterInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsp: Option<LspInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<Layout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<BTreeMap<String, bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise: Option<EnterpriseInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<ToolOutputInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<ExperimentalInfo>,
}

/// Decode + validate a raw config value against the full wire schema.
///
/// Collects *all* top-level issues (TS `errors: "all"`) before failing, each
/// reported as `{ path: [field], message }` via `CoreError::ConfigInvalid`.
pub fn decode_config(value: &serde_json::Value, source: &Path) -> Result<Config, CoreError> {
    if !value.is_object() {
        return Err(CoreError::ConfigInvalid {
            path: source.to_path_buf(),
            message: None,
            issues: vec![crate::SchemaIssue {
                path: Vec::new(),
                message: "expected a JSON object".to_owned(),
            }],
        });
    }

    match serde_json::from_value::<Config>(value.clone()) {
        Ok(config) => {
            if let Some(lsp) = &config.lsp {
                lsp.validate().map_err(|message| CoreError::ConfigInvalid {
                    path: source.to_path_buf(),
                    message: None,
                    issues: vec![crate::SchemaIssue {
                        path: vec!["lsp".to_owned()],
                        message,
                    }],
                })?;
            }
            Ok(config)
        }
        Err(_) => Err(CoreError::ConfigInvalid {
            path: source.to_path_buf(),
            message: None,
            issues: collect_issues(value),
        }),
    }
}

/// Re-validate each top-level field individually to collect every issue
/// (`errors: "all"`), reporting one issue per offending field. Nested issue
/// paths are not walked — the serde message carries the nested detail.
fn collect_issues(value: &serde_json::Value) -> Vec<crate::SchemaIssue> {
    macro_rules! check {
        ($issues:expr, $key:expr, $ty:ty) => {
            if let Some(v) = value.get($key) {
                if let Err(err) = serde_json::from_value::<$ty>(v.clone()) {
                    $issues.push(crate::SchemaIssue {
                        path: vec![$key.to_owned()],
                        message: err.to_string(),
                    });
                }
            }
        };
    }

    let mut issues = Vec::new();
    let Some(object) = value.as_object() else {
        return issues;
    };
    for key in object.keys() {
        match key.as_str() {
            "$schema" => check!(issues, key, String),
            "shell" => check!(issues, key, String),
            "logLevel" => check!(issues, key, LogLevel),
            "server" => check!(issues, key, ServerInfo),
            "command" => check!(issues, key, BTreeMap<String, CommandInfo>),
            "skills" => check!(issues, key, SkillsInfo),
            "references" => check!(issues, key, ReferenceInfo),
            "reference" => check!(issues, key, ReferenceInfo),
            "watcher" => check!(issues, key, WatcherInfo),
            "snapshot" => check!(issues, key, bool),
            "plugin" => check!(issues, key, Vec<PluginSpec>),
            "share" => check!(issues, key, Share),
            "autoshare" => check!(issues, key, bool),
            "autoupdate" => check!(issues, key, Autoupdate),
            "disabled_providers" => check!(issues, key, Vec<String>),
            "enabled_providers" => check!(issues, key, Vec<String>),
            "model" => check!(issues, key, String),
            "small_model" => check!(issues, key, String),
            "default_agent" => check!(issues, key, String),
            "subagent_depth" => check!(issues, key, u64),
            "username" => check!(issues, key, String),
            "mode" => check!(issues, key, BTreeMap<String, AgentInfo>),
            "agent" => check!(issues, key, BTreeMap<String, AgentInfo>),
            "provider" => check!(issues, key, BTreeMap<String, ProviderInfo>),
            "mcp" => check!(issues, key, BTreeMap<String, McpEntry>),
            "formatter" => check!(issues, key, FormatterInfo),
            "lsp" => check!(issues, key, LspInfo),
            "instructions" => check!(issues, key, Vec<String>),
            "layout" => check!(issues, key, Layout),
            "permission" => check!(issues, key, PermissionInfo),
            "tools" => check!(issues, key, BTreeMap<String, bool>),
            "attachment" => check!(issues, key, AttachmentInfo),
            "enterprise" => check!(issues, key, EnterpriseInfo),
            "tool_output" => check!(issues, key, ToolOutputInfo),
            "compaction" => check!(issues, key, CompactionInfo),
            "experimental" => check!(issues, key, ExperimentalInfo),
            _ => {}
        }
    }
    issues
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;

    fn source() -> std::path::PathBuf {
        std::path::PathBuf::from("opencode.json")
    }

    #[test]
    fn decodes_minimal_config() {
        let config = decode_config(&json!({}), &source()).unwrap();
        assert_eq!(config.schema, None);
    }

    #[test]
    fn decodes_every_field_and_round_trips_keys() {
        // A raw JSON literal (not `json!`) — the fully-populated config is
        // big enough to blow the macro recursion limit.
        let value: Value = serde_json::from_str(
            r#"{
            "$schema": "https://opencode.ai/config.json",
            "shell": "/bin/bash",
            "logLevel": "DEBUG",
            "server": {
                "port": 4096, "hostname": "0.0.0.0", "mdns": true,
                "mdnsDomain": "lan.local", "cors": ["https://x.test"]
            },
            "command": { "deploy": { "template": "ship it", "agent": "build" } },
            "skills": { "paths": ["/abs/skills"], "urls": ["https://example.com/skills"] },
            "references": {
                "repo": "github.com/opencode/opencode",
                "git-full": { "repository": "a/b", "branch": "main", "hidden": false },
                "local": { "path": "/tmp" }
            },
            "reference": { "old": { "path": "/old" } },
            "watcher": { "ignore": ["dist/**"] },
            "snapshot": false,
            "plugin": ["./a.ts", ["./b.ts", { "opt": 1 }]],
            "share": "auto",
            "autoshare": true,
            "autoupdate": "notify",
            "disabled_providers": ["x"],
            "enabled_providers": ["y"],
            "model": "anthropic/claude-sonnet-4-5",
            "small_model": "anthropic/claude-haiku",
            "default_agent": "build",
            "subagent_depth": 2,
            "username": "jon",
            "mode": { "build": { "prompt": "m", "options": {}, "permission": {} } },
            "agent": {
                "plan": {
                    "model": "m", "mode": "primary", "options": {}, "permission": {}
                }
            },
            "provider": {
                "acme": {
                    "api": "https://api.acme.test", "name": "Acme", "env": ["ACME_KEY"],
                    "id": "acme", "npm": "acme-sdk", "whitelist": ["x"], "blacklist": ["y"],
                    "options": {
                        "apiKey": "k", "baseURL": "https://api.acme.test",
                        "enterpriseUrl": "https://gh.test", "setCacheKey": true,
                        "timeout": 5000, "headerTimeout": false, "chunkTimeout": 1000,
                        "extraOption": 7
                    },
                    "models": {
                        "big": {
                            "id": "big", "name": "Big", "family": "bigf",
                            "release_date": "2026-01-01", "attachment": true,
                            "reasoning": true, "temperature": true, "tool_call": true,
                            "interleaved": { "field": "reasoning" },
                            "cost": {
                                "input": 1.5, "output": 2.5, "cache_read": 0.1,
                                "cache_write": 0.2,
                                "context_over_200k": { "input": 3.5, "output": 4.5 }
                            },
                            "limit": { "context": 200000.5, "input": 1.5, "output": 8192.5 },
                            "modalities": { "input": ["text"], "output": ["text"] },
                            "experimental": true, "status": "beta",
                            "provider": { "npm": "acme-sdk", "api": "chat" },
                            "options": { "x": 1 }, "headers": { "a": "b" },
                            "variants": { "fast": { "disabled": false } }
                        }
                    }
                }
            },
            "mcp": {
                "local": { "type": "local", "command": ["npx", "x"], "cwd": "/tmp",
                            "environment": { "A": "B" }, "enabled": true, "timeout": 1000 },
                "remote": {
                    "type": "remote", "url": "https://mcp.test", "headers": { "h": "v" },
                    "oauth": { "clientId": "c", "clientSecret": "s", "scope": "sc",
                               "callbackPort": 1234, "redirectUri": "https://cb" },
                    "timeout": 50
                },
                "off": { "enabled": false }
            },
            "formatter": { "prettier": { "command": ["prettier"], "extensions": [".ts"],
                                          "environment": { "F": "V" }, "disabled": false } },
            "lsp": { "rust": { "command": ["rust-analyzer"] }, "off": { "disabled": true } },
            "instructions": ["AGENTS.md"],
            "layout": "stretch",
            "permission": {
                "edit": "allow", "bash": { "git push": "ask" }, "todowrite": "deny"
            },
            "tools": { "bash": true },
            "attachment": {
                "image": { "auto_resize": false, "max_width": 800, "max_height": 600,
                           "max_base64_bytes": 1024 }
            },
            "enterprise": { "url": "https://ent.test" },
            "tool_output": { "max_lines": 100, "max_bytes": 512 },
            "compaction": { "auto": true, "prune": false, "tail_turns": 1,
                             "preserve_recent_tokens": 2, "reserved": 3 },
            "experimental": {
                "disable_paste_summary": true, "batch_tool": true, "openTelemetry": false,
                "primary_tools": ["read"], "continue_loop_on_deny": true,
                "mcp_timeout": 7500,
                "policies": [ { "action": "provider.use", "effect": "allow",
                                 "resource": "provider/anthropic" } ]
            }
        }"#,
        )
        .unwrap();

        let config = decode_config(&value, &source()).unwrap();

        // Spot-check the mixed-casing hot spots.
        assert_eq!(config.log_level, Some(LogLevel::Debug));
        assert_eq!(config.default_agent.as_deref(), Some("build"));
        assert_eq!(
            config
                .tool_output
                .as_ref()
                .unwrap()
                .max_lines
                .unwrap()
                .get(),
            100
        );
        assert_eq!(
            config.experimental.as_ref().unwrap().open_telemetry,
            Some(false)
        );
        let acme = config.provider.as_ref().unwrap().get("acme").unwrap();
        assert_eq!(acme.options.as_ref().unwrap().api_key.as_deref(), Some("k"));
        assert_eq!(
            acme.options.as_ref().unwrap().extra.get("extraOption"),
            Some(&json!(7))
        );
        assert!(matches!(
            acme.options.as_ref().unwrap().header_timeout,
            Some(TimeoutSetting::Disabled(false))
        ));
        assert_eq!(
            config.mcp.as_ref().unwrap().get("off"),
            Some(&McpEntry::EnabledOnly { enabled: false })
        );
        assert_eq!(
            config
                .experimental
                .as_ref()
                .unwrap()
                .policies
                .as_ref()
                .unwrap()[0]
                .action,
            PolicyAction::ProviderUse
        );

        // Round-trip: serialize and compare parsed keys/values.
        let reserialized = serde_json::to_value(&config).unwrap();
        assert_eq!(reserialized, value);
    }

    #[test]
    fn unknown_top_level_fields_are_ignored() {
        let config = decode_config(&json!({ "totally-unknown": 1 }), &source()).unwrap();
        assert_eq!(config.schema, None);
    }

    #[test]
    fn collects_all_issues() {
        let err = decode_config(
            &json!({ "logLevel": "nope", "subagent_depth": -1, "shell": 5 }),
            &source(),
        )
        .unwrap_err();
        match err {
            CoreError::ConfigInvalid { issues, .. } => {
                let paths: Vec<String> = issues.iter().flat_map(|i| i.path.clone()).collect::<_>();
                assert!(paths.contains(&"logLevel".to_owned()), "{issues:?}");
                assert!(paths.contains(&"subagent_depth".to_owned()), "{issues:?}");
                assert!(paths.contains(&"shell".to_owned()), "{issues:?}");
                assert_eq!(issues.len(), 3);
            }
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_object_config() {
        let err = decode_config(&json!([1, 2]), &source()).unwrap_err();
        match err {
            CoreError::ConfigInvalid { issues, .. } => {
                assert!(issues[0].path.is_empty());
            }
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
    }

    #[test]
    fn positive_int_rejects_zero() {
        assert!(decode_config(&json!({ "tool_output": { "max_lines": 0 } }), &source()).is_err());
        assert!(decode_config(&json!({ "tool_output": { "max_lines": 1 } }), &source()).is_ok());
    }

    #[test]
    fn lsp_custom_server_requires_extensions() {
        let err = decode_config(
            &json!({ "lsp": { "my-custom-lsp": { "command": ["x"] } } }),
            &source(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("extensions"));

        let ok_builtin = json!({ "lsp": { "rust": { "command": ["rust-analyzer"] } } });
        assert!(decode_config(&ok_builtin, &source()).is_ok());

        let ok_disabled = json!({
            "lsp": { "my-custom-lsp": { "command": ["x"], "disabled": true } }
        });
        assert!(decode_config(&ok_disabled, &source()).is_ok());
    }

    #[test]
    fn permission_accepts_bare_action_and_records() {
        let config = decode_config(&json!({ "permission": "allow" }), &source()).unwrap();
        let permission = config.permission.unwrap();
        assert_eq!(
            permission.rules.get("*"),
            Some(&PermissionRule::Action(PermissionAction::Allow))
        );

        let config = decode_config(
            &json!({ "permission": { "bash": { "git push": "ask" } } }),
            &source(),
        )
        .unwrap();
        assert!(config.permission.unwrap().rules.contains_key("bash"));

        assert!(decode_config(&json!({ "permission": "nope" }), &source()).is_err());
        // `todowrite` is a bare Action key — a record there is invalid.
        assert!(decode_config(
            &json!({ "permission": { "todowrite": { "x": "allow" } } }),
            &source(),
        )
        .is_err());
    }

    #[test]
    fn agent_normalizes_like_ts() {
        let config = decode_config(
            &json!({
                "agent": {
                    "plan": {
                        "name": "Plan",
                        "unknown": { "nested": 1 },
                        "tools": { "write": true, "read": false, "edit": true },
                        "steps": 5,
                        "maxSteps": 9,
                        "permission": { "bash": "deny" },
                    }
                }
            }),
            &source(),
        )
        .unwrap();
        let agent = config.agent.unwrap();
        let plan = agent.get("plan").unwrap();

        // steps ?? maxSteps: `steps` wins.
        assert_eq!(plan.steps.map(|s| s.get()), Some(5));
        assert_eq!(plan.max_steps.map(|s| s.get()), Some(9));
        // tools fold; write → edit.
        assert_eq!(
            plan.permission.rules.get("edit"),
            Some(&PermissionRule::Action(PermissionAction::Allow))
        );
        assert_eq!(
            plan.permission.rules.get("read"),
            Some(&PermissionRule::Action(PermissionAction::Deny))
        );
        // own permission wins over the folded value
        assert_eq!(
            plan.permission.rules.get("bash"),
            Some(&PermissionRule::Action(PermissionAction::Deny))
        );
        // unknown keys land in options…
        assert_eq!(plan.options.get("unknown"), Some(&json!({ "nested": 1 })));
        // …but `name` is a KNOWN_KEY: kept in rest, not migrated to options.
        assert_eq!(plan.rest.get("name"), Some(&json!("Plan")));
        assert!(!plan.options.contains_key("name"));
    }

    #[test]
    fn agent_max_steps_fallback() {
        let config = decode_config(
            &json!({ "agent": { "plan": { "maxSteps": 9 } } }),
            &source(),
        )
        .unwrap();
        assert_eq!(
            config.agent.unwrap()["plan"].steps.map(|s| s.get()),
            Some(9)
        );
    }
}
