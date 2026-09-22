//! `session-delivery.ts`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDelivery {
    Steer,
    Queue,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn delivery_round_trips() {
        assert_eq!(
            serde_json::to_value(SessionDelivery::Steer).unwrap(),
            json!("steer")
        );
        assert_eq!(
            serde_json::to_value(SessionDelivery::Queue).unwrap(),
            json!("queue")
        );
        assert_eq!(
            serde_json::from_value::<SessionDelivery>(json!("queue")).unwrap(),
            SessionDelivery::Queue
        );
    }
}
