//! `schema-src/session-input.ts` — openapi `SessionInputAdmitted`.

use serde::{Deserialize, Serialize};

use crate::ids::{MessageId, SessionId};
use crate::prompt::Prompt;
use crate::schema::EpochMillis;
use crate::session_delivery::SessionDelivery;

/// `SessionInput.Admitted` (openapi `SessionInputAdmitted`; required: all
/// but `promotedSeq`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInputAdmitted {
    pub admitted_seq: u64, // admittedSeq, NonNegativeInt
    pub id: MessageId,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub prompt: Prompt,
    pub delivery: SessionDelivery,
    pub time_created: EpochMillis, // timeCreated
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_seq: Option<u64>, // promotedSeq, NonNegativeInt
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::SessionInputAdmitted;

    #[test]
    fn admitted_roundtrip_omits_promoted_seq() {
        let value = json!({
            "admittedSeq": 1,
            "id": "msg_01JDY",
            "sessionID": "ses_01JDY",
            "prompt": { "text": "hello" },
            "delivery": "queue",
            "timeCreated": 1778031210000i64,
        });
        let admitted: SessionInputAdmitted = serde_json::from_value(value.clone()).unwrap();
        let roundtrip = serde_json::to_value(&admitted).unwrap();
        assert_eq!(roundtrip, value);
        assert!(roundtrip.get("promotedSeq").is_none());
    }

    #[test]
    fn admitted_roundtrip_with_promoted_seq() {
        let value = json!({
            "admittedSeq": 2,
            "id": "msg_01JDY",
            "sessionID": "ses_01JDY",
            "prompt": { "text": "hello" },
            "delivery": "steer",
            "timeCreated": 1778031210000i64,
            "promotedSeq": 1,
        });
        let admitted: SessionInputAdmitted = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&admitted).unwrap(), value);
    }
}
