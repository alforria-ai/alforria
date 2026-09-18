//! Wire DTOs for the MCP `Status` union — openapi `MCPStatus*` components
//! (`mcp/index.ts:83-107`).

use serde::Serialize;

/// openapi `MCPStatus` union, discriminated on `status`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum McpStatus {
    Connected,
    Disabled,
    Failed { error: String },
    NeedsAuth,
    NeedsClientRegistration { error: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_wire_shapes() {
        assert_eq!(
            serde_json::to_string(&McpStatus::Connected).unwrap(),
            r#"{"status":"connected"}"#
        );
        assert_eq!(
            serde_json::to_string(&McpStatus::Disabled).unwrap(),
            r#"{"status":"disabled"}"#
        );
        assert_eq!(
            serde_json::to_string(&McpStatus::Failed {
                error: "boom".into()
            })
            .unwrap(),
            r#"{"status":"failed","error":"boom"}"#
        );
        assert_eq!(
            serde_json::to_string(&McpStatus::NeedsAuth).unwrap(),
            r#"{"status":"needs_auth"}"#
        );
        assert_eq!(
            serde_json::to_string(&McpStatus::NeedsClientRegistration {
                error: "no client_id".into()
            })
            .unwrap(),
            r#"{"status":"needs_client_registration","error":"no client_id"}"#
        );
    }
}
