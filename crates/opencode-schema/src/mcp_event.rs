//! Wire DTOs for `schema-src/mcp-event.ts` — openapi `McpToolsChanged`,
//! `McpBrowserOpenFailed`.

use serde::{Deserialize, Serialize};

/// `mcp.tools.changed` payload — openapi `McpToolsChanged.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolsChangedData {
    pub server: String,
}

/// `mcp.browser.open.failed` payload — openapi `McpBrowserOpenFailed.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpBrowserOpenFailedData {
    /// Wire name is `mcpName` (plain camelCase).
    pub mcp_name: String,
    pub url: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::McpBrowserOpenFailedData;

    #[test]
    fn mcp_browser_open_failed_wire_shape() {
        let data = McpBrowserOpenFailedData {
            mcp_name: "docs".to_string(),
            url: "https://example.com".to_string(),
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(
            json,
            json!({"mcpName": "docs", "url": "https://example.com"})
        );
        let back: McpBrowserOpenFailedData =
            serde_json::from_value(serde_json::to_value(&data).unwrap()).unwrap();
        assert_eq!(data, back);
    }
}
