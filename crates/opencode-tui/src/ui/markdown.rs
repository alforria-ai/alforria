//! The hand-rolled minimal markdown renderer (M8.5) — `ui/markdown.rs`.
//!
//! The TS reference renders `<markdown>` (OpenTUI) with
//! `syntaxStyle={syntax()}`; this port hand-rolls the constructs the
//! transcript actually exercises: headings, emphasis/strong/inline
//! code, fenced code blocks (with the fence language), lists,
//! blockquotes, gfm grid tables and links as text. It is
//! **streaming-safe**: an unterminated fence renders as a code block
//! running to the end of the partial input.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::{Rgba, Theme};

/// Render `content` into styled lines, word-wrapped to `width`.
pub fn render(content: &str, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    blocks(content)
        .into_iter()
        .flat_map(|block| render_block(block, theme, width))
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
enum Block {
    Paragraph(String),
    Heading(u8, String),
    Code {
        language: Option<String>,
        lines: Vec<String>,
    },
    ListItem {
        depth: usize,
        ordered: Option<u64>,
        text: String,
    },
    Quote(String),
    Table(Vec<Vec<String>>),
    Rule,
}

/// Split into blocks. An open fence at EOF flushes as a code block —
/// the streaming-safe behavior.
fn blocks(content: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut paragraph: Option<String> = None;
    let mut quote: Option<String> = None;
    let mut code: Option<(Option<String>, Vec<String>)> = None;

    let lines: Vec<&str> = content.lines().collect();
    let mut index = 0usize;
    while index < lines.len() {
        let raw = lines[index];
        let trimmed = raw.trim_start();
        if let Some((language, lines)) = code.as_mut() {
            let close = match &raw.trim() {
                fence if fence.starts_with("```") && fence.chars().all(|c| c == '`') => Some(3),
                fence if fence.starts_with("~~~") && fence.chars().all(|c| c == '~') => Some(3),
                _ => None,
            };
            if close.is_some() {
                out.push(Block::Code {
                    language: language.take(),
                    lines: std::mem::take(lines),
                });
                code = None;
            } else {
                lines.push(raw.to_string());
            }
            index += 1;
            continue;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            flush_paragraph(&mut out, &mut paragraph);
            flush_quote(&mut out, &mut quote);
            let marker = if trimmed.starts_with("```") { '`' } else { '~' };
            let fence_len = trimmed.chars().take_while(|c| *c == marker).count();
            let info = trimmed[fence_len..].trim();
            let language = info.split(',').next().unwrap_or("").trim().to_string();
            code = Some(((!language.is_empty()).then_some(language), Vec::new()));
            index += 1;
            continue;
        }
        // A GFM table opens with any pipe row whose next line is a
        // delimiter row — the outer pipes are optional
        // (`mdast-util-gfm-table`).
        if trimmed.contains('|')
            && lines
                .get(index + 1)
                .is_some_and(|next| is_delimiter_row(next.trim_start()))
        {
            flush_paragraph(&mut out, &mut paragraph);
            flush_quote(&mut out, &mut quote);
            let mut rows = vec![row_cells(trimmed), row_cells(lines[index + 1].trim_start())];
            index += 2;
            while let Some(next) = lines.get(index) {
                let next_trimmed = next.trim_start();
                if next_trimmed.is_empty() || !next_trimmed.contains('|') {
                    break;
                }
                rows.push(row_cells(next_trimmed));
                index += 1;
            }
            out.push(Block::Table(rows));
            continue;
        }
        if trimmed.is_empty() {
            flush_paragraph(&mut out, &mut paragraph);
            flush_quote(&mut out, &mut quote);
            index += 1;
            continue;
        }
        if let Some(block) = structural_block(trimmed, raw) {
            flush_paragraph(&mut out, &mut paragraph);
            match block {
                Block::Quote(text) => {
                    flush_quote(&mut out, &mut quote);
                    let merged = quote.get_or_insert_with(String::new);
                    if !merged.is_empty() {
                        merged.push('\n');
                    }
                    merged.push_str(&text);
                }
                Block::Code { .. } => unreachable!(),
                other => {
                    flush_quote(&mut out, &mut quote);
                    out.push(other);
                }
            }
            index += 1;
            continue;
        }
        flush_quote(&mut out, &mut quote);
        let merged = paragraph.get_or_insert_with(String::new);
        if !merged.is_empty() {
            merged.push('\n');
        }
        merged.push_str(raw.trim());
        index += 1;
    }

    if let Some((language, rows)) = code {
        out.push(Block::Code {
            language,
            lines: rows,
        });
    }
    flush_paragraph(&mut out, &mut paragraph);
    flush_quote(&mut out, &mut quote);
    out
}

/// `#`-headings, rules, quotes, table rows and list items — every
/// block decided by one trimmed line.
fn structural_block(trimmed: &str, raw: &str) -> Option<Block> {
    if let Some(block) = heading(trimmed) {
        return Some(block);
    }
    if is_rule(trimmed) {
        return Some(Block::Rule);
    }
    if let Some(text) = trimmed.strip_prefix('>') {
        return Some(Block::Quote(text.trim_start().to_string()));
    }
    list_item(trimmed, raw).map(|(depth, ordered, text)| Block::ListItem {
        depth,
        ordered,
        text,
    })
}

fn heading(trimmed: &str) -> Option<Block> {
    let hashes = trimmed.len() - trimmed.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if rest.starts_with(' ') {
        Some(Block::Heading(hashes as u8, rest.trim().to_string()))
    } else {
        None
    }
}

fn is_rule(trimmed: &str) -> bool {
    let Some(first) = trimmed.chars().next() else {
        return false;
    };
    if !matches!(first, '-' | '*' | '_') {
        return false;
    }
    trimmed.chars().all(|c| c == first) && trimmed.len() >= 3
}

/// A GFM delimiter row (`mdast-util-gfm-table`): optional outer pipes,
/// every cell `:?-+:?` (or `-` alone).
fn is_delimiter_row(trimmed: &str) -> bool {
    let inner = trimmed.trim_start_matches('|').trim_end_matches('|');
    if !trimmed.contains('|') || inner.is_empty() {
        return false;
    }
    inner.split('|').all(|cell| is_delimiter_cell(cell.trim()))
}

fn is_delimiter_cell(cell: &str) -> bool {
    let dashes = cell.trim_matches(':');
    !dashes.is_empty() && dashes.chars().all(|c| c == '-')
}

/// The cells of a table row — split on `|`, outer pipes optional.
fn row_cells(trimmed: &str) -> Vec<String> {
    trimmed
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

fn list_item<'a>(trimmed: &'a str, raw: &'a str) -> Option<(usize, Option<u64>, String)> {
    let bullet = trimmed.chars().next()?;
    if !matches!(bullet, '-' | '*' | '+') {
        let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
        let number: u64 = trimmed[..digits].parse().ok()?;
        if !trimmed[digits..].starts_with(". ") {
            return None;
        }
        return Some((
            indent(raw),
            Some(number),
            trimmed[digits + 2..].trim().to_string(),
        ));
    }
    if !trimmed[1..].starts_with(' ') {
        return None;
    }
    let text = trimmed[1..].trim();
    if text.is_empty() {
        return None;
    }
    Some((indent(raw), None, text.to_string()))
}

/// List nesting: two leading spaces per level.
fn indent(raw: &str) -> usize {
    let spaces = raw.chars().take_while(|c| *c == ' ').count();
    spaces / 2
}

fn flush_paragraph(out: &mut Vec<Block>, paragraph: &mut Option<String>) {
    if let Some(text) = paragraph.take() {
        out.push(Block::Paragraph(text));
    }
}

fn flush_quote(out: &mut Vec<Block>, quote: &mut Option<String>) {
    if let Some(text) = quote.take() {
        out.push(Block::Quote(text));
    }
}

// ------------------------------------------------------------- render

fn render_block(block: Block, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    match block {
        Block::Heading(_, text) => {
            let style = Style::new()
                .fg(theme.markdown_heading.to_color())
                .add_modifier(Modifier::BOLD);
            inline_wrap(&text, width, Some(style), theme)
        }
        Block::Paragraph(text) => {
            let style = Style::new().fg(theme.markdown_text.to_color());
            inline_wrap(&text, width, Some(style), theme)
        }
        Block::Quote(text) => {
            let style = Style::new().fg(theme.markdown_block_quote.to_color());
            let mut lines = Vec::new();
            for row in text.split('\n') {
                let quote_style = Style::new().fg(theme.markdown_block_quote.to_color());
                let mut spans = vec![Span::styled("> ", quote_style)];
                spans.extend(inline_spans(row, theme, style));
                lines.extend(wrap_spans(spans, width));
            }
            lines
        }
        Block::Code { lines, .. } => lines
            .into_iter()
            .flat_map(|row| {
                let style = Style::new().fg(theme.markdown_code_block.to_color());
                wrap_spans(vec![Span::styled(row, style)], width)
            })
            .collect(),
        Block::ListItem {
            depth,
            ordered,
            text,
        } => {
            let (bullet, color) = match ordered {
                Some(number) => (format!("{number}. "), theme.markdown_list_enumeration),
                None => ("• ".to_string(), theme.markdown_list_item),
            };
            let style = Style::new().fg(theme.markdown_text.to_color());
            let mut spans: Vec<Span<'static>> = vec![Span::styled("  ".repeat(depth), style)];
            spans.push(Span::styled(bullet, Style::new().fg(color.to_color())));
            spans.extend(inline_spans(&text, theme, style));
            wrap_spans(spans, width)
        }
        Block::Table(rows) => render_table(rows, theme, width),
        Block::Rule => vec![Line::from(Span::styled(
            "─".repeat(width.max(1) as usize),
            Style::new().fg(theme.markdown_horizontal_rule.to_color()),
        ))],
    }
}

/// Per-column alignment carried by the GFM delimiter row
/// (`:---`, `---:`, `:---:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

fn cell_alignment(delimiter_cell: &str) -> Align {
    match (
        delimiter_cell.starts_with(':'),
        delimiter_cell.ends_with(':'),
    ) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    }
}

fn pad_cell(cell: &str, width: usize, align: Align) -> String {
    let pad = width.saturating_sub(cell.chars().count());
    match align {
        Align::Left => format!("{cell}{}", " ".repeat(pad)),
        Align::Right => format!("{}{cell}", " ".repeat(pad)),
        Align::Center => {
            let left = pad / 2;
            format!("{}{cell}{}", " ".repeat(left), " ".repeat(pad - left))
        }
    }
}

/// `tableOptions={{ style: "grid" }}` — OpenTUI's single-line
/// box-drawing borders around the fitted columns.
fn render_table(rows: Vec<Vec<String>>, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    let columns = rows.iter().map(|row| row.len()).max().unwrap_or(0);
    if columns == 0 || rows.len() < 2 {
        return Vec::new();
    }
    // rows[0] is the header, rows[1] the delimiter row (construction
    // guarantees it); its cells carry the column alignments.
    let alignments: Vec<Align> = (0..columns)
        .map(|column| cell_alignment(rows[1].get(column).map(String::as_str).unwrap_or("")))
        .collect();
    let data: Vec<&Vec<String>> = rows
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 1)
        .map(|(_, row)| row)
        .collect();
    let mut widths = vec![0usize; columns];
    for row in &data {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }
    let total: usize = widths.iter().sum::<usize>() + columns * 3 + 1;
    if let Some(budget) = (width as usize).checked_sub(1) {
        if total > budget && budget > 0 {
            // `fitColumnWidthsProportional` — every column keeps a
            // minimum of one cell.
            widths = widths.iter().map(|w| (w * budget / total).max(1)).collect();
        }
    }
    let border = Style::new().fg(theme.border_subtle.to_color());
    let text = Style::new().fg(theme.markdown_text.to_color());
    let rule = |left: char, mid: char, right: char| -> Line<'static> {
        let mut spans = vec![Span::styled(left.to_string(), border)];
        for (index, cell_width) in widths.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(mid.to_string(), border));
            }
            spans.push(Span::styled("─".repeat(cell_width + 2), border));
        }
        spans.push(Span::styled(right.to_string(), border));
        Line::from(spans)
    };

    let mut lines = vec![rule('┌', '┬', '┐')];
    for (row_index, row) in data.iter().enumerate() {
        if row_index > 0 {
            lines.push(rule('├', '┼', '┤'));
        }
        let row_style = if row_index == 0 {
            text.add_modifier(Modifier::BOLD)
        } else {
            text
        };
        // Cells wrap within their column (`computeColumnWidths` fits
        // the columns; the text buffers wrap the content).
        let cells: Vec<Vec<String>> = (0..columns)
            .map(|column| {
                wrap_cell(
                    row.get(column).map(String::as_str).unwrap_or(""),
                    widths[column].max(1),
                )
            })
            .collect();
        let height = cells.iter().map(|cell| cell.len()).max().unwrap_or(1);
        for line_index in 0..height {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for column in 0..columns {
                let cell = cells[column]
                    .get(line_index)
                    .map(String::as_str)
                    .unwrap_or("");
                spans.push(Span::styled("│", border));
                spans.push(Span::styled(" ", row_style));
                spans.push(Span::styled(
                    pad_cell(cell, widths[column].max(1), alignments[column]),
                    row_style,
                ));
                spans.push(Span::styled(" ", row_style));
            }
            spans.push(Span::styled("│", border));
            lines.push(Line::from(spans));
        }
    }
    lines.push(rule('└', '┴', '┘'));
    lines
}

/// Word-wrap a cell within its column, hard-breaking words that do
/// not fit.
fn wrap_cell(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split(' ') {
        let pieces = hard_break(word, width);
        for (index, piece) in pieces.iter().enumerate() {
            if index == 0 && !current.is_empty() {
                if current.chars().count() + 1 + piece.chars().count() <= width {
                    current.push(' ');
                } else {
                    lines.push(std::mem::take(&mut current));
                }
            } else if index > 0 && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            current.push_str(piece);
        }
    }
    lines.push(current);
    lines
}

fn hard_break(word: &str, width: usize) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut piece = String::new();
    for char in word.chars() {
        if piece.chars().count() >= width {
            pieces.push(std::mem::take(&mut piece));
        }
        piece.push(char);
    }
    if !piece.is_empty() || pieces.is_empty() {
        pieces.push(piece);
    }
    pieces
}



// ------------------------------------------------------------- inline

/// Inline styling: `**strong**`, `*em*`/`_em_`, `` `code` ``,
/// `[text](url)` (links render as their text), `![alt](url)`.
fn inline_spans(text: &str, theme: &Theme, base: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    let mut plain = String::new();
    while index < chars.len() {
        // `code`
        if chars[index] == '`' && chars[index..].iter().skip(1).any(|c| *c == '`') {
            if let Some(end) = chars[index + 1..].iter().position(|c| *c == '`') {
                push_plain(&mut spans, &mut plain, base);
                let content: String = chars[index + 1..index + 1 + end].iter().collect();
                spans.push(Span::styled(
                    content,
                    Style::new().fg(theme.markdown_code.to_color()),
                ));
                index += end + 2;
                continue;
            }
        }
        // **strong** / *em*
        if chars[index] == '*' {
            let (marker_len, marker) = match chars.get(index + 1) {
                Some('*') => (2, "**"),
                _ => (1, "*"),
            };
            if let Some(end) = find_marker(&chars[index + marker_len..], marker) {
                push_plain(&mut spans, &mut plain, base);
                let content: String = chars[index + marker_len..index + marker_len + end]
                    .iter()
                    .collect();
                let styled = if marker_len == 2 {
                    Style::new()
                        .fg(theme.markdown_strong.to_color())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                        .fg(theme.markdown_emph.to_color())
                        .add_modifier(Modifier::ITALIC)
                };
                spans.push(Span::styled(content, styled));
                index += marker_len + end + marker_len;
                continue;
            }
        }
        if let Some((content, style, next)) = inline_link(&chars, index, theme) {
            push_plain(&mut spans, &mut plain, base);
            spans.push(Span::styled(content, style));
            index = next;
            continue;
        }
        plain.push(chars[index]);
        index += 1;
    }
    push_plain(&mut spans, &mut plain, base);
    spans
}

fn find_marker(chars: &[char], marker: &str) -> Option<usize> {
    let marker: Vec<char> = marker.chars().collect();
    let mut position = 0;
    while position + marker.len() <= chars.len() {
        if chars[position..position + marker.len()] == marker[..] {
            return Some(position);
        }
        position += 1;
    }
    None
}

/// `[text](url)` / `![alt](url)` — links render as their text.
fn inline_link(chars: &[char], index: usize, theme: &Theme) -> Option<(String, Style, usize)> {
    let image = chars[index] == '!';
    let open = if image { index + 1 } else { index };
    if chars.get(open) != Some(&'[') {
        return None;
    }
    let text_end = chars[open + 1..].iter().position(|c| *c == ']')? + open + 1;
    if chars.get(text_end + 1) != Some(&'(') {
        return None;
    }
    let close = chars[text_end + 2..].iter().position(|c| *c == ')')? + text_end + 2;
    let content: String = chars[open + 1..text_end].iter().collect();
    let (content, color) = if image {
        (format!("[image: {content}]"), theme.markdown_image_text)
    } else {
        (content, theme.markdown_link_text)
    };
    Some((content, Style::new().fg(color.to_color()), close + 1))
}

fn push_plain(spans: &mut Vec<Span<'static>>, plain: &mut String, base: Style) {
    if !plain.is_empty() {
        spans.push(Span::styled(std::mem::take(plain), base));
    }
}

fn inline_wrap(text: &str, width: u16, base: Option<Style>, theme: &Theme) -> Vec<Line<'static>> {
    let style = base.unwrap_or_else(|| Style::new().fg(theme.markdown_text.to_color()));
    let mut lines = Vec::new();
    for row in text.split('\n') {
        lines.extend(wrap_spans(inline_spans(row, theme, style), width));
    }
    lines
}

/// Word-wrap a span list to `width` columns (char-based).
fn wrap_spans(spans: Vec<Span<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = (width.max(1) as usize).max(1);
    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    for span in spans {
        let style = span.style;
        let mut remaining: &str = &span.content;
        while !remaining.is_empty() {
            let space = width.saturating_sub(used);
            if space == 0 {
                lines.push(Line::from(std::mem::take(&mut current)));
                used = 0;
                continue;
            }
            let count = remaining.chars().count();
            if count <= space {
                current.push(Span::styled(remaining.to_string(), style));
                used += count;
                break;
            }
            let mut break_at = None;
            for (position, _) in remaining.char_indices().skip(1) {
                if position > space {
                    break;
                }
                if remaining.as_bytes()[position - 1] == b' ' {
                    break_at = Some(position);
                }
            }
            match break_at {
                Some(position) => {
                    let (head, tail) = remaining.split_at(position - 1);
                    current.push(Span::styled(head.to_string(), style));
                    remaining = tail.strip_prefix(' ').unwrap_or(tail);
                    lines.push(Line::from(std::mem::take(&mut current)));
                    used = 0;
                }
                None => {
                    let head: String = remaining.chars().take(space).collect();
                    let head_len = head.len();
                    current.push(Span::styled(head, style));
                    remaining = &remaining[head_len..];
                    lines.push(Line::from(std::mem::take(&mut current)));
                    used = 0;
                }
            }
        }
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

/// The alpha composite the thinking header applies to
/// `theme.warning` (`thinkingOpacity`).
pub fn blend_over(base: Rgba, overlay: Rgba, alpha: f32) -> Rgba {
    let channel = |b: f32, o: f32| b + (o - b) * alpha;
    Rgba::from_values(
        channel(base.r, overlay.r),
        channel(base.g, overlay.g),
        channel(base.b, overlay.b),
        1.0,
    )
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

    fn lines_of(markdown: &str, width: u16) -> Vec<String> {
        let theme = theme();
        render(markdown, width, &theme)
            .into_iter()
            .map(|line| line.spans.iter().map(|s| s.content.to_string()).collect())
            .collect()
    }

    #[test]
    fn paragraphs_wrap() {
        let lines = lines_of("hello world", 80);
        assert_eq!(lines, vec!["hello world"]);
        let wrapped = lines_of("aaa bbb ccc ddd", 9);
        assert_eq!(wrapped, vec!["aaa bbb", "ccc ddd"]);
    }

    #[test]
    fn headings_are_bold_and_colored() {
        let theme = theme();
        let lines = render("# Title\n\nbody", 80, &theme);
        let (heading, body) = (lines[0].clone(), lines[1].clone());
        assert_eq!(heading.spans[0].content, "Title");
        assert_eq!(
            heading.spans[0].style.fg,
            Some(theme.markdown_heading.to_color())
        );
        assert!(heading.spans[0]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
        assert_eq!(body.spans[0].content, "body");
    }

    #[test]
    fn code_fences_render_with_language() {
        let markdown = "before\n```rust\nfn main() {}\n```\nafter";
        let lines = lines_of(markdown, 80);
        assert_eq!(
            lines,
            vec!["before", "fn main() {}", "after"],
            "fence lines drop, language tolerated"
        );
    }

    #[test]
    fn unterminated_fence_is_streaming_safe() {
        let markdown = "text\n```python\nprint(1)\nprint(2)";
        let lines = lines_of(markdown, 80);
        assert_eq!(lines, vec!["text", "print(1)", "print(2)"]);
    }

    #[test]
    fn lists_render_bullets_and_numbers() {
        let markdown = "- one\n- two\n1. three\n  - nested";
        let lines = lines_of(markdown, 80);
        assert_eq!(lines, vec!["• one", "• two", "1. three", "  • nested"]);
    }

    #[test]
    fn blockquotes_prefix_rows() {
        let lines = lines_of("> quoted text", 80);
        assert_eq!(lines, vec!["> quoted text"]);
    }

    #[test]
    fn grid_tables_align_columns() {
        let markdown = "| a | b |\n|---|---|\n| 1 | 2 |";
        let lines = lines_of(markdown, 40);
        assert_eq!(lines[0], "┌───┬───┐");
        assert_eq!(lines[1], "│ a │ b │");
        assert_eq!(lines[2], "├───┼───┤");
        assert_eq!(lines[3], "│ 1 │ 2 │");
        assert_eq!(lines[4], "└───┴───┘");
    }

    #[test]
    fn pipeless_tables_align_columns() {
        // GFM makes the outer pipes optional
        // (`mdast-util-gfm-table`) — the shape models emit when not
        // fenced in full pipes.
        let markdown = "a | b\n--- | ---\n1 | 2";
        let lines = lines_of(markdown, 40);
        assert_eq!(lines[1], "│ a │ b │");
        assert_eq!(lines[3], "│ 1 │ 2 │");
    }

    #[test]
    fn partially_piped_tables_align_columns() {
        let markdown = "| a | b\n|---|---\n| 1 | 2";
        let lines = lines_of(markdown, 40);
        assert_eq!(lines[1], "│ a │ b │");
        assert_eq!(lines[3], "│ 1 │ 2 │");
    }

    #[test]
    fn a_lone_pipe_row_is_prose() {
        // Without a following delimiter row it is not a table.
        let lines = lines_of("before\n\na | b\n\nafter", 40);
        assert_eq!(lines, vec!["before", "a | b", "after"]);
    }

    #[test]
    fn over_wide_tables_wrap_cells() {
        // The table is wider than the budget — columns fit
        // proportionally and cell content wraps inside its column so
        // the grid stays aligned (computeColumnWidths).
        let markdown = "| aaa bbb | ccc ddd |\n|---|---|\n| 123456789 123456789 | x |";
        let lines = lines_of(markdown, 30);
        // Every rendered row must be closed at the same column — the
        // grid stays aligned.
        for line in &lines {
            if line.contains('┬') || line.contains('┼') || line.contains('┴') {
                continue; // a border rule
            }
            assert!(
                line.ends_with('│'),
                "row not closed: {line:?} (lines={lines:?})"
            );
        }
        // The wide cell wrapped inside its column across multiple rows.
        assert!(lines.len() > 3, "cell did not wrap: {lines:?}");
    }

    #[test]
    fn inline_emphasis_code_and_links() {
        let theme = theme();
        let lines = render("*em* **strong** `x` [docs](https://x)", 80, &theme);
        let spans: Vec<String> = lines[0]
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .filter(|content| content != " ")
            .collect();
        assert_eq!(spans, vec!["em", "strong", "x", "docs"]);
        let spans = &lines[0].spans;
        assert_eq!(spans[0].style.fg, Some(theme.markdown_emph.to_color()));
        // **strong** at index 2 — after the " " separator span.
        assert_eq!(spans[2].content, "strong");
        assert_eq!(spans[2].style.fg, Some(theme.markdown_strong.to_color()));
        assert_eq!(spans[4].content, "x");
        assert_eq!(spans[4].style.fg, Some(theme.markdown_code.to_color()));
        // Links render as their text.
        assert_eq!(spans[6].content, "docs");
        assert_eq!(spans[6].style.fg, Some(theme.markdown_link_text.to_color()));
    }

    #[test]
    fn horizontal_rules_fill_the_width() {
        let lines = lines_of("a\n\n---\n\nb", 20);
        assert_eq!(lines, vec!["a", "────────────────────", "b"]);
    }
}
