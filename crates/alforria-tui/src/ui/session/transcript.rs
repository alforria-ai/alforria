//! `routes/session/index.tsx:1198-1294` — the transcript scrollbox
//! (M8.5): messages with revert marker, user/assistant rendering, and
//! the sticky-bottom scroll.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use serde_json::Value;

use alforria_schema::session_v1::{AssistantError, V1Message, V1Part};

use super::parts;
use crate::state::{kv, App};
use crate::ui::theme::{Rgba, Theme};

use super::Ctx;

/// `SplitBorder.customBorderChars.vertical` (`ui/border.ts`).
const SPLIT_VERTICAL: &str = "┃";

fn blank() -> Line<'static> {
    Line::from("")
}

fn styled(text: impl Into<String>, color: Rgba) -> Span<'static> {
    Span::styled(text.into(), Style::new().fg(color.to_color()))
}

fn message_id(message: &V1Message) -> &str {
    match message {
        V1Message::User { id, .. } | V1Message::Assistant { id, .. } => id,
    }
}

/// `pending()` (`session/index.tsx:243-249`): the last running assistant
/// message index after the last completed one.
fn pending_index(messages: &[V1Message]) -> Option<usize> {
    let completed = messages
        .iter()
        .rposition(|message| {
            matches!(
                message,
                V1Message::Assistant { time, .. } if time.completed.is_some()
            )
        })
        .map(|index| index as i64)
        .unwrap_or(-1);
    messages.iter().enumerate().rposition(|(index, message)| {
        (index as i64) > completed
            && matches!(
                message,
                V1Message::Assistant { time, .. } if time.completed.is_none()
            )
    })
}

/// `Model.name()` (`util/model.ts:22-28`) — provider model display name
/// or the modelID fallback.
fn model_name(app: &App, provider_id: &str, model_id: &str) -> String {
    app.state
        .sync
        .provider
        .iter()
        .find(|provider| provider.get("id").and_then(Value::as_str) == Some(provider_id))
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
        .and_then(|models| models.get(model_id))
        .and_then(|model| model.get("name"))
        .and_then(Value::as_str)
        .unwrap_or(model_id)
        .to_string()
}

/// `formatKeySequence` (`keymap.tsx:250-258`) — the first registered
/// binding of a command, formatted for display. The TS formats through
/// the OpenTUI keymap registry; the port formats the resolved binding
/// directly (documented divergence — shape only).
fn command_shortcut(app: &App, command: &str) -> String {
    use crate::keymap::bindings::BindingValue;
    let Some(BindingValue::Alternatives(alternatives)) = app.keymap.bindings.get(command) else {
        return String::new();
    };
    let Some(sequence) = alternatives.first() else {
        return String::new();
    };
    sequence
        .iter()
        .map(|stroke| {
            let mut parts: Vec<String> = Vec::new();
            if stroke.ctrl {
                parts.push("ctrl".to_string());
            }
            if stroke.meta {
                parts.push("meta".to_string());
            }
            if stroke.super_key {
                parts.push("super".to_string());
            }
            if stroke.hyper {
                parts.push("hyper".to_string());
            }
            let single = stroke.key.chars().count() == 1;
            if stroke.shift {
                if single && stroke.key.chars().all(|c| c.is_ascii_lowercase()) {
                    parts.push(stroke.key.to_ascii_uppercase());
                    return parts.join("+");
                }
                parts.push("shift".to_string());
            }
            parts.push(stroke.key.clone());
            parts.join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The transcript entry point — the `<scrollbox>` contents
/// (`session/index.tsx:1198-1294`) plus the memoized scroll geometry.
pub fn render(
    app: &mut App,
    frame: &mut ratatui::Frame,
    theme: &Theme,
    area: Rect,
    session_id: &str,
    content_width: u16,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let messages = app
        .state
        .sync
        .message
        .get(session_id)
        .cloned()
        .unwrap_or_default();
    let session = app.state.sync.session(session_id).cloned();
    let pending = pending_index(&messages);

    // `revert()` + `revertMessageIndex()` (`session/index.tsx:1122-1150`).
    let revert = session
        .as_ref()
        .and_then(|session| session.revert.clone())
        .filter(|revert| !revert.message_id.is_empty());
    let revert_index = revert.as_ref().and_then(|revert| {
        messages
            .iter()
            .position(|message| message_id(message) == revert.message_id)
    });

    let mut clicks = Vec::new();
    let (lines, children) = build_lines(
        app,
        theme,
        &messages,
        session_id,
        content_width,
        revert.as_ref(),
        revert_index,
        pending,
        &mut clicks,
    );

    super::memo_scroll(
        &mut app.ui.session_scroll,
        session_id,
        lines.len(),
        area.height as usize,
        children,
    );
    app.ui.session_scroll.clicks = clicks;
    app.ui.session_scroll.area_y = area.y;

    let top = app.ui.session_scroll.effective_y();
    let visible: Vec<Line<'static>> = lines
        .iter()
        .skip(top)
        .take(area.height as usize)
        .cloned()
        .collect();
    Paragraph::new(visible).render(area, frame.buffer_mut());
}

#[allow(clippy::too_many_arguments)]
fn build_lines(
    app: &App,
    theme: &Theme,
    messages: &[V1Message],
    session_id: &str,
    content_width: u16,
    revert: Option<&alforria_schema::session_v1::V1SessionRevert>,
    revert_index: Option<usize>,
    pending: Option<usize>,
    clicks: &mut Vec<crate::state::ClickTarget>,
) -> (Vec<Line<'static>>, Vec<(String, usize)>) {
    let ctx = Ctx::new(app, theme, session_id, content_width);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut children: Vec<(String, usize)> = Vec::new();

    // `<box height={1} />` (`session/index.tsx:1198`).
    lines.push(blank());

    for (index, message) in messages.iter().enumerate() {
        // The revert marker replaces the reverted message; everything
        // from the revert point on is hidden (`:1200-1266`).
        if Some(index) == revert_index {
            if let (Some(revert), Some(revert_index)) = (revert, revert_index) {
                lines.extend(revert_marker_lines(
                    app,
                    theme,
                    messages,
                    revert,
                    revert_index,
                ));
            }
            break;
        }
        let parts = app
            .state
            .sync
            .part
            .get(message_id(message))
            .cloned()
            .unwrap_or_default();
        match message {
            V1Message::User { .. } => {
                user_message_lines(
                    &ctx,
                    theme,
                    message,
                    &parts,
                    index,
                    pending,
                    content_width,
                    &mut lines,
                    &mut children,
                );
            }
            V1Message::Assistant { .. } => {
                assistant_message_lines(
                    app, &ctx, theme, messages, message, &parts, &mut lines, clicks,
                );
            }
        }
    }

    (lines, children)
}

// --------------------------------------------------------------- revert

/// The revert marker box (`session/index.tsx:1202-1261`).
fn revert_marker_lines(
    app: &App,
    theme: &Theme,
    messages: &[V1Message],
    revert: &alforria_schema::session_v1::V1SessionRevert,
    revert_index: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    // marginTop={1}
    lines.push(blank());

    // `reverted` — the user messages from the revert point on
    // (`session/index.tsx:1132-1140`).
    let reverted = messages
        .iter()
        .skip(revert_index)
        .filter(|message| matches!(message, V1Message::User { .. }))
        .count();
    let redo = command_shortcut(app, "session.redo");

    let mut rows: Vec<Line<'static>> = Vec::new();
    // paddingTop={1} / paddingBottom={1}.
    rows.push(blank());
    rows.push(Line::from(styled(
        format!("{reverted} message reverted"),
        theme.text_muted,
    )));
    rows.push(Line::from(vec![
        styled(redo, theme.text),
        Span::styled(
            " or /redo to restore",
            Style::new().fg(theme.text_muted.to_color()),
        ),
    ]));
    // The per-file ±N rows (`:1240-1256`).
    let files = crate::ui::diff::revert_files(revert.diff.as_deref().unwrap_or(""));
    if !files.is_empty() {
        rows.push(blank());
        for file in files {
            let mut spans = vec![styled(file.filename.clone(), theme.text)];
            if file.additions > 0 {
                spans.push(styled(format!(" +{}", file.additions), theme.diff_added));
            }
            if file.deletions > 0 {
                spans.push(styled(format!(" -{}", file.deletions), theme.diff_removed));
            }
            rows.push(Line::from(spans));
        }
    }
    rows.push(blank());
    for mut row in rows {
        row.spans.insert(0, Span::raw("  "));
        row.spans.insert(
            0,
            Span::styled(
                SPLIT_VERTICAL,
                Style::new().fg(theme.background_panel.to_color()),
            ),
        );
        lines.push(row);
    }
    lines
}

// ----------------------------------------------------------------- user

/// `UserMessage` (`session/index.tsx:1364-1467`).
#[allow(clippy::too_many_arguments)]
fn user_message_lines(
    ctx: &Ctx,
    theme: &Theme,
    message: &V1Message,
    parts: &[V1Part],
    index: usize,
    pending: Option<usize>,
    content_width: u16,
    lines: &mut Vec<Line<'static>>,
    children: &mut Vec<(String, usize)>,
) {
    let V1Message::User {
        id, agent, time, ..
    } = message
    else {
        return;
    };

    // Non-synthetic text parts joined with a blank line
    // (`session/index.tsx:1373-1383`).
    let text = parts
        .iter()
        .filter_map(|part| match part {
            V1Part::Text {
                text, synthetic, ..
            } if !synthetic.unwrap_or(false) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let files: Vec<&V1Part> = parts
        .iter()
        .filter(|part| matches!(part, V1Part::File { .. }))
        .collect();

    if !text.is_empty() {
        if index != 0 {
            lines.push(blank());
        }
        children.push((id.clone(), lines.len()));
        let color = ctx.agent_color(agent);
        let border = Style::new().fg(color.to_color());

        let mut rows: Vec<Line<'static>> = Vec::new();
        // paddingTop={1}
        rows.push(blank());
        for row in text.split('\n') {
            rows.push(Line::from(styled(row.to_string(), theme.text)));
        }
        // QUEUED beyond the pending index, else the timestamp (`:1437-1452`).
        let queued = pending.is_some() && index > pending.unwrap();
        let timestamps = ctx
            .app
            .state
            .kv
            .get(kv::keys::TIMESTAMPS, serde_json::json!("hide"))
            .as_str()
            == Some("show");
        // The file chips (`:1420-1436`) — `paddingBottom` only when the
        // metadata row below will render.
        if !files.is_empty() {
            rows.push(blank());
            let mut chips: Vec<Span> = Vec::new();
            for (position, file) in files.iter().enumerate() {
                if position > 0 {
                    chips.push(Span::raw(" "));
                }
                let (directory, filename) = match file {
                    V1Part::File { mime, filename, .. } => (
                        mime == "application/x-directory",
                        filename.clone().unwrap_or_default(),
                    ),
                    _ => unreachable!(),
                };
                chips.push(Span::styled(
                    if directory { " Directory " } else { " File " },
                    Style::new()
                        .bg(theme.secondary.to_color())
                        .fg(theme.background.to_color()),
                ));
                chips.push(Span::styled(
                    format!(" {filename} "),
                    Style::new()
                        .bg(theme.background_element.to_color())
                        .fg(theme.text_muted.to_color()),
                ));
            }
            rows.push(Line::from(chips));
            if queued || timestamps {
                rows.push(blank());
            }
        }
        if queued {
            rows.push(Line::from(Span::styled(
                " QUEUED ",
                Style::new()
                    .bg(color.to_color())
                    .fg(ctx.selected_foreground(color).to_color())
                    .add_modifier(ratatui::style::Modifier::BOLD),
            )));
        } else if timestamps {
            rows.push(Line::from(styled(
                crate::ui::locale::today_time_or_date_time(time.created as i64),
                theme.text_muted,
            )));
        }
        // paddingBottom={1}
        rows.push(blank());
        for mut row in rows {
            row.spans.insert(0, Span::raw("  "));
            row.spans.insert(0, Span::styled(SPLIT_VERTICAL, border));
            lines.push(row);
        }
    }

    // The compaction separator (`:1456-1464`).
    if parts
        .iter()
        .any(|part| matches!(part, V1Part::Compaction { .. }))
    {
        lines.push(blank());
        let title = " Compaction ";
        let dashes = (content_width as usize).saturating_sub(title.len());
        let left = dashes / 2;
        let right = dashes - left;
        lines.push(Line::from(Span::styled(
            format!("{}{}{}", "─".repeat(left), title, "─".repeat(right)),
            Style::new().fg(theme.border_active.to_color()),
        )));
    }
}

// ------------------------------------------------------------ assistant

/// `AssistantMessage` (`session/index.tsx:1469-1576`).
#[allow(clippy::too_many_arguments)]
fn assistant_message_lines(
    app: &App,
    ctx: &Ctx,
    theme: &Theme,
    messages: &[V1Message],
    message: &V1Message,
    parts: &[V1Part],
    lines: &mut Vec<Line<'static>>,
    clicks: &mut Vec<crate::state::ClickTarget>,
) {
    let V1Message::Assistant {
        agent,
        mode,
        parent_id,
        provider_id,
        model_id,
        time,
        error,
        finish,
        ..
    } = message
    else {
        return;
    };
    // A completed/errored message is done — its parts must not animate.
    let ctx = Ctx {
        app: ctx.app,
        theme: ctx.theme,
        width: ctx.width,
        session_id: ctx.session_id,
        message_done: time.completed.is_some() || error.is_some(),
    };

    for part in parts {
        // The clickable part ranges (BlockTool/InlineTool/ReasoningPart
        // onClick, session/index.tsx:1822,1900,1609).
        let start = lines.len();
        lines.extend(parts::render_part(part, &ctx));
        let id = match part {
            V1Part::Reasoning { id, .. } | V1Part::Tool { id, .. } => Some(id.clone()),
            _ => None,
        };
        if let Some(id) = id {
            clicks.push(crate::state::ClickTarget {
                id,
                start,
                end: lines.len(),
            });
        }
    }

    // The subagent hint row (`:1509-1532`).
    let has_task = parts
        .iter()
        .any(|part| matches!(part, V1Part::Tool { tool, state, .. } if tool == "task"));
    if has_task {
        lines.push(blank());
        let mut spans = vec![styled(
            command_shortcut(app, "session.child.first"),
            theme.text,
        )];
        spans.push(styled(" view subagents", theme.text_muted));
        let running_foreground = parts.iter().any(|part| {
            matches!(part, V1Part::Tool { tool, state, .. }
                if tool == "task"
                    && matches!(state, alforria_schema::session_v1::V1ToolState::Running { metadata, .. }
                        if metadata.as_ref().and_then(|m| m.get("background")).and_then(Value::as_bool) != Some(true)))
        });
        if app.state.sync.capabilities && running_foreground {
            spans.push(styled(" · ", theme.text_muted));
            spans.push(styled(
                command_shortcut(app, "session.background"),
                theme.text,
            ));
            spans.push(styled(" background", theme.text_muted));
        }
        lines.push(Line::from(spans));
    }

    // The error box (`:1533-1547`).
    let aborted = matches!(error, Some(AssistantError::Aborted { .. }));
    if let Some(error) = error {
        if !aborted {
            lines.push(blank());
            let rows = parts::assistant_error_message(error);
            for row in rows.split('\n') {
                lines.push(Line::from(vec![
                    Span::styled(SPLIT_VERTICAL, Style::new().fg(theme.error.to_color())),
                    Span::raw("  "),
                    styled(row.to_string(), theme.text_muted),
                ]));
            }
            lines.push(Line::from(Span::styled(
                SPLIT_VERTICAL,
                Style::new().fg(theme.error.to_color()),
            )));
        }
    }

    // The footer row (`:1548-1573`).
    let final_ = match finish {
        Some(finish) => !["tool-calls", "unknown"].contains(&finish.as_str()),
        None => false,
    };
    let last = app
        .state
        .sync
        .message
        .get(ctx.session_id)
        .and_then(|messages| messages.last())
        .map(|last| message_id(last) == message_id(message))
        .unwrap_or(false);
    let duration = if !final_ {
        0
    } else {
        match time.completed {
            Some(completed) => messages
                .iter()
                .find_map(|message| match message {
                    V1Message::User { id, time, .. } if id == parent_id => {
                        Some((completed as f64 - time.created).max(0.0))
                    }
                    _ => None,
                })
                .unwrap_or(0.0) as i64,
            None => 0,
        }
    };
    if last || final_ || aborted {
        lines.push(blank());
        let mut spans = vec![styled(
            "▣ ",
            if aborted {
                theme.text_muted
            } else {
                ctx.agent_color(agent)
            },
        )];
        spans.push(styled(crate::ui::locale::titlecase(mode), theme.text));
        spans.push(styled(
            format!(" · {}", model_name(app, provider_id, model_id)),
            theme.text_muted,
        ));
        if duration > 0 {
            spans.push(styled(
                format!(" · {}", crate::ui::locale::duration(duration)),
                theme.text_muted,
            ));
        }
        if aborted {
            spans.push(styled(" · interrupted", theme.text_muted));
        }
        lines.push(Line::from(spans));
    }
}

#[cfg(test)]
mod tests {
    use alforria_schema::session_v1::{
        AssistantTime, UserTime, V1Message, V1Path, V1SessionInfo, V1SessionTime, V1StepTokens,
        V1UserModel,
    };

    use super::*;
    fn user_message(id: &str, created: f64) -> V1Message {
        V1Message::User {
            id: id.to_string(),
            session_id: "ses_a".to_string(),
            time: UserTime { created },
            format: None,
            summary: None,
            agent: "build".to_string(),
            model: V1UserModel {
                provider_id: "anthropic".to_string(),
                model_id: "claude".to_string(),
                variant: None,
            },
            system: None,
            tools: None,
        }
    }

    fn assistant_message(id: &str, completed: Option<u64>) -> V1Message {
        V1Message::Assistant {
            id: id.to_string(),
            session_id: "ses_a".to_string(),
            time: AssistantTime {
                created: 1,
                completed,
            },
            error: None,
            parent_id: "msg_parent".to_string(),
            model_id: "claude".to_string(),
            provider_id: "anthropic".to_string(),
            mode: "primary".to_string(),
            agent: "build".to_string(),
            path: V1Path {
                cwd: "/repo".to_string(),
                root: "/repo".to_string(),
            },
            summary: None,
            cost: 0.0,
            tokens: V1StepTokens {
                total: None,
                input: 0.0,
                output: 0.0,
                reasoning: 0.0,
                cache: alforria_schema::session_v1::V1TokenCache {
                    read: 0.0,
                    write: 0.0,
                },
            },
            structured: None,
            variant: None,
            finish: None,
        }
    }

    fn session_info(revert: Option<alforria_schema::session_v1::V1SessionRevert>) -> V1SessionInfo {
        V1SessionInfo {
            id: "ses_a".to_string(),
            slug: "x".to_string(),
            project_id: "prj".to_string(),
            workspace_id: None,
            directory: "/repo".to_string(),
            path: None,
            parent_id: None,
            summary: None,
            cost: None,
            tokens: None,
            share: None,
            title: "X".to_string(),
            agent: None,
            model: None,
            version: "1".to_string(),
            metadata: None,
            time: V1SessionTime {
                created: 1,
                updated: 1,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert,
        }
    }

    fn app_with_messages(messages: Vec<V1Message>) -> App {
        let mut app = App::new(
            crate::config::TuiConfig::default(),
            crate::state::Args::default(),
            None,
        );
        app.state.sync.message.insert("ses_a".to_string(), messages);
        app.state.sync.session = vec![session_info(None)];
        app
    }

    fn render_lines(app: &mut App, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let theme = app
            .ui
            .theme
            .resolve(&app.state.kv)
            .expect("builtin theme resolves");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render(app, frame, &theme, area, "ses_a", width - 4)
            })
            .unwrap();
        let mut text = String::new();
        for y in 0..height {
            for x in 0..width {
                text.push_str(terminal.backend().buffer()[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn user_message_renders_text() {
        let mut app = app_with_messages(vec![user_message("msg_1", 1.0)]);
        app.state.sync.part.insert(
            "msg_1".to_string(),
            vec![V1Part::Text {
                id: "prt_1".to_string(),
                session_id: "ses_a".to_string(),
                message_id: "msg_1".to_string(),
                text: "hello transcript".to_string(),
                synthetic: None,
                ignored: None,
                time: None,
                metadata: None,
            }],
        );
        let text = render_lines(&mut app, 80, 24);
        assert!(text.contains("hello transcript"), "{text}");
    }

    #[test]
    fn queued_tag_renders_beyond_pending_index() {
        // [user, running assistant, user] — the trailing user message
        // is queued (`session/index.tsx:1387-1388`).
        let mut app = app_with_messages(vec![
            user_message("msg_1", 1.0),
            assistant_message("msg_2", None),
            user_message("msg_3", 2.0),
        ]);
        for id in ["msg_1", "msg_3"] {
            app.state.sync.part.insert(
                id.to_string(),
                vec![V1Part::Text {
                    id: format!("prt_{id}"),
                    session_id: "ses_a".to_string(),
                    message_id: id.to_string(),
                    text: format!("text {id}"),
                    synthetic: None,
                    ignored: None,
                    time: None,
                    metadata: None,
                }],
            );
        }
        let text = render_lines(&mut app, 80, 24);
        assert!(text.contains("QUEUED"), "{text}");
    }

    #[test]
    fn revert_marker_renders_and_hides_tail() {
        let revert = alforria_schema::session_v1::V1SessionRevert {
            message_id: "msg_1".to_string(),
            part_id: None,
            snapshot: None,
            diff: Some("--- a/x.ts\n+++ b/x.ts\n@@\n+added\n-removed\n".to_string()),
        };
        let mut app =
            app_with_messages(vec![user_message("msg_1", 1.0), user_message("msg_2", 2.0)]);
        app.state.sync.session = vec![session_info(Some(revert))];
        for id in ["msg_1", "msg_2"] {
            app.state.sync.part.insert(
                id.to_string(),
                vec![V1Part::Text {
                    id: format!("prt_{id}"),
                    session_id: "ses_a".to_string(),
                    message_id: id.to_string(),
                    text: format!("text {id}"),
                    synthetic: None,
                    ignored: None,
                    time: None,
                    metadata: None,
                }],
            );
        }
        let text = render_lines(&mut app, 80, 24);
        // Both messages from the revert point are users → 2 reverted.
        assert!(text.contains("2 message reverted"), "{text}");
        assert!(text.contains("or /redo to restore"), "{text}");
        assert!(text.contains("x.ts +1"), "{text}");
        // Messages at/after the revert point are hidden.
        assert!(!text.contains("text msg_1"), "{text}");
    }
}
