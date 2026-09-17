//! Wire DTOs for `schema-src/v1/question.ts` (openapi `Question*`).

use serde::{Deserialize, Serialize};

use crate::ids::{QuestionId, SessionId};

/// openapi `QuestionAnswer`: an array of selected labels.
pub type QuestionV1Answer = Vec<String>;

/// openapi `QuestionOption`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionV1Option {
    pub label: String,
    pub description: String,
}

/// openapi `QuestionInfo`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionV1Info {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionV1Option>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<bool>,
}

/// `QuestionV1.Prompt` — NOT in openapi (schema-src only, spec STOP S5):
/// like `QuestionV1Info` but without the `custom` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionV1Prompt {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionV1Option>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
}

/// openapi `QuestionTool` (both fields required).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionV1Tool {
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "callID")]
    pub call_id: String,
}

/// openapi `QuestionRequest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionV1Request {
    pub id: QuestionId,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub questions: Vec<QuestionV1Info>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<QuestionV1Tool>,
}

/// `QuestionV1.Reply` — NOT in openapi (schema-src only, spec STOP S5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionV1Reply {
    pub answers: Vec<QuestionV1Answer>,
}

/// `question.asked` payload — openapi `EventQuestionAsked.properties`
/// (same fields as `QuestionV1Request`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionAskedData {
    pub id: QuestionId,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub questions: Vec<QuestionV1Info>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<QuestionV1Tool>,
}

/// `question.replied` payload — openapi `EventQuestionReplied.properties`
/// (openapi `QuestionReplied`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionRepliedData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "requestID")]
    pub request_id: QuestionId,
    pub answers: Vec<QuestionV1Answer>,
}

/// `question.rejected` payload — openapi `EventQuestionRejected.properties`
/// (openapi `QuestionRejected`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionRejectedData {
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
        let request = QuestionV1Request {
            id: "que_01J".to_string(),
            session_id: "ses_01JDY".to_string(),
            questions: vec![QuestionV1Info {
                question: "Proceed?".to_string(),
                header: "Confirm".to_string(),
                options: vec![QuestionV1Option {
                    label: "yes".to_string(),
                    description: "Proceed with the action".to_string(),
                }],
                multiple: Some(true),
                custom: None,
            }],
            tool: Some(QuestionV1Tool {
                message_id: "msg_01JDY".to_string(),
                call_id: "call_1".to_string(),
            }),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(
            json,
            json!({
                "id": "que_01J",
                "sessionID": "ses_01JDY",
                "questions": [{
                    "question": "Proceed?",
                    "header": "Confirm",
                    "options": [{
                        "label": "yes",
                        "description": "Proceed with the action"
                    }],
                    "multiple": true
                }],
                "tool": {"messageID": "msg_01JDY", "callID": "call_1"}
            })
        );
        let back: QuestionV1Request = serde_json::from_value(json).unwrap();
        assert_eq!(request, back);
    }

    #[test]
    fn question_info_omits_optional_keys() {
        let info = QuestionV1Info {
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
    fn question_prompt_has_no_custom_field() {
        // NOT in openapi (schema-src only, spec STOP S5) — self-authored vector.
        let prompt = QuestionV1Prompt {
            question: "Which?".to_string(),
            header: "Pick".to_string(),
            options: vec![],
            multiple: None,
        };
        let json = serde_json::to_value(&prompt).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.get("custom").is_none(), "custom must not exist");
        assert!(obj.get("multiple").is_none(), "multiple must be absent");
    }

    #[test]
    fn question_reply_wire_shape() {
        // NOT in openapi (schema-src only, spec STOP S5) — self-authored vector.
        let reply = QuestionV1Reply {
            answers: vec![vec!["yes".to_string()]],
        };
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            json!({"answers": [["yes"]]})
        );
    }

    #[test]
    fn question_event_data_wire_shapes() {
        // openapi `EventQuestionAsked.properties` / `EventQuestionReplied.properties` /
        // `EventQuestionRejected.properties`.
        let asked = QuestionAskedData {
            id: "que_01J".to_string(),
            session_id: "ses_01JDY".to_string(),
            questions: vec![],
            tool: None,
        };
        let json = serde_json::to_value(&asked).unwrap();
        assert_eq!(
            json,
            json!({
                "id": "que_01J",
                "sessionID": "ses_01JDY",
                "questions": []
            })
        );
        let back: QuestionAskedData = serde_json::from_value(json).unwrap();
        assert_eq!(asked, back);

        let replied = QuestionRepliedData {
            session_id: "ses_01JDY".to_string(),
            request_id: "que_01J".to_string(),
            answers: vec![vec!["yes".to_string()]],
        };
        assert_eq!(
            serde_json::to_value(&replied).unwrap(),
            json!({"sessionID": "ses_01JDY", "requestID": "que_01J", "answers": [["yes"]]})
        );
        let back: QuestionRepliedData =
            serde_json::from_value(serde_json::to_value(&replied).unwrap()).unwrap();
        assert_eq!(replied, back);

        let rejected = QuestionRejectedData {
            session_id: "ses_01JDY".to_string(),
            request_id: "que_01J".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&rejected).unwrap(),
            json!({"sessionID": "ses_01JDY", "requestID": "que_01J"})
        );
    }
}
