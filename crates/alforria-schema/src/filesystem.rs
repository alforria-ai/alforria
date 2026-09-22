//! Wire DTOs for `schema-src/filesystem.ts` — openapi `FileEdited`,
//! `FileSystemEntry`.
//!
//! `FileSystem.Submatch`/`Match`/`FindInput` have no openapi components
//! (spec STOP S5): shapes verified against `schema-src/filesystem.ts` only.

use serde::{Deserialize, Serialize};

/// `file.edited` payload — openapi `FileEdited.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEditedData {
    pub file: String,
}

/// `FileSystem.Entry` — openapi `FileSystemEntry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemEntry {
    /// RelativePath.
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: FileSystemEntryType,
}

/// Entry kind — openapi `FileSystemEntry.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSystemEntryType {
    File,
    Directory,
}

/// `FileSystem.Submatch` — schema-src only (NOT verified against openapi).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemSubmatch {
    pub text: String,
    pub start: u64,
    pub end: u64,
}

/// `FileSystem.Match` — schema-src only (NOT verified against openapi).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemMatch {
    pub entry: FileSystemEntry,
    pub line: u64,
    pub offset: u64,
    pub text: String,
    pub submatches: Vec<FileSystemSubmatch>,
}

/// `FileSystem.FindInput` — schema-src only (NOT verified against openapi).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemFindInput {
    pub query: String,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub entry_type: Option<FileSystemEntryType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{FileEditedData, FileSystemFindInput};

    #[test]
    fn file_edited_wire_shape() {
        let data = FileEditedData {
            file: "src/main.rs".to_string(),
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json, json!({"file": "src/main.rs"}));
        let back: FileEditedData =
            serde_json::from_value(serde_json::to_value(&data).unwrap()).unwrap();
        assert_eq!(data, back);
    }

    /// Self-authored vector — unverified against openapi (see STOP S5).
    #[test]
    fn find_input_omits_optional_keys() {
        let value = json!({"query": "main"});
        let input: FileSystemFindInput = serde_json::from_value(value.clone()).unwrap();
        let json = serde_json::to_value(&input).unwrap();
        assert_eq!(json, value);
        assert!(json.get("type").is_none());
        assert!(json.get("limit").is_none());
    }
}
