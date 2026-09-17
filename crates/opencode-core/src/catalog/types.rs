//! models.dev catalog wire types — port of `packages/core/src/models-dev.ts:15-132`.
//!
//! `models-dev.ts` is authoritative; the JSON root of `{source}/api.json` is a
//! `Record<providerID, Provider>` (no envelope). Unknown fields are ignored
//! (`onExcessProperty: "ignore"`); every optional field is plain-absent when
//! missing, never `null`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `CatalogModelStatus` — models-dev.ts:15.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatalogModelStatus {
    Alpha,
    Beta,
    Deprecated,
}

/// Input/output modality — models-dev.ts:94.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    Text,
    Audio,
    Image,
    Video,
    Pdf,
}

/// `InterleavedField` — a known name or any string, so plain `String`.
pub type InterleavedField = String;

/// `Cost.tier` — models-dev.ts:30-33.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostTierType {
    #[serde(rename = "type")]
    pub kind: ContextTierType,
    pub size: f64,
}

/// The single literal `"context"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextTierType {
    #[serde(rename = "context")]
    Context,
}

/// `CostTier` — models-dev.ts:25-34.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostTier {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    pub tier: CostTierType,
}

/// `Cost.context_over_200k` — models-dev.ts:42-49.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextOver200k {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// `Cost` — models-dev.ts:36-50.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<CostTier>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_over_200k: Option<ContextOver200k>,
}

/// `ReasoningOption` — tagged union on `type` — models-dev.ts:52-65.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningOption {
    Effort {
        values: Vec<Option<String>>,
    },
    Toggle,
    BudgetTokens {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
    },
}

/// `Interleaved` — `bool | InterleavedField | { field: InterleavedField }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Interleaved {
    Boolean(bool),
    Field(InterleavedField),
    Struct { field: InterleavedField },
}

/// `Model.limit` — models-dev.ts:87-91.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelLimit {
    pub context: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    pub output: f64,
}

/// `Model.modalities` — models-dev.ts:92-97.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Modalities {
    pub input: Vec<Modality>,
    pub output: Vec<Modality>,
}

/// `Experimental.modes[*].provider` — models-dev.ts:104-110.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentalProvider {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
}

/// `Experimental.modes[*]` — models-dev.ts:99-115.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentalMode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ExperimentalProvider>,
}

/// `Model.experimental` — models-dev.ts:98-115.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Experimental {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<BTreeMap<String, ExperimentalMode>>,
}

/// `Model.provider` — models-dev.ts:117-119.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

/// `Model` — models-dev.ts:67-120.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    pub release_date: String,
    pub attachment: bool,
    pub reasoning: bool,
    pub temperature: bool,
    pub tool_call: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_options: Option<Vec<ReasoningOption>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interleaved: Option<Interleaved>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    pub limit: ModelLimit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Modalities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<Experimental>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<CatalogModelStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderInfo>,
}

/// `Provider` — models-dev.ts:123-130.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provider {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    pub name: String,
    pub env: Vec<String>,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    pub models: BTreeMap<String, Model>,
}

/// `Record<providerID, Provider>` — the `{source}/api.json` root.
pub type Providers = BTreeMap<String, Provider>;

/// Parsed `{source}/api.json` document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub providers: Providers,
}

impl Catalog {
    /// Parse the raw `{source}/api.json` text (the document root is the
    /// provider record itself; there is no envelope object).
    pub fn parse(text: &str) -> Result<Catalog, serde_json::Error> {
        let providers = serde_json::from_str::<Providers>(text)?;
        Ok(Catalog { providers })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_api_json() -> String {
        r#"{
  "anthropic": {
    "api": "https://api.anthropic.com",
    "name": "Anthropic",
    "env": ["ANTHROPIC_API_KEY"],
    "id": "anthropic",
    "npm": "@anthropic-ai/sdk",
    "models": {
      "claude-sonnet-4-5": {
        "id": "claude-sonnet-4-5",
        "name": "Claude Sonnet 4.5",
        "family": "claude",
        "release_date": "2025-09-29",
        "attachment": true,
        "reasoning": true,
        "temperature": true,
        "tool_call": true,
        "reasoning_options": [
          { "type": "effort", "values": ["low", null, "high"] },
          { "type": "toggle" },
          { "type": "budget_tokens", "min": 1024, "max": 65536 }
        ],
        "interleaved": { "field": "reasoning_content" },
        "cost": {
          "input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75,
          "tiers": [
            { "input": 3, "output": 15,
              "tier": { "type": "context", "size": 200000 } }
          ],
          "context_over_200k": { "input": 6, "output": 22.5 }
        },
        "limit": { "context": 200000, "input": 0.3, "output": 8192 },
        "modalities": { "input": ["text", "image", "pdf"], "output": ["text"] },
        "experimental": {
          "modes": {
            "fast": {
              "cost": { "input": 1.5, "output": 7.5 },
              "provider": {
                "body": { "thinking": { "type": "enabled" } },
                "headers": { "x-mode": "fast" }
              }
            }
          }
        },
        "status": "beta",
        "provider": { "npm": "@anthropic-ai/sdk", "api": "messages" }
      },
      "claude-haiku": {
        "id": "claude-haiku", "name": "Claude Haiku",
        "release_date": "2024-10-22",
        "attachment": false, "reasoning": false, "temperature": true, "tool_call": true,
        "limit": { "context": 200000, "output": 8192 },
        "interleaved": "reasoning_text"
      }
    }
  }
}"#
        .to_string()
    }

    #[test]
    fn parses_full_catalog() {
        let catalog = Catalog::parse(&full_api_json()).unwrap();
        assert_eq!(catalog.providers.len(), 1);
        let anthropic = catalog.providers.get("anthropic").unwrap();
        assert_eq!(anthropic.name, "Anthropic");
        assert_eq!(anthropic.env, vec!["ANTHROPIC_API_KEY".to_string()]);
        assert_eq!(anthropic.models.len(), 2);

        let model = &anthropic.models["claude-sonnet-4-5"];
        assert_eq!(model.family.as_deref(), Some("claude"));
        assert!(model.attachment && model.reasoning && model.tool_call);
        assert_eq!(model.status, Some(CatalogModelStatus::Beta));
        assert_eq!(
            &model.reasoning_options.as_ref().unwrap()[2],
            &ReasoningOption::BudgetTokens {
                min: Some(1024.0),
                max: Some(65536.0)
            }
        );
        assert_eq!(
            model.interleaved,
            Some(Interleaved::Struct {
                field: "reasoning_content".to_string()
            })
        );
        assert_eq!(model.cost.as_ref().unwrap().input, 3.0);
        assert_eq!(
            model.cost.as_ref().unwrap().tiers.as_ref().unwrap()[0]
                .tier
                .kind,
            ContextTierType::Context
        );
        assert_eq!(model.limit.output, 8192.0);
        assert_eq!(
            model.modalities.as_ref().unwrap().input,
            vec![Modality::Text, Modality::Image, Modality::Pdf]
        );
        assert_eq!(
            model.provider.as_ref().unwrap().api.as_deref(),
            Some("messages")
        );
    }

    #[test]
    fn parses_minimal_model_and_ignores_unknown_fields() {
        let text = r#"{
            "p": {
 "unknown-top": 1, "name": "P", "id": "p", "env": [], "unknown": ["x"],
                "models": { "m": { "id": "m", "name": "M", "release_date": "2020-01-01",
                     "attachment": false, "reasoning": false, "temperature": false,
                     "tool_call": false, "limit": {"context": 1, "output": 2},
                     "extra": null } }
            }
        }"#;
        let catalog = Catalog::parse(text).unwrap();
        assert_eq!(catalog.providers["p"].models["m"].limit.context, 1.0);
    }

    #[test]
    fn interleaved_accepts_bool_and_string() {
        let model = |interleaved: &str| {
            let prefix = concat!(
                r#"{"p":{"name":"P","id":"p","env":[],"models":{"m":{"#,
                r#""id":"m","name":"M","release_date":"d","attachment":false,"#,
                r#""reasoning":false,"temperature":false,"tool_call":false,"#,
                r#""limit":{"context":1,"output":2},"interleaved":"#,
            );
            let text = format!("{prefix}{interleaved}") + "}}}}";
            let model = Catalog::parse(&text).unwrap().providers["p"].models["m"].clone();
            model.interleaved
        };
        assert_eq!(model("true"), Some(Interleaved::Boolean(true)));
        assert_eq!(model("false"), Some(Interleaved::Boolean(false)));
        assert_eq!(
            model(r#""reasoning_content""#),
            Some(Interleaved::Field("reasoning_content".to_string()))
        );
    }
}
