//! `schema-src/connection.ts`.

use serde::{Deserialize, Serialize};

use crate::ids::CredentialId;

/// `Connection.Info` (openapi `ConnectionInfo`): tagged union over
/// `Connection.CredentialInfo` and `Connection.EnvInfo`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(rename_all_fields = "camelCase")]
pub enum ConnectionInfo {
    /// `Connection.CredentialInfo` (openapi `ConnectionCredentialInfo`).
    Credential { id: CredentialId, label: String },
    /// `Connection.EnvInfo` (openapi `ConnectionEnvInfo`).
    Env { name: String },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::ConnectionInfo;

    #[test]
    fn credential_connection_roundtrips() {
        let value = json!({
            "type": "credential",
            "id": "cred_123",
            "label": "my-key",
        });
        let info: ConnectionInfo = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(info, ConnectionInfo::Credential { .. }));
        assert_eq!(serde_json::to_value(&info).unwrap(), value);
    }

    #[test]
    fn env_connection_roundtrips() {
        let value = json!({
            "type": "env",
            "name": "ANTHROPIC_API_KEY",
        });
        let info: ConnectionInfo = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(info, ConnectionInfo::Env { .. }));
        assert_eq!(serde_json::to_value(&info).unwrap(), value);
    }
}
