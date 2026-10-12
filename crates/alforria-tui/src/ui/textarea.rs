//! `ui/textarea.rs` — the editing primitive (M8.6): cursor movement
//! (char/word/line/buffer), insert/delete, undo/redo, soft-wrap display
//! and the prompt height cap. The OpenTUI `TextareaRenderable` becomes
//! a pure buffer model — offsets are **char offsets** (`promptOffsetWidth`
//! counts newlines as one position; char offsets match that exactly for
//! every ASCII input).
//!
//! Virtual text (`extmark`) semantics are the TS extmark model: a
//! `(start, end)` span over the buffer that shifts with edits and is
//! destroyed when a deletion fully covers it (spec M8.6 S1).

/// One virtual-text span (`input.extmarks.create`, `prompt/index.tsx:1156`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extmark {
    pub id: u64,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone)]
struct Snapshot {
    buffer: String,
    cursor: usize,
    anchor: Option<usize>,
    extmarks: Vec<Extmark>,
}

/// The soft-wrap display of a textarea: rows of `(char, extmark id)`,
/// plus the cursor's row/col.
#[derive(Debug, Default)]
pub struct Display {
    pub rows: Vec<Vec<(char, Option<u64>)>>,
    pub cursor_row: usize,
    /// The cursor's column in display cells.
    pub cursor_col: usize,
    /// The cursor's char index within its row (what the renderer
    /// paints; differs from `cursor_col` after wide glyphs).
    pub cursor_index: usize,
}

impl Display {
    /// The visible `maxHeight` window — the OpenTUI textarea scrolls
    /// to keep the cursor visible (`prompt/index.tsx:1368`); the window
    /// keeps the cursor row on screen (pinned to the bottom row while
    /// typing at the end).
    pub fn window(&self, max_height: u16) -> std::ops::Range<usize> {
        let max = max_height as usize;
        if self.rows.len() <= max {
            return 0..self.rows.len();
        }
        let start = self
            .cursor_row
            .saturating_add(1)
            .saturating_sub(max)
            .min(self.rows.len() - max);
        start..start + max
    }
}

const UNDO_LIMIT: usize = 100;

/// The editing primitive.
#[derive(Debug, Clone)]
pub struct Textarea {
    buffer: String,
    /// Char offset of the cursor (`input.cursorOffset`).
    cursor: usize,
    /// Selection anchor, if a visual selection is active.
    anchor: Option<usize>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    extmarks: Vec<Extmark>,
    next_extmark: u64,
}

impl Default for Textarea {
    fn default() -> Textarea {
        Textarea {
            buffer: String::new(),
            cursor: 0,
            anchor: None,
            undo: Vec::new(),
            redo: Vec::new(),
            extmarks: Vec::new(),
            next_extmark: 1,
        }
    }
}

impl Textarea {
    pub fn new() -> Textarea {
        Textarea::default()
    }

    pub fn text(&self) -> &str {
        &self.buffer
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn char_count(&self) -> usize {
        self.buffer.chars().count()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The char at `offset`, if any (`displayCharAt`).
    pub fn char_at(&self, offset: usize) -> Option<char> {
        self.buffer.chars().nth(offset)
    }

    fn clamp(&self, offset: usize) -> usize {
        offset.min(self.char_count())
    }

    /// (line index, column) of a char offset.
    fn line_position(&self, offset: usize) -> (usize, usize) {
        let mut line = 0;
        for (index, char) in self.buffer.chars().enumerate() {
            if index >= offset {
                break;
            }
            if char == '\n' {
                line += 1;
            }
        }
        let start = self.line_start(line);
        (line, offset - start)
    }

    /// The char offset where `line` begins.
    fn line_start(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        self.buffer
            .char_indices()
            .filter(|(_, c)| *c == '\n')
            .nth(line - 1)
            .map(|(index, _)| self.buffer[..=index].chars().count())
            .unwrap_or(self.char_count())
    }

    fn line_span(&self, line: usize) -> (usize, usize) {
        let lines: Vec<&str> = self.buffer.split('\n').collect();
        let start = self.line_start(line);
        let len = lines
            .get(line)
            .map(|text| text.chars().count())
            .unwrap_or(0);
        let newline = if line + 1 < lines.len() { 1 } else { 0 };
        (start, start + len + newline)
    }

    // ------------------------------------------------------------- edits

    fn snapshot(&mut self) {
        let snapshot = Snapshot {
            buffer: self.buffer.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
            extmarks: self.extmarks.clone(),
        };
        self.undo.push(snapshot);
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn current_snapshot(&self) -> Snapshot {
        Snapshot {
            buffer: self.buffer.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
            extmarks: self.extmarks.clone(),
        }
    }

    /// `input.setText(...)` (`prompt/index.tsx:596`).
    pub fn set_text(&mut self, text: &str) {
        self.snapshot();
        self.buffer = text.to_string();
        self.cursor = self.char_count();
        self.anchor = None;
    }

    pub fn clear(&mut self) {
        self.set_text("");
    }

    /// `input.insertText(...)` — insert at the cursor, replacing an
    /// active selection.
    pub fn insert_text(&mut self, text: &str) {
        if let Some((start, end)) = self.selection() {
            self.delete_range(start, end);
        }
        self.snapshot();
        let at = self.cursor;
        let byte = self.byte_index(at);
        self.buffer.insert_str(byte, text);
        let len = text.chars().count();
        for mark in &mut self.extmarks {
            if mark.start >= at {
                mark.start += len;
            }
            if mark.end >= at {
                mark.end += len;
            }
        }
        self.cursor += len;
        self.anchor = None;
    }

    /// `input.deleteRange(...)` — delete a char range.
    pub fn delete_range(&mut self, start: usize, end: usize) {
        if start == end {
            return;
        }
        let (start, end) = if start < end {
            (start, end)
        } else {
            (end, start)
        };
        let (start, end) = (self.clamp(start), self.clamp(end));
        if start == end {
            return;
        }
        self.snapshot();
        self.delete_range_inner(start, end);
        self.anchor = None;
    }

    fn delete_range_inner(&mut self, start: usize, end: usize) {
        let (from, to) = (self.byte_index(start), self.byte_index(end));
        self.buffer.replace_range(from..to, "");
        let len = end - start;
        self.extmarks
            .retain(|mark| !(mark.start >= start && mark.end <= end));
        for mark in &mut self.extmarks {
            if mark.start > end {
                mark.start -= len;
            } else if mark.start > start {
                mark.start = start;
            }
            if mark.end >= end {
                mark.end -= len;
            } else if mark.end > start {
                mark.end = start;
            }
        }
        if self.cursor >= end {
            self.cursor -= len;
        } else if self.cursor > start {
            self.cursor = start;
        }
        self.cursor = self.cursor.min(self.char_count());
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.delete_at(self.cursor - 1, self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.char_count() {
            self.delete_at(self.cursor, self.cursor + 1);
        }
    }

    fn delete_at(&mut self, start: usize, end: usize) {
        self.snapshot();
        self.delete_range_inner(start, end);
        self.anchor = None;
    }

    // --------------------------------------------------------- selection

    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    /// The active selection range, if non-empty.
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.anchor.map(|anchor| {
            if anchor < self.cursor {
                (anchor, self.cursor)
            } else {
                (self.cursor, anchor)
            }
        })
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.char_count();
    }

    // --------------------------------------------------------- movement

    pub fn move_to(&mut self, offset: usize, select: bool) {
        let offset = self.clamp(offset);
        if !select {
            self.anchor = None;
        } else if self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }
        self.cursor = offset;
    }

    pub fn move_left(&mut self, select: bool) {
        self.move_to(self.cursor.saturating_sub(1), select);
    }

    pub fn move_right(&mut self, select: bool) {
        self.move_to(self.cursor + 1, select);
    }

    pub fn move_up(&mut self, select: bool) {
        let (line, col) = self.line_position(self.cursor);
        if line == 0 {
            self.move_to(0, select);
            return;
        }
        let (prev_start, prev_len) = self.line_extent(line - 1);
        self.move_to(prev_start + col.min(prev_len), select);
    }

    pub fn move_down(&mut self, select: bool) {
        let (line, col) = self.line_position(self.cursor);
        let (next_start, next_len) = self.line_extent(line + 1);
        if next_start > self.char_count() {
            self.buffer_end(select);
            return;
        }
        self.move_to(next_start + col.min(next_len), select);
    }

    /// (start offset, content length) of the line `line`, counting the
    /// trailing newline as part of its content.
    fn line_extent(&self, line: usize) -> (usize, usize) {
        let lines: Vec<&str> = self.buffer.split('\n').collect();
        let Some(content) = lines.get(line) else {
            return (self.char_count(), 0);
        };
        let start = self.line_start(line);
        (start, content.chars().count())
    }

    pub fn line_home(&mut self, select: bool) {
        let (start, _) = self.line_span(self.line_position(self.cursor).0);
        self.move_to(start, select);
    }

    pub fn line_end(&mut self, select: bool) {
        let (start, len) = self.line_extent(self.line_position(self.cursor).0);
        self.move_to(start + len, select);
    }

    pub fn buffer_home(&mut self, select: bool) {
        self.move_to(0, select);
    }

    pub fn buffer_end(&mut self, select: bool) {
        self.move_to(self.char_count(), select);
    }

    // --------------------------------------------------------- word ops

    pub fn word_forward(&mut self, select: bool) {
        let count = self.char_count();
        let mut offset = self.cursor;
        while offset < count && self.is_word(offset).is_none() {
            offset += 1;
        }
        while offset < count && self.is_word(offset) == Some(true) {
            offset += 1;
        }
        self.move_to(offset, select);
    }

    pub fn word_backward(&mut self, select: bool) {
        let mut offset = self.cursor;
        while offset > 0 && self.is_word(offset - 1).is_none() {
            offset -= 1;
        }
        while offset > 0 && self.is_word(offset - 1) == Some(true) {
            offset -= 1;
        }
        self.move_to(offset, select);
    }

    /// `Some(true)` when the char at `offset` is a word char,
    /// `Some(false)` when it is punctuation, `None` for whitespace.
    fn is_word(&self, offset: usize) -> Option<bool> {
        let char = self.buffer.chars().nth(offset)?;
        if char.is_whitespace() {
            None
        } else {
            Some(char.is_alphanumeric() || char == '_')
        }
    }

    pub fn delete_word_forward(&mut self) {
        let count = self.char_count();
        let mut offset = self.cursor;
        while offset < count && self.is_word(offset).is_none() {
            offset += 1;
        }
        while offset < count && self.is_word(offset) == Some(true) {
            offset += 1;
        }
        if offset > self.cursor {
            self.delete_range(self.cursor, offset);
        }
    }

    pub fn delete_word_backward(&mut self) {
        let mut offset = self.cursor;
        while offset > 0 && self.is_word(offset - 1).is_none() {
            offset -= 1;
        }
        while offset > 0 && self.is_word(offset - 1) == Some(true) {
            offset -= 1;
        }
        if offset < self.cursor {
            self.delete_range(offset, self.cursor);
        }
    }

    // ------------------------------------------------------ line deletes

    pub fn delete_line(&mut self) {
        let (start, end) = self.line_span(self.line_position(self.cursor).0);
        self.delete_range(start, end);
    }

    pub fn delete_to_line_end(&mut self) {
        let (_, end) = self.line_span(self.line_position(self.cursor).0);
        self.delete_range(self.cursor, end);
    }

    pub fn delete_to_line_start(&mut self) {
        let (start, _) = self.line_span(self.line_position(self.cursor).0);
        self.delete_range(start, self.cursor);
    }

    // ------------------------------------------------------ undo / redo

    pub fn undo(&mut self) {
        if let Some(snapshot) = self.undo.pop() {
            let redo = self.current_snapshot();
            self.redo.push(redo);
            self.restore(snapshot);
        }
    }

    pub fn redo(&mut self) {
        if let Some(snapshot) = self.redo.pop() {
            let undo = self.current_snapshot();
            self.undo.push(undo);
            self.restore(snapshot);
        }
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.buffer = snapshot.buffer;
        self.cursor = snapshot.cursor;
        self.anchor = snapshot.anchor;
        self.extmarks = snapshot.extmarks;
    }

    // ------------------------------------------------------ extmarks

    pub fn create_extmark(&mut self, start: usize, end: usize) -> u64 {
        let id = self.next_extmark;
        self.next_extmark += 1;
        self.extmarks.push(Extmark {
            id,
            start: self.clamp(start),
            end: self.clamp(end),
        });
        id
    }

    pub fn extmarks(&self) -> &[Extmark] {
        &self.extmarks
    }

    pub fn clear_extmarks(&mut self) {
        self.extmarks.clear();
    }

    pub fn extmark(&self, id: u64) -> Option<&Extmark> {
        self.extmarks.iter().find(|mark| mark.id == id)
    }

    // ------------------------------------------------------ display

    fn byte_index(&self, offset: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(offset)
            .map(|(index, _)| index)
            .unwrap_or(self.buffer.len())
    }

    /// Soft-wrap the buffer into `width` columns (display-width aware —
    /// CJK/emoji count as two columns, `unicode_width`) and place the
    /// cursor.
    pub fn display(&self, width: u16) -> Display {
        let chars = self.buffer.chars().enumerate().map(|(offset, char)| {
            let mark = self
                .extmarks
                .iter()
                .find(|mark| offset >= mark.start && offset < mark.end)
                .map(|mark| mark.id);
            (char, mark)
        });
        wrap_chars(chars, self.cursor, width)
    }

    /// `maxHeight` (`prompt/index.tsx:1345`): `prompt.max_height`
    /// config or `max(6, height/3)`.
    pub fn max_height(config: Option<u16>, terminal_height: u16) -> u16 {
        config
            .unwrap_or_else(|| std::cmp::max(6, terminal_height / 3))
            .max(1)
    }
}

/// Soft-wrap plain `text` with the cursor at its end (the seeded
/// `--prompt` input).
pub fn wrap_text(text: &str, width: u16) -> Display {
    wrap_chars(text.chars().map(|char| (char, None)), usize::MAX, width)
}

/// Soft-wrap `(char, extmark)` cells into `width` display columns; the
/// cursor sits before the char at offset `cursor` (past the end when
/// `cursor` is beyond the text).
fn wrap_chars(
    chars: impl Iterator<Item = (char, Option<u64>)>,
    cursor: usize,
    width: u16,
) -> Display {
    let width = (width.max(1) as usize).max(1);
    let mut rows: Vec<Vec<(char, Option<u64>)>> = vec![Vec::new()];
    // The display width used by the current row.
    let mut used = 0usize;
    let mut placed: Option<(usize, usize, usize)> = None;
    for (offset, (char, mark)) in chars.enumerate() {
        if char == '\n' {
            if offset == cursor {
                placed = Some((rows.len() - 1, used, rows.last().map_or(0, Vec::len)));
            }
            rows.push(Vec::new());
            used = 0;
            continue;
        }
        let char_width = unicode_width::UnicodeWidthChar::width(char).unwrap_or(0);
        if used + char_width > width && used > 0 {
            rows.push(Vec::new());
            used = 0;
        }
        // Placed after the wrap decision: a cursor on a char that wraps
        // sits at the start of the next row.
        if offset == cursor {
            placed = Some((rows.len() - 1, used, rows.last().map_or(0, Vec::len)));
        }
        rows.last_mut().expect("a row").push((char, mark));
        used += char_width;
    }
    let (cursor_row, cursor_col, cursor_index) =
        placed.unwrap_or((rows.len() - 1, used, rows.last().map_or(0, Vec::len)));
    Display {
        rows,
        cursor_row,
        cursor_col,
        cursor_index,
    }
}

impl Display {
    /// The visible rows as styled lines, with the block cursor painted
    /// over the cursor's char (or one cell past the row's end) when
    /// `cursor` is set — shared by the session and home prompts.
    pub fn lines(
        &self,
        window: std::ops::Range<usize>,
        text: ratatui::style::Style,
        cursor: Option<ratatui::style::Style>,
    ) -> Vec<ratatui::text::Line<'static>> {
        use ratatui::text::{Line, Span};
        let mut lines = Vec::new();
        for (row, cells) in self.rows[window.clone()].iter().enumerate() {
            let on_cursor_row = window.start + row == self.cursor_row;
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(cells.len() + 1);
            for (index, (char, _mark)) in cells.iter().enumerate() {
                let style = match cursor {
                    Some(cursor) if on_cursor_row && index == self.cursor_index => cursor,
                    _ => text,
                };
                spans.push(Span::styled(char.to_string(), style));
            }
            // The cursor one past the last cell (its usual position
            // while typing): an explicit block cell.
            if let Some(cursor) = cursor {
                if on_cursor_row && self.cursor_index >= cells.len() {
                    spans.push(Span::styled(" ", cursor));
                }
            }
            lines.push(Line::from(spans));
        }
        lines
    }
}

/// The OpenTUI block cursor: one cell painted `theme.text`
/// (`cursorColor={theme.text}`, prompt/index.tsx:1439-1441). `visible`
/// is the `cursor.blinking` phase — false hides the cell and restores
/// the underlying glyph.
pub fn cursor_cell_style(theme: &crate::ui::theme::Theme) -> ratatui::style::Style {
    ratatui::style::Style::new()
        .fg(theme.background_element.to_color())
        .bg(theme.text.to_color())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_cursor() {
        let mut textarea = Textarea::new();
        textarea.insert_text("hello");
        assert_eq!(textarea.text(), "hello");
        textarea.move_left(false);
        textarea.move_left(false);
        textarea.insert_text(" ");
        assert_eq!(textarea.text(), "hel lo");
        assert_eq!(textarea.cursor(), 4);
    }

    #[test]
    fn backspace_and_delete() {
        let mut textarea = Textarea::new();
        textarea.insert_text("abc");
        textarea.move_left(false);
        textarea.backspace();
        assert_eq!(textarea.text(), "ac");
        textarea.delete();
        assert_eq!(textarea.text(), "a");
    }

    #[test]
    fn line_movement() {
        let mut textarea = Textarea::new();
        textarea.insert_text("one\ntwo\nthree");
        textarea.buffer_end(false);
        textarea.line_home(false);
        assert_eq!(textarea.cursor(), 8);
        textarea.line_end(false);
        assert_eq!(textarea.cursor(), 13);
        // Column 5 clamps to the shorter "two" line.
        textarea.move_up(false);
        assert_eq!(textarea.cursor(), 7);
        textarea.buffer_home(false);
        textarea.move_down(false);
        assert_eq!(textarea.cursor(), 4);
    }

    #[test]
    fn word_ops_skip_whitespace_and_words() {
        let mut textarea = Textarea::new();
        textarea.insert_text("hello world foo");
        textarea.buffer_home(false);
        textarea.word_forward(false);
        assert_eq!(textarea.cursor(), 5);
        textarea.word_forward(false);
        assert_eq!(textarea.cursor(), 11);
        textarea.word_backward(false);
        textarea.word_backward(false);
        assert_eq!(textarea.cursor(), 0);
    }

    #[test]
    fn delete_word_ops() {
        let mut textarea = Textarea::new();
        textarea.insert_text("foo bar baz");
        textarea.delete_word_backward();
        assert_eq!(textarea.text(), "foo bar ");
        let mut textarea = Textarea::new();
        textarea.insert_text("foo bar baz");
        textarea.move_to(4, false);
        textarea.delete_word_forward();
        assert_eq!(textarea.text(), "foo  baz");
    }

    #[test]
    fn undo_redo_round_trip() {
        let mut textarea = Textarea::new();
        textarea.insert_text("one ");
        textarea.insert_text("two");
        textarea.undo();
        assert_eq!(textarea.text(), "one ");
        textarea.redo();
        assert_eq!(textarea.text(), "one two");
        textarea.undo();
        textarea.undo();
        assert_eq!(textarea.text(), "");
    }

    #[test]
    fn undo_restores_extmarks() {
        let mut textarea = Textarea::new();
        textarea.insert_text("[Pasted ~3 lines] ");
        textarea.create_extmark(0, 18);
        let count = textarea.char_count();
        textarea.delete_range(0, count);
        assert!(textarea.extmarks().is_empty());
        textarea.undo();
        assert_eq!(textarea.extmarks().len(), 1);
    }

    #[test]
    fn extmarks_shift_with_edits() {
        let mut textarea = Textarea::new();
        textarea.insert_text("ab");
        let mark = textarea.create_extmark(0, 2);
        textarea.move_left(false);
        textarea.move_left(false);
        textarea.insert_text("XX");
        assert_eq!(textarea.extmark(mark).unwrap().start, 2);
        assert_eq!(textarea.extmark(mark).unwrap().end, 4);
        textarea.delete_range(0, 1);
        assert_eq!(textarea.extmark(mark).unwrap().start, 1);
    }

    #[test]
    fn extmark_destroyed_when_fully_deleted() {
        let mut textarea = Textarea::new();
        textarea.insert_text("hello world");
        let mark = textarea.create_extmark(0, 5);
        textarea.delete_range(1, 4);
        assert!(textarea.extmark(mark).is_some());
        textarea.delete_range(0, 5);
        assert!(textarea.extmark(mark).is_none());
    }

    #[test]
    fn display_wraps_and_places_the_cursor() {
        let mut textarea = Textarea::new();
        textarea.insert_text("abc\nef");
        let display = textarea.display(3);
        assert_eq!(display.rows.len(), 2);
        textarea.buffer_end(false);
        let display = textarea.display(3);
        assert_eq!(display.cursor_row, 1);
        assert_eq!(display.cursor_col, 2);
    }

    #[test]
    fn display_wraps_long_lines() {
        let mut textarea = Textarea::new();
        textarea.insert_text("abcdef");
        let display = textarea.display(4);
        assert_eq!(display.rows.len(), 2);
        assert_eq!(display.rows[0].len(), 4);
    }

    fn cursor_cells(
        lines: &[ratatui::text::Line<'_>],
        cursor: ratatui::style::Style,
    ) -> Vec<String> {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.style == cursor)
            .map(|span| span.content.to_string())
            .collect()
    }

    #[test]
    fn the_cursor_lands_on_its_char_after_wide_glyphs() {
        let mut textarea = Textarea::new();
        textarea.insert_text("你好ab");
        textarea.move_to(3, false); // before `b`
        let display = textarea.display(20);
        assert_eq!((display.cursor_row, display.cursor_col), (0, 5));
        assert_eq!(display.cursor_index, 3);
        let cursor = ratatui::style::Style::new().bg(ratatui::style::Color::Red);
        let lines = display.lines(0..1, ratatui::style::Style::new(), Some(cursor));
        assert_eq!(cursor_cells(&lines, cursor), vec!["b"]);
        // At the end: one block cell past the text, no other.
        textarea.move_to(4, false);
        let display = textarea.display(20);
        let lines = display.lines(0..1, ratatui::style::Style::new(), Some(cursor));
        assert_eq!(cursor_cells(&lines, cursor), vec![" "]);
    }

    #[test]
    fn a_cursor_on_a_wrapping_char_starts_the_next_row() {
        let mut textarea = Textarea::new();
        textarea.insert_text("你好世");
        textarea.move_to(2, false); // before `世`, which wraps at width 4
        let display = textarea.display(4);
        assert_eq!(display.rows.len(), 2);
        assert_eq!(
            (display.cursor_row, display.cursor_col, display.cursor_index),
            (1, 0, 0)
        );
    }

    #[test]
    fn a_scrolled_window_paints_one_cursor() {
        // Empty rows above the cursor used to get a phantom cursor once
        // the window scrolled (window-relative vs absolute rows).
        let mut textarea = Textarea::new();
        textarea.insert_text("\n\n\n\n\nlast");
        let display = textarea.display(20);
        let window = display.window(2);
        assert_eq!(window, 4..6);
        let cursor = ratatui::style::Style::new().bg(ratatui::style::Color::Red);
        let lines = display.lines(window, ratatui::style::Style::new(), Some(cursor));
        assert_eq!(cursor_cells(&lines, cursor), vec![" "]);
        assert_eq!(wrap_text("ab\n你好", 3).rows.len(), 3);
    }

    #[test]
    fn max_height_cap() {
        assert_eq!(Textarea::max_height(None, 20), 6);
        assert_eq!(Textarea::max_height(None, 30), 10);
        assert_eq!(Textarea::max_height(Some(2), 30), 2);
    }

    #[test]
    fn selection_replaced_by_insert() {
        let mut textarea = Textarea::new();
        textarea.insert_text("hello world");
        textarea.move_to(5, false);
        textarea.move_to(0, true);
        textarea.insert_text("goodbye");
        assert_eq!(textarea.text(), "goodbye world");
    }

    #[test]
    fn line_deletes() {
        let mut textarea = Textarea::new();
        textarea.insert_text("one\ntwo\nthree");
        textarea.move_to(2, false);
        textarea.delete_line();
        assert_eq!(textarea.text(), "two\nthree");
        textarea.move_to(4, false);
        textarea.delete_to_line_end();
        assert_eq!(textarea.text(), "two\n");
        textarea.move_to(0, false);
        textarea.delete_to_line_start();
        assert_eq!(textarea.text(), "two\n");
    }
}
