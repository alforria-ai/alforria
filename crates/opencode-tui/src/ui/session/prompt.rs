//! `component/prompt/index.tsx` rendering (M8.6): the textarea frame
//! (`1352-1401`), the meta row, and the autocomplete popup
//! (`autocomplete.tsx`).

use crate::state::route::Route;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph, Widget};

use super::super::theme::Theme;
use crate::state::route::PromptMode;
use crate::state::App;

/// The autocomplete popup `maxHeight` (`autocomplete.tsx`).
const AUTOCOMPLETE_HEIGHT: usize = 8;

/// The inner textarea width: the frame pads left+right by 2.
fn inner_width(area: Rect) -> u16 {
    area.width.saturating_sub(4).max(1)
}

/// The wrapped textarea rows (`Textarea::display`).
fn display(app: &App, area: Rect) -> crate::ui::textarea::Display {
    app.ui.prompt.textarea.display(inner_width(area))
}

/// The rendered prompt row count: `paddingTop={1}` + the visible
/// textarea rows + the meta row.
pub fn height(app: &App, area: Rect, terminal_height: u16) -> u16 {
    let max_height = crate::ui::textarea::Textarea::max_height(None, terminal_height);
    let rows = display(app, area).rows.len() as u16;
    let busy = status_row_visible(app) as u16;
    1 + rows.min(max_height) + 1 + busy
}

/// `placeholderText` (`prompt/index.tsx:1311-1319`).
fn placeholder_text(app: &App) -> String {
    let roll = app.ui.prompt.placeholder as usize % crate::ui::home::PLACEHOLDER_NORMAL.len();
    if app.ui.prompt.mode == PromptMode::Shell {
        format!("{}…", crate::ui::home::PLACEHOLDER_SHELL[roll])
    } else {
        format!(
            "Ask anything… \"{}\"",
            crate::ui::home::PLACEHOLDER_NORMAL[roll]
        )
    }
}

/// The meta row under the textarea: agent · model (`prompt/index.tsx`
/// `Keybinds` row — the muted hint line).
fn meta_line<'a>(app: &'a App, theme: &Theme) -> Line<'a> {
    let muted = theme.text_muted.to_color();
    let mut spans: Vec<Span<'_>> = Vec::new();
    if let Some(agent) = app
        .state
        .local
        .agent_current(&app.state.sync)
        .and_then(|a| a.get("name").and_then(|v| v.as_str()))
    {
        spans.push(Span::styled(agent.to_string(), theme.text.to_color()));
    }
    if let Some(model) = app
        .state
        .local
        .model_current(&app.state.sync, &app.state.args)
    {
        spans.push(Span::styled("  ·  ", muted));
        spans.push(Span::styled(model.key(), muted));
    }
    if app.ui.prompt.submitting {
        // `move.progress() === "Submitting prompt"` with animated dots
        // (`move.tsx:174-177`).
        spans.push(Span::styled("  ·  ", muted));
        spans.push(Span::styled(
            format!("{} Submitting prompt", app.session_spinner()),
            theme.text.to_color(),
        ));
    }
    if app.ui.prompt.mode == PromptMode::Shell {
        spans.push(Span::styled("  ·  ", muted));
        spans.push(Span::styled("shell", theme.warning.to_color()));
    }
    Line::from(spans)
}

/// `formatDuration` (`util/format.ts:1-18`).
fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let mins = secs / 60;
        let remaining = secs % 60;
        if remaining > 0 {
            format!("{mins}m {remaining}s")
        } else {
            format!("{mins}m")
        }
    } else if secs < 86_400 {
        let hours = secs / 3600;
        let remaining = (secs % 3600) / 60;
        if remaining > 0 {
            format!("{hours}h {remaining}m")
        } else {
            format!("{hours}h")
        }
    } else if secs < 604_800 {
        let days = secs / 86_400;
        if days == 1 {
            "~1 day".to_string()
        } else {
            format!("~{days} days")
        }
    } else {
        let weeks = secs / 604_800;
        if weeks == 1 {
            "~1 week".to_string()
        } else {
            format!("~{weeks} weeks")
        }
    }
}

/// Whether the busy bottom row is showing for the current session.
fn status_row_visible(app: &App) -> bool {
    let Route::Session { session_id, .. } = &app.state.route.data else {
        return false;
    };
    !matches!(
        app.state.sync.session_status.get(session_id),
        None | Some(opencode_schema::session_status::SessionStatusInfo::Idle)
    )
}

/// The busy bottom row (`prompt/index.tsx:1515-1596`): a spinner, the
/// retry message + countdown, and the `esc interrupt` hint — shown
/// while the session status is not idle.
fn status_row(app: &App, theme: &Theme) -> Option<Line<'static>> {
    let Route::Session { session_id, .. } = &app.state.route.data else {
        return None;
    };
    let retry = app
        .state
        .sync
        .session_status
        .get(session_id)
        .and_then(|status| {
            match status {
                opencode_schema::session_status::SessionStatusInfo::Idle => None,
                retry @ opencode_schema::session_status::SessionStatusInfo::Retry { .. } => {
                    Some(Some(retry.clone()))
                }
                _ => Some(None), // Busy — spinner + interrupt hint only.
            }
        })?;
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::styled(
        format!("{} ", app.session_spinner()),
        theme.text.to_color(),
    ));
    if let Some(opencode_schema::session_status::SessionStatusInfo::Retry {
        attempt,
        message,
        next,
        ..
    }) = &retry
    {
        // `message()` (`prompt/index.tsx:1545-1554`).
        let message =
            if message.contains("exceeded your current quota") && message.contains("gemini") {
                "gemini is way too hot right now".to_string()
            } else if message.len() > 80 {
                format!("{}…", &message[..80])
            } else {
                message.clone()
            };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let seconds = next.saturating_sub(now_ms) / 1000;
        let duration = format_duration(seconds);
        let retry_info = if duration.is_empty() {
            format!(" [retrying attempt #{attempt}]")
        } else {
            format!(" [retrying in {duration} attempt #{attempt}]")
        };
        spans.push(Span::styled(
            format!("{message}{retry_info}"),
            theme.error.to_color(),
        ));
        spans.push(Span::raw(" "));
    }
    // `esc interrupt` / `esc again to interrupt` — primary once the
    // double-press is armed (`store.interrupt`).
    let armed = app.ui.interrupt > 0;
    spans.push(Span::styled(
        "esc ",
        if armed {
            theme.primary.to_color()
        } else {
            theme.text.to_color()
        },
    ));
    spans.push(Span::styled(
        if armed {
            "again to interrupt"
        } else {
            "interrupt"
        },
        if armed {
            theme.primary.to_color()
        } else {
            theme.text_muted.to_color()
        },
    ));
    Some(Line::from(spans))
}

/// The prompt frame + textarea + meta row (`prompt/index.tsx:1352-1401`).
pub fn render(app: &App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    let display = display(app, area);
    let max_height = crate::ui::textarea::Textarea::max_height(None, area.height.max(1));
    let rows: Vec<&Vec<(char, Option<u64>)>> =
        display.rows.iter().take(max_height as usize).collect();
    let text_empty = app.ui.prompt.is_empty();
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));
    if text_empty {
        lines.push(Line::from(Span::styled(
            placeholder_text(app),
            theme.text_muted.to_color(),
        )));
    } else {
        for (row, cells) in rows.iter().enumerate() {
            let mut spans: Vec<Span> = Vec::new();
            for (column, (char, _mark)) in cells.iter().enumerate() {
                let mut style = Style::new().fg(theme.text.to_color());
                if row == display.cursor_row && column == display.cursor_col {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                spans.push(Span::styled(char.to_string(), style));
            }
            if row == display.cursor_row && spans.is_empty() {
                spans.push(Span::styled(
                    " ",
                    Style::new().add_modifier(Modifier::REVERSED),
                ));
            }
            lines.push(Line::from(spans));
        }
    }
    lines.push(meta_line(app, theme));
    if let Some(row) = status_row(app, theme) {
        lines.push(row);
    }

    let mut text = ratatui::text::Text::from(lines);
    if text.height() as u16 > area.height {
        text = ratatui::text::Text::from(
            text.lines
                .into_iter()
                .take(area.height as usize)
                .collect::<Vec<Line>>(),
        );
    }
    Paragraph::new(text)
        .block(
            Block::new()
                .borders(ratatui::widgets::Borders::LEFT)
                .border_style(theme.border.to_color())
                .style(Style::new().bg(theme.background_element.to_color()))
                .padding(Padding {
                    left: 2,
                    right: 2,
                    top: 0,
                    bottom: 0,
                }),
        )
        .render(area, frame.buffer_mut());

    if app.ui.prompt.autocomplete.visible.is_some() {
        render_autocomplete(app, frame, theme, area);
    }
}

pub(crate) fn render_autocomplete(
    app: &App,
    frame: &mut ratatui::Frame,
    theme: &Theme,
    area: Rect,
) {
    let autocomplete = &app.ui.prompt.autocomplete;
    let count = autocomplete
        .options
        .len()
        .min(AUTOCOMPLETE_HEIGHT)
        .min(area.y as usize);
    if count == 0 {
        return;
    }
    let popup = Rect {
        x: area.x + 1,
        y: area.y - count as u16,
        width: area.width.saturating_sub(1),
        height: count as u16,
    };
    let lines: Vec<Line> = autocomplete
        .options
        .iter()
        .take(count)
        .enumerate()
        .map(|(index, option)| {
            let selected = index == autocomplete.selected;
            let (fg, bg) = if selected {
                (theme.text, theme.background_element)
            } else {
                (theme.text_muted, theme.background_menu)
            };
            Line::from(Span::styled(
                format!(" {} ", option.display),
                Style::new()
                    .fg(fg.to_color())
                    .bg(bg.to_color())
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ))
        })
        .collect();
    Paragraph::new(lines).render(popup, frame.buffer_mut());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::App;

    fn theme() -> crate::ui::theme::Theme {
        let app = App::new(
            crate::config::TuiConfig::default(),
            Default::default(),
            None,
        );
        app.ui
            .theme
            .resolve(&app.state.kv)
            .expect("builtin theme resolves")
    }

    fn app_busy() -> App {
        let mut app = App::new(
            crate::config::TuiConfig::default(),
            Default::default(),
            None,
        );
        app.state.route.data = Route::Session {
            session_id: "ses_1".to_string(),
            prompt: None,
        };
        app.state.sync.session_status.insert(
            "ses_1".to_string(),
            opencode_schema::session_status::SessionStatusInfo::Busy,
        );
        app
    }

    #[test]
    fn busy_session_shows_interrupt_hint() {
        let app = app_busy();
        let row = status_row(&app, &theme()).unwrap();
        let text = row
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.contains("esc interrupt"), "hint missing: {text}");
        // Armed double-press flips to the "again" hint.
        let mut app = app_busy();
        app.ui.interrupt = 1;
        let row = status_row(&app, &theme()).unwrap();
        let text = row
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(
            text.contains("esc again to interrupt"),
            "hint missing: {text}"
        );
    }

    #[test]
    fn retry_status_shows_message_and_attempt() {
        let mut app = app_busy();
        app.state.sync.session_status.insert(
            "ses_1".to_string(),
            opencode_schema::session_status::SessionStatusInfo::Retry {
                attempt: 2,
                message: "rate limited".to_string(),
                action: None,
                next: 0,
            },
        );
        let row = status_row(&app, &theme()).unwrap();
        let text = row
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.contains("rate limited"), "message missing: {text}");
        assert!(text.contains("attempt #2"), "attempt missing: {text}");
    }

    #[test]
    fn idle_session_hides_interrupt_row() {
        let mut app = app_busy();
        app.state.sync.session_status.insert(
            "ses_1".to_string(),
            opencode_schema::session_status::SessionStatusInfo::Idle,
        );
        assert!(status_row(&app, &theme()).is_none());
    }
}
