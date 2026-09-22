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

/// The autocomplete popup `maxHeight` (`autocomplete.tsx:712-716`).
pub(crate) const AUTOCOMPLETE_HEIGHT: usize = 10;

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
    let max_height =
        crate::ui::textarea::Textarea::max_height(app.config.prompt_max_height, terminal_height);
    let rows = display(app, area).rows.len() as u16;
    let busy = status_row_visible(app) as u16;
    1 + rows.min(max_height) + 1 + busy
}

/// `placeholderText` (`prompt/index.tsx:1311-1319`).
fn placeholder_text(app: &App) -> String {
    let roll = app.ui.prompt.placeholder as usize % crate::ui::home::PLACEHOLDER_NORMAL.len();
    if app.ui.prompt.mode == PromptMode::Shell {
        format!(
            "Run a command… \"{}\"",
            crate::ui::home::PLACEHOLDER_SHELL[roll]
        )
    } else {
        format!(
            "Ask anything… \"{}\"",
            crate::ui::home::PLACEHOLDER_NORMAL[roll]
        )
    }
}

/// The meta row under the textarea (`prompt/index.tsx:1445-1484`):
/// `space-between` — agent · model · provider · variant on the left,
/// the `agents`/`commands` shortcut hints (or `esc exit shell mode`) on
/// the right.
fn meta_line(app: &App, theme: &Theme, width: u16) -> Line<'static> {
    let muted = theme.text_muted.to_color();
    let shell = app.ui.prompt.mode == PromptMode::Shell;
    let mut left: Vec<Span<'static>> = Vec::new();
    if let Some(agent) = app
        .state
        .local
        .agent_current(&app.state.sync)
        .and_then(|a| a.get("name").and_then(|v| v.as_str()))
    {
        left.push(Span::styled(
            if shell {
                "Shell".to_string()
            } else {
                crate::ui::locale::titlecase(agent)
            },
            theme.text.to_color(),
        ));
        if !shell && app.state.permission_mode == crate::state::PermissionMode::Auto {
            left.push(Span::styled("auto".to_string(), muted));
        }
        if !shell {
            let parsed = app
                .state
                .local
                .model_parsed(&app.state.sync, &app.state.args);
            left.push(Span::styled(" · ".to_string(), muted));
            left.push(Span::styled(parsed.model.clone(), theme.text.to_color()));
            left.push(Span::styled(parsed.provider.clone(), muted));
            if let Some(variant) = app
                .state
                .local
                .variant_current(&app.state.sync, &app.state.args)
            {
                if variant != "default" {
                    left.push(Span::styled(" · ".to_string(), muted));
                    left.push(Span::styled(
                        variant,
                        Style::new()
                            .fg(theme.warning.to_color())
                            .add_modifier(Modifier::BOLD),
                    ));
                }
            }
        }
    }
    let mut right: Vec<Span<'static>> = Vec::new();
    if shell {
        right.push(Span::styled("esc".to_string(), theme.text.to_color()));
        right.push(Span::styled(" exit shell mode".to_string(), muted));
    } else {
        if let Some(shortcut) = crate::keymap::bindings::keybind_for_command("agent.cycle")
            .and_then(|k| key_hint(app, k))
        {
            right.push(Span::styled(shortcut, theme.text.to_color()));
            right.push(Span::styled(" agents".to_string(), muted));
            right.push(Span::raw(" "));
        }
        if let Some(shortcut) = crate::keymap::bindings::keybind_for_command("command.palette.show")
            .and_then(|k| key_hint(app, k))
        {
            right.push(Span::styled(shortcut, theme.text.to_color()));
            right.push(Span::styled(" commands".to_string(), muted));
        }
    }
    let left_width: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let right_width: usize = right.iter().map(|s| s.content.chars().count()).sum();
    let padding = (width as usize).saturating_sub(left_width + right_width);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(padding.max(1))));
    spans.extend(right);
    if app.ui.prompt.submitting {
        // `move.progress() === "Submitting prompt"` with animated dots
        // (`move.tsx:174-177`).
        spans.push(Span::styled("  · ".to_string(), muted));
        spans.push(Span::styled(
            format!("{} Submitting prompt", app.session_spinner()),
            theme.text.to_color(),
        ));
    }
    Line::from(spans)
}

fn key_hint(app: &App, keybind: &str) -> Option<String> {
    crate::ui::dialogs::key_hint(app, keybind)
}

/// `formatDuration` (`util/format.ts:1-18`).
fn format_duration(secs: u64) -> String {
    if secs == 0 {
        String::new()
    } else if secs < 60 {
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
        None | Some(alforria_schema::session_status::SessionStatusInfo::Idle)
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
                alforria_schema::session_status::SessionStatusInfo::Idle => None,
                retry @ alforria_schema::session_status::SessionStatusInfo::Retry { .. } => {
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
    if let Some(alforria_schema::session_status::SessionStatusInfo::Retry {
        attempt,
        message,
        next,
        ..
    }) = &retry
    {
        // `message()` / `isTruncated()` (`prompt/index.tsx:1545-1556`).
        let truncated = message.chars().count() > 120;
        let message =
            if message.contains("exceeded your current quota") && message.contains("gemini") {
                "gemini is way too hot right now".to_string()
            } else if message.chars().count() > 80 {
                format!("{}…", message.chars().take(80).collect::<String>())
            } else {
                message.clone()
            };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let seconds = next.saturating_sub(now_ms) / 1000;
        let duration = format_duration(seconds);
        let truncated_hint = if truncated { " (click to expand)" } else { "" };
        let retry_info = if duration.is_empty() {
            format!(" [retrying attempt #{attempt}]")
        } else {
            format!(" [retrying in {duration} attempt #{attempt}]")
        };
        spans.push(Span::styled(
            format!("{message}{truncated_hint}{retry_info}"),
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
    let max_height =
        crate::ui::textarea::Textarea::max_height(app.config.prompt_max_height, area.height.max(1));
    let rows: Vec<&Vec<(char, Option<u64>)>> =
        display.rows.iter().take(max_height as usize).collect();
    let text_empty = app.ui.prompt.is_empty();
    // `cursor.blinking` (config/index.tsx:33-42) — the OpenTUI cursor
    // blinks; the blink phase is driven by the render tick.
    let blink = (app.ui.tick_ms / 530).is_multiple_of(2);
    let cursor = crate::ui::textarea::cursor_cell_style(theme);
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::raw(""));
    if text_empty {
        let mut spans: Vec<Span> = Vec::new();
        let placeholder = placeholder_text(app);
        if blink {
            // The block cursor sits over the first placeholder cell.
            spans.push(Span::styled(
                placeholder.chars().take(1).collect::<String>(),
                cursor,
            ));
        } else {
            spans.push(Span::styled(
                placeholder.chars().take(1).collect::<String>(),
                theme.text_muted.to_color(),
            ));
        }
        spans.push(Span::styled(
            placeholder.chars().skip(1).collect::<String>(),
            theme.text_muted.to_color(),
        ));
        lines.push(Line::from(spans));
    } else {
        for (row, cells) in rows.iter().enumerate() {
            let mut spans: Vec<Span> = Vec::new();
            for (column, (char, _mark)) in cells.iter().enumerate() {
                let mut style = Style::new().fg(theme.text.to_color());
                if blink && row == display.cursor_row && column == display.cursor_col {
                    style = cursor;
                }
                spans.push(Span::styled(char.to_string(), style));
            }
            // The cursor one-past-the-last cell (its usual position
            // while typing) — draw an explicit block cell.
            if blink && row == display.cursor_row && display.cursor_col >= cells.len() {
                spans.push(Span::styled(" ", cursor));
            }
            if row == display.cursor_row && spans.is_empty() && blink {
                spans.push(Span::styled(" ", cursor));
            }
            lines.push(Line::from(spans));
        }
    }
    lines.push(meta_line(app, theme, area.width.saturating_sub(5)));
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
    let empty = autocomplete.options.is_empty();
    let count = autocomplete
        .options
        .len()
        .min(AUTOCOMPLETE_HEIGHT)
        .min(area.y as usize);
    if count == 0 && !empty {
        return;
    }
    // `options().length || 1` — the empty popup still occupies one row
    // (`autocomplete.tsx:713, 730-735`).
    let count = count.max(1);
    let popup = Rect {
        x: area.x + 1,
        y: area.y - count as u16,
        width: area.width.saturating_sub(1),
        height: count as u16,
    };
    let scroll = autocomplete.scroll.min(autocomplete.options.len());
    let lines: Vec<Line> = autocomplete
        .options
        .iter()
        .skip(scroll)
        .take(count)
        .enumerate()
        .map(|(index, option)| {
            let selected = index + scroll == autocomplete.selected;
            let fg = if selected {
                super::super::theme::selected_foreground(theme, Some(theme.primary))
            } else {
                theme.text
            };
            let bg = if selected {
                theme.primary
            } else {
                theme.background_menu
            };
            let mut spans = vec![Span::styled(
                format!(" {} ", option.display),
                Style::new().fg(fg.to_color()).bg(bg.to_color()),
            )];
            if let Some(description) = &option.description {
                spans.push(Span::styled(
                    format!(" {}", description.trim_start()),
                    Style::new()
                        .fg(if selected { fg } else { theme.text_muted }.to_color())
                        .bg(bg.to_color()),
                ));
            }
            Line::from(spans)
        })
        .collect();
    // The zero-match fallback row keeps the popup visible
    // (`autocomplete.tsx:730-735`).
    let lines = if empty {
        vec![Line::from(Span::styled(
            " No matching items",
            Style::new().fg(theme.text_muted.to_color()),
        ))]
    } else {
        lines
    };
    Paragraph::new(lines).render(popup, frame.buffer_mut());
}

#[cfg(test)]
mod caret_tests {
    use super::*;
    use crate::state::App;

    fn app() -> App {
        App::new(
            crate::config::TuiConfig::default(),
            Default::default(),
            None,
        )
    }

    fn theme() -> crate::ui::theme::Theme {
        let app = app();
        app.ui
            .theme
            .resolve(&app.state.kv)
            .expect("builtin theme resolves")
    }

    #[test]
    fn cursor_renders_at_end_of_row() {
        // The cursor one-past-the-last cell — its usual position while
        // typing — must draw an explicit block cell.
        let mut app = app();
        app.ui.prompt.textarea.set_text("hi");
        app.ui.tick_ms = 0; // blink on
        let t = theme();
        let area = Rect::new(0, 0, 40, 5);
        let backend = ratatui::backend::TestBackend::new(40, 5);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(&app, frame, &t, area);
            })
            .unwrap();
        // The block cursor cell — bg == theme.text.
        let buffer = terminal.backend().buffer();
        let ratatui::style::Color::Rgb(r, g, b) = t.text.to_color() else {
            panic!("expected rgb theme text color");
        };
        assert!(
            (0..5u16).any(|y| {
                (0..40u16).any(|x| {
                    matches!(
                        buffer[(x, y)].bg,
                        ratatui::style::Color::Rgb(r2, g2, b2) if r2 == r && g2 == g && b2 == b
                    )
                })
            }),
            "block cursor cell not rendered"
        );
    }
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
            alforria_schema::session_status::SessionStatusInfo::Busy,
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
            alforria_schema::session_status::SessionStatusInfo::Retry {
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
            alforria_schema::session_status::SessionStatusInfo::Idle,
        );
        assert!(status_row(&app, &theme()).is_none());
    }
}
