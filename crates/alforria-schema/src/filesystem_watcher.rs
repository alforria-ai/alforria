//! Wire DTOs for `schema-src/filesystem-watcher.ts` — openapi
//! `FileWatcherUpdated`.

use serde::{Deserialize, Serialize};

/// `file.watcher.updated` payload — openapi `FileWatcherUpdated.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileWatcherUpdatedData {
    pub file: String,
    pub event: FileWatcherEvent,
}

/// Watcher event kind — openapi `FileWatcherUpdated.data.event`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileWatcherEvent {
    Add,
    Change,
    Unlink,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{FileWatcherEvent, FileWatcherUpdatedData};

    #[test]
    fn file_watcher_updated_wire_shape() {
        let data = FileWatcherUpdatedData {
            file: "src/main.rs".to_string(),
            event: FileWatcherEvent::Change,
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json, json!({"file": "src/main.rs", "event": "change"}));
        let back: FileWatcherUpdatedData =
            serde_json::from_value(serde_json::to_value(&data).unwrap()).unwrap();
        assert_eq!(data, back);
    }
}
