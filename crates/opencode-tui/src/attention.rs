//! `attention.ts` + `feature-plugins/system/notifications.ts` — the
//! terminal bell/notify seam (M8.8). Terminal BEL + the OSC 9 desktop
//! notification escape sequence; sound-pack playback is a non-goal
//! (spec §6 N5 — the bell stands in for the audio pack).

use std::io::Write;
use std::sync::Arc;

use opencode_schema::event_manifest::Event;

use crate::state::{App, Effect};

/// `notify(request)` (`attention.ts` — `TuiAttentionNotifyInput`): a
/// title plus message, a notification request and a bell request. The
/// TS focus states (`focused`/`blurred`) have no crossterm equivalent —
/// the port stays in the `"unknown"` state, where `focusSkip` never
/// skips (documented divergence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyRequest {
    pub title: Option<String>,
    pub message: String,
    /// `notification: false` for subagent sessions
    /// (`notifications.ts:14-22`).
    pub notification: bool,
    pub bell: bool,
}

/// The `TuiAttentionNotifyResult` bits the port can observe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NotifyOutcome {
    pub notification: bool,
    pub bell: bool,
}

/// The seam (spec §2.3): production writes terminal sequences; tests
/// record.
pub trait Attention: Send + Sync {
    /// OSC 9 desktop notification (`triggerNotification`).
    fn trigger_notification(&self, message: &str, title: &str) -> bool;
    /// The terminal bell (BEL, `\x07`).
    fn bell(&self) -> bool;
}

const DEFAULT_TITLE: &str = "opencode";

/// Production [`Attention`] — OSC 9 (`ESC ] 9 ; message BEL`) and BEL.
#[derive(Debug, Default)]
pub struct TerminalAttention;

fn write_sequence(sequence: &str) -> bool {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(sequence.as_bytes()).is_ok() && stdout.flush().is_ok()
}

impl Attention for TerminalAttention {
    fn trigger_notification(&self, message: &str, title: &str) -> bool {
        if title == DEFAULT_TITLE {
            write_sequence(&format!("\x1b]9;{message}\x07"))
        } else {
            write_sequence(&format!("\x1b]9;{title}: {message}\x07"))
        }
    }

    fn bell(&self) -> bool {
        write_sequence("\x07")
    }
}

/// The `notify()` gate chain (`attention.ts:184-235`): `enabled`, then
/// `notifications`/`sound` config, per request.
pub fn notify(
    request: &NotifyRequest,
    config: &crate::config::AttentionConfig,
    attention: &dyn Attention,
) -> NotifyOutcome {
    if !config.enabled {
        return NotifyOutcome::default();
    }
    let mut outcome = NotifyOutcome::default();
    if request.notification && config.notifications {
        let title = request.title.as_deref().unwrap_or(DEFAULT_TITLE);
        outcome.notification = attention.trigger_notification(&request.message, title);
    }
    if request.bell && config.sound {
        outcome.bell = attention.bell();
    }
    outcome
}

// ------------------------------------------------- the notifications plugin

/// The `sessionErrorMessage` map (`notifications.ts:28-36`).
fn session_error_message(error: &opencode_schema::session_v1::AssistantError) -> &'static str {
    use opencode_schema::session_v1::AssistantError;
    match error {
        AssistantError::Aborted { .. } => "Session aborted",
        _ => "Session error",
    }
}

/// `notifications.ts:14-22` — one attention effect per surface.
fn ask_effect(app: &App, session_id: Option<&str>, message: &str) -> Effect {
    let session = session_id.and_then(|id| app.state.sync.session(id));
    let is_subagent = session
        .map(|session| session.parent_id.is_some())
        .unwrap_or(false);
    Effect::Attention {
        title: session.map(|session| session.title.clone()),
        message: message.to_string(),
        notification: !is_subagent,
        bell: true,
    }
}

/// The built-in `internal:notifications` plugin body
/// (`notifications.ts:38-88`) — question/permission asks, the
/// busy→idle "Session done" and the errored-session path. Returns the
/// attention effects for the runtime to execute.
pub fn on_bus_event(app: &mut App, event: &Event) -> Vec<Effect> {
    let sets = &mut app.ui.attention;
    match event {
        Event::QuestionAsked(evt) => {
            if sets.questions.contains(&evt.id) {
                return Vec::new();
            }
            sets.questions.insert(evt.id.clone());
            vec![ask_effect(
                app,
                Some(&evt.session_id),
                "Question needs input",
            )]
        }
        Event::QuestionReplied(evt) => {
            app.ui.attention.questions.remove(&evt.request_id);
            Vec::new()
        }
        Event::QuestionRejected(evt) => {
            app.ui.attention.questions.remove(&evt.request_id);
            Vec::new()
        }
        Event::PermissionAsked(evt) => {
            if sets.permissions.contains(&evt.id) {
                return Vec::new();
            }
            sets.permissions.insert(evt.id.clone());
            vec![ask_effect(
                app,
                Some(&evt.session_id),
                "Permission needs input",
            )]
        }
        Event::PermissionReplied(evt) => {
            sets.permissions.remove(&evt.request_id);
            Vec::new()
        }
        Event::SessionStatus(evt) => {
            let session_id = &evt.session_id;
            let busy_like = matches!(
                evt.status,
                opencode_schema::session_status::SessionStatusInfo::Busy
                    | opencode_schema::session_status::SessionStatusInfo::Retry { .. }
            );
            if busy_like {
                sets.active.insert(session_id.clone());
                sets.errored.remove(session_id);
                return Vec::new();
            }
            if !sets.active.remove(session_id) {
                return Vec::new();
            }
            if sets.errored.remove(session_id) {
                return Vec::new();
            }
            let is_subagent = app
                .state
                .sync
                .session(session_id)
                .map(|session| session.parent_id.is_some())
                .unwrap_or(false);
            let _ = is_subagent;
            vec![ask_effect(app, Some(session_id), "Session done")]
        }
        Event::SessionError(evt) => {
            let Some(session_id) = &evt.session_id else {
                return Vec::new();
            };
            if !app.ui.attention.active.contains(session_id) {
                return Vec::new();
            }
            app.ui
                .attention
                .errored
                .insert(evt.session_id.clone().expect("checked above"));
            vec![ask_effect(
                app,
                Some(session_id),
                session_error_message(&evt.error),
            )]
        }
        _ => Vec::new(),
    }
}

/// A test double — records every notification/bell request.
#[derive(Debug, Default)]
pub struct AttentionRecorder {
    requests: std::sync::Mutex<Vec<(String, Option<String>)>>,
    bells: std::sync::Mutex<usize>,
}

impl AttentionRecorder {
    pub fn new() -> AttentionRecorder {
        AttentionRecorder::default()
    }

    /// The (message, title) pairs, in order.
    pub fn notifications(&self) -> Vec<(String, Option<String>)> {
        self.requests.lock().unwrap().clone()
    }

    pub fn bells(&self) -> usize {
        *self.bells.lock().unwrap()
    }
}

impl Attention for AttentionRecorder {
    fn trigger_notification(&self, message: &str, title: &str) -> bool {
        let title = (title != DEFAULT_TITLE).then(|| title.to_string());
        self.requests
            .lock()
            .unwrap()
            .push((message.to_string(), title));
        true
    }

    fn bell(&self) -> bool {
        *self.bells.lock().unwrap() += 1;
        true
    }
}

/// Convenience for wiring [`App`]'s seam default.
pub fn terminal_attention() -> Arc<dyn Attention> {
    Arc::new(TerminalAttention)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{App, Args};
    use opencode_schema::session_status::SessionStatusData;
    use opencode_schema::session_v1::{AssistantError, SessionErrorData};

    fn session_info(id: &str, title: &str) -> opencode_schema::session_v1::V1SessionInfo {
        opencode_schema::session_v1::V1SessionInfo {
            id: id.into(),
            slug: "x".into(),
            project_id: "prj".into(),
            workspace_id: None,
            directory: "/x".into(),
            path: None,
            parent_id: None,
            summary: None,
            cost: None,
            tokens: None,
            share: None,
            title: title.into(),
            agent: None,
            model: None,
            version: "1".into(),
            metadata: None,
            time: opencode_schema::session_v1::V1SessionTime {
                created: 0,
                updated: 0,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert: None,
        }
    }

    fn app() -> App {
        App::new(crate::config::TuiConfig::default(), Args::default(), None)
    }

    fn attention_effects(app: &mut App, event: Event) -> Vec<Effect> {
        on_bus_event(app, &event)
    }

    #[test]
    fn notify_gates_on_enabled() {
        let request = NotifyRequest {
            title: Some("T".into()),
            message: "m".into(),
            notification: true,
            bell: true,
        };
        let recorder = AttentionRecorder::new();
        let config = crate::config::AttentionConfig::default();
        assert_eq!(
            notify(&request, &config, &recorder),
            NotifyOutcome::default(),
            "enabled defaults to false"
        );
        assert_eq!(recorder.notifications().len(), 0);
        assert_eq!(recorder.bells(), 0);

        let config = crate::config::AttentionConfig {
            enabled: true,
            ..crate::config::AttentionConfig::default()
        };
        notify(&request, &config, &recorder);
        assert_eq!(
            recorder.notifications(),
            vec![("m".into(), Some("T".into()))]
        );
        assert_eq!(recorder.bells(), 1);
    }

    #[test]
    fn notify_honors_the_per_request_flags() {
        let recorder = AttentionRecorder::new();
        let config = crate::config::AttentionConfig {
            enabled: true,
            ..crate::config::AttentionConfig::default()
        };
        notify(
            &NotifyRequest {
                title: None,
                message: "subagent turn".into(),
                notification: false,
                bell: true,
            },
            &config,
            &recorder,
        );
        assert!(recorder.notifications().is_empty());
        assert_eq!(recorder.bells(), 1);

        notify(
            &NotifyRequest {
                title: None,
                message: "muted".into(),
                notification: true,
                bell: false,
            },
            &config,
            &recorder,
        );
        assert_eq!(
            recorder.notifications(),
            vec![("muted".to_string(), None)],
            "the opencode default title collapses to None"
        );
        assert_eq!(recorder.bells(), 1);
    }

    #[test]
    fn question_and_permission_asks_dedup_by_id() {
        let mut app = app();
        let event = Event::QuestionAsked(opencode_schema::question_v1::QuestionAskedData {
            id: "q_1".into(),
            session_id: "ses_1".into(),
            questions: Vec::new(),
            tool: None,
        });
        assert_eq!(attention_effects(&mut app, event.clone()).len(), 1);
        assert_eq!(attention_effects(&mut app, event.clone()).len(), 0, "dedup");
        let effects = attention_effects(
            &mut app,
            Event::QuestionReplied(opencode_schema::question_v1::QuestionRepliedData {
                session_id: "ses_1".into(),
                request_id: "q_1".into(),
                answers: Vec::new(),
            }),
        );
        assert!(effects.is_empty());
        assert_eq!(
            attention_effects(&mut app, event).len(),
            1,
            "re-asks after reply"
        );
    }

    #[test]
    fn idle_without_active_session_stays_silent() {
        let mut app = app();
        let event = Event::SessionStatus(SessionStatusData {
            session_id: "ses_1".into(),
            status: opencode_schema::session_status::SessionStatusInfo::Idle,
        });
        assert!(attention_effects(&mut app, event).is_empty());
    }

    #[test]
    fn busy_then_idle_notifies_once_and_error_suppresses_done() {
        let mut app = app();
        let busy = Event::SessionStatus(SessionStatusData {
            session_id: "ses_1".into(),
            status: opencode_schema::session_status::SessionStatusInfo::Busy,
        });
        let idle = Event::SessionStatus(SessionStatusData {
            session_id: "ses_1".into(),
            status: opencode_schema::session_status::SessionStatusInfo::Idle,
        });
        attention_effects(&mut app, busy.clone());
        assert!(attention_effects(&mut app, idle.clone())
            .iter()
            .any(|effect| matches!(
                effect,
                Effect::Attention { message, .. } if message == "Session done"
            )));
        // idle removed the session from active — a second idle is silent.
        assert!(attention_effects(&mut app, idle.clone()).is_empty());

        // busy → error → idle: the done notification is suppressed.
        attention_effects(&mut app, busy);
        let error = Event::SessionError(SessionErrorData {
            session_id: Some("ses_1".into()),
            error: AssistantError::OutputLength {},
        });
        let effects = attention_effects(&mut app, error);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Attention { message, .. }] if message == "Session error"
        ));
        assert!(
            attention_effects(&mut app, idle).is_empty(),
            "errored sessions do not fire Session done"
        );
    }

    #[test]
    fn ask_effects_carry_the_session_title() {
        let mut app = app();
        app.state.sync.session = vec![session_info("ses_1", "Fix the build")];
        let effects = attention_effects(
            &mut app,
            Event::QuestionAsked(opencode_schema::question_v1::QuestionAskedData {
                id: "q_1".into(),
                session_id: "ses_1".into(),
                questions: Vec::new(),
                tool: None,
            }),
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::Attention { title, notification: true, .. }] if title.as_deref() == Some("Fix the build")
        ));
    }
}
