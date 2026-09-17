//! Wire DTOs for `schema-src/legacy-event.ts` (`v1/legacy-event.ts`) —
//! openapi `CommandExecuted`.

use serde::{Deserialize, Serialize};

use crate::ids::{MessageId, SessionId};

/// `command.executed` payload — openapi `CommandExecuted.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutedData {
    pub name: String,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub arguments: String,
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::CommandExecutedData;

    #[test]
    fn command_executed_wire_shape() {
        let data = CommandExecutedData {
            name: "build".to_string(),
            session_id: "ses_1".to_string(),
            arguments: "{}".to_string(),
            message_id: "msg_1".to_string(),
        };
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(
            json,
            json!({
                "name": "build",
                "sessionID": "ses_1",
                "arguments": "{}",
                "messageID": "msg_1",
            })
        );
        let back: CommandExecutedData =
            serde_json::from_value(serde_json::to_value(&data).unwrap()).unwrap();
        assert_eq!(data, back);
    }
}
