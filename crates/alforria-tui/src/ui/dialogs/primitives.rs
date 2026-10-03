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

/// The option list in display order — `filtered` → `grouped` → `flat`
/// (`dialog-select.tsx:154-215`): filtered, then grouped by category in
/// first-seen order, so a category never renders twice and the selection
/// index follows the rows on screen. `flat` dialogs (model, variant) drop
/// the grouping while filtering and show the category as the footer
/// (`flatten()`, `dialog-select.tsx:190,693`).
pub fn arrange(needle: &str, options: Vec<SelectOption>, flat: bool) -> Vec<SelectOption> {
    let filtered = filter_options(needle, options);
    if flat && !needle.is_empty() {
        return filtered
            .into_iter()
            .map(|option| SelectOption {
                footer: option.category.clone().or(option.footer),
                category: None,
                ..option
            })
            .collect();
    }
    let mut groups: Vec<(String, Vec<SelectOption>)> = Vec::new();
    for option in filtered {
        let category = option.category.clone().unwrap_or_default();
        match groups.iter_mut().find(|(name, _)| *name == category) {
            Some((_, group)) => group.push(option),
            None => groups.push((category, vec![option])),
        }
    }
    groups.into_iter().flat_map(|(_, group)| group).collect()
}

/// One row of the select scrollbox (`dialog-select.tsx:617-700`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// The `paddingTop={1}` above every category header but the first.
    Gap,
    Category(String),
    /// An option, by its index in the [`arrange`]d list.
    Option(usize),
}

/// The scrollbox rows of an [`arrange`]d option list: a header opens
/// each named category.
pub fn rows(options: &[SelectOption]) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut previous: Option<&str> = None;
    for (index, option) in options.iter().enumerate() {
        let category = option.category.as_deref().unwrap_or("");
        if previous != Some(category) {
            if !category.is_empty() {
                if index > 0 {
                    rows.push(Row::Gap);
                }
                rows.push(Row::Category(category.to_string()));
            }
            previous = Some(category);
        }
        rows.push(Row::Option(index));
    }
    rows
}

fn option_count(rows: &[Row]) -> usize {
    rows.iter()
        .filter(|row| matches!(row, Row::Option(_)))
        .count()
}

fn row_of(rows: &[Row], index: usize) -> Option<usize> {
    rows.iter().position(|row| *row == Row::Option(index))
}

/// The `DialogSelect` interaction state: `selected`, the filter input and
/// the scrollbox offset — the first visible row (`dialog-select.tsx:90-94`).
#[derive(Debug, Default, Clone)]
pub struct SelectState {
    pub selected: usize,
    pub filter: String,
    pub scroll: usize,
}

impl SelectState {
    /// `move()` (`dialog-select.tsx:290-297`) — wrap-around, then the
    /// keyboard's centered `scrollToSelection`.
    pub fn move_by(&mut self, direction: i64, rows: &[Row], max_visible: usize) {
        let len = option_count(rows);
        if len == 0 {
            return;
        }
        let next = self.selected.min(len - 1) as i64 + direction;
        self.selected = if next < 0 {
            len - 1
        } else if next >= len as i64 {
            0
        } else {
            next as usize
        };
        self.center(rows, max_visible);
    }

    /// `moveTo()` (`dialog-select.tsx:299-309`) — the minimal scroll that
    /// brings the selection into view.
    pub fn move_to(&mut self, index: usize, rows: &[Row], max_visible: usize) {
        self.selected = index;
        self.reveal(rows, max_visible);
    }

    /// `scrollToSelection(true)` — the selection sits mid-window.
    pub fn center(&mut self, rows: &[Row], max_visible: usize) {
        self.clamp(rows, max_visible);
        let Some(row) = row_of(rows, self.selected) else {
            return;
        };
        self.scroll = row.saturating_sub(max_visible.max(1) / 2);
        self.clamp(rows, max_visible);
    }

    /// `scrollToSelection(false)` (`dialog-select.tsx:311-342`): scroll
    /// just enough; the first option scrolls to the very top so its
    /// category header shows — when the window has room for both.
    pub fn reveal(&mut self, rows: &[Row], max_visible: usize) {
        self.clamp(rows, max_visible);
        let Some(row) = row_of(rows, self.selected) else {
            return;
        };
        let height = max_visible.max(1);
        if self.selected == 0 && row < height {
            self.scroll = 0;
        } else if row < self.scroll {
            self.scroll = row;
        } else if row >= self.scroll + height {
            self.scroll = row + 1 - height;
        }
    }

    /// Keep the selection on an existing option and the window inside the
    /// rows — the list can shrink under it (filtering, live updates) and
    /// the window can grow (a resize).
    pub fn clamp(&mut self, rows: &[Row], max_visible: usize) {
        self.selected = self.selected.min(option_count(rows).saturating_sub(1));
        self.scroll = self
            .scroll
            .min(rows.len().saturating_sub(max_visible.max(1)));
    }
}

/// The longest prefix of `text` within `width` display columns.
fn prefix(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut used = 0;
    for char in text.chars() {
        let char_width = char.width().unwrap_or(0);
        if used + char_width > width {
            break;
        }
        out.push(char);
        used += char_width;
    }
    out
}

/// `text` cut to `width` display columns, `…` marking the cut.
pub fn fit(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", prefix(text, width - 1))
}

/// `line` cut to `width` columns, `…` marking the cut — nothing a dialog
/// draws may run past its frame.
pub fn fit_line(line: Line<'static>, width: usize) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;
    if line.width() <= width {
        return line;
    }
    let budget = width.saturating_sub(1);
    let mut spans = Vec::new();
    let mut used = 0;
    for span in line.spans {
        let span_width = span.content.width();
        if used + span_width <= budget {
            used += span_width;
            spans.push(span);
            continue;
        }
        let cut = prefix(&span.content, budget - used);
        if width > 0 {
            spans.push(Span::styled(format!("{cut}…"), span.style));
        }
        break;
    }
    Line::from(spans).style(line.style)
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

/// The shared header row of the select dialogs: bold title left, muted
/// close hint right (`paddingLeft/Right 4`, `justifyContent:
/// space-between`, `dialog-select.tsx:557-568`).
pub fn header_line(theme: &Theme, title: &str, hint: &str, width: u16) -> Line<'static> {
    header_line_padded(theme, title, hint, width, 4)
}

/// [`header_line`] with the `paddingLeft/Right 2` of the alert, confirm,
/// prompt and help dialogs (`dialog-alert.tsx:30-38`). A long title gives
/// way to the hint: it is cut with `…`, the hint stays.
pub fn header_line_padded(
    theme: &Theme,
    title: &str,
    hint: &str,
    width: u16,
    pad: usize,
) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;
    let room = (width as usize).saturating_sub(2 * pad + hint.width() + 1);
    let title = fit(title, room);
    let padding = (width as usize).saturating_sub(2 * pad + title.width() + hint.width());
    Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(
            title,
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

/// One option row (`dialog-select.tsx:672-700,732-791`). The row box spans
/// the scrollbox between its 1-column paddings, so the selection bar does
/// too; inside it, `paddingLeft 3` (1 beside the `●` current marker or the
/// gutter) + the title's own `paddingLeft 3`, the footer right against
/// `paddingRight 3`. The title (`Locale.truncate(title, 61)`) and the
/// description give way to the footer and are cut with `…`.
fn option_line(
    theme: &Theme,
    option: &SelectOption,
    active: bool,
    muted: bool,
    width: u16,
) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;
    const LEAD: usize = 6;
    const TRAIL: usize = 3;
    let inner = (width as usize).saturating_sub(2);
    let footer = option
        .footer
        .clone()
        .filter(|footer| !footer.is_empty() && LEAD + footer.width() + 2 + TRAIL <= inner);
    let room =
        inner.saturating_sub(LEAD + TRAIL + footer.as_ref().map_or(0, |footer| footer.width() + 1));
    let title = fit(&fit(&option.title, 61), room);
    let description = option
        .description
        .as_deref()
        .map(|description| fit(&format!(" {description}"), room - title.width()))
        .filter(|description| description.width() > 1);
    let option = SelectOption {
        title,
        description: description.map(|description| description[1..].to_string()),
        footer,
        ..option.clone()
    };
    let mut line = option_spans(theme, &option, active, muted, inner as u16);
    line.spans.insert(0, Span::raw(" "));
    line
}

fn option_spans(
    theme: &Theme,
    option: &SelectOption,
    active: bool,
    muted: bool,
    width: u16,
) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;
    let selected_fg = super::super::theme::selected_foreground(theme, Some(theme.primary));
    // A focused footer action mutes the selection bar
    // (`dialog-select.tsx:679-684`).
    let bg = if active {
        Some(if muted {
            theme.background_element
        } else if option.bg_error {
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
    // The bar fills the row; the footer sits right, before `paddingRight 3`.
    let used: usize = spans.iter().map(|span| span.content.width()).sum();
    let footer = option.footer.as_deref().unwrap_or_default();
    let trail = if footer.is_empty() { 0 } else { 3 };
    let padding = (width as usize).saturating_sub(used + footer.width() + trail);
    spans.push(Span::styled(" ".repeat(padding), with_bg(Style::new())));
    if !footer.is_empty() {
        spans.push(Span::styled(
            footer.to_string(),
            with_bg(Style::new().fg(muted_fg.to_color())),
        ));
        spans.push(Span::styled("   ", with_bg(Style::new())));
    }
    Line::from(spans)
}

/// Render the scrollbox window of an [`arrange`]d option list (category
/// headers included) into `lines` — at most `max_visible` rows from
/// `select.scroll`, the `maxHeight` of `dialog-select.tsx:213,606`.
///
/// Returns the row layout — `(line index, option index)` pairs — for the
/// mouse hit-testing of `dialog-select.tsx:640-676`.
pub fn render_options(
    select: &SelectState,
    view: &SelectView,
    theme: &Theme,
    lines: &mut Vec<Line<'static>>,
    width: u16,
    max_visible: usize,
) -> Vec<(usize, usize)> {
    let mut layout = Vec::new();
    let options = &view.options;
    if options.is_empty() {
        // The `emptyView` fallback (`dialog-select.tsx:600-606`).
        lines.push(Line::styled(
            "    No results found",
            Style::new().fg(theme.text_muted.to_color()),
        ));
        return layout;
    }
    let rows = rows(options);
    let height = max_visible.max(1).min(rows.len());
    let start = select.scroll.min(rows.len() - height);
    let selected = select.selected.min(options.len() - 1);
    for row in &rows[start..start + height] {
        match row {
            Row::Gap => lines.push(Line::raw("")),
            Row::Category(category) => lines.push(Line::styled(
                format!("    {}", fit(category, (width as usize).saturating_sub(8))),
                Style::new()
                    .fg(theme.accent.to_color())
                    .add_modifier(ratatui::style::Modifier::BOLD),
            )),
            Row::Option(index) => {
                layout.push((lines.len(), *index));
                lines.push(option_line(
                    theme,
                    &options[*index],
                    *index == selected,
                    view.action_focused,
                    width,
                ));
            }
        }
    }
    layout
}

/// The footer action row (`dialog-select.tsx:717-728`, `526-555`): the
/// `title label` pairs, with the `focusedAction` highlighted. Pairs that
/// do not fit the row are left out whole rather than cut mid-word.
pub fn render_actions(
    theme: &Theme,
    actions: &[(String, String)],
    focused: Option<usize>,
    width: u16,
) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;
    let selected_fg = super::super::theme::selected_foreground(theme, Some(theme.primary));
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw("    "));
    // `paddingLeft 4`, `paddingRight 2`.
    let mut used = 4;
    for (index, (title, label)) in actions.iter().enumerate() {
        let gap = if index > 0 { 2 } else { 0 };
        let item = title.width() + 1 + label.width();
        if used + gap + item + 2 > width as usize {
            break;
        }
        used += gap + item;
        if gap > 0 {
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

/// The input cursor block (`cursorColor`) of the dialog inputs.
pub fn cursor_style(theme: &Theme, color: crate::ui::theme::Rgba) -> Style {
    Style::new()
        .fg(theme.background_panel.to_color())
        .bg(color.to_color())
}

/// One input row: the text (its tail when longer than the row, so the
/// cursor stays in view) and the block cursor after it, or the
/// placeholder under the cursor while empty.
pub fn input_line(
    text: &str,
    placeholder: &str,
    text_style: Style,
    placeholder_style: Style,
    cursor: Style,
    pad: usize,
    width: u16,
) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;
    let room = (width as usize).saturating_sub(2 * pad + 1);
    if text.is_empty() {
        let mut chars = placeholder.chars();
        let first = chars
            .next()
            .map(String::from)
            .unwrap_or_else(|| " ".to_string());
        return Line::from(vec![
            Span::raw(" ".repeat(pad)),
            Span::styled(first, cursor),
            Span::styled(fit(chars.as_str(), room), placeholder_style),
        ]);
    }
    let mut tail: String = text.to_string();
    while tail.width() > room {
        tail.remove(0);
    }
    Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(tail, text_style),
        Span::styled(" ", cursor),
    ])
}

/// The filter input row (`<input placeholder="Search">`,
/// `dialog-select.tsx:570-597`): typed text muted, the cursor primary.
pub fn filter_line(
    theme: &Theme,
    select: &SelectState,
    placeholder: &str,
    width: u16,
) -> Line<'static> {
    let muted = Style::new().fg(theme.text_muted.to_color());
    input_line(
        &select.filter,
        placeholder,
        muted,
        muted,
        cursor_style(theme, theme.primary),
        4,
        width,
    )
}

/// Word-wrap `text` into `lines` at `width` columns; a word longer than
/// the row (a URL, a path) is broken across rows instead of running off.
pub fn wrap_text(text: &str, width: u16, lines: &mut Vec<Line<'static>>, style: Style) {
    wrap_indented(text, width, 0, lines, style);
}

/// [`wrap_text`] with every row indented by `indent` columns.
pub fn wrap_indented(
    text: &str,
    width: u16,
    indent: usize,
    lines: &mut Vec<Line<'static>>,
    style: Style,
) {
    use unicode_width::UnicodeWidthStr;
    let max = width.max(4) as usize;
    let pad = " ".repeat(indent);
    let mut push = |row: &str| lines.push(Line::styled(format!("{pad}{row}"), style));
    for raw in text.split('\n') {
        let mut current = String::new();
        for word in raw.split(' ') {
            let mut word = word.to_string();
            if !current.is_empty() && current.width() + 1 + word.width() <= max {
                current.push(' ');
                current.push_str(&word);
                continue;
            }
            if !current.is_empty() {
                push(&current);
            }
            while word.width() > max {
                let head = prefix(&word, max);
                push(&head);
                word = word[head.len()..].to_string();
            }
            current = word;
        }
        push(&current);
    }
}

/// Paint `lines` on the opaque panel `area`, below its `paddingTop={1}`
/// row (`dialog.tsx:58-60`). The area is cleared first, so nothing behind
/// the dialog shows between or after the spans, and every line is cut to
/// the panel width.
pub fn paint(lines: &[Line<'static>], theme: &Theme, area: Rect, frame: &mut ratatui::Frame) {
    ratatui::widgets::Clear.render(area, frame.buffer_mut());
    ratatui::widgets::Block::new()
        .style(Style::new().bg(theme.background_panel.to_color()))
        .render(area, frame.buffer_mut());
    let lines: Vec<Line<'static>> = lines
        .iter()
        .map(|line| fit_line(line.clone(), area.width as usize))
        .collect();
    let body = Rect {
        y: area.y + PADDING_TOP,
        height: area.height.saturating_sub(PADDING_TOP),
        ..area
    };
    Paragraph::new(lines)
        .style(Style::new().bg(theme.background_panel.to_color()))
        .render(body, frame.buffer_mut());
}

/// The panel's `paddingTop={1}` (`dialog.tsx:58-60`).
pub const PADDING_TOP: u16 = 1;

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
        select.move_by(15, &rows(&view.options), 8);
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
        // The second group's header gets its `paddingTop={1}` gap
        // (`dialog-select.tsx:621`); its options follow the header.
        assert_eq!(layout, vec![(0, 0), (3, 1), (4, 2)]);
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
