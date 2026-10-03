//! `component/prompt/index.tsx` rendering (M8.6): the textarea frame
//! (`1352-1401`), the meta row, and the autocomplete popup
//! (`autocomplete.tsx`).

use crate::state::route::Route;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph, Widget};

use super::super::theme::{Rgba, Theme};
use crate::state::route::PromptMode;
use crate::state::App;

/// The autocomplete popup `maxHeight` (`autocomplete.tsx:712-716`).
pub(crate) const AUTOCOMPLETE_HEIGHT: usize = 10;

/// `SplitBorder.customBorderChars` (`ui/border.ts:15-21`).
fn split_border_set() -> ratatui::symbols::border::Set<'static> {
    ratatui::symbols::border::Set {
        vertical_left: "┃",
        vertical_right: "┃",
        bottom_left: "╹",
        ..Default::default()
    }
}

/// `highlight()` (`prompt/index.tsx:1288-1293`): the border tint —
/// `theme.primary` in shell mode, the current agent's colour otherwise
/// (the `tint(theme.border, highlight(), agentMetaAlpha())` blend at
/// full opacity).
pub(crate) fn border_highlight(app: &App, theme: &Theme) -> Rgba {
    if app.ui.prompt.mode == PromptMode::Shell {
        return theme.primary;
    }
    let Some(agent) = app
        .state
        .local
        .agent_current(&app.state.sync)
        .and_then(|agent| agent.get("name").and_then(serde_json::Value::as_str))
    else {
        return theme.border;
    };
    match app.state.local.agent_color(agent, &app.state.sync) {
        crate::state::local::AgentColor::Hex(hex) => Rgba::from_hex(&hex).unwrap_or(theme.border),
        crate::state::local::AgentColor::Theme(key) => theme.get(&key).unwrap_or(theme.border),
    }
}

/// The inner textarea width: the frame pads left+right by 2.
fn inner_width(area: Rect) -> u16 {
    area.width.saturating_sub(4).max(1)
}

/// The wrapped textarea rows (`Textarea::display`).
fn display(app: &App, area: Rect) -> crate::ui::textarea::Display {
    app.ui.prompt.textarea.display(inner_width(area))
}

/// The rendered prompt row count: `paddingTop={1}` + the visible
/// textarea rows + the meta box `paddingTop={1}` + the meta row + the
/// 1-row bottom cap + the bottom row (always rendered)
/// (`prompt/index.tsx:1356-1512, 1515`).
pub fn height(app: &App, area: Rect, terminal_height: u16) -> u16 {
    let max_height =
        crate::ui::textarea::Textarea::max_height(app.config.prompt_max_height, terminal_height);
    let rows = display(app, area).rows.len() as u16;
    1 + rows.min(max_height) + 1 + 1 + 1 + 1
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
/// the agent name (in the `highlight()` colour) · model · provider ·
/// variant, separated by the box `gap={1}`. The right side is the
/// `session_prompt_right` plugin slot — empty in the vanilla UI.
pub(crate) fn meta_line(app: &App, theme: &Theme) -> Line<'static> {
    let muted = theme.text_muted.to_color();
    let shell = app.ui.prompt.mode == PromptMode::Shell;
    let mut items: Vec<Vec<Span<'static>>> = Vec::new();
    if let Some(agent) = app
        .state
        .local
        .agent_current(&app.state.sync)
        .and_then(|a| a.get("name").and_then(|v| v.as_str()))
    {
        // `fadeColor(highlight(), agentMetaAlpha())` at full alpha.
        items.push(vec![Span::styled(
            if shell {
                "Shell".to_string()
            } else {
                crate::ui::locale::titlecase(agent)
            },
            border_highlight(app, theme).to_color(),
        )]);
        if !shell && app.state.permission_mode == crate::state::PermissionMode::Auto {
            items.push(vec![Span::styled("auto".to_string(), muted)]);
        }
        if !shell {
            let parsed = app
                .state
                .local
                .model_parsed(&app.state.sync, &app.state.args);
            items.push(vec![Span::styled("·".to_string(), muted)]);
            items.push(vec![Span::styled(parsed.model, theme.text.to_color())]);
            items.push(vec![Span::styled(parsed.provider, muted)]);
            if let Some(variant) = app
                .state
                .local
                .variant_current(&app.state.sync, &app.state.args)
            {
                if variant != "default" {
                    items.push(vec![Span::styled("·".to_string(), muted)]);
                    items.push(vec![Span::styled(
                        variant,
                        Style::new()
                            .fg(theme.warning.to_color())
                            .add_modifier(Modifier::BOLD),
                    )]);
                }
            }
        }
    }
    // The box `gap={1}` between the row's children.
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        spans.extend(item);
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

/// The spinner colour (`spinnerDef`, `prompt/index.tsx:1331-1343`):
/// the agent colour of the last user message's agent — falling back
/// to the current agent — else `theme.border`.
fn spinner_color(app: &App, theme: &Theme, session_id: Option<&str>) -> Rgba {
    let agent = session_id
        .and_then(|id| app.state.sync.message.get(id))
        .and_then(|messages| {
            messages.iter().rev().find_map(|message| match message {
                alforria_schema::session_v1::V1Message::User { agent, .. } => Some(agent.clone()),
                _ => None,
            })
        })
        .or_else(|| {
            app.state
                .local
                .agent_current(&app.state.sync)
                .and_then(|a| a.get("name").and_then(serde_json::Value::as_str))
                .map(str::to_string)
        });
    let Some(agent) = agent else {
        return theme.border;
    };
    match app.state.local.agent_color(&agent, &app.state.sync) {
        crate::state::local::AgentColor::Hex(hex) => Rgba::from_hex(&hex).unwrap_or(theme.border),
        crate::state::local::AgentColor::Theme(key) => theme.get(&key).unwrap_or(theme.border),
    }
}

/// `usage()` (`prompt/index.tsx:264-286`): the token total of the
/// last assistant message (with its context-window percentage) plus
/// the session cost — `$x · $y` collapsed into one muted label.
fn usage_label(app: &App, session_id: &str) -> Option<String> {
    let session = app.state.sync.session(session_id)?;
    let last = app
        .state
        .sync
        .message
        .get(session_id)?
        .iter()
        .rev()
        .find(|message| {
            matches!(
                message,
                alforria_schema::session_v1::V1Message::Assistant { tokens, .. }
                    if tokens.output > 0.0
            )
        })?;
    let alforria_schema::session_v1::V1Message::Assistant {
        tokens,
        provider_id,
        model_id,
        ..
    } = last
    else {
        return None;
    };
    let total =
        tokens.input + tokens.output + tokens.reasoning + tokens.cache.read + tokens.cache.write;
    if total <= 0.0 {
        return None;
    }
    let limit = app
        .state
        .sync
        .provider
        .iter()
        .find(|p| p.get("id").and_then(serde_json::Value::as_str) == Some(provider_id.as_str()))
        .and_then(|p| {
            p.get("models")?
                .get(model_id.as_str())?
                .get("limit")?
                .get("context")?
                .as_f64()
        });
    let tokens = crate::ui::locale::number(total.round() as i64);
    let context = match limit {
        Some(limit) if limit > 0.0 => {
            format!("{tokens} ({}%)", (total / limit * 100.0).round() as i64)
        }
        _ => tokens,
    };
    let cost = session.cost.unwrap_or(0.0);
    Some(if cost > 0.0 {
        format!("{context} · {}", crate::ui::locale::usd(cost))
    } else {
        context
    })
}

/// The bottom row (`prompt/index.tsx:1515-1700`): always rendered
/// below the cap — the spinner / retry message / `esc interrupt`
/// cluster while the session is busy, the `Submitting prompt` spinner
/// or the session directory while idle, and the right-hand
/// `agents`/`commands` shortcut cluster (`esc exit shell mode` in
/// shell mode; hidden while retrying).
pub(crate) fn bottom_row(app: &App, theme: &Theme, width: u16) -> Line<'static> {
    let session_id = match &app.state.route.data {
        Route::Session { session_id, .. } => Some(session_id.clone()),
        _ => None,
    };
    let status = session_id
        .as_deref()
        .and_then(|id| app.state.sync.session_status.get(id));
    let retry = match status {
        Some(retry @ alforria_schema::session_status::SessionStatusInfo::Retry { .. }) => {
            Some(retry.clone())
        }
        _ => None,
    };
    let busy = retry.is_some()
        || matches!(
            status,
            Some(alforria_schema::session_status::SessionStatusInfo::Busy)
        );
    let mut left: Vec<Span<'static>> = Vec::new();
    if busy {
        // `marginLeft={1}` before the spinner (`prompt/index.tsx:1522`).
        left.push(Span::raw(" "));
        left.push(Span::styled(
            app.session_spinner(),
            spinner_color(app, theme, session_id.as_deref()).to_color(),
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
            left.push(Span::raw(" "));
            left.push(Span::styled(
                format!("{message}{truncated_hint}{retry_info}"),
                theme.error.to_color(),
            ));
        }
    } else if app.ui.prompt.submitting {
        // `move.progress()` — `Submitting prompt` with animated dots
        // (`move.tsx:174-177`), `paddingLeft={3}` + accent spinner.
        left.push(Span::raw("   "));
        left.push(Span::styled(app.session_spinner(), theme.accent.to_color()));
        left.push(Span::styled(" Submitting prompt", theme.text.to_color()));
        left.push(Span::styled(
            app.submitting_dots(),
            theme.text_muted.to_color(),
        ));
    } else if let Some(session) = session_id
        .as_deref()
        .and_then(|id| app.state.sync.session(id))
    {
        // The idle hint fallback — the session directory
        // (`prompt/index.tsx:1650-1659`).
        left.push(Span::raw(" "));
        left.push(Span::styled(
            session.directory.clone(),
            theme.text_muted.to_color(),
        ));
    }
    // The right cluster: `esc interrupt` right-aligned while retrying
    // (`space-between`), the shortcut hints otherwise (`gap={2}`).
    let armed = app.ui.interrupt > 0;
    let mut right: Vec<Span<'static>> = Vec::new();
    if retry.is_some() {
        right.extend(interrupt_spans(theme, armed));
    } else if busy {
        left.push(Span::raw(" "));
        left.extend(interrupt_spans(theme, armed));
        right.extend(shortcut_spans(app, theme, session_id.as_deref()));
    } else {
        right.extend(shortcut_spans(app, theme, session_id.as_deref()));
    }
    let left_width: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let right_width: usize = right.iter().map(|s| s.content.chars().count()).sum();
    let padding = (width as usize).saturating_sub(left_width + right_width);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(padding.max(1))));
    spans.extend(right);
    Line::from(spans)
}

/// `esc interrupt` / `esc again to interrupt` — primary once the
/// double-press is armed (`store.interrupt`).
fn interrupt_spans(theme: &Theme, armed: bool) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            "esc ",
            if armed {
                theme.primary.to_color()
            } else {
                theme.text.to_color()
            },
        ),
        Span::styled(
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
        ),
    ]
}

/// The right-hand shortcut cluster (`prompt/index.tsx:1663-1686`):
/// the usage label (or the `agents` shortcut) and `commands`, or
/// `esc exit shell mode` in shell mode.
fn shortcut_spans(app: &App, theme: &Theme, session_id: Option<&str>) -> Vec<Span<'static>> {
    let muted = theme.text_muted.to_color();
    let mut spans = vec![Span::raw("  ")]; // gap={2}
    if app.ui.prompt.mode == PromptMode::Shell {
        spans.push(Span::styled("esc", theme.text.to_color()));
        spans.push(Span::styled(" exit shell mode", muted));
        return spans;
    }
    let usage = session_id.and_then(|id| usage_label(app, id));
    match usage {
        Some(usage) => spans.push(Span::styled(usage, muted)),
        None => {
            if let Some(shortcut) = crate::keymap::bindings::keybind_for_command("agent.cycle")
                .and_then(|k| key_hint(app, k))
            {
                spans.push(Span::styled(shortcut, theme.text.to_color()));
                spans.push(Span::styled(" agents", muted));
            }
        }
    }
    if let Some(shortcut) = crate::keymap::bindings::keybind_for_command("command.palette.show")
        .and_then(|k| key_hint(app, k))
    {
        spans.push(Span::raw("  ")); // gap={2}
        spans.push(Span::styled(shortcut, theme.text.to_color()));
        spans.push(Span::styled(" commands", muted));
    }
    spans
}

/// The prompt frame + textarea + meta row (`prompt/index.tsx:1352-1401`).
pub fn render(app: &App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    let display = display(app, area);
    let max_height =
        crate::ui::textarea::Textarea::max_height(app.config.prompt_max_height, area.height.max(1));
    let window = display.window(max_height);
    let rows: Vec<&Vec<(char, Option<u64>)>> = display.rows[window.clone()].iter().collect();
    let text_empty = app.ui.prompt.is_empty();
    // `cursor.blinking` (config/index.tsx:33-42) — the OpenTUI cursor
    // blinks; the blink phase is driven by the render tick.
    // An open dialog holds the focus — the prompt behind it shows no
    // cursor (`focus?.blur()`, `ui/dialog.tsx:150-156`).
    let blink = app.ui.prompt_focused && (app.ui.tick_ms / 530).is_multiple_of(2);
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
                if blink && window.start + row == display.cursor_row && column == display.cursor_col
                {
                    style = cursor;
                }
                spans.push(Span::styled(char.to_string(), style));
            }
            // The cursor one-past-the-last cell (its usual position
            // while typing) — draw an explicit block cell.
            if blink
                && window.start + row == display.cursor_row
                && display.cursor_col >= cells.len()
            {
                spans.push(Span::styled(" ", cursor));
            }
            if row == display.cursor_row && spans.is_empty() && blink {
                spans.push(Span::styled(" ", cursor));
            }
            lines.push(Line::from(spans));
        }
    }
    // The meta box `paddingTop={1}` — a blank row above the meta row.
    lines.push(Line::raw(""));
    lines.push(meta_line(app, theme));

    // The bottom cap (`prompt/index.tsx:1487-1512`) and the bottom
    // row render below the frame, outside the left border.
    let bottom = 2;
    let frame_height = area.height.saturating_sub(bottom);
    if frame_height == 0 {
        return;
    }
    let mut text = ratatui::text::Text::from(lines);
    if text.height() as u16 > frame_height {
        text = ratatui::text::Text::from(
            text.lines
                .into_iter()
                .take(frame_height as usize)
                .collect::<Vec<Line>>(),
        );
    }
    Paragraph::new(text)
        .block(
            Block::new()
                .borders(ratatui::widgets::Borders::LEFT)
                .border_set(split_border_set())
                .border_style(border_highlight(app, theme).to_color())
                .style(Style::new().bg(theme.background_element.to_color()))
                .padding(Padding {
                    left: 2,
                    right: 2,
                    top: 0,
                    bottom: 0,
                }),
        )
        .render(
            Rect {
                height: frame_height,
                ..area
            },
            frame.buffer_mut(),
        );
    let visible = theme.background_element.a != 0.0;
    let cap = Line::from(vec![
        Span::styled(
            if visible { "╹" } else { " " },
            Style::new().fg(border_highlight(app, theme).to_color()),
        ),
        Span::styled(
            if visible { "▀" } else { " " }.repeat(area.width.saturating_sub(1) as usize),
            Style::new().fg(theme.background_element.to_color()),
        ),
    ]);
    Paragraph::new(cap).render(
        Rect {
            y: area.y + frame_height,
            height: 1,
            ..area
        },
        frame.buffer_mut(),
    );
    Paragraph::new(bottom_row(app, theme, area.width)).render(
        Rect {
            y: area.y + frame_height + 1,
            height: 1,
            ..area
        },
        frame.buffer_mut(),
    );

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
    // `top={position().y - height()} left={position().x}
    // width={position().width}` — the anchor spans the prompt area
    // (`autocomplete.tsx:725-728`).
    let popup = Rect {
        x: area.x,
        y: area.y.saturating_sub(count as u16),
        width: area.width,
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
    Paragraph::new(lines)
        .block(
            Block::new()
                .borders(ratatui::widgets::Borders::LEFT | ratatui::widgets::Borders::RIGHT)
                .border_set(split_border_set())
                .border_style(theme.border.to_color())
                .style(Style::new().bg(theme.background_menu.to_color())),
        )
        .render(popup, frame.buffer_mut());
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

    fn row_text(app: &App, width: u16) -> String {
        bottom_row(app, &theme(), width)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn busy_session_shows_interrupt_hint() {
        let app = app_busy();
        let text = row_text(&app, 80);
        assert!(text.contains("esc interrupt"), "hint missing: {text}");
        // Armed double-press flips to the "again" hint.
        let mut app = app_busy();
        app.ui.interrupt = 1;
        let text = row_text(&app, 80);
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
        let text = row_text(&app, 80);
        assert!(text.contains("rate limited"), "message missing: {text}");
        assert!(text.contains("attempt #2"), "attempt missing: {text}");
    }

    #[test]
    fn idle_session_shows_shortcuts_instead_of_interrupt() {
        let mut app = app_busy();
        app.state.sync.session_status.insert(
            "ses_1".to_string(),
            alforria_schema::session_status::SessionStatusInfo::Idle,
        );
        let text = row_text(&app, 80);
        assert!(!text.contains("interrupt"), "unexpected hint: {text}");
        assert!(
            text.contains("commands") && text.contains("agents"),
            "shortcut hints missing: {text}"
        );
    }

    #[test]
    fn idle_session_shows_the_directory_hint() {
        let mut app = app_busy();
        app.state.sync.session_status.insert(
            "ses_1".to_string(),
            alforria_schema::session_status::SessionStatusInfo::Idle,
        );
        app.state.sync.session = vec![crate::ui::session::tests::session_info("ses_1", "x")];
        let text = row_text(&app, 80);
        assert!(text.contains(" /x"), "directory hint missing: {text}");
    }

    #[test]
    fn meta_row_gaps_and_agent_color() {
        let mut app = App::new(
            crate::config::TuiConfig::default(),
            Default::default(),
            None,
        );
        app.state.sync.agent = vec![serde_json::json!({ "name": "build" })];
        app.state.sync.provider = vec![serde_json::json!({
            "id": "anthropic",
            "name": "Anthropic",
            "models": {"claude": {"id": "claude", "name": "Claude"}},
        })];
        app.state.local.model_set(
            &app.state.sync,
            crate::state::local::ModelRef {
                provider_id: "anthropic".into(),
                model_id: "claude".into(),
            },
            false,
        );
        app.state.permission_mode = crate::state::PermissionMode::Auto;
        let theme = theme();
        let line = meta_line(&app, &theme);
        let text = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        // The box `gap={1}` — never `buildauto`/`claudeanthropic`.
        assert_eq!(text, "Build auto · Claude Anthropic");
        // The agent name carries the `highlight()` colour.
        assert_eq!(
            line.spans[0].style.fg,
            border_highlight(&app, &theme).to_color().into()
        );
    }

    #[test]
    fn usage_label_formats_tokens_and_cost() {
        let mut app = App::new(
            crate::config::TuiConfig::default(),
            Default::default(),
            None,
        );
        app.state.sync.provider = vec![serde_json::json!({
            "id": "anthropic",
            "models": {"claude": {"id": "claude", "limit": {"context": 1000}}},
        })];
        let mut session = crate::ui::session::tests::session_info("ses_1", "x");
        session.cost = Some(0.03);
        app.state.sync.session = vec![session];
        app.state.sync.message.insert(
            "ses_1".to_string(),
            vec![alforria_schema::session_v1::V1Message::Assistant {
                id: "msg_1".into(),
                session_id: "ses_1".into(),
                time: alforria_schema::session_v1::AssistantTime {
                    created: 0,
                    completed: Some(1),
                },
                error: None,
                parent_id: "msg_0".into(),
                model_id: "claude".into(),
                provider_id: "anthropic".into(),
                mode: "primary".into(),
                agent: "build".into(),
                path: alforria_schema::session_v1::V1Path {
                    cwd: "/x".into(),
                    root: "/x".into(),
                },
                summary: None,
                cost: 0.0,
                tokens: alforria_schema::session_v1::V1StepTokens {
                    total: None,
                    input: 500.0,
                    output: 300.0,
                    reasoning: 0.0,
                    cache: alforria_schema::session_v1::V1TokenCache {
                        read: 0.0,
                        write: 0.0,
                    },
                },
                structured: None,
                variant: None,
                finish: None,
            }],
        );
        assert_eq!(
            usage_label(&app, "ses_1"),
            Some("800 (80%) · $0.03".to_string())
        );
    }
}
