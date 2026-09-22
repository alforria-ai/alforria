//! Session status service — port of `session/status.ts`: an in-memory
//! `Map<SessionID, Info>` behind `get`/`list`/`set`.
//!
//! `set` publishes the ephemeral `session.status` event on every call and
//! the deprecated `session.idle` event when the status is `idle`
//! (status.ts:33-43). The wire shapes live in
//! `alforria_schema::session_status` — they are not duplicated here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use alforria_schema::session_status::{SessionIdleData, SessionStatusData, SessionStatusInfo};

use crate::event::bus::{EventBus, PublishOptions};
use crate::event::definition::Definition;

/// `SessionStatusEvent.Event.Status` (`session.status`) — ephemeral.
pub const STATUS: Definition = Definition::ephemeral("session.status");

/// `SessionStatusEvent.Event.Idle` (`session.idle`, deprecated) — ephemeral.
pub const IDLE: Definition = Definition::ephemeral("session.idle");

/// The `SessionStatus.Service` state (status.ts:12-49).
pub struct SessionStatusService {
    state: Mutex<HashMap<String, SessionStatusInfo>>,
    events: Arc<EventBus>,
}

impl SessionStatusService {
    pub fn new(events: Arc<EventBus>) -> SessionStatusService {
        SessionStatusService {
            state: Mutex::new(HashMap::new()),
            events,
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, HashMap<String, SessionStatusInfo>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `get` (status.ts:35-38): `{ type: "idle" }` when unknown.
    pub fn get(&self, session_id: &str) -> SessionStatusInfo {
        self.lock_state()
            .get(session_id)
            .cloned()
            .unwrap_or(SessionStatusInfo::Idle)
    }

    /// `list` (status.ts:40-42).
    pub fn list(&self) -> HashMap<String, SessionStatusInfo> {
        self.lock_state().clone()
    }

    /// `set` (status.ts:44-49): publish `session.status`, then delete the
    /// entry (and publish `session.idle`) when idle, else store it.
    pub fn set(&self, session_id: &str, status: SessionStatusInfo) -> Result<(), crate::CoreError> {
        self.events.publish(
            &STATUS,
            serde_json::to_value(SessionStatusData {
                session_id: session_id.to_string(),
                status: status.clone(),
            })
            .map_err(|err| crate::CoreError::Storage(err.to_string()))?,
            PublishOptions::default(),
        )?;
        if matches!(status, SessionStatusInfo::Idle) {
            self.lock_state().remove(session_id);
            self.events.publish(
                &IDLE,
                serde_json::to_value(SessionIdleData {
                    session_id: session_id.to_string(),
                })
                .map_err(|err| crate::CoreError::Storage(err.to_string()))?,
                PublishOptions::default(),
            )?;
            return Ok(());
        }
        self.lock_state().insert(session_id.to_string(), status);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alforria_schema::session_status::SessionStatusInfo;
    use std::sync::Arc;

    fn service() -> Arc<SessionStatusService> {
        Arc::new(SessionStatusService::new(test_bus()))
    }

    fn test_bus() -> Arc<EventBus> {
        let storage = crate::storage::Storage::open_in_memory().unwrap();
        Arc::new(EventBus::new(storage, None))
    }

    #[test]
    fn unknown_session_is_idle() {
        let status = service();
        assert_eq!(status.get("ses_01"), SessionStatusInfo::Idle);
        assert!(status.list().is_empty());
    }

    #[test]
    fn set_publishes_status_and_remembers_busy() {
        let status = service();
        status
            .set(
                "ses_01",
                SessionStatusInfo::Retry {
                    attempt: 1,
                    message: "rate limited".to_string(),
                    action: None,
                    next: 1,
                },
            )
            .unwrap();
        assert_eq!(
            status.get("ses_01"),
            SessionStatusInfo::Retry {
                attempt: 1,
                message: "rate limited".to_string(),
                action: None,
                next: 1,
            }
        );
        status.set("ses_01", SessionStatusInfo::Busy).unwrap();
        assert_eq!(status.get("ses_01"), SessionStatusInfo::Busy);
    }

    #[test]
    fn set_idle_publishes_idle_and_forgets() {
        let status = service();
        status.set("ses_01", SessionStatusInfo::Busy).unwrap();
        status.set("ses_01", SessionStatusInfo::Idle).unwrap();
        assert_eq!(status.get("ses_01"), SessionStatusInfo::Idle);
        assert!(status.list().is_empty());
    }

    #[test]
    fn set_publishes_on_the_bus() {
        let status = service();
        let mut rx = status.events.subscribe("session.status");
        let mut idle_rx = status.events.subscribe("session.idle");
        status.set("ses_01", SessionStatusInfo::Busy).unwrap();
        status.set("ses_01", SessionStatusInfo::Idle).unwrap();
        let payload = rx.try_recv().expect("session.status published");
        assert_eq!(payload.data["sessionID"], "ses_01");
        assert_eq!(payload.data["status"]["type"], "busy");
        let payload = idle_rx.try_recv().expect("session.idle published");
        assert_eq!(payload.data["sessionID"], "ses_01");
    }
}
