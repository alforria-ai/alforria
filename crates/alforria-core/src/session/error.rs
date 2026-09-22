//! Session error types — port of `session.ts` `BusyError`, the storage
//! `NotFoundError`, and `session/message-error.ts`.

use crate::CoreError;
use alforria_schema::session_v1::AssistantError;

/// `Session.BusyError` (session.ts:407-409) — `_tag: "SessionBusyError"`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("SessionBusyError: sessionID: {session_id}")]
pub struct BusyError {
    pub session_id: String,
}

/// The storage `NotFoundError` (packages/opencode/src/storage/storage.ts) —
/// `message` only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("NotFoundError: {message}")]
pub struct NotFoundError {
    pub message: String,
}

/// `OutputLengthError` — `NamedError.create("MessageOutputLengthError", {})`
/// (message-error.ts:4).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("MessageOutputLengthError")]
pub struct OutputLengthError;

/// `AuthError` — `NamedError.create("ProviderAuthError", { providerID,
/// message })` (message-error.ts:6-9).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("ProviderAuthError: {provider_id}: {message}")]
pub struct AuthError {
    pub provider_id: String,
    pub message: String,
}

impl OutputLengthError {
    /// The wire shape this error serializes to (openapi
    /// `MessageOutputLengthError`).
    pub fn to_assistant_error(&self) -> AssistantError {
        AssistantError::OutputLength {}
    }
}

impl AuthError {
    /// The wire shape this error serializes to (openapi `ProviderAuthError`).
    pub fn to_assistant_error(&self) -> AssistantError {
        AssistantError::Auth {
            provider_id: self.provider_id.clone(),
            message: self.message.clone(),
        }
    }
}

/// The error surface of the session engine (M5.1): storage defects plus the
/// typed session errors.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error(transparent)]
    NotFound(#[from] NotFoundError),
    #[error(transparent)]
    Busy(#[from] BusyError),
}

impl SessionError {
    pub fn not_found(message: impl Into<String>) -> SessionError {
        SessionError::NotFound(NotFoundError {
            message: message.into(),
        })
    }
}

impl From<rusqlite::Error> for SessionError {
    fn from(err: rusqlite::Error) -> Self {
        CoreError::from(err).into()
    }
}

impl From<serde_json::Error> for SessionError {
    fn from(err: serde_json::Error) -> Self {
        CoreError::Storage(err.to_string()).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_error_display() {
        let err = BusyError {
            session_id: "ses_01J".to_string(),
        };
        assert_eq!(err.to_string(), "SessionBusyError: sessionID: ses_01J");
    }

    #[test]
    fn message_errors_map_to_the_wire_shapes() {
        let output_length: AssistantError = OutputLengthError.to_assistant_error();
        assert_eq!(
            serde_json::to_value(&output_length).unwrap(),
            serde_json::json!({"name": "MessageOutputLengthError", "data": {}})
        );
        let auth = AuthError {
            provider_id: "anthropic".to_string(),
            message: "no key".to_string(),
        }
        .to_assistant_error();
        assert_eq!(
            serde_json::to_value(&auth).unwrap(),
            serde_json::json!({
                "name": "ProviderAuthError",
                "data": {"providerID": "anthropic", "message": "no key"}
            })
        );
    }
}
