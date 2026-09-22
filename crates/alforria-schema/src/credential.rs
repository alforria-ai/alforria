//! `schema-src/credential.ts`.

use serde::{Deserialize, Serialize};

use crate::ids::IntegrationMethodId;
use crate::schema::JsonMap;

/// `Credential.Value` (openapi `CredentialValue`): tagged union over
/// `Credential.OAuth` and `Credential.Key`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum CredentialValue {
    /// `Credential.OAuth` (openapi `CredentialOAuth`).
    Oauth {
        #[serde(rename = "methodID")]
        method_id: IntegrationMethodId,
        refresh: String,
        access: String,
        expires: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
    },
    /// `Credential.Key` (openapi `CredentialKey`).
    Key {
        key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
    },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::CredentialValue;

    #[test]
    fn oauth_roundtrips_method_id_casing() {
        let value = json!({
            "type": "oauth",
            "methodID": "github",
            "refresh": "rtoken",
            "access": "atoken",
            "expires": 123,
        });
        let credential: CredentialValue = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(credential, CredentialValue::Oauth { .. }));
        let roundtrip = serde_json::to_value(&credential).unwrap();
        assert_eq!(roundtrip, value);
        assert!(roundtrip.get("methodID").is_some());
        assert!(roundtrip.get("methodId").is_none());
        assert!(roundtrip.get("metadata").is_none());
    }

    #[test]
    fn key_roundtrips_with_metadata() {
        let value = json!({
            "type": "key",
            "key": "sk-…",
            "metadata": {"hint": "value"},
        });
        let credential: CredentialValue = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(credential, CredentialValue::Key { .. }));
        assert_eq!(serde_json::to_value(&credential).unwrap(), value);
    }
}
