//! `routes/session/permission.tsx` — the once/always/reject permission
//! prompt (M8.7). Renders in the session bottom stack above the prompt;
//! requests across all child sessions surface on the parent
//! (`session/index.tsx:230-234`).

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph, Widget};

use crate::state::route::Route;
use crate::state::{App, Effect};
use crate::ui::theme::{selected_foreground, Theme};
use opencode_schema::permission_v1::PermissionV1Reply;
use opencode_schema::permission_v1::PermissionV1Request;
use opencode_schema::session_v1::V1Part;
use serde_json::Value;

/// `PermissionStage` (`permission.tsx:20`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionStage {
    #[default]
    Permission,
    Always,
    Reject,
}

/// The prompt state machine (`permission.tsx:115-117` + the reject
/// textarea).
#[derive(Debug, Default)]
pub struct PermissionState {
    /// The request the stage belongs to — a new head request resets.
    pub request_id: Option<String>,
    pub stage: PermissionStage,
    /// The selected option row (`store.selected`).
    pub selected: usize,
    /// `store.expanded` — the fullscreen toggle (`ctrl+f`).
    pub expanded: bool,
    /// The reject-stage textarea content.
    pub reject_input: String,
}

const OPTIONS: [(&str, &str); 3] = [
    ("once", "Allow once"),
    ("always", "Allow always"),
    ("reject", "Reject"),
];

/// `permissions()` (`session/index.tsx:230-234`): the parentless session
/// surfaces the requests of all its children, sorted by id; the head is
/// shown (`session/index.tsx:1297-1300`).
pub fn visible(app: &App) -> Option<PermissionV1Request> {
    let Route::Session { session_id, .. } = &app.state.route.data else {
        return None;
    };
    let session = app.state.sync.session(session_id)?;
    if session.parent_id.is_some() {
        return None;
    }
    let parent = &session.id;
    let mut requests: Vec<PermissionV1Request> = Vec::new();
    for child in &app.state.sync.session {
        if child.parent_id.as_deref() == Some(parent.as_str()) || child.id == *parent {
            if let Some(list) = app.state.sync.permission.get(&child.id) {
                requests.extend(list.iter().cloned());
            }
        }
    }
    requests.sort_by(|a, b| a.id.cmp(&b.id));
    requests.into_iter().next()
}

/// Reset the stage when the head request changes.
pub fn observe(app: &mut App) {
    let request = visible(app);
    let request_id = request.as_ref().map(|request| request.id.clone());
    if request.is_none() {
        app.ui.permission.request_id = None;
        return;
    }
    if app.ui.permission.request_id != request_id {
        app.ui.permission.request_id = request_id;
        app.ui.permission.stage = PermissionStage::Permission;
        app.ui.permission.selected = 0;
        app.ui.permission.reject_input.clear();
    }
}

/// `input()` (`permission.tsx:122-132`) — the tool part's state input.
fn tool_input(app: &App, request: &PermissionV1Request) -> Value {
    let Some(tool) = &request.tool else {
        return Value::Null;
    };
    for part in app
        .state
        .sync
        .part
        .get(&tool.message_id)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        if let V1Part::Tool { call_id, state, .. } = part {
            if *call_id == tool.call_id {
                return match state {
                    opencode_schema::session_v1::V1ToolState::Pending { input, .. }
                    | opencode_schema::session_v1::V1ToolState::Running { input, .. }
                    | opencode_schema::session_v1::V1ToolState::Completed { input, .. }
                    | opencode_schema::session_v1::V1ToolState::Error { input, .. } => {
                        Value::Object(input.clone())
                    }
                };
            }
        }
    }
    Value::Null
}

fn string_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The per-permission body (`permission.tsx:194-381`): icon, title, body
/// lines.
#[allow(clippy::type_complexity)]
fn info(app: &App, request: &PermissionV1Request) -> (&'static str, String, Vec<Line<'static>>) {
    let theme = app_theme(app);
    let data = tool_input(app, request);
    let muted = Style::new().fg(theme.text_muted.to_color());
    let text = Style::new().fg(theme.text.to_color());
    let body_line = |line: String| Line::styled(line, text);
    let muted_line = |line: String| Line::styled(line, muted);
    match request.permission.as_str() {
        "edit" => {
            let meta = Value::Object(request.metadata.clone());
            let filepath = string_of(&meta, "filepath");
            (
                "→",
                format!("Edit {filepath}"),
                vec![muted_line("  No diff provided".to_string())],
            )
        }
        "read" => {
            let path = string_of(&data, "filePath");
            (
                "→",
                format!("Read {path}"),
                if path.is_empty() {
                    Vec::new()
                } else {
                    vec![muted_line(format!("  Path: {path}"))]
                },
            )
        }
        "glob" => {
            let pattern = string_of(&data, "pattern");
            (
                "✱",
                format!("Glob \"{pattern}\""),
                if pattern.is_empty() {
                    Vec::new()
                } else {
                    vec![muted_line(format!("  Pattern: {pattern}"))]
                },
            )
        }
        "grep" => {
            let pattern = string_of(&data, "pattern");
            (
                "✱",
                format!("Grep \"{pattern}\""),
                if pattern.is_empty() {
                    Vec::new()
                } else {
                    vec![muted_line(format!("  Pattern: {pattern}"))]
                },
            )
        }
        "list" => {
            let dir = string_of(&data, "path");
            (
                "→",
                format!("List {dir}"),
                if dir.is_empty() {
                    Vec::new()
                } else {
                    vec![muted_line(format!("  Path: {dir}"))]
                },
            )
        }
        "bash" => {
            let command = string_of(&data, "command");
            (
                "#",
                "Shell command".to_string(),
                if command.is_empty() {
                    Vec::new()
                } else {
                    vec![body_line(format!("  $ {command}"))]
                },
            )
        }
        "task" => {
            let task_type = {
                let value = string_of(&data, "subagent_type");
                if value.is_empty() {
                    "Unknown".to_string()
                } else {
                    value
                }
            };
            let description = string_of(&data, "description");
            (
                "#",
                crate::ui::locale::titlecase(&task_type) + " Task",
                if description.is_empty() {
                    Vec::new()
                } else {
                    vec![body_line(format!("  ◉ {description}"))]
                },
            )
        }
        "webfetch" => {
            let url = string_of(&data, "url");
            (
                "%",
                format!("WebFetch {url}"),
                if url.is_empty() {
                    Vec::new()
                } else {
                    vec![muted_line(format!("  URL: {url}"))]
                },
            )
        }
        "websearch" => {
            let query = string_of(&data, "query");
            (
                "◈",
                format!("WebSearch \"{query}\""),
                if query.is_empty() {
                    Vec::new()
                } else {
                    vec![muted_line(format!("  Query: {query}"))]
                },
            )
        }
        "external_directory" => {
            let patterns: Vec<String> = request
                .patterns
                .iter()
                .map(|pattern| format!("  - {pattern}"))
                .collect();
            let dir = request
                .patterns
                .first()
                .map(|pattern| {
                    if pattern.contains('*') {
                        pattern
                            .rsplit_once('/')
                            .map(|(head, _)| head.to_string())
                            .unwrap_or_default()
                    } else {
                        pattern.clone()
                    }
                })
                .unwrap_or_default();
            (
                "←",
                format!("Access external directory {dir}"),
                if patterns.is_empty() {
                    Vec::new()
                } else {
                    patterns
                        .into_iter()
                        .map(|pattern| Line::styled(pattern, text))
                        .collect()
                },
            )
        }
        "doom_loop" => (
            "�",
            "Continue after repeated failures".to_string(),
            vec![muted_line(
                "  This keeps the session running despite repeated failures.".to_string(),
            )],
        ),
        permission => (
            "⚙",
            format!("Call tool {permission}"),
            vec![muted_line(format!("  Tool: {permission}"))],
        ),
    }
}

fn app_theme(app: &App) -> Theme {
    app.ui
        .theme
        .resolve(&app.state.kv)
        .expect("builtin theme resolves")
}

fn reply_effect(
    request: &PermissionV1Request,
    reply: PermissionV1Reply,
    message: Option<String>,
) -> Effect {
    Effect::PermissionReply {
        request_id: request.id.clone(),
        reply,
        message,
    }
}

/// Handle a key while a permission request is pending. `None` = no
/// request pending (fall through to the normal keymap).
pub fn handle_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Option<Vec<Effect>> {
    let request = visible(app)?;
    let session_parent = app
        .state
        .sync
        .session(&request.session_id)
        .map(|session| session.parent_id.clone())
        .unwrap_or(None);
    let stage = app.ui.permission.stage;
    let kind = key.code;
    let left_right = matches!(
        kind,
        crossterm::event::KeyCode::Left | crossterm::event::KeyCode::Char('h')
    );
    let right = matches!(
        kind,
        crossterm::event::KeyCode::Right | crossterm::event::KeyCode::Char('l')
    );
    let escape = kind == crossterm::event::KeyCode::Esc;
    let enter = kind == crossterm::event::KeyCode::Enter;
    let state = app.keymap.matches("app_exit", key);

    match stage {
        PermissionStage::Permission => {
            if left_right {
                let selected = &mut app.ui.permission.selected;
                *selected = (*selected + OPTIONS.len() - 1) % OPTIONS.len();
            } else if right {
                let selected = &mut app.ui.permission.selected;
                *selected = (*selected + 1) % OPTIONS.len();
            } else if enter {
                return Some(select(app, &request, session_parent));
            } else if escape || state {
                // `escapeKey: "reject"` (`permission.tsx:405-407`).
                app.ui.permission.selected = 2;
                return Some(select(app, &request, session_parent));
            } else if app.keymap.matches("permission.prompt.fullscreen", key) {
                app.ui.permission.expanded = !app.ui.permission.expanded;
            }
        }
        PermissionStage::Always => {
            if left_right || right {
                app.ui.permission.selected = 1 - app.ui.permission.selected;
            } else if enter {
                let selected = app.ui.permission.selected;
                app.ui.permission.stage = PermissionStage::Permission;
                app.ui.permission.selected = 0;
                if selected == 1 {
                    return Some(vec![reply_effect(
                        &request,
                        PermissionV1Reply::Always,
                        None,
                    )]);
                }
            } else if escape || state {
                // `escapeKey: "cancel"`.
                app.ui.permission.stage = PermissionStage::Permission;
                app.ui.permission.selected = 0;
            }
        }
        PermissionStage::Reject => {
            if escape || state {
                app.ui.permission.stage = PermissionStage::Permission;
            } else if enter {
                let message = if app.ui.permission.reject_input.is_empty() {
                    None
                } else {
                    Some(app.ui.permission.reject_input.clone())
                };
                return Some(vec![reply_effect(
                    &request,
                    PermissionV1Reply::Reject,
                    message,
                )]);
            } else if let crossterm::event::KeyCode::Backspace = kind {
                app.ui.permission.reject_input.pop();
            } else if !key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
            {
                if let crossterm::event::KeyCode::Char(char) = kind {
                    app.ui.permission.reject_input.push(char);
                }
            }
        }
    }
    Some(Vec::new())
}

/// `onSelect` (`permission.tsx:408-432`).
fn select(
    app: &mut App,
    request: &PermissionV1Request,
    session_parent: Option<String>,
) -> Vec<Effect> {
    let selected = app.ui.permission.selected;
    let option = OPTIONS[selected].0;
    match option {
        "once" => vec![reply_effect(request, PermissionV1Reply::Once, None)],
        "always" => {
            app.ui.permission.stage = PermissionStage::Always;
            app.ui.permission.selected = 1;
            Vec::new()
        }
        _ => {
            // "reject"
            if session_parent.is_some() {
                app.ui.permission.stage = PermissionStage::Reject;
                app.ui.permission.reject_input.clear();
                Vec::new()
            } else {
                vec![reply_effect(request, PermissionV1Reply::Reject, None)]
            }
        }
    }
}

/// The option row + the rendered line count.
fn option_row(state: &PermissionState, theme: &Theme) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, (_, label)) in OPTIONS.iter().enumerate() {
        let active = index == state.selected;
        spans.push(Span::styled(
            format!(" {label} "),
            Style::new()
                .fg(if active {
                    selected_foreground(theme, Some(theme.warning))
                } else {
                    theme.text_muted
                }
                .to_color())
                .bg(if active {
                    theme.warning.to_color()
                } else {
                    theme.background_menu.to_color()
                }),
        ));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

/// The permission box's rendered rows (the box is the left-bordered
/// panel; `maxHeight: 15` when not expanded).
pub fn lines(app: &App) -> Vec<Line<'static>> {
    let theme = app_theme(app);
    let Some(request) = visible(app) else {
        return Vec::new();
    };
    let state = &app.ui.permission;
    let mut rows: Vec<Line<'static>> = Vec::new();
    match state.stage {
        PermissionStage::Permission => {
            let (icon, title, body) = info(app, &request);
            rows.push(Line::from(vec![
                Span::styled("△ ", Style::new().fg(theme.warning.to_color())),
                Span::styled(
                    "Permission required".to_string(),
                    Style::new().fg(theme.text.to_color()),
                ),
            ]));
            rows.push(Line::from(vec![
                Span::styled(
                    format!("{icon} "),
                    Style::new().fg(theme.text_muted.to_color()),
                ),
                Span::styled(title, Style::new().fg(theme.text.to_color())),
            ]));
            rows.extend(body);
            rows.push(option_row(state, &theme));
            rows.push(Line::from(vec![
                Span::styled("⇆ ", Style::new().fg(theme.text.to_color())),
                Span::styled("select", Style::new().fg(theme.text_muted.to_color())),
                Span::styled("   enter ", Style::new().fg(theme.text.to_color())),
                Span::styled("confirm", Style::new().fg(theme.text_muted.to_color())),
            ]));
        }
        PermissionStage::Always => {
            rows.push(Line::from(vec![
                Span::styled("△ ", Style::new().fg(theme.warning.to_color())),
                Span::styled(
                    "Always allow".to_string(),
                    Style::new().fg(theme.text.to_color()),
                ),
            ]));
            if request.always.len() == 1 && request.always[0] == "*" {
                rows.push(Line::styled(
                    format!(
                        "  This will allow {} until OpenCode is restarted.",
                        request.permission
                    ),
                    Style::new().fg(theme.text_muted.to_color()),
                ));
            } else {
                rows.push(Line::styled(
                    "  This will allow the following patterns until OpenCode is restarted",
                    Style::new().fg(theme.text_muted.to_color()),
                ));
                for pattern in &request.always {
                    rows.push(Line::styled(
                        format!("  - {pattern}"),
                        Style::new().fg(theme.text.to_color()),
                    ));
                }
            }
            let labels = ["Cancel", "Confirm"];
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (index, label) in labels.iter().enumerate() {
                let active = index == state.selected;
                spans.push(Span::styled(
                    format!(" {label} "),
                    Style::new()
                        .fg(if active {
                            selected_foreground(&theme, Some(theme.warning))
                        } else {
                            theme.text_muted
                        }
                        .to_color())
                        .bg(if active {
                            theme.warning.to_color()
                        } else {
                            theme.background_menu.to_color()
                        }),
                ));
            }
            rows.push(Line::from(spans));
        }
        PermissionStage::Reject => {
            rows.push(Line::from(vec![
                Span::styled("△ ", Style::new().fg(theme.error.to_color())),
                Span::styled(
                    "Reject permission".to_string(),
                    Style::new().fg(theme.text.to_color()),
                ),
            ]));
            rows.push(Line::styled(
                "  Tell OpenCode what to do differently",
                Style::new().fg(theme.text_muted.to_color()),
            ));
            rows.push(Line::styled(
                format!("  {}", state.reject_input),
                Style::new().fg(theme.text.to_color()),
            ));
            rows.push(Line::from(vec![
                Span::styled("enter ", Style::new().fg(theme.text.to_color())),
                Span::styled("confirm", Style::new().fg(theme.text_muted.to_color())),
                Span::styled("   esc ", Style::new().fg(theme.text.to_color())),
                Span::styled("cancel", Style::new().fg(theme.text_muted.to_color())),
            ]));
        }
    }
    rows
}

/// `maxHeight: 15` when not expanded (`permission.tsx:638-647`).
const MAX_HEIGHT: usize = 15;

pub fn height(app: &App) -> u16 {
    let count = lines(app).len() + 2;
    if app.ui.permission.expanded {
        count as u16
    } else {
        (count.min(MAX_HEIGHT)) as u16
    }
}

/// Render into the prompt slot.
pub fn render(app: &App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    let rows = lines(app);
    let border = match app.ui.permission.stage {
        PermissionStage::Reject => theme.error,
        _ => theme.warning,
    };
    Paragraph::new(rows)
        .block(
            Block::new()
                .borders(ratatui::widgets::Borders::LEFT)
                .border_style(border.to_color())
                .style(Style::new().bg(theme.background_panel.to_color()))
                .padding(Padding {
                    left: 1,
                    right: 3,
                    top: 1,
                    bottom: 1,
                }),
        )
        .render(area, frame.buffer_mut());
}
