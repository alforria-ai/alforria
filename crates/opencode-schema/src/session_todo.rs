//! `schema-src/session-todo.ts` — openapi `Todo`, `EventTodoUpdated`.

use serde::{Deserialize, Serialize};

use crate::ids::SessionId;

/// `SessionTodo.Info` (openapi `Todo`).
///
/// `status`/`priority` are free strings on the wire (spec S3 — do NOT tighten
/// into Rust enums): documented vocabulary is pending/in_progress/completed/
/// cancelled and high/medium/low.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoInfo {
    pub content: String,
    pub status: String,
    pub priority: String,
}

/// `"todo.updated"` event payload (openapi `EventTodoUpdated.properties`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoUpdatedData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub todos: Vec<TodoInfo>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::TodoUpdatedData;

    #[test]
    fn todo_updated_roundtrip() {
        let value = json!({
            "sessionID": "ses_01JDY",
            "todos": [
                {
                    "content": "write tests",
                    "status": "in_progress",
                    "priority": "high",
                },
            ],
        });
        let data: TodoUpdatedData = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&data).unwrap(), value);
    }

    #[test]
    fn todo_accepts_undocumented_status() {
        // Free strings (spec S3): unknown vocabulary must round-trip verbatim.
        let value = json!({
            "sessionID": "ses_01JDY",
            "todos": [
                {
                    "content": "task",
                    "status": "some-future-status",
                    "priority": "ultra",
                },
            ],
        });
        let data: TodoUpdatedData = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&data).unwrap(), value);
    }
}
