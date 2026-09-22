//! `component/prompt/index.tsx` rendering (M8.6): the textarea frame
//! (`1352-1401`), the meta row, and the autocomplete popup
//! (`autocomplete.tsx`).

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
    1 + rows.min(max_height) + 1
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
        spans.push(Span::styled("  ·  ", muted));
        spans.push(Span::styled("sending…", muted));
    }
    if app.ui.prompt.mode == PromptMode::Shell {
        spans.push(Span::styled("  ·  ", muted));
        spans.push(Span::styled("shell", theme.warning.to_color()));
    }
    Line::from(spans)
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
