//! Wire DTOs for `schema-src/server-event.ts` — openapi `ServerConnected`,
//! `GlobalDisposed`, `ServerInstanceDisposed`.

use serde::{Deserialize, Serialize};

/// `server.connected` payload — openapi `ServerConnected.data` (empty object).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerConnectedData {}

/// `global.disposed` payload — openapi `GlobalDisposed.data` (empty object).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalDisposedData {}

/// `server.instance.disposed` payload — openapi
/// `EventServerInstanceDisposed.properties` (server-injected event; present in
/// the legacy `Event` union only, never in `V2Event`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerInstanceDisposedData {
    pub directory: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn server_instance_disposed_wire_shape() {
        let data = ServerInstanceDisposedData {
            directory: "/home/jon/repo".to_string(),
        };
        let value = serde_json::to_value(&data).unwrap();
        assert_eq!(value, json!({ "directory": "/home/jon/repo" }));
        let back: ServerInstanceDisposedData = serde_json::from_value(value).unwrap();
        assert_eq!(back, data);
    }
}
