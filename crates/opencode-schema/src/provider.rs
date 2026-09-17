//! `schema-src/provider.ts`.

use serde::{Deserialize, Serialize};

use crate::ids::{IntegrationId, ProviderId};
use crate::schema::JsonMap;

/// `Provider.Api` (openapi `ProviderApi`): `aisdk` | `native`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ProviderApi {
    #[serde(rename_all = "camelCase")]
    Aisdk {
        package: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        settings: Option<JsonMap>,
    },
    #[serde(rename_all = "camelCase")]
    Native {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        settings: JsonMap,
    },
}

/// `Provider.Request` (openapi `ProviderRequest`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRequest {
    pub headers: std::collections::BTreeMap<String, String>,
    pub body: JsonMap,
}

/// `Provider.Info` (openapi `ProviderV2Info`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub id: ProviderId,
    #[serde(
        rename = "integrationID",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub integration_id: Option<IntegrationId>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    pub api: ProviderApi,
    pub request: ProviderRequest,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ProviderApi, ProviderInfo, ProviderRequest};

    #[test]
    fn provider_info_roundtrip_omits_optional_keys() {
        let value = json!({
            "id": "anthropic",
            "name": "Anthropic",
            "api": {
                "type": "native",
                "settings": {"baseURL": "https://api.anthropic.com"},
            },
            "request": {"headers": {}, "body": {}},
        });
        let info: ProviderInfo = serde_json::from_value(value.clone()).unwrap();
        let roundtrip = serde_json::to_value(&info).unwrap();
        assert_eq!(roundtrip, value);
        assert!(roundtrip.get("integrationID").is_none());
        assert!(roundtrip.get("disabled").is_none());
    }

    #[test]
    fn provider_api_casing_asserted() {
        let info: ProviderInfo = serde_json::from_value(json!({
            "id": "x",
            "integrationID": "oauth",
            "name": "x",
            "disabled": true,
            "api": {"type": "native", "settings": {}},
            "request": {"headers": {}, "body": {}},
        }))
        .unwrap();
        let roundtrip = serde_json::to_value(&info).unwrap();
        assert_eq!(roundtrip["integrationID"], json!("oauth"));
        assert_eq!(roundtrip["disabled"], json!(true));
        assert!(matches!(info.api, ProviderApi::Native { settings, .. } if settings.is_empty()));
        let _: ProviderRequest = info.request;
    }
}
