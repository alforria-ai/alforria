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
use alforria_schema::permission_v1::PermissionV1Reply;
use alforria_schema::permission_v1::PermissionV1Request;
use alforria_schema::session_v1::V1Part;
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
    /// The rendered option-button rows (mouse hit-testing): screen row
    /// and each button's `(x, width)`.
    pub clicks: Vec<ClickRow>,
}

/// One clickable option-button row (`permission.tsx:676-693`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickRow {
    pub row: u16,
    pub buttons: Vec<(u16, u16)>,
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
                    // `Pending` has no input yet (permission.tsx:127).
                    alforria_schema::session_v1::V1ToolState::Running { input, .. }
                    | alforria_schema::session_v1::V1ToolState::Completed { input, .. }
                    | alforria_schema::session_v1::V1ToolState::Error { input, .. } => {
                        Value::Object(input.clone())
                    }
                    alforria_schema::session_v1::V1ToolState::Pending { .. } => Value::Null,
                };
            }
        }
    }
    Value::Null
}

/// `usePathFormatter()` (`context/path-format.tsx:26-40`) — paths
/// render relative to the instance directory, home-abbreviated.
fn format_path(app: &App, path: &str) -> String {
    let instance = &app.state.project.instance_path;
    crate::ui::locale::format_path(
        Some(path),
        &instance.directory.clone().unwrap_or_default(),
        &instance.home.clone().unwrap_or_default(),
    )
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
            // `EditBody` (permission.tsx:22-88, 199-206) — render the
            // diff via the same `<diff>` component as the transcript;
            // the muted fallback only when it is empty.
            let diff_content = Value::Object(request.metadata.clone())
                .get("diff")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let lines = if diff_content.is_empty() {
                vec![muted_line("  No diff provided".to_string())]
            } else {
                let style = if app.config.diff_style == "stacked" {
                    crate::ui::diff::DiffStyle::Stacked
                } else {
                    crate::ui::diff::DiffStyle::Auto
                };
                let width = app.ui.terminal_width;
                let view = crate::ui::diff::view_for(style, width);
                crate::ui::diff::render(
                    &diff_content,
                    view,
                    width.saturating_sub(4),
                    crate::ui::diff::WrapMode::Word,
                    &theme,
                )
            };
            ("→", format!("Edit {}", format_path(app, &filepath)), lines)
        }
        "read" => {
            let path = format_path(app, &string_of(&data, "filePath"));
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
            let dir = format_path(app, &string_of(&data, "path"));
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
            // `webSearchProviderLabel` (util/tool-display.ts:1-5).
            let label = match string_of(&data, "provider").as_str() {
                "parallel" => "Parallel Web Search",
                "exa" => "Exa Web Search",
                _ => "Web Search",
            };
            (
                "◈",
                format!("{label} \"{query}\""),
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
            // `parent ?? filepath ?? derived` (permission.tsx:333-341).
            let meta = Value::Object(request.metadata.clone());
            let dir = string_of(&meta, "parentDir")
                .is_empty()
                .then(|| string_of(&meta, "filepath"))
                .filter(|filepath| !filepath.is_empty())
                .unwrap_or_default();
            let dir = if dir.is_empty() {
                request
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
                    .unwrap_or_default()
            } else {
                dir
            };
            (
                "←",
                format!("Access external directory {}", format_path(app, &dir)),
                if patterns.is_empty() {
                    Vec::new()
                } else {
                    let mut lines = vec![Line::styled("Patterns", muted), Line::raw("")];
                    lines.extend(
                        patterns
                            .into_iter()
                            .map(|pattern| Line::styled(format!("- {pattern}"), text)),
                    );
                    lines
                },
            )
        }
        "doom_loop" => (
            "⟳",
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

/// Handle a key while a permission request is pending. The prompt owns
/// only its own bindings (`permission.tsx:449-470,545-620`): left/`h`,
/// right/`l`, return, escape, `app.exit` and the fullscreen toggle — plus
/// the reject textarea's typing. `None` = not the prompt's key (or no
/// request pending): it falls through to the keymap, so the base-mode
/// app and session bindings (`ctrl+p`, leader sequences, scrolling) stay
/// live as in TS, where the prompt replaces the unmounted `<Prompt>`.
pub fn handle_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Option<Vec<Effect>> {
    let request = visible(app)?;
    // A pending leader sequence completes in the keymap first.
    if !app.keymap.pending_sequence().is_empty() {
        return None;
    }
    let session_parent = app
        .state
        .sync
        .session(&request.session_id)
        .map(|session| session.parent_id.clone())
        .unwrap_or(None);
    let stage = app.ui.permission.stage;
    let kind = key.code;
    let plain = !key.modifiers.intersects(
        crossterm::event::KeyModifiers::CONTROL
            | crossterm::event::KeyModifiers::ALT
            | crossterm::event::KeyModifiers::SUPER,
    );
    let left_right = kind == crossterm::event::KeyCode::Left
        || (plain && kind == crossterm::event::KeyCode::Char('h'));
    let right = kind == crossterm::event::KeyCode::Right
        || (plain && kind == crossterm::event::KeyCode::Char('l'));
    let escape = kind == crossterm::event::KeyCode::Esc;
    let enter = kind == crossterm::event::KeyCode::Enter;
    let state = app.keymap.matches("app_exit", key);
    let fullscreen = app.keymap.matches("permission.prompt.fullscreen", key);

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
            } else if fullscreen {
                app.ui.permission.expanded = !app.ui.permission.expanded;
            } else {
                return None;
            }
        }
        PermissionStage::Always => {
            if left_right || right {
                app.ui.permission.selected = 1 - app.ui.permission.selected;
            } else if enter {
                let selected = app.ui.permission.selected;
                app.ui.permission.stage = PermissionStage::Permission;
                app.ui.permission.selected = 0;
                if selected == 0 {
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
            } else if !fullscreen {
                return None;
            }
        }
        PermissionStage::Reject => {
            // The focused textarea takes its editing keys
            // (`permission.tsx:501-510`); chords fall through.
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
            } else if let (true, crossterm::event::KeyCode::Char(char)) = (plain, kind) {
                app.ui.permission.reject_input.push(char);
            } else if !is_textarea_key(kind) {
                return None;
            }
        }
    }
    Some(Vec::new())
}

/// The editing keys a focused textarea consumes even where this port's
/// single-line input has no use for them (cursor movement).
pub(crate) fn is_textarea_key(code: crossterm::event::KeyCode) -> bool {
    matches!(
        code,
        crossterm::event::KeyCode::Left
            | crossterm::event::KeyCode::Right
            | crossterm::event::KeyCode::Up
            | crossterm::event::KeyCode::Down
            | crossterm::event::KeyCode::Home
            | crossterm::event::KeyCode::End
            | crossterm::event::KeyCode::Delete
            | crossterm::event::KeyCode::Tab
            | crossterm::event::KeyCode::BackTab
    )
}

/// The prompt's `app.exit` command (`permission.tsx:451-460,547-555`):
/// while a request shows, the exit chords — `<leader>q` included — reject
/// (or step back), they never quit. `None` when no request is pending.
pub fn app_exit(app: &mut App) -> Option<Vec<Effect>> {
    visible(app)?;
    handle_key(
        app,
        &crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ),
    )
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
            app.ui.permission.selected = 0;
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
            // The header wrapper pads the block by 1; the icon row adds
            // its own `paddingLeft={2}` (`permission.tsx:386-397, 651`).
            rows.push(Line::from(vec![
                Span::raw(" "),
                Span::styled("△ ", Style::new().fg(theme.warning.to_color())),
                Span::styled(
                    "Permission required".to_string(),
                    Style::new().fg(theme.text.to_color()),
                ),
            ]));
            rows.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(
                    format!("{icon} "),
                    Style::new().fg(theme.text_muted.to_color()),
                ),
                Span::styled(title, Style::new().fg(theme.text.to_color())),
            ]));
            rows.extend(body);
            rows.push(option_row(state, &theme));
            // The `permission.prompt.fullscreen` shortcut hint
            // (`permission.tsx:698-702`).
            let fullscreen_hint =
                crate::keymap::bindings::keybind_for_command("permission.prompt.fullscreen")
                    .and_then(|keybind| crate::ui::dialogs::key_hint(app, keybind))
                    .unwrap_or_default();
            rows.push(Line::from(vec![
                Span::styled(
                    format!("{fullscreen_hint} "),
                    Style::new().fg(theme.text.to_color()),
                ),
                Span::styled(
                    if app.ui.permission.expanded {
                        "minimize"
                    } else {
                        "fullscreen"
                    },
                    Style::new().fg(theme.text_muted.to_color()),
                ),
                Span::styled("   ⇆ ", Style::new().fg(theme.text.to_color())),
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
                        "  This will allow {} until Alforria is restarted.",
                        request.permission
                    ),
                    Style::new().fg(theme.text_muted.to_color()),
                ));
            } else {
                rows.push(Line::styled(
                    "  This will allow the following patterns until Alforria is restarted",
                    Style::new().fg(theme.text_muted.to_color()),
                ));
                // `gap={1}` between the heading and the pattern list
                // (`permission.tsx:147`).
                rows.push(Line::raw(""));
                for pattern in &request.always {
                    rows.push(Line::styled(
                        format!("  - {pattern}"),
                        Style::new().fg(theme.text.to_color()),
                    ));
                }
            }
            let labels = ["Confirm", "Cancel"];
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
                "  Tell Alforria what to do differently",
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

/// The `(start, width)` of every button span of a row — the only
/// spans carrying a background colour (`option_row` and the
/// Confirm/Cancel row).
fn button_ranges(line: &Line<'_>) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut x = 0usize;
    for span in &line.spans {
        let width = span.content.chars().count();
        if span.style.bg.is_some() {
            ranges.push((x, width));
        }
        x += width;
    }
    ranges
}

/// Render into the prompt slot.
pub fn render(app: &mut App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    let rows = lines(app);
    let border = match app.ui.permission.stage {
        PermissionStage::Reject => theme.error,
        _ => theme.warning,
    };
    // The option buttons are mouse-interactive (`permission.tsx:676-693`)
    // — record their screen geometry for hit-testing. Content starts at
    // `area.x + 2` (left border + padding) / `area.y + 1` (top padding).
    let mut clicks = Vec::new();
    for (index, line) in rows.iter().enumerate() {
        let buttons = button_ranges(line);
        if buttons.is_empty() {
            continue;
        }
        clicks.push(ClickRow {
            row: area.y + 1 + index as u16,
            buttons: buttons
                .into_iter()
                .map(|(x, width)| (area.x + 2 + x as u16, width as u16))
                .collect(),
        });
    }
    app.ui.permission.clicks = clicks;
    Paragraph::new(rows)
        .block(
            Block::new()
                .borders(ratatui::widgets::Borders::LEFT)
                .border_set(ratatui::symbols::border::Set {
                    vertical_left: "┃",
                    ..Default::default()
                })
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

/// The option index at the position, if the last render placed a
/// button there.
fn hit(app: &App, column: u16, row: u16) -> Option<usize> {
    visible(app)?;
    let click = app.ui.permission.clicks.first()?;
    if row != click.row {
        return None;
    }
    click
        .buttons
        .iter()
        .position(|(x, width)| column >= *x && column < *x + *width)
}

/// `onMouseOver` (`permission.tsx:685`) — hover moves the selection to
/// the button under the pointer.
pub fn mouse_over(app: &mut App, column: u16, row: u16) {
    if let Some(selected) = hit(app, column, row) {
        app.ui.permission.selected = selected;
    }
}

/// `onMouseUp` (`permission.tsx:686-690`) — release selects and
/// activates the button under the pointer.
pub fn mouse_select(app: &mut App, column: u16, row: u16) -> Option<Vec<Effect>> {
    let selected = hit(app, column, row)?;
    app.ui.permission.selected = selected;
    let request = visible(app)?;
    let session_parent = app
        .state
        .sync
        .session(&request.session_id)
        .and_then(|session| session.parent_id.clone());
    Some(select(app, &request, session_parent))
}
