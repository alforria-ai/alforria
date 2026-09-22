//! `schema-src/model.ts`.

use serde::{Deserialize, Serialize};

use crate::ids::{ModelFamily, ModelId, ModelVariantId, ProviderId};
use crate::schema::JsonMap;

/// `Model.Ref` (openapi `ModelRef`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub id: ModelId,
    #[serde(rename = "providerID")]
    pub provider_id: ProviderId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<ModelVariantId>,
}

/// `Model.Capabilities` (openapi `ModelCapabilities`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCapabilities {
    pub tools: bool,
    pub input: Vec<String>,
    pub output: Vec<String>,
}

/// `Model.Cost` cache sub-object (openapi `ModelCost.cache`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostCache {
    #[serde(serialize_with = "crate::js_number::js_f64")]
    pub read: f64,
    #[serde(serialize_with = "crate::js_number::js_f64")]
    pub write: f64,
}

/// The single `tier.type` literal of `Model.Cost`: `"context"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCostTierType {
    Context,
}

/// `Model.Cost` tier sub-object (openapi `ModelCost.tier`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    #[serde(rename = "type")]
    pub type_: ModelCostTierType,
    pub size: i64,
}

/// `Model.Cost` (openapi `ModelCost`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<ModelCostTier>,
    #[serde(serialize_with = "crate::js_number::js_f64")]
    pub input: f64,
    #[serde(serialize_with = "crate::js_number::js_f64")]
    pub output: f64,
    pub cache: ModelCostCache,
}

/// `Model.Api` (openapi `ModelApi`): `aisdk` | `native`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ModelApi {
    #[serde(rename_all = "camelCase")]
    Aisdk {
        id: ModelId,
        package: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        settings: Option<JsonMap>,
    },
    #[serde(rename_all = "camelCase")]
    Native {
        id: ModelId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        settings: JsonMap,
    },
}

/// `Model.Info.request` (openapi `ModelV2Info.request`): `Provider.Request`
/// fields plus optional `variant`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRequest {
    pub headers: std::collections::BTreeMap<String, String>,
    pub body: JsonMap,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// `Model.Info.variants` items (openapi `ModelV2Info.variants`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelVariant {
    pub id: ModelVariantId,
    pub headers: std::collections::BTreeMap<String, String>,
    pub body: JsonMap,
}

/// `Model.Info.time` (openapi `ModelV2Info.time`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelTime {
    #[serde(serialize_with = "crate::js_number::js_f64")]
    pub released: f64,
}

/// `Model.Info.status` (openapi `ModelV2Info.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStatus {
    Alpha,
    Beta,
    Active,
    Deprecated,
}

/// `Model.Info.limit` (openapi `ModelV2Info.limit`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelLimit {
    pub context: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<i64>,
    pub output: i64,
}

/// `Model.Info` (openapi `ModelV2Info`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: ModelId,
    #[serde(rename = "providerID")]
    pub provider_id: ProviderId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<ModelFamily>,
    pub name: String,
    pub api: ModelApi,
    pub capabilities: ModelCapabilities,
    pub request: ModelRequest,
    pub variants: Vec<ModelVariant>,
    pub time: ModelTime,
    pub cost: Vec<ModelCost>,
    pub status: ModelStatus,
    pub enabled: bool,
    pub limit: ModelLimit,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::ModelApi;

    #[test]
    fn model_api_native_empty_settings_roundtrip() {
        let api = ModelApi::Native {
            id: "claude-sonnet-4".to_string(),
            url: None,
            settings: serde_json::Map::new(),
        };
        let value = serde_json::to_value(&api).unwrap();
        assert_eq!(
            value,
            json!({"id": "claude-sonnet-4", "type": "native", "settings": {}})
        );
        let roundtrip: ModelApi = serde_json::from_value(value).unwrap();
        assert_eq!(roundtrip, api);
    }
}
