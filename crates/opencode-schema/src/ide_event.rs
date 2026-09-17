//! Wire DTOs for `schema-src/ide-event.ts`.
//!
//! `ide.installed` has no openapi component (spec STOP S5): shape verified
//! against `schema-src/ide-event.ts` only.

use serde::{Deserialize, Serialize};

/// `ide.installed` payload — schema-src `IdeEvent.Installed`
/// (`{ ide: Schema.String }`). NOT verified against openapi (absent there).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdeInstalledData {
    pub ide: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::IdeInstalledData;

    /// Self-authored vector — unverified against openapi (see STOP S5).
    #[test]
    fn ide_installed_data_roundtrip() {
        let data = IdeInstalledData {
            ide: "vscode".to_string(),
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json, json!({"ide": "vscode"}));
        let back: IdeInstalledData = serde_json::from_value(json).unwrap();
        assert_eq!(data, back);
    }
}
