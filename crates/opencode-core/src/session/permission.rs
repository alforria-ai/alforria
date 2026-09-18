//! Permission service — the production [`crate::tool::def::Ask`]
//! implementation. Port of the ask/answer flow of `permission/index.ts`
//! (67-172).
//!
//! The pure halves of the module — `evaluate`, `fromConfig`, `merge`,
//! `disabled` — landed with M4 in [`crate::tool::permission`] and are not
//! duplicated here. This module owns the pending-request registry keyed by
//! `per_` id (spec §7 decision 6: the Effect `Deferred` of
//! permission/index.ts:18-26 becomes a `tokio::sync::oneshot`):
//!
//! * `ask` (permission/index.ts:67-107 — binding): every pattern is
//!   evaluated against `ruleset + approved`; `deny` fails with
//!   `DeniedError` carrying the ruleset filtered to the permission,
//!   `allow` continues, anything else registers a pending request,
//!   publishes `permission.asked` and awaits the answer oneshot.
//! * `reply` (109-167 — binding): unknown ids fail with
//!   `Permission.NotFoundError`. `reject` fails the deferred —
//!   `CorrectedError` when a message is present, `RejectedError`
//!   otherwise — and rejects every other pending request of the same
//!   session (129-138). `once` succeeds only. `always` appends `allow`
//!   rules for the request's `always` patterns, then auto-approves every
//!   other pending request of the same session whose patterns are all
//!   allowed now (143-166), publishing `reply: "always"` per request.
//!
//! Both events are ephemeral in TS (`define` without `durable`,
//! schema-src/v1/permission.ts:61-66) — the session durable manifest does
//! not include them.

use std::sync::{Arc, Mutex, MutexGuard};

use opencode_schema::permission_v1::{
    PermissionAskedData, PermissionRepliedData, PermissionV1Action, PermissionV1AskInput,
    PermissionV1Reply, PermissionV1ReplyInput, PermissionV1Request, PermissionV1Rule,
    PermissionV1Ruleset, PermissionV1Tool,
};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::event::bus::{EventBus, PublishOptions};
use crate::event::definition::Definition;
use crate::session::ids;
use crate::session::processor::AskPermission;
use crate::tool::def::{AskRequest, BoxFuture};
use crate::tool::error::ToolError;
use crate::tool::permission::{evaluate, wildcard_match};

/// `Permission.Event.Asked` (schema-src/v1/permission.ts:61) — ephemeral.
pub const PERMISSION_ASKED: Definition = Definition::ephemeral("permission.asked");

/// `Permission.Event.Replied` (schema-src/v1/permission.ts:62-65) —
/// ephemeral.
pub const PERMISSION_REPLIED: Definition = Definition::ephemeral("permission.replied");

/// The failure surface of the permission service: `PermissionV1.Error`
/// (`DeniedError | RejectedError | CorrectedError`,
/// core/v1/permission.ts:7-27) plus the reply-side
/// `Permission.NotFoundError` (29-31).
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionError {
    /// `PermissionV1.RejectedError` (core/v1/permission.ts:7-11).
    Rejected,
    /// `PermissionV1.CorrectedError` (core/v1/permission.ts:13-19).
    Corrected { feedback: String },
    /// `PermissionV1.DeniedError` (core/v1/permission.ts:21-27) — the
    /// ruleset filtered to rules whose permission matches.
    Denied { ruleset: PermissionV1Ruleset },
    /// `Permission.NotFoundError` (core/v1/permission.ts:29-31).
    NotFound { request_id: String },
    /// An event-bus defect (TS: a failing `events.publish` effect).
    Publish(String),
}

impl std::fmt::Display for PermissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PermissionError::Rejected => f.write_str(
                "The user rejected permission to use this specific tool call.",
            ),
            PermissionError::Corrected { feedback } => write!(
                f,
                "The user rejected permission to use this specific tool call with the following feedback: {feedback}"
            ),
            PermissionError::Denied { ruleset } => {
                let ruleset = serde_json::to_string(ruleset).unwrap_or_else(|_| "[]".to_string());
                write!(
                    f,
                    "The user has specified a rule which prevents you from using this specific tool call. Here are some of the relevant rules {ruleset}"
                )
            }
            PermissionError::NotFound { request_id } => {
                write!(f, "Permission.NotFoundError: requestID: {request_id}")
            }
            PermissionError::Publish(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PermissionError {}

/// `PendingEntry` (permission/index.ts:18-21).
struct PendingEntry {
    info: PermissionV1Request,
    done: oneshot::Sender<Result<(), PermissionError>>,
}

/// `State` (permission/index.ts:23-26): insertion-ordered pending registry
/// plus the `always`-approved rules.
#[derive(Default)]
struct State {
    pending: Vec<PendingEntry>,
    approved: PermissionV1Ruleset,
}

/// `Permission.Service` (permission/index.ts:12-16): ask/reply/list over a
/// per-instance pending registry.
pub struct PermissionService {
    events: Arc<EventBus>,
    state: Mutex<State>,
}

impl PermissionService {
    pub fn new(events: Arc<EventBus>) -> PermissionService {
        PermissionService {
            events,
            state: Mutex::new(State::default()),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn publish_asked(&self, request: &PermissionV1Request) -> Result<(), PermissionError> {
        let data = serde_json::to_value(PermissionAskedData {
            id: request.id.clone(),
            session_id: request.session_id.clone(),
            permission: request.permission.clone(),
            patterns: request.patterns.clone(),
            metadata: request.metadata.clone(),
            always: request.always.clone(),
            tool: request.tool.clone(),
        })
        .map_err(|err| PermissionError::Publish(err.to_string()))?;
        self.events
            .publish(&PERMISSION_ASKED, data, PublishOptions::default())
            .map(|_| ())
            .map_err(|err| PermissionError::Publish(err.to_string()))
    }

    fn publish_replied(
        &self,
        session_id: &str,
        request_id: &str,
        reply: PermissionV1Reply,
    ) -> Result<(), PermissionError> {
        let data = serde_json::to_value(PermissionRepliedData {
            session_id: session_id.to_string(),
            request_id: request_id.to_string(),
            reply,
        })
        .map_err(|err| PermissionError::Publish(err.to_string()))?;
        self.events
            .publish(&PERMISSION_REPLIED, data, PublishOptions::default())
            .map(|_| ())
            .map_err(|err| PermissionError::Publish(err.to_string()))
    }

    /// `list` (permission/index.ts:169-172): pending requests in insertion
    /// order.
    pub fn list(&self) -> Vec<PermissionV1Request> {
        self.lock_state()
            .pending
            .iter()
            .map(|entry| entry.info.clone())
            .collect()
    }

    /// `ask` (permission/index.ts:67-107 — binding).
    pub async fn ask(&self, input: PermissionV1AskInput) -> Result<(), PermissionError> {
        let mut needs_ask = false;
        {
            let state = self.lock_state();
            for pattern in &input.patterns {
                let rule = evaluate(
                    &input.permission,
                    pattern,
                    &[&input.ruleset, &state.approved],
                );
                match rule.action {
                    PermissionV1Action::Deny => {
                        let ruleset = input
                            .ruleset
                            .iter()
                            .filter(|rule| wildcard_match(&input.permission, &rule.permission))
                            .cloned()
                            .collect();
                        return Err(PermissionError::Denied { ruleset });
                    }
                    PermissionV1Action::Allow => continue,
                    PermissionV1Action::Ask => needs_ask = true,
                }
            }
        }
        if !needs_ask {
            return Ok(());
        }

        let request = PermissionV1Request {
            id: input
                .id
                .clone()
                .unwrap_or_else(|| ids::PermissionId::ascending(None).expect("generates per_ id")),
            session_id: input.session_id.clone(),
            permission: input.permission.clone(),
            patterns: input.patterns.clone(),
            metadata: input.metadata.clone(),
            always: input.always.clone(),
            tool: input.tool.clone(),
        };

        let (done, rx) = oneshot::channel();
        {
            let mut state = self.lock_state();
            state.pending.retain(|entry| entry.info.id != request.id);
            state.pending.push(PendingEntry {
                info: request.clone(),
                done,
            });
        }
        self.publish_asked(&request)?;
        let result = match rx.await {
            Ok(result) => result,
            // The service went away with the ask still pending — the TS
            // finalizer fails every pending deferred with RejectedError
            // (permission/index.ts:54-60).
            Err(_) => Err(PermissionError::Rejected),
        };
        let mut state = self.lock_state();
        state.pending.retain(|entry| entry.info.id != request.id);
        result
    }

    /// `reply` (permission/index.ts:109-167 — binding).
    pub fn reply(&self, input: PermissionV1ReplyInput) -> Result<(), PermissionError> {
        let mut state = self.lock_state();
        let position = state
            .pending
            .iter()
            .position(|entry| entry.info.id == input.request_id)
            .ok_or_else(|| PermissionError::NotFound {
                request_id: input.request_id.clone(),
            })?;
        let existing = state.pending.remove(position);
        self.publish_replied(&existing.info.session_id, &existing.info.id, input.reply)?;

        if input.reply == PermissionV1Reply::Reject {
            let failure = match &input.message {
                Some(feedback) => PermissionError::Corrected {
                    feedback: feedback.clone(),
                },
                None => PermissionError::Rejected,
            };
            let _ = existing.done.send(Err(failure));
            // Reject every other pending request of the same session
            // (129-138), publishing `reply: "reject"` per request.
            let session_id = existing.info.session_id.clone();
            let rejected: Vec<String> = state
                .pending
                .iter()
                .filter(|entry| entry.info.session_id == session_id)
                .map(|entry| entry.info.id.clone())
                .collect();
            for request_id in rejected {
                let position = state
                    .pending
                    .iter()
                    .position(|entry| entry.info.id == request_id)
                    .expect("position computed under the state lock");
                let entry = state.pending.remove(position);
                self.publish_replied(
                    &entry.info.session_id,
                    &entry.info.id,
                    PermissionV1Reply::Reject,
                )?;
                let _ = entry.done.send(Err(PermissionError::Rejected));
            }
            return Ok(());
        }

        let _ = existing.done.send(Ok(()));
        if input.reply == PermissionV1Reply::Once {
            return Ok(());
        }

        // `always`: append `allow` rules for the request's `always`
        // patterns (145-151), then auto-approve every other pending
        // request of the same session whose patterns are all allowed now
        // (153-166) — evaluated against `approved` only.
        for pattern in &existing.info.always {
            state.approved.push(PermissionV1Rule {
                permission: existing.info.permission.clone(),
                pattern: pattern.clone(),
                action: PermissionV1Action::Allow,
            });
        }
        let approved = state.approved.clone();
        let session_id = existing.info.session_id.clone();
        let satisfied: Vec<String> = state
            .pending
            .iter()
            .filter(|entry| entry.info.session_id == session_id)
            .filter(|entry| {
                entry.info.patterns.iter().all(|pattern| {
                    evaluate(&entry.info.permission, pattern, &[&approved]).action
                        == PermissionV1Action::Allow
                })
            })
            .map(|entry| entry.info.id.clone())
            .collect();
        for request_id in satisfied {
            let position = state
                .pending
                .iter()
                .position(|entry| entry.info.id == request_id)
                .expect("position computed under the state lock");
            let entry = state.pending.remove(position);
            self.publish_replied(
                &entry.info.session_id,
                &entry.info.id,
                PermissionV1Reply::Always,
            )?;
            let _ = entry.done.send(Ok(()));
        }
        Ok(())
    }
}

/// The M4 [`crate::tool::def::Ask`] seam bound to this service — the
/// production `ctx.ask` (tools.ts:81-89): carries the session/message/call
/// ids and the merged `agent.permission + session.permission` ruleset of
/// the current turn.
pub struct SessionAsk {
    service: Arc<PermissionService>,
    session_id: String,
    message_id: String,
    call_id: String,
    ruleset: PermissionV1Ruleset,
}

impl SessionAsk {
    pub fn new(
        service: Arc<PermissionService>,
        session_id: impl Into<String>,
        message_id: impl Into<String>,
        call_id: impl Into<String>,
        ruleset: PermissionV1Ruleset,
    ) -> SessionAsk {
        SessionAsk {
            service,
            session_id: session_id.into(),
            message_id: message_id.into(),
            call_id: call_id.into(),
            ruleset,
        }
    }
}

fn json_map(value: &Value) -> opencode_schema::schema::JsonMap {
    match value {
        Value::Object(map) => map.clone(),
        other => {
            let mut map = opencode_schema::schema::JsonMap::new();
            map.insert("value".to_string(), other.clone());
            map
        }
    }
}

fn ask_tool_error(error: PermissionError) -> ToolError {
    ToolError::Permission(error.to_string())
}

impl crate::tool::def::Ask for SessionAsk {
    fn ask<'a>(&'a self, request: AskRequest) -> BoxFuture<'a, Result<(), ToolError>> {
        Box::pin(async move {
            self.service
                .ask(PermissionV1AskInput {
                    id: None,
                    session_id: self.session_id.clone(),
                    permission: request.permission,
                    patterns: request.patterns,
                    metadata: json_map(&request.metadata),
                    always: request.always,
                    tool: Some(PermissionV1Tool {
                        message_id: self.message_id.clone(),
                        call_id: self.call_id.clone(),
                    }),
                    ruleset: self.ruleset.clone(),
                })
                .await
                .map_err(ask_tool_error)
        })
    }
}

/// The processor seam (M5.4): the permission service is the production
/// `Arc<dyn AskPermission>`. Deny/corrected surface as `Other(message)`,
/// plain rejections as `Rejected(message)` (processor.ts:200-201 matches
/// `PermissionV1.RejectedError` only — `CorrectedError` does not block).
impl AskPermission for PermissionService {
    fn ask<'a>(
        &'a self,
        request: crate::session::processor::PermissionAsk,
    ) -> BoxFuture<'a, Result<(), crate::session::processor::PermissionAskError>> {
        Box::pin(async move {
            let input = PermissionV1AskInput {
                id: None,
                session_id: request.session_id,
                permission: request.permission,
                patterns: request.patterns,
                metadata: json_map(&request.metadata),
                always: request.always,
                tool: Some(PermissionV1Tool {
                    message_id: request.tool.message_id,
                    call_id: request.tool.call_id,
                }),
                ruleset: request.ruleset,
            };
            self.ask(input).await.map_err(|error| match error {
                PermissionError::Rejected => {
                    crate::session::processor::PermissionAskError::Rejected(error.to_string())
                }
                other => crate::session::processor::PermissionAskError::Other(other.to_string()),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use opencode_schema::permission_v1::PermissionV1Action;
    use tokio::sync::broadcast;

    use super::*;
    use crate::event::definition::Payload;

    fn bus() -> Arc<EventBus> {
        Arc::new(EventBus::new(
            crate::storage::Storage::open_in_memory().unwrap(),
            None,
        ))
    }

    fn rule(permission: &str, pattern: &str, action: PermissionV1Action) -> PermissionV1Rule {
        PermissionV1Rule {
            permission: permission.to_string(),
            pattern: pattern.to_string(),
            action,
        }
    }

    fn ask_input(id: Option<&str>) -> PermissionV1AskInput {
        PermissionV1AskInput {
            id: id.map(str::to_string),
            session_id: "ses_test".to_string(),
            permission: "bash".to_string(),
            patterns: vec!["ls".to_string()],
            metadata: opencode_schema::schema::JsonMap::new(),
            always: vec![],
            ruleset: vec![],
            tool: None,
        }
    }

    fn reply_once(request_id: &str) -> PermissionV1ReplyInput {
        PermissionV1ReplyInput {
            request_id: request_id.to_string(),
            reply: PermissionV1Reply::Once,
            message: None,
        }
    }

    async fn recv(rx: &mut broadcast::Receiver<Payload>, type_: &str) -> Payload {
        loop {
            let event = rx.recv().await.unwrap();
            if event.r#type == type_ {
                return event;
            }
        }
    }

    #[tokio::test]
    async fn ask_resolves_immediately_when_action_is_allow() {
        let service = PermissionService::new(bus());
        let input = PermissionV1AskInput {
            ruleset: vec![rule("bash", "*", PermissionV1Action::Allow)],
            ..ask_input(None)
        };
        service.ask(input).await.unwrap();
        assert!(service.list().is_empty());
    }

    #[tokio::test]
    async fn ask_fails_with_denied_and_filters_the_ruleset() {
        let service = PermissionService::new(bus());
        let input = PermissionV1AskInput {
            patterns: vec!["rm -rf /".to_string()],
            ruleset: vec![
                rule("edit", "*", PermissionV1Action::Allow),
                rule("bash", "*", PermissionV1Action::Deny),
                rule("bash", "rm *", PermissionV1Action::Deny),
            ],
            ..ask_input(None)
        };
        let error = service.ask(input).await.unwrap_err();
        match &error {
            PermissionError::Denied { ruleset } => {
                // ruleset.filter(rule => Wildcard.match(permission, rule.permission))
                assert_eq!(
                    ruleset,
                    &vec![
                        rule("bash", "*", PermissionV1Action::Deny),
                        rule("bash", "rm *", PermissionV1Action::Deny),
                    ]
                );
            }
            other => panic!("expected Denied, got {other:?}"),
        }
        assert_eq!(
            error.to_string(),
            "The user has specified a rule which prevents you from using this specific tool call. Here are some of the relevant rules [{\"permission\":\"bash\",\"pattern\":\"*\",\"action\":\"deny\"},{\"permission\":\"bash\",\"pattern\":\"rm *\",\"action\":\"deny\"}]"
        );
        assert!(service.list().is_empty(), "a denied ask never registers");
    }

    #[tokio::test]
    async fn ask_checks_all_patterns_and_stops_on_first_deny() {
        let service = PermissionService::new(bus());
        let input = PermissionV1AskInput {
            patterns: vec!["echo hello".to_string(), "rm -rf /".to_string()],
            ruleset: vec![
                rule("bash", "*", PermissionV1Action::Allow),
                rule("bash", "rm *", PermissionV1Action::Deny),
            ],
            ..ask_input(None)
        };
        let error = service.ask(input).await.unwrap_err();
        assert!(matches!(error, PermissionError::Denied { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn ask_denies_even_when_an_earlier_pattern_is_ask() {
        let service = PermissionService::new(bus());
        let input = PermissionV1AskInput {
            patterns: vec!["echo hello".to_string(), "rm -rf /".to_string()],
            ruleset: vec![
                rule("bash", "echo *", PermissionV1Action::Ask),
                rule("bash", "rm *", PermissionV1Action::Deny),
            ],
            ..ask_input(None)
        };
        let error = service.ask(input).await.unwrap_err();
        assert!(matches!(error, PermissionError::Denied { .. }), "{error:?}");
        assert!(service.list().is_empty());
    }

    #[tokio::test]
    async fn ask_allows_all_patterns_when_all_match_allow_rules() {
        let service = PermissionService::new(bus());
        let input = PermissionV1AskInput {
            patterns: vec![
                "echo hello".to_string(),
                "ls -la".to_string(),
                "pwd".to_string(),
            ],
            ruleset: vec![rule("bash", "*", PermissionV1Action::Allow)],
            ..ask_input(None)
        };
        service.ask(input).await.unwrap();
        assert!(service.list().is_empty());
    }

    #[tokio::test]
    async fn ask_stays_pending_when_action_is_ask() {
        let events = bus();
        let service = Arc::new(PermissionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("permission.asked");
        let handle = {
            let service = Arc::clone(&service);
            let input = ask_input(Some("per_t1"));
            tokio::spawn(async move { service.ask(input).await })
        };

        recv(&mut asked, "permission.asked").await;
        assert_eq!(service.list().len(), 1);
        service.reply(reply_once("per_t1")).unwrap();
        handle.await.unwrap().unwrap();
        assert!(service.list().is_empty());
    }

    #[tokio::test]
    async fn ask_registers_the_request_shape() {
        let events = bus();
        let service = Arc::new(PermissionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("permission.asked");
        let input = PermissionV1AskInput {
            metadata: serde_json::json!({ "cmd": "ls" })
                .as_object()
                .unwrap()
                .clone(),
            always: vec!["ls".to_string()],
            tool: Some(PermissionV1Tool {
                message_id: "msg_test".to_string(),
                call_id: "call_test".to_string(),
            }),
            ..ask_input(Some("per_shape"))
        };
        let handle = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.ask(input).await })
        };

        let event = recv(&mut asked, "permission.asked").await;
        assert_eq!(
            event.data,
            serde_json::json!({
                "id": "per_shape",
                "sessionID": "ses_test",
                "permission": "bash",
                "patterns": ["ls"],
                "metadata": { "cmd": "ls" },
                "always": ["ls"],
                "tool": { "messageID": "msg_test", "callID": "call_test" }
            })
        );

        let pending = service.list();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "per_shape");
        service.reply(reply_once("per_shape")).unwrap();
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn reply_reject_throws_rejected_error() {
        let events = bus();
        let service = Arc::new(PermissionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("permission.asked");
        let handle = {
            let service = Arc::clone(&service);
            let input = ask_input(Some("per_rej"));
            tokio::spawn(async move { service.ask(input).await })
        };
        recv(&mut asked, "permission.asked").await;
        service
            .reply(PermissionV1ReplyInput {
                request_id: "per_rej".to_string(),
                reply: PermissionV1Reply::Reject,
                message: None,
            })
            .unwrap();
        let error = handle.await.unwrap().unwrap_err();
        assert_eq!(error, PermissionError::Rejected);
        assert_eq!(
            error.to_string(),
            "The user rejected permission to use this specific tool call."
        );
        assert!(service.list().is_empty());
    }

    #[tokio::test]
    async fn reply_reject_with_message_throws_corrected_error() {
        let events = bus();
        let service = Arc::new(PermissionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("permission.asked");
        let handle = {
            let service = Arc::clone(&service);
            let input = ask_input(Some("per_corr"));
            tokio::spawn(async move { service.ask(input).await })
        };
        recv(&mut asked, "permission.asked").await;
        service
            .reply(PermissionV1ReplyInput {
                request_id: "per_corr".to_string(),
                reply: PermissionV1Reply::Reject,
                message: Some("Use a safer command".to_string()),
            })
            .unwrap();
        let error = handle.await.unwrap().unwrap_err();
        assert_eq!(
            error,
            PermissionError::Corrected {
                feedback: "Use a safer command".to_string()
            }
        );
        assert_eq!(
            error.to_string(),
            "The user rejected permission to use this specific tool call with the following feedback: Use a safer command"
        );
    }

    #[tokio::test]
    async fn reply_always_persists_approval_across_sessions() {
        let events = bus();
        let service = Arc::new(PermissionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("permission.asked");
        let handle = {
            let service = Arc::clone(&service);
            let input = PermissionV1AskInput {
                always: vec!["ls".to_string()],
                ..ask_input(Some("per_alw"))
            };
            tokio::spawn(async move { service.ask(input).await })
        };
        recv(&mut asked, "permission.asked").await;
        service
            .reply(PermissionV1ReplyInput {
                request_id: "per_alw".to_string(),
                reply: PermissionV1Reply::Always,
                message: None,
            })
            .unwrap();
        handle.await.unwrap().unwrap();

        // Approved rules stick at the instance level: a fresh ask in a
        // *different* session resolves without any ruleset.
        let second = PermissionV1AskInput {
            session_id: "ses_other".to_string(),
            ..ask_input(None)
        };
        service.ask(second).await.unwrap();
        assert!(service.list().is_empty());
    }

    #[tokio::test]
    async fn reply_reject_cancels_all_pending_for_same_session() {
        let events = bus();
        let service = Arc::new(PermissionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("permission.asked");
        let mut replied = events.subscribe("permission.replied");

        let input_a = PermissionV1AskInput {
            session_id: "ses_same".to_string(),
            ..ask_input(Some("per_a"))
        };
        let input_b = PermissionV1AskInput {
            session_id: "ses_same".to_string(),
            permission: "edit".to_string(),
            patterns: vec!["foo.ts".to_string()],
            ..ask_input(Some("per_b"))
        };
        let a = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.ask(input_a).await })
        };
        recv(&mut asked, "permission.asked").await;
        let b = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.ask(input_b).await })
        };
        recv(&mut asked, "permission.asked").await;

        service
            .reply(PermissionV1ReplyInput {
                request_id: "per_a".to_string(),
                reply: PermissionV1Reply::Reject,
                message: None,
            })
            .unwrap();

        assert_eq!(
            a.await.unwrap().unwrap_err(),
            PermissionError::Rejected,
            "the rejected ask fails with RejectedError"
        );
        assert_eq!(
            b.await.unwrap().unwrap_err(),
            PermissionError::Rejected,
            "the same-session sibling fails with RejectedError too"
        );
        // One replied event per rejected request, both `reply: "reject"`.
        for expected in ["per_a", "per_b"] {
            let event = recv(&mut replied, "permission.replied").await;
            assert_eq!(event.data["reply"], "reject");
            assert_eq!(event.data["requestID"], expected);
        }
        assert!(service.list().is_empty());
    }

    #[tokio::test]
    async fn reply_always_resolves_matching_pending_in_same_session() {
        let events = bus();
        let service = Arc::new(PermissionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("permission.asked");
        let mut replied = events.subscribe("permission.replied");
        let input_a = PermissionV1AskInput {
            always: vec!["ls".to_string()],
            ..ask_input(Some("per_c"))
        };
        let input_b = ask_input(Some("per_d"));
        let a = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.ask(input_a).await })
        };
        recv(&mut asked, "permission.asked").await;
        let b = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.ask(input_b).await })
        };
        recv(&mut asked, "permission.asked").await;

        service
            .reply(PermissionV1ReplyInput {
                request_id: "per_c".to_string(),
                reply: PermissionV1Reply::Always,
                message: None,
            })
            .unwrap();

        a.await.unwrap().unwrap();
        // The direct reply publishes first, then the always cascade
        // auto-approves the sibling.
        let event = recv(&mut replied, "permission.replied").await;
        assert_eq!(event.data["requestID"], "per_c");
        assert_eq!(event.data["reply"], "always");
        let event = recv(&mut replied, "permission.replied").await;
        assert_eq!(event.data["requestID"], "per_d");
        assert_eq!(event.data["reply"], "always");
        b.await.unwrap().unwrap();
        assert!(service.list().is_empty());
    }

    #[tokio::test]
    async fn reply_always_keeps_other_session_pending() {
        let events = bus();
        let service = Arc::new(PermissionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("permission.asked");
        let input_a = PermissionV1AskInput {
            session_id: "ses_a".to_string(),
            always: vec!["ls".to_string()],
            ..ask_input(Some("per_e"))
        };
        let input_b = PermissionV1AskInput {
            session_id: "ses_b".to_string(),
            ..ask_input(Some("per_f"))
        };
        let a = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.ask(input_a).await })
        };
        recv(&mut asked, "permission.asked").await;
        let b = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.ask(input_b).await })
        };
        recv(&mut asked, "permission.asked").await;

        service
            .reply(PermissionV1ReplyInput {
                request_id: "per_e".to_string(),
                reply: PermissionV1Reply::Always,
                message: None,
            })
            .unwrap();
        a.await.unwrap().unwrap();
        let pending: Vec<String> = service.list().iter().map(|item| item.id.clone()).collect();
        assert_eq!(pending, vec!["per_f".to_string()]);
        service.reply(reply_once("per_f")).unwrap();
        b.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn reply_publishes_replied_event() {
        let events = bus();
        let service = Arc::new(PermissionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("permission.asked");
        let mut replied = events.subscribe("permission.replied");
        let input = ask_input(Some("per_ev"));
        let handle = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.ask(input).await })
        };
        recv(&mut asked, "permission.asked").await;
        service.reply(reply_once("per_ev")).unwrap();
        handle.await.unwrap().unwrap();

        let event = recv(&mut replied, "permission.replied").await;
        assert_eq!(
            event.data,
            serde_json::json!({
                "sessionID": "ses_test",
                "requestID": "per_ev",
                "reply": "once",
            })
        );
    }

    #[test]
    fn reply_fails_for_unknown_request_id() {
        let service = PermissionService::new(bus());
        let error = service
            .reply(PermissionV1ReplyInput {
                request_id: "per_unknown".to_string(),
                reply: PermissionV1Reply::Once,
                message: None,
            })
            .unwrap_err();
        assert_eq!(
            error,
            PermissionError::NotFound {
                request_id: "per_unknown".to_string()
            }
        );
        assert_eq!(
            error.to_string(),
            "Permission.NotFoundError: requestID: per_unknown"
        );
        assert!(service.list().is_empty());
    }

    #[test]
    fn error_messages_are_byte_exact() {
        assert_eq!(
            PermissionError::Rejected.to_string(),
            "The user rejected permission to use this specific tool call."
        );
        assert_eq!(
            PermissionError::Corrected {
                feedback: "no".to_string()
            }
            .to_string(),
            "The user rejected permission to use this specific tool call with the following feedback: no"
        );
        assert_eq!(
            PermissionError::Denied { ruleset: vec![] }.to_string(),
            "The user has specified a rule which prevents you from using this specific tool call. Here are some of the relevant rules []"
        );
    }

    #[tokio::test]
    async fn concurrent_asks_serialize_into_the_registry() {
        let events = bus();
        let service = Arc::new(PermissionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("permission.asked");

        let mut handles = Vec::new();
        for _ in 0..5 {
            let service = Arc::clone(&service);
            let input = PermissionV1AskInput {
                always: vec!["ls".to_string()],
                ..ask_input(None)
            };
            handles.push(tokio::spawn(async move { service.ask(input).await }));
        }
        for _ in 0..5 {
            recv(&mut asked, "permission.asked").await;
        }
        let pending = service.list();
        assert_eq!(pending.len(), 5);
        let ids: Vec<String> = pending.iter().map(|item| item.id.clone()).collect();
        for id in &ids {
            assert!(id.starts_with("per_"), "{id}");
        }

        // `always` cascades to every now-satisfied pending request.
        service
            .reply(PermissionV1ReplyInput {
                request_id: ids[0].clone(),
                reply: PermissionV1Reply::Always,
                message: None,
            })
            .unwrap();
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
        assert!(service.list().is_empty());
    }

    #[tokio::test]
    async fn ask_permission_impl_maps_rejected_and_other() {
        let events = bus();
        let service = Arc::new(PermissionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("permission.asked");

        // Denied → Other carrying the DeniedError message.
        let denied = crate::session::processor::AskPermission::ask(
            &*service,
            crate::session::processor::PermissionAsk {
                session_id: "ses_test".to_string(),
                permission: "bash".to_string(),
                patterns: vec!["rm".to_string()],
                always: vec![],
                metadata: serde_json::json!({}),
                ruleset: vec![rule("bash", "*", PermissionV1Action::Deny)],
                tool: crate::session::processor::PermissionAskTool {
                    message_id: "msg_1".to_string(),
                    call_id: "call_1".to_string(),
                },
            },
        )
        .await
        .unwrap_err();
        match denied {
            crate::session::processor::PermissionAskError::Other(message) => {
                assert!(
                    message.starts_with("The user has specified a rule"),
                    "{message}"
                );
            }
            other => panic!("expected Other, got {other:?}"),
        }

        // Ask-action → pending; reject → Rejected carrying the
        // RejectedError message.
        let handle = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                crate::session::processor::AskPermission::ask(
                    &*service,
                    crate::session::processor::PermissionAsk {
                        session_id: "ses_test".to_string(),
                        permission: "bash".to_string(),
                        patterns: vec!["rm".to_string()],
                        always: vec![],
                        metadata: serde_json::json!({}),
                        ruleset: vec![],
                        tool: crate::session::processor::PermissionAskTool {
                            message_id: "msg_1".to_string(),
                            call_id: "call_1".to_string(),
                        },
                    },
                )
                .await
            })
        };
        recv(&mut asked, "permission.asked").await;
        let pending = service.list();
        assert_eq!(
            pending[0].tool,
            Some(PermissionV1Tool {
                message_id: "msg_1".to_string(),
                call_id: "call_1".to_string(),
            })
        );
        let request_id = pending[0].id.clone();
        service
            .reply(PermissionV1ReplyInput {
                request_id,
                reply: PermissionV1Reply::Reject,
                message: None,
            })
            .unwrap();
        let rejected = handle.await.unwrap().unwrap_err();
        match rejected {
            crate::session::processor::PermissionAskError::Rejected(message) => {
                assert_eq!(
                    message,
                    "The user rejected permission to use this specific tool call."
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_ask_maps_failures_onto_tool_errors() {
        let events = bus();
        let service = Arc::new(PermissionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("permission.asked");
        let session_ask = SessionAsk::new(
            Arc::clone(&service),
            "ses_1",
            "msg_1",
            "cal_1",
            vec![rule("bash", "*", PermissionV1Action::Deny)],
        );
        let error = crate::tool::def::Ask::ask(
            &session_ask,
            crate::tool::def::AskRequest {
                permission: "bash".to_string(),
                patterns: vec!["ls".to_string()],
                always: vec![],
                metadata: serde_json::json!({}),
            },
        )
        .await
        .unwrap_err();
        match error {
            ToolError::Permission(message) => {
                assert!(
                    message.starts_with("The user has specified a rule"),
                    "{message}"
                );
            }
            other => panic!("expected Permission, got {other:?}"),
        }

        // Once approved, the same ask succeeds.
        let handle = {
            let session_ask =
                SessionAsk::new(Arc::clone(&service), "ses_1", "msg_1", "cal_1", vec![]);
            tokio::spawn(async move {
                crate::tool::def::Ask::ask(
                    &session_ask,
                    crate::tool::def::AskRequest {
                        permission: "bash".to_string(),
                        patterns: vec!["ls".to_string()],
                        always: vec![],
                        metadata: serde_json::json!({}),
                    },
                )
                .await
            })
        };
        recv(&mut asked, "permission.asked").await;
        let pending = service.list();
        assert_eq!(pending[0].tool.as_ref().unwrap().message_id, "msg_1");
        service.reply(reply_once(&pending[0].id)).unwrap();
        handle.await.unwrap().unwrap();
    }

    /// Spec M5.5 acceptance: M4 tools run against the real service
    /// end-to-end — bash ask → allow → execute.
    #[tokio::test]
    async fn bash_tool_runs_against_the_real_service() {
        use crate::tool::ripgrep::test_support::fixed_agents;
        use crate::tool::shell::{ShellTool, TokioSpawner};

        let temp = crate::storage::test_support::TempDir::new("permission-bash-e2e");
        let events = bus();
        let service = Arc::new(PermissionService::new(Arc::clone(&events)));
        let shell = ShellTool::new(
            "bash",
            crate::tool::shell::default_timeout_ms(),
            Arc::new(crate::tool::truncate::TruncateService::default_limits(
                temp.path().join("tool-output"),
            )),
            Arc::new(TokioSpawner),
        );
        let def = shell.def(fixed_agents()).await;
        let session_ask = SessionAsk::new(Arc::clone(&service), "ses_1", "msg_1", "cal_1", vec![]);

        // `echo hi` is not a cwd command, so the bash tool asks for
        // permission with the parsed command pattern. Reply "once" from a
        // side task once the ask lands on the bus.
        let replier = {
            let service = Arc::clone(&service);
            let mut asked = events.subscribe("permission.asked");
            tokio::spawn(async move {
                let event = recv(&mut asked, "permission.asked").await;
                let request_id = event.data["id"].as_str().unwrap().to_string();
                service
                    .reply(PermissionV1ReplyInput {
                        request_id,
                        reply: PermissionV1Reply::Once,
                        message: None,
                    })
                    .unwrap();
            })
        };

        let instance = crate::tool::def::InstanceContext {
            directory: temp.path().to_path_buf(),
            worktree: temp.path().to_path_buf(),
        };
        let extra = crate::tool::def::Extra::default();
        let metadata = crate::tool::ripgrep::test_support::RecordingAsk::new();
        let ctx = crate::tool::def::ToolCtxRef {
            session_id: "ses_1",
            message_id: "msg_1",
            agent: "build",
            call_id: Some("cal_1"),
            abort: tokio_util::sync::CancellationToken::new(),
            messages: &[],
            extra: &extra,
            instance: &instance,
            ask: &session_ask,
            metadata: &metadata,
        };
        let result = (def.execute)(serde_json::json!({ "command": "echo hi" }), ctx)
            .await
            .unwrap();
        replier.await.unwrap();
        assert_eq!(result.output.trim(), "hi");
    }
}
