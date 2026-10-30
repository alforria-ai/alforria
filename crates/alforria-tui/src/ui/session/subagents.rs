//! What the session screen says about subagents: their state at a glance, and
//! the system's notes about them, which the model reads as text but a person
//! should see as what they are.

use alforria_schema::session_status::SessionStatusInfo;
use alforria_schema::session_v1::{V1Message, V1Part, V1ToolState};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::parts::assistant_error_message;
use crate::state::App;
use crate::ui::theme::{Rgba, Theme};

/// The most subagents the docked list shows.
const DOCKED_ROWS: usize = 3;

/// A system note in a transcript.
#[derive(Debug, PartialEq, Eq)]
pub enum Note {
    /// A subagent's result delivered to its parent outside a `task` call.
    Update {
        session_id: String,
        state: String,
        summary: String,
    },
    /// A subagent was resumed automatically after an error.
    Resume { error: String },
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)?;
    Some(&text[start..start + end])
}

/// The note's summary is written for the model; say it to the person.
fn update_summary(summary: &str) -> String {
    match summary.strip_prefix("The user continued this subagent directly: ") {
        Some(title) => format!(
            "You continued {}. The main agent has its result.",
            short_title(title)
        ),
        None => short_title(summary).to_string(),
    }
}

/// Recognize a synthetic text part as one of the notes.
pub fn note_of(text: &str) -> Option<Note> {
    if text.starts_with("<auto_resume>") {
        let error = between(text, "stopped with an error: ", "\n\n")?;
        return Some(Note::Resume {
            error: first_line(error),
        });
    }
    if text.starts_with("<task id=\"") {
        return Some(Note::Update {
            session_id: between(text, "<task id=\"", "\"")?.to_string(),
            state: between(text, " state=\"", "\"")?.to_string(),
            summary: update_summary(between(text, "<summary>", "</summary>")?),
        });
    }
    None
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// A tool error as a person should read it: the server stores a failure as a
/// serialized `{ name, data: { message } }`.
pub fn tool_error_text(error: &str) -> String {
    serde_json::from_str::<Value>(error)
        .ok()
        .and_then(|value| {
            value
                .pointer("/data/message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| error.to_string())
}

/// The one line that says why a subagent failed, without the instructions the
/// parent's copy of the error carries for the model.
pub fn failure_line(error: &str) -> String {
    let text = first_line(error);
    match text.strip_prefix("Subagent failed (task_id: ") {
        Some(rest) => rest
            .split_once("): ")
            .map_or(text.clone(), |(_, reason)| reason.to_string()),
        None => text,
    }
}

/// `Research (@general subagent)` → `Research`.
pub fn short_title(title: &str) -> &str {
    match title.rfind(" (@") {
        Some(index) if title.ends_with(')') => &title[..index],
        _ => title,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Running,
    Retrying,
    Waiting,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub title: String,
    pub state: RowState,
    pub detail: Option<String>,
    /// When its session last changed, in epoch milliseconds.
    pub updated: f64,
}

fn is_active(state: RowState) -> bool {
    matches!(
        state,
        RowState::Running | RowState::Retrying | RowState::Waiting
    )
}

/// The newest assistant message's error, and what its session is doing now.
/// `awaited` says a `task` call is still running on it, in which case a failure
/// is on its way to being resumed.
fn row(app: &App, session_id: &str, title: &str, updated: f64, awaited: bool) -> Row {
    let sync = &app.state.sync;
    let messages = sync
        .message
        .get(session_id)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let newest = messages.iter().rev().find_map(|message| match message {
        V1Message::Assistant { id, error, .. } => Some((id, error)),
        V1Message::User { .. } => None,
    });
    let waiting = sync
        .permission
        .get(session_id)
        .is_some_and(|requests| !requests.is_empty())
        || sync
            .question
            .get(session_id)
            .is_some_and(|requests| !requests.is_empty());
    let (state, detail) = match sync.session_status.get(session_id) {
        _ if waiting => (RowState::Waiting, Some("needs your answer".to_string())),
        Some(SessionStatusInfo::Retry {
            attempt, message, ..
        }) => (
            RowState::Retrying,
            Some(format!("attempt {attempt}: {}", first_line(message))),
        ),
        Some(SessionStatusInfo::Busy) => {
            let current = newest.and_then(|(id, _)| {
                sync.part
                    .get(id.as_str())?
                    .iter()
                    .rev()
                    .find_map(|part| match part {
                        V1Part::Tool {
                            tool,
                            state: V1ToolState::Running { title, .. },
                            ..
                        } => Some(match title {
                            Some(title) => format!("{tool} {title}"),
                            None => tool.clone(),
                        }),
                        _ => None,
                    })
            });
            (RowState::Running, current)
        }
        _ => match newest.and_then(|(_, error)| error.as_ref()) {
            Some(alforria_schema::session_v1::AssistantError::Aborted { .. }) => {
                (RowState::Cancelled, None)
            }
            Some(error) if awaited => (
                RowState::Retrying,
                Some(format!(
                    "resuming: {}",
                    first_line(&assistant_error_message(error))
                )),
            ),
            Some(error) => (
                RowState::Failed,
                Some(first_line(&assistant_error_message(error))),
            ),
            None => (RowState::Done, None),
        },
    };
    Row {
        title: short_title(title).to_string(),
        state,
        detail,
        updated,
    }
}

/// The sessions of this session's `task` calls that are still running.
fn awaited_children(app: &App, session_id: &str) -> Vec<String> {
    let sync = &app.state.sync;
    sync.message
        .get(session_id)
        .into_iter()
        .flatten()
        .flat_map(|message| {
            let id = match message {
                V1Message::User { id, .. } | V1Message::Assistant { id, .. } => id,
            };
            sync.part.get(id.as_str()).map(Vec::as_slice).unwrap_or(&[])
        })
        .filter(|part| {
            matches!(
                part,
                V1Part::Tool {
                    state: V1ToolState::Running { .. },
                    ..
                }
            )
        })
        .filter_map(super::parts::task_child_session)
        .collect()
}

/// A session's subagents, the ones still at work first.
pub fn rows(app: &App, session_id: &str) -> Vec<Row> {
    let awaited = awaited_children(app, session_id);
    let mut rows: Vec<Row> = app
        .state
        .sync
        .session
        .iter()
        .filter(|session| session.parent_id.as_deref() == Some(session_id))
        // A workflow's agents are shown with their workflow.
        .filter(|session| {
            !session
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.contains_key("workflowAgent"))
        })
        .map(|session| {
            row(
                app,
                &session.id,
                &session.title,
                session.time.updated as f64,
                awaited.contains(&session.id),
            )
        })
        .collect();
    rows.sort_by_key(|row| !is_active(row.state));
    rows
}

fn mark(theme: &Theme, state: RowState) -> (&'static str, Rgba) {
    match state {
        RowState::Running => ("•", theme.warning),
        RowState::Retrying => ("↻", theme.warning),
        RowState::Waiting => ("!", theme.warning),
        RowState::Done => ("✓", theme.text_muted),
        RowState::Failed => ("✗", theme.error),
        RowState::Cancelled => ("-", theme.text_muted),
    }
}

fn row_line(theme: &Theme, row: &Row) -> Line<'static> {
    let (mark, color) = mark(theme, row.state);
    let mut spans = vec![
        Span::styled(format!("{mark} "), Style::new().fg(color.to_color())),
        Span::styled(row.title.clone(), Style::new().fg(theme.text.to_color())),
    ];
    if let Some(detail) = &row.detail {
        spans.push(Span::styled(
            format!(" · {detail}"),
            Style::new().fg(color.to_color()),
        ));
    }
    Line::from(spans)
}

fn heading(theme: &Theme) -> Span<'static> {
    Span::styled(
        "Subagents",
        Style::new()
            .fg(theme.text.to_color())
            .add_modifier(Modifier::BOLD),
    )
}

/// The sidebar section: every subagent of the session.
pub fn sidebar_lines(app: &App, theme: &Theme, session_id: &str) -> Vec<Line<'static>> {
    let rows = rows(app, session_id);
    if rows.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![Line::from(heading(theme))];
    lines.extend(rows.iter().map(|row| row_line(theme, row)));
    lines
}

/// The list above the prompt when the sidebar is hidden, while subagents are
/// at work: finished ones are in the transcript by then. A failure in this turn
/// stays until the turn is over, since the parent may be about to resume it.
pub fn docked_lines(app: &App, theme: &Theme, session_id: &str) -> Vec<Line<'static>> {
    let turn_started = app
        .state
        .sync
        .message
        .get(session_id)
        .into_iter()
        .flatten()
        .rev()
        .find_map(|message| match message {
            V1Message::User { time, .. } => Some(time.created),
            V1Message::Assistant { .. } => None,
        })
        .unwrap_or_default();
    let rows: Vec<Row> = rows(app, session_id)
        .into_iter()
        .filter(|row| row.state != RowState::Failed || row.updated >= turn_started)
        .collect();
    let active = rows.iter().filter(|row| is_active(row.state)).count();
    let failed = rows
        .iter()
        .filter(|row| row.state == RowState::Failed)
        .count();
    let parent_busy = !matches!(
        app.state.sync.session_status.get(session_id),
        None | Some(SessionStatusInfo::Idle)
    );
    if active == 0 && !(failed > 0 && parent_busy) {
        return Vec::new();
    }
    let shown: Vec<&Row> = rows
        .iter()
        .filter(|row| is_active(row.state) || row.state == RowState::Failed)
        .collect();
    let mut counts = Vec::new();
    if active > 0 {
        counts.push(format!("{active} running"));
    }
    if failed > 0 {
        counts.push(format!("{failed} failed"));
    }
    let muted = Style::new().fg(theme.text_muted.to_color());
    let mut header = vec![
        heading(theme),
        Span::styled(format!(" {}", counts.join(" · ")), muted),
    ];
    if shown.len() > DOCKED_ROWS {
        header.push(Span::styled(
            format!(" · +{} more", shown.len() - DOCKED_ROWS),
            muted,
        ));
    }
    let view = app.keymap.shortcut("session.child.first");
    if !view.is_empty() {
        header.push(Span::styled(format!("  {view} to view"), muted));
    }
    let mut lines = vec![Line::from(header)];
    lines.extend(
        shown
            .iter()
            .take(DOCKED_ROWS)
            .map(|row| row_line(theme, row)),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_the_system_notes() {
        let note = "<auto_resume>\nYour previous attempt stopped with an error: boom\nmore\n\nContinue the task.\n</auto_resume>";
        assert_eq!(
            note_of(note),
            Some(Note::Resume {
                error: "boom".to_string()
            })
        );
        let update = "<task id=\"ses_1\" state=\"completed\">\n<summary>The user continued this subagent directly: Research (@general subagent)</summary>\n<task_result>\nok\n</task_result>\n</task>";
        assert_eq!(
            note_of(update),
            Some(Note::Update {
                session_id: "ses_1".to_string(),
                state: "completed".to_string(),
                summary: "You continued Research. The main agent has its result.".to_string(),
            })
        );
        assert_eq!(note_of("The following tool was executed"), None);
    }

    #[test]
    fn errors_read_as_sentences() {
        let stored = serde_json::json!({
            "name": "Error",
            "data": { "message": "Subagent failed (task_id: ses_1): boom\n\nIts session is intact." },
        })
        .to_string();
        let text = tool_error_text(&stored);
        assert!(text.starts_with("Subagent failed"), "{text}");
        assert_eq!(failure_line(&text), "boom");
        assert_eq!(tool_error_text("plain words"), "plain words");
        assert_eq!(failure_line("plain words"), "plain words");
    }

    #[test]
    fn titles_lose_the_agent_suffix() {
        assert_eq!(short_title("Research (@general subagent)"), "Research");
        assert_eq!(short_title("Plain title"), "Plain title");
    }
}
