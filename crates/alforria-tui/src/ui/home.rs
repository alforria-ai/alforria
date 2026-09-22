//! `routes/home.tsx` — vertically centered logo + prompt (M8.3). The prompt
//! editor itself lands with M8.6; this renders the frame + placeholder.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget};

use super::theme::{tint, Rgba, Theme};
use crate::state::route::Route;
use crate::state::App;

/// `placeholder` (`home.tsx:17-20`).
pub const PLACEHOLDER_NORMAL: [&str; 3] = [
    "Fix a TODO in the codebase",
    "What is the tech stack of this project?",
    "Fix broken tests",
];
pub const PLACEHOLDER_SHELL: [&str; 3] = ["ls -la", "git status", "pwd"];

/// `placeholderText` (`prompt/index.tsx:1311-1319`).
pub fn placeholder_text(app: &App) -> String {
    let index = app.ui.home_placeholder % PLACEHOLDER_NORMAL.len();
    format!("Ask anything… \"{}\"", PLACEHOLDER_NORMAL[index])
}

/// `renderLine` (`component/logo.tsx:9-47`): `_` is a shadowed space, `^`
/// a shadowed `▀`, `~` a shadowed `▀`, `,` a shadowed `▄` — everything
/// else uses the foreground.
fn logo_spans(line: &str, fg: Rgba, bold: bool, theme: &Theme) -> Vec<Span<'static>> {
    let shadow = tint(theme.background, fg, 0.25);
    let mut style = Style::new().fg(fg.to_color());
    if bold {
        style = style.add_modifier(ratatui::style::Modifier::BOLD);
    }
    line.chars()
        .map(|char| match char {
            '_' => Span::styled(" ", Style::new().fg(fg.to_color()).bg(shadow.to_color())),
            '^' => Span::styled("▀", Style::new().fg(fg.to_color()).bg(shadow.to_color())),
            '~' => Span::styled("▀", Style::new().fg(shadow.to_color())),
            ',' => Span::styled("▄", Style::new().fg(shadow.to_color())),
            _ => Span::styled(char.to_string(), style),
        })
        .collect()
}

/// `Logo` (`component/logo.tsx:49-60`).
fn logo_lines(theme: &Theme) -> Vec<Line<'static>> {
    let logo = &crate::ui::LOGO;
    logo.left
        .iter()
        .zip(logo.right.iter())
        .map(|(left, right)| {
            let mut spans = logo_spans(left, theme.text_muted, false, theme);
            spans.push(Span::raw(" "));
            spans.extend(logo_spans(right, theme.text, true, theme));
            Line::from(spans)
        })
        .collect()
}

/// `Logo` column width: left half + gap + right half.
const LOGO_WIDTH: u16 = 39;

/// The inner textarea width: the frame pads left+right by 2.
fn inner_width(area: Rect) -> u16 {
    area.width.saturating_sub(4).max(1)
}

/// The wrapped textarea rows: the live editor, or the seeded --prompt
/// input while the editor is empty.
fn rows(app: &App, area: Rect) -> crate::ui::textarea::Display {
    let seed = match &app.state.route.data {
        Route::Home {
            prompt: Some(seed), ..
        } if app.ui.prompt.is_empty() => Some(&seed.input),
        _ => None,
    };
    let Some(seed) = seed else {
        return app.ui.prompt.textarea.display(inner_width(area));
    };
    let mut rows: Vec<Vec<(char, Option<u64>)>> = vec![Vec::new()];
    for char in seed.chars() {
        if char == '\n' {
            rows.push(Vec::new());
        } else {
            rows.last_mut()
                .expect("rows starts with one row")
                .push((char, None));
        }
    }
    let cursor_row = rows.len() - 1;
    let cursor_col = rows.last().map(Vec::len).unwrap_or(0);
    crate::ui::textarea::Display {
        rows,
        cursor_row,
        cursor_col,
    }
}

/// The rendered prompt row count: `paddingTop={1}` + the textarea rows.
fn prompt_height(rows: usize) -> u16 {
    1 + rows.min(u16::MAX as usize) as u16
}

pub fn render(app: &App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    let padded = Rect {
        x: area.x.saturating_add(2),
        y: area.y,
        width: area.width.saturating_sub(4),
        height: area.height,
    };
    let rows = rows(app, padded).rows;
    let [_, _gap, logo, _, prompt, _bottom] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(4),
        Constraint::Length(4),
        Constraint::Length(1),
        Constraint::Length(prompt_height(rows.len())),
        Constraint::Fill(1),
    ])
    .areas(padded);
    Paragraph::new(logo_lines(theme))
        .render(center_horizontally(logo, LOGO_WIDTH), frame.buffer_mut());
    render_prompt(app, frame, theme, prompt);
}

fn center_horizontally(area: Rect, width: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        width: width.min(area.width),
        y: area.y,
        height: area.height,
    }
}

/// `Prompt` frame (`prompt/index.tsx:1352-1401`): left border +
/// `backgroundElement` fill + 2-col padding. The textarea contents
/// render over the seeded --prompt input (M8.6).
fn render_prompt(app: &App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    let max_width = app
        .config
        .prompt_max_width(area.width.saturating_sub(4))
        .min(area.width);
    let display = rows(app, area);
    let placeholder = display.rows.iter().all(|row| row.is_empty());
    // `cursor.blinking` — the same block cursor as the session prompt.
    let blink = (app.ui.tick_ms / 530).is_multiple_of(2);
    let cursor = crate::ui::textarea::cursor_cell_style(theme);
    let mut lines: Vec<Line> = Vec::new();
    if placeholder {
        let mut spans: Vec<Span> = Vec::new();
        let text = placeholder_text(app);
        if blink {
            spans.push(Span::styled(
                text.chars().take(1).collect::<String>(),
                cursor,
            ));
        } else {
            spans.push(Span::styled(
                text.chars().take(1).collect::<String>(),
                theme.text_muted.to_color(),
            ));
        }
        spans.push(Span::styled(
            text.chars().skip(1).collect::<String>(),
            theme.text_muted.to_color(),
        ));
        lines.push(Line::from(spans));
    } else {
        for (row, cells) in display.rows.iter().enumerate() {
            let mut spans: Vec<Span> = Vec::new();
            for (column, (char, _mark)) in cells.iter().enumerate() {
                let mut style = Style::new().fg(theme.text.to_color());
                if blink && row == display.cursor_row && column == display.cursor_col {
                    style = cursor;
                }
                spans.push(Span::styled(char.to_string(), style));
            }
            if blink && row == display.cursor_row && display.cursor_col >= cells.len() {
                spans.push(Span::styled(" ", cursor));
            }
            if row == display.cursor_row && spans.is_empty() && blink {
                spans.push(Span::styled(" ", cursor));
            }
            lines.push(Line::from(spans));
        }
    }
    let prompt_area = center_horizontally(area, max_width);
    Paragraph::new(lines)
        .block(
            Block::new()
                .borders(ratatui::widgets::Borders::LEFT)
                .border_style(theme.border.to_color())
                .style(Style::new().bg(theme.background_element.to_color()))
                .padding(ratatui::widgets::Padding {
                    left: 2,
                    right: 2,
                    top: 1,
                    bottom: 0,
                }),
        )
        .render(prompt_area, frame.buffer_mut());
    if app.ui.prompt.autocomplete.visible.is_some() {
        crate::ui::session::prompt::render_autocomplete(app, frame, theme, prompt_area);
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{PromptMaxWidth, TuiConfig};

    use super::*;

    fn make_app() -> App {
        App::new(TuiConfig::default(), crate::state::Args::default(), None)
    }

    fn buffer_text(app: &App, width: u16, height: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut lines = Vec::new();
        terminal
            .draw(|frame| {
                let theme = app
                    .ui
                    .theme
                    .resolve(&app.state.kv)
                    .expect("builtin theme resolves");
                let area = frame.area();
                super::render(app, frame, &theme, area);
            })
            .unwrap();
        for y in 0..height {
            let mut line = String::new();
            for x in 0..width {
                line.push_str(terminal.backend().buffer()[(x, y)].symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        lines
    }

    #[test]
    fn logo_renders_the_wordmark() {
        let lines = buffer_text(&make_app(), 80, 24);
        let joined = lines.join("\n");
        assert!(
            joined.contains("█▀▀█ █    █▀▀█ █▀▀█ █▀▀█ █▀▀█ ▀▀▀▀ █▀▀█"),
            "{joined}"
        );
        assert!(
            joined.contains("█▄▄█ █    █▀▀▀ █  █ █  ▀ █  ▀  █   █▄▄█"),
            "{joined}"
        );
        assert!(
            joined.contains("█  █ █▀▀█ █    ▀▀▀▀ █  █ █  █ ▀▀▀▀ █  █"),
            "{joined}"
        );
    }

    #[test]
    fn prompt_renders_the_placeholder() {
        let lines = buffer_text(&make_app(), 80, 24);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Ask anything… \"Fix a TODO in the codebase\"")),
            "{lines:?}"
        );
    }

    /// Regression (battle-test round 4): the home prompt box renders the
    /// shared prompt editor — typed text must appear on the home screen.
    #[test]
    fn prompt_renders_typed_text() {
        let mut app = make_app();
        app.ui.prompt.textarea.insert_text("hello");
        let lines = buffer_text(&app, 80, 24);
        assert!(lines.iter().any(|l| l.contains("hello")), "{lines:?}");
    }

    #[test]
    fn placeholder_rolls_with_the_index() {
        let mut app = make_app();
        app.ui.home_placeholder = 1;
        let lines = buffer_text(&app, 80, 24);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Ask anything… \"What is the tech stack of this project?\"")),
            "{lines:?}"
        );
    }

    #[test]
    fn renders_at_three_widths() {
        for width in [60u16, 80, 200] {
            let lines = buffer_text(&make_app(), width, 24);
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("Ask anything… \"Fix a TODO in the codebase\"")),
                "{width}: {lines:?}"
            );
            assert!(
                lines.iter().any(|l| l.contains("█▀▀█")),
                "{width}: logo missing"
            );
        }
    }

    #[test]
    fn auto_prompt_width_scales() {
        let mut app = make_app();
        app.config = TuiConfig {
            prompt_max_width: PromptMaxWidth::Auto,
            ..TuiConfig::default()
        };
        let lines = buffer_text(&app, 140, 24);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Ask anything… \"Fix a TODO in the codebase\"")),
            "{lines:?}"
        );
    }

    #[test]
    fn golden_home_80x24() {
        let lines = buffer_text(&make_app(), 80, 24);
        let pad = |n| " ".repeat(n);
        assert_eq!(lines.len(), 24);
        // vertical: fill(7) + gap(4) + logo(4) + spacer(1) + prompt(2) + fill(6)
        assert_eq!(lines[11], format!("{}▄", pad(53)));
        assert_eq!(
            lines[12],
            format!("{}█▀▀█ █    █▀▀█ █▀▀█ █▀▀█ █▀▀█ ▀▀▀▀ █▀▀█", pad(20))
        );
        assert_eq!(
            lines[13],
            format!("{}█▄▄█ █    █▀▀▀ █  █ █  ▀ █  ▀  █   █▄▄█", pad(20))
        );
        assert_eq!(
            lines[14],
            format!("{}█  █ █▀▀█ █    ▀▀▀▀ █  █ █  █ ▀▀▀▀ █  █", pad(20))
        );
        assert_eq!(lines[16], format!("{}│", pad(2)));
        assert_eq!(
            lines[17],
            format!("{}│  Ask anything… \"Fix a TODO in the codebase\"", pad(2))
        );
        for (row, line) in lines.iter().enumerate() {
            if ![11, 12, 13, 14, 16, 17].contains(&row) {
                assert_eq!(line, "", "row {row}: {}", lines[row]);
            }
        }
    }

    #[test]
    fn golden_home_styles() {
        // The left half renders muted, the right half bold in `text`.
        let app = make_app();
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let theme = app
                    .ui
                    .theme
                    .resolve(&app.state.kv)
                    .expect("builtin theme resolves");
                super::render(&app, frame, &theme, frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let cell = |column: u16| buffer[(column, 12)].clone();
        // Left half `█` in textMuted, no modifiers.
        assert_eq!(cell(20).fg, ratatui::style::Color::Rgb(0x80, 0x80, 0x80));
        assert_eq!(cell(20).modifier, ratatui::style::Modifier::empty());
        // Right half `█` in text + bold.
        assert_eq!(cell(40).fg, ratatui::style::Color::Rgb(0xee, 0xee, 0xee));
        assert_eq!(
            cell(40).modifier,
            ratatui::style::Modifier::BOLD,
            "right logo half is bold"
        );
    }

    #[test]
    fn placeholder_sets_are_the_ts_ones() {
        assert_eq!(
            PLACEHOLDER_NORMAL,
            [
                "Fix a TODO in the codebase",
                "What is the tech stack of this project?",
                "Fix broken tests",
            ]
        );
        assert_eq!(PLACEHOLDER_SHELL, ["ls -la", "git status", "pwd"]);
    }
}
