//! Wire DTOs for `schema-src/question.ts` (`QuestionV2.*` in openapi).

use serde::{Deserialize, Serialize};

use crate::ids::{QuestionId, SessionId};

/// `QuestionV2.Answer` — openapi `QuestionV2Answer`: an array of selected labels.
pub type QuestionAnswer = Vec<String>;

/// `QuestionV2.Option` — openapi `QuestionV2Option`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// `QuestionV2.Info` — openapi `QuestionV2Info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionInfo {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<bool>,
}

/// `QuestionV2.Tool` — openapi `QuestionV2Tool` (both fields required).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionTool {
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "callID")]
    pub call_id: String,
}

/// `QuestionV2.Request` — openapi `QuestionV2Request`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionRequest {
    pub id: QuestionId,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub questions: Vec<QuestionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<QuestionTool>,
}

/// `QuestionV2.Reply` — openapi `QuestionV2Reply`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionReply {
    pub answers: Vec<QuestionAnswer>,
}

/// `question.v2.asked` payload — openapi `QuestionV2Asked.data`
/// (same fields as `QuestionRequest`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionV2AskedData {
    pub id: QuestionId,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub questions: Vec<QuestionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<QuestionTool>,
}

/// `question.v2.replied` payload — openapi `QuestionV2Replied.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionV2RepliedData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "requestID")]
    pub request_id: QuestionId,
    pub answers: Vec<QuestionAnswer>,
}

/// `question.v2.rejected` payload — openapi `QuestionV2Rejected.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionV2RejectedData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "requestID")]
    pub request_id: QuestionId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn question_request_round_trip() {
        let request = QuestionRequest {
            id: "que_1".to_string(),
            session_id: "ses_1".to_string(),
            questions: vec![QuestionInfo {
                question: "Proceed?".to_string(),
                header: "Confirm".to_string(),
                options: vec![QuestionOption {
                    label: "yes".to_string(),
                    description: "Proceed with the action".to_string(),
                }],
                multiple: None,
                custom: None,
            }],
            tool: Some(QuestionTool {
                message_id: "msg_1".to_string(),
                call_id: "call_1".to_string(),
            }),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(
            json,
            json!({
                "id": "que_1",
                "sessionID": "ses_1",
                "questions": [{
                    "question": "Proceed?",
                    "header": "Confirm",
                    "options": [{
                        "label": "yes",
                        "description": "Proceed with the action"
                    }]
                }],
                "tool": {"messageID": "msg_1", "callID": "call_1"}
            })
        );
        let back: QuestionRequest =
            serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
        assert_eq!(request, back);
    }

    #[test]
    fn question_info_omits_optional_keys() {
        let info = QuestionInfo {
            question: "Which?".to_string(),
            header: "Pick".to_string(),
            options: vec![],
            multiple: None,
            custom: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(
            json,
            json!({"question": "Which?", "header": "Pick", "options": []})
        );
    }

    #[test]
    fn question_reply_wire_shape() {
        let reply = QuestionReply {
            answers: vec![vec!["yes".to_string()]],
        };
        let json = serde_json::to_value(&reply).unwrap();
        assert_eq!(json, json!({"answers": [["yes"]]}));
        let back: QuestionReply =
            serde_json::from_value(serde_json::to_value(&reply).unwrap()).unwrap();
        assert_eq!(reply, back);
    }

    #[test]
    fn question_replied_and_rejected_data_wire_shapes() {
        let replied = QuestionV2RepliedData {
            session_id: "ses_1".to_string(),
            request_id: "que_1".to_string(),
            answers: vec![vec!["yes".to_string()]],
        };
        assert_eq!(
            serde_json::to_value(&replied).unwrap(),
            json!({"sessionID": "ses_1", "requestID": "que_1", "answers": [["yes"]]})
        );
        let rejected = QuestionV2RejectedData {
            session_id: "ses_1".to_string(),
            request_id: "que_1".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&rejected).unwrap(),
            json!({"sessionID": "ses_1", "requestID": "que_1"})
        );
    }
}
