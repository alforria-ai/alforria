//! `routes/home.tsx` — vertically centered logo + prompt (M8.3). The prompt
//! editor itself lands with M8.6; this renders the frame + placeholder.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
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

/// The rendered prompt row count: `paddingTop={1}` + one textarea row.
const PROMPT_HEIGHT: u16 = 2;

pub fn render(app: &App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    let padded = Rect {
        x: area.x.saturating_add(2),
        y: area.y,
        width: area.width.saturating_sub(4),
        height: area.height,
    };
    let [_, _gap, logo, _, prompt, _bottom] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(4),
        Constraint::Length(4),
        Constraint::Length(1),
        Constraint::Length(PROMPT_HEIGHT),
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
    // The prompt box edits the shared prompt editor (typing inserts
    // through `prompt::text_input`); the seeded --prompt input renders
    // as content until the editor holds text of its own.
    let (text, cursor) = match &app.state.route.data {
        Route::Home {
            prompt: Some(seed), ..
        } if app.ui.prompt.is_empty() => (seed.input.clone(), seed.input.chars().count()),
        _ => (
            app.ui.prompt.textarea.text().to_string(),
            app.ui.prompt.textarea.cursor(),
        ),
    };
    let placeholder = text.is_empty();
    let text = if placeholder {
        placeholder_text(app)
    } else {
        text
    };
    let style = if placeholder {
        theme.text_muted.to_color()
    } else {
        theme.text.to_color()
    };
    // The shared editor renders with a visible cursor (see the session
    // prompt, `ui/session/prompt.rs`): the char at the cursor position is
    // reversed, or a reversed block when the cursor sits at the end.
    let mut spans: Vec<Span> = text
        .chars()
        .enumerate()
        .map(|(index, char)| {
            let mut style = Style::new().fg(style);
            if !placeholder && index == cursor {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Span::styled(char.to_string(), style)
        })
        .collect();
    if !placeholder && cursor >= text.chars().count() {
        spans.push(Span::styled(
            " ",
            Style::new().add_modifier(Modifier::REVERSED),
        ));
    }
    Paragraph::new(Line::from(spans))
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
        .render(center_horizontally(area, max_width), frame.buffer_mut());
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
        // `_` renders as a shadowed space and `~` as a shadowed `▀`.
        let joined = lines.join("\n");
        assert!(joined.contains("█▀▀█ █▀▀█ █▀▀█ █▀▀▄"), "{joined}");
        assert!(joined.contains("█  █ █  █ █▀▀▀ █  █"), "{joined}");
        assert!(joined.contains("▀▀▀▀ █▀▀▀ ▀▀▀▀ ▀▀▀▀"), "{joined}");
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
            format!("{}█▀▀█ █▀▀█ █▀▀█ █▀▀▄ █▀▀▀ █▀▀█ █▀▀█ █▀▀█", pad(20))
        );
        assert_eq!(
            lines[13],
            format!("{}█  █ █  █ █▀▀▀ █  █ █    █  █ █  █ █▀▀▀", pad(20))
        );
        assert_eq!(
            lines[14],
            format!("{}▀▀▀▀ █▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀ ▀▀▀▀", pad(20))
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
        let cell = |column: u16| buffer[(column, 13)].clone();
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
