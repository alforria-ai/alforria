//! Wire DTOs for `schema-src/pty-ticket.ts` — openapi `PtyTicketConnectToken`.

use serde::{Deserialize, Serialize};

/// `PtyTicket.ConnectToken` — openapi `PtyTicketConnectToken`.
///
/// `expires_in` is one of the two snake_case exceptions on the wire
/// (`#[serde(rename = "expires_in")]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PtyTicketConnectToken {
    pub ticket: String,
    #[serde(rename = "expires_in")]
    pub expires_in: u64,
}

#[cfg(test)]
mod tests {
    use crate::pty::{PtyInfo, PtyStatus};
    use serde_json::json;

    use super::PtyTicketConnectToken;

    /// Acceptance: `PtyTicketConnectToken` wire has snake_case `expires_in`
    /// and `PtyInfo` has camelCase `exitCode` in the same test file.
    #[test]
    fn pty_ticket_expires_in_and_pty_info_exit_code() {
        // openapi `PtyTicketConnectToken` vector.
        let value = json!({ "ticket": "abc", "expires_in": 300 });
        let token: PtyTicketConnectToken = serde_json::from_value(value.clone()).unwrap();
        let json = serde_json::to_value(&token).unwrap();
        assert_eq!(json, value);
        assert!(json.get("expires_in").is_some());
        assert!(json.get("expiresIn").is_none());

        // openapi `Pty` vector (required properties only).
        let value = json!({
            "id": "pty_1",
            "title": "shell",
            "command": "bash",
            "args": [],
            "cwd": "/repo",
            "status": "running",
            "pid": 42,
            "exitCode": 0,
        });
        let info: PtyInfo = serde_json::from_value(value.clone()).unwrap();
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json, value);
        assert!(json.get("exitCode").is_some());
        assert!(json.get("exit_code").is_none());
        assert_eq!(info.status, PtyStatus::Running);
    }
}
