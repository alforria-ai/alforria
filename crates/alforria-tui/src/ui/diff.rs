//! The diff renderer (M8.5) — `ui/diff.rs`.
//!
//! The TS reference renders `<diff view={…} showLineNumbers wrapMode={…}>`
//! with the theme's diff colors. This port parses the unified diff and
//! renders it either **unified** (single column — always when
//! `diff_style == "stacked"` or the width ≤ 120) or **split** (old and
//! new side by side). It also ports `getRevertDiffFiles`
//! (`util/revert-diff.ts`) for the revert-marker box.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::theme::Theme;

/// One parsed diff row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRow {
    Context {
        old_no: Option<u32>,
        new_no: Option<u32>,
        text: String,
    },
    Added {
        new_no: Option<u32>,
        text: String,
    },
    Removed {
        old_no: Option<u32>,
        text: String,
    },
    Hunk {
        text: String,
    },
}

/// Parse a unified diff. Everything before the first `@@` (the
/// `Index:` / `====` / `---` / `+++` / `diff --git` headers core emits)
/// is skipped; inside a hunk the header's line counts decide where it
/// ends, so a removed line that reads `-- x` stays a removal while the
/// next file's headers are skipped. `\ No newline at end of file`
/// markers are dropped.
pub fn parse(diff: &str) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    let mut old_no: u32 = 0;
    let mut new_no: u32 = 0;
    // The lines left in the current hunk (old side, new side); `None`
    // outside a hunk. An unparseable header runs to the next `@@`.
    let mut hunk: Option<(u32, u32)> = None;
    for raw in diff.lines() {
        if raw.starts_with("@@") {
            match parse_hunk_header(raw) {
                Some(header) => {
                    old_no = header.old_start;
                    new_no = header.new_start;
                    hunk = Some((header.old_lines, header.new_lines));
                }
                None => hunk = Some((u32::MAX, u32::MAX)),
            }
            rows.push(DiffRow::Hunk {
                text: raw.to_string(),
            });
            continue;
        }
        let Some((old_left, new_left)) = hunk.as_mut() else {
            continue;
        };
        if raw.starts_with('\\') {
            continue;
        }
        let mut chars = raw.chars();
        let sign = chars.next();
        let text = expand_tabs(chars.as_str());
        match sign {
            Some('+') => {
                rows.push(DiffRow::Added {
                    new_no: Some(new_no),
                    text,
                });
                new_no = new_no.saturating_add(1);
                *new_left = new_left.saturating_sub(1);
            }
            Some('-') => {
                rows.push(DiffRow::Removed {
                    old_no: Some(old_no),
                    text,
                });
                old_no = old_no.saturating_add(1);
                *old_left = old_left.saturating_sub(1);
            }
            // A context line; an empty line is a context line whose
            // leading space was stripped.
            Some(' ') | None => {
                rows.push(DiffRow::Context {
                    old_no: Some(old_no),
                    new_no: Some(new_no),
                    text,
                });
                old_no = old_no.saturating_add(1);
                new_no = new_no.saturating_add(1);
                *old_left = old_left.saturating_sub(1);
                *new_left = new_left.saturating_sub(1);
            }
            // Anything else ends the hunk (the next file's headers).
            Some(_) => {
                hunk = None;
                continue;
            }
        }
        if hunk.is_some_and(|(old, new)| old == 0 && new == 0) {
            hunk = None;
        }
    }
    rows
}

/// A parsed `@@ -a,b +c,d @@` header.
struct HunkHeader {
    old_start: u32,
    old_lines: u32,
    new_start: u32,
    new_lines: u32,
}

/// `@@ -3,4 +3,5 @@` → starts `(3, 3)`, counts `(4, 5)`. A missing
/// count is 1 (`@@ -1 +1 @@`). 1-based; a zero start renders as none.
fn parse_hunk_header(raw: &str) -> Option<HunkHeader> {
    let mut fields = raw.strip_prefix("@@")?.split_whitespace();
    let old = fields.next()?.strip_prefix('-')?;
    let new = fields.next()?.strip_prefix('+')?;
    let range = |range: &str| -> Option<(u32, u32)> {
        match range.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((range.parse().ok()?, 1)),
        }
    };
    let (old_start, old_lines) = range(old)?;
    let (new_start, new_lines) = range(new)?;
    Some(HunkHeader {
        old_start,
        old_lines,
        new_start,
        new_lines,
    })
}

/// Tabs expand to spaces before any width math (the terminal would
/// otherwise paint them as zero or one cell).
fn expand_tabs(text: &str) -> String {
    crate::ui::markdown::expand_tabs(text, 0).into_owned()
}

/// `DiffStyle` (`config/index.tsx:30-32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffStyle {
    Auto,
    Stacked,
}

/// The view resolution (`session/index.tsx:2395-2401`): stacked always
/// means unified; auto means split only when wide.
pub fn view_for(style: DiffStyle, width: u16) -> &'static str {
    if style == DiffStyle::Stacked || width <= 120 {
        "unified"
    } else {
        "split"
    }
}

/// `diff_wrap_mode` kv (`"word" | "none"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapMode {
    Word,
    None,
}

/// Render a unified diff at `width` columns.
pub fn render(
    diff: &str,
    view: &str,
    width: u16,
    wrap_mode: WrapMode,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let rows = parse(diff);
    if view == "split" {
        render_split(rows, width, wrap_mode, theme)
    } else {
        render_unified(rows, width, wrap_mode, theme)
    }
}

fn line_number(number: Option<u32>) -> String {
    match number {
        Some(number) if number > 0 => format!("{number:>4} "),
        _ => "     ".to_string(),
    }
}

/// Display-width of a string (`unicode_width`) — wide CJK and
/// emoji glyphs occupy two cells.
fn display_width(text: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    text.width()
}

fn trim_to(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    let truncated = take_width(text, width);
    if truncated.is_empty() {
        truncated
    } else {
        format!("{truncated}…")
    }
}

/// The longest prefix of `text` that fits `width` display cells.
fn take_width(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut consumed = 0usize;
    for (index, char) in text.char_indices() {
        let char_width = char.width().unwrap_or(0);
        if consumed + char_width > width {
            return text[..index].to_string();
        }
        consumed += char_width;
    }
    text.to_string()
}

fn wrap_to(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    use unicode_width::UnicodeWidthChar;
    let mut lines = Vec::new();
    let mut remaining = text;
    while display_width(remaining) > width {
        // The last space that fits the width budget is the break
        // candidate; `break_at` points one past it so the space is
        // dropped.
        let mut break_at = None;
        let mut consumed = 0usize;
        for (index, char) in remaining.char_indices() {
            if consumed + char.width().unwrap_or(0) > width {
                break;
            }
            consumed += char.width().unwrap_or(0);
            if char == ' ' {
                break_at = Some(index + char.len_utf8());
            }
        }
        match break_at {
            Some(position) if position > 0 => {
                lines.push(remaining[..position - 1].to_string());
                remaining = &remaining[position..];
            }
            _ => {
                let head = take_width(remaining, width);
                let cut = if head.is_empty() {
                    // A single glyph wider than the budget — consume it
                    // anyway or the loop never advances.
                    remaining
                        .char_indices()
                        .nth(1)
                        .map(|(index, _)| index)
                        .unwrap_or(remaining.len())
                } else {
                    head.len()
                };
                lines.push(remaining[..cut].to_string());
                remaining = &remaining[cut..];
            }
        }
    }
    lines.push(remaining.to_string());
    lines
}

fn content_rows(text: &str, width: usize, mode: WrapMode) -> Vec<String> {
    match mode {
        WrapMode::None => vec![trim_to(text, width)],
        WrapMode::Word => wrap_to(text, width),
    }
}

fn render_unified(
    rows: Vec<DiffRow>,
    width: u16,
    wrap_mode: WrapMode,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let budget = (width as usize).saturating_sub(12);
    for row in rows {
        match row {
            DiffRow::Hunk { text } => lines.push(Line::from(Span::styled(
                text,
                Style::new()
                    .fg(theme.diff_hunk_header.to_color())
                    .bg(theme.diff_context_bg.to_color()),
            ))),
            DiffRow::Context {
                old_no,
                new_no,
                text,
            } => lines.extend(styled_rows(
                &line_number(old_no),
                &line_number(new_no),
                " ",
                &text,
                theme.diff_context,
                theme.diff_context_bg,
                theme.diff_context,
                budget,
                wrap_mode,
            )),
            DiffRow::Added { new_no, text } => lines.extend(styled_rows(
                "     ",
                &line_number(new_no),
                "+",
                &text,
                theme.diff_highlight_added,
                theme.diff_added_bg,
                theme.diff_added,
                budget,
                wrap_mode,
            )),
            DiffRow::Removed { old_no, text } => lines.extend(styled_rows(
                &line_number(old_no),
                "     ",
                "-",
                &text,
                theme.diff_highlight_removed,
                theme.diff_removed_bg,
                theme.diff_removed,
                budget,
                wrap_mode,
            )),
        }
    }
    lines
}

#[allow(clippy::too_many_arguments)]
fn styled_rows(
    old: &str,
    new: &str,
    sign: &str,
    text: &str,
    sign_color: super::theme::Rgba,
    bg: super::theme::Rgba,
    _fg: super::theme::Rgba,
    budget: usize,
    wrap_mode: WrapMode,
) -> Vec<Line<'static>> {
    let sign_style = Style::new().fg(sign_color.to_color()).bg(bg.to_color());
    let content_style = Style::new().bg(bg.to_color());
    let mut out = Vec::new();
    for content in content_rows(text, budget, wrap_mode) {
        out.push(Line::from(vec![
            Span::styled(old.to_string(), sign_style),
            Span::styled(new.to_string(), sign_style),
            Span::styled(format!("{sign} "), sign_style),
            Span::styled(content, content_style),
        ]));
    }
    out
}

fn render_split(
    rows: Vec<DiffRow>,
    width: u16,
    wrap_mode: WrapMode,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let pane = (width.max(1) as usize).saturating_sub(1) / 2;
    let budget = pane.saturating_sub(9);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut old_buffer: Vec<(Option<u32>, String)> = Vec::new();
    let mut new_buffer: Vec<(Option<u32>, String)> = Vec::new();

    for row in rows {
        match row {
            DiffRow::Hunk { .. } => {
                flush_split_panes(&mut old_buffer, &mut new_buffer, &mut lines, pane, theme);
                lines.push(Line::from(Span::styled(
                    "─".repeat(width.max(1) as usize),
                    Style::new()
                        .fg(theme.diff_hunk_header.to_color())
                        .bg(theme.diff_context_bg.to_color()),
                )));
            }
            DiffRow::Context {
                old_no,
                new_no,
                text,
            } => {
                for content in content_rows(&text, budget, wrap_mode) {
                    old_buffer.push((old_no, content.clone()));
                    new_buffer.push((new_no, content));
                    flush_split_panes(&mut old_buffer, &mut new_buffer, &mut lines, pane, theme);
                }
            }
            DiffRow::Added { new_no, text } => {
                for content in content_rows(&text, budget, wrap_mode) {
                    new_buffer.push((new_no, content));
                }
            }
            DiffRow::Removed { old_no, text } => {
                for content in content_rows(&text, budget, wrap_mode) {
                    old_buffer.push((old_no, content));
                }
            }
        }
    }
    flush_split_panes(&mut old_buffer, &mut new_buffer, &mut lines, pane, theme);
    lines
}

/// One side of a split row.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SplitSide {
    Removed(Option<u32>, String),
    Added(Option<u32>, String),
}

/// Pair buffered removals against buffered additions; leftovers render
/// on their own side with the other side blank.
fn flush_split_panes(
    old_buffer: &mut Vec<(Option<u32>, String)>,
    new_buffer: &mut Vec<(Option<u32>, String)>,
    lines: &mut Vec<Line<'static>>,
    pane: usize,
    theme: &Theme,
) {
    while !old_buffer.is_empty() || !new_buffer.is_empty() {
        let left = old_buffer
            .first()
            .map(|(number, text)| SplitSide::Removed(*number, text.clone()));
        let right = new_buffer
            .first()
            .map(|(number, text)| SplitSide::Added(*number, text.clone()));
        if left.is_none() && right.is_none() {
            break;
        }
        if left.is_some() {
            old_buffer.remove(0);
        }
        if right.is_some() {
            new_buffer.remove(0);
        }
        let divider_fg = match &right {
            Some(_) => theme.diff_added,
            None => theme.diff_hunk_header,
        };
        lines.push(Line::from(vec![
            split_pane_span(left, pane, theme),
            Span::styled("│", Style::new().fg(divider_fg.to_color())),
            split_pane_span(right, pane, theme),
        ]));
    }
}

fn split_pane_span(side: Option<SplitSide>, pane: usize, theme: &Theme) -> Span<'static> {
    let (number, sign, text, number_bg) = match side {
        Some(SplitSide::Removed(number, text)) => {
            (number, "-", text, theme.diff_removed_line_number_bg)
        }
        Some(SplitSide::Added(number, text)) => {
            (number, "+", text, theme.diff_added_line_number_bg)
        }
        None => {
            return Span::raw(" ".repeat(pane));
        }
    };
    let prefix = match number {
        Some(number) if number > 0 => format!("{number:>4} {sign} "),
        _ => format!("     {sign} "),
    };
    let content = trim_to(&text, pane.saturating_sub(prefix.chars().count()));
    Span::styled(
        format!("{prefix}{content}"),
        Style::new()
            .fg(theme.diff_line_number.to_color())
            .bg(number_bg.to_color()),
    )
}

/// `getRevertDiffFiles` (`util/revert-diff.ts`) — `{filename,
/// additions, deletions}` per file patch, with `a/`/`b/` prefixes
/// stripped.
pub fn revert_files(diff: &str) -> Vec<RevertFile> {
    let mut files = Vec::new();
    let mut current: Option<RevertFile> = None;
    // Lines left in the current hunk: `---`/`+++` are file headers only
    // outside one.
    let mut hunk: Option<(u32, u32)> = None;
    for raw in diff.lines() {
        if raw.starts_with("@@") {
            hunk = Some(
                parse_hunk_header(raw)
                    .map(|header| (header.old_lines, header.new_lines))
                    .unwrap_or((u32::MAX, u32::MAX)),
            );
            continue;
        }
        if let Some((old_left, new_left)) = hunk.as_mut() {
            match raw.chars().next() {
                Some('+') => {
                    *new_left = new_left.saturating_sub(1);
                    if let Some(file) = current.as_mut() {
                        file.additions += 1;
                    }
                }
                Some('-') => {
                    *old_left = old_left.saturating_sub(1);
                    if let Some(file) = current.as_mut() {
                        file.deletions += 1;
                    }
                }
                Some('\\') => {}
                Some(' ') | None => {
                    *old_left = old_left.saturating_sub(1);
                    *new_left = new_left.saturating_sub(1);
                }
                Some(_) => hunk = None,
            }
            if hunk.is_some_and(|(old, new)| old == 0 && new == 0) {
                hunk = None;
            }
            continue;
        }
        if raw.starts_with("+++ ") {
            let name = raw.strip_prefix("+++ ").unwrap_or("").trim();
            let name = name.strip_prefix("b/").unwrap_or(name);
            if let Some(existing) = current.take() {
                files.push(existing);
            }
            if !name.is_empty() && name != "/dev/null" {
                current = Some(RevertFile {
                    filename: name.to_string(),
                    additions: 0,
                    deletions: 0,
                });
            }
        }
    }
    if let Some(file) = current {
        files.push(file);
    }
    files
}

/// One `getRevertDiffFiles` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertFile {
    pub filename: String,
    pub additions: usize,
    pub deletions: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        let mut kv = crate::state::kv::Kv::in_memory();
        crate::ui::theme::ThemeStore::init(&mut kv, None)
            .resolve(&kv)
            .unwrap()
    }

    const DIFF: &str = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 context
-removed line
 context two
+added line
+another addition
";

    #[test]
    fn parses_unified_diff() {
        let rows = parse(DIFF);
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, DiffRow::Added { .. }))
                .count(),
            2
        );
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, DiffRow::Removed { .. }))
                .count(),
            1
        );
        match &rows[0] {
            DiffRow::Hunk { text } => assert_eq!(text, "@@ -1,3 +1,4 @@"),
            other => panic!("expected hunk header, got {other:?}"),
        }
        match &rows[1] {
            DiffRow::Context { new_no, .. } => assert_eq!(*new_no, Some(1)),
            other => panic!("expected context, got {other:?}"),
        }
    }

    #[test]
    fn hunk_header_numbers() {
        let rows = parse(DIFF);
        // `@@ -1,3 +1,4 @@` starts both sides at line 1.
        match &rows[2] {
            DiffRow::Removed { old_no, .. } => assert_eq!(*old_no, Some(2)),
            other => panic!("expected removal, got {other:?}"),
        }
        match &rows[4] {
            DiffRow::Added { new_no, .. } => assert_eq!(*new_no, Some(3)),
            other => panic!("expected addition, got {other:?}"),
        }
    }

    #[test]
    fn view_resolution_follows_diff_style_and_width() {
        assert_eq!(view_for(DiffStyle::Auto, 80), "unified");
        assert_eq!(view_for(DiffStyle::Auto, 120), "unified");
        assert_eq!(view_for(DiffStyle::Auto, 121), "split");
        assert_eq!(view_for(DiffStyle::Stacked, 200), "unified");
    }

    #[test]
    fn unified_renders_signs_and_numbers() {
        let lines = render(DIFF, "unified", 80, WrapMode::None, &theme());
        let text: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(text[0], "@@ -1,3 +1,4 @@");
        assert_eq!(text[1], "   1    1   context");
        assert_eq!(text[2], "   2      - removed line");
        assert_eq!(text[4], "        3 + added line");
        assert!(text.iter().any(|l| l.ends_with("+ another addition")));
    }

    #[test]
    fn split_panes_render_side_by_side() {
        let lines = render(DIFF, "split", 160, WrapMode::None, &theme());
        assert!(lines.len() >= 4);
        for line in &lines {
            if line.spans.len() == 1 {
                // the hunk separator row
                continue;
            }
            assert!(
                line.spans.iter().any(|span| span.content == "│"),
                "split rows have a divider"
            );
        }
    }

    #[test]
    fn revert_files_count_additions_and_deletions() {
        let files = revert_files(DIFF);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "src/lib.rs");
        assert_eq!(files[0].additions, 2);
        assert_eq!(files[0].deletions, 1);
    }

    /// `createTwoFilesPatch` as core's edit tool emits it
    /// (`alforria-core/src/tool/diff.rs`), with a removed line that
    /// itself reads `-- x` and a no-newline marker.
    const CORE_DIFF: &str = concat!(
        "Index: /p/a.rs\n",
        "===================================================================\n",
        "--- /p/a.rs\n",
        "+++ /p/a.rs\n",
        "@@ -1,3 +1,3 @@\n",
        " keep\n",
        "--- x\n",
        "+\tnew 你好\n",
        " tail\n",
        "\\ No newline at end of file\n",
    );

    #[test]
    fn core_edit_diff_skips_headers() {
        let rows = parse(CORE_DIFF);
        assert_eq!(
            rows,
            vec![
                DiffRow::Hunk {
                    text: "@@ -1,3 +1,3 @@".into()
                },
                DiffRow::Context {
                    old_no: Some(1),
                    new_no: Some(1),
                    text: "keep".into()
                },
                DiffRow::Removed {
                    old_no: Some(2),
                    text: "-- x".into()
                },
                DiffRow::Added {
                    new_no: Some(2),
                    text: "    new 你好".into()
                },
                DiffRow::Context {
                    old_no: Some(3),
                    new_no: Some(3),
                    text: "tail".into()
                },
            ]
        );
        let text: Vec<String> = render(CORE_DIFF, "unified", 80, WrapMode::Word, &theme())
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(text.iter().all(|row| !row.contains("Index:")
            && !row.contains("====")
            && !row.contains("No newline")));
    }

    #[test]
    fn multi_file_patches_skip_the_next_headers() {
        let diff =
            "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n--- a/y\n+++ b/y\n@@ -0,0 +1,2 @@\n+c\n+d\n";
        let rows = parse(diff);
        assert_eq!(rows.len(), 6, "{rows:?}");
        assert!(matches!(&rows[3], DiffRow::Hunk { .. }), "{rows:?}");
        assert_eq!(
            rows[4],
            DiffRow::Added {
                new_no: Some(1),
                text: "c".into()
            }
        );
        let files = revert_files(diff);
        assert_eq!(files.len(), 2);
        assert_eq!((files[0].additions, files[0].deletions), (1, 1));
        assert_eq!((files[1].additions, files[1].deletions), (2, 0));
        // Core's `Index:` headers don't count either.
        let files = revert_files(CORE_DIFF);
        assert_eq!((files[0].additions, files[0].deletions), (1, 1));
    }

    #[test]
    fn trim_and_wrap_are_display_width_aware() {
        // A CJK string of 4 chars occupies 8 cells.
        let wide = "你好世界";
        assert_eq!(display_width(wide), 8);

        // Truncation is by display cells, not chars.
        assert_eq!(trim_to(wide, 8), wide);
        assert_eq!(trim_to(wide, 4), "你好…");
        assert_eq!(trim_to("abcd", 2), "ab…");

        // Wrapping breaks at spaces by display width...
        let wrapped = wrap_to("你 好 world", 4);
        assert_eq!(wrapped, vec!["你", "好", "worl", "d"]);
        // ...and hard-breaks wide runs without spaces.
        let wrapped = wrap_to("你好世界!", 4);
        assert_eq!(wrapped, vec!["你好", "世界", "!"]);
    }
}
