//! `schema-src/llm.ts`.

use serde::{Deserialize, Serialize};

use crate::schema::JsonMap;

/// `LLM.ProviderMetadata`: `Record<String, Record<String, Unknown>>`.
pub type ProviderMetadata = std::collections::BTreeMap<String, JsonMap>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    Text {
        text: String,
    },
    File {
        uri: String,
        mime: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::ToolContent;

    #[test]
    fn tool_content_file_roundtrip_omits_optional_keys() {
        let value = json!({
            "type": "file",
            "uri": "file:///tmp/a.txt",
            "mime": "text/plain",
        });
        let content: ToolContent = serde_json::from_value(value.clone()).unwrap();
        let roundtrip = serde_json::to_value(&content).unwrap();
        assert_eq!(roundtrip, value);
        assert!(roundtrip.get("name").is_none());
    }
}
