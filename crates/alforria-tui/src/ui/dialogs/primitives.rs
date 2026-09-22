//! `ui/dialog-select.tsx` — the list + filter primitive (`DialogSelect`)
//! plus the confirm/alert/prompt state machines
//! (`ui/dialog-confirm.tsx`, `ui/dialog-alert.tsx`, `ui/dialog-prompt.tsx`).

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::super::theme::Theme;

/// `Math.floor(dimensions().height / 2) - 6` — the scrollbox window
/// (`dialog-select.tsx:213`), clamped to at least one row.
pub fn max_visible_options(terminal_height: u16) -> usize {
    ((terminal_height / 2).saturating_sub(6)).max(1) as usize
}

/// One `DialogSelectOption` — the option subset the port renders.
#[derive(Debug, Clone, Default)]
pub struct SelectOption {
    pub title: String,
    /// Identity used for the `●` current marker and select actions.
    pub value: Option<String>,
    pub description: Option<String>,
    pub category: Option<String>,
    pub footer: Option<String>,
    pub gutter: Option<String>,
    /// The foreground of the gutter cell — the status-coloured `●`/
    /// `✓` markers (`dialog-workspace-list.tsx:49`,
    /// `dialog-provider.tsx:144`).
    pub gutter_fg: Option<crate::ui::theme::Rgba>,
    /// The `props.current` `●` marker (`dialog-select.tsx:747`).
    pub current: bool,
    /// The error background of the delete-confirm rows.
    pub bg_error: bool,
}

impl SelectOption {
    pub fn new(title: impl Into<String>) -> SelectOption {
        SelectOption {
            title: title.into(),
            ..SelectOption::default()
        }
    }

    pub fn with_value(self, value: impl Into<String>) -> SelectOption {
        let mut option = self;
        option.value = Some(value.into());
        option
    }

    pub fn with_description(self, description: impl Into<String>) -> SelectOption {
        let mut option = self;
        option.description = Some(description.into());
        option
    }

    pub fn with_category(self, category: impl Into<String>) -> SelectOption {
        let mut option = self;
        option.category = Some(category.into());
        option
    }

    pub fn with_gutter(self, gutter: Option<String>) -> SelectOption {
        SelectOption { gutter, ..self }
    }

    pub fn with_gutter_fg(self, gutter_fg: Option<crate::ui::theme::Rgba>) -> SelectOption {
        SelectOption { gutter_fg, ..self }
    }

    pub fn with_footer(self, footer: impl Into<String>) -> SelectOption {
        let mut option = self;
        option.footer = Some(footer.into());
        option
    }

    pub fn with_current(self, current: bool) -> SelectOption {
        SelectOption { current, ..self }
    }

    pub fn with_bg_error(self, bg_error: bool) -> SelectOption {
        SelectOption { bg_error, ..self }
    }
}

/// A fuzzysort-style score for a case-insensitive subsequence match
/// (`dialog-select.tsx:154-173`). Earlier + tighter matches score higher.
/// The TS library scores are not ported verbatim — this is the hand-rolled
/// equivalent (a recorded divergence; the palette goldens pin the
/// ordering).
pub fn fuzzy_score(needle: &str, haystack: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let needle: Vec<char> = needle.to_lowercase().chars().collect();
    let haystack: Vec<char> = haystack.to_lowercase().chars().collect();
    let mut score = 0i32;
    let mut hi = 0usize;
    let mut last_match = 0usize;
    for (index, char) in needle.iter().enumerate() {
        let offset = haystack[hi..].iter().position(|c| c == char)?;
        let position = hi + offset;
        if index == 0 {
            score -= offset as i32;
        } else if position != last_match + 1 {
            score -= (position - last_match) as i32;
        }
        last_match = position;
        hi = position + 1;
    }
    Some(score)
}

/// Filter the options like `fuzzysort.go(needle, options, { keys:
/// ["title", "category"], scoreFn: r => r[0] * 2 + r[1] })` — title
/// matches weigh double. Stable on input order for ties.
pub fn filter_options(needle: &str, options: Vec<SelectOption>) -> Vec<SelectOption> {
    if needle.is_empty() {
        return options;
    }
    let mut scored: Vec<(i32, usize, SelectOption)> = options
        .into_iter()
        .enumerate()
        .filter_map(|(index, option)| {
            let title = fuzzy_score(needle, &option.title);
            let category = option
                .category
                .as_deref()
                .and_then(|category| fuzzy_score(needle, category));
            let score = match (title, category) {
                (Some(title), Some(category)) => Some(title * 2 + category),
                (Some(title), None) => Some(title * 2),
                (None, Some(category)) => Some(category),
                (None, None) => None,
            };
            score.map(|score| (score, index, option))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, option)| option).collect()
}

/// The `DialogSelect` interaction state: `selected`, the filter input and
/// the scrollbox offset (`dialog-select.tsx:90-94`).
#[derive(Debug, Default, Clone)]
pub struct SelectState {
    pub selected: usize,
    pub filter: String,
    pub scroll: usize,
}

impl SelectState {
    /// `move()` (`dialog-select.tsx:290-297`) — wrap-around.
    pub fn move_by(&mut self, direction: i64, len: usize, max_visible: usize) {
        if len == 0 {
            return;
        }
        let next = self.selected as i64 + direction;
        self.selected = if next < 0 {
            len - 1
        } else if next >= len as i64 {
            0
        } else {
            next as usize
        };
        self.clamp_scroll(max_visible);
    }

    /// `moveTo()` (`dialog-select.tsx:299-309`).
    pub fn move_to(&mut self, index: usize, max_visible: usize) {
        self.selected = index;
        self.clamp_scroll(max_visible);
    }

    /// Keep the selection inside the scrollbox window
    /// (`scrollToSelection`, `dialog-select.tsx:311-342`).
    fn clamp_scroll(&mut self, max_visible: usize) {
        let max_visible = max_visible.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + max_visible {
            self.scroll = self.selected + 1 - max_visible;
        }
    }
}

/// What the generic select renderer draws for one dialog — the pre-built
/// option list plus its chrome.
pub struct SelectView {
    pub title: String,
    /// `renderFilter === false` hides the search input.
    pub filter: bool,
    pub options: Vec<SelectOption>,
    /// The `actions`/`footerHints` — `(title, label)` pairs.
    pub actions: Vec<(String, String)>,
    /// `actionFocused()` — a focused footer action mutes the option rows
    /// (`dialog-select.tsx:637-654`).
    pub action_focused: bool,
}

/// The shared header row: bold title left, muted hint right.
pub fn header_line(theme: &Theme, title: &str, hint: &str, width: u16) -> Line<'static> {
    // `paddingLeft/Right 4`, `justifyContent: space-between`
    // (`dialog-select.tsx:557-568`).
    let prefix = "    ";
    let used = 2 * prefix.chars().count() + title.chars().count() + hint.chars().count();
    let padding = (width as usize).saturating_sub(used);
    Line::from(vec![
        Span::raw(prefix),
        Span::styled(
            title.to_string(),
            Style::new()
                .fg(theme.text.to_color())
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::raw(" ".repeat(padding)),
        Span::styled(
            hint.to_string(),
            Style::new().fg(theme.text_muted.to_color()),
        ),
    ])
}

/// One option row (`dialog-select.tsx:732-791`): the `●` current marker,
/// the gutter, title + description, footer right. The title column is
/// fixed at 6 (`paddingLeft 3` + the leading pad), or `1 +` marker +
/// gap + `3` when current/gutter.
fn option_line(
    theme: &Theme,
    option: &SelectOption,
    active: bool,
    muted: bool,
    width: u16,
) -> Line<'static> {
    let selected_fg = super::super::theme::selected_foreground(theme, Some(theme.primary));
    let bg = if active {
        Some(if option.bg_error {
            theme.error
        } else {
            theme.primary
        })
    } else {
        None
    };
    let with_bg = |style: Style| match bg {
        Some(bg) => style.bg(bg.to_color()),
        None => style,
    };
    let text_fg = if muted && (active || option.current) {
        theme.text_muted
    } else if active {
        selected_fg
    } else if option.current {
        theme.primary
    } else {
        theme.text
    };
    let muted_fg = if active && !muted {
        selected_fg
    } else {
        theme.text_muted
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    let current = option.current && option.gutter.is_none();
    if current {
        spans.push(Span::styled(" ", with_bg(Style::new())));
        spans.push(Span::styled(
            "●".to_string(),
            with_bg(Style::new().fg(text_fg.to_color())),
        ));
        spans.push(Span::styled("    ", with_bg(Style::new())));
    } else if let Some(gutter) = &option.gutter {
        spans.push(Span::styled(" ", with_bg(Style::new())));
        spans.push(Span::styled(
            gutter.clone(),
            with_bg(Style::new().fg(option.gutter_fg.unwrap_or(text_fg).to_color())),
        ));
        spans.push(Span::styled("    ", with_bg(Style::new())));
    } else {
        spans.push(Span::styled("      ", with_bg(Style::new())));
    }
    let title_style = Style::new()
        .fg(text_fg.to_color())
        .add_modifier(if active && !muted {
            ratatui::style::Modifier::BOLD
        } else {
            ratatui::style::Modifier::empty()
        });
    spans.push(Span::styled(option.title.clone(), with_bg(title_style)));
    if let Some(description) = &option.description {
        spans.push(Span::styled(
            format!(" {description}"),
            with_bg(Style::new().fg(muted_fg.to_color())),
        ));
    }
    if let Some(footer) = &option.footer {
        let used: usize = spans.iter().map(|span| span.content.chars().count()).sum();
        let padding = width.saturating_sub((used + footer.chars().count()) as u16);
        if padding > 0 {
            spans.push(Span::styled(
                " ".repeat(padding as usize),
                with_bg(Style::new()),
            ));
        }
        spans.push(Span::styled(
            footer.clone(),
            with_bg(Style::new().fg(muted_fg.to_color())),
        ));
    }
    Line::from(spans)
}

/// Render the filtered option window (plus category headers) into `lines`.
///
/// Returns the row layout — `(line index, filtered option index)` pairs —
/// for the mouse hit-testing of `dialog-select.tsx:640-676`.
pub fn render_options(
    select: &SelectState,
    view: &SelectView,
    theme: &Theme,
    lines: &mut Vec<Line<'static>>,
    width: u16,
    max_visible: usize,
) -> Vec<(usize, usize)> {
    let mut layout = Vec::new();
    let options = filter_options(&select.filter, view.options.clone());
    if options.is_empty() {
        lines.push(Line::styled(
            "    No results found",
            Style::new().fg(theme.text_muted.to_color()),
        ));
        return layout;
    }
    let mut category = String::new();
    // The scrollbox window (`scrollToSelection`,
    // dialog-select.tsx:311-342): render only the visible slice —
    // the selection can move past the fixed window.
    let start = select.scroll.min(options.len().saturating_sub(1));
    for (index, option) in options.iter().enumerate().skip(start).take(max_visible) {
        if let Some(group) = &option.category {
            if group != &category && !group.is_empty() {
                if !category.is_empty() {
                    // `paddingTop={1}` between category groups
                    // (dialog-select.tsx:621).
                    lines.push(Line::raw(""));
                }
                category = group.clone();
                lines.push(Line::styled(
                    format!("    {group}"),
                    Style::new()
                        .fg(theme.accent.to_color())
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ));
            }
        }
        layout.push((lines.len(), index));
        lines.push(option_line(
            theme,
            option,
            index == select.selected,
            view.action_focused,
            width,
        ));
    }
    layout
}

/// The footer action row (`dialog-select.tsx:717-728`, `526-555`): the
/// `title label` pairs, with the `focusedAction` highlighted.
pub fn render_actions(
    theme: &Theme,
    actions: &[(String, String)],
    focused: Option<usize>,
) -> Line<'static> {
    let selected_fg = super::super::theme::selected_foreground(theme, Some(theme.primary));
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw("    "));
    for (index, (title, label)) in actions.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let active = focused == Some(index);
        let (fg, mut style) = if active {
            (
                selected_fg,
                Style::new()
                    .bg(theme.primary.to_color())
                    .add_modifier(ratatui::style::Modifier::BOLD),
            )
        } else {
            (theme.text, Style::new())
        };
        style = style.fg(fg.to_color());
        spans.push(Span::styled(title.clone(), style));
        spans.push(Span::styled(
            format!(" {label}"),
            if active {
                style
            } else {
                Style::new().fg(theme.text_muted.to_color())
            },
        ));
    }
    Line::from(spans)
}

/// The filter input row (`<input placeholder="Search">`, `dialog-select.tsx:570-597`).
pub fn filter_line(theme: &Theme, select: &SelectState, placeholder: &str) -> Line<'static> {
    if select.filter.is_empty() {
        Line::from(Span::styled(
            format!("    {placeholder}"),
            Style::new().fg(theme.text_muted.to_color()),
        ))
    } else {
        Line::from(Span::styled(
            format!("    {}", select.filter),
            Style::new().fg(theme.text_muted.to_color()),
        ))
    }
}

/// Word-wrap `text` into `lines` at `width` columns.
pub fn wrap_text(text: &str, width: u16, lines: &mut Vec<Line<'static>>, style: Style) {
    let max = width.max(4) as usize;
    for raw in text.split('\n') {
        let mut current = String::new();
        for word in raw.split(' ') {
            if current.is_empty() {
                current = word.to_string();
            } else if current.chars().count() + 1 + word.chars().count() <= max {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(Line::styled(current.clone(), style));
                current = word.to_string();
            }
        }
        lines.push(Line::styled(current, style));
    }
}

/// Paint a padded block of `lines` over `area`.
pub fn paint(lines: &[Line<'static>], theme: &Theme, area: Rect, frame: &mut ratatui::Frame) {
    Paragraph::new(lines.to_vec())
        .style(Style::new().bg(theme.background_panel.to_color()))
        .render(area, frame.buffer_mut());
}

#[cfg(test)]
mod scroll_tests {
    use super::*;

    fn theme() -> crate::ui::theme::Theme {
        let app = crate::state::App::new(
            crate::config::TuiConfig::default(),
            Default::default(),
            None,
        );
        app.ui
            .theme
            .resolve(&app.state.kv)
            .expect("builtin theme resolves")
    }

    #[test]
    fn render_options_follows_the_scroll_window() {
        // A selection past the visible window must stay on screen —
        // the window scrolls, it does not strand the highlight
        // (`scrollToSelection`, dialog-select.tsx:311-342).
        let view = SelectView {
            title: "Models".to_string(),
            filter: false,
            options: (0..20)
                .map(|i| SelectOption {
                    title: format!("model-{i}"),
                    value: Some(format!("model-{i}")),
                    description: None,
                    category: None,
                    footer: None,
                    gutter: None,
                    gutter_fg: None,
                    current: false,
                    bg_error: false,
                })
                .collect(),
            actions: Vec::new(),
            action_focused: false,
        };
        let mut select = SelectState::default();
        select.move_by(15, 20, 8);
        let mut lines = Vec::new();
        render_options(&select, &view, &theme(), &mut lines, 80, 8);
        let text = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("model-15"), "selection missing: {text}");
        assert!(!text.contains("model-0"), "window did not scroll: {text}");
    }

    #[test]
    fn render_options_reports_the_row_layout() {
        // The layout maps each option row's line index — the mouse
        // hit-testing of `dialog-select.tsx:640-676`.
        let view = SelectView {
            title: "Models".to_string(),
            filter: false,
            options: vec![
                SelectOption::new("a").with_value("a"),
                SelectOption::new("b").with_value("b").with_category("Cat"),
                SelectOption::new("c").with_value("c").with_category("Cat"),
            ],
            actions: Vec::new(),
            action_focused: false,
        };
        let select = SelectState::default();
        let mut lines = Vec::new();
        let layout = render_options(&select, &view, &theme(), &mut lines, 80, 8);
        // First option row directly follows the first category header.
        assert_eq!(layout, vec![(0, 0), (2, 1), (3, 2)]);
    }

    #[test]
    fn max_visible_options_scales_with_the_terminal() {
        // `Math.floor(dimensions().height / 2) - 6`
        // (dialog-select.tsx:213).
        assert_eq!(max_visible_options(40), 14);
        assert_eq!(max_visible_options(24), 6);
        assert_eq!(max_visible_options(12), 1);
        assert_eq!(max_visible_options(2), 1);
    }
}
